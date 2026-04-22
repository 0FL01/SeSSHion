use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use sha2::{Digest, Sha256};
use similar::TextDiff;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tracing::error;

#[cfg(unix)]
use crate::platform::O_NOFOLLOW_FLAG;
use crate::server::SshMcpServer;
use crate::server::make_job_id;
use crate::server::validation::common::extract_text_from_call_tool_result;
use crate::server::validation::file_edit::*;
use crate::server::validation::read_file::{
    SHA256_HEX_LEN, normalize_sha256_hex, sanitize_read_file_stderr_snippet,
};
use crate::shell_escape::escape_for_shell;
use crate::tools::{ReadFileMode, ReadFileParams};

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(in crate::server) enum FileEditFaultInjection {
    None,
    FailBeforeFinalize,
    Sha256Unavailable,
    PartialDeleteBeforeWrite,
    PartialMutateBeforeWrite,
}

pub(in crate::server) struct FileWriteTransactionRequest<'a> {
    pub remote_path: &'a str,
    pub new_content: &'a str,
    pub expected_sha256: Option<String>,
    pub timeout: Duration,
    pub fault_injection: FileEditFaultInjection,
    pub too_large_error: String,
    pub operation_name: &'a str,
}

pub(in crate::server) enum RemoteTextFileState {
    Missing,
    Existing { content: String, sha256: String },
}

enum RemotePathKind {
    Missing,
    RegularFile,
    NonRegularFile,
}

fn file_edit_lock_busy_result(remote_path: &str, operation_name: &str) -> CallToolResult {
    let result = serde_json::json!({
        "error": "lock_busy",
        "path": remote_path,
        "operation": operation_name,
        "retryable": true,
        "retry_after_ms": FILE_EDIT_LOCK_RETRY_AFTER_MS,
        "retry_same_tool": true,
        "message": format!(
            "Path is temporarily locked by another file-edit operation. Retry the same {operation_name} call after a short delay; do not fall back to exec/sudo-exec."
        ),
    });

    CallToolResult::error(vec![Content::text(result.to_string())])
}

pub(in crate::server) fn local_text_sha256_hex(content: &str) -> String {
    let hash = Sha256::digest(content.as_bytes());
    hash.iter()
        .fold(String::with_capacity(SHA256_HEX_LEN), |mut acc, b| {
            use std::fmt::Write as _;
            let _ = write!(acc, "{b:02x}");
            acc
        })
}

pub(in crate::server) fn build_unified_diff(
    remote_path: &str,
    original: &str,
    modified: &str,
) -> String {
    if original == modified {
        return String::new();
    }

    format!(
        "{}",
        TextDiff::from_lines(original, modified)
            .unified_diff()
            .context_radius(3)
            .header(remote_path, remote_path)
    )
}

pub(in crate::server) fn build_file_edit_conflict_result(
    remote_path: &str,
    expected_sha256: &str,
    actual_sha256: &str,
) -> CallToolResult {
    let conflict = serde_json::json!({
        "error": "conflict",
        "path": remote_path,
        "expected_sha256": expected_sha256,
        "actual_sha256": actual_sha256,
    });

    CallToolResult::error(vec![Content::text(conflict.to_string())])
}

impl SshMcpServer {
    async fn probe_remote_path_kind(
        &self,
        remote_path: &str,
        timeout: Duration,
    ) -> std::result::Result<RemotePathKind, CallToolResult> {
        let escaped = escape_for_shell(remote_path);
        let cmd = format!(
            r#"sh -c 'p=$1; if [ ! -e "$p" ]; then printf "missing"; elif [ -f "$p" ]; then printf "regular"; else printf "other"; fi' sh '{escaped}'"#
        );

        let out = match self.connection.exec_command(&cmd, timeout).await {
            Ok(output) => output,
            Err(e) => {
                return Err(CallToolResult::error(vec![Content::text(format!(
                    "Error: failed to inspect remote file while preparing edit: {e}"
                ))]));
            }
        };

        match out.exit_code {
            Some(0) => {}
            Some(code) => {
                let mut msg = format!(
                    "Error: failed to inspect remote file while preparing edit: remote command failed with exit_code={code}"
                );
                if let Some(snippet) = sanitize_read_file_stderr_snippet(&out.stderr) {
                    msg.push_str(&format!("; stderr={snippet}"));
                }
                return Err(CallToolResult::error(vec![Content::text(msg)]));
            }
            None => {
                if !out.stderr.trim().is_empty() {
                    let mut msg = "Error: failed to inspect remote file while preparing edit: remote command did not provide an exit status".to_string();
                    if let Some(snippet) = sanitize_read_file_stderr_snippet(&out.stderr) {
                        msg.push_str(&format!("; stderr={snippet}"));
                    }
                    return Err(CallToolResult::error(vec![Content::text(msg)]));
                }
            }
        }

        match out.stdout.trim() {
            "missing" => Ok(RemotePathKind::Missing),
            "regular" => Ok(RemotePathKind::RegularFile),
            "other" => Ok(RemotePathKind::NonRegularFile),
            other => Err(CallToolResult::error(vec![Content::text(format!(
                "Error: unexpected remote file probe result: {other}"
            ))])),
        }
    }

