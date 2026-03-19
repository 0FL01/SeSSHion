use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use std::time::Duration;
use tracing::debug;

use crate::server::SshMcpServer;
use crate::server::handlers::file_edit_common::FileEditFaultInjection;
use crate::server::validation::common::extract_text_from_call_tool_result;
use crate::server::validation::common::validate_read_file_path;
use crate::server::validation::file_edit::replace_in_file_too_large_error;
use crate::server::validation::read_file::normalize_sha256_hex;
use crate::tools::{ReadFileMode, ReadFileParams, ReplaceInFileParams};

impl SshMcpServer {
    pub(in crate::server) async fn execute_replace_in_file(
        &self,
        params: ReplaceInFileParams,
        fault_injection: FileEditFaultInjection,
    ) -> std::result::Result<CallToolResult, McpError> {
        debug!(remote_path = ?params.remote_path, "replace-in-file tool called");

        let ReplaceInFileParams {
            remote_path,
            old_text,
            new_text,
            replace_all,
            expected_sha256,
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

        let replace_all = replace_all.unwrap_or(false);
        if old_text.is_empty() {
            return Err(McpError::invalid_params(
                "old_text must not be empty in replace-in-file",
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
        let read_value: serde_json::Value =
            serde_json::from_str(read_text.trim()).map_err(|e| {
                McpError::internal_error(
                    format!(
                        "failed to parse read-file response while preparing replace-in-file: {e}"
                    ),
                    None,
                )
            })?;

        let current_content = read_value
            .get("content")
            .and_then(|value| value.as_str())
            .ok_or_else(|| {
                McpError::internal_error(
                    "read-file response missing content while preparing replace-in-file"
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

        self.execute_file_write_transaction(
            remote_path.as_str(),
            updated_content.as_str(),
            user_expected_sha256.or(partial_baseline_sha256),
            timeout,
            fault_injection,
            replace_in_file_too_large_error(
                crate::server::validation::file_edit::FILE_EDIT_HARD_MAX_BYTES,
            ),
            "replace-in-file",
        )
        .await
    }
}
