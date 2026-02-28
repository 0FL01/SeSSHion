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
use crate::ticket::DEFAULT_TICKET_TTL_SECS;
use crate::tools::ReadFileParams;

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
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "SSH connection error: {}",
                e
            ))]));
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
        let read_cmd = format!(
            r#"sh -c 'set -eu; p=$1; max_bytes=$2; if [ ! -e "$p" ]; then printf "%s\n" "{}not_found" >&2; exit 1; fi; if [ ! -f "$p" ]; then printf "%s\n" "{}not_regular_file" >&2; exit 1; fi; head -c "$((max_bytes + 1))" < "$p"' sh '{escaped_path}' '{max_read_bytes}'"#,
            READ_FILE_ERROR_MARKER, READ_FILE_ERROR_MARKER,
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
        let content_sha256 = {
            use sha2::{Digest, Sha256};
            let hash = Sha256::digest(content.as_bytes());
            hash.iter()
                .fold(String::with_capacity(SHA256_HEX_LEN), |mut acc, b| {
                    use std::fmt::Write as _;
                    let _ = write!(acc, "{b:02x}");
                    acc
                })
        };
        let _ = tokio::fs::remove_file(&capture_path).await;

        let content_window = apply_read_file_window(&content, mode, line_limit);
        let returned_content = content_window.content;
        let approx_tokens_returned = estimate_tokens_from_bytes(returned_content.len());
        let approx_tokens_total_estimate = estimate_tokens_from_bytes(metadata.len() as usize);
        let mut result = serde_json::json!({
            "path": remote_path,
            "mode": mode.as_str(),
            "content": returned_content,
            "returned_lines": content_window.returned_lines,
            "truncated": content_window.truncated,
            "approx_tokens_returned": approx_tokens_returned,
            "approx_tokens_total_estimate": approx_tokens_total_estimate,
        });
        if let Some(hint) = content_window.hint {
            result["hint"] = serde_json::Value::String(hint);
        }
        result["sha256"] = serde_json::Value::String(content_sha256);
        result["read_ticket"] = serde_json::Value::String(
            self.ticket_signer
                .issue(&remote_path, DEFAULT_TICKET_TTL_SECS),
        );

        Ok(CallToolResult::success(vec![Content::text(
            result.to_string(),
        )]))
    }
}
