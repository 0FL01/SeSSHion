use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

use crate::error::{Result, SshMcpError};
use crate::ssh::SshConnectionManager;

use super::exec_raw;
use super::process;
use super::staging;
use super::types::{
    RsyncOptions, StagingLocal, StagingRemote, TransferCounts, TransferKind, TransferOperation,
    TransferStaging,
};

// Staging/marker helpers live in `super::staging`.

#[derive(Debug, Clone)]
pub struct RsyncEndpoint {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub key_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct RsyncTransferArgs<'a> {
    pub conn: &'a SshConnectionManager,
    pub remote_home: &'a str,
    pub local_root: &'a Path,
    pub id: u64,
    pub timeout: Duration,
    pub operation: TransferOperation,
    pub kind: TransferKind,
    pub local_path: &'a Path,
    pub remote_path: &'a str,
    pub overwrite: bool,
    pub rsync_options: RsyncOptions,
}

pub async fn run_transfer(
    endpoint: RsyncEndpoint,
    args: RsyncTransferArgs<'_>,
) -> std::result::Result<(TransferStaging, TransferCounts), super::TransportAttemptError> {
    // Check local rsync availability first
    if let Err(e) = check_local_rsync().await {
        return Err(super::TransportAttemptError::Unsupported {
            transport: super::TransferTransport::Rsync,
            reason: format!("local rsync not available: {e}"),
        });
    }

    // Check remote rsync availability via SSH
    match check_remote_rsync(args.conn, args.timeout).await {
        Ok(true) => {}
        Ok(false) => {
            return Err(super::TransportAttemptError::Unsupported {
                transport: super::TransferTransport::Rsync,
                reason: "rsync not found on remote host".to_string(),
            });
        }
        Err(e) => {
            return Err(super::TransportAttemptError::Other(e));
        }
    }

    match (args.operation, args.kind) {
        (TransferOperation::Put, TransferKind::File) => put_file(endpoint, args).await,
        (TransferOperation::Get, TransferKind::File) => get_file(endpoint, args).await,
        (TransferOperation::Put, TransferKind::Directory) => put_dir(endpoint, args).await,
        (TransferOperation::Get, TransferKind::Directory) => get_dir(endpoint, args).await,
    }
}

async fn check_local_rsync() -> Result<()> {
    match Command::new("rsync").arg("--version").output().await {
        Ok(output) if output.status.success() => Ok(()),
        Ok(_) => Err(SshMcpError::connection("rsync --version failed")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(SshMcpError::connection("rsync binary not found"))
        }
        Err(e) => Err(SshMcpError::Io(e)),
    }
}

async fn check_remote_rsync(conn: &SshConnectionManager, timeout: Duration) -> Result<bool> {
    let cmd = r#"sh -c 'command -v rsync'"#;
    let out = conn.exec_command(cmd, timeout).await?;
    Ok(out.exit_code == Some(0) && !out.stdout.trim().is_empty())
}

fn build_ssh_options(endpoint: &RsyncEndpoint) -> String {
    let mut opts = vec![
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "StrictHostKeyChecking=no".to_string(),
        "-o".to_string(),
        "UserKnownHostsFile=/dev/null".to_string(),
        "-o".to_string(),
        "LogLevel=ERROR".to_string(),
    ];

    if endpoint.port != 22 {
        opts.push("-p".to_string());
        opts.push(endpoint.port.to_string());
    }

    if let Some(ref key) = endpoint.key_path {
        opts.push("-i".to_string());
        opts.push(key.display().to_string());
    }

    opts.join(" ")
}

fn rsync_remote_spec(endpoint: &RsyncEndpoint, remote_path: &str) -> String {
    format!("{}@{}:{}", endpoint.user, endpoint.host, remote_path)
}

