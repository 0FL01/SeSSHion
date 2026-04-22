//! Test helper methods for SshMcpServer
//!
//! This module contains internal methods exposed for integration testing.
//! All methods are marked `#[doc(hidden)]` to indicate they are not part of
//! the public API.

use std::time::Duration;

use rmcp::{ErrorData as McpError, model::CallToolResult};

use crate::server::handlers::file_edit_common::FileEditFaultInjection;
use crate::server::{ReadFileMode, SshMcpServer};
use crate::tools::{CheckProcessParams, ReadFileParams, ReplaceInFileParams, WriteFileParams};

impl SshMcpServer {
    /// Internal method exposed for testing - executes a command directly
    #[doc(hidden)]
    pub async fn test_execute_command(
        &self,
        command: &str,
    ) -> std::result::Result<CallToolResult, McpError> {
        self.execute_command(command).await
    }

    /// Internal method exposed for testing - executes a command with a timeout override
    #[doc(hidden)]
    pub async fn test_execute_command_with_timeout_ms(
        &self,
        command: &str,
        timeout_ms: u64,
    ) -> std::result::Result<CallToolResult, McpError> {
        self.execute_command_with_timeout(command, Duration::from_millis(timeout_ms))
            .await
    }

    /// Internal method exposed for testing - executes a sudo command directly
    #[doc(hidden)]
    pub async fn test_execute_sudo_command(
        &self,
        command: &str,
    ) -> std::result::Result<CallToolResult, McpError> {
        self.execute_sudo_command(command).await
    }

    /// Internal method exposed for testing - executes a sudo command with a timeout override
    #[doc(hidden)]
    pub async fn test_execute_sudo_command_with_timeout_ms(
        &self,
        command: &str,
        timeout_ms: u64,
    ) -> std::result::Result<CallToolResult, McpError> {
        self.execute_sudo_command_with_timeout(command, Duration::from_millis(timeout_ms))
            .await
    }

    /// Internal method exposed for testing - checks a process status by PID
    #[doc(hidden)]
    pub async fn test_check_process(
        &self,
        job_id: &str,
        tail_lines: usize,
    ) -> std::result::Result<CallToolResult, McpError> {
        let params = CheckProcessParams {
            job_id: job_id.to_string(),
            tail_lines,
        };
        self.execute_check_process(params).await
    }

    /// Internal method exposed for testing - reads a remote UTF-8 file
    #[doc(hidden)]
    pub async fn test_read_file(
        &self,
        remote_path: &str,
        timeout_ms: Option<u64>,
    ) -> std::result::Result<CallToolResult, McpError> {
        self.test_read_file_with_options(remote_path, ReadFileMode::Preview, None, timeout_ms)
            .await
    }

    /// Internal method exposed for testing - reads a remote UTF-8 file with mode controls
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

    /// Internal method exposed for testing - writes a remote UTF-8 file atomically
    #[doc(hidden)]
    pub async fn test_write_file(
        &self,
        remote_path: &str,
        new_content: &str,
        expected_sha256: Option<&str>,
        read_ticket: Option<&str>,
        timeout_ms: Option<u64>,
    ) -> std::result::Result<CallToolResult, McpError> {
        self.execute_write_file(
            WriteFileParams {
                remote_path: remote_path.to_string(),
                new_content: new_content.to_string(),
                expected_sha256: expected_sha256.map(str::to_string),
                read_ticket: read_ticket.map(str::to_string),
                dry_run: None,
                timeout_ms,
            },
            FileEditFaultInjection::None,
        )
        .await
    }

    /// Internal method exposed for testing - runs write-file with raw params
    #[doc(hidden)]
    pub async fn test_write_file_with_params(
        &self,
        params: WriteFileParams,
    ) -> std::result::Result<CallToolResult, McpError> {
        self.execute_write_file(params, FileEditFaultInjection::None)
            .await
    }

    /// Internal method exposed for testing - applies a text replacement edit
    #[doc(hidden)]
    pub async fn test_replace_in_file(
        &self,
        remote_path: &str,
        old_text: &str,
        new_text: &str,
        replace_all: bool,
        expected_sha256: Option<&str>,
        timeout_ms: Option<u64>,
    ) -> std::result::Result<CallToolResult, McpError> {
        self.execute_replace_in_file(
            ReplaceInFileParams {
                remote_path: remote_path.to_string(),
                old_text: old_text.to_string(),
                new_text: new_text.to_string(),
                scope_text: None,
                replace_all: Some(replace_all),
                match_index: None,
                dry_run: None,
                expected_sha256: expected_sha256.map(str::to_string),
                timeout_ms,
            },
            FileEditFaultInjection::None,
        )
        .await
    }

