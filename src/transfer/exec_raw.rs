use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::fs;
use tokio::fs::OpenOptions;
use tokio::io;
use tokio::io::AsyncWriteExt;

use crate::error::{Result, SshMcpError};
use crate::ssh::{CommandOutput, SshConnectionManager, TransferRawOutput, escape_for_shell};

use super::local_root;
use super::tar;
use super::types::{StagingLocal, StagingRemote, TransferCounts, TransferKind, TransferStaging};

const REMOTE_STAGING_BASE_SUFFIX: &str = "/.ssh-mcp/staging";
const STAGE_MARKER: &str = "__SSH_MCP_STAGE=";
const STAGE_BASE_MARKER: &str = "__SSH_MCP_STAGE_BASE=";
const BACKUP_MARKER: &str = "__SSH_MCP_BACKUP=";
const ERR_MARKER: &str = "__SSH_MCP_ERR=";

#[derive(Debug, Clone, Copy)]
pub struct ExecRawCtx<'a> {
    pub conn: &'a SshConnectionManager,
    pub id: u64,
    pub timeout: Duration,
}

#[derive(Debug, Clone, Copy)]
pub struct ProbeRemoteKindArgs<'a> {
    pub ctx: ExecRawCtx<'a>,
    pub remote_path: &'a str,
}

#[derive(Debug, Clone, Copy)]
pub struct PutFileExecRawArgs<'a> {
    pub ctx: ExecRawCtx<'a>,
    pub remote_home: &'a str,
    pub local_src: &'a Path,
    pub remote_dst: &'a str,
    pub overwrite: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct GetFileExecRawArgs<'a> {
    pub ctx: ExecRawCtx<'a>,
    pub remote_src: &'a str,
    pub local_dst: &'a Path,
    pub local_root: &'a Path,
    pub overwrite: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct PutDirExecRawArgs<'a> {
    pub ctx: ExecRawCtx<'a>,
    pub remote_home: &'a str,
    pub local_src_dir: &'a Path,
    pub remote_dst_dir: &'a str,
    pub overwrite: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct GetDirExecRawArgs<'a> {
    pub ctx: ExecRawCtx<'a>,
    pub remote_src_dir: &'a str,
    pub local_dst_dir: &'a Path,
    pub local_root: &'a Path,
    pub overwrite: bool,
}

pub(crate) fn validate_remote_user_path(path: &str, field: &'static str) -> Result<()> {
    if path.is_empty() {
        return Err(SshMcpError::invalid_params(format!(
            "{field} cannot be empty",
        )));
    }

    if path.trim().is_empty() {
        return Err(SshMcpError::invalid_params(format!(
            "{field} must not be whitespace-only",
        )));
    }

    if path != path.trim() {
        return Err(SshMcpError::invalid_params(format!(
            "{field} must not have leading or trailing whitespace",
        )));
    }

    if path.contains('\0') {
        return Err(SshMcpError::invalid_params(format!(
            "{field} must not contain NUL",
        )));
    }

    if path.chars().any(|c| c.is_control()) {
        return Err(SshMcpError::invalid_params(format!(
            "{field} must not contain control characters",
        )));
    }

    // Without consistent `--` support across all remote utilities, a leading '-' may be
    // interpreted as an option. Reject early for user-supplied remote paths.
    if path.starts_with('-') {
        return Err(SshMcpError::invalid_params(format!(
            "{field} must not start with '-'",
        )));
    }

    Ok(())
}

pub(crate) fn validate_remote_user_file_path(path: &str, field: &'static str) -> Result<()> {
    validate_remote_user_path(path, field)?;
    if path.ends_with('/') {
        return Err(SshMcpError::invalid_params(format!(
            "{field} must not end with '/' for file transfers",
        )));
    }
    Ok(())
}

fn parse_marker_value(stderr: &str, prefix: &str) -> Option<String> {
    stderr
        .lines()
        .find_map(|line| line.strip_prefix(prefix).map(|v| v.to_string()))
}

pub(crate) fn remote_home_staging_base(remote_home: &str) -> String {
    format!("{remote_home}{REMOTE_STAGING_BASE_SUFFIX}")
}

