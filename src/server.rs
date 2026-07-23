//! MCP Server implementation
//!
//! This module provides the main MCP server that integrates SSH connection
//! management with the `shell` and `sudo_shell` tools.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use rmcp::{
    ErrorData as McpError,
    handler::server::ServerHandler,
    model::*,
    service::{RequestContext, RoleServer},
};
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use crate::background::job::NewRunningJob;
use crate::background::{JobRegistry, JobState, LocalLogSpooler, SharedJobState};
use crate::config::Config;
use crate::error::{Result, SshMcpError};
#[cfg(unix)]
use crate::platform::O_NOFOLLOW_FLAG;
use crate::server::handlers::file_edit_common::{FileEditFaultInjection, FileEditPrivilege};
#[cfg(test)]
use crate::server::validation::read_file::{
    READ_FILE_BYTES_PER_TOKEN, READ_FILE_DEFAULT_PREVIEW_LINES, READ_FILE_HARD_MAX_BYTES,
    READ_FILE_MAX_LINE_WINDOW,
};
#[cfg(test)]
use crate::server::validation::read_file::{
    estimate_tokens_from_bytes, resolve_read_file_line_limit, resolve_read_file_max_bytes,
};
#[cfg(test)]
use crate::server::validation::validate_background_log_path;
use crate::ssh::{
    CommandOutput, SshConfig, SshConnectionManager, sanitize_command, wrap_sudo_command,
};
use crate::tools::{ApplyPatchParams, ReadFileMode, ReadFileParams};
use crate::transfer::{TransferEngine, TransferParams, TransferRunContext, TransferSshOptions};

mod args;
mod exec;
mod handlers;
mod testing;
mod tools;
mod validation;

const BACKGROUND_START_TIMEOUT: Duration = Duration::from_secs(20);
const READ_FILE_ERROR_MARKER: &str = "__SSH_MCP_READ_FILE_ERR__";

const JOB_COMPLETED_RETENTION: Duration = Duration::from_secs(60 * 60);

static JOB_COUNTER: AtomicU64 = AtomicU64::new(0);

fn make_job_id() -> String {
    let counter = JOB_COUNTER.fetch_add(1, Ordering::Relaxed);
    let epoch_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{}-{}", epoch_ms, counter)
}

/// SSH MCP Server
///
/// The main server implementation that provides MCP tools for remote SSH
/// command execution.
#[derive(Clone)]
pub struct SshMcpServer {
    /// Server configuration
    config: Config,

    /// SSH connection manager
    connection: Arc<SshConnectionManager>,

    /// Command execution timeout
    timeout: Duration,

    /// Maximum command length
    max_chars: Option<usize>,

    spooler: Arc<LocalLogSpooler>,
    job_registry: Arc<JobRegistry>,

    transfer: TransferEngine,
}

impl SshMcpServer {
    /// Create a new SSH MCP Server
    ///
    /// This sets up the SSH connection manager based on the provided configuration.
    /// Connection is not established until a tool is actually used.
    pub async fn new(config: Config) -> Result<Self> {
        let local_root = std::env::current_dir()?;

        let spooler = Arc::new(LocalLogSpooler::new_default());
        spooler.ensure_dir().await.map_err(|e| {
            SshMcpError::Config(format!(
                "failed to initialize local log spool dir {}: {e}",
                spooler.base_dir().display()
            ))
        })?;
        let job_registry = Arc::new(JobRegistry::new(JOB_COMPLETED_RETENTION));

        // Build SSH configuration
        let mut ssh_config = SshConfig::new(&config.host, &config.user).with_port(config.port);

        // Add authentication
        if let Some(ref password) = config.password {
            ssh_config = ssh_config.with_password(password);
        }

        if let Some(ref key_path) = config.key {
            // Read the key file
            let key_content = tokio::fs::read_to_string(key_path)
                .await
                .map_err(SshMcpError::Io)?;
            ssh_config = ssh_config.with_private_key(&key_content);
        }

        // Add elevation passwords if provided
        if let Some(ref su_password) = config.su_password {
            ssh_config = ssh_config.with_su_password(su_password);
        }

        if let Some(ref sudo_password) = config.sudo_password {
            ssh_config = ssh_config.with_sudo_password(sudo_password);
        }

        // Add keepalive settings for human-like connection persistence
        ssh_config = ssh_config
            .with_keepalive_interval(config.keepalive_interval)
            .with_keepalive_max(config.keepalive_max);

        // Add reconnect and health probe settings
        ssh_config = ssh_config
            .with_reconnect_retries(config.reconnect_retries)
            .with_reconnect_backoff_ms(config.reconnect_backoff_ms)
            .with_health_probe_timeout_ms(config.health_probe_timeout_ms);

        // Add host key verification settings
        ssh_config = ssh_config
            .with_host_key_checking(config.strict_host_key_checking)
            .with_known_hosts(config.known_hosts.clone());

        // Add output token limit for OOM protection
        ssh_config = ssh_config.with_max_output_tokens(config.max_output_tokens);

        // Create connection manager
        let connection = Arc::new(SshConnectionManager::new(ssh_config).await);

        let timeout = Duration::from_millis(config.timeout_ms);
        let max_chars = config.max_chars;

        Ok(Self {
            config,
            connection,
            timeout,
            max_chars,
            spooler,
            job_registry,
            transfer: TransferEngine::new(local_root),
        })
    }