    pub(in crate::server) async fn load_remote_text_file_state(
        &self,
        remote_path: &str,
        timeout: Duration,
    ) -> std::result::Result<RemoteTextFileState, CallToolResult> {
        match self.probe_remote_path_kind(remote_path, timeout).await? {
            RemotePathKind::Missing => Ok(RemoteTextFileState::Missing),
            RemotePathKind::NonRegularFile => Err(CallToolResult::error(vec![Content::text(
                "Error: remote_path is not a regular file".to_string(),
            )])),
            RemotePathKind::RegularFile => {
                let timeout_ms = (timeout.as_millis().min(u128::from(u64::MAX))) as u64;
                let read_result = match self
                    .execute_read_file(ReadFileParams {
                        remote_path: remote_path.to_string(),
                        mode: ReadFileMode::Full,
                        lines: None,
                        timeout_ms: Some(timeout_ms),
                    })
                    .await
                {
                    Ok(result) => result,
                    Err(e) => {
                        return Err(CallToolResult::error(vec![Content::text(format!(
                            "Error: failed to read remote file while preparing edit: {e}"
                        ))]));
                    }
                };

                if read_result.is_error.unwrap_or(false) {
                    return Err(read_result);
                }

                let read_text = extract_text_from_call_tool_result(&read_result);
                let read_value: serde_json::Value = match serde_json::from_str(read_text.trim()) {
                    Ok(value) => value,
                    Err(e) => {
                        return Err(CallToolResult::error(vec![Content::text(format!(
                            "Error: failed to parse read-file response while preparing edit: {e}"
                        ))]));
                    }
                };

                let content = match read_value.get("content").and_then(|value| value.as_str()) {
                    Some(value) => value.to_string(),
                    None => {
                        return Err(CallToolResult::error(vec![Content::text(
                            "Error: read-file response missing content while preparing edit"
                                .to_string(),
                        )]));
                    }
                };
                let sha256 = match read_value.get("sha256").and_then(|value| value.as_str()) {
                    Some(value) => value.to_string(),
                    None => {
                        return Err(CallToolResult::error(vec![Content::text(
                            "Error: read-file response missing sha256 while preparing edit"
                                .to_string(),
                        )]));
                    }
                };

                Ok(RemoteTextFileState::Existing { content, sha256 })
            }
        }
    }

