use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::error::{Result, SshMcpError};
use crate::ssh::{SshConnectionManager, escape_for_shell};

use super::process;
use super::skeleton;
use super::types::{TransferCounts, TransferKind, TransferOperation, TransferStaging};

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
            .map_err(super::io_to_transport_attempt)?;
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

fn scp_legacy_args(extra: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut args = vec!["-O".to_string()];
    args.extend(extra);
    args
}

fn scp_receive_args(extra: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut args = vec!["-T".to_string()];
    args.extend(extra);
    args
}

async fn remove_remote_dir(
    conn: &SshConnectionManager,
    timeout: Duration,
    path: &str,
) -> std::result::Result<(), super::TransportAttemptError> {
    super::exec_raw::validate_remote_user_path(path, "remote_stage")
        .map_err(super::TransportAttemptError::Other)?;

    let escaped = escape_for_shell(path);
    let cmd = format!(r#"sh -c 'set -eu; rm -rf -- "$1"' sh '{escaped}'"#);
    let out = conn
        .exec_command(&cmd, timeout)
        .await
        .map_err(super::TransportAttemptError::Other)?;
    super::staging::ensure_remote_exec_success("reset scp remote directory", &out)
        .map_err(super::TransportAttemptError::Other)
}

async fn remove_local_dir(path: &Path) -> std::result::Result<(), super::TransportAttemptError> {
    match fs::remove_dir_all(path).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(super::TransportAttemptError::Other(SshMcpError::Io(err))),
    }
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
    let OpenSshTransferArgs {
        transport,
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
    } = args;

    let local_path_str = local_path.display().to_string();

    skeleton::put_file_with_remote_staging(
        skeleton::PutFileWithRemoteStagingArgs {
            conn,
            remote_home,
            remote_path,
            overwrite,
            id,
            timeout,
            local_path: &local_path,
        },
        move |stage_path| async move {
            match transport {
                OpenSshTransport::Sftp => {
                    let batch = format!(
                        "put {} {}\n",
                        sftp_quote_token(&local_path_str),
                        sftp_quote_token(&stage_path)
                    );
                    let out = run_sftp_batch(&endpoint, &batch, timeout).await?;
                    if !out.status.success() {
                        return Err(classify_openssh_failure(OpenSshTransport::Sftp, &out));
                    }
                }
                OpenSshTransport::Scp => {
                    let remote = scp_remote_spec(&endpoint, &stage_path);
                    let try_o = scp_legacy_args([local_path_str.clone(), remote.clone()]);
                    let out_o = run_scp(&endpoint, &try_o, timeout).await?;
                    if !out_o.status.success() {
                        let classified = classify_openssh_failure(OpenSshTransport::Scp, &out_o);
                        if matches!(classified, super::TransportAttemptError::Unsupported { .. }) {
                            let no_o = vec![local_path_str, remote];
                            let out = run_scp(&endpoint, &no_o, timeout).await?;
                            if !out.status.success() {
                                return Err(classify_openssh_failure(OpenSshTransport::Scp, &out));
                            }
                        } else {
                            return Err(classified);
                        }
                    }
                }
            }

            Ok(())
        },
    )
    .await
}

async fn get_file(
    endpoint: OpenSshEndpoint,
    args: OpenSshTransferArgs<'_>,
) -> std::result::Result<(TransferStaging, TransferCounts), super::TransportAttemptError> {
    let OpenSshTransferArgs {
        transport,
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
    } = args;

    let remote_path_for_download = remote_path.clone();

    skeleton::get_file_with_local_staging(
        skeleton::GetFileWithLocalStagingArgs {
            local_root,
            local_path: &local_path,
            remote_path: remote_path.as_str(),
            overwrite,
            id,
        },
        move |tmp_path| async move {
            match transport {
                OpenSshTransport::Sftp => {
                    let batch = format!(
                        "get {} {}\n",
                        sftp_quote_token(&remote_path_for_download),
                        sftp_quote_token(&tmp_path)
                    );
                    let out = run_sftp_batch(&endpoint, &batch, timeout).await?;
                    if !out.status.success() {
                        return Err(classify_openssh_failure(OpenSshTransport::Sftp, &out));
                    }
                }
                OpenSshTransport::Scp => {
                    let remote = scp_remote_spec(&endpoint, &remote_path_for_download);
                    let try_o = scp_legacy_args(scp_receive_args([remote.clone(), tmp_path.clone()]));
                    let out_o = run_scp(&endpoint, &try_o, timeout).await?;
                    if !out_o.status.success() {
                        let classified = classify_openssh_failure(OpenSshTransport::Scp, &out_o);
                        if matches!(classified, super::TransportAttemptError::Unsupported { .. }) {
                            let no_o = scp_receive_args([remote, tmp_path]);
                            let out = run_scp(&endpoint, &no_o, timeout).await?;
                            if !out.status.success() {
                                return Err(classify_openssh_failure(OpenSshTransport::Scp, &out));
                            }
                        } else {
                            return Err(classified);
                        }
                    }
                }
            }

            Ok(())
        },
    )
    .await
}