    fn connection_id(&self) -> String {
        format!(
            "{}@{}:{}",
            self.config.user, self.config.host, self.config.port
        )
    }

    fn default_local_log_path(
        &self,
        job_id: &str,
    ) -> std::result::Result<(PathBuf, String), String> {
        let path = self
            .spooler
            .log_path_for(job_id)
            .map_err(|e| format!("failed to generate local log path for job_id='{job_id}': {e}"))?;
        let path_str = path.to_string_lossy().to_string();
        Ok((path, path_str))
    }

    async fn ensure_local_log_file(&self, log_path: &Path) -> std::result::Result<(), SshMcpError> {
        self.spooler.ensure_dir().await.map_err(|e| {
            SshMcpError::Config(format!(
                "failed to ensure local log spool dir {}: {e}",
                self.spooler.base_dir().display()
            ))
        })?;

        if log_path.parent() != Some(self.spooler.base_dir()) {
            return Err(SshMcpError::InvalidParams(format!(
                "log_path must be directly under {}",
                self.spooler.base_dir().display()
            )));
        }

        match tokio::fs::symlink_metadata(log_path).await {
            Ok(meta) => {
                let ft = meta.file_type();
                if ft.is_symlink() {
                    return Err(SshMcpError::invalid_params(
                        "log_path is a symlink (refusing to follow it)",
                    ));
                }
                if !ft.is_file() {
                    return Err(SshMcpError::invalid_params(
                        "log_path exists but is not a regular file",
                    ));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(SshMcpError::Io(e)),
        }

        let mut opts = tokio::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);

        #[cfg(unix)]
        {
            opts.custom_flags(O_NOFOLLOW_FLAG);
        }

        let file = match opts.open(log_path).await {
            Ok(f) => f,
            Err(e) => {
                if let Ok(meta) = tokio::fs::symlink_metadata(log_path).await
                    && meta.file_type().is_symlink()
                {
                    return Err(SshMcpError::invalid_params(
                        "log_path is a symlink (refusing to follow it)",
                    ));
                }
                return Err(SshMcpError::Io(e));
            }
        };

        file.sync_all().await.map_err(SshMcpError::Io)
    }

    async fn register_running_job(
        &self,
        job_id: &str,
        pid: u32,
        log_path: PathBuf,
        command: &str,
    ) -> SharedJobState {
        let job = Arc::new(Mutex::new(JobState::new_running(NewRunningJob {
            job_id: job_id.to_string(),
            pid,
            log_path,
            command: command.to_string(),
            connection_id: self.connection_id(),
        })));

        self.job_registry
            .insert(job_id.to_string(), Arc::clone(&job))
            .await;

        let persisted = {
            let guard = job.lock().await;
            guard.clone()
        };
        if let Err(e) = self.spooler.persist_job_state(&persisted).await {
            warn!(job_id = ?job_id, error = ?e, "failed to persist running job state");
        }

        job
    }

    /// Get a reference to the SSH connection manager
    pub fn connection(&self) -> &Arc<SshConnectionManager> {
        &self.connection
    }

    /// Close the server and cleanup resources
    pub async fn shutdown(&self) {
        info!("Shutting down SSH MCP Server...");
        self.connection.close().await;
    }

    /// Execute a command (used by shell tool)
    async fn execute_command_with_timeout(
        &self,
        command: &str,
        timeout: Duration,
    ) -> std::result::Result<CallToolResult, McpError> {
        debug!(
            "shell tool called: cmd_len={}, background=false, sudo=false, timeout_ms={}",
            command.len(),
            timeout.as_millis()
        );

        // Sanitize the command
        let sanitized = match self.sanitize_or_tool_error(command) {
            Ok(cmd) => cmd,
            Err(result) => return Ok(result),
        };

        // Foreground execution is detachable-by-design:
        // - Start the command on a dedicated SSH channel
        // - Stream remote stdout/stderr into a local spool file
        // - If timeout elapses, return JSON with job_id/pid/log_path while the stream continues

        let requires_elevation = self.connection.get_su_password().is_some();
        if requires_elevation {
            if let Err(e) = self.connection.ensure_connected().await {
                error!(error = ?e, "Failed to ensure SSH connection");
                return Ok(CallToolResult::error(vec![ContentBlock::text(
                    e.to_string(),
                )]));
            }

            if let Err(e) = self.connection.ensure_elevated().await {
                debug!(error = ?e, "Elevation failed, will run as normal user");
            }
        }

        // Ensure connection is established for detached foreground execution path.
        if !requires_elevation && let Err(e) = self.connection.ensure_connected().await {
            error!(error = ?e, "Failed to ensure SSH connection");
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                e.to_string(),
            )]));
        }

        self.execute_detachable_foreground_impl(&sanitized, &sanitized, timeout)
            .await
    }

    async fn execute_command(
        &self,
        command: &str,
    ) -> std::result::Result<CallToolResult, McpError> {
        self.execute_command_with_timeout(command, self.timeout)
            .await
    }

    async fn execute_background_command(
        &self,
        command: &str,
        log_path: Option<&str>,
    ) -> std::result::Result<CallToolResult, McpError> {
        self.execute_background_impl(command, log_path, exec::BackgroundPrivilege::Normal)
            .await
    }

    /// Execute a command with sudo (used by sudo_shell tool)
    async fn execute_sudo_command_with_timeout(
        &self,
        command: &str,
        timeout: Duration,
    ) -> std::result::Result<CallToolResult, McpError> {
        debug!(
            "sudo_shell tool called: cmd_len={}, background=false, sudo=true, timeout_ms={}",
            command.len(),
            timeout.as_millis()
        );

        // Sanitize the command
        let sanitized = match self.sanitize_or_tool_error(command) {
            Ok(cmd) => cmd,
            Err(result) => return Ok(result),
        };

        // Wrap the command with sudo
        let sudo_password = self.connection.get_sudo_password();
        let wrapped_command = wrap_sudo_command(&sanitized, sudo_password);
        debug!(
            "Wrapped sudo command (password hidden): sudo -n sh -c '...' or printf '...' | sudo ..."
        );

        if let Err(e) = self.connection.ensure_connected().await {
            error!(error = ?e, "Failed to ensure SSH connection");
            return Ok(CallToolResult::error(vec![ContentBlock::text(
                e.to_string(),
            )]));
        }

        self.execute_detachable_foreground_impl(
            &wrapped_command,
            &format!("sudo {sanitized}"),
            timeout,
        )
        .await
    }

    async fn execute_sudo_command(
        &self,
        command: &str,
    ) -> std::result::Result<CallToolResult, McpError> {
        self.execute_sudo_command_with_timeout(command, self.timeout)
            .await
    }

    async fn execute_background_sudo_command(
        &self,
        command: &str,
        log_path: Option<&str>,
    ) -> std::result::Result<CallToolResult, McpError> {
        let sudo_password = self.connection.get_sudo_password();
        self.execute_background_impl(
            command,
            log_path,
            exec::BackgroundPrivilege::Sudo {
                password: sudo_password,
            },
        )
        .await
    }

    fn sanitize_or_tool_error(&self, command: &str) -> std::result::Result<String, CallToolResult> {
        sanitize_command(command, self.max_chars).map_err(|e| {
            error!(error = ?e, "Command sanitization failed");
            CallToolResult::error(vec![ContentBlock::text(format!("Error: {}", e))])
        })
    }

    fn calltool_from_command_output(output: CommandOutput) -> CallToolResult {
        // Combine stdout and stderr for the response
        let mut result_text = output.stdout;
        if !output.stderr.is_empty() {
            if !result_text.is_empty() {
                result_text.push_str("\n--- stderr ---\n");
            }
            result_text.push_str(&output.stderr);
        }

        // Check for error exit code.
        // exit_code=None means the SSH channel was torn down without delivering
        // an exit status or exit signal — treat as error, not success.
        if output.exit_code.map(|code| code != 0).unwrap_or(true) {
            CallToolResult::error(vec![ContentBlock::text(result_text)])
        } else {
            CallToolResult::success(vec![ContentBlock::text(result_text)])
        }
    }

    /// Build shell tool definition (compact)
    fn shell_tool() -> Tool {
        tools::shell_tool()
    }

    /// Build sudo_shell tool definition (compact)
    fn sudo_shell_tool() -> Tool {
        tools::sudo_shell_tool()
    }

    /// Build transfer tool definition (compact)
    fn transfer_tool() -> Tool {
        tools::transfer_tool()
    }

    /// Build check_process tool definition
    fn check_process_tool() -> Tool {
        tools::check_process_tool()
    }

    /// Build read tool definition
    fn read_file_tool() -> Tool {
        tools::read_file_tool()
    }

    /// Build apply_patch tool definition
    fn apply_patch_tool() -> Tool {
        tools::apply_patch_tool()
    }

    /// Build sudo_apply_patch tool definition
    fn sudo_apply_patch_tool() -> Tool {
        tools::sudo_apply_patch_tool()
    }

    /// Get extended documentation for a tool by name
    ///
    /// Returns the full documentation text that was removed from compact tool definitions
    /// to save tokens in the MCP protocol.
    pub fn get_tool_documentation(tool_name: &str) -> Option<&'static str> {
        tools::get_tool_documentation(tool_name)
    }

    /// Resolve timeout duration from optional milliseconds, falling back to server default.
    fn resolve_timeout(&self, timeout_ms: Option<u64>) -> Duration {
        timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(self.timeout)
    }

    /// Parse tool parameters from JSON with standardized error handling.
    fn parse_tool_params<T: serde::de::DeserializeOwned>(
        &self,
        args: serde_json::Map<String, serde_json::Value>,
        tool_name: &str,
    ) -> std::result::Result<T, McpError> {
        serde_json::from_value(serde_json::Value::Object(args))
            .map_err(|e| McpError::invalid_params(format!("invalid {tool_name} params: {e}"), None))
    }

    /// Execute transfer tool with connection management and JSON serialization.
    async fn execute_transfer(
        &self,
        params: TransferParams,
        verbose: bool,
    ) -> std::result::Result<CallToolResult, McpError> {
        let timeout = self.resolve_timeout(params.timeout_ms);
        let key_path = self.config.key.clone();

        // Ensure connection is established (so errors are deterministic).
        if let Err(e) = self.connection.ensure_connected().await {
            let resp = crate::transfer::TransferResponse::error(
                params,
                self.transfer.local_root(),
                &e.to_string(),
            );
            let body = resp
                .to_json(verbose)
                .unwrap_or_else(|_| "{\"ok\":false,\"error\":\"serialization_error\"}".to_string());
            return Ok(CallToolResult::success(vec![ContentBlock::text(body)]));
        }

        let resp = self
            .transfer
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
            .await;
        let body = resp
            .to_json(verbose)
            .unwrap_or_else(|_| "{\"ok\":false,\"error\":\"serialization_error\"}".to_string());
        Ok(CallToolResult::success(vec![ContentBlock::text(body)]))
    }
}

