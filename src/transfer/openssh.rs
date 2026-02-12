use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::error::{Result, SshMcpError};
use crate::ssh::{SshConnectionManager, escape_for_shell};

use super::exec_raw;
use super::process;
use super::staging;
use super::types::{
    StagingLocal, StagingRemote, TransferCounts, TransferKind, TransferOperation, TransferStaging,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenSshTransport {
    Sftp,
    Scp,
}

#[derive(Debug, Clone)]
pub struct OpenSshEndpoint {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub key_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct OpenSshTransferArgs<'a> {
    pub transport: OpenSshTransport,
    pub conn: &'a SshConnectionManager,
    pub remote_home: &'a str,
    pub local_root: &'a Path,
    pub id: u64,
    pub timeout: Duration,
    pub operation: TransferOperation,
    pub kind: TransferKind,
    pub local_path: PathBuf,
    pub remote_path: String,
    pub overwrite: bool,
}

// Staging/marker helpers live in `super::staging`.

pub async fn run_transfer(
    endpoint: OpenSshEndpoint,
    args: OpenSshTransferArgs<'_>,
) -> std::result::Result<(TransferStaging, TransferCounts), super::TransportAttemptError> {
    match (args.operation, args.kind) {
        (TransferOperation::Put, TransferKind::File) => put_file(endpoint, args).await,
        (TransferOperation::Get, TransferKind::File) => get_file(endpoint, args).await,
        (TransferOperation::Put, TransferKind::Directory) => put_dir(endpoint, args).await,
        (TransferOperation::Get, TransferKind::Directory) => get_dir(endpoint, args).await,
    }
}

// Remote staging helpers are implemented in `super::staging`.

fn sftp_quote_token(s: &str) -> String {
    // sftp batch mode supports double-quoted tokens.
    // Avoid relying on local shell quoting.
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

fn scp_remote_spec(endpoint: &OpenSshEndpoint, remote_path: &str) -> String {
    // scp uses remote shell parsing for the path portion. Single-quote it.
    let escaped = escape_for_shell(remote_path);
    format!("{}@{}:'{}'", endpoint.user, endpoint.host, escaped)
}

#[cfg(unix)]
fn null_known_hosts_path() -> &'static str {
    "/dev/null"
}

#[cfg(windows)]
fn null_known_hosts_path() -> &'static str {
    "NUL"
}

fn common_ssh_options(endpoint: &OpenSshEndpoint) -> Vec<String> {
    vec![
        "-i".to_string(),
        endpoint.key_path.display().to_string(),
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "PasswordAuthentication=no".to_string(),
        "-o".to_string(),
        "KbdInteractiveAuthentication=no".to_string(),
        "-o".to_string(),
        "PreferredAuthentications=publickey".to_string(),
        "-o".to_string(),
        "StrictHostKeyChecking=no".to_string(),
        "-o".to_string(),
        format!("UserKnownHostsFile={}", null_known_hosts_path()),
        "-o".to_string(),
        "LogLevel=ERROR".to_string(),
    ]
}

async fn run_sftp_batch(
    endpoint: &OpenSshEndpoint,
    batch: &str,
    timeout: Duration,
) -> std::result::Result<ProcessOutput, super::TransportAttemptError> {
    let mut cmd = Command::new("sftp");
    cmd.arg("-P").arg(endpoint.port.to_string());
    for opt in common_ssh_options(endpoint) {
        cmd.arg(opt);
    }
    cmd.arg("-b").arg("-");
    cmd.arg(format!("{}@{}", endpoint.user, endpoint.host));
    cmd.env("LC_ALL", "C");
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| classify_spawn_error(OpenSshTransport::Sftp, e))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(batch.as_bytes())
            .await
            .map_err(SshMcpError::Io)
            .map_err(super::TransportAttemptError::Other)?;
        let _ = stdin.shutdown().await;
    }

    wait_child_with_timeout(OpenSshTransport::Sftp, child, timeout).await
}

async fn run_scp(
    endpoint: &OpenSshEndpoint,
    args: &[String],
    timeout: Duration,
) -> std::result::Result<ProcessOutput, super::TransportAttemptError> {
    let mut cmd = Command::new("scp");
    cmd.arg("-P").arg(endpoint.port.to_string());
    for opt in common_ssh_options(endpoint) {
        cmd.arg(opt);
    }
    for a in args {
        cmd.arg(a);
    }
    cmd.env("LC_ALL", "C");
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let child = cmd
        .spawn()
        .map_err(|e| classify_spawn_error(OpenSshTransport::Scp, e))?;
    wait_child_with_timeout(OpenSshTransport::Scp, child, timeout).await
}