pub async fn resolve_remote_home(conn: &SshConnectionManager, timeout: Duration) -> Result<String> {
    let cmd = r#"sh -c 'printf %s "$HOME"'"#;
    let out: CommandOutput = conn.exec_command(cmd, timeout).await?;
    if out.exit_code.is_some_and(|c| c != 0) {
        return Err(SshMcpError::connection(format!(
            "failed to resolve HOME: exit_code={:?}; stderr={}",
            out.exit_code, out.stderr
        )));
    }

    let home = out.stdout;
    if home.trim().is_empty() {
        return Err(SshMcpError::connection("remote HOME is empty"));
    }

    if let Err(e) = validate_remote_user_path(&home, "remote_home") {
        return Err(SshMcpError::connection(format!("invalid remote HOME: {e}")));
    }
    Ok(home)
}

pub async fn probe_remote_kind(args: ProbeRemoteKindArgs<'_>) -> Result<TransferKind> {
    validate_remote_user_path(args.remote_path, "remote_path")?;

    let escaped = escape_for_shell(args.remote_path);
    let cmd = format!(
        r#"sh -c 'p=$1; if [ -d "$p" ]; then printf dir; elif [ -f "$p" ]; then printf file; else printf missing; fi' sh '{escaped}'"#
    );
    let out = args.ctx.conn.exec_command(&cmd, args.ctx.timeout).await?;
    if out.exit_code.is_some_and(|c| c != 0) {
        return Err(SshMcpError::connection(format!(
            "failed to probe remote path kind: exit_code={:?}; stderr={}",
            out.exit_code, out.stderr
        )));
    }

    match out.stdout.trim() {
        "dir" => Ok(TransferKind::Directory),
        "file" => Ok(TransferKind::File),
        "missing" => Err(SshMcpError::invalid_params("remote_path does not exist")),
        other => Err(SshMcpError::connection(format!(
            "unexpected probe output: {other}"
        ))),
    }
}