async fn run_rsync(
    endpoint: &RsyncEndpoint,
    rsync_options: &RsyncOptions,
    src: &str,
    dst: &str,
    timeout_duration: Duration,
) -> std::result::Result<TransferCounts, super::TransportAttemptError> {
    let ssh_opts = build_ssh_options(endpoint);
    let mut cmd = Command::new("rsync");

    cmd.arg("--archive")
        .arg("--checksum")
        .arg("--inplace")
        .arg("--partial")
        .arg("--stats");

    if rsync_options.compress {
        cmd.arg("--compress");
    }

    if rsync_options.delete {
        cmd.arg("--delete");
    }

    cmd.arg("-e")
        .arg(format!("ssh {ssh_opts}"))
        .arg(src)
        .arg(dst);

    cmd.env("LC_ALL", "C");
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let child = cmd.spawn().map_err(classify_spawn_error)?;
    let captured = process::wait_child_with_timeout(child, timeout_duration).await?;

    let stdout = String::from_utf8_lossy(&captured.stdout).to_string();
    let stderr = String::from_utf8_lossy(&captured.stderr).to_string();

    if !captured.status.success() {
        return Err(classify_rsync_failure(captured.status.code(), &stderr));
    }

    Ok(parse_rsync_stats(&stdout))
}

fn parse_rsync_stats(stdout: &str) -> TransferCounts {
    let mut files = 0u64;
    let mut bytes = 0u64;
    let mut found_transferred_files = false;

    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("Number of regular files transferred: ") {
            // Prefer this count as it represents actual files (not including directories)
            if let Ok(n) = rest.parse::<u64>() {
                files = n;
                found_transferred_files = true;
            }
        } else if let Some(rest) = line.strip_prefix("Number of files: ") {
            // Format: "Number of files: 10 (reg: 8, dir: 2)"
            // Only use this as fallback if we haven't found "regular files transferred"
            if !found_transferred_files
                && let Some(num_str) = rest.split_whitespace().next()
                && let Ok(n) = num_str.parse::<u64>()
            {
                files = n;
            }
        } else if let Some(rest) = line.strip_prefix("Total transferred file size: ") {
            // Format: "Total transferred file size: 1,234,567 bytes"
            let cleaned: String = rest.chars().filter(|c| c.is_ascii_digit()).collect();
            if let Ok(n) = cleaned.parse::<u64>() {
                bytes = n;
            }
        }
    }

    TransferCounts {
        bytes,
        files,
        directories: 0,
    }
}

fn classify_spawn_error(err: std::io::Error) -> super::TransportAttemptError {
    process::classify_spawn_error_with_reason(
        err,
        super::TransferTransport::Rsync,
        "missing local rsync binary".to_string(),
    )
}

fn classify_rsync_failure(exit_code: Option<i32>, stderr: &str) -> super::TransportAttemptError {
    let stderr_lower = stderr.to_lowercase();

    // Check for rsync not found on remote
    if stderr_lower.contains("rsync: not found")
        || stderr_lower.contains("rsync: command not found")
        || stderr_lower.contains("could not find rsync")
    {
        return super::TransportAttemptError::Unsupported {
            transport: super::TransferTransport::Rsync,
            reason: "rsync not found on remote host".to_string(),
        };
    }

    // Check for SSH connection issues
    if stderr_lower.contains("connection refused")
        || stderr_lower.contains("connection timed out")
        || stderr_lower.contains("no route to host")
        || stderr_lower.contains("network is unreachable")
    {
        return super::TransportAttemptError::Other(SshMcpError::connection(format!(
            "rsync failed: network error; stderr={}",
            stderr.trim()
        )));
    }

    // Check for permission denied
    if stderr_lower.contains("permission denied") || stderr_lower.contains("access denied") {
        return super::TransportAttemptError::Other(SshMcpError::connection(format!(
            "rsync failed: permission denied; stderr={}",
            stderr.trim()
        )));
    }

    super::TransportAttemptError::Other(SshMcpError::connection(format!(
        "rsync failed: exit_code={exit_code:?}; stderr={}",
        stderr.trim()
    )))
}

// Remote staging helpers are implemented in `super::staging`.