#[derive(Debug)]
struct ProcessOutput {
    status: std::process::ExitStatus,
    stderr: String,
}

async fn wait_child_with_timeout(
    _transport: OpenSshTransport,
    child: tokio::process::Child,
    timeout: Duration,
) -> std::result::Result<ProcessOutput, super::TransportAttemptError> {
    let captured = process::wait_child_with_timeout(child, timeout).await?;
    Ok(ProcessOutput {
        status: captured.status,
        stderr: String::from_utf8_lossy(&captured.stderr).to_string(),
    })
}

fn classify_spawn_error(
    transport: OpenSshTransport,
    err: std::io::Error,
) -> super::TransportAttemptError {
    let (bin, transfer_transport) = match transport {
        OpenSshTransport::Sftp => ("sftp", super::TransferTransport::Sftp),
        OpenSshTransport::Scp => ("scp", super::TransferTransport::Scp),
    };

    process::classify_spawn_error_with_reason(
        err,
        transfer_transport,
        format!("missing local OpenSSH binary '{bin}'"),
    )
}

fn classify_openssh_failure(
    transport: OpenSshTransport,
    out: &ProcessOutput,
) -> super::TransportAttemptError {
    let exit_code = out.status.code();

    let stderr = out.stderr.as_str();

    if matches!(transport, OpenSshTransport::Sftp)
        && exit_code == Some(255)
        && (stderr.contains("subsystem request failed")
            || stderr.contains("Subsystem request failed")
            || stderr.contains("Unknown subsystem")
            || stderr.contains("unknown subsystem"))
    {
        return super::TransportAttemptError::Unsupported {
            transport: super::TransferTransport::Sftp,
            reason: stderr.trim().to_string(),
        };
    }

    if matches!(transport, OpenSshTransport::Scp)
        && (stderr.contains("unknown option -- O")
            || stderr.contains("illegal option -- O")
            || stderr.contains("unknown option: -O")
            || stderr.contains("unrecognized option") && stderr.contains("-O"))
    {
        return super::TransportAttemptError::Unsupported {
            transport: super::TransferTransport::Scp,
            reason: "scp -O flag unsupported".to_string(),
        };
    }

    if matches!(transport, OpenSshTransport::Scp)
        && (exit_code == Some(127)
            || stderr.contains("scp: not found")
            || stderr.contains("scp: command not found"))
    {
        return super::TransportAttemptError::Unsupported {
            transport: super::TransferTransport::Scp,
            reason: stderr.trim().to_string(),
        };
    }

    super::TransportAttemptError::Other(SshMcpError::connection(format!(
        "OpenSSH transport failed: exit_code={exit_code:?}; stderr={}",
        stderr.trim()
    )))
}

// Remote staging helpers are implemented in `super::staging`.