pub async fn put_file_exec_raw(
    args: PutFileExecRawArgs<'_>,
) -> Result<(TransferStaging, TransferCounts)> {
    let id = args.ctx.id;

    validate_remote_user_path(args.remote_home, "remote_home")?;
    validate_remote_user_file_path(args.remote_dst, "remote_path")?;

    let meta = fs::symlink_metadata(args.local_src).await?;
    if !meta.is_file() {
        return Err(SshMcpError::invalid_params("local_path is not a file"));
    }
    let size = meta.len();

    let remote_tmp_sibling = remote_temp_sibling(args.remote_dst, args.ctx.id);
    let remote_dir = remote_parent_dir(args.remote_dst);
    let home_staging_base = remote_home_staging_base(args.remote_home);

    let dir_escaped = escape_for_shell(&remote_dir);
    let dst_escaped = escape_for_shell(args.remote_dst);
    let tmp_sib_escaped = escape_for_shell(&remote_tmp_sibling);
    let home_base_escaped = escape_for_shell(&home_staging_base);

    // Decide the staging path before consuming stdin (single pass).
    // overwrite=true: prefer a sibling stage for atomic-ish rename; fall back to $HOME staging.
    // overwrite=false: require sibling staging so finalize can use hard-link without replacement.
    let cmd = if args.overwrite {
        format!(
            r#"sh -c 'set -eu; parent=$1; dst=$2; sib=$3; home_base=$4; id=$5; \
             home_ok=1; if ! mkdir -p -- "$home_base" 2>/dev/null; then home_ok=0; fi; \
             stage_dir="$home_base/$id"; if [ "$home_ok" -eq 1 ]; then if ! mkdir -p -- "$stage_dir" 2>/dev/null; then home_ok=0; fi; fi; \
             bn=${{dst##*/}}; stage="$sib"; stage_base="$parent"; \
             if ! (mkdir -p -- "$parent" 2>/dev/null && : > "$sib" 2>/dev/null); then \
                if [ "$home_ok" -eq 1 ]; then stage="$stage_dir/$bn.ssh-mcp-staging-$id"; stage_base="$home_base"; \
                 else printf "%s\\n" "{ERR_MARKER}staging_unwritable" >&2; exit 1; fi; \
               fi; \
             trap "rm -f -- \"$stage\" 2>/dev/null || true" EXIT; \
               printf "%s\\n" "{STAGE_MARKER}$stage" >&2; \
               printf "%s\\n" "{STAGE_BASE_MARKER}$stage_base" >&2; \
                cat > "$stage"; \
                if [ -d "$dst" ]; then printf "%s\\n" "{ERR_MARKER}destination_is_directory" >&2; exit 1; fi; \
                mv -- "$stage" "$dst"; \
                trap - EXIT' sh '{dir_escaped}' '{dst_escaped}' '{tmp_sib_escaped}' '{home_base_escaped}' '{id}'"#
        )
    } else {
        format!(
            r#"sh -c 'set -eu; parent=$1; dst=$2; sib=$3; \
             if ! (mkdir -p -- "$parent" 2>/dev/null && : > "$sib" 2>/dev/null); then \
                  printf "%s\\n" "{ERR_MARKER}staging_unwritable" >&2; exit 1; fi; \
                trap "rm -f -- \"$sib\" 2>/dev/null || true" EXIT; \
                printf "%s\\n" "{STAGE_MARKER}$sib" >&2; \
                printf "%s\\n" "{STAGE_BASE_MARKER}$parent" >&2; \
                cat > "$sib"; \
                if [ -d "$dst" ]; then printf "%s\\n" "{ERR_MARKER}destination_is_directory" >&2; exit 1; fi; \
                if ln -- "$sib" "$dst" 2>/dev/null; then rm -f -- "$sib" 2>/dev/null || true; trap - EXIT; exit 0; fi; \
                if [ -e "$dst" ]; then printf "%s\\n" "{ERR_MARKER}destination_exists" >&2; else printf "%s\\n" "{ERR_MARKER}hardlink_failed" >&2; fi; \
                exit 1' sh '{dir_escaped}' '{dst_escaped}' '{tmp_sib_escaped}'"#
        )
    };

    let mut input = fs::File::open(args.local_src).await?;
    let mut sink = io::sink();
    let out = args
        .ctx
        .conn
        .exec_raw_streaming(&cmd, Some(&mut input), Some(&mut sink), args.ctx.timeout)
        .await?;

    ensure_remote_success("put_file", &out)?;

    let staging_path =
        parse_marker_value(&out.stderr, STAGE_MARKER).unwrap_or_else(|| remote_tmp_sibling.clone());
    let staging_base_used =
        parse_marker_value(&out.stderr, STAGE_BASE_MARKER).unwrap_or_else(|| remote_dir.clone());

    let staging = TransferStaging {
        local: None,
        remote: Some(StagingRemote {
            staging_path,
            backup_path: None,
            final_path: args.remote_dst.to_string(),
            staging_base_home: staging_base_used,
        }),
    };

    Ok((
        staging,
        TransferCounts {
            bytes: size,
            files: 1,
            directories: 0,
        },
    ))
}

pub async fn get_file_exec_raw(
    args: GetFileExecRawArgs<'_>,
) -> Result<(TransferStaging, TransferCounts)> {
    validate_remote_user_file_path(args.remote_src, "remote_path")?;

    let (tmp, mut out_file) =
        create_unique_local_staging_file(args.local_root, args.local_dst, args.ctx.id).await?;

    let src_escaped = escape_for_shell(args.remote_src);
    let cmd = format!(r#"sh -c 'set -eu; src=$1; cat < "$src"' sh '{src_escaped}'"#);

    let mut empty = io::empty();
    let exec_out = match args
        .ctx
        .conn
        .exec_raw_streaming(
            &cmd,
            Some(&mut empty),
            Some(&mut out_file),
            args.ctx.timeout,
        )
        .await
    {
        Ok(v) => v,
        Err(e) => {
            let _ = fs::remove_file(&tmp).await;
            return Err(e);
        }
    };

    out_file.flush().await?;
    out_file.sync_all().await?;

    if let Err(e) = ensure_remote_success("get_file", &exec_out) {
        let _ = fs::remove_file(&tmp).await;
        return Err(e);
    }

    let bytes = exec_out.stdout_bytes;
    if args.overwrite {
        atomic_replace_file(&tmp, args.local_dst).await?;
    } else {
        atomic_install_file_overwrite_false(&tmp, args.local_dst).await?;
    }

    let staging = TransferStaging {
        local: Some(StagingLocal {
            staging_path: tmp.display().to_string(),
            backup_path: None,
            final_path: args.local_dst.display().to_string(),
        }),
        remote: None,
    };

    Ok((
        staging,
        TransferCounts {
            bytes,
            files: 1,
            directories: 0,
        },
    ))
}

pub async fn put_dir_exec_raw(
    args: PutDirExecRawArgs<'_>,
) -> Result<(TransferStaging, TransferCounts)> {
    let id = args.ctx.id;

    validate_remote_user_path(args.remote_home, "remote_home")?;
    validate_remote_user_path(args.remote_dst_dir, "remote_path")?;

    let meta = fs::symlink_metadata(args.local_src_dir).await?;
    if !meta.is_dir() {
        return Err(SshMcpError::invalid_params("local_path is not a directory"));
    }

    let remote_parent = remote_parent_dir(args.remote_dst_dir);
    let remote_stage_sibling = remote_temp_dir_sibling(args.remote_dst_dir, args.ctx.id);
    let remote_backup_sibling = remote_backup_dir_sibling(args.remote_dst_dir, args.ctx.id);
    let home_staging_base = remote_home_staging_base(args.remote_home);

    let parent_escaped = escape_for_shell(&remote_parent);
    let dst_escaped = escape_for_shell(args.remote_dst_dir);
    let stage_sib_escaped = escape_for_shell(&remote_stage_sibling);
    let backup_sib_escaped = escape_for_shell(&remote_backup_sibling);
    let home_base_escaped = escape_for_shell(&home_staging_base);

    let cmd = if args.overwrite {
        let tar_extract = portable_tar_extract_cmd("$stage");
        // Prefer staging as a sibling directory for atomic rename; fall back to $HOME-based staging.
        // Always emit markers to stderr so we can report actual staging paths.
        format!(
            r#"sh -c 'set -eu; parent=$1; dst=$2; stage_sib=$3; backup_sib=$4; home_base=$5; id=$6; \
             home_ok=1; if ! mkdir -p -- "$home_base" 2>/dev/null; then home_ok=0; fi; \
             stage_dir="$home_base/$id"; if [ "$home_ok" -eq 1 ]; then if ! mkdir -p -- "$stage_dir" 2>/dev/null; then home_ok=0; fi; fi; \
              stage="$stage_sib"; stage_base="$parent"; \
             if ! (mkdir -p -- "$parent" 2>/dev/null && rm -rf -- "$stage_sib" 2>/dev/null && mkdir -p -- "$stage_sib" 2>/dev/null); then \
               if [ "$home_ok" -eq 1 ]; then stage="$stage_dir/stage-dir-$id"; stage_base="$home_base"; rm -rf -- "$stage" 2>/dev/null || true; mkdir -p -- "$stage"; \
                 else printf "%s\\n" "{ERR_MARKER}staging_unwritable" >&2; exit 1; fi; \
               fi; \
             trap "rm -rf -- \"$stage\" 2>/dev/null || true" EXIT; \
               printf "%s\\n" "{STAGE_MARKER}$stage" >&2; \
               printf "%s\\n" "{STAGE_BASE_MARKER}$stage_base" >&2; \
               {tar_extract}; \
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
              mv -- "$stage" "$dst"; trap - EXIT; \
               if [ -n "$backup" ]; then rm -rf -- "$backup" 2>/dev/null || true; fi' sh '{parent_escaped}' '{dst_escaped}' '{stage_sib_escaped}' '{backup_sib_escaped}' '{home_base_escaped}' '{id}'"#
        )
    } else {
        let tar_extract = portable_tar_extract_cmd("$dst");
        // overwrite=false: fail if destination exists; extract directly into created dir.
        format!(
            r#"sh -c 'set -eu; parent=$1; dst=$2; \
             mkdir -p -- "$parent" 2>/dev/null || true; \
              if ! mkdir -- "$dst" 2>/dev/null; then \
                 if [ -e "$dst" ]; then printf "%s\\n" "{ERR_MARKER}destination_exists" >&2; else printf "%s\\n" "{ERR_MARKER}mkdir_failed" >&2; fi; \
                 exit 1; fi; \
              trap "rm -rf -- \"$dst\" 2>/dev/null || true" EXIT; \
               printf "%s\\n" "{STAGE_MARKER}$dst" >&2; \
               printf "%s\\n" "{STAGE_BASE_MARKER}$parent" >&2; \
               {tar_extract}; trap - EXIT' sh '{parent_escaped}' '{dst_escaped}'"#
        )
    };

    let (mut tx, mut rx) = io::duplex(64 * 1024);
    let local_src = args.local_src_dir.to_path_buf();
    let tar_task = tokio::spawn(async move { tar::write_dir_as_tar(&local_src, &mut tx).await });

    let mut sink = io::sink();
    let exec_res = args
        .ctx
        .conn
        .exec_raw_streaming(&cmd, Some(&mut rx), Some(&mut sink), args.ctx.timeout)
        .await;

    let tar_res: Result<tar::TarCounts> = match tar_task.await {
        Ok(res) => res,
        Err(e) => Err(SshMcpError::connection(format!(
            "tar writer task failed: {e}"
        ))),
    };

    let exec_out = match exec_res {
        Ok(out) => out,
        Err(exec_err) => {
            if let Err(tar_err) = tar_res {
                return Err(SshMcpError::connection(format!(
                    "put_dir failed: {exec_err}; additionally tar encoder failed: {tar_err}"
                )));
            }
            return Err(exec_err);
        }
    };

    if let Err(remote_err) = ensure_remote_success("put_dir", &exec_out) {
        if let Err(tar_err) = tar_res {
            return Err(SshMcpError::connection(format!(
                "put_dir failed: {remote_err}; additionally tar encoder failed: {tar_err}"
            )));
        }
        return Err(remote_err);
    }

    let tar_counts = tar_res?;

    let staging_path = parse_marker_value(&exec_out.stderr, STAGE_MARKER)
        .unwrap_or_else(|| remote_stage_sibling.clone());
    let staging_base_used = parse_marker_value(&exec_out.stderr, STAGE_BASE_MARKER)
        .unwrap_or_else(|| remote_parent.clone());
    let backup_path = parse_marker_value(&exec_out.stderr, BACKUP_MARKER).filter(|s| !s.is_empty());

    let staging = TransferStaging {
        local: None,
        remote: Some(StagingRemote {
            staging_path,
            backup_path,
            final_path: args.remote_dst_dir.to_string(),
            staging_base_home: staging_base_used,
        }),
    };

    Ok((
        staging,
        TransferCounts {
            bytes: tar_counts.bytes,
            files: tar_counts.files,
            directories: tar_counts.directories,
        },
    ))
}

pub async fn get_dir_exec_raw(
    args: GetDirExecRawArgs<'_>,
) -> Result<(TransferStaging, TransferCounts)> {
    validate_remote_user_path(args.remote_src_dir, "remote_path")?;

    let (extract_target, local_backup) = if args.overwrite {
        let stage =
            create_unique_local_staging_dir(args.local_root, args.local_dst_dir, args.ctx.id)
                .await?;
        let backup = local_backup_dir_sibling(args.local_dst_dir, args.ctx.id);
        (stage, Some(backup))
    } else {
        match fs::create_dir(args.local_dst_dir).await {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                return Err(SshMcpError::invalid_params(
                    "local destination exists and overwrite=false",
                ));
            }
            Err(e) => return Err(SshMcpError::Io(e)),
        }

        (args.local_dst_dir.to_path_buf(), None)
    };

    let src_escaped = escape_for_shell(args.remote_src_dir);
    let tar_create = portable_tar_create_cmd("$src");
    let cmd = format!(r#"sh -c 'set -eu; src=$1; {tar_create}' sh '{src_escaped}'"#);

    let (mut tx, rx) = io::duplex(64 * 1024);
    let stage_clone = extract_target.clone();
    let extract_task = tokio::spawn(async move { tar::extract_tar_to_dir(rx, &stage_clone).await });

    let mut empty = io::empty();
    let exec_res = args
        .ctx
        .conn
        .exec_raw_streaming(&cmd, Some(&mut empty), Some(&mut tx), args.ctx.timeout)
        .await;
    drop(tx);

    let extract_res: Result<tar::ExtractCounts> = match extract_task.await {
        Ok(res) => res,
        Err(e) => Err(SshMcpError::connection(format!(
            "tar extract task failed: {e}"
        ))),
    };

    let exec_out = match exec_res {
        Ok(out) => out,
        Err(exec_err) => {
            if let Err(extract_err) = extract_res {
                let _ = fs::remove_dir_all(&extract_target).await;
                return Err(SshMcpError::connection(format!(
                    "get_dir failed: {exec_err}; additionally tar decoder failed: {extract_err}"
                )));
            }
            let _ = fs::remove_dir_all(&extract_target).await;
            return Err(exec_err);
        }
    };

    if let Err(remote_err) = ensure_remote_success("get_dir", &exec_out) {
        if let Err(extract_err) = extract_res {
            let _ = fs::remove_dir_all(&extract_target).await;
            return Err(SshMcpError::connection(format!(
                "get_dir failed: {remote_err}; additionally tar decoder failed: {extract_err}"
            )));
        }
        let _ = fs::remove_dir_all(&extract_target).await;
        return Err(remote_err);
    }

    let extract_counts = match extract_res {
        Ok(v) => v,
        Err(e) => {
            let _ = fs::remove_dir_all(&extract_target).await;
            return Err(e);
        }
    };

    let (staging_path, backup_path) = if args.overwrite {
        let backup = local_backup
            .as_ref()
            .ok_or_else(|| SshMcpError::connection("missing local backup path"))?;

        if let Err(e) = atomic_replace_dir(&extract_target, args.local_dst_dir, backup).await {
            let _ = fs::remove_dir_all(&extract_target).await;
            return Err(e);
        }

        (
            extract_target.display().to_string(),
            Some(backup.display().to_string()),
        )
    } else {
        (args.local_dst_dir.display().to_string(), None)
    };

    let staging = TransferStaging {
        local: Some(StagingLocal {
            staging_path,
            backup_path,
            final_path: args.local_dst_dir.display().to_string(),
        }),
        remote: None,
    };

    Ok((
        staging,
        TransferCounts {
            bytes: extract_counts.bytes,
            files: extract_counts.files,
            directories: extract_counts.directories,
        },
    ))
}

fn ensure_remote_success(what: &str, out: &TransferRawOutput) -> Result<()> {
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
                out.stderr
            )))
        }
        None => Err(SshMcpError::connection(format!(
            "{what} failed: missing exit status; stderr={}",
            out.stderr
        ))),
    }
}

