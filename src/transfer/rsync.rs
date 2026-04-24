use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

use crate::error::{Result, SshMcpError};
use crate::ssh::{HostKeyCheckMode, SshConnectionManager, escape_for_shell};

use super::process;
use super::skeleton;
use super::types::{
    RsyncOptions, TransferCounts, TransferKind, TransferOperation, TransferStaging,
};

// Staging/marker helpers live in `super::staging`.

#[derive(Debug, Clone)]
pub struct RsyncEndpoint {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub key_path: Option<PathBuf>,
    pub host_key_checking: HostKeyCheckMode,
    pub known_hosts: Option<PathBuf>,
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

    skeleton::dispatch_transfer(skeleton::DispatchTransferArgs {
        operation: args.operation,
        kind: args.kind,
        endpoint,
        args,
        put_file,
        get_file,
        put_dir,
        get_dir,
    })
    .await
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

#[cfg(unix)]
fn null_known_hosts_path() -> &'static str {
    "/dev/null"
}

#[cfg(windows)]
fn null_known_hosts_path() -> &'static str {
    "NUL"
}

fn build_ssh_options(endpoint: &RsyncEndpoint) -> String {
    let mut opts = vec![
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        format!(
            "StrictHostKeyChecking={}",
            endpoint.host_key_checking.as_openssh_value()
        ),
    ];

    match endpoint.host_key_checking {
        HostKeyCheckMode::No => {
            opts.push("-o".to_string());
            opts.push(format!("UserKnownHostsFile={}", null_known_hosts_path()));
        }
        HostKeyCheckMode::Yes | HostKeyCheckMode::AcceptNew => {
            if let Some(path) = &endpoint.known_hosts {
                let path_str = path.display().to_string();
                opts.push("-o".to_string());
                opts.push(format!(
                    "UserKnownHostsFile='{}'",
                    escape_for_shell(&path_str)
                ));
            }
        }
    }

    opts.push("-o".to_string());
    opts.push("LogLevel=ERROR".to_string());

    if endpoint.port != 22 {
        opts.push("-p".to_string());
        opts.push(endpoint.port.to_string());
    }

    if let Some(ref key) = endpoint.key_path {
        opts.push("-i".to_string());
        let key_str = key.display().to_string();
        let escaped = escape_for_shell(&key_str);
        // rsync -e passes a single command string; ensure key_path stays a single token.
        opts.push(format!("'{}'", escaped));
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
    let RsyncTransferArgs {
        conn,
        remote_home,
        local_root: _,
        id,
        timeout,
        operation: _,
        kind: _,
        local_path,
        remote_path,
        overwrite,
        rsync_options,
    } = args;

    let local_path_str = local_path.display().to_string();
    let remote_path = remote_path.to_string();

    skeleton::put_file_with_remote_staging(
        skeleton::PutFileWithRemoteStagingArgs {
            conn,
            remote_home,
            remote_path,
            overwrite,
            id,
            timeout,
            local_path,
        },
        move |stage_path| async move {
            let remote = rsync_remote_spec(&endpoint, &stage_path);
            run_rsync(&endpoint, &rsync_options, &local_path_str, &remote, timeout)
                .await
                .map(|_| ())
        },
    )
    .await
}

async fn get_file(
    endpoint: RsyncEndpoint,
    args: RsyncTransferArgs<'_>,
) -> std::result::Result<(TransferStaging, TransferCounts), super::TransportAttemptError> {
    let RsyncTransferArgs {
        conn: _,
        remote_home: _,
        local_root,
        id,
        timeout,
        operation: _,
        kind: _,
        local_path,
        remote_path,
        overwrite,
        rsync_options,
    } = args;

    let remote = rsync_remote_spec(&endpoint, remote_path);

    skeleton::get_file_with_local_staging(
        skeleton::GetFileWithLocalStagingArgs {
            local_root,
            local_path,
            remote_path,
            overwrite,
            id,
        },
        move |tmp_path| async move {
            run_rsync(&endpoint, &rsync_options, &remote, &tmp_path, timeout)
                .await
                .map(|_| ())
        },
    )
    .await
}

async fn count_local_dir_no_symlinks(root: &Path) -> Result<TransferCounts> {
    super::walk::count_dir_no_symlinks(root).await
}

async fn put_dir(
    endpoint: RsyncEndpoint,
    args: RsyncTransferArgs<'_>,
) -> std::result::Result<(TransferStaging, TransferCounts), super::TransportAttemptError> {
    let RsyncTransferArgs {
        conn,
        remote_home,
        id,
        timeout,
        local_path,
        remote_path,
        overwrite,
        rsync_options,
        ..
    } = args;

    let counts = count_local_dir_no_symlinks(local_path)
        .await
        .map_err(super::TransportAttemptError::Other)?;

    let remote_path = remote_path.to_string();

    skeleton::put_dir_with_remote_staging(
        skeleton::PutDirWithRemoteStagingArgs {
            conn,
            remote_home,
            remote_path,
            overwrite,
            id,
            timeout,
            counts,
        },
        move |stage_path| async move {
            let local_dot = format!("{}/.", local_path.display());
            let remote = rsync_remote_spec(&endpoint, &stage_path);
            run_rsync(&endpoint, &rsync_options, &local_dot, &remote, timeout)
                .await
                .map(|_| ())
        },
    )
    .await
}

async fn get_dir(
    endpoint: RsyncEndpoint,
    args: RsyncTransferArgs<'_>,
) -> std::result::Result<(TransferStaging, TransferCounts), super::TransportAttemptError> {
    let RsyncTransferArgs {
        conn,
        remote_home: _,
        local_root,
        id,
        timeout,
        operation: _,
        kind: _,
        local_path,
        remote_path,
        overwrite,
        rsync_options,
    } = args;

    let remote_dot = format!("{}/.", remote_path);
    let remote = rsync_remote_spec(&endpoint, &remote_dot);

    skeleton::get_dir_with_local_staging(
        skeleton::GetDirWithLocalStagingArgs {
            conn,
            local_root,
            local_path,
            remote_path,
            overwrite,
            id,
            timeout,
        },
        move |extract_target| async move {
            run_rsync(&endpoint, &rsync_options, &remote, &extract_target, timeout)
                .await
                .map(|_| ())
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ssh::escape_for_shell;

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
            host_key_checking: HostKeyCheckMode::No,
            known_hosts: None,
        };
        let spec = rsync_remote_spec(&endpoint, "/path/to/file.txt");
        assert_eq!(spec, "alice@example.com:/path/to/file.txt");
    }

    #[test]
    fn test_build_ssh_options() {
        let key_path = if cfg!(windows) {
            PathBuf::from(r"C:\Users\Alice\My Keys\id_rsa")
        } else {
            PathBuf::from("/home/alice/my keys/id_rsa")
        };

        let endpoint = RsyncEndpoint {
            host: "example.com".to_string(),
            port: 2222,
            user: "alice".to_string(),
            key_path: Some(key_path.clone()),
            host_key_checking: HostKeyCheckMode::No,
            known_hosts: None,
        };
        let opts = build_ssh_options(&endpoint);
        assert!(opts.contains("-p 2222"));

        let key_str = key_path.display().to_string();
        assert!(opts.contains(&format!("-i '{}'", escape_for_shell(&key_str))));
        assert!(opts.contains("BatchMode=yes"));

        let null_hosts = if cfg!(windows) { "NUL" } else { "/dev/null" };
        assert!(opts.contains(&format!("UserKnownHostsFile={null_hosts}")));
    }

    #[test]
    fn test_build_ssh_options_accept_new_known_hosts() {
        let endpoint = RsyncEndpoint {
            host: "example.com".to_string(),
            port: 22,
            user: "alice".to_string(),
            key_path: None,
            host_key_checking: HostKeyCheckMode::AcceptNew,
            known_hosts: Some(PathBuf::from("/tmp/my known_hosts")),
        };
        let opts = build_ssh_options(&endpoint);
        assert!(opts.contains("StrictHostKeyChecking=accept-new"));
        assert!(opts.contains("UserKnownHostsFile='/tmp/my known_hosts'"));
    }
}
