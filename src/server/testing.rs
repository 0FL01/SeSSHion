//! Internal helpers exposed for integration testing.

use std::time::Duration;

use rmcp::{ErrorData as McpError, model::CallToolResult};

use crate::server::SshMcpServer;
use crate::server::handlers::file_edit_common::{FileEditFaultInjection, FileEditPrivilege};
use crate::tools::{ApplyPatchParams, CheckProcessParams};

impl SshMcpServer {
    #[doc(hidden)]
    pub async fn test_execute_command(
        &self,
        command: &str,
    ) -> std::result::Result<CallToolResult, McpError> {
        self.execute_command(command).await
    }

    #[doc(hidden)]
    pub async fn test_execute_command_with_timeout_ms(
        &self,
        command: &str,
        timeout_ms: u64,
    ) -> std::result::Result<CallToolResult, McpError> {
        self.execute_command_with_timeout(command, Duration::from_millis(timeout_ms))
            .await
    }

    #[doc(hidden)]
    pub async fn test_execute_sudo_command(
        &self,
        command: &str,
    ) -> std::result::Result<CallToolResult, McpError> {
        self.execute_sudo_command(command).await
    }

    #[doc(hidden)]
    pub async fn test_execute_sudo_command_with_timeout_ms(
        &self,
        command: &str,
        timeout_ms: u64,
    ) -> std::result::Result<CallToolResult, McpError> {
        self.execute_sudo_command_with_timeout(command, Duration::from_millis(timeout_ms))
            .await
    }

    #[doc(hidden)]
    pub async fn test_check_process(
        &self,
        job_id: &str,
        tail_lines: usize,
    ) -> std::result::Result<CallToolResult, McpError> {
        self.execute_check_process(
            CheckProcessParams {
                job_id: job_id.to_string(),
                tail_lines,
            },
            0,
            std::future::pending(),
        )
        .await
    }

    #[doc(hidden)]
    pub async fn test_check_process_with_wait(
        &self,
        job_id: &str,
        tail_lines: usize,
        wait_for: u64,
    ) -> std::result::Result<CallToolResult, McpError> {
        self.execute_check_process(
            CheckProcessParams {
                job_id: job_id.to_string(),
                tail_lines,
            },
            wait_for,
            std::future::pending(),
        )
        .await
    }

    #[doc(hidden)]
    pub async fn test_check_process_with_wait_cancellation(
        &self,
        job_id: &str,
        tail_lines: usize,
        wait_for: u64,
        cancelled: tokio::sync::oneshot::Receiver<()>,
    ) -> std::result::Result<CallToolResult, McpError> {
        self.execute_check_process(
            CheckProcessParams {
                job_id: job_id.to_string(),
                tail_lines,
            },
            wait_for,
            async move {
                let _ = cancelled.await;
            },
        )
        .await
    }

    #[doc(hidden)]
    pub async fn test_apply_patch(
        &self,
        patch: &str,
    ) -> std::result::Result<CallToolResult, McpError> {
        self.test_apply_patch_with_fault(
            patch,
            FileEditFaultInjection::None,
            FileEditPrivilege::User,
        )
        .await
    }

    #[doc(hidden)]
    pub async fn test_sudo_apply_patch(
        &self,
        patch: &str,
    ) -> std::result::Result<CallToolResult, McpError> {
        self.test_apply_patch_with_fault(
            patch,
            FileEditFaultInjection::None,
            FileEditPrivilege::Sudo,
        )
        .await
    }

    #[doc(hidden)]
    pub async fn test_apply_patch_mutate_before_commit(
        &self,
        patch: &str,
    ) -> std::result::Result<CallToolResult, McpError> {
        self.test_apply_patch_with_fault(
            patch,
            FileEditFaultInjection::PartialMutateBeforeWrite,
            FileEditPrivilege::User,
        )
        .await
    }

    #[doc(hidden)]
    pub async fn test_sudo_apply_patch_mutate_before_commit(
        &self,
        patch: &str,
    ) -> std::result::Result<CallToolResult, McpError> {
        self.test_apply_patch_with_fault(
            patch,
            FileEditFaultInjection::PartialMutateBeforeWrite,
            FileEditPrivilege::Sudo,
        )
        .await
    }

    async fn test_apply_patch_with_fault(
        &self,
        patch: &str,
        fault: FileEditFaultInjection,
        privilege: FileEditPrivilege,
    ) -> std::result::Result<CallToolResult, McpError> {
        self.execute_apply_patch(
            ApplyPatchParams {
                patch: patch.to_owned(),
            },
            fault,
            privilege,
        )
        .await
    }

    #[doc(hidden)]
    pub async fn test_execute_background_command(
        &self,
        command: &str,
    ) -> std::result::Result<CallToolResult, McpError> {
        self.execute_background_command(command, None).await
    }

    #[doc(hidden)]
    pub async fn test_execute_background_sudo_command(
        &self,
        command: &str,
    ) -> std::result::Result<CallToolResult, McpError> {
        self.execute_background_sudo_command(command, None).await
    }

    #[doc(hidden)]
    pub async fn test_transfer(
        &self,
        params: crate::transfer::TransferParams,
    ) -> crate::transfer::TransferResponse {
        let timeout = params
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(self.timeout);
        let key_path = self.config.key.clone();

        use crate::transfer::{TransferRunContext, TransferSshOptions};
        self.transfer
            .run_controlled(
                &self.connection,
                params,
                TransferRunContext {
                    timeout,
                    ssh: TransferSshOptions {
                        host: self.config.host.clone(),
                        port: self.config.port,
                        user: self.config.user.clone(),
                        key_path,
                        host_key_checking: self.config.strict_host_key_checking,
                        known_hosts: self.config.known_hosts.clone(),
                    },
                },
                tokio_util::sync::CancellationToken::new(),
                None,
            )
            .await
    }

    #[doc(hidden)]
    pub async fn test_background_transfer(
        &self,
        mut params: crate::transfer::TransferParams,
    ) -> std::result::Result<CallToolResult, McpError> {
        params.background = true;
        self.execute_background_transfer(params).await
    }
}