pub(crate) fn remote_parent_dir(path: &str) -> String {
    match path.rsplit_once('/') {
        Some(("", _)) => "/".to_string(),
        Some((parent, _)) => parent.to_string(),
        None => ".".to_string(),
    }
}

pub(crate) fn remote_temp_sibling(final_path: &str, id: u64) -> String {
    format!("{final_path}.ssh-mcp-staging-{id}")
}

pub(crate) fn remote_temp_dir_sibling(final_dir: &str, id: u64) -> String {
    format!("{final_dir}.ssh-mcp-staging-dir-{id}")
}

pub(crate) fn remote_backup_dir_sibling(final_dir: &str, id: u64) -> String {
    format!("{final_dir}.ssh-mcp-backup-dir-{id}")
}

fn local_temp_sibling_with_attempt(final_path: &Path, id: u64, attempt: u32) -> PathBuf {
    let file_name = final_path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());

    if attempt == 0 {
        final_path.with_file_name(format!("{file_name}.ssh-mcp-staging-{id}"))
    } else {
        final_path.with_file_name(format!("{file_name}.ssh-mcp-staging-{id}-{attempt}"))
    }
}

fn local_temp_dir_sibling_with_attempt(final_dir: &Path, id: u64, attempt: u32) -> PathBuf {
    let name = final_dir
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "dir".to_string());

    if attempt == 0 {
        final_dir.with_file_name(format!("{name}.ssh-mcp-staging-dir-{id}"))
    } else {
        final_dir.with_file_name(format!("{name}.ssh-mcp-staging-dir-{id}-{attempt}"))
    }
}