async fn put_file(
    endpoint: RsyncEndpoint,
    args: RsyncTransferArgs<'_>,
) -> std::result::Result<(TransferStaging, TransferCounts), super::TransportAttemptError> {
    let meta = tokio::fs::symlink_metadata(args.local_path)
        .await
        .map_err(SshMcpError::Io)
        .map_err(super::TransportAttemptError::Other)?;
    if !meta.is_file() {
        return Err(super::TransportAttemptError::Other(
            SshMcpError::invalid_params("local_path is not a file"),
        ));
    }
    let size = meta.len();

    let stage = staging::remote_prepare_put_file_stage(
        args.conn,
        args.remote_home,
        args.remote_path,
        args.overwrite,
        args.id,
        args.timeout,
    )
    .await
    .map_err(super::TransportAttemptError::Other)?;

    let local_path_str = args.local_path.display().to_string();
    let remote = rsync_remote_spec(&endpoint, &stage.stage_path);

    run_rsync(
        &endpoint,
        &args.rsync_options,
        &local_path_str,
        &remote,
        args.timeout,
    )
    .await?;

    staging::remote_finalize_put_file(
        args.conn,
        args.remote_path,
        &stage.stage_path,
        args.overwrite,
        args.timeout,
    )
    .await
    .map_err(super::TransportAttemptError::Other)?;

    Ok((
        TransferStaging {
            local: None,
            remote: Some(StagingRemote {
                staging_path: stage.stage_path,
                backup_path: None,
                final_path: args.remote_path.to_string(),
                staging_base_home: stage.stage_base,
            }),
        },
        TransferCounts {
            bytes: size,
            files: 1,
            directories: 0,
        },
    ))
}

async fn get_file(
    endpoint: RsyncEndpoint,
    args: RsyncTransferArgs<'_>,
) -> std::result::Result<(TransferStaging, TransferCounts), super::TransportAttemptError> {
    exec_raw::validate_remote_user_file_path(args.remote_path, "remote_path")
        .map_err(super::TransportAttemptError::Other)?;

    let (tmp, f) =
        exec_raw::create_unique_local_staging_file(args.local_root, args.local_path, args.id)
            .await
            .map_err(super::TransportAttemptError::Other)?;
    drop(f);

    let remote = rsync_remote_spec(&endpoint, args.remote_path);
    let tmp_str = tmp.display().to_string();

    run_rsync(
        &endpoint,
        &args.rsync_options,
        &remote,
        &tmp_str,
        args.timeout,
    )
    .await?;

    let meta = tokio::fs::metadata(&tmp)
        .await
        .map_err(SshMcpError::Io)
        .map_err(super::TransportAttemptError::Other)?;
    let bytes = meta.len();

    if args.overwrite {
        exec_raw::atomic_replace_file(&tmp, args.local_path)
            .await
            .map_err(super::TransportAttemptError::Other)?;
    } else {
        exec_raw::atomic_install_file_overwrite_false(&tmp, args.local_path)
            .await
            .map_err(super::TransportAttemptError::Other)?;
    }

    Ok((
        TransferStaging {
            local: Some(StagingLocal {
                staging_path: tmp.display().to_string(),
                backup_path: None,
                final_path: args.local_path.display().to_string(),
            }),
            remote: None,
        },
        TransferCounts {
            bytes,
            files: 1,
            directories: 0,
        },
    ))
}

async fn count_local_dir_no_symlinks(root: &Path) -> Result<TransferCounts> {
    super::walk::count_dir_no_symlinks(root).await
}

async fn put_dir(
    endpoint: RsyncEndpoint,
    args: RsyncTransferArgs<'_>,
) -> std::result::Result<(TransferStaging, TransferCounts), super::TransportAttemptError> {
    let counts = count_local_dir_no_symlinks(args.local_path)
        .await
        .map_err(super::TransportAttemptError::Other)?;

    let stage = staging::remote_prepare_put_dir_stage(
        args.conn,
        args.remote_home,
        args.remote_path,
        args.overwrite,
        args.id,
        args.timeout,
    )
    .await
    .map_err(super::TransportAttemptError::Other)?;

    let local_dot = format!("{}/.", args.local_path.display());
    let remote = rsync_remote_spec(&endpoint, &stage.stage_path);

    run_rsync(
        &endpoint,
        &args.rsync_options,
        &local_dot,
        &remote,
        args.timeout,
    )
    .await?;

    let backup_path = if args.overwrite {
        staging::remote_finalize_put_dir_overwrite_true(
            args.conn,
            args.remote_home,
            args.remote_path,
            &stage.stage_path,
            args.id,
            args.timeout,
        )
        .await
        .map_err(super::TransportAttemptError::Other)?
    } else {
        None
    };

    Ok((
        TransferStaging {
            local: None,
            remote: Some(StagingRemote {
                staging_path: stage.stage_path,
                backup_path,
                final_path: args.remote_path.to_string(),
                staging_base_home: stage.stage_base,
            }),
        },
        counts,
    ))
}

