use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::error::{Result, SshMcpError};
use crate::ssh::{SshConnectionManager, escape_for_shell};

use super::exec_raw;
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

const REMOTE_STAGING_BASE_SUFFIX: &str = "/.ssh-mcp/staging";
const STAGE_MARKER: &str = "__SSH_MCP_STAGE=";
const STAGE_BASE_MARKER: &str = "__SSH_MCP_STAGE_BASE=";
const BACKUP_MARKER: &str = "__SSH_MCP_BACKUP=";
const ERR_MARKER: &str = "__SSH_MCP_ERR=";

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

fn parse_marker_value(stderr: &str, prefix: &str) -> Option<String> {
    stderr
        .lines()
        .find_map(|line| line.strip_prefix(prefix).map(|v| v.to_string()))
}

fn remote_home_staging_base(remote_home: &str) -> String {
    format!("{remote_home}{REMOTE_STAGING_BASE_SUFFIX}")
}

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
    mut child: tokio::process::Child,
    timeout: Duration,
) -> std::result::Result<ProcessOutput, super::TransportAttemptError> {
    let mut stdout_pipe = child.stdout.take().ok_or_else(|| {
        super::TransportAttemptError::Other(SshMcpError::connection("missing stdout pipe"))
    })?;
    let mut stderr_pipe = child.stderr.take().ok_or_else(|| {
        super::TransportAttemptError::Other(SshMcpError::connection("missing stderr pipe"))
    })?;

    let stdout_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        stdout_pipe.read_to_end(&mut buf).await?;
        Ok::<Vec<u8>, std::io::Error>(buf)
    });

    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        stderr_pipe.read_to_end(&mut buf).await?;
        Ok::<Vec<u8>, std::io::Error>(buf)
    });

    let sleep = tokio::time::sleep(timeout);
    tokio::pin!(sleep);

    let status = tokio::select! {
        res = child.wait() => {
            res.map_err(SshMcpError::Io).map_err(super::TransportAttemptError::Other)?
        }
        _ = &mut sleep => {
            stdout_task.abort();
            stderr_task.abort();
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(super::TransportAttemptError::Other(SshMcpError::Timeout(
                timeout.as_millis() as u64,
            )));
        }
    };

    let _stdout_bytes = match stdout_task.await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return Err(super::TransportAttemptError::Other(SshMcpError::Io(e))),
        Err(_) => {
            return Err(super::TransportAttemptError::Other(
                SshMcpError::connection("stdout task join failed"),
            ));
        }
    };

    let stderr_bytes = match stderr_task.await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return Err(super::TransportAttemptError::Other(SshMcpError::Io(e))),
        Err(_) => {
            return Err(super::TransportAttemptError::Other(
                SshMcpError::connection("stderr task join failed"),
            ));
        }
    };

    Ok(ProcessOutput {
        status,
        stderr: String::from_utf8_lossy(&stderr_bytes).to_string(),
    })
}