pub(crate) fn local_backup_dir_sibling(final_dir: &Path, id: u64) -> PathBuf {
    let name = final_dir
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "dir".to_string());
    final_dir.with_file_name(format!("{name}.ssh-mcp-backup-dir-{id}"))
}

pub(crate) async fn atomic_replace_file(staging: &Path, final_path: &Path) -> Result<()> {
    match fs::rename(staging, final_path).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            // Windows rename does not replace an existing destination.
            let _ = fs::remove_file(final_path).await;
            match fs::rename(staging, final_path).await {
                Ok(()) => Ok(()),
                Err(e) => {
                    let _ = fs::remove_file(staging).await;
                    Err(SshMcpError::Io(e))
                }
            }
        }
        Err(e) => {
            let _ = fs::remove_file(staging).await;
            Err(SshMcpError::Io(e))
        }
    }
}

pub(crate) async fn atomic_install_file_overwrite_false(
    staging: &Path,
    final_path: &Path,
) -> Result<()> {
    match fs::hard_link(staging, final_path).await {
        Ok(()) => {
            let _ = fs::remove_file(staging).await;
            Ok(())
        }
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            let _ = fs::remove_file(staging).await;
            Err(SshMcpError::invalid_params(
                "local destination exists and overwrite=false",
            ))
        }
        Err(e) if e.kind() == ErrorKind::Unsupported => {
            let _ = fs::remove_file(staging).await;
            Err(SshMcpError::invalid_params(
                "overwrite=false requires hard-link support on the local filesystem",
            ))
        }
        Err(e) => {
            let _ = fs::remove_file(staging).await;
            Err(SshMcpError::Io(e))
        }
    }
}

