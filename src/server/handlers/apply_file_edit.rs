use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tracing::{debug, error};

#[cfg(unix)]
use crate::platform::O_NOFOLLOW_FLAG;
use crate::server::SshMcpServer;
use crate::server::validation::apply_file_edit::*;
use crate::server::validation::common::extract_text_from_call_tool_result;
use crate::server::validation::read_file::{normalize_sha256_hex, sanitize_read_file_stderr_snippet};
use crate::server::validation::common::validate_read_file_path;
use crate::server::make_job_id;
use crate::shell_escape::escape_for_shell;
use crate::tools::{ApplyFileEditParams, ApplyFileEditMode, ApplyFileEditFaultInjection, ReadFileMode, ReadFileParams};

impl SshMcpServer {
    pub(in crate::server) async fn execute_apply_file_edit(
        &self,
        params: ApplyFileEditParams,
        fault_injection: ApplyFileEditFaultInjection,
    ) -> std::result::Result<CallToolResult, McpError> {
        debug!(remote_path = ?params.remote_path, "apply-file-edit tool called");

        let ApplyFileEditParams {
            remote_path,
            new_content,
            old_text,
            new_text,
            replace_all,
            expected_sha256,
            read_ticket,
            timeout_ms,
        } = params;

        validate_read_file_path(&remote_path).map_err(|msg| McpError::invalid_params(msg, None))?;

        let user_expected_sha256 = match expected_sha256.as_deref() {
            Some(value) => Some(
                normalize_sha256_hex(value, "expected_sha256")
                    .map_err(|msg| McpError::invalid_params(msg, None))?,
            ),
            None => None,
        };

        let timeout = match timeout_ms {
            Some(0) => {
                return Err(McpError::invalid_params(
                    "timeout_ms must be a positive integer",
                    None,
                ));
            }
            Some(ms) => Duration::from_millis(ms),
            None => self.timeout,
        };
        let mode_error = "apply-file-edit requires exactly one mode: provide new_content for full mode, or provide old_text and new_text for partial mode (replace_all is only valid in partial mode)";

        let edit_mode = match (new_content, old_text, new_text, replace_all) {
            (Some(new_content), None, None, None) => ApplyFileEditMode::Full { new_content },
            (None, Some(old_text), Some(new_text), replace_all) => ApplyFileEditMode::Partial {
                old_text,
                new_text,
                replace_all: replace_all.unwrap_or(false),
            },
            _ => return Err(McpError::invalid_params(mode_error, None)),
        };

        // ── Read-ticket enforcement ──────────────────────────────────────
        // Full mode: require a valid read_ticket when editing a non-empty
        // existing file. Partial mode reads the file internally, so the
        // precondition is implicitly satisfied.
        if let ApplyFileEditMode::Full { .. } = &edit_mode {
            match read_ticket {
                Some(ref ticket) => {
                    // Ticket provided — verify it matches the path.
                    self.ticket_signer
                        .verify(ticket, &remote_path)
                        .map_err(|e| {
                            McpError::invalid_params(
                                format!("read_ticket verification failed: {e}"),
                                None,
                            )
                        })?;
                }
                None => {
                    // No ticket — check if file exists and is non-empty.
                    if self
                        .check_remote_file_nonempty(&remote_path, timeout)
                        .await?
                    {
                        return Err(McpError::invalid_params(
                            "Error: existing non-empty file must be read before editing. Call read-file first, then pass the returned read_ticket to apply-file-edit.",
                            None,
                        ));
                    }
                    // File is missing or empty — proceed with creation/overwrite.
                }
            }
        }

        let (next_content, partial_baseline_sha256) = match edit_mode {
            ApplyFileEditMode::Full { new_content } => (new_content, None),
            ApplyFileEditMode::Partial {
                old_text,
                new_text,
                replace_all,
            } => {
                if old_text.is_empty() {
                    return Err(McpError::invalid_params(
                        "old_text must not be empty in partial mode",
                        None,
                    ));
                }

                let read_result = self
                    .execute_read_file(ReadFileParams {
                        remote_path: remote_path.clone(),
                        mode: ReadFileMode::Full,
                        lines: None,
                        timeout_ms,
                    })
                    .await?;

                if read_result.is_error.unwrap_or(false) {
                    return Ok(read_result);
                }

                let read_text = extract_text_from_call_tool_result(&read_result);
                let read_value: serde_json::Value = serde_json::from_str(read_text.trim()).map_err(|e| {
                    McpError::internal_error(
                        format!(
                            "failed to parse read-file response while preparing partial apply-file-edit: {e}"
                        ),
                        None,
                    )
                })?;

                let current_content = read_value
                    .get("content")
                    .and_then(|value| value.as_str())
                    .ok_or_else(|| {
                        McpError::internal_error(
                            "read-file response missing content while preparing partial apply-file-edit"
                                .to_string(),
                            None,
                        )
                    })?;

                let match_count = current_content.matches(old_text.as_str()).count();
                if match_count == 0 {
                    return Ok(CallToolResult::error(vec![Content::text(
                        "Error: old_text was not found in remote file".to_string(),
                    )]));
                }

                if !replace_all && match_count != 1 {
                    return Ok(CallToolResult::error(vec![Content::text(format!(
                        "Error: old_text matched {match_count} times; set replace_all=true to replace all matches"
                    ))]));
                }

                let partial_baseline_sha256 = if user_expected_sha256.is_none() {
                    let baseline = match self
                        .compute_partial_baseline_sha256(current_content, timeout)
                        .await
                    {
                        Ok(value) => value,
                        Err(result) => return Ok(result),
                    };
                    Some(baseline)
                } else {
                    None
                };

                if let Err(result) = self
                    .apply_partial_fault_injection(remote_path.as_str(), timeout, fault_injection)
                    .await
                {
                    return Ok(result);
                }

                let updated_content = if replace_all {
                    current_content.replace(old_text.as_str(), new_text.as_str())
                } else {
                    current_content.replacen(old_text.as_str(), new_text.as_str(), 1)
                };

                (updated_content, partial_baseline_sha256)
            }
        };

        let expected_sha256 = user_expected_sha256.or(partial_baseline_sha256);

        self.execute_apply_file_edit_write_transaction(
            remote_path.as_str(),
            next_content.as_str(),
            expected_sha256,
            timeout,
            fault_injection,
        )
        .await
    }