fn classify_spawn_error(
    transport: OpenSshTransport,
    err: std::io::Error,
) -> super::TransportAttemptError {
    if err.kind() == std::io::ErrorKind::NotFound {
        let bin = match transport {
            OpenSshTransport::Sftp => "sftp",
            OpenSshTransport::Scp => "scp",
        };
        return super::TransportAttemptError::Unsupported {
            transport: match transport {
                OpenSshTransport::Sftp => super::TransferTransport::Sftp,
                OpenSshTransport::Scp => super::TransferTransport::Scp,
            },
            reason: format!("missing local OpenSSH binary '{bin}'"),
        };
    }
    super::TransportAttemptError::Other(SshMcpError::Io(err))
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

fn ensure_remote_exec_success(what: &str, out: &crate::ssh::CommandOutput) -> Result<()> {
    match out.exit_code {
        Some(0) => Ok(()),
        Some(code) => {
            if let Some(err) = parse_marker_value(&out.stderr, ERR_MARKER) {
                match err.trim() {
                    "destination_exists" => {
                        return Err(SshMcpError::invalid_params(
                            "destination exists and overwrite=false",
                        ));
                    }
                    "destination_is_directory" => {
                        return Err(SshMcpError::invalid_params(
                            "remote_path is an existing directory",
                        ));
                    }
                    "hardlink_failed" => {
                        return Err(SshMcpError::invalid_params(
                            "overwrite=false requires hard-link support on the remote filesystem",
                        ));
                    }
                    _ => {}
                }
            }

            Err(SshMcpError::connection(format!(
                "{what} failed: exit_code={code}; stderr={}",
                out.stderr.trim()
            )))
        }
        None => {
            // Some SSH servers do not reliably emit an exit status for simple exec
            // channels. Treat missing status as success unless the tool emitted an
            // explicit error marker.
            if let Some(err) = parse_marker_value(&out.stderr, ERR_MARKER) {
                return Err(SshMcpError::connection(format!(
                    "{what} failed: {err}; stderr={}",
                    out.stderr.trim()
                )));
            }
            Ok(())
        }
    }
}

#[derive(Debug, Clone)]
struct RemoteStage {
    stage_path: String,
    stage_base: String,
}

async fn remote_prepare_put_file_stage(
    conn: &SshConnectionManager,
    remote_home: &str,
    remote_dst: &str,
    overwrite: bool,
    id: u64,
    timeout: Duration,
) -> Result<RemoteStage> {
    exec_raw::validate_remote_user_path(remote_home, "remote_home")?;
    exec_raw::validate_remote_user_file_path(remote_dst, "remote_path")?;

    let remote_tmp_sibling = exec_raw::remote_temp_sibling(remote_dst, id);
    let remote_dir = exec_raw::remote_parent_dir(remote_dst);
    let home_staging_base = remote_home_staging_base(remote_home);

    let dir_escaped = escape_for_shell(&remote_dir);
    let dst_escaped = escape_for_shell(remote_dst);
    let tmp_sib_escaped = escape_for_shell(&remote_tmp_sibling);
    let home_base_escaped = escape_for_shell(&home_staging_base);

    let cmd = if overwrite {
        format!(
            r#"sh -c 'set -eu; parent=$1; dst=$2; sib=$3; home_base=$4; id=$5; \
             home_ok=1; if ! mkdir -p -- "$home_base" 2>/dev/null; then home_ok=0; fi; \
             stage_dir="$home_base/$id"; if [ "$home_ok" -eq 1 ]; then if ! mkdir -p -- "$stage_dir" 2>/dev/null; then home_ok=0; fi; fi; \
             bn=${{dst##*/}}; stage="$sib"; stage_base="$parent"; \
             if ! (mkdir -p -- "$parent" 2>/dev/null && : > "$sib" 2>/dev/null); then \
               if [ "$home_ok" -eq 1 ]; then stage="$stage_dir/$bn.ssh-mcp-staging-$id"; stage_base="$home_base"; : > "$stage"; \
               else printf "%s\\n" "{ERR_MARKER}staging_unwritable" >&2; exit 1; fi; \
             fi; \
             printf "%s\\n" "{STAGE_MARKER}$stage" >&2; \
             printf "%s\\n" "{STAGE_BASE_MARKER}$stage_base" >&2' sh '{dir_escaped}' '{dst_escaped}' '{tmp_sib_escaped}' '{home_base_escaped}' '{id}'"#
        )
    } else {
        format!(
            r#"sh -c 'set -eu; parent=$1; dst=$2; sib=$3; \
             if ! (mkdir -p -- "$parent" 2>/dev/null && : > "$sib" 2>/dev/null); then \
               printf "%s\\n" "{ERR_MARKER}staging_unwritable" >&2; exit 1; fi; \
             printf "%s\\n" "{STAGE_MARKER}$sib" >&2; \
             printf "%s\\n" "{STAGE_BASE_MARKER}$parent" >&2' sh '{dir_escaped}' '{dst_escaped}' '{tmp_sib_escaped}'"#
        )
    };

    let out = conn.exec_command(&cmd, timeout).await?;
    ensure_remote_exec_success("prepare_put_file_stage", &out)?;

    let stage_path =
        parse_marker_value(&out.stderr, STAGE_MARKER).unwrap_or_else(|| remote_tmp_sibling.clone());
    let stage_base =
        parse_marker_value(&out.stderr, STAGE_BASE_MARKER).unwrap_or_else(|| remote_dir.clone());

    Ok(RemoteStage {
        stage_path,
        stage_base,
    })
}

async fn remote_finalize_put_file(
    conn: &SshConnectionManager,
    remote_dst: &str,
    stage_path: &str,
    overwrite: bool,
    timeout: Duration,
) -> Result<()> {
    exec_raw::validate_remote_user_file_path(remote_dst, "remote_path")?;
    exec_raw::validate_remote_user_file_path(stage_path, "remote_stage")?;

    let dst_escaped = escape_for_shell(remote_dst);
    let stage_escaped = escape_for_shell(stage_path);

    let cmd = if overwrite {
        format!(
            r#"sh -c 'set -eu; dst=$1; stage=$2; if [ -d "$dst" ]; then printf "%s\\n" "{ERR_MARKER}destination_is_directory" >&2; exit 1; fi; mv -- "$stage" "$dst"' sh '{dst_escaped}' '{stage_escaped}'"#
        )
    } else {
        format!(
            r#"sh -c 'set -eu; dst=$1; stage=$2; if [ -d "$dst" ]; then printf "%s\\n" "{ERR_MARKER}destination_is_directory" >&2; exit 1; fi; if ln -- "$stage" "$dst" 2>/dev/null; then rm -f -- "$stage" 2>/dev/null || true; exit 0; fi; if [ -e "$dst" ]; then printf "%s\\n" "{ERR_MARKER}destination_exists" >&2; else printf "%s\\n" "{ERR_MARKER}hardlink_failed" >&2; fi; exit 1' sh '{dst_escaped}' '{stage_escaped}'"#
        )
    };

    let out = conn.exec_command(&cmd, timeout).await?;
    ensure_remote_exec_success("finalize_put_file", &out)
}

async fn remote_prepare_put_dir_stage(
    conn: &SshConnectionManager,
    remote_home: &str,
    remote_dst_dir: &str,
    overwrite: bool,
    id: u64,
    timeout: Duration,
) -> Result<RemoteStage> {
    exec_raw::validate_remote_user_path(remote_home, "remote_home")?;
    exec_raw::validate_remote_user_path(remote_dst_dir, "remote_path")?;

    let remote_parent = exec_raw::remote_parent_dir(remote_dst_dir);
    let remote_stage_sibling = exec_raw::remote_temp_dir_sibling(remote_dst_dir, id);
    let home_staging_base = remote_home_staging_base(remote_home);

    let parent_escaped = escape_for_shell(&remote_parent);
    let dst_escaped = escape_for_shell(remote_dst_dir);
    let stage_sib_escaped = escape_for_shell(&remote_stage_sibling);
    let home_base_escaped = escape_for_shell(&home_staging_base);

    let cmd = if overwrite {
        format!(
            r#"sh -c 'set -eu; parent=$1; dst=$2; stage_sib=$3; home_base=$4; id=$5; \
             home_ok=1; if ! mkdir -p -- "$home_base" 2>/dev/null; then home_ok=0; fi; \
             stage_dir="$home_base/$id"; if [ "$home_ok" -eq 1 ]; then if ! mkdir -p -- "$stage_dir" 2>/dev/null; then home_ok=0; fi; fi; \
              stage="$stage_sib"; stage_base="$parent"; \
             if ! (mkdir -p -- "$parent" 2>/dev/null && rm -rf -- "$stage_sib" 2>/dev/null && mkdir -p -- "$stage_sib" 2>/dev/null); then \
               if [ "$home_ok" -eq 1 ]; then stage="$stage_dir/stage-dir-$id"; stage_base="$home_base"; rm -rf -- "$stage" 2>/dev/null || true; mkdir -p -- "$stage"; \
               else printf "%s\\n" "{ERR_MARKER}staging_unwritable" >&2; exit 1; fi; \
               fi; \
             printf "%s\\n" "{STAGE_MARKER}$stage" >&2; \
             printf "%s\\n" "{STAGE_BASE_MARKER}$stage_base" >&2' sh '{parent_escaped}' '{dst_escaped}' '{stage_sib_escaped}' '{home_base_escaped}' '{id}'"#
        )
    } else {
        format!(
            r#"sh -c 'set -eu; parent=$1; dst=$2; \
             mkdir -p -- "$parent" 2>/dev/null || true; \
              if ! mkdir -- "$dst" 2>/dev/null; then \
               if [ -e "$dst" ]; then printf "%s\\n" "{ERR_MARKER}destination_exists" >&2; else printf "%s\\n" "{ERR_MARKER}mkdir_failed" >&2; fi; \
               exit 1; fi; \
             printf "%s\\n" "{STAGE_MARKER}$dst" >&2; \
             printf "%s\\n" "{STAGE_BASE_MARKER}$parent" >&2' sh '{parent_escaped}' '{dst_escaped}'"#
        )
    };

    let out = conn.exec_command(&cmd, timeout).await?;
    ensure_remote_exec_success("prepare_put_dir_stage", &out)?;

    let stage_path = parse_marker_value(&out.stderr, STAGE_MARKER)
        .unwrap_or_else(|| remote_stage_sibling.clone());
    let stage_base =
        parse_marker_value(&out.stderr, STAGE_BASE_MARKER).unwrap_or_else(|| remote_parent.clone());

    Ok(RemoteStage {
        stage_path,
        stage_base,
    })
}

async fn remote_finalize_put_dir_overwrite_true(
    conn: &SshConnectionManager,
    remote_home: &str,
    remote_dst_dir: &str,
    stage_dir: &str,
    id: u64,
    timeout: Duration,
) -> Result<Option<String>> {
    exec_raw::validate_remote_user_path(remote_home, "remote_home")?;
    exec_raw::validate_remote_user_path(remote_dst_dir, "remote_path")?;
    exec_raw::validate_remote_user_path(stage_dir, "remote_stage")?;

    let remote_backup_sibling = exec_raw::remote_backup_dir_sibling(remote_dst_dir, id);
    let home_staging_base = remote_home_staging_base(remote_home);

    let dst_escaped = escape_for_shell(remote_dst_dir);
    let stage_escaped = escape_for_shell(stage_dir);
    let backup_sib_escaped = escape_for_shell(&remote_backup_sibling);
    let home_base_escaped = escape_for_shell(&home_staging_base);

    let cmd = format!(
        r#"sh -c 'set -eu; dst=$1; stage=$2; backup_sib=$3; home_base=$4; id=$5; \
          home_ok=1; if ! mkdir -p -- "$home_base" 2>/dev/null; then home_ok=0; fi; \
          stage_dir="$home_base/$id"; if [ "$home_ok" -eq 1 ]; then if ! mkdir -p -- "$stage_dir" 2>/dev/null; then home_ok=0; fi; fi; \
          backup=""; \
          if [ -e "$dst" ]; then \
            if rm -rf -- "$backup_sib" 2>/dev/null && mv -- "$dst" "$backup_sib" 2>/dev/null; then backup="$backup_sib"; \
            else \
              if [ "$home_ok" -eq 1 ]; then backup_home="$stage_dir/backup-dir-$id"; rm -rf -- "$backup_home" 2>/dev/null || true; \
                if mv -- "$dst" "$backup_home" 2>/dev/null; then backup="$backup_home"; else rm -rf -- "$dst" 2>/dev/null || true; backup=""; fi; \
              else rm -rf -- "$dst" 2>/dev/null || true; backup=""; fi; \
            fi; \
          fi; \
          printf "%s\\n" "{BACKUP_MARKER}$backup" >&2; \
          mv -- "$stage" "$dst"; \
          if [ -n "$backup" ]; then rm -rf -- "$backup" 2>/dev/null || true; fi' sh '{dst_escaped}' '{stage_escaped}' '{backup_sib_escaped}' '{home_base_escaped}' '{id}'"#
    );

    let out = conn.exec_command(&cmd, timeout).await?;
    ensure_remote_exec_success("finalize_put_dir", &out)?;
    Ok(parse_marker_value(&out.stderr, BACKUP_MARKER).filter(|s| !s.is_empty()))
}

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

    let stage = remote_prepare_put_file_stage(
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

    remote_finalize_put_file(
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
    let meta = tokio::fs::symlink_metadata(root).await?;
    if !meta.is_dir() {
        return Err(SshMcpError::invalid_params("local_path is not a directory"));
    }

    let mut counts = TransferCounts::zero();

    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut rd = tokio::fs::read_dir(&dir).await?;
        while let Some(ent) = rd.next_entry().await? {
            let p = ent.path();
            let m = tokio::fs::symlink_metadata(&p).await?;
            if m.file_type().is_symlink() {
                return Err(SshMcpError::invalid_params(
                    "symlinks are not supported by directory transfer",
                ));
            }
            if m.is_dir() {
                counts.directories += 1;
                stack.push(p);
            } else if m.is_file() {
                counts.files += 1;
                counts.bytes += m.len();
            } else {
                return Err(SshMcpError::invalid_params(
                    "unsupported file type in directory transfer",
                ));
            }
        }
    }

    Ok(counts)
}

async fn put_dir(
    endpoint: OpenSshEndpoint,
    args: OpenSshTransferArgs<'_>,
) -> std::result::Result<(TransferStaging, TransferCounts), super::TransportAttemptError> {
    let counts = count_dir_no_symlinks(&args.local_path)
        .await
        .map_err(super::TransportAttemptError::Other)?;

    let stage = remote_prepare_put_dir_stage(
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
        remote_finalize_put_dir_overwrite_true(
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

    remote_validate_dir_contents(args.conn, &args.remote_path, args.timeout)
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
                    SshMcpError::invalid_params("local destination exists and overwrite=false"),
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

async fn remote_validate_dir_contents(
    conn: &SshConnectionManager,
    remote_src_dir: &str,
    timeout: Duration,
) -> Result<()> {
    exec_raw::validate_remote_user_path(remote_src_dir, "remote_path")?;
    let escaped = escape_for_shell(remote_src_dir);
    // Reject symlinks and special files for parity with the tar-based transport.
    let cmd = format!(
        r#"sh -c 'set -eu; src=$1; if [ ! -d "$src" ]; then printf "%s\\n" "{ERR_MARKER}not_a_directory" >&2; exit 1; fi; \
           bad=$(find "$src" \( -type l -o -type b -o -type c -o -type p -o -type s \) 2>/dev/null | head -n 1 || true); \
           if [ -n "$bad" ]; then printf "%s\\n" "{ERR_MARKER}unsupported_entry" >&2; exit 1; fi' sh '{escaped}'"#
    );
    let out = conn.exec_command(&cmd, timeout).await?;
    match out.exit_code {
        Some(0) => Ok(()),
        Some(_) => {
            if let Some(err) = parse_marker_value(&out.stderr, ERR_MARKER) {
                match err.trim() {
                    "not_a_directory" => Err(SshMcpError::invalid_params(
                        "remote_path is not a directory",
                    )),
                    "unsupported_entry" => Err(SshMcpError::invalid_params(
                        "unsupported file type in directory transfer",
                    )),
                    _ => Err(SshMcpError::connection(format!(
                        "remote dir validation failed: stderr={}",
                        out.stderr.trim()
                    ))),
                }
            } else {
                Err(SshMcpError::connection(format!(
                    "remote dir validation failed: stderr={}",
                    out.stderr.trim()
                )))
            }
        }
        None => Err(SshMcpError::connection(
            "remote dir validation failed: missing exit status",
        )),
    }
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
