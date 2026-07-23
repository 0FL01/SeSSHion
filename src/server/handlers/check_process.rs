use std::future::Future;
use std::time::Duration;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, ContentBlock};
use tracing::{debug, error};

use crate::server::SshMcpServer;
use crate::tools::CheckProcessParams;

impl SshMcpServer {
    /// Execute check_process tool
    pub(in crate::server) async fn execute_check_process(
        &self,
        params: CheckProcessParams,
        wait_for: u64,
        cancelled: impl Future<Output = ()>,
    ) -> std::result::Result<CallToolResult, McpError> {
        debug!(job_id = ?params.job_id, wait_for, "check_process tool called");

        let status_result = async {
            let initial = self
                .connection
                .check_process(
                    &params.job_id,
                    params.tail_lines,
                    self.job_registry.as_ref(),
                    &self.spooler,
                )
                .await?;

            if wait_for == 0 || !initial.running {
                return Ok(initial);
            }

            let delay = tokio::time::sleep(Duration::from_secs(wait_for));
            tokio::pin!(cancelled);
            tokio::pin!(delay);

            tokio::select! {
                biased;
                _ = &mut cancelled => {
                    debug!(
                        job_id = ?params.job_id,
                        "check_process wait cancelled; remote job was not stopped"
                    );
                    Ok(initial)
                }
                _ = &mut delay => {
                    self.connection.check_process(
                        &params.job_id,
                        params.tail_lines,
                        self.job_registry.as_ref(),
                        &self.spooler,
                    ).await
                }
            }
        }
        .await;

        match status_result {
            Ok(status) => {
                let result = serde_json::json!({
                    "pid": status.pid,
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
                Ok(CallToolResult::success(vec![ContentBlock::text(
                    result.to_string(),
                )]))
            }
            Err(e) => {
                error!(job_id = ?params.job_id, error = ?e, "check_process failed");
                Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "Error checking process: {}",
                    e
                ))]))
            }
        }
    }
}