    pub(in crate::server) async fn compute_partial_baseline_sha256(
        &self,
        content: &str,
        timeout: Duration,
    ) -> std::result::Result<String, CallToolResult> {
        let hash_cmd = format!(
            r#"sh -c 'set -eu; sha256_stdin() {{ if command -v sha256sum >/dev/null 2>&1; then set -- $(sha256sum); printf "%s\n" "$1"; return 0; fi; if command -v shasum >/dev/null 2>&1; then set -- $(shasum -a 256); printf "%s\n" "$1"; return 0; fi; return 1; }}; if ! baseline_hash=$(sha256_stdin); then printf "%s\n" "{APPLY_FILE_EDIT_ERROR_MARKER}sha256_unavailable" >&2; exit 1; fi; printf "%s%s\n" "{APPLY_FILE_EDIT_BASELINE_SHA_MARKER}" "$baseline_hash" >&2'"#
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
                    "Error: failed to hash partial baseline content on remote host: {e}"
                ))]));
            }
        };

        if let Some(marker) = parse_apply_file_edit_error_marker(&out.stderr)
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
                    "Error: failed to hash partial baseline content: remote command failed with exit_code={code}"
                );
                if let Some(snippet) = sanitize_read_file_stderr_snippet(&out.stderr) {
                    msg.push_str(&format!("; stderr={snippet}"));
                }
                return Err(CallToolResult::error(vec![Content::text(msg)]));
            }
            None => {
                if !out.stderr.trim().is_empty() {
                    let mut msg = "Error: failed to hash partial baseline content: remote command did not provide an exit status".to_string();
                    if let Some(snippet) = sanitize_read_file_stderr_snippet(&out.stderr) {
                        msg.push_str(&format!("; stderr={snippet}"));
                    }
                    return Err(CallToolResult::error(vec![Content::text(msg)]));
                }
            }
        }

        let baseline_raw = match parse_apply_file_edit_marker_value(
            &out.stderr,
            APPLY_FILE_EDIT_BASELINE_SHA_MARKER,
        ) {
            Some(value) => value,
            None => {
                return Err(CallToolResult::error(vec![Content::text(
                    "Error: missing partial baseline SHA-256 marker".to_string(),
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
        fault_injection: ApplyFileEditFaultInjection,
    ) -> std::result::Result<(), CallToolResult> {
        let injected_cmd = match fault_injection {
            ApplyFileEditFaultInjection::PartialDeleteBeforeWrite => {
                let escaped = escape_for_shell(remote_path);
                Some(format!("sh -c 'set -eu; rm -f -- \"$1\"' sh '{escaped}'"))
            }
            ApplyFileEditFaultInjection::PartialMutateBeforeWrite => {
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
                    "Error: failed to run partial race fault injection: {e}"
                ))]));
            }
        };

        match out.exit_code {
            Some(0) => Ok(()),
            Some(code) => Err(CallToolResult::error(vec![Content::text(format!(
                "Error: partial race fault injection failed with exit_code={code}"
            ))])),
            None => Err(CallToolResult::error(vec![Content::text(
                "Error: partial race fault injection did not report exit status".to_string(),
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

    pub(in crate::server) async fn execute_apply_file_edit_write_transaction(
        &self,
        remote_path: &str,
        new_content: &str,
        expected_sha256: Option<String>,
        timeout: Duration,
        fault_injection: ApplyFileEditFaultInjection,
    ) -> std::result::Result<CallToolResult, McpError> {
        let bytes_written = new_content.len();
        if bytes_written > APPLY_FILE_EDIT_HARD_MAX_BYTES {
            return Ok(CallToolResult::error(vec![Content::text(
                apply_file_edit_too_large_error(APPLY_FILE_EDIT_HARD_MAX_BYTES),
            )]));
        }

        if let Err(e) = self.connection.ensure_connected().await {
            error!(error = ?e, "Failed to ensure SSH connection");
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "SSH connection error: {}",
                e
            ))]));
        }

        let local_tmp_rel = format!("target/tmp/apply-file-edit-{}.tmp", make_job_id());
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
        let missing_sha_for_remote = APPLY_FILE_EDIT_MISSING_SHA256;

        let dst_escaped = escape_for_shell(remote_path);
        let expected_escaped = escape_for_shell(&expected_for_remote);
        let missing_sha_escaped = escape_for_shell(missing_sha_for_remote);
        let lock_escaped = escape_for_shell(&remote_lock_dir);
        let stage_escaped = escape_for_shell(&remote_stage_path);
        let force_fail_before_finalize =
            if fault_injection == ApplyFileEditFaultInjection::FailBeforeFinalize {
                "1"
            } else {
                "0"
            };
        let force_sha256_unavailable =
            if fault_injection == ApplyFileEditFaultInjection::Sha256Unavailable {
                "1"
            } else {
                "0"
            };
        let force_fail_before_finalize_escaped = escape_for_shell(force_fail_before_finalize);
        let force_sha256_unavailable_escaped = escape_for_shell(force_sha256_unavailable);

        let apply_cmd = format!(
            r#"sh -c 'set -eu; dst=$1; expected=$2; lock_dir=$3; stage=$4; fail_before_finalize=$5; missing_sha=$6; force_sha_unavailable=$7; \
             drain_stdin() {{ cat > /dev/null || true; }}; \
             sha256_file() {{ file=$1; if command -v sha256sum >/dev/null 2>&1; then set -- $(sha256sum -- "$file"); printf "%s\n" "$1"; return 0; fi; if command -v shasum >/dev/null 2>&1; then set -- $(shasum -a 256 -- "$file"); printf "%s\n" "$1"; return 0; fi; return 1; }}; \
             parent=${{dst%/*}}; if [ -z "$parent" ]; then parent=/; fi; if [ ! -d "$parent" ]; then printf "%s\n" "{APPLY_FILE_EDIT_ERROR_MARKER}parent_not_found" >&2; drain_stdin; exit 1; fi; \
             lock_spins=0; while ! mkdir -- "$lock_dir" 2>/dev/null; do if [ -d "$lock_dir" ]; then lock_spins=$((lock_spins + 1)); if [ "$lock_spins" -ge 20 ]; then printf "%s\n" "{APPLY_FILE_EDIT_ERROR_MARKER}lock_busy" >&2; drain_stdin; exit 1; fi; sleep 1; continue; fi; printf "%s\n" "{APPLY_FILE_EDIT_ERROR_MARKER}lock_acquire_failed" >&2; drain_stdin; exit 1; done; \
             cleanup() {{ rm -f -- "$stage" 2>/dev/null || true; rmdir -- "$lock_dir" 2>/dev/null || true; }}; \
             trap cleanup EXIT INT TERM; \
             if [ "$force_sha_unavailable" = "1" ]; then printf "%s\n" "{APPLY_FILE_EDIT_ERROR_MARKER}sha256_unavailable" >&2; drain_stdin; exit 1; fi; \
             if ! sha256_file /dev/null >/dev/null 2>&1; then printf "%s\n" "{APPLY_FILE_EDIT_ERROR_MARKER}sha256_unavailable" >&2; drain_stdin; exit 1; fi; \
             if [ -e "$dst" ]; then if [ ! -f "$dst" ]; then printf "%s\n" "{APPLY_FILE_EDIT_ERROR_MARKER}not_regular_file" >&2; drain_stdin; exit 1; fi; if ! current_hash=$(sha256_file "$dst"); then printf "%s\n" "{APPLY_FILE_EDIT_ERROR_MARKER}sha256_unavailable" >&2; drain_stdin; exit 1; fi; else if [ "$expected" != "-" ]; then printf "%s%s\n" "{APPLY_FILE_EDIT_ACTUAL_SHA_MARKER}" "$missing_sha" >&2; printf "%s\n" "{APPLY_FILE_EDIT_CONFLICT_MARKER}" >&2; drain_stdin; exit 3; fi; current_hash=$missing_sha; fi; \
             printf "%s%s\n" "{APPLY_FILE_EDIT_PREVIOUS_SHA_MARKER}" "$current_hash" >&2; \
             if [ "$expected" != "-" ] && [ "$expected" != "$current_hash" ]; then printf "%s%s\n" "{APPLY_FILE_EDIT_ACTUAL_SHA_MARKER}" "$current_hash" >&2; printf "%s\n" "{APPLY_FILE_EDIT_CONFLICT_MARKER}" >&2; drain_stdin; exit 3; fi; \
             if ! : > "$stage" 2>/dev/null; then printf "%s\n" "{APPLY_FILE_EDIT_ERROR_MARKER}staging_unwritable" >&2; drain_stdin; exit 1; fi; \
             if ! cat > "$stage"; then printf "%s\n" "{APPLY_FILE_EDIT_ERROR_MARKER}stage_write_failed" >&2; exit 1; fi; \
             if [ "$fail_before_finalize" = "1" ]; then printf "%s\n" "{APPLY_FILE_EDIT_ERROR_MARKER}finalize_failed" >&2; exit 1; fi; \
             if ! mv -- "$stage" "$dst"; then printf "%s\n" "{APPLY_FILE_EDIT_ERROR_MARKER}finalize_failed" >&2; exit 1; fi; \
             if ! new_hash=$(sha256_file "$dst"); then printf "%s\n" "{APPLY_FILE_EDIT_ERROR_MARKER}sha256_unavailable" >&2; exit 1; fi; \
             printf "%s%s\n" "{APPLY_FILE_EDIT_NEW_SHA_MARKER}" "$new_hash" >&2; \
             trap - EXIT INT TERM; cleanup' sh '{dst_escaped}' '{expected_escaped}' '{lock_escaped}' '{stage_escaped}' '{force_fail_before_finalize_escaped}' '{missing_sha_escaped}' '{force_sha256_unavailable_escaped}'"#
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
                    "Error applying file edit: {e}"
                ))]));
            }
        };

        let _ = tokio::fs::remove_file(&local_tmp_path).await;

        if has_apply_file_edit_conflict_marker(&out.stderr) {
            let expected = match expected_sha256 {
                Some(value) => value,
                None => {
                    return Ok(CallToolResult::error(vec![Content::text(
                        "Error: apply-file-edit conflict marker was returned without expected_sha256"
                            .to_string(),
                    )]));
                }
            };

            let actual_raw =
                parse_apply_file_edit_marker_value(&out.stderr, APPLY_FILE_EDIT_ACTUAL_SHA_MARKER)
                    .or_else(|| {
                        parse_apply_file_edit_marker_value(
                            &out.stderr,
                            APPLY_FILE_EDIT_PREVIOUS_SHA_MARKER,
                        )
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
                    return Ok(CallToolResult::error(vec![Content::text(
                        "Error: apply-file-edit conflict response missing actual_sha256"
                            .to_string(),
                    )]));
                }
            };

            let conflict = serde_json::json!({
                "error": "conflict",
                "path": remote_path,
                "expected_sha256": expected,
                "actual_sha256": actual_sha256,
            });
            return Ok(CallToolResult::error(vec![Content::text(
                conflict.to_string(),
            )]));
        }

        if let Some(marker) = parse_apply_file_edit_error_marker(&out.stderr) {
            let msg = match marker {
                "not_found" => "Error: remote_path does not exist".to_string(),
                "parent_not_found" => "Error: remote parent directory does not exist".to_string(),
                "not_regular_file" => "Error: remote_path is not a regular file".to_string(),
                "sha256_unavailable" => {
                    "Error: remote host does not provide SHA-256 utilities".to_string()
                }
                "lock_busy" => {
                    "Error: remote_path is being edited by another operation; retry shortly"
                        .to_string()
                }
                "lock_acquire_failed" => {
                    "Error: failed to acquire remote apply-file-edit lock due to filesystem error"
                        .to_string()
                }
                "staging_unwritable" => {
                    "Error: failed to create remote staging file in destination directory"
                        .to_string()
                }
                "stage_write_failed" => "Error: failed to write remote staging file".to_string(),
                "finalize_failed" => "Error: failed to atomically replace remote file".to_string(),
                _ => "Error: apply-file-edit failed on remote host".to_string(),
            };
            return Ok(CallToolResult::error(vec![Content::text(msg)]));
        }

        match out.exit_code {
            Some(0) => {}
            Some(code) => {
                let mut msg = format!(
                    "Error applying file edit: remote command failed with exit_code={code}"
                );
                if let Some(snippet) = sanitize_read_file_stderr_snippet(&out.stderr) {
                    msg.push_str(&format!("; stderr={snippet}"));
                }
                return Ok(CallToolResult::error(vec![Content::text(msg)]));
            }
            None => {
                if !out.stderr.trim().is_empty() {
                    let mut msg =
                        "Error applying file edit: remote command did not provide an exit status"
                            .to_string();
                    if let Some(snippet) = sanitize_read_file_stderr_snippet(&out.stderr) {
                        msg.push_str(&format!("; stderr={snippet}"));
                    }
                    return Ok(CallToolResult::error(vec![Content::text(msg)]));
                }
            }
        }

        let previous_sha_raw = match parse_apply_file_edit_marker_value(
            &out.stderr,
            APPLY_FILE_EDIT_PREVIOUS_SHA_MARKER,
        ) {
            Some(value) => value,
            None => {
                return Ok(CallToolResult::error(vec![Content::text(
                    "Error: missing previous_sha256 marker from apply-file-edit response"
                        .to_string(),
                )]));
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

        let new_sha_raw =
            match parse_apply_file_edit_marker_value(&out.stderr, APPLY_FILE_EDIT_NEW_SHA_MARKER) {
                Some(value) => value,
                None => {
                    return Ok(CallToolResult::error(vec![Content::text(
                        "Error: missing new_sha256 marker from apply-file-edit response"
                            .to_string(),
                    )]));
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
