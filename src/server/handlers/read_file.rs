use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{debug, error};

#[cfg(unix)]
use crate::platform::O_NOFOLLOW_FLAG;
use crate::server::SshMcpServer;
use crate::server::validation::read_file::*;
use crate::server::validation::{parse_read_file_error_marker, validate_read_file_path};
use crate::server::{READ_FILE_ERROR_MARKER, make_job_id};
use crate::ssh::escape_for_shell;
use crate::tools::{ReadFileMode, ReadFileParams};

impl SshMcpServer {
    /// Execute read-file tool
    pub(in crate::server) async fn execute_read_file(
        &self,
        params: ReadFileParams,
    ) -> std::result::Result<CallToolResult, McpError> {
        let ReadFileParams {
            remote_path,
            mode,
            lines,
            timeout_ms,
        } = params;

        debug!(
            remote_path = ?remote_path,
            mode = mode.as_str(),
            lines = ?lines,
            "read-file tool called"
        );

        validate_read_file_path(&remote_path).map_err(|msg| McpError::invalid_params(msg, None))?;

        let line_limit = resolve_read_file_line_limit(mode, lines)
            .map_err(|msg| McpError::invalid_params(msg, None))?;

        let timeout = match timeout_ms {
            Some(0) => {
                return Err(McpError::invalid_params(
                    "timeout_ms must be a positive integer",
                    None,
                ));
            }
            Some(ms) => std::time::Duration::from_millis(ms),
            None => self.timeout,
        };
        let max_read_bytes = resolve_read_file_max_bytes(self.config.max_output_tokens);

        if let Err(e) = self.connection.ensure_connected().await {
            error!(error = ?e, "Failed to ensure SSH connection");
            return Ok(CallToolResult::error(vec![Content::text(e.to_string())]));
        }

        let capture_path = self
            .spooler
            .base_dir()
            .join(format!("read-file-{}.tmp", make_job_id()));

        let mut capture_opts = tokio::fs::OpenOptions::new();
        capture_opts.write(true).create_new(true);

        #[cfg(unix)]
        {
            capture_opts.custom_flags(O_NOFOLLOW_FLAG);
        }

        let mut capture_file = match capture_opts.open(&capture_path).await {
            Ok(file) => file,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error: failed to create local read capture file: {e}"
                ))]));
            }
        };

        let escaped_path = escape_for_shell(&remote_path);
        let n = line_limit.unwrap_or(0);

        // Mode-specific producer appended after the shared preamble.
        //   full:     stat-gate rejects oversized files BEFORE any byte is streamed.
        //   head/preview/tail: bounded remote producers (head/tail -n N | head -c max+1)
        //                       so they work correctly on files of ANY size.
        let mode_suffix = match mode {
            ReadFileMode::Full => format!(
                r#"; if [ "$size" -gt "$max" ]; then printf "%s\n" "{err}too_large" >&2; exit 0; fi; head -c "$((max + 1))" < "$p""#,
                err = READ_FILE_ERROR_MARKER,
            ),
            ReadFileMode::Head | ReadFileMode::Preview => format!(
                r#"; head -n "$n" "$p" | head -c "$((max + 1))"; if [ -n "$(sed -n "$((n+1))=" "$p")" ]; then printf "%s\n" "{trunc_m}" >&2; fi"#,
                trunc_m = READ_FILE_TRUNC_MARKER,
            ),
            ReadFileMode::Tail => format!(
                r#"; tail -n "$n" "$p" | head -c "$((max + 1))"; if [ -n "$(sed -n "$((n+1))=" "$p")" ]; then printf "%s\n" "{trunc_m}" >&2; fi"#,
                trunc_m = READ_FILE_TRUNC_MARKER,
            ),
        };

        let read_cmd = format!(
            r#"sh -c 'set -eu; p=$1; max=$2; n=$3; if [ ! -e "$p" ]; then printf "%s\n" "{err}not_found" >&2; exit 1; fi; if [ ! -f "$p" ]; then printf "%s\n" "{err}not_regular_file" >&2; exit 1; fi; size=$(stat -c %s "$p" 2>/dev/null || stat -f %z "$p" 2>/dev/null || printf 0); printf "%s%s\n" "{size_m}" "$size" >&2{mode_suffix}' sh '{escaped_path}' '{max_read_bytes}' '{n}'"#,
            err = READ_FILE_ERROR_MARKER,
            size_m = READ_FILE_SIZE_MARKER,
            mode_suffix = mode_suffix,
            escaped_path = escaped_path,
            max_read_bytes = max_read_bytes,
            n = n,
        );

        let mut empty_stdin = tokio::io::empty();
        let exec_result = self
            .connection
            .exec_raw_streaming(
                &read_cmd,
                Some(&mut empty_stdin),
                Some(&mut capture_file),
                timeout,
            )
            .await;

        if let Err(e) = capture_file.flush().await {
            let _ = tokio::fs::remove_file(&capture_path).await;
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "Error: failed to flush local read capture file: {e}"
            ))]));
        }
        if let Err(e) = capture_file.sync_all().await {
            let _ = tokio::fs::remove_file(&capture_path).await;
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "Error: failed to sync local read capture file: {e}"
            ))]));
        }
        drop(capture_file);

        let exec_out = match exec_result {
            Ok(out) => out,
            Err(e) => {
                let _ = tokio::fs::remove_file(&capture_path).await;
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error reading file: {e}"
                ))]));
            }
        };

        if let Some(marker) = parse_read_file_error_marker(&exec_out.stderr) {
            let msg = match marker {
                "not_found" => "Error: remote_path does not exist".to_string(),
                "not_regular_file" => "Error: remote_path is not a regular file".to_string(),
                "too_large" => read_file_too_large_error(max_read_bytes),
                _ => "Error: read-file failed on remote host".to_string(),
            };
            let _ = tokio::fs::remove_file(&capture_path).await;
            return Ok(CallToolResult::error(vec![Content::text(msg)]));
        }

        match exec_out.exit_code {
            Some(0) => {}
            Some(code) => {
                let _ = tokio::fs::remove_file(&capture_path).await;
                return Ok(CallToolResult::error(vec![Content::text(
                    build_read_file_remote_failure(Some(code), &exec_out.stderr),
                )]));
            }
            None => {
                if !exec_out.stderr.trim().is_empty() {
                    let _ = tokio::fs::remove_file(&capture_path).await;
                    return Ok(CallToolResult::error(vec![Content::text(
                        build_read_file_remote_failure(None, &exec_out.stderr),
                    )]));
                }
            }
        }

        let metadata = match tokio::fs::metadata(&capture_path).await {
            Ok(metadata) => metadata,
            Err(e) => {
                let _ = tokio::fs::remove_file(&capture_path).await;
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error: failed to inspect local capture file: {e}"
                ))]));
            }
        };

        if metadata.len() > max_read_bytes as u64 {
            let _ = tokio::fs::remove_file(&capture_path).await;
            return Ok(CallToolResult::error(vec![Content::text(
                read_file_too_large_error(max_read_bytes),
            )]));
        }

        let mut capture_reader = match tokio::fs::File::open(&capture_path).await {
            Ok(file) => file,
            Err(e) => {
                let _ = tokio::fs::remove_file(&capture_path).await;
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error: failed to open local capture file: {e}"
                ))]));
            }
        };

        let mut bytes = Vec::with_capacity((metadata.len() as usize).min(max_read_bytes));
        let mut chunk = vec![0u8; 8192];
        loop {
            let read = match capture_reader.read(&mut chunk).await {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => {
                    let _ = tokio::fs::remove_file(&capture_path).await;
                    return Ok(CallToolResult::error(vec![Content::text(format!(
                        "Error: failed to read local capture file: {e}"
                    ))]));
                }
            };

            if bytes.len().saturating_add(read) > max_read_bytes {
                let _ = tokio::fs::remove_file(&capture_path).await;
                return Ok(CallToolResult::error(vec![Content::text(
                    read_file_too_large_error(max_read_bytes),
                )]));
            }

            bytes.extend_from_slice(&chunk[..read]);
        }

        let content = match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(e) => {
                let _ = tokio::fs::remove_file(&capture_path).await;
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error: file is not valid UTF-8 text ({})",
                    e.utf8_error()
                ))]));
            }
        };
        let _ = tokio::fs::remove_file(&capture_path).await;

        // Content is already windowed by the remote producer (head/tail -n N | head -c max+1).
        // Truncation and total-file-size come from stderr markers the remote command emitted.
        let returned_lines = read_file_line_count(&content);
        let truncated = if matches!(mode, ReadFileMode::Full) {
            false
        } else {
            read_file_stderr_indicates_truncated(&exec_out.stderr)
        };
        let approx_tokens_returned = estimate_tokens_from_bytes(content.len());
        let total_bytes =
            parse_read_file_size_marker(&exec_out.stderr).unwrap_or(metadata.len() as usize);
        let approx_tokens_total_estimate = estimate_tokens_from_bytes(total_bytes);
        let hint = build_read_file_hint(
            mode,
            line_limit.unwrap_or(READ_FILE_DEFAULT_PREVIEW_LINES),
            truncated,
        );
        let mut result = serde_json::json!({
            "path": remote_path,
            "mode": mode.as_str(),
            "content": content,
            "returned_lines": returned_lines,
            "truncated": truncated,
            "approx_tokens_returned": approx_tokens_returned,
            "approx_tokens_total_estimate": approx_tokens_total_estimate,
        });
        if let Some(hint) = hint {
            result["hint"] = serde_json::Value::String(hint);
        }

        Ok(CallToolResult::success(vec![Content::text(
            result.to_string(),
        )]))
    }
}