#[cfg(all(unix, any(target_os = "linux", target_os = "android")))]
const O_NOFOLLOW_FLAG: i32 = 0o400000;

#[cfg(all(
    unix,
    any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    )
))]
const O_NOFOLLOW_FLAG: i32 = 0x0100;

#[cfg(all(
    unix,
    not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    ))
))]
const O_NOFOLLOW_FLAG: i32 = 0;

pub(crate) async fn create_unique_local_staging_file(
    local_root_path: &Path,
    final_path: &Path,
    id: u64,
) -> Result<(PathBuf, fs::File)> {
    // Limit retries to avoid an infinite loop in pathological cases.
    for attempt in 0u32..128u32 {
        let candidate = local_temp_sibling_with_attempt(final_path, id, attempt);

        local_root::validate_get_target_no_symlinks(local_root_path, &candidate)
            .await
            .map_err(SshMcpError::invalid_params)?;

        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);

        #[cfg(unix)]
        {
            opts.custom_flags(O_NOFOLLOW_FLAG);
        }

        match opts.open(&candidate).await {
            Ok(file) => return Ok((candidate, file)),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(SshMcpError::Io(e)),
        }
    }

    Err(SshMcpError::Io(std::io::Error::new(
        ErrorKind::AlreadyExists,
        "failed to allocate unique staging file name",
    )))
}