async fn put_file(
    endpoint: OpenSshEndpoint,
    args: OpenSshTransferArgs<'_>,
) -> std::result::Result<(TransferStaging, TransferCounts), super::TransportAttemptError> {
    let meta = tokio::fs::symlink_metadata(&args.local_path)
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
        &args.remote_path,
        args.overwrite,
        args.id,
        args.timeout,
    )
    .await
    .map_err(super::TransportAttemptError::Other)?;

    match args.transport {
        OpenSshTransport::Sftp => {
            let batch = format!(
                "put {} {}\n",
                sftp_quote_token(&args.local_path.display().to_string()),
                sftp_quote_token(&stage.stage_path)
            );
            let out = run_sftp_batch(&endpoint, &batch, args.timeout).await?;
            if !out.status.success() {
                return Err(classify_openssh_failure(OpenSshTransport::Sftp, &out));
            }
        }
        OpenSshTransport::Scp => {
            let remote = scp_remote_spec(&endpoint, &stage.stage_path);
            let try_o = vec![
                "-O".to_string(),
                args.local_path.display().to_string(),
                remote.clone(),
            ];
            let out_o = run_scp(&endpoint, &try_o, args.timeout).await?;
            if !out_o.status.success() {
                let classified = classify_openssh_failure(OpenSshTransport::Scp, &out_o);
                if matches!(classified, super::TransportAttemptError::Unsupported { .. }) {
                    let no_o = vec![args.local_path.display().to_string(), remote];
                    let out = run_scp(&endpoint, &no_o, args.timeout).await?;
                    if !out.status.success() {
                        return Err(classify_openssh_failure(OpenSshTransport::Scp, &out));
                    }
                } else {
                    return Err(classified);
                }
            }
        }
    }

    staging::remote_finalize_put_file(
        args.conn,
        &args.remote_path,
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
                final_path: args.remote_path,
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
    endpoint: OpenSshEndpoint,
    args: OpenSshTransferArgs<'_>,
) -> std::result::Result<(TransferStaging, TransferCounts), super::TransportAttemptError> {
    exec_raw::validate_remote_user_file_path(&args.remote_path, "remote_path")
        .map_err(super::TransportAttemptError::Other)?;

    let (tmp, f) =
        exec_raw::create_unique_local_staging_file(args.local_root, &args.local_path, args.id)
            .await
            .map_err(super::TransportAttemptError::Other)?;
    drop(f);

    match args.transport {
        OpenSshTransport::Sftp => {
            let batch = format!(
                "get {} {}\n",
                sftp_quote_token(&args.remote_path),
                sftp_quote_token(&tmp.display().to_string())
            );
            let out = run_sftp_batch(&endpoint, &batch, args.timeout).await?;
            if !out.status.success() {
                let _ = tokio::fs::remove_file(&tmp).await;
                return Err(classify_openssh_failure(OpenSshTransport::Sftp, &out));
            }
        }
        OpenSshTransport::Scp => {
            let remote = scp_remote_spec(&endpoint, &args.remote_path);
            let try_o = vec!["-O".to_string(), remote.clone(), tmp.display().to_string()];
            let out_o = run_scp(&endpoint, &try_o, args.timeout).await?;
            if !out_o.status.success() {
                let classified = classify_openssh_failure(OpenSshTransport::Scp, &out_o);
                if matches!(classified, super::TransportAttemptError::Unsupported { .. }) {
                    let no_o = vec![remote, tmp.display().to_string()];
                    let out = run_scp(&endpoint, &no_o, args.timeout).await?;
                    if !out.status.success() {
                        let _ = tokio::fs::remove_file(&tmp).await;
                        return Err(classify_openssh_failure(OpenSshTransport::Scp, &out));
                    }
                } else {
                    let _ = tokio::fs::remove_file(&tmp).await;
                    return Err(classified);
                }
            }
        }
    }

    let meta = tokio::fs::metadata(&tmp)
        .await
        .map_err(SshMcpError::Io)
        .map_err(super::TransportAttemptError::Other)?;
    let bytes = meta.len();

    if args.overwrite {
        exec_raw::atomic_replace_file(&tmp, &args.local_path)
            .await
            .map_err(super::TransportAttemptError::Other)?;
    } else {
        exec_raw::atomic_install_file_overwrite_false(&tmp, &args.local_path)
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

async fn count_dir_no_symlinks(root: &Path) -> Result<TransferCounts> {
    super::walk::count_dir_no_symlinks(root).await
}

async fn put_dir(
    endpoint: OpenSshEndpoint,
    args: OpenSshTransferArgs<'_>,
) -> std::result::Result<(TransferStaging, TransferCounts), super::TransportAttemptError> {
    let counts = count_dir_no_symlinks(&args.local_path)
        .await
        .map_err(super::TransportAttemptError::Other)?;

    let stage = staging::remote_prepare_put_dir_stage(
        args.conn,
        args.remote_home,
        &args.remote_path,
        args.overwrite,
        args.id,
        args.timeout,
    )
    .await
    .map_err(super::TransportAttemptError::Other)?;

    let upload_target = stage.stage_path.clone();

    match args.transport {
        OpenSshTransport::Sftp => {
            let local_dot = format!("{}/.", args.local_path.display());
            let batch = format!(
                "put -r {} {}\n",
                sftp_quote_token(&local_dot),
                sftp_quote_token(&upload_target)
            );
            let out = run_sftp_batch(&endpoint, &batch, args.timeout).await?;
            if !out.status.success() {
                return Err(classify_openssh_failure(OpenSshTransport::Sftp, &out));
            }
        }
        OpenSshTransport::Scp => {
            let local_dot = format!("{}/.", args.local_path.display());
            let remote = scp_remote_spec(&endpoint, &upload_target);
            let try_o = vec![
                "-O".to_string(),
                "-r".to_string(),
                local_dot.clone(),
                remote.clone(),
            ];
            let out_o = run_scp(&endpoint, &try_o, args.timeout).await?;
            if !out_o.status.success() {
                let classified = classify_openssh_failure(OpenSshTransport::Scp, &out_o);
                if matches!(classified, super::TransportAttemptError::Unsupported { .. }) {
                    let no_o = vec!["-r".to_string(), local_dot, remote];
                    let out = run_scp(&endpoint, &no_o, args.timeout).await?;
                    if !out.status.success() {
                        return Err(classify_openssh_failure(OpenSshTransport::Scp, &out));
                    }
                } else {
                    return Err(classified);
                }
            }
        }
    }

    let backup_path = if args.overwrite {
        staging::remote_finalize_put_dir_overwrite_true(
            args.conn,
            args.remote_home,
            &args.remote_path,
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
                final_path: args.remote_path,
                staging_base_home: stage.stage_base,
            }),
        },
        counts,
    ))
}

