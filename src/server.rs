//! MCP Server implementation
//!
//! This module provides the main MCP server that integrates SSH connection
//! management with the `exec` and `sudo-exec` tools.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use rmcp::{
    ErrorData as McpError,
    handler::server::ServerHandler,
    model::*,
    service::{RequestContext, RoleServer},
};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::Mutex;
use tracing::{debug, error, info};

use crate::background::detach::{DetachMode, DetachProbeOutput, DetachProbeRequest};
use crate::background::job::NewRunningJob;
use crate::background::marker::read_background_markers_from_channel;
use crate::background::response::background_json_timeout;
use crate::background::wrapper::{
    build_background_wrapper_script_full, build_background_wrapper_script_portable,
    remote_job_log_path,
};
use crate::background::{JobRegistry, JobState, LocalLogSpooler, OutputStreamer, SharedJobState};
use crate::config::Config;
use crate::error::{Result, SshMcpError};
#[cfg(unix)]
use crate::platform::O_NOFOLLOW_FLAG;
use crate::ssh::sanitize::wrap_in_posix_shell;
use crate::ssh::{
    CommandOutput, SshConfig, SshConnectionManager, sanitize_command, wrap_sudo_command,
};
use crate::tools::CheckProcessParams;
use crate::transfer::{TransferEngine, TransferParams, TransferRunContext, TransferSshOptions};
use crate::validate::validate_basic_path_str;

mod args;
mod exec;
mod tools;

const BACKGROUND_START_TIMEOUT: Duration = Duration::from_secs(20);

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

fn build_background_wrapper_script(
    mode: DetachMode,
    job_id: &str,
    user_command: &str,
    log_path: &str,
) -> String {
    match mode {
        DetachMode::Full | DetachMode::Unknown => {
            build_background_wrapper_script_full(job_id, user_command, log_path)
        }
        DetachMode::Portable => {
            build_background_wrapper_script_portable(job_id, user_command, log_path)
        }
        DetachMode::DirectOnly => {
            build_background_wrapper_script_portable(job_id, user_command, log_path)
        }
    }
}

