use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use tracing::{debug, error};

use crate::server::SshMcpServer;
use crate::tools::CheckProcessParams;

impl SshMcpServer {
    /// Execute check-process tool
    pub(in crate::server) async fn execute_check_process(
        &self,
        params: CheckProcessParams,
    ) -> std::result::Result<CallToolResult, McpError> {
        debug!(job_id = ?params.job_id, "check-process tool called");

        // Ensure connection is established
        if let Err(e) = self.connection.ensure_connected().await {
            error!(error = ?e, "Failed to ensure SSH connection");
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "SSH connection error: {}",
                e
            ))]));
        }

        match self
            .connection
            .check_process(
                &params.job_id,
                params.tail_lines,
                self.job_registry.as_ref(),
            )
            .await
        {
            Ok(status) => {
                let result = serde_json::json!({
                    "running": status.running,
                    "exit_code": status.exit_code,
                    "elapsed_time": status.elapsed_time,
                    "command": status.command,
                    "log_tail": status.log_tail,
                });
                Ok(CallToolResult::success(vec![Content::text(
                    result.to_string(),
                )]))
            }
            Err(e) => {
                error!(job_id = ?params.job_id, error = ?e, "check-process failed");
                Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error checking process: {}",
                    e
                ))]))
            }
        }
    }
}