async fn get_dir(
    endpoint: OpenSshEndpoint,
    args: OpenSshTransferArgs<'_>,
) -> std::result::Result<(TransferStaging, TransferCounts), super::TransportAttemptError> {
    exec_raw::validate_remote_user_path(&args.remote_path, "remote_path")
        .map_err(super::TransportAttemptError::Other)?;

    staging::remote_validate_dir_contents(args.conn, &args.remote_path, args.timeout)
        .await
        .map_err(super::TransportAttemptError::Other)?;

    let (extract_target, local_backup) = if args.overwrite {
        let stage =
            exec_raw::create_unique_local_staging_dir(args.local_root, &args.local_path, args.id)
                .await
                .map_err(super::TransportAttemptError::Other)?;
        let backup = exec_raw::local_backup_dir_sibling(&args.local_path, args.id);
        (stage, Some(backup))
    } else {
        match tokio::fs::create_dir(&args.local_path).await {
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
        (args.local_path.clone(), None)
    };

    match args.transport {
        OpenSshTransport::Sftp => {
            let remote_dot = format!("{}/.", args.remote_path);
            let batch = format!(
                "get -r {} {}\n",
                sftp_quote_token(&remote_dot),
                sftp_quote_token(&extract_target.display().to_string())
            );
            let out = run_sftp_batch(&endpoint, &batch, args.timeout).await?;
            if !out.status.success() {
                let _ = tokio::fs::remove_dir_all(&extract_target).await;
                return Err(classify_openssh_failure(OpenSshTransport::Sftp, &out));
            }
        }
        OpenSshTransport::Scp => {
            let remote_dot = format!("{}/.", args.remote_path);
            let remote = scp_remote_spec(&endpoint, &remote_dot);
            let try_o = vec![
                "-O".to_string(),
                "-r".to_string(),
                remote.clone(),
                extract_target.display().to_string(),
            ];
            let out_o = run_scp(&endpoint, &try_o, args.timeout).await?;
            if !out_o.status.success() {
                let classified = classify_openssh_failure(OpenSshTransport::Scp, &out_o);
                if matches!(classified, super::TransportAttemptError::Unsupported { .. }) {
                    let no_o = vec![
                        "-r".to_string(),
                        remote,
                        extract_target.display().to_string(),
                    ];
                    let out = run_scp(&endpoint, &no_o, args.timeout).await?;
                    if !out.status.success() {
                        let _ = tokio::fs::remove_dir_all(&extract_target).await;
                        return Err(classify_openssh_failure(OpenSshTransport::Scp, &out));
                    }
                } else {
                    let _ = tokio::fs::remove_dir_all(&extract_target).await;
                    return Err(classified);
                }
            }
        }
    }

    let counts = count_dir_no_symlinks(&extract_target)
        .await
        .map_err(super::TransportAttemptError::Other)?;

    let (staging_path, backup_path) = if args.overwrite {
        let backup = local_backup.as_ref().ok_or_else(|| {
            super::TransportAttemptError::Other(SshMcpError::connection(
                "missing local backup path",
            ))
        })?;
        exec_raw::atomic_replace_dir(&extract_target, &args.local_path, backup)
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
    fn test_sftp_quote_token() {
        assert_eq!(sftp_quote_token("simple"), "\"simple\"");
        assert_eq!(sftp_quote_token("a b"), "\"a b\"");
        assert_eq!(sftp_quote_token("a\\b\"c"), "\"a\\\\b\\\"c\"");
    }

    #[test]
    fn test_scp_remote_spec_single_quotes_and_escapes() {
        let endpoint = OpenSshEndpoint {
            host: "example.com".to_string(),
            port: 22,
            user: "alice".to_string(),
            key_path: PathBuf::from("/k"),
        };
        let spec = scp_remote_spec(&endpoint, "/path/with space/it's.txt");
        assert_eq!(spec, "alice@example.com:'/path/with space/it'\"'\"'s.txt'");
    }
}