    /// Internal method exposed for testing - runs replace-in-file with raw params
    #[doc(hidden)]
    pub async fn test_replace_in_file_with_params(
        &self,
        params: ReplaceInFileParams,
    ) -> std::result::Result<CallToolResult, McpError> {
        self.execute_replace_in_file(params, FileEditFaultInjection::None)
            .await
    }

    /// Internal method exposed for testing - deletes destination after partial read and before write
    #[doc(hidden)]
    pub async fn test_replace_in_file_delete_before_write(
        &self,
        remote_path: &str,
        old_text: &str,
        new_text: &str,
        replace_all: bool,
        expected_sha256: Option<&str>,
        timeout_ms: Option<u64>,
    ) -> std::result::Result<CallToolResult, McpError> {
        self.execute_replace_in_file(
            ReplaceInFileParams {
                remote_path: remote_path.to_string(),
                old_text: old_text.to_string(),
                new_text: new_text.to_string(),
                scope_text: None,
                replace_all: Some(replace_all),
                match_index: None,
                dry_run: None,
                expected_sha256: expected_sha256.map(str::to_string),
                timeout_ms,
            },
            FileEditFaultInjection::PartialDeleteBeforeWrite,
        )
        .await
    }

    /// Internal method exposed for testing - mutates destination after partial read and before write
    #[doc(hidden)]
    pub async fn test_replace_in_file_mutate_before_write(
        &self,
        remote_path: &str,
        old_text: &str,
        new_text: &str,
        replace_all: bool,
        expected_sha256: Option<&str>,
        timeout_ms: Option<u64>,
    ) -> std::result::Result<CallToolResult, McpError> {
        self.execute_replace_in_file(
            ReplaceInFileParams {
                remote_path: remote_path.to_string(),
                old_text: old_text.to_string(),
                new_text: new_text.to_string(),
                scope_text: None,
                replace_all: Some(replace_all),
                match_index: None,
                dry_run: None,
                expected_sha256: expected_sha256.map(str::to_string),
                timeout_ms,
            },
            FileEditFaultInjection::PartialMutateBeforeWrite,
        )
        .await
    }

    /// Internal method exposed for testing - injects a failure after stage write and before rename
    #[doc(hidden)]
    pub async fn test_write_file_fail_before_finalize(
        &self,
        remote_path: &str,
        new_content: &str,
        expected_sha256: Option<&str>,
        read_ticket: Option<&str>,
        timeout_ms: Option<u64>,
    ) -> std::result::Result<CallToolResult, McpError> {
        self.execute_write_file(
            WriteFileParams {
                remote_path: remote_path.to_string(),
                new_content: new_content.to_string(),
                expected_sha256: expected_sha256.map(str::to_string),
                read_ticket: read_ticket.map(str::to_string),
                dry_run: None,
                timeout_ms,
            },
            FileEditFaultInjection::FailBeforeFinalize,
        )
        .await
    }

    /// Internal method exposed for testing - injects a SHA-256 preflight failure before mutation
    #[doc(hidden)]
    pub async fn test_write_file_sha256_unavailable(
        &self,
        remote_path: &str,
        new_content: &str,
        expected_sha256: Option<&str>,
        read_ticket: Option<&str>,
        timeout_ms: Option<u64>,
    ) -> std::result::Result<CallToolResult, McpError> {
        self.execute_write_file(
            WriteFileParams {
                remote_path: remote_path.to_string(),
                new_content: new_content.to_string(),
                expected_sha256: expected_sha256.map(str::to_string),
                read_ticket: read_ticket.map(str::to_string),
                dry_run: None,
                timeout_ms,
            },
            FileEditFaultInjection::Sha256Unavailable,
        )
        .await
    }

    /// Internal method exposed for testing - starts an exec command in background=true mode
    #[doc(hidden)]
    pub async fn test_execute_background_command(
        &self,
        command: &str,
    ) -> std::result::Result<CallToolResult, McpError> {
        self.execute_background_command(command, None).await
    }

    /// Internal method exposed for testing - starts a sudo-exec command in background=true mode
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
                &format!("SSH connection error: {e}"),
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
                    },
                },
            )
            .await
    }
}