fn validate_background_log_path(
    base_dir: &Path,
    log_path: &str,
) -> std::result::Result<(), String> {
    validate_basic_path_str(log_path, "log_path")?;

    // Current semantics: log_path is a LOCAL path on the MCP server.
    // Keep it in a single, fixed spool directory to avoid arbitrary local writes.
    let path = Path::new(log_path);
    if !path.is_absolute() {
        return Err("log_path must be an absolute path".to_string());
    }
    if path
        .components()
        .any(|c| matches!(c, Component::CurDir | Component::ParentDir))
    {
        return Err("log_path must not contain '.' or '..' path components".to_string());
    }

    if path.parent() != Some(base_dir) {
        return Err(format!(
            "log_path must be directly under {}",
            base_dir.display()
        ));
    }
    if path.extension().and_then(|s| s.to_str()) != Some("log") {
        return Err("log_path must have a .log extension".to_string());
    }

    Ok(())
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

    detach_mode: Arc<AtomicU8>,
    detach_mode_lock: Arc<Mutex<()>>,

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
            detach_mode: Arc::new(AtomicU8::new(DetachMode::Unknown.as_u8())),
            detach_mode_lock: Arc::new(Mutex::new(())),
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

    async fn determine_detach_mode(&self) -> DetachMode {
        let server = self.clone();
        crate::background::detach::determine_detach_mode(
            self.detach_mode.as_ref(),
            self.detach_mode_lock.as_ref(),
            make_job_id,
            move |req, timeout| {
                let server = server.clone();
                async move { server.exec_detach_probe(req, timeout).await }
            },
        )
        .await
    }

    async fn exec_detach_probe(
        &self,
        req: DetachProbeRequest,
        timeout: Duration,
    ) -> Result<DetachProbeOutput> {
        let output = self.connection.exec_command(&req.wrapper, timeout).await?;
        Ok(DetachProbeOutput {
            stdout: output.stdout,
            stderr: output.stderr,
            exit_code: output.exit_code,
        })
    }

    /// Execute a command (used by exec tool)
    async fn execute_command_with_timeout(
        &self,
        command: &str,
        timeout: Duration,
    ) -> std::result::Result<CallToolResult, McpError> {
        debug!(
            "exec tool called: cmd_len={}, background=false, sudo=false, timeout_ms={}",
            command.len(),
            timeout.as_millis()
        );

        // Sanitize the command
        let sanitized = match self.sanitize_or_tool_error(command) {
            Ok(cmd) => cmd,
            Err(result) => return Ok(result),
        };

        // Ensure connection is established
        if let Err(e) = self.connection.ensure_connected().await {
            error!(error = ?e, "Failed to ensure SSH connection");
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "SSH connection error: {}",
                e
            ))]));
        }

        // If su elevation is configured and available, ensure we're elevated
        if self.connection.get_su_password().is_some()
            && let Err(e) = self.connection.ensure_elevated().await
        {
            debug!(error = ?e, "Elevation failed, will run as normal user");
        }

        // Foreground execution is detachable-by-design:
        // - Start the command on a dedicated SSH channel
        // - Stream remote stdout/stderr into a local spool file
        // - If timeout elapses, return JSON with job_id/pid/log_path while the stream continues

        let detach_mode = self.determine_detach_mode().await;
        if detach_mode == DetachMode::DirectOnly {
            match self.connection.exec_command(&sanitized, timeout).await {
                Ok(output) => return Ok(Self::calltool_from_command_output(output)),
                Err(e) => {
                    error!(error = ?e, "Command execution failed");
                    let mut msg = format!("Error: {}", e);
                    if matches!(e, SshMcpError::Timeout(_)) {
                        msg.push_str("\nHint: background detach is not supported on this target; rerun with background=true or a larger timeout_ms.");
                    }
                    return Ok(CallToolResult::error(vec![Content::text(msg)]));
                }
            }
        }

        let job_id = make_job_id();

        let (final_log_path_buf, final_log_path) = match self.default_local_log_path(&job_id) {
            Ok(v) => v,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error: {e}"
                ))]));
            }
        };
        if let Err(e) = self.ensure_local_log_file(&final_log_path_buf).await {
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "Error: {e}"
            ))]));
        }

        let remote_log_path = remote_job_log_path(&job_id);
        let wrapper =
            build_background_wrapper_script(detach_mode, &job_id, &sanitized, &remote_log_path);

        let permit = match self.connection.acquire_command_slot_raw().await {
            Ok(p) => p,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error: Failed to acquire command slot: {e}"
                ))]));
            }
        };

        let mut channel = match self.connection.open_channel().await {
            Ok(ch) => ch,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error: {e}"
                ))]));
            }
        };

        let wrapped_wrapper = wrap_in_posix_shell(&wrapper, false);
        if let Err(e) = channel.exec(true, wrapped_wrapper.as_str()).await {
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "Error: Failed to exec background wrapper: {e}"
            ))]));
        }

        let (markers, initial_stdout) = match read_background_markers_from_channel(
            &mut channel,
            &job_id,
            &remote_log_path,
            BACKGROUND_START_TIMEOUT,
        )
        .await
        {
            Ok(v) => v,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error: {e}"
                ))]));
            }
        };

        self.register_running_job(&job_id, markers.pid, final_log_path_buf.clone(), &sanitized)
            .await;

        let streamer = OutputStreamer::new(
            job_id.clone(),
            final_log_path_buf.clone(),
            Arc::clone(&self.job_registry),
        );

        let join = tokio::spawn(async move {
            let _permit = permit;
            streamer.stream_channel(channel, initial_stdout).await
        });

        let completed = tokio::time::timeout(timeout, join).await;
        let join_exit_code: Option<i32> = match completed {
            Ok(joined) => match joined {
                Ok(Ok(code)) => code,
                Ok(Err(e)) => {
                    error!(job_id = ?job_id, error = ?e, "streaming failed");
                    return Ok(CallToolResult::error(vec![Content::text(format!(
                        "Error: streaming failed: {e}"
                    ))]));
                }
                Err(e) => {
                    error!(job_id = ?job_id, error = ?e, "streaming task join failed");
                    return Ok(CallToolResult::error(vec![Content::text(format!(
                        "Error: streaming task join failed: {e}"
                    ))]));
                }
            },
            Err(_) => {
                return Ok(background_json_timeout(
                    &job_id,
                    markers.pid,
                    &final_log_path,
                    &markers.remote_log_path,
                ));
            }
        };

        // Phase 5 semantics: exit codes are sourced from local JobRegistry (updated by OutputStreamer).
        let registry_exit_code: Option<i32> = match self.job_registry.get(&job_id).await {
            Some(job) => {
                let job_guard = job.lock().await;
                job_guard.exit_code
            }
            None => None,
        };

        let exit_code_u32 = registry_exit_code
            .or(join_exit_code)
            .and_then(|code| u32::try_from(code).ok())
            .unwrap_or(255)
            .min(255);

        let mut file = match tokio::fs::File::open(&final_log_path_buf).await {
            Ok(f) => f,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error: failed to read local log: {e}"
                ))]));
            }
        };

        let max_bytes = self.config.max_output_tokens.map(|t| t.saturating_mul(4));

        const TAIL_BYTES: u64 = 512;

        let stdout = match max_bytes {
            Some(limit) => {
                let meta = file.metadata().await.map_err(|e| {
                    McpError::internal_error(format!("failed to stat local log: {e}"), None)
                })?;
                let file_len = meta.len();

                if file_len <= limit as u64 {
                    let mut buf = Vec::new();
                    file.read_to_end(&mut buf).await.map_err(|e| {
                        McpError::internal_error(format!("failed to read local log: {e}"), None)
                    })?;
                    String::from_utf8_lossy(&buf).to_string()
                } else {
                    let mut head = vec![0u8; limit];
                    let mut read_total = 0usize;
                    while read_total < limit {
                        let n = file.read(&mut head[read_total..]).await.map_err(|e| {
                            McpError::internal_error(format!("failed to read local log: {e}"), None)
                        })?;
                        if n == 0 {
                            break;
                        }
                        read_total = read_total.saturating_add(n);
                    }
                    head.truncate(read_total);

                    let tail_len = std::cmp::min(TAIL_BYTES, file_len);
                    file.seek(std::io::SeekFrom::Start(file_len.saturating_sub(tail_len)))
                        .await
                        .map_err(|e| {
                            McpError::internal_error(format!("failed to seek local log: {e}"), None)
                        })?;

                    let mut tail = vec![0u8; tail_len as usize];
                    let mut tail_read = 0usize;
                    while tail_read < tail.len() {
                        let n = file.read(&mut tail[tail_read..]).await.map_err(|e| {
                            McpError::internal_error(
                                format!("failed to read local log tail: {e}"),
                                None,
                            )
                        })?;
                        if n == 0 {
                            break;
                        }
                        tail_read = tail_read.saturating_add(n);
                    }
                    tail.truncate(tail_read);

                    let total_tokens = (file_len as usize).saturating_div(4);
                    let mut out = String::from_utf8_lossy(&head).to_string();
                    out.push_str(&format!(
                        "\n[Output truncated: {} tokens total]",
                        total_tokens
                    ));
                    out.push_str(
                        "\n[Tip: Use 'head -n 100' for first lines, 'tail -n 100' for last lines]",
                    );
                    out.push_str("\n[Tip: For large output use SFTP/SCP tools to download files]");
                    if !tail.is_empty() {
                        out.push('\n');
                        out.push_str(&String::from_utf8_lossy(&tail));
                    }
                    out
                }
            }
            None => {
                let mut buf = Vec::new();
                file.read_to_end(&mut buf).await.map_err(|e| {
                    McpError::internal_error(format!("failed to read local log: {e}"), None)
                })?;
                String::from_utf8_lossy(&buf).to_string()
            }
        };

        let output = CommandOutput {
            stdout,
            stderr: String::new(),
            exit_code: Some(exit_code_u32),
            ..Default::default()
        };
        Ok(Self::calltool_from_command_output(output))
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

    /// Execute a command with sudo (used by sudo-exec tool)
    async fn execute_sudo_command_with_timeout(
        &self,
        command: &str,
        timeout: Duration,
    ) -> std::result::Result<CallToolResult, McpError> {
        debug!(
            "sudo-exec tool called: cmd_len={}, background=false, sudo=true, timeout_ms={}",
            command.len(),
            timeout.as_millis()
        );

        // Sanitize the command
        let sanitized = match self.sanitize_or_tool_error(command) {
            Ok(cmd) => cmd,
            Err(result) => return Ok(result),
        };

        // Ensure connection is established
        if let Err(e) = self.connection.ensure_connected().await {
            error!(error = ?e, "Failed to ensure SSH connection");
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "SSH connection error: {}",
                e
            ))]));
        }

        // Wrap the command with sudo
        let sudo_password = self.connection.get_sudo_password();
        let wrapped_command = wrap_sudo_command(&sanitized, sudo_password);
        debug!(
            "Wrapped sudo command (password hidden): sudo -n sh -c '...' or printf '...' | sudo ..."
        );

        // Execute the wrapped command
        match self
            .connection
            .exec_command(&wrapped_command, timeout)
            .await
        {
            Ok(output) => Ok(Self::calltool_from_command_output(output)),
            Err(e) => {
                error!(error = ?e, "Sudo command execution failed");
                let mut msg = format!("Error: {}", e);
                if matches!(e, SshMcpError::Timeout(_)) {
                    msg.push_str(
                        "\nHint: rerun with background=true; then use check-process with job_id.",
                    );
                }
                Ok(CallToolResult::error(vec![Content::text(msg)]))
            }
        }
    }

    async fn execute_sudo_command(
        &self,
        command: &str,
    ) -> std::result::Result<CallToolResult, McpError> {
        self.execute_sudo_command_with_timeout(command, self.timeout)
            .await
    }

    /// Execute check-process tool
    async fn execute_check_process(
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
            CallToolResult::error(vec![Content::text(format!("Error: {}", e))])
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

        // Check for error exit code
        if output.exit_code.map(|code| code != 0).unwrap_or(false) {
            CallToolResult::error(vec![Content::text(result_text)])
        } else {
            CallToolResult::success(vec![Content::text(result_text)])
        }
    }

    /// Build exec tool definition (compact)
    fn exec_tool() -> Tool {
        tools::exec_tool()
    }

    /// Build sudo-exec tool definition (compact)
    fn sudo_exec_tool() -> Tool {
        tools::sudo_exec_tool()
    }

    /// Build transfer tool definition (compact)
    fn transfer_tool() -> Tool {
        tools::transfer_tool()
    }

    /// Build check-process tool definition
    fn check_process_tool() -> Tool {
        tools::check_process_tool()
    }

    /// Get extended documentation for a tool by name
    ///
    /// Returns the full documentation text that was removed from compact tool definitions
    /// to save tokens in the MCP protocol.
    pub fn get_tool_documentation(tool_name: &str) -> Option<&'static str> {
        tools::get_tool_documentation(tool_name)
    }
}

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

    /// Internal method exposed for testing - starts an exec command in background=true mode
    #[doc(hidden)]
    pub async fn test_execute_background_command(
        &self,
        command: &str,
    ) -> std::result::Result<CallToolResult, McpError> {
        self.execute_background_command(command, None).await
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

impl ServerHandler for SshMcpServer {
    /// Return server information
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::LATEST,
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation::from_build_env(),
            instructions: Some(format!(
                "SSH MCP Server v{} - Execute commands on {}@{}:{}",
                env!("CARGO_PKG_VERSION"),
                self.config.user,
                self.config.host,
                self.config.port,
            )),
        }
    }

    /// List available tools
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParam>,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<ListToolsResult, McpError> {
        debug!("list_tools called");

        let mut tools = vec![Self::exec_tool()];

        // Docs/expected order: exec, (optional) sudo-exec, check-process, transfer.
        if !self.config.disable_sudo {
            tools.push(Self::sudo_exec_tool());
        }
        tools.push(Self::check_process_tool());
        tools.push(Self::transfer_tool());

        Ok(ListToolsResult {
            tools,
            next_cursor: None,
            meta: Default::default(),
        })
    }

    /// Call a tool
    async fn call_tool(
        &self,
        request: CallToolRequestParam,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResult, McpError> {
        let tool_name: &str = request.name.as_ref();
        debug!("call_tool called: {:?}", tool_name);

        let args = request.arguments.unwrap_or_default();

        // Route to the appropriate tool
        match tool_name {
            "exec" => {
                let parsed = self.parse_common_tool_args(&args)?;

                if parsed.background {
                    self.execute_background_command(&parsed.command, parsed.log_path.as_deref())
                        .await
                } else {
                    let timeout = parsed
                        .timeout_ms
                        .map(Duration::from_millis)
                        .unwrap_or(self.timeout);
                    self.execute_command_with_timeout(&parsed.command, timeout)
                        .await
                }
            }
            "sudo_exec" | "sudo-exec" => {
                // Check if sudo is enabled
                if self.config.disable_sudo {
                    return Err(McpError::invalid_params("sudo-exec tool is disabled", None));
                }

                let parsed = self.parse_common_tool_args(&args)?;

                if parsed.background {
                    self.execute_background_sudo_command(
                        &parsed.command,
                        parsed.log_path.as_deref(),
                    )
                    .await
                } else {
                    let timeout = parsed
                        .timeout_ms
                        .map(Duration::from_millis)
                        .unwrap_or(self.timeout);
                    self.execute_sudo_command_with_timeout(&parsed.command, timeout)
                        .await
                }
            }
            "transfer" => {
                let params: TransferParams =
                    serde_json::from_value(serde_json::Value::Object(args)).map_err(|e| {
                        McpError::invalid_params(format!("invalid transfer params: {e}"), None)
                    })?;

                let timeout = params
                    .timeout_ms
                    .map(Duration::from_millis)
                    .unwrap_or(self.timeout);

                let key_path = self.config.key.clone();

                // Store verbose flag before params is moved
                let verbose = params.verbose;

                // Ensure connection is established (so errors are deterministic).
                if let Err(e) = self.connection.ensure_connected().await {
                    let resp = crate::transfer::TransferResponse::error(
                        params,
                        self.transfer.local_root(),
                        &format!("SSH connection error: {e}"),
                    );
                    let body = resp.to_json(verbose).unwrap_or_else(|_| {
                        "{\"ok\":false,\"error\":\"serialization_error\"}".to_string()
                    });
                    return Ok(CallToolResult::success(vec![Content::text(body)]));
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
                            },
                        },
                    )
                    .await;
                let body = resp.to_json(verbose).unwrap_or_else(|_| {
                    "{\"ok\":false,\"error\":\"serialization_error\"}".to_string()
                });
                Ok(CallToolResult::success(vec![Content::text(body)]))
            }
            "check-process" | "check_process" => {
                let params: CheckProcessParams =
                    serde_json::from_value(serde_json::Value::Object(args)).map_err(|e| {
                        McpError::invalid_params(format!("invalid check-process params: {e}"), None)
                    })?;

                self.execute_check_process(params).await
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

    fn extract_text_from_result(result: &CallToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|c| c.raw.as_text().map(|text| text.text.clone()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    // Note: Real tests would require a mock SSH server or testcontainers
    // These are placeholder tests

    #[test]
    fn test_server_info() {
        // Verify the package version is defined
        assert!(!env!("CARGO_PKG_VERSION").is_empty());
    }

    #[test]
    fn test_exec_tool_definition() {
        let tool = SshMcpServer::exec_tool();
        assert_eq!(tool.name.as_ref(), "exec");
        assert!(tool.description.is_some());
    }

    #[test]
    fn test_sudo_exec_tool_definition() {
        let tool = SshMcpServer::sudo_exec_tool();
        assert_eq!(tool.name.as_ref(), "sudo-exec");
        assert!(tool.description.is_some());
    }

    #[test]
    fn test_build_background_wrapper_full_escapes_single_quotes_in_user_command() {
        let remote_log = remote_job_log_path("job-1");
        let script =
            build_background_wrapper_script_full("job-1", "echo 'hello world'", &remote_log);
        assert!(script.contains("exec sh -lc 'set +m; echo '\"'\"'hello world'\"'\"''"));
    }

    #[test]
    fn test_build_background_wrapper_portable_is_busybox_friendly() {
        let remote_log = remote_job_log_path("job-1");
        let script = build_background_wrapper_script_portable("job-1", "echo test", &remote_log);
        assert!(!script.contains("dirname --"));
        assert!(!script.contains("mkdir -p --"));
        assert!(!script.contains("sh -lc"));
        assert!(script.contains("exec sh -c"));
        assert!(!script.contains("nohup"));
    }

    #[test]
    fn test_background_wrappers_emit_markers_and_exec() {
        let remote_log = remote_job_log_path("job-1");

        let full = build_background_wrapper_script_full("job-1", "echo test", &remote_log);
        assert!(full.contains("__SSH_MCP_JOB_ID=job-1"));
        assert!(full.contains("__SSH_MCP_PID=$$"));
        assert!(full.contains("__SSH_MCP_LOG=$LOG"));
        assert!(full.contains("exec sh -lc"));

        let portable = build_background_wrapper_script_portable("job-1", "echo test", &remote_log);
        assert!(portable.contains("__SSH_MCP_JOB_ID=job-1"));
        assert!(portable.contains("__SSH_MCP_PID=$$"));
        assert!(portable.contains("__SSH_MCP_LOG=$LOG"));
        assert!(portable.contains("exec sh -c"));
    }

    #[test]
    fn test_background_wrappers_do_not_redirect_remote_output() {
        let remote_log = remote_job_log_path("job-1");
        let full = build_background_wrapper_script_full("job-1", "echo test", &remote_log);
        assert!(!full.contains(">$LOG"));
        assert!(!full.contains("2>&1"));
        assert!(!full.contains("$EXIT"));
        assert!(!full.contains("nohup"));

        let portable = build_background_wrapper_script_portable("job-1", "echo test", &remote_log);
        assert!(!portable.contains(">$LOG"));
        assert!(!portable.contains("2>&1"));
        assert!(!portable.contains("$EXIT"));
        assert!(!portable.contains("nohup"));
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
    fn test_background_json_err_sets_truncation_flag_and_hint() {
        let long_error = "e".repeat(BACKGROUND_JSON_SNIPPET_LIMIT_CHARS + 10);
        let long_stderr = "s".repeat(BACKGROUND_JSON_SNIPPET_LIMIT_CHARS + 10);

        let result = background_json_err(
            "job-1",
            "/tmp/ssh-mcp/job-1.log",
            Some("/tmp/.ssh-mcp-job-job-1.log"),
            &long_error,
            &long_stderr,
        );
        let text = extract_text_from_result(&result);

        let value: serde_json::Value =
            serde_json::from_str(text.trim()).expect("background_json_err should return JSON");

        assert_eq!(value.get("ok").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(
            value.get("background").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(value.get("truncated").and_then(|v| v.as_bool()), Some(true));

        let fields = value
            .get("truncated_fields")
            .expect("expected truncated_fields");
        assert_eq!(fields.get("error").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(fields.get("stderr").and_then(|v| v.as_bool()), Some(true));

        let hint = value
            .get("hint")
            .and_then(|v| v.as_str())
            .expect("expected hint when truncated");
        assert!(
            hint.contains("check-process") && hint.contains("job_id=job-1"),
            "hint should point to check-process job_id; got: '{hint}'"
        );

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
            "/tmp/.ssh-mcp-job-job-42.log",
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

        let hint = value
            .get("hint")
            .and_then(|v| v.as_str())
            .expect("expected hint field");

        // Hint should contain the actual job_id value
        assert!(
            hint.contains("job_id=job-42"),
            "hint should contain the actual job_id value; got: '{hint}'"
        );
        // Hint should mention the check-process tool
        assert!(
            hint.contains("check-process"),
            "hint should mention check-process tool; got: '{hint}'"
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
        assert!(SshMcpServer::get_tool_documentation("exec").is_some());
        assert!(SshMcpServer::get_tool_documentation("sudo-exec").is_some());
        assert!(SshMcpServer::get_tool_documentation("transfer").is_some());
        assert!(SshMcpServer::get_tool_documentation("unknown").is_none());
    }

    #[test]
    fn test_exec_documentation_content() {
        let docs = SshMcpServer::get_tool_documentation("exec").unwrap();
        assert!(docs.contains("EXEC TOOL"));
        assert!(docs.contains("PARAMETERS:"));
        assert!(docs.contains("BACKGROUND MODE:"));
        assert!(docs.contains("command"));
        assert!(docs.contains("background"));
    }

    #[test]
    fn test_sudo_exec_documentation_content() {
        let docs = SshMcpServer::get_tool_documentation("sudo-exec").unwrap();
        assert!(docs.contains("SUDO-EXEC TOOL"));
        assert!(docs.contains("sudo"));
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
    fn test_compact_tool_descriptions() {
        // Verify that tool descriptions are compact (not verbose)
        let exec = SshMcpServer::exec_tool();
        let sudo_exec = SshMcpServer::sudo_exec_tool();
        let transfer = SshMcpServer::transfer_tool();

        // Descriptions should be present but concise (under 100 chars)
        if let Some(desc) = exec.description {
            assert!(
                desc.len() < 100,
                "exec description too long: {} chars",
                desc.len()
            );
        }
        if let Some(desc) = sudo_exec.description {
            assert!(
                desc.len() < 100,
                "sudo-exec description too long: {} chars",
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
    }
}