    pub(in crate::server) async fn compute_partial_baseline_sha256(
        &self,
        content: &str,
        timeout: Duration,
    ) -> std::result::Result<String, CallToolResult> {
        let hash_cmd = format!(
            r#"sh -c 'set -eu; sha256_stdin() {{ if command -v sha256sum >/dev/null 2>&1; then set -- $(sha256sum); printf "%s\n" "$1"; return 0; fi; if command -v shasum >/dev/null 2>&1; then set -- $(shasum -a 256); printf "%s\n" "$1"; return 0; fi; return 1; }}; if ! baseline_hash=$(sha256_stdin); then printf "%s\n" "{FILE_EDIT_ERROR_MARKER}sha256_unavailable" >&2; exit 1; fi; printf "%s%s\n" "{FILE_EDIT_BASELINE_SHA_MARKER}" "$baseline_hash" >&2'"#
        );

        let mut input = std::io::Cursor::new(content.as_bytes());
        let mut sink = tokio::io::sink();
        let out = match self
            .connection
            .exec_raw_streaming(&hash_cmd, Some(&mut input), Some(&mut sink), timeout)
            .await
        {
            Ok(output) => output,
            Err(e) => {
                return Err(CallToolResult::error(vec![Content::text(format!(
                    "Error: failed to hash replace-in-file baseline content on remote host: {e}"
                ))]));
            }
        };

        if let Some(marker) = parse_file_edit_error_marker(&out.stderr)
            && marker == "sha256_unavailable"
        {
            return Err(CallToolResult::error(vec![Content::text(
                "Error: remote host does not provide SHA-256 utilities".to_string(),
            )]));
        }

        match out.exit_code {
            Some(0) => {}
            Some(code) => {
                let mut msg = format!(
                    "Error: failed to hash replace-in-file baseline content: remote command failed with exit_code={code}"
                );
                if let Some(snippet) = sanitize_read_file_stderr_snippet(&out.stderr) {
                    msg.push_str(&format!("; stderr={snippet}"));
                }
                return Err(CallToolResult::error(vec![Content::text(msg)]));
            }
            None => {
                if !out.stderr.trim().is_empty() {
                    let mut msg = "Error: failed to hash replace-in-file baseline content: remote command did not provide an exit status".to_string();
                    if let Some(snippet) = sanitize_read_file_stderr_snippet(&out.stderr) {
                        msg.push_str(&format!("; stderr={snippet}"));
                    }
                    return Err(CallToolResult::error(vec![Content::text(msg)]));
                }
            }
        }

        let baseline_raw =
            match parse_file_edit_marker_value(&out.stderr, FILE_EDIT_BASELINE_SHA_MARKER) {
                Some(value) => value,
                None => {
                    return Err(CallToolResult::error(vec![Content::text(
                        "Error: missing replace-in-file baseline SHA-256 marker".to_string(),
                    )]));
                }
            };

        match normalize_sha256_hex(baseline_raw, "partial_baseline_sha256") {
            Ok(value) => Ok(value),
            Err(_) => Err(CallToolResult::error(vec![Content::text(
                "Error: failed to parse remote SHA-256 output".to_string(),
            )])),
        }
    }

    pub(in crate::server) async fn apply_partial_fault_injection(
        &self,
        remote_path: &str,
        timeout: Duration,
        fault_injection: FileEditFaultInjection,
    ) -> std::result::Result<(), CallToolResult> {
        let injected_cmd = match fault_injection {
            FileEditFaultInjection::PartialDeleteBeforeWrite => {
                let escaped = escape_for_shell(remote_path);
                Some(format!("sh -c 'set -eu; rm -f -- \"$1\"' sh '{escaped}'"))
            }
            FileEditFaultInjection::PartialMutateBeforeWrite => {
                let escaped = escape_for_shell(remote_path);
                Some(format!(
                    "sh -c 'set -eu; [ -f \"$1\" ]; printf \"__ssh_mcp_race_injected__\\n\" > \"$1\"' sh '{escaped}'"
                ))
            }
            _ => None,
        };

        let Some(injected_cmd) = injected_cmd else {
            return Ok(());
        };

        let mut empty_input = tokio::io::empty();
        let mut sink = tokio::io::sink();
        let out = match self
            .connection
            .exec_raw_streaming(
                &injected_cmd,
                Some(&mut empty_input),
                Some(&mut sink),
                timeout,
            )
            .await
        {
            Ok(output) => output,
            Err(e) => {
                return Err(CallToolResult::error(vec![Content::text(format!(
                    "Error: failed to run replace-in-file race fault injection: {e}"
                ))]));
            }
        };

        match out.exit_code {
            Some(0) => Ok(()),
            Some(code) => Err(CallToolResult::error(vec![Content::text(format!(
                "Error: replace-in-file race fault injection failed with exit_code={code}"
            ))])),
            None => Err(CallToolResult::error(vec![Content::text(
                "Error: replace-in-file race fault injection did not report exit status"
                    .to_string(),
            )])),
        }
    }

