//! Internal helpers exposed for integration testing.

use std::time::Duration;

use rmcp::{ErrorData as McpError, model::CallToolResult};

use crate::server::handlers::file_edit_common::{FileEditFaultInjection, FileEditPrivilege};
use crate::server::{ReadFileMode, SshMcpServer};
use crate::tools::{ApplyPatchParams, CheckProcessParams, ReadFileParams};

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
        self.execute_check_process(CheckProcessParams {
            job_id: job_id.to_string(),
            tail_lines,
        })
        .await
    }

    #[doc(hidden)]
    pub async fn test_read_file(
        &self,
        remote_path: &str,
        timeout_ms: Option<u64>,
    ) -> std::result::Result<CallToolResult, McpError> {
        self.test_read_file_with_options(remote_path, ReadFileMode::Preview, None, timeout_ms)
            .await
    }

    #[doc(hidden)]
    pub async fn test_read_file_with_options(
        &self,
        remote_path: &str,
        mode: ReadFileMode,
        lines: Option<usize>,
        timeout_ms: Option<u64>,
    ) -> std::result::Result<CallToolResult, McpError> {
        self.execute_read_file(ReadFileParams {
            remote_path: remote_path.to_string(),
            mode,
            lines,
            timeout_ms,
        })
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

        if let Err(e) = self.connection.ensure_connected().await {
            return crate::transfer::TransferResponse::error(
                params,
                self.transfer.local_root(),
                &e.to_string(),
            );
        }

        use crate::transfer::{TransferRunContext, TransferSshOptions};
        self.transfer
            .run(
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
            )
            .await
    }
}
