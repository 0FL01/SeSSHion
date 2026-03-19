use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use std::time::Duration;
use tracing::debug;

use crate::server::SshMcpServer;
use crate::server::handlers::file_edit_common::FileEditFaultInjection;
use crate::server::validation::common::validate_read_file_path;
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

        match read_ticket {
            Some(ref ticket) => {
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

        self.execute_file_write_transaction(
            remote_path.as_str(),
            new_content.as_str(),
            user_expected_sha256,
            timeout,
            fault_injection,
            write_file_too_large_error(
                crate::server::validation::file_edit::FILE_EDIT_HARD_MAX_BYTES,
            ),
            "write-file",
        )
        .await
    }
}
