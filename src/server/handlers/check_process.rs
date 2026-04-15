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

        match self
            .connection
            .check_process(
                &params.job_id,
                params.tail_lines,
                self.job_registry.as_ref(),
                &self.spooler,
            )
            .await
        {
            Ok(status) => {
                let result = serde_json::json!({
                    "state": status.state,
                    "running": status.running,
                    "exit_code": status.exit_code,
                    "state_reason": status.state_reason,
                    "elapsed_time": status.elapsed_time,
                    "command": status.command,
                    "log_path": status.log_path,
                    "log_exists": status.log_exists,
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