async fn count_dir_no_symlinks(root: &Path) -> Result<TransferCounts> {
    super::walk::count_dir_no_symlinks(root).await
}

async fn put_dir(
    endpoint: OpenSshEndpoint,
    args: OpenSshTransferArgs<'_>,
) -> std::result::Result<(TransferStaging, TransferCounts), super::TransportAttemptError> {
    let OpenSshTransferArgs {
        transport,
        conn,
        remote_home,
        id,
        timeout,
        local_path,
        remote_path,
        overwrite,
        ..
    } = args;

    let local_path_for_scp = fs::canonicalize(&local_path)
        .await
        .map_err(super::io_to_transport_attempt)?;
    let local_path_for_scp = local_path_for_scp.display().to_string();

    let counts = count_dir_no_symlinks(&local_path)
        .await
        .map_err(super::TransportAttemptError::Other)?;

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
            match transport {
                OpenSshTransport::Sftp => {
                    let local_dot = format!("{}/.", local_path.display());
                    let batch = format!(
                        "put -r {} {}\n",
                        sftp_quote_token(&local_dot),
                        sftp_quote_token(&stage_path)
                    );
                    let out = run_sftp_batch(&endpoint, &batch, timeout).await?;
                    if !out.status.success() {
                        return Err(classify_openssh_failure(OpenSshTransport::Sftp, &out));
                    }
                }
                OpenSshTransport::Scp => {
                    remove_remote_dir(conn, timeout, &stage_path).await?;
                    let remote = scp_remote_spec(&endpoint, &stage_path);
                    let try_o = scp_legacy_args([
                        "-r".to_string(),
                        local_path_for_scp.clone(),
                        remote.clone(),
                    ]);
                    let out_o = run_scp(&endpoint, &try_o, timeout).await?;
                    if !out_o.status.success() {
                        let classified = classify_openssh_failure(OpenSshTransport::Scp, &out_o);
                        if matches!(classified, super::TransportAttemptError::Unsupported { .. }) {
                            let no_o = vec!["-r".to_string(), local_path_for_scp, remote];
                            let out = run_scp(&endpoint, &no_o, timeout).await?;
                            if !out.status.success() {
                                return Err(classify_openssh_failure(OpenSshTransport::Scp, &out));
                            }
                        } else {
                            return Err(classified);
                        }
                    }
                }
            }

            Ok(())
        },
    )
    .await
}

async fn get_dir(
    endpoint: OpenSshEndpoint,
    args: OpenSshTransferArgs<'_>,
) -> std::result::Result<(TransferStaging, TransferCounts), super::TransportAttemptError> {
    let OpenSshTransferArgs {
        transport,
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
    } = args;

    let remote_path_for_download = remote_path.clone();

    skeleton::get_dir_with_local_staging(
        skeleton::GetDirWithLocalStagingArgs {
            conn,
            local_root,
            local_path: &local_path,
            remote_path: remote_path.as_str(),
            overwrite,
            id,
            timeout,
        },
        move |extract_target| async move {
            match transport {
                OpenSshTransport::Sftp => {
                    let remote_dot = format!("{}/.", remote_path_for_download);
                    let batch = format!(
                        "get -r {} {}\n",
                        sftp_quote_token(&remote_dot),
                        sftp_quote_token(&extract_target)
                    );
                    let out = run_sftp_batch(&endpoint, &batch, timeout).await?;
                    if !out.status.success() {
                        return Err(classify_openssh_failure(OpenSshTransport::Sftp, &out));
                    }
                }
                OpenSshTransport::Scp => {
                    remove_local_dir(Path::new(&extract_target)).await?;
                    let remote = scp_remote_spec(&endpoint, &remote_path_for_download);
                    let try_o = scp_legacy_args(scp_receive_args([
                        "-r".to_string(),
                        remote.clone(),
                        extract_target.clone(),
                    ]));
                    let out_o = run_scp(&endpoint, &try_o, timeout).await?;
                    if !out_o.status.success() {
                        let classified = classify_openssh_failure(OpenSshTransport::Scp, &out_o);
                        if matches!(classified, super::TransportAttemptError::Unsupported { .. }) {
                            let no_o = scp_receive_args([
                                "-r".to_string(),
                                remote,
                                extract_target,
                            ]);
                            let out = run_scp(&endpoint, &no_o, timeout).await?;
                            if !out.status.success() {
                                return Err(classify_openssh_failure(OpenSshTransport::Scp, &out));
                            }
                        } else {
                            return Err(classified);
                        }
                    }
                }
            }

            Ok(())
        },
    )
    .await
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
