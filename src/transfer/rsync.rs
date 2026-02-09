use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::error::{Result, SshMcpError};
use crate::ssh::{SshConnectionManager, escape_for_shell};

use super::exec_raw;
use super::types::{
    RsyncOptions, StagingLocal, StagingRemote, TransferCounts, TransferKind, TransferOperation,
    TransferStaging,
};

const REMOTE_STAGING_BASE_SUFFIX: &str = "/.ssh-mcp/staging";
const STAGE_MARKER: &str = "__SSH_MCP_STAGE=";
const STAGE_BASE_MARKER: &str = "__SSH_MCP_STAGE_BASE=";
const BACKUP_MARKER: &str = "__SSH_MCP_BACKUP=";
const ERR_MARKER: &str = "__SSH_MCP_ERR=";

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
) -> std::result::Result<RsyncOutput, super::TransportAttemptError> {
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

    // Spawn child separately to allow proper cleanup on timeout
    let mut child = cmd.spawn().map_err(classify_spawn_error)?;

    // Take stdout/stderr handles before select! to read them separately
    let mut stdout_pipe = child.stdout.take().ok_or_else(|| {
        super::TransportAttemptError::Other(SshMcpError::connection("missing stdout pipe"))
    })?;
    let mut stderr_pipe = child.stderr.take().ok_or_else(|| {
        super::TransportAttemptError::Other(SshMcpError::connection("missing stderr pipe"))
    })?;

    // Spawn tasks to read stdout and stderr
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

    // Use select! for timeout handling with proper cleanup
    let status = tokio::select! {
        res = child.wait() => {
            res.map_err(|e| {
                super::TransportAttemptError::Other(SshMcpError::Io(e))
            })?
        }
        _ = tokio::time::sleep(timeout_duration) => {
            // Kill the child process on timeout
            stdout_task.abort();
            stderr_task.abort();
            let _ = child.kill().await;
            let _ = child.wait().await; // Reap the process
            return Err(super::TransportAttemptError::Other(
                SshMcpError::Timeout(timeout_duration.as_millis() as u64)
            ));
        }
    };

    // Collect stdout/stderr after child completes
    let stdout_bytes = match stdout_task.await {
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

    let stdout = String::from_utf8_lossy(&stdout_bytes).to_string();
    let stderr = String::from_utf8_lossy(&stderr_bytes).to_string();

    if !status.success() {
        return Err(classify_rsync_failure(status.code(), &stderr));
    }

    let counts = parse_rsync_stats(&stdout);

    Ok(RsyncOutput {
        status,
        stdout,
        stderr,
        counts,
    })
}

#[derive(Debug)]
#[allow(dead_code)]
struct RsyncOutput {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
    counts: TransferCounts,
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
    if err.kind() == std::io::ErrorKind::NotFound {
        return super::TransportAttemptError::Unsupported {
            transport: super::TransferTransport::Rsync,
            reason: "missing local rsync binary".to_string(),
        };
    }
    super::TransportAttemptError::Other(SshMcpError::Io(err))
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

fn parse_marker_value(stderr: &str, prefix: &str) -> Option<String> {
    stderr
        .lines()
        .find_map(|line| line.strip_prefix(prefix).map(|v| v.to_string()))
}

fn remote_home_staging_base(remote_home: &str) -> String {
    format!("{remote_home}{REMOTE_STAGING_BASE_SUFFIX}")
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
               else printf "%s\n" "{ERR_MARKER}staging_unwritable" >&2; exit 1; fi; \
             fi; \
             printf "%s\n" "{STAGE_MARKER}$stage" >&2; \
             printf "%s\n" "{STAGE_BASE_MARKER}$stage_base" >&2' sh '{dir_escaped}' '{dst_escaped}' '{tmp_sib_escaped}' '{home_base_escaped}' '{id}'"#
        )
    } else {
        format!(
            r#"sh -c 'set -eu; parent=$1; dst=$2; sib=$3; \
             if ! (mkdir -p -- "$parent" 2>/dev/null && : > "$sib" 2>/dev/null); then \
               printf "%s\n" "{ERR_MARKER}staging_unwritable" >&2; exit 1; fi; \
             printf "%s\n" "{STAGE_MARKER}$sib" >&2; \
             printf "%s\n" "{STAGE_BASE_MARKER}$parent" >&2' sh '{dir_escaped}' '{dst_escaped}' '{tmp_sib_escaped}'"#
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
            r#"sh -c 'set -eu; dst=$1; stage=$2; if [ -d "$dst" ]; then printf "%s\n" "{ERR_MARKER}destination_is_directory" >&2; exit 1; fi; mv -- "$stage" "$dst"' sh '{dst_escaped}' '{stage_escaped}'"#
        )
    } else {
        format!(
            r#"sh -c 'set -eu; dst=$1; stage=$2; if [ -d "$dst" ]; then printf "%s\n" "{ERR_MARKER}destination_is_directory" >&2; exit 1; fi; if ln -- "$stage" "$dst" 2>/dev/null; then rm -f -- "$stage" 2>/dev/null || true; exit 0; fi; if [ -e "$dst" ]; then printf "%s\n" "{ERR_MARKER}destination_exists" >&2; else printf "%s\n" "{ERR_MARKER}hardlink_failed" >&2; fi; exit 1' sh '{dst_escaped}' '{stage_escaped}'"#
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
               else printf "%s\n" "{ERR_MARKER}staging_unwritable" >&2; exit 1; fi; \
               fi; \
             printf "%s\n" "{STAGE_MARKER}$stage" >&2; \
             printf "%s\n" "{STAGE_BASE_MARKER}$stage_base" >&2' sh '{parent_escaped}' '{dst_escaped}' '{stage_sib_escaped}' '{home_base_escaped}' '{id}'"#
        )
    } else {
        format!(
            r#"sh -c 'set -eu; parent=$1; dst=$2; \
             mkdir -p -- "$parent" 2>/dev/null || true; \
              if ! mkdir -- "$dst" 2>/dev/null; then \
               if [ -e "$dst" ]; then printf "%s\n" "{ERR_MARKER}destination_exists" >&2; else printf "%s\n" "{ERR_MARKER}mkdir_failed" >&2; fi; \
               exit 1; fi; \
             printf "%s\n" "{STAGE_MARKER}$dst" >&2; \
             printf "%s\n" "{STAGE_BASE_MARKER}$parent" >&2' sh '{parent_escaped}' '{dst_escaped}'"#
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
          printf "%s\n" "{BACKUP_MARKER}$backup" >&2; \
          mv -- "$stage" "$dst"; \
          if [ -n "$backup" ]; then rm -rf -- "$backup" 2>/dev/null || true; fi' sh '{dst_escaped}' '{stage_escaped}' '{backup_sib_escaped}' '{home_base_escaped}' '{id}'"#
    );

    let out = conn.exec_command(&cmd, timeout).await?;
    ensure_remote_exec_success("finalize_put_dir", &out)?;
    Ok(parse_marker_value(&out.stderr, BACKUP_MARKER).filter(|s| !s.is_empty()))
}

fn ensure_remote_exec_success(what: &str, out: &crate::ssh::CommandOutput) -> Result<()> {
    match out.exit_code {
        Some(0) => Ok(()),
        Some(code) => {
            if let Some(err) = parse_marker_value(&out.stderr, ERR_MARKER) {
                match err.trim() {
                    "destination_exists" => {
                        return Err(SshMcpError::invalid_params(
                            "destination exists and overwrite=false. Use overwrite=true to replace it.",
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

    let stage = remote_prepare_put_file_stage(
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

    remote_finalize_put_file(
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
    endpoint: RsyncEndpoint,
    args: RsyncTransferArgs<'_>,
) -> std::result::Result<(TransferStaging, TransferCounts), super::TransportAttemptError> {
    let counts = count_local_dir_no_symlinks(args.local_path)
        .await
        .map_err(super::TransportAttemptError::Other)?;

    let stage = remote_prepare_put_dir_stage(
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
        remote_finalize_put_dir_overwrite_true(
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

    remote_validate_dir_contents(args.conn, args.remote_path, args.timeout)
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

async fn remote_validate_dir_contents(
    conn: &SshConnectionManager,
    remote_src_dir: &str,
    timeout: Duration,
) -> Result<()> {
    exec_raw::validate_remote_user_path(remote_src_dir, "remote_path")?;
    let escaped = escape_for_shell(remote_src_dir);
    let cmd = format!(
        r#"sh -c 'set -eu; src=$1; if [ ! -d "$src" ]; then printf "%s\n" "{ERR_MARKER}not_a_directory" >&2; exit 1; fi; \
           bad=$(find "$src" \( -type l -o -type b -o -type c -o -type p -o -type s \) 2>/dev/null | head -n 1 || true); \
           if [ -n "$bad" ]; then printf "%s\n" "{ERR_MARKER}unsupported_entry" >&2; exit 1; fi' sh '{escaped}'"#
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