impl ServerHandler for SshMcpServer {
    /// Return server information
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(ProtocolVersion::LATEST)
            .with_server_info(Implementation::from_build_env())
            .with_instructions(format!(
                "SSH MCP Server v{} - Execute commands on {}@{}:{}",
                env!("CARGO_PKG_VERSION"),
                self.config.user,
                self.config.host,
                self.config.port,
            ))
    }

    /// List available tools
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<ListToolsResult, McpError> {
        debug!("list_tools called");

        let mut tools = vec![Self::shell_tool()];

        // Docs/expected order: shell, optional sudo tools, check_process, transfer, read, apply_patch.
        if !self.config.disable_sudo {
            tools.push(Self::sudo_shell_tool());
            tools.push(Self::sudo_apply_patch_tool());
        }
        tools.push(Self::check_process_tool());
        tools.push(Self::transfer_tool());
        tools.push(Self::read_file_tool());
        tools.push(Self::apply_patch_tool());

        Ok(ListToolsResult {
            tools,
            next_cursor: None,
            meta: Default::default(),
        })
    }

    /// Call a tool
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResult, McpError> {
        let tool_name: &str = request.name.as_ref();
        debug!("call_tool called: {:?}", tool_name);

        let args = request.arguments.unwrap_or_default();

        // Route to the appropriate tool
        match tool_name {
            "shell" => {
                let parsed = self.parse_common_tool_args(&args)?;
                let timeout = self.resolve_timeout(parsed.timeout_ms);

                if parsed.background {
                    self.execute_background_command(&parsed.command, parsed.log_path.as_deref())
                        .await
                } else {
                    self.execute_command_with_timeout(&parsed.command, timeout)
                        .await
                }
            }
            "sudo_shell" => {
                if self.config.disable_sudo {
                    return Err(McpError::invalid_params(
                        "sudo_shell tool is disabled",
                        None,
                    ));
                }

                let parsed = self.parse_common_tool_args(&args)?;
                let timeout = self.resolve_timeout(parsed.timeout_ms);

                if parsed.background {
                    self.execute_background_sudo_command(
                        &parsed.command,
                        parsed.log_path.as_deref(),
                    )
                    .await
                } else {
                    self.execute_sudo_command_with_timeout(&parsed.command, timeout)
                        .await
                }
            }
            "transfer" => {
                let params: TransferParams = self.parse_tool_params(args, "transfer")?;
                let verbose = params.verbose;
                self.execute_transfer(params, verbose).await
            }
            "check_process" => {
                let params: args::CheckProcessToolArgs =
                    self.parse_tool_params(args, "check_process")?;
                self.execute_check_process(params.check, params.wait_for, context.ct.cancelled())
                    .await
            }
            "read" => {
                let params: ReadFileParams = self.parse_tool_params(args, "read")?;
                self.execute_read_file(params).await
            }
            "apply_patch" => {
                let params: ApplyPatchParams = self.parse_tool_params(args, "apply_patch")?;
                self.execute_apply_patch(
                    params,
                    FileEditFaultInjection::None,
                    FileEditPrivilege::User,
                )
                .await
            }
            "sudo_apply_patch" => {
                if self.config.disable_sudo {
                    return Err(McpError::invalid_params(
                        "sudo_apply_patch tool is disabled",
                        None,
                    ));
                }

                let params: ApplyPatchParams = self.parse_tool_params(args, "sudo_apply_patch")?;
                self.execute_apply_patch(
                    params,
                    FileEditFaultInjection::None,
                    FileEditPrivilege::Sudo,
                )
                .await
            }
            _ => Err(McpError::invalid_params(
                format!("Unknown tool: {}", tool_name),
                None,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::background::response::{
        BACKGROUND_JSON_SNIPPET_LIMIT_CHARS, background_json_err, background_json_timeout,
    };
    use crate::background::wrapper::{build_background_wrapper_script, remote_job_log_path};
    use crate::server::validation::common::validate_read_file_path;
    use crate::server::validation::read_file::sanitize_read_file_stderr_snippet;

    fn extract_text_from_result(result: &CallToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|text| text.text.clone()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn test_server_info() {
        // Verify the package version is defined
        assert!(!env!("CARGO_PKG_VERSION").is_empty());
    }

    #[test]
    fn test_shell_tool_definition() {
        let tool = SshMcpServer::shell_tool();
        assert_eq!(tool.name.as_ref(), "shell");
        assert!(tool.description.is_some());
    }

    #[test]
    fn test_sudo_shell_tool_definition() {
        let tool = SshMcpServer::sudo_shell_tool();
        assert_eq!(tool.name.as_ref(), "sudo_shell");
        assert!(tool.description.is_some());
    }

    #[test]
    fn test_read_file_tool_definition() {
        let tool = SshMcpServer::read_file_tool();
        assert_eq!(tool.name.as_ref(), "read");
        assert!(tool.description.is_some());
    }

    #[test]
    fn test_apply_patch_tool_definition() {
        let tool = SshMcpServer::apply_patch_tool();
        assert_eq!(tool.name.as_ref(), "apply_patch");
        assert!(tool.description.is_some());
    }

    #[test]
    fn test_sudo_apply_patch_tool_definition() {
        let tool = SshMcpServer::sudo_apply_patch_tool();
        assert_eq!(tool.name.as_ref(), "sudo_apply_patch");
        assert!(tool.description.is_some());
    }

    #[test]
    fn test_build_background_wrapper_escapes_single_quotes_in_user_command() {
        let remote_log = remote_job_log_path("job-1");
        let script = build_background_wrapper_script("job-1", "echo 'hello world'", &remote_log);
        assert!(script.contains("exec sh -c 'set +m; echo '\"'\"'hello world'\"'\"''"));
    }

    #[test]
    fn test_build_background_wrapper_is_busybox_friendly() {
        let remote_log = remote_job_log_path("job-1");
        let script = build_background_wrapper_script("job-1", "echo test", &remote_log);
        assert!(!script.contains("dirname --"));
        assert!(!script.contains("mkdir -p --"));
        assert!(!script.contains("sh -lc"));
        assert!(script.contains("exec sh -c"));
        assert!(!script.contains("nohup"));
    }

    #[test]
    fn test_background_wrapper_emits_markers_and_exec() {
        let remote_log = remote_job_log_path("job-1");
        let script = build_background_wrapper_script("job-1", "echo test", &remote_log);
        assert!(script.contains("__SSH_MCP_JOB_ID=job-1"));
        assert!(script.contains("__SSH_MCP_PID=$$"));
        assert!(script.contains("__SSH_MCP_LOG=$LOG"));
        assert!(script.contains("exec sh -c"));
    }

    #[test]
    fn test_background_wrapper_does_not_redirect_remote_output() {
        let remote_log = remote_job_log_path("job-1");
        let script = build_background_wrapper_script("job-1", "echo test", &remote_log);
        assert!(!script.contains(">$LOG"));
        assert!(!script.contains("2>&1"));
        assert!(!script.contains("$EXIT"));
        assert!(!script.contains("nohup"));
    }

    #[test]
    fn test_validate_background_log_path_rejects_leading_dash() {
        let err =
            validate_background_log_path(Path::new("/tmp/ssh-mcp"), "-not-a-path").unwrap_err();
        assert!(err.contains("start with '-'") || err.contains("start with"));
    }

    #[test]
    fn test_validate_background_log_path_rejects_newlines() {
        assert!(
            validate_background_log_path(Path::new("/tmp/ssh-mcp"), "/tmp/x\nrm -rf /").is_err()
        );
        assert!(
            validate_background_log_path(Path::new("/tmp/ssh-mcp"), "/tmp/x\rrm -rf /").is_err()
        );
    }

    #[test]
    fn test_validate_read_file_path_requires_absolute() {
        let err = validate_read_file_path("relative/path").unwrap_err();
        assert!(err.contains("absolute"));
    }

    #[test]
    fn test_validate_read_file_path_rejects_trailing_slash() {
        let err = validate_read_file_path("/etc/").unwrap_err();
        assert!(err.contains("must not end with '/'"));
    }

    #[test]
    fn test_resolve_read_file_max_bytes_uses_token_limit() {
        assert_eq!(
            resolve_read_file_max_bytes(Some(12_000)),
            12_000 * READ_FILE_BYTES_PER_TOKEN
        );
    }

    #[test]
    fn test_resolve_read_file_max_bytes_none_uses_hard_cap() {
        assert_eq!(resolve_read_file_max_bytes(None), READ_FILE_HARD_MAX_BYTES);
    }

    #[test]
    fn test_resolve_read_file_max_bytes_applies_hard_cap() {
        let very_large_tokens = READ_FILE_HARD_MAX_BYTES;
        assert_eq!(
            resolve_read_file_max_bytes(Some(very_large_tokens)),
            READ_FILE_HARD_MAX_BYTES
        );
    }

    #[test]
    fn test_estimate_tokens_from_bytes_rounds_up() {
        assert_eq!(estimate_tokens_from_bytes(0), 0);
        assert_eq!(estimate_tokens_from_bytes(1), 1);
        assert_eq!(estimate_tokens_from_bytes(4), 1);
        assert_eq!(estimate_tokens_from_bytes(5), 2);
    }

    #[test]
    fn test_resolve_read_file_line_limit_defaults_to_preview_window() {
        let preview = resolve_read_file_line_limit(ReadFileMode::Preview, None)
            .expect("preview lines should resolve");
        assert_eq!(preview, Some(READ_FILE_DEFAULT_PREVIEW_LINES));

        let head = resolve_read_file_line_limit(ReadFileMode::Head, None)
            .expect("head lines should resolve");
        assert_eq!(head, Some(READ_FILE_DEFAULT_PREVIEW_LINES));

        let tail = resolve_read_file_line_limit(ReadFileMode::Tail, None)
            .expect("tail lines should resolve");
        assert_eq!(tail, Some(READ_FILE_DEFAULT_PREVIEW_LINES));
    }

    #[test]
    fn test_resolve_read_file_line_limit_for_full_ignores_lines() {
        let full = resolve_read_file_line_limit(ReadFileMode::Full, Some(123))
            .expect("full mode should ignore lines");
        assert_eq!(full, None);
    }

    #[test]
    fn test_resolve_read_file_line_limit_rejects_zero() {
        let err = resolve_read_file_line_limit(ReadFileMode::Head, Some(0)).unwrap_err();
        assert!(err.contains("positive"));
    }

    #[test]
    fn test_resolve_read_file_line_limit_rejects_too_large() {
        let err =
            resolve_read_file_line_limit(ReadFileMode::Tail, Some(READ_FILE_MAX_LINE_WINDOW + 1))
                .unwrap_err();
        assert!(err.contains("<="));
    }

    #[test]
    fn test_sanitize_read_file_stderr_snippet_normalizes_whitespace_and_controls() {
        let stderr = "line1\nline2\t\u{0007}bad\rline3";
        let snippet = sanitize_read_file_stderr_snippet(stderr)
            .expect("snippet should be present for non-empty stderr");
        assert_eq!(snippet, "line1 line2 bad line3");
    }

    #[test]
    fn test_background_json_err_omits_unregistered_job_fields() {
        let long_error = "e".repeat(BACKGROUND_JSON_SNIPPET_LIMIT_CHARS + 10);
        let long_stderr = "s".repeat(BACKGROUND_JSON_SNIPPET_LIMIT_CHARS + 10);

        let result = background_json_err(&long_error, &long_stderr);
        let text = extract_text_from_result(&result);

        let value: serde_json::Value =
            serde_json::from_str(text.trim()).expect("background_json_err should return JSON");

        assert_eq!(value.get("ok").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(
            value.get("background").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(value.get("truncated").and_then(|v| v.as_bool()), Some(true));
        assert!(value.get("job_id").is_none());
        assert!(value.get("log_path").is_none());
        assert!(value.get("hint").is_none());

        let fields = value
            .get("truncated_fields")
            .expect("expected truncated_fields");
        assert_eq!(fields.get("error").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(fields.get("stderr").and_then(|v| v.as_bool()), Some(true));

        let error_snippet = value
            .get("error")
            .and_then(|v| v.as_str())
            .expect("expected error field");
        assert_eq!(
            error_snippet.chars().count(),
            BACKGROUND_JSON_SNIPPET_LIMIT_CHARS
        );
        let stderr_snippet = value
            .get("stderr")
            .and_then(|v| v.as_str())
            .expect("expected stderr field");
        assert_eq!(
            stderr_snippet.chars().count(),
            BACKGROUND_JSON_SNIPPET_LIMIT_CHARS
        );
    }

    #[test]
    fn test_background_json_timeout_hint_contains_pid_and_check_process_tool() {
        let result = background_json_timeout(
            "job-42",
            4242,
            "/tmp/ssh-mcp/local.log",
            &crate::background::response::BackgroundTimeoutSnapshot {
                state: "running",
                still_running: true,
                exit_code: None,
                state_reason: None,
                elapsed_time: "00:01",
                log_exists: true,
                log_tail: "tail line",
                tail_lines_used: 50,
            },
        );
        let text = extract_text_from_result(&result);

        let value: serde_json::Value =
            serde_json::from_str(text.trim()).expect("background_json_timeout should return JSON");

        assert_eq!(value.get("ok").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(value.get("timeout").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(
            value.get("background").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            value.get("still_running").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(value.get("state").and_then(|v| v.as_str()), Some("running"));
        assert_eq!(
            value.get("tail_lines_used").and_then(|v| v.as_u64()),
            Some(50)
        );
        assert_eq!(
            value.get("elapsed_time").and_then(|v| v.as_str()),
            Some("00:01")
        );
        assert_eq!(
            value.get("log_tail").and_then(|v| v.as_str()),
            Some("tail line")
        );

        let hint = value
            .get("hint")
            .and_then(|v| v.as_str())
            .expect("expected hint field");

        // Hint should contain the actual job_id value
        assert!(
            hint.contains("job_id=job-42"),
            "hint should contain the actual job_id value; got: '{hint}'"
        );
        // Hint should mention the check_process tool
        assert!(
            hint.contains("check_process"),
            "hint should mention check_process tool; got: '{hint}'"
        );
        // Hint should warn against restarting
        assert!(
            hint.contains("DO NOT restart"),
            "hint should warn against restarting; got: '{hint}'"
        );
        // Hint should use TIMEOUT_RECOVERY prefix
        assert!(
            hint.contains("TIMEOUT_RECOVERY"),
            "hint should start with TIMEOUT_RECOVERY; got: '{hint}'"
        );
        assert!(
            hint.contains("MCP client deadlines may be shorter than timeout_ms"),
            "hint should distinguish the client deadline from timeout_ms; got: '{hint}'"
        );
        assert!(
            hint.contains("background=true"),
            "hint should recommend explicit background mode; got: '{hint}'"
        );
        // Hint should NOT contain old placeholders
        assert!(
            !hint.contains("<pid>"),
            "hint should not contain <pid> placeholder; got: '{hint}'"
        );
        assert!(
            !hint.contains("<log_path>"),
            "hint should not contain <log_path> placeholder; got: '{hint}'"
        );
    }

    #[test]
    fn test_tool_documentation_available() {
        // Verify that extended documentation is available for all tools
        assert!(SshMcpServer::get_tool_documentation("shell").is_some());
        assert!(SshMcpServer::get_tool_documentation("sudo_shell").is_some());
        assert!(SshMcpServer::get_tool_documentation("transfer").is_some());
        assert!(SshMcpServer::get_tool_documentation("read").is_some());
        assert!(SshMcpServer::get_tool_documentation("apply_patch").is_some());
        assert!(SshMcpServer::get_tool_documentation("sudo_apply_patch").is_some());
        assert!(SshMcpServer::get_tool_documentation("write-file").is_none());
        assert!(SshMcpServer::get_tool_documentation("replace-in-file").is_none());
        assert!(SshMcpServer::get_tool_documentation("unknown").is_none());
    }

    #[test]
    fn test_shell_documentation_content() {
        let docs = SshMcpServer::get_tool_documentation("shell").unwrap();
        assert!(docs.contains("SHELL TOOL"));
        assert!(docs.contains("PARAMETERS:"));
        assert!(docs.contains("BACKGROUND MODE:"));
        assert!(docs.contains("command"));
        assert!(docs.contains("background"));
        assert!(docs.contains("still_running"));
        assert!(docs.contains("not the full tool-call deadline"));
        assert!(docs.contains("client may stop waiting earlier"));
    }

    #[test]
    fn test_sudo_shell_documentation_content() {
        let docs = SshMcpServer::get_tool_documentation("sudo_shell").unwrap();
        assert!(docs.contains("SUDO_SHELL TOOL"));
        assert!(docs.contains("sudo"));
        assert!(docs.contains("not the full tool-call deadline"));
    }

    #[test]
    fn test_transfer_documentation_content() {
        let docs = SshMcpServer::get_tool_documentation("transfer").unwrap();
        assert!(docs.contains("TRANSFER TOOL"));
        assert!(docs.contains("put"));
        assert!(docs.contains("get"));
        assert!(docs.contains("TRANSPORTS:"));
    }

    #[test]
    fn test_read_file_documentation_content() {
        let docs = SshMcpServer::get_tool_documentation("read").unwrap();
        assert!(docs.contains("READ TOOL"));
        assert!(docs.contains("remote_path"));
        assert!(docs.contains("mode"));
        assert!(docs.contains("UTF-8"));
    }

    #[test]
    fn test_apply_patch_documentation_content() {
        let docs = SshMcpServer::get_tool_documentation("apply_patch").unwrap();
        assert!(docs.contains("APPLY_PATCH TOOL"));
        assert!(docs.contains("Add File"));
        assert!(docs.contains("Delete File"));
    }

    #[test]
    fn test_sudo_apply_patch_documentation_content() {
        let docs = SshMcpServer::get_tool_documentation("sudo_apply_patch").unwrap();
        assert!(docs.contains("SUDO_APPLY_PATCH TOOL"));
        assert!(docs.contains("sudo"));
    }

    #[test]
    fn test_compact_tool_descriptions() {
        // Verify that tool descriptions are compact (not verbose)
        let shell = SshMcpServer::shell_tool();
        let sudo_shell = SshMcpServer::sudo_shell_tool();
        let transfer = SshMcpServer::transfer_tool();
        let read_file = SshMcpServer::read_file_tool();
        let apply_patch = SshMcpServer::apply_patch_tool();
        let sudo_apply_patch = SshMcpServer::sudo_apply_patch_tool();

        // Descriptions should be present but concise (under 100 chars)
        if let Some(desc) = shell.description {
            assert!(
                desc.len() < 100,
                "shell description too long: {} chars",
                desc.len()
            );
        }
        if let Some(desc) = sudo_shell.description {
            assert!(
                desc.len() < 100,
                "sudo_shell description too long: {} chars",
                desc.len()
            );
        }
        if let Some(desc) = transfer.description {
            assert!(
                desc.len() < 100,
                "transfer description too long: {} chars",
                desc.len()
            );
        }
        if let Some(desc) = read_file.description {
            assert!(
                desc.len() < 100,
                "read description too long: {} chars",
                desc.len()
            );
        }
        if let Some(desc) = apply_patch.description {
            assert!(
                desc.len() < 100,
                "apply_patch description too long: {} chars",
                desc.len()
            );
        }
        if let Some(desc) = sudo_apply_patch.description {
            assert!(
                desc.len() < 100,
                "sudo_apply_patch description too long: {} chars",
                desc.len()
            );
        }
    }
}
