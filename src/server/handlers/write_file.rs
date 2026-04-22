use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use std::time::Duration;
use tracing::debug;

use crate::server::SshMcpServer;
use crate::server::handlers::file_edit_common::{
    FileEditFaultInjection, FileWriteTransactionRequest, RemoteTextFileState,
    build_file_edit_conflict_result, build_unified_diff, local_text_sha256_hex,
};
use crate::server::validation::common::validate_read_file_path;
use crate::server::validation::file_edit::FILE_EDIT_MISSING_SHA256;
use crate::server::validation::file_edit::write_file_too_large_error;
use crate::server::validation::read_file::normalize_sha256_hex;
use crate::tools::WriteFileParams;

impl SshMcpServer {
    pub(in crate::server) async fn execute_write_file(
        &self,
        params: WriteFileParams,
        fault_injection: FileEditFaultInjection,
    ) -> std::result::Result<CallToolResult, McpError> {
        debug!(remote_path = ?params.remote_path, "write-file tool called");

        let WriteFileParams {
            remote_path,
            new_content,
            expected_sha256,
            read_ticket,
            dry_run,
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

        let dry_run = dry_run.unwrap_or(false);

        let ticket_bound_sha256 = match read_ticket {
            Some(ref ticket) => {
                let claims = self
                    .ticket_signer
                    .verify(ticket, &remote_path)
                    .map_err(|e| {
                        McpError::invalid_params(
                            format!("read_ticket verification failed: {e}"),
                            None,
                        )
                    })?;
                claims.content_sha256().map(str::to_string)
            }
            None => None,
        };

        let effective_expected_sha256 = match (
            user_expected_sha256.as_deref(),
            ticket_bound_sha256.as_deref(),
        ) {
            (Some(user), _) => Some(user.to_string()),
            (None, Some(ticket)) => Some(ticket.to_string()),
            (None, None) => None,
        };

        match read_ticket {
            Some(_) => {}
            None => {
                if self
                    .check_remote_file_nonempty(&remote_path, timeout)
                    .await?
                {
                    return Err(McpError::invalid_params(
                        "Error: existing non-empty file must be read before editing. Call read-file first, then pass the returned read_ticket to write-file.",
                        None,
                    ));
                }
            }
        }

        if dry_run {
            let current_state = match self
                .load_remote_text_file_state(&remote_path, timeout)
                .await
            {
                Ok(state) => state,
                Err(result) => return Ok(result),
            };

            let (current_content, current_sha256) = match current_state {
                RemoteTextFileState::Missing => {
                    (String::new(), FILE_EDIT_MISSING_SHA256.to_string())
                }
                RemoteTextFileState::Existing { content, sha256 } => (content, sha256),
            };

            if let Some(expected) = effective_expected_sha256.as_deref()
                && expected != current_sha256
            {
                return Ok(build_file_edit_conflict_result(
                    &remote_path,
                    expected,
                    &current_sha256,
                ));
            }

            let preview = serde_json::json!({
                "path": remote_path,
                "dry_run": true,
                "changed": current_content != new_content,
                "previous_sha256": current_sha256,
                "predicted_new_sha256": local_text_sha256_hex(&new_content),
                "bytes_written": new_content.len(),
                "diff": build_unified_diff(&remote_path, &current_content, &new_content),
            });
            return Ok(CallToolResult::success(vec![Content::text(
                preview.to_string(),
            )]));
        }

        self.execute_file_write_transaction(FileWriteTransactionRequest {
            remote_path: remote_path.as_str(),
            new_content: new_content.as_str(),
            expected_sha256: effective_expected_sha256,
            timeout,
            fault_injection,
            too_large_error: write_file_too_large_error(
                crate::server::validation::file_edit::FILE_EDIT_HARD_MAX_BYTES,
            ),
            operation_name: "write-file",
        })
        .await
    }
}
