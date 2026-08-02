use std::future::Future;
use std::time::Duration;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, ContentBlock};
use tracing::{debug, error};

use crate::server::SshMcpServer;
use crate::ssh::escape_for_shell;
use crate::tools::CheckProcessParams;
use crate::transfer::TransferProgressTarget;

impl SshMcpServer {
    /// Execute check_process tool
    pub(in crate::server) async fn execute_check_process(
        &self,
        params: CheckProcessParams,
        wait_for: u64,
        cancelled: impl Future<Output = ()>,
    ) -> std::result::Result<CallToolResult, McpError> {
        debug!(job_id = ?params.job_id, wait_for, "check_process tool called");

        if self.transfer_job_registry.contains(&params.job_id) {
            return self
                .execute_check_transfer(&params.job_id, wait_for, cancelled)
                .await;
        }

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

    async fn execute_check_transfer(
        &self,
        job_id: &str,
        wait_for: u64,
        cancelled: impl Future<Output = ()>,
    ) -> std::result::Result<CallToolResult, McpError> {
        let Some(mut snapshot) = self.transfer_job_registry.snapshot(job_id) else {
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "Error checking process: job not found: {job_id}"
            ))]));
        };

        if wait_for > 0 && snapshot.running {
            let delay = tokio::time::sleep(Duration::from_secs(wait_for));
            tokio::pin!(cancelled);
            tokio::pin!(delay);
            tokio::select! {
                _ = &mut cancelled => {}
                _ = &mut delay => {
                    if let Some(updated) = self.transfer_job_registry.snapshot(job_id) {
                        snapshot = updated;
                    }
                }
            }
        }

        let bytes_done = if let Some(result) = &snapshot.result {
            result.counts.as_ref().map(|counts| counts.bytes)
        } else {
            match snapshot.progress_target.as_ref() {
                Some(TransferProgressTarget::Local(path)) => tokio::fs::metadata(path)
                    .await
                    .ok()
                    .map(|metadata| metadata.len()),
                Some(TransferProgressTarget::Remote(path)) => {
                    let escaped = escape_for_shell(path);
                    let command = format!(
                        r#"sh -c 'if [ -f "$1" ]; then wc -c < "$1"; else printf 0; fi' sh '{escaped}'"#
                    );
                    self.connection
                        .exec_command(&command, Duration::from_secs(5))
                        .await
                        .ok()
                        .and_then(|output| output.stdout.trim().parse::<u64>().ok())
                }
                None => None,
            }
        };

        let result = serde_json::json!({
            "job_id": snapshot.job_id,
            "job_type": "transfer",
            "state": snapshot.state,
            "running": snapshot.running,
            "phase": snapshot.phase.as_str(),
            "elapsed_ms": snapshot.elapsed_ms,
            "current_transport": snapshot.transport,
            "bytes_done": bytes_done,
            "bytes_total": snapshot.total_bytes,
            "result": snapshot.result,
        });
        Ok(CallToolResult::success(vec![ContentBlock::text(
            result.to_string(),
        )]))
    }
}