    /// Lightweight remote check: does the file exist and have size > 0?
    ///
    /// Returns `true` when the path is a regular file with at least one byte.
    /// Returns `false` for missing paths, non-files, and zero-byte files.
    pub(in crate::server) async fn check_remote_file_nonempty(
        &self,
        remote_path: &str,
        timeout: Duration,
    ) -> std::result::Result<bool, McpError> {
        let escaped = escape_for_shell(remote_path);
        let cmd = format!(
            r#"sh -c 'if [ -f "$1" ] && [ -s "$1" ]; then printf "1"; else printf "0"; fi' sh '{escaped}'"#
        );
        let out = self
            .connection
            .exec_command(&cmd, timeout)
            .await
            .map_err(|e| {
                McpError::internal_error(format!("failed to check remote file status: {e}"), None)
            })?;
        Ok(out.stdout.trim() == "1")
    }

    pub(in crate::server) async fn execute_file_write_transaction(
        &self,
        request: FileWriteTransactionRequest<'_>,
    ) -> std::result::Result<CallToolResult, McpError> {
        let FileWriteTransactionRequest {
            remote_path,
            new_content,
            expected_sha256,
            timeout,
            fault_injection,
            too_large_error,
            operation_name,
        } = request;

        let bytes_written = new_content.len();
        if bytes_written > FILE_EDIT_HARD_MAX_BYTES {
            return Ok(CallToolResult::error(vec![Content::text(too_large_error)]));
        }

        if let Err(e) = self.connection.ensure_connected().await {
            error!(error = ?e, "Failed to ensure SSH connection");
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "SSH connection error: {}",
                e
            ))]));
        }

        let local_tmp_rel = format!("target/tmp/file-edit-{}.tmp", make_job_id());
        let local_tmp_path = self.transfer.local_root().join(&local_tmp_rel);

        if let Some(parent) = local_tmp_path.parent()
            && let Err(e) = tokio::fs::create_dir_all(parent).await
        {
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "Error: failed to create local staging directory: {e}"
            ))]));
        }

        let mut local_tmp_opts = tokio::fs::OpenOptions::new();
        local_tmp_opts.write(true).create_new(true);

        #[cfg(unix)]
        {
            local_tmp_opts.custom_flags(O_NOFOLLOW_FLAG);
        }

        let mut local_tmp_file = match local_tmp_opts.open(&local_tmp_path).await {
            Ok(file) => file,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error: failed to create local staging file: {e}"
                ))]));
            }
        };

        if let Err(e) = local_tmp_file.write_all(new_content.as_bytes()).await {
            let _ = tokio::fs::remove_file(&local_tmp_path).await;
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "Error: failed to write local staging file: {e}"
            ))]));
        }
        if let Err(e) = local_tmp_file.flush().await {
            let _ = tokio::fs::remove_file(&local_tmp_path).await;
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "Error: failed to flush local staging file: {e}"
            ))]));
        }
        if let Err(e) = local_tmp_file.sync_all().await {
            let _ = tokio::fs::remove_file(&local_tmp_path).await;
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "Error: failed to sync local staging file: {e}"
            ))]));
        }
        drop(local_tmp_file);

        let remote_lock_dir = format!("{}.ssh-mcp-lock", remote_path);
        let remote_stage_path = format!("{}.ssh-mcp-stage-{}", remote_path, make_job_id());
        let expected_for_remote = expected_sha256.clone().unwrap_or_else(|| "-".to_string());
        let missing_sha_for_remote = FILE_EDIT_MISSING_SHA256;
        let lock_stale_after_secs = FILE_EDIT_LOCK_STALE_AFTER_SECS.to_string();

        let dst_escaped = escape_for_shell(remote_path);
        let expected_escaped = escape_for_shell(&expected_for_remote);
        let missing_sha_escaped = escape_for_shell(missing_sha_for_remote);
        let lock_escaped = escape_for_shell(&remote_lock_dir);
        let stage_escaped = escape_for_shell(&remote_stage_path);
        let lock_stale_after_secs_escaped = escape_for_shell(&lock_stale_after_secs);
        let operation_name_escaped = escape_for_shell(operation_name);
        let force_fail_before_finalize =
            if fault_injection == FileEditFaultInjection::FailBeforeFinalize {
                "1"
            } else {
                "0"
            };
        let force_sha256_unavailable =
            if fault_injection == FileEditFaultInjection::Sha256Unavailable {
                "1"
            } else {
                "0"
            };
        let force_fail_before_finalize_escaped = escape_for_shell(force_fail_before_finalize);
        let force_sha256_unavailable_escaped = escape_for_shell(force_sha256_unavailable);

        let apply_cmd = format!(
            r#"sh -c 'set -eu; dst=$1; expected=$2; lock_dir=$3; stage=$4; fail_before_finalize=$5; missing_sha=$6; force_sha_unavailable=$7; stale_after_secs=$8; operation_name=$9; \
              drain_stdin() {{ cat > /dev/null || true; }}; \
              sha256_file() {{ file=$1; if command -v sha256sum >/dev/null 2>&1; then set -- $(sha256sum -- "$file"); printf "%s\n" "$1"; return 0; fi; if command -v shasum >/dev/null 2>&1; then set -- $(shasum -a 256 -- "$file"); printf "%s\n" "$1"; return 0; fi; return 1; }}; \
              reclaim_stale_lock() {{ now_epoch=$1; lock_started_path=$lock_dir/started_at; lock_operation_path=$lock_dir/operation; if [ ! -f "$lock_started_path" ]; then return 1; fi; if ! IFS= read -r lock_started_at < "$lock_started_path"; then return 1; fi; case "$lock_started_at" in ""|*[!0-9]*) return 1 ;; esac; if [ "$lock_started_at" -gt "$now_epoch" ]; then return 1; fi; lock_age=$((now_epoch - lock_started_at)); if [ "$lock_age" -lt "$stale_after_secs" ]; then return 1; fi; rm -f -- "$lock_started_path" "$lock_operation_path" 2>/dev/null || true; rmdir -- "$lock_dir" 2>/dev/null; }}; \
              parent=${{dst%/*}}; if [ -z "$parent" ]; then parent=/; fi; if [ ! -d "$parent" ]; then printf "%s\n" "{FILE_EDIT_ERROR_MARKER}parent_not_found" >&2; drain_stdin; exit 1; fi; \
              lock_spins=0; while ! mkdir -- "$lock_dir" 2>/dev/null; do if [ -d "$lock_dir" ]; then if now_epoch=$(date +%s 2>/dev/null); then if reclaim_stale_lock "$now_epoch"; then continue; fi; fi; lock_spins=$((lock_spins + 1)); if [ "$lock_spins" -ge {FILE_EDIT_LOCK_MAX_SPINS} ]; then printf "%s\n" "{FILE_EDIT_ERROR_MARKER}lock_busy" >&2; drain_stdin; exit 1; fi; sleep 1; continue; fi; printf "%s\n" "{FILE_EDIT_ERROR_MARKER}lock_acquire_failed" >&2; drain_stdin; exit 1; done; \
              lock_started_path=$lock_dir/started_at; lock_operation_path=$lock_dir/operation; if now_epoch=$(date +%s 2>/dev/null); then printf "%s\n" "$now_epoch" > "$lock_started_path" 2>/dev/null || true; fi; printf "%s\n" "$operation_name" > "$lock_operation_path" 2>/dev/null || true; \
              cleanup() {{ rm -f -- "$stage" "$lock_started_path" "$lock_operation_path" 2>/dev/null || true; rmdir -- "$lock_dir" 2>/dev/null || true; }}; \
              trap cleanup EXIT INT TERM; \
              if [ "$force_sha_unavailable" = "1" ]; then printf "%s\n" "{FILE_EDIT_ERROR_MARKER}sha256_unavailable" >&2; drain_stdin; exit 1; fi; \
              if ! sha256_file /dev/null >/dev/null 2>&1; then printf "%s\n" "{FILE_EDIT_ERROR_MARKER}sha256_unavailable" >&2; drain_stdin; exit 1; fi; \
             if [ -e "$dst" ]; then if [ ! -f "$dst" ]; then printf "%s\n" "{FILE_EDIT_ERROR_MARKER}not_regular_file" >&2; drain_stdin; exit 1; fi; if ! current_hash=$(sha256_file "$dst"); then printf "%s\n" "{FILE_EDIT_ERROR_MARKER}sha256_unavailable" >&2; drain_stdin; exit 1; fi; else if [ "$expected" != "-" ]; then printf "%s%s\n" "{FILE_EDIT_ACTUAL_SHA_MARKER}" "$missing_sha" >&2; printf "%s\n" "{FILE_EDIT_CONFLICT_MARKER}" >&2; drain_stdin; exit 3; fi; current_hash=$missing_sha; fi; \
             printf "%s%s\n" "{FILE_EDIT_PREVIOUS_SHA_MARKER}" "$current_hash" >&2; \
             if [ "$expected" != "-" ] && [ "$expected" != "$current_hash" ]; then printf "%s%s\n" "{FILE_EDIT_ACTUAL_SHA_MARKER}" "$current_hash" >&2; printf "%s\n" "{FILE_EDIT_CONFLICT_MARKER}" >&2; drain_stdin; exit 3; fi; \
             if ! : > "$stage" 2>/dev/null; then printf "%s\n" "{FILE_EDIT_ERROR_MARKER}staging_unwritable" >&2; drain_stdin; exit 1; fi; \
             if ! cat > "$stage"; then printf "%s\n" "{FILE_EDIT_ERROR_MARKER}stage_write_failed" >&2; exit 1; fi; \
             if [ "$fail_before_finalize" = "1" ]; then printf "%s\n" "{FILE_EDIT_ERROR_MARKER}finalize_failed" >&2; exit 1; fi; \
             if ! mv -- "$stage" "$dst"; then printf "%s\n" "{FILE_EDIT_ERROR_MARKER}finalize_failed" >&2; exit 1; fi; \
             if ! new_hash=$(sha256_file "$dst"); then printf "%s\n" "{FILE_EDIT_ERROR_MARKER}sha256_unavailable" >&2; exit 1; fi; \
              printf "%s%s\n" "{FILE_EDIT_NEW_SHA_MARKER}" "$new_hash" >&2; \
              trap - EXIT INT TERM; cleanup' sh '{dst_escaped}' '{expected_escaped}' '{lock_escaped}' '{stage_escaped}' '{force_fail_before_finalize_escaped}' '{missing_sha_escaped}' '{force_sha256_unavailable_escaped}' '{lock_stale_after_secs_escaped}' '{operation_name_escaped}'"#
        );

        let mut local_tmp_input = match tokio::fs::File::open(&local_tmp_path).await {
            Ok(file) => file,
            Err(e) => {
                let _ = tokio::fs::remove_file(&local_tmp_path).await;
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error: failed to open local staging file for upload: {e}"
                ))]));
            }
        };
        let mut sink = tokio::io::sink();
        let out = match self
            .connection
            .exec_raw_streaming(
                &apply_cmd,
                Some(&mut local_tmp_input),
                Some(&mut sink),
                timeout,
            )
            .await
        {
            Ok(output) => output,
            Err(e) => {
                let _ = tokio::fs::remove_file(&local_tmp_path).await;
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error running {operation_name}: {e}"
                ))]));
            }
        };

        let _ = tokio::fs::remove_file(&local_tmp_path).await;

        if has_file_edit_conflict_marker(&out.stderr) {
            let expected = match expected_sha256 {
                Some(value) => value,
                None => {
                    return Ok(CallToolResult::error(vec![Content::text(format!(
                        "Error: {operation_name} conflict marker was returned without expected_sha256"
                    ))]));
                }
            };

            let actual_raw = parse_file_edit_marker_value(&out.stderr, FILE_EDIT_ACTUAL_SHA_MARKER)
                .or_else(|| {
                    parse_file_edit_marker_value(&out.stderr, FILE_EDIT_PREVIOUS_SHA_MARKER)
                });
            let actual_sha256 = match actual_raw {
                Some(value) => match normalize_sha256_hex(value, "actual_sha256") {
                    Ok(normalized) => normalized,
                    Err(_) => {
                        return Ok(CallToolResult::error(vec![Content::text(
                            "Error: failed to parse remote SHA-256 output".to_string(),
                        )]));
                    }
                },
                None => {
                    return Ok(CallToolResult::error(vec![Content::text(format!(
                        "Error: {operation_name} conflict response missing actual_sha256"
                    ))]));
                }
            };

            return Ok(build_file_edit_conflict_result(
                remote_path,
                &expected,
                &actual_sha256,
            ));
        }

        if let Some(marker) = parse_file_edit_error_marker(&out.stderr) {
            if marker == "lock_busy" {
                return Ok(file_edit_lock_busy_result(remote_path, operation_name));
            }

            let msg = match marker {
                "not_found" => "Error: remote_path does not exist".to_string(),
                "parent_not_found" => "Error: remote parent directory does not exist".to_string(),
                "not_regular_file" => "Error: remote_path is not a regular file".to_string(),
                "sha256_unavailable" => {
                    "Error: remote host does not provide SHA-256 utilities".to_string()
                }
                "lock_acquire_failed" => {
                    format!(
                        "Error: failed to acquire remote {operation_name} lock due to filesystem error"
                    )
                }
                "staging_unwritable" => {
                    "Error: failed to create remote staging file in destination directory"
                        .to_string()
                }
                "stage_write_failed" => "Error: failed to write remote staging file".to_string(),
                "finalize_failed" => "Error: failed to atomically replace remote file".to_string(),
                _ => format!("Error: {operation_name} failed on remote host"),
            };
            return Ok(CallToolResult::error(vec![Content::text(msg)]));
        }

        match out.exit_code {
            Some(0) => {}
            Some(code) => {
                let mut msg = format!(
                    "Error running {operation_name}: remote command failed with exit_code={code}"
                );
                if let Some(snippet) = sanitize_read_file_stderr_snippet(&out.stderr) {
                    msg.push_str(&format!("; stderr={snippet}"));
                }
                return Ok(CallToolResult::error(vec![Content::text(msg)]));
            }
            None => {
                if !out.stderr.trim().is_empty() {
                    let mut msg = format!(
                        "Error running {operation_name}: remote command did not provide an exit status"
                    );
                    if let Some(snippet) = sanitize_read_file_stderr_snippet(&out.stderr) {
                        msg.push_str(&format!("; stderr={snippet}"));
                    }
                    return Ok(CallToolResult::error(vec![Content::text(msg)]));
                }
            }
        }

        let previous_sha_raw =
            match parse_file_edit_marker_value(&out.stderr, FILE_EDIT_PREVIOUS_SHA_MARKER) {
                Some(value) => value,
                None => {
                    return Ok(CallToolResult::error(vec![Content::text(format!(
                        "Error: missing previous_sha256 marker from {operation_name} response"
                    ))]));
                }
            };
        let previous_sha256 = match normalize_sha256_hex(previous_sha_raw, "previous_sha256") {
            Ok(normalized) => normalized,
            Err(_) => {
                return Ok(CallToolResult::error(vec![Content::text(
                    "Error: failed to parse remote SHA-256 output".to_string(),
                )]));
            }
        };

        let new_sha_raw = match parse_file_edit_marker_value(&out.stderr, FILE_EDIT_NEW_SHA_MARKER)
        {
            Some(value) => value,
            None => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error: missing new_sha256 marker from {operation_name} response"
                ))]));
            }
        };
        let new_sha256 = match normalize_sha256_hex(new_sha_raw, "new_sha256") {
            Ok(normalized) => normalized,
            Err(_) => {
                return Ok(CallToolResult::error(vec![Content::text(
                    "Error: failed to parse remote SHA-256 output".to_string(),
                )]));
            }
        };

        let changed = previous_sha256 != new_sha256;
        let result = serde_json::json!({
            "path": remote_path,
            "previous_sha256": previous_sha256,
            "new_sha256": new_sha256,
            "bytes_written": bytes_written,
            "changed": changed,
        });

        Ok(CallToolResult::success(vec![Content::text(
            result.to_string(),
        )]))
    }
}

#[cfg(test)]
mod tests {
    use super::file_edit_lock_busy_result;

    #[test]
    fn test_file_edit_lock_busy_error_is_structured_retryable_json() {
        let result = file_edit_lock_busy_result("/tmp/example.txt", "replace-in-file");
        assert_eq!(result.is_error, Some(true));

        let text = result
            .content
            .first()
            .and_then(|content| content.raw.as_text().map(|text| text.text.as_str()))
            .expect("lock busy error should include text content");

        let json: serde_json::Value =
            serde_json::from_str(text).expect("lock busy response should be valid JSON");
        assert_eq!(
            json.get("error").and_then(|value| value.as_str()),
            Some("lock_busy")
        );
        assert_eq!(
            json.get("path").and_then(|value| value.as_str()),
            Some("/tmp/example.txt")
        );
        assert_eq!(
            json.get("retryable").and_then(|value| value.as_bool()),
            Some(true)
        );
        assert_eq!(
            json.get("retry_same_tool")
                .and_then(|value| value.as_bool()),
            Some(true)
        );
        assert!(
            json.get("message")
                .and_then(|value| value.as_str())
                .expect("lock busy response should include message")
                .contains("do not fall back to exec/sudo-exec")
        );
    }
}