async fn get_dir(
    endpoint: RsyncEndpoint,
    args: RsyncTransferArgs<'_>,
) -> std::result::Result<(TransferStaging, TransferCounts), super::TransportAttemptError> {
    exec_raw::validate_remote_user_path(args.remote_path, "remote_path")
        .map_err(super::TransportAttemptError::Other)?;

    staging::remote_validate_dir_contents(args.conn, args.remote_path, args.timeout)
        .await
        .map_err(super::TransportAttemptError::Other)?;

    let (extract_target, local_backup) = if args.overwrite {
        let stage =
            exec_raw::create_unique_local_staging_dir(args.local_root, args.local_path, args.id)
                .await
                .map_err(super::TransportAttemptError::Other)?;
        let backup = exec_raw::local_backup_dir_sibling(args.local_path, args.id);
        (stage, Some(backup))
    } else {
        match tokio::fs::create_dir(args.local_path).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(super::TransportAttemptError::Other(
                    SshMcpError::invalid_params(
                        "local destination exists and overwrite=false. Use overwrite=true to replace it.",
                    ),
                ));
            }
            Err(e) => return Err(super::TransportAttemptError::Other(SshMcpError::Io(e))),
        }
        (args.local_path.to_path_buf(), None)
    };

    let remote_dot = format!("{}/.", args.remote_path);
    let remote = rsync_remote_spec(&endpoint, &remote_dot);
    let extract_str = extract_target.display().to_string();

    let result = run_rsync(
        &endpoint,
        &args.rsync_options,
        &remote,
        &extract_str,
        args.timeout,
    )
    .await;

    if let Err(e) = result {
        let _ = tokio::fs::remove_dir_all(&extract_target).await;
        return Err(e);
    }

    let counts = count_local_dir_no_symlinks(&extract_target)
        .await
        .map_err(super::TransportAttemptError::Other)?;

    let (staging_path, backup_path) = if args.overwrite {
        let backup = local_backup.as_ref().ok_or_else(|| {
            super::TransportAttemptError::Other(SshMcpError::connection(
                "missing local backup path",
            ))
        })?;
        exec_raw::atomic_replace_dir(&extract_target, args.local_path, backup)
            .await
            .map_err(super::TransportAttemptError::Other)?;
        (
            extract_target.display().to_string(),
            Some(backup.display().to_string()),
        )
    } else {
        (args.local_path.display().to_string(), None)
    };

    Ok((
        TransferStaging {
            local: Some(StagingLocal {
                staging_path,
                backup_path,
                final_path: args.local_path.display().to_string(),
            }),
            remote: None,
        },
        counts,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_rsync_stats() {
        let output = r#"Number of files: 10 (reg: 8, dir: 2)
Number of created files: 10 (reg: 8, dir: 2)
Number of deleted files: 0
Number of regular files transferred: 8
Total file size: 1,234,567 bytes
Total transferred file size: 1,234,567 bytes
Literal data: 1,234,567 bytes
Matched data: 0 bytes
File list size: 0
File list generation time: 0.001 seconds
File list transfer time: 0.000 seconds
Total bytes sent: 1,235,890
Total bytes received: 172"#;

        let counts = parse_rsync_stats(output);
        assert_eq!(counts.files, 8);
        assert_eq!(counts.bytes, 1234567);
    }

    #[test]
    fn test_rsync_remote_spec() {
        let endpoint = RsyncEndpoint {
            host: "example.com".to_string(),
            port: 22,
            user: "alice".to_string(),
            key_path: None,
        };
        let spec = rsync_remote_spec(&endpoint, "/path/to/file.txt");
        assert_eq!(spec, "alice@example.com:/path/to/file.txt");
    }

    #[test]
    fn test_build_ssh_options() {
        let endpoint = RsyncEndpoint {
            host: "example.com".to_string(),
            port: 2222,
            user: "alice".to_string(),
            key_path: Some(PathBuf::from("/home/alice/.ssh/id_rsa")),
        };
        let opts = build_ssh_options(&endpoint);
        assert!(opts.contains("-p 2222"));
        assert!(opts.contains("-i /home/alice/.ssh/id_rsa"));
        assert!(opts.contains("BatchMode=yes"));
    }
}