pub(crate) async fn create_unique_local_staging_dir(
    local_root_path: &Path,
    final_dir: &Path,
    id: u64,
) -> Result<PathBuf> {
    for attempt in 0u32..128u32 {
        let candidate = local_temp_dir_sibling_with_attempt(final_dir, id, attempt);

        local_root::validate_get_target_no_symlinks(local_root_path, &candidate)
            .await
            .map_err(SshMcpError::invalid_params)?;

        match fs::create_dir(&candidate).await {
            Ok(()) => return Ok(candidate),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(SshMcpError::Io(e)),
        }
    }

    Err(SshMcpError::Io(std::io::Error::new(
        ErrorKind::AlreadyExists,
        "failed to allocate unique staging directory name",
    )))
}

pub(crate) async fn atomic_replace_dir(
    staging: &Path,
    final_dir: &Path,
    backup: &Path,
) -> Result<()> {
    if final_dir.exists() {
        let _ = fs::remove_dir_all(backup).await;
        fs::rename(final_dir, backup).await?;
    }
    if let Some(parent) = final_dir.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::rename(staging, final_dir).await?;
    if backup.exists() {
        let _ = fs::remove_dir_all(backup).await;
    }
    Ok(())
}

fn portable_tar_extract_cmd(stage_var: &str) -> String {
    // Prefer tar; fallback to busybox tar. Read from stdin.
    format!(
        "(command -v tar >/dev/null 2>&1 && tar -x -f - -C \"{stage_var}\") || (command -v busybox >/dev/null 2>&1 && busybox tar -x -f - -C \"{stage_var}\")"
    )
}

fn portable_tar_create_cmd(src_var: &str) -> String {
    // Stream directory contents to stdout.
    // Important: include contents of src, not an extra top-level folder.
    format!(
        "(command -v tar >/dev/null 2>&1 && tar -c -f - -C \"{src_var}\" .) || (command -v busybox >/dev/null 2>&1 && busybox tar -c -f - -C \"{src_var}\" .)"
    )
}
