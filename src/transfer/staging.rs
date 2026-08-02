use std::time::Duration;

use crate::error::{Result, SshMcpError};
use crate::ssh::{SshConnectionManager, escape_for_shell};

use super::exec_raw;

pub(crate) const STAGE_MARKER: &str = "__SSH_MCP_STAGE=";
pub(crate) const STAGE_BASE_MARKER: &str = "__SSH_MCP_STAGE_BASE=";
pub(crate) const BACKUP_MARKER: &str = "__SSH_MCP_BACKUP=";
pub(crate) const ERR_MARKER: &str = "__SSH_MCP_ERR=";

pub(crate) fn parse_marker_value(stderr: &str, prefix: &str) -> Option<String> {
    stderr
        .lines()
        .find_map(|line| line.strip_prefix(prefix).map(|v| v.to_string()))
}

pub(crate) fn ensure_remote_exec_success(
    what: &str,
    out: &crate::ssh::CommandOutput,
) -> Result<()> {
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
pub(crate) struct RemoteStage {
    pub(crate) stage_path: String,
    pub(crate) stage_base: String,
    pub(crate) stage_is_destination: bool,
}

pub(crate) async fn remote_prepare_put_file_stage(
    conn: &SshConnectionManager,
    remote_home: &str,
    remote_dst: &str,
    _overwrite: bool,
    id: &str,
    timeout: Duration,
) -> Result<RemoteStage> {
    exec_raw::validate_remote_user_path(remote_home, "remote_home")?;
    exec_raw::validate_remote_user_file_path(remote_dst, "remote_path")?;

    let remote_tmp_sibling = exec_raw::remote_temp_sibling(remote_dst, id);
    let remote_dir = exec_raw::remote_parent_dir(remote_dst);
    let dir_escaped = escape_for_shell(&remote_dir);
    let dst_escaped = escape_for_shell(remote_dst);
    let tmp_sib_escaped = escape_for_shell(&remote_tmp_sibling);

    let cmd = format!(
        r#"sh -c 'set -eu; parent=$1; dst=$2; sib=$3; \
         if ! (mkdir -p -- "$parent" 2>/dev/null && (set -C; : > "$sib") 2>/dev/null); then \
           if [ -e "$sib" ]; then printf "%s\\n" "{ERR_MARKER}staging_collision" >&2; else printf "%s\\n" "{ERR_MARKER}staging_unwritable" >&2; fi; exit 1; fi; \
         printf "%s\\n" "{STAGE_MARKER}$sib" >&2; \
         printf "%s\\n" "{STAGE_BASE_MARKER}$parent" >&2' sh '{dir_escaped}' '{dst_escaped}' '{tmp_sib_escaped}'"#
    );

    let out = conn.exec_command(&cmd, timeout).await?;
    ensure_remote_exec_success("prepare_put_file_stage", &out)?;

    let stage_path =
        parse_marker_value(&out.stderr, STAGE_MARKER).unwrap_or_else(|| remote_tmp_sibling.clone());
    let stage_base =
        parse_marker_value(&out.stderr, STAGE_BASE_MARKER).unwrap_or_else(|| remote_dir.clone());

    Ok(RemoteStage {
        stage_path,
        stage_base,
        stage_is_destination: false,
    })
}

pub(crate) async fn remote_finalize_put_file(
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

pub(crate) async fn remote_prepare_put_dir_stage(
    conn: &SshConnectionManager,
    remote_home: &str,
    remote_dst_dir: &str,
    overwrite: bool,
    id: &str,
    timeout: Duration,
) -> Result<RemoteStage> {
    exec_raw::validate_remote_user_path(remote_home, "remote_home")?;
    exec_raw::validate_remote_user_path(remote_dst_dir, "remote_path")?;

    let remote_parent = exec_raw::remote_parent_dir(remote_dst_dir);
    let remote_stage_sibling = exec_raw::remote_temp_dir_sibling(remote_dst_dir, id);
    let parent_escaped = escape_for_shell(&remote_parent);
    let dst_escaped = escape_for_shell(remote_dst_dir);
    let stage_sib_escaped = escape_for_shell(&remote_stage_sibling);

    let cmd = if overwrite {
        format!(
            r#"sh -c 'set -eu; parent=$1; dst=$2; stage_sib=$3; \
              stage="$stage_sib"; stage_base="$parent"; \
             if ! (mkdir -p -- "$parent" 2>/dev/null && mkdir -- "$stage_sib" 2>/dev/null); then \
               if [ -e "$stage_sib" ]; then printf "%s\\n" "{ERR_MARKER}staging_collision" >&2; else printf "%s\\n" "{ERR_MARKER}staging_unwritable" >&2; fi; exit 1; \
               fi; \
             printf "%s\\n" "{STAGE_MARKER}$stage" >&2; \
             printf "%s\\n" "{STAGE_BASE_MARKER}$stage_base" >&2' sh '{parent_escaped}' '{dst_escaped}' '{stage_sib_escaped}'"#
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
        stage_is_destination: !overwrite,
    })
}

pub(crate) async fn remote_finalize_put_dir_overwrite_true(
    conn: &SshConnectionManager,
    remote_home: &str,
    remote_dst_dir: &str,
    stage_dir: &str,
    id: &str,
    timeout: Duration,
) -> Result<Option<String>> {
    exec_raw::validate_remote_user_path(remote_home, "remote_home")?;
    exec_raw::validate_remote_user_path(remote_dst_dir, "remote_path")?;
    exec_raw::validate_remote_user_path(stage_dir, "remote_stage")?;

    let remote_backup_sibling = exec_raw::remote_backup_dir_sibling(remote_dst_dir, id);
    let dst_escaped = escape_for_shell(remote_dst_dir);
    let stage_escaped = escape_for_shell(stage_dir);
    let backup_sib_escaped = escape_for_shell(&remote_backup_sibling);

    let cmd = format!(
        r#"sh -c 'set -eu; dst=$1; stage=$2; backup=$3; had_dst=0; \
          if [ -e "$dst" ]; then \
            if [ -e "$backup" ]; then printf "%s\\n" "{ERR_MARKER}backup_collision" >&2; exit 1; fi; \
            if ! mv -- "$dst" "$backup"; then printf "%s\\n" "{ERR_MARKER}backup_failed" >&2; exit 1; fi; had_dst=1; \
          fi; \
          printf "%s\\n" "{BACKUP_MARKER}$backup" >&2; \
          if mv -- "$stage" "$dst"; then if [ "$had_dst" -eq 1 ]; then rm -rf -- "$backup" 2>/dev/null || true; fi; exit 0; fi; \
          if [ "$had_dst" -eq 1 ] && ! mv -- "$backup" "$dst"; then printf "%s\\n" "{ERR_MARKER}rollback_failed:$backup" >&2; else printf "%s\\n" "{ERR_MARKER}install_failed" >&2; fi; exit 1' sh '{dst_escaped}' '{stage_escaped}' '{backup_sib_escaped}'"#
    );

    let out = conn.exec_command(&cmd, timeout).await?;
    ensure_remote_exec_success("finalize_put_dir", &out)?;
    Ok(parse_marker_value(&out.stderr, BACKUP_MARKER).filter(|s| !s.is_empty()))
}

pub(crate) async fn remote_validate_dir_contents(
    conn: &SshConnectionManager,
    remote_src_dir: &str,
    timeout: Duration,
) -> Result<()> {
    exec_raw::validate_remote_user_path(remote_src_dir, "remote_path")?;
    let escaped = escape_for_shell(remote_src_dir);
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
