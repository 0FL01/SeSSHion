//! MCP Server implementation
//!
//! This module provides the main MCP server that integrates SSH connection
//! management with the `exec` and `sudo-exec` tools.

use std::path::{Path, PathBuf};
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
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
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
use crate::server::validation::{
    APPLY_FILE_EDIT_HARD_MAX_BYTES, apply_file_edit_too_large_error,
    extract_text_from_call_tool_result, has_apply_file_edit_conflict_marker, normalize_sha256_hex,
    parse_apply_file_edit_error_marker, parse_apply_file_edit_marker_value,
    sanitize_read_file_stderr_snippet, validate_background_log_path,
    validate_read_file_path,
};
#[cfg(test)]
use crate::server::validation::read_file::{
    READ_FILE_BYTES_PER_TOKEN, READ_FILE_DEFAULT_PREVIEW_LINES, READ_FILE_HARD_MAX_BYTES,
    READ_FILE_MAX_LINE_WINDOW,
};
#[cfg(test)]
use crate::server::validation::{
    apply_read_file_window, estimate_tokens_from_bytes, resolve_read_file_line_limit,
    resolve_read_file_max_bytes,
};
use crate::ssh::sanitize::wrap_in_posix_shell;
use crate::ssh::{
    CommandOutput, SshConfig, SshConnectionManager, escape_for_shell, sanitize_command,
    wrap_sudo_command,
};
use crate::ticket::TicketSigner;
use crate::tools::{ApplyFileEditParams, CheckProcessParams, ReadFileMode, ReadFileParams};
use crate::transfer::{TransferEngine, TransferParams, TransferRunContext, TransferSshOptions};

mod args;
mod exec;
mod handlers;
mod testing;
mod tools;
mod validation;

const BACKGROUND_START_TIMEOUT: Duration = Duration::from_secs(20);
const READ_FILE_ERROR_MARKER: &str = "__SSH_MCP_READ_FILE_ERR__";
const APPLY_FILE_EDIT_ERROR_MARKER: &str = "__SSH_MCP_APPLY_FILE_EDIT_ERR__";
const APPLY_FILE_EDIT_PREVIOUS_SHA_MARKER: &str = "__SSH_MCP_APPLY_FILE_EDIT_PREVIOUS_SHA__";
const APPLY_FILE_EDIT_NEW_SHA_MARKER: &str = "__SSH_MCP_APPLY_FILE_EDIT_NEW_SHA__";
const APPLY_FILE_EDIT_ACTUAL_SHA_MARKER: &str = "__SSH_MCP_APPLY_FILE_EDIT_ACTUAL_SHA__";
const APPLY_FILE_EDIT_BASELINE_SHA_MARKER: &str = "__SSH_MCP_APPLY_FILE_EDIT_BASELINE_SHA__";
const APPLY_FILE_EDIT_CONFLICT_MARKER: &str = "__SSH_MCP_APPLY_FILE_EDIT_CONFLICT__";
const APPLY_FILE_EDIT_MISSING_SHA256: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ApplyFileEditFaultInjection {
    None,
    FailBeforeFinalize,
    Sha256Unavailable,
    PartialDeleteBeforeWrite,
    PartialMutateBeforeWrite,
}

#[derive(Debug)]
enum ApplyFileEditMode {
    Full {
        new_content: String,
    },
    Partial {
        old_text: String,
        new_text: String,
        replace_all: bool,
    },
}

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
    ticket_signer: Arc<TicketSigner>,
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
            ticket_signer: Arc::new(TicketSigner::new()),
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

    async fn execute_apply_file_edit(
        &self,
        params: ApplyFileEditParams,
        fault_injection: ApplyFileEditFaultInjection,
    ) -> std::result::Result<CallToolResult, McpError> {
        debug!(remote_path = ?params.remote_path, "apply-file-edit tool called");

        let ApplyFileEditParams {
            remote_path,
            new_content,
            old_text,
            new_text,
            replace_all,
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
        let mode_error = "apply-file-edit requires exactly one mode: provide new_content for full mode, or provide old_text and new_text for partial mode (replace_all is only valid in partial mode)";

        let edit_mode = match (new_content, old_text, new_text, replace_all) {
            (Some(new_content), None, None, None) => ApplyFileEditMode::Full { new_content },
            (None, Some(old_text), Some(new_text), replace_all) => ApplyFileEditMode::Partial {
                old_text,
                new_text,
                replace_all: replace_all.unwrap_or(false),
            },
            _ => return Err(McpError::invalid_params(mode_error, None)),
        };

        // ── Read-ticket enforcement ──────────────────────────────────────
        // Full mode: require a valid read_ticket when editing a non-empty
        // existing file. Partial mode reads the file internally, so the
        // precondition is implicitly satisfied.
        if let ApplyFileEditMode::Full { .. } = &edit_mode {
            match read_ticket {
                Some(ref ticket) => {
                    // Ticket provided — verify it matches the path.
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
                    // No ticket — check if file exists and is non-empty.
                    if self
                        .check_remote_file_nonempty(&remote_path, timeout)
                        .await?
                    {
                        return Err(McpError::invalid_params(
                            "Error: existing non-empty file must be read before editing. Call read-file first, then pass the returned read_ticket to apply-file-edit.",
                            None,
                        ));
                    }
                    // File is missing or empty — proceed with creation/overwrite.
                }
            }
        }

        let (next_content, partial_baseline_sha256) = match edit_mode {
            ApplyFileEditMode::Full { new_content } => (new_content, None),
            ApplyFileEditMode::Partial {
                old_text,
                new_text,
                replace_all,
            } => {
                if old_text.is_empty() {
                    return Err(McpError::invalid_params(
                        "old_text must not be empty in partial mode",
                        None,
                    ));
                }

                let read_result = self
                    .execute_read_file(ReadFileParams {
                        remote_path: remote_path.clone(),
                        mode: ReadFileMode::Full,
                        lines: None,
                        timeout_ms,
                    })
                    .await?;

                if read_result.is_error.unwrap_or(false) {
                    return Ok(read_result);
                }

                let read_text = extract_text_from_call_tool_result(&read_result);
                let read_value: serde_json::Value = serde_json::from_str(read_text.trim()).map_err(|e| {
                    McpError::internal_error(
                        format!(
                            "failed to parse read-file response while preparing partial apply-file-edit: {e}"
                        ),
                        None,
                    )
                })?;

                let current_content = read_value
                    .get("content")
                    .and_then(|value| value.as_str())
                    .ok_or_else(|| {
                        McpError::internal_error(
                            "read-file response missing content while preparing partial apply-file-edit"
                                .to_string(),
                            None,
                        )
                    })?;

                let match_count = current_content.matches(old_text.as_str()).count();
                if match_count == 0 {
                    return Ok(CallToolResult::error(vec![Content::text(
                        "Error: old_text was not found in remote file".to_string(),
                    )]));
                }

                if !replace_all && match_count != 1 {
                    return Ok(CallToolResult::error(vec![Content::text(format!(
                        "Error: old_text matched {match_count} times; set replace_all=true to replace all matches"
                    ))]));
                }

                let partial_baseline_sha256 = if user_expected_sha256.is_none() {
                    let baseline = match self
                        .compute_partial_baseline_sha256(current_content, timeout)
                        .await
                    {
                        Ok(value) => value,
                        Err(result) => return Ok(result),
                    };
                    Some(baseline)
                } else {
                    None
                };

                if let Err(result) = self
                    .apply_partial_fault_injection(remote_path.as_str(), timeout, fault_injection)
                    .await
                {
                    return Ok(result);
                }

                let updated_content = if replace_all {
                    current_content.replace(old_text.as_str(), new_text.as_str())
                } else {
                    current_content.replacen(old_text.as_str(), new_text.as_str(), 1)
                };

                (updated_content, partial_baseline_sha256)
            }
        };

        let expected_sha256 = user_expected_sha256.or(partial_baseline_sha256);

        self.execute_apply_file_edit_write_transaction(
            remote_path.as_str(),
            next_content.as_str(),
            expected_sha256,
            timeout,
            fault_injection,
        )
        .await
    }

    async fn compute_partial_baseline_sha256(
        &self,
        content: &str,
        timeout: Duration,
    ) -> std::result::Result<String, CallToolResult> {
        let hash_cmd = format!(
            r#"sh -c 'set -eu; sha256_stdin() {{ if command -v sha256sum >/dev/null 2>&1; then set -- $(sha256sum); printf "%s\n" "$1"; return 0; fi; if command -v shasum >/dev/null 2>&1; then set -- $(shasum -a 256); printf "%s\n" "$1"; return 0; fi; return 1; }}; if ! baseline_hash=$(sha256_stdin); then printf "%s\n" "{APPLY_FILE_EDIT_ERROR_MARKER}sha256_unavailable" >&2; exit 1; fi; printf "%s%s\n" "{APPLY_FILE_EDIT_BASELINE_SHA_MARKER}" "$baseline_hash" >&2'"#
        );

        let mut input = std::io::Cursor::new(content.as_bytes());
        let mut sink = tokio::io::sink();
        let out = match self
            .connection
            .exec_raw_streaming(&hash_cmd, Some(&mut input), Some(&mut sink), timeout)
            .await
        {
            Ok(output) => output,
            Err(e) => {
                return Err(CallToolResult::error(vec![Content::text(format!(
                    "Error: failed to hash partial baseline content on remote host: {e}"
                ))]));
            }
        };

        if let Some(marker) = parse_apply_file_edit_error_marker(&out.stderr)
            && marker == "sha256_unavailable"
        {
            return Err(CallToolResult::error(vec![Content::text(
                "Error: remote host does not provide SHA-256 utilities".to_string(),
            )]));
        }

        match out.exit_code {
            Some(0) => {}
            Some(code) => {
                let mut msg = format!(
                    "Error: failed to hash partial baseline content: remote command failed with exit_code={code}"
                );
                if let Some(snippet) = sanitize_read_file_stderr_snippet(&out.stderr) {
                    msg.push_str(&format!("; stderr={snippet}"));
                }
                return Err(CallToolResult::error(vec![Content::text(msg)]));
            }
            None => {
                if !out.stderr.trim().is_empty() {
                    let mut msg = "Error: failed to hash partial baseline content: remote command did not provide an exit status".to_string();
                    if let Some(snippet) = sanitize_read_file_stderr_snippet(&out.stderr) {
                        msg.push_str(&format!("; stderr={snippet}"));
                    }
                    return Err(CallToolResult::error(vec![Content::text(msg)]));
                }
            }
        }

        let baseline_raw = match parse_apply_file_edit_marker_value(
            &out.stderr,
            APPLY_FILE_EDIT_BASELINE_SHA_MARKER,
        ) {
            Some(value) => value,
            None => {
                return Err(CallToolResult::error(vec![Content::text(
                    "Error: missing partial baseline SHA-256 marker".to_string(),
                )]));
            }
        };

        match normalize_sha256_hex(baseline_raw, "partial_baseline_sha256") {
            Ok(value) => Ok(value),
            Err(_) => Err(CallToolResult::error(vec![Content::text(
                "Error: failed to parse remote SHA-256 output".to_string(),
            )])),
        }
    }

    async fn apply_partial_fault_injection(
        &self,
        remote_path: &str,
        timeout: Duration,
        fault_injection: ApplyFileEditFaultInjection,
    ) -> std::result::Result<(), CallToolResult> {
        let injected_cmd = match fault_injection {
            ApplyFileEditFaultInjection::PartialDeleteBeforeWrite => {
                let escaped = escape_for_shell(remote_path);
                Some(format!("sh -c 'set -eu; rm -f -- \"$1\"' sh '{escaped}'"))
            }
            ApplyFileEditFaultInjection::PartialMutateBeforeWrite => {
                let escaped = escape_for_shell(remote_path);
                Some(format!(
                    "sh -c 'set -eu; [ -f \"$1\" ]; printf \"__ssh_mcp_race_injected__\\n\" > \"$1\"' sh '{escaped}'"
                ))
            }
            _ => None,
        };

        let Some(injected_cmd) = injected_cmd else {
            return Ok(());
        };

        let mut empty_input = tokio::io::empty();
        let mut sink = tokio::io::sink();
        let out = match self
            .connection
            .exec_raw_streaming(
                &injected_cmd,
                Some(&mut empty_input),
                Some(&mut sink),
                timeout,
            )
            .await
        {
            Ok(output) => output,
            Err(e) => {
                return Err(CallToolResult::error(vec![Content::text(format!(
                    "Error: failed to run partial race fault injection: {e}"
                ))]));
            }
        };

        match out.exit_code {
            Some(0) => Ok(()),
            Some(code) => Err(CallToolResult::error(vec![Content::text(format!(
                "Error: partial race fault injection failed with exit_code={code}"
            ))])),
            None => Err(CallToolResult::error(vec![Content::text(
                "Error: partial race fault injection did not report exit status".to_string(),
            )])),
        }
    }

    /// Lightweight remote check: does the file exist and have size > 0?
    ///
    /// Returns `true` when the path is a regular file with at least one byte.
    /// Returns `false` for missing paths, non-files, and zero-byte files.
    async fn check_remote_file_nonempty(
        &self,
        remote_path: &str,
        timeout: Duration,
    ) -> std::result::Result<bool, McpError> {
        let escaped = escape_for_shell(remote_path);
        let cmd = format!(
            r#"sh -c 'if [ -f "$1" ] && [ -s "$1" ]; then printf "1"; else printf "0"; fi' sh '{escaped}'"#
        );
        let out = self
            .connection
            .exec_command(&cmd, timeout)
            .await
            .map_err(|e| {
                McpError::internal_error(format!("failed to check remote file status: {e}"), None)
            })?;
        Ok(out.stdout.trim() == "1")
    }

    async fn execute_apply_file_edit_write_transaction(
        &self,
        remote_path: &str,
        new_content: &str,
        expected_sha256: Option<String>,
        timeout: Duration,
        fault_injection: ApplyFileEditFaultInjection,
    ) -> std::result::Result<CallToolResult, McpError> {
        let bytes_written = new_content.len();
        if bytes_written > APPLY_FILE_EDIT_HARD_MAX_BYTES {
            return Ok(CallToolResult::error(vec![Content::text(
                apply_file_edit_too_large_error(APPLY_FILE_EDIT_HARD_MAX_BYTES),
            )]));
        }

        if let Err(e) = self.connection.ensure_connected().await {
            error!(error = ?e, "Failed to ensure SSH connection");
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "SSH connection error: {}",
                e
            ))]));
        }

        let local_tmp_rel = format!("target/tmp/apply-file-edit-{}.tmp", make_job_id());
        let local_tmp_path = self.transfer.local_root().join(&local_tmp_rel);

        if let Some(parent) = local_tmp_path.parent()
            && let Err(e) = tokio::fs::create_dir_all(parent).await
        {
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "Error: failed to create local staging directory: {e}"
            ))]));
        }

        let mut local_tmp_opts = tokio::fs::OpenOptions::new();
        local_tmp_opts.write(true).create_new(true);

        #[cfg(unix)]
        {
            local_tmp_opts.custom_flags(O_NOFOLLOW_FLAG);
        }

        let mut local_tmp_file = match local_tmp_opts.open(&local_tmp_path).await {
            Ok(file) => file,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error: failed to create local staging file: {e}"
                ))]));
            }
        };

        if let Err(e) = local_tmp_file.write_all(new_content.as_bytes()).await {
            let _ = tokio::fs::remove_file(&local_tmp_path).await;
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "Error: failed to write local staging file: {e}"
            ))]));
        }
        if let Err(e) = local_tmp_file.flush().await {
            let _ = tokio::fs::remove_file(&local_tmp_path).await;
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "Error: failed to flush local staging file: {e}"
            ))]));
        }
        if let Err(e) = local_tmp_file.sync_all().await {
            let _ = tokio::fs::remove_file(&local_tmp_path).await;
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "Error: failed to sync local staging file: {e}"
            ))]));
        }
        drop(local_tmp_file);

        let remote_lock_dir = format!("{}.ssh-mcp-lock", remote_path);
        let remote_stage_path = format!("{}.ssh-mcp-stage-{}", remote_path, make_job_id());
        let expected_for_remote = expected_sha256.clone().unwrap_or_else(|| "-".to_string());
        let missing_sha_for_remote = APPLY_FILE_EDIT_MISSING_SHA256;

        let dst_escaped = escape_for_shell(remote_path);
        let expected_escaped = escape_for_shell(&expected_for_remote);
        let missing_sha_escaped = escape_for_shell(missing_sha_for_remote);
        let lock_escaped = escape_for_shell(&remote_lock_dir);
        let stage_escaped = escape_for_shell(&remote_stage_path);
        let force_fail_before_finalize =
            if fault_injection == ApplyFileEditFaultInjection::FailBeforeFinalize {
                "1"
            } else {
                "0"
            };
        let force_sha256_unavailable =
            if fault_injection == ApplyFileEditFaultInjection::Sha256Unavailable {
                "1"
            } else {
                "0"
            };
        let force_fail_before_finalize_escaped = escape_for_shell(force_fail_before_finalize);
        let force_sha256_unavailable_escaped = escape_for_shell(force_sha256_unavailable);

        let apply_cmd = format!(
            r#"sh -c 'set -eu; dst=$1; expected=$2; lock_dir=$3; stage=$4; fail_before_finalize=$5; missing_sha=$6; force_sha_unavailable=$7; \
             drain_stdin() {{ cat > /dev/null || true; }}; \
             sha256_file() {{ file=$1; if command -v sha256sum >/dev/null 2>&1; then set -- $(sha256sum -- "$file"); printf "%s\n" "$1"; return 0; fi; if command -v shasum >/dev/null 2>&1; then set -- $(shasum -a 256 -- "$file"); printf "%s\n" "$1"; return 0; fi; return 1; }}; \
             parent=${{dst%/*}}; if [ -z "$parent" ]; then parent=/; fi; if [ ! -d "$parent" ]; then printf "%s\n" "{APPLY_FILE_EDIT_ERROR_MARKER}parent_not_found" >&2; drain_stdin; exit 1; fi; \
             lock_spins=0; while ! mkdir -- "$lock_dir" 2>/dev/null; do if [ -d "$lock_dir" ]; then lock_spins=$((lock_spins + 1)); if [ "$lock_spins" -ge 20 ]; then printf "%s\n" "{APPLY_FILE_EDIT_ERROR_MARKER}lock_busy" >&2; drain_stdin; exit 1; fi; sleep 1; continue; fi; printf "%s\n" "{APPLY_FILE_EDIT_ERROR_MARKER}lock_acquire_failed" >&2; drain_stdin; exit 1; done; \
             cleanup() {{ rm -f -- "$stage" 2>/dev/null || true; rmdir -- "$lock_dir" 2>/dev/null || true; }}; \
             trap cleanup EXIT INT TERM; \
             if [ "$force_sha_unavailable" = "1" ]; then printf "%s\n" "{APPLY_FILE_EDIT_ERROR_MARKER}sha256_unavailable" >&2; drain_stdin; exit 1; fi; \
             if ! sha256_file /dev/null >/dev/null 2>&1; then printf "%s\n" "{APPLY_FILE_EDIT_ERROR_MARKER}sha256_unavailable" >&2; drain_stdin; exit 1; fi; \
             if [ -e "$dst" ]; then if [ ! -f "$dst" ]; then printf "%s\n" "{APPLY_FILE_EDIT_ERROR_MARKER}not_regular_file" >&2; drain_stdin; exit 1; fi; if ! current_hash=$(sha256_file "$dst"); then printf "%s\n" "{APPLY_FILE_EDIT_ERROR_MARKER}sha256_unavailable" >&2; drain_stdin; exit 1; fi; else if [ "$expected" != "-" ]; then printf "%s%s\n" "{APPLY_FILE_EDIT_ACTUAL_SHA_MARKER}" "$missing_sha" >&2; printf "%s\n" "{APPLY_FILE_EDIT_CONFLICT_MARKER}" >&2; drain_stdin; exit 3; fi; current_hash=$missing_sha; fi; \
             printf "%s%s\n" "{APPLY_FILE_EDIT_PREVIOUS_SHA_MARKER}" "$current_hash" >&2; \
             if [ "$expected" != "-" ] && [ "$expected" != "$current_hash" ]; then printf "%s%s\n" "{APPLY_FILE_EDIT_ACTUAL_SHA_MARKER}" "$current_hash" >&2; printf "%s\n" "{APPLY_FILE_EDIT_CONFLICT_MARKER}" >&2; drain_stdin; exit 3; fi; \
             if ! : > "$stage" 2>/dev/null; then printf "%s\n" "{APPLY_FILE_EDIT_ERROR_MARKER}staging_unwritable" >&2; drain_stdin; exit 1; fi; \
             if ! cat > "$stage"; then printf "%s\n" "{APPLY_FILE_EDIT_ERROR_MARKER}stage_write_failed" >&2; exit 1; fi; \
             if [ "$fail_before_finalize" = "1" ]; then printf "%s\n" "{APPLY_FILE_EDIT_ERROR_MARKER}finalize_failed" >&2; exit 1; fi; \
             if ! mv -- "$stage" "$dst"; then printf "%s\n" "{APPLY_FILE_EDIT_ERROR_MARKER}finalize_failed" >&2; exit 1; fi; \
             if ! new_hash=$(sha256_file "$dst"); then printf "%s\n" "{APPLY_FILE_EDIT_ERROR_MARKER}sha256_unavailable" >&2; exit 1; fi; \
             printf "%s%s\n" "{APPLY_FILE_EDIT_NEW_SHA_MARKER}" "$new_hash" >&2; \
             trap - EXIT INT TERM; cleanup' sh '{dst_escaped}' '{expected_escaped}' '{lock_escaped}' '{stage_escaped}' '{force_fail_before_finalize_escaped}' '{missing_sha_escaped}' '{force_sha256_unavailable_escaped}'"#
        );

        let mut local_tmp_input = match tokio::fs::File::open(&local_tmp_path).await {
            Ok(file) => file,
            Err(e) => {
                let _ = tokio::fs::remove_file(&local_tmp_path).await;
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error: failed to open local staging file for upload: {e}"
                ))]));
            }
        };
        let mut sink = tokio::io::sink();
        let out = match self
            .connection
            .exec_raw_streaming(
                &apply_cmd,
                Some(&mut local_tmp_input),
                Some(&mut sink),
                timeout,
            )
            .await
        {
            Ok(output) => output,
            Err(e) => {
                let _ = tokio::fs::remove_file(&local_tmp_path).await;
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error applying file edit: {e}"
                ))]));
            }
        };

        let _ = tokio::fs::remove_file(&local_tmp_path).await;

        if has_apply_file_edit_conflict_marker(&out.stderr) {
            let expected = match expected_sha256 {
                Some(value) => value,
                None => {
                    return Ok(CallToolResult::error(vec![Content::text(
                        "Error: apply-file-edit conflict marker was returned without expected_sha256"
                            .to_string(),
                    )]));
                }
            };

            let actual_raw =
                parse_apply_file_edit_marker_value(&out.stderr, APPLY_FILE_EDIT_ACTUAL_SHA_MARKER)
                    .or_else(|| {
                        parse_apply_file_edit_marker_value(
                            &out.stderr,
                            APPLY_FILE_EDIT_PREVIOUS_SHA_MARKER,
                        )
                    });
            let actual_sha256 = match actual_raw {
                Some(value) => match normalize_sha256_hex(value, "actual_sha256") {
                    Ok(normalized) => normalized,
                    Err(_) => {
                        return Ok(CallToolResult::error(vec![Content::text(
                            "Error: failed to parse remote SHA-256 output".to_string(),
                        )]));
                    }
                },
                None => {
                    return Ok(CallToolResult::error(vec![Content::text(
                        "Error: apply-file-edit conflict response missing actual_sha256"
                            .to_string(),
                    )]));
                }
            };

            let conflict = serde_json::json!({
                "error": "conflict",
                "path": remote_path,
                "expected_sha256": expected,
                "actual_sha256": actual_sha256,
            });
            return Ok(CallToolResult::error(vec![Content::text(
                conflict.to_string(),
            )]));
        }

        if let Some(marker) = parse_apply_file_edit_error_marker(&out.stderr) {
            let msg = match marker {
                "not_found" => "Error: remote_path does not exist".to_string(),
                "parent_not_found" => "Error: remote parent directory does not exist".to_string(),
                "not_regular_file" => "Error: remote_path is not a regular file".to_string(),
                "sha256_unavailable" => {
                    "Error: remote host does not provide SHA-256 utilities".to_string()
                }
                "lock_busy" => {
                    "Error: remote_path is being edited by another operation; retry shortly"
                        .to_string()
                }
                "lock_acquire_failed" => {
                    "Error: failed to acquire remote apply-file-edit lock due to filesystem error"
                        .to_string()
                }
                "staging_unwritable" => {
                    "Error: failed to create remote staging file in destination directory"
                        .to_string()
                }
                "stage_write_failed" => "Error: failed to write remote staging file".to_string(),
                "finalize_failed" => "Error: failed to atomically replace remote file".to_string(),
                _ => "Error: apply-file-edit failed on remote host".to_string(),
            };
            return Ok(CallToolResult::error(vec![Content::text(msg)]));
        }

        match out.exit_code {
            Some(0) => {}
            Some(code) => {
                let mut msg = format!(
                    "Error applying file edit: remote command failed with exit_code={code}"
                );
                if let Some(snippet) = sanitize_read_file_stderr_snippet(&out.stderr) {
                    msg.push_str(&format!("; stderr={snippet}"));
                }
                return Ok(CallToolResult::error(vec![Content::text(msg)]));
            }
            None => {
                if !out.stderr.trim().is_empty() {
                    let mut msg =
                        "Error applying file edit: remote command did not provide an exit status"
                            .to_string();
                    if let Some(snippet) = sanitize_read_file_stderr_snippet(&out.stderr) {
                        msg.push_str(&format!("; stderr={snippet}"));
                    }
                    return Ok(CallToolResult::error(vec![Content::text(msg)]));
                }
            }
        }

        let previous_sha_raw = match parse_apply_file_edit_marker_value(
            &out.stderr,
            APPLY_FILE_EDIT_PREVIOUS_SHA_MARKER,
        ) {
            Some(value) => value,
            None => {
                return Ok(CallToolResult::error(vec![Content::text(
                    "Error: missing previous_sha256 marker from apply-file-edit response"
                        .to_string(),
                )]));
            }
        };
        let previous_sha256 = match normalize_sha256_hex(previous_sha_raw, "previous_sha256") {
            Ok(normalized) => normalized,
            Err(_) => {
                return Ok(CallToolResult::error(vec![Content::text(
                    "Error: failed to parse remote SHA-256 output".to_string(),
                )]));
            }
        };

        let new_sha_raw =
            match parse_apply_file_edit_marker_value(&out.stderr, APPLY_FILE_EDIT_NEW_SHA_MARKER) {
                Some(value) => value,
                None => {
                    return Ok(CallToolResult::error(vec![Content::text(
                        "Error: missing new_sha256 marker from apply-file-edit response"
                            .to_string(),
                    )]));
                }
            };
        let new_sha256 = match normalize_sha256_hex(new_sha_raw, "new_sha256") {
            Ok(normalized) => normalized,
            Err(_) => {
                return Ok(CallToolResult::error(vec![Content::text(
                    "Error: failed to parse remote SHA-256 output".to_string(),
                )]));
            }
        };

        let changed = previous_sha256 != new_sha256;
        let result = serde_json::json!({
            "path": remote_path,
            "previous_sha256": previous_sha256,
            "new_sha256": new_sha256,
            "bytes_written": bytes_written,
            "changed": changed,
        });

        Ok(CallToolResult::success(vec![Content::text(
            result.to_string(),
        )]))
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

    /// Build read-file tool definition
    fn read_file_tool() -> Tool {
        tools::read_file_tool()
    }

    /// Build apply-file-edit tool definition
    fn apply_file_edit_tool() -> Tool {
        tools::apply_file_edit_tool()
    }

    /// Get extended documentation for a tool by name
    ///
    /// Returns the full documentation text that was removed from compact tool definitions
    /// to save tokens in the MCP protocol.
    pub fn get_tool_documentation(tool_name: &str) -> Option<&'static str> {
        tools::get_tool_documentation(tool_name)
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

        // Docs/expected order: exec, (optional) sudo-exec, check-process, transfer, read-file, apply-file-edit.
        if !self.config.disable_sudo {
            tools.push(Self::sudo_exec_tool());
        }
        tools.push(Self::check_process_tool());
        tools.push(Self::transfer_tool());
        tools.push(Self::read_file_tool());
        tools.push(Self::apply_file_edit_tool());

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
            "read-file" | "read_file" => {
                let params: ReadFileParams =
                    serde_json::from_value(serde_json::Value::Object(args)).map_err(|e| {
                        McpError::invalid_params(format!("invalid read-file params: {e}"), None)
                    })?;

                self.execute_read_file(params).await
            }
            "apply-file-edit" | "apply_file_edit" => {
                let params: ApplyFileEditParams =
                    serde_json::from_value(serde_json::Value::Object(args)).map_err(|e| {
                        McpError::invalid_params(
                            format!("invalid apply-file-edit params: {e}"),
                            None,
                        )
                    })?;

                self.execute_apply_file_edit(params, ApplyFileEditFaultInjection::None)
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
    fn test_read_file_tool_definition() {
        let tool = SshMcpServer::read_file_tool();
        assert_eq!(tool.name.as_ref(), "read-file");
        assert!(tool.description.is_some());
    }

    #[test]
    fn test_apply_file_edit_tool_definition() {
        let tool = SshMcpServer::apply_file_edit_tool();
        assert_eq!(tool.name.as_ref(), "apply-file-edit");
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
    fn test_normalize_sha256_hex_accepts_uppercase_input() {
        let input = "AABBCCDDEEFF00112233445566778899AABBCCDDEEFF00112233445566778899";
        let normalized = normalize_sha256_hex(input, "expected_sha256").unwrap();
        assert_eq!(
            normalized,
            "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899"
        );
    }

    #[test]
    fn test_normalize_sha256_hex_rejects_invalid_length() {
        let err = normalize_sha256_hex("abcd", "expected_sha256").unwrap_err();
        assert!(err.contains("64-character"));
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
    fn test_apply_read_file_window_preview_truncates_and_sets_hint() {
        let text = "line1\nline2\nline3\nline4\n";
        let window = apply_read_file_window(text, ReadFileMode::Preview, Some(2));
        assert_eq!(window.content, "line1\nline2\n");
        assert_eq!(window.returned_lines, 2);
        assert!(window.truncated);
        assert!(window.hint.is_some());
    }

    #[test]
    fn test_apply_read_file_window_tail_returns_last_lines() {
        let text = "line1\nline2\nline3\nline4\n";
        let window = apply_read_file_window(text, ReadFileMode::Tail, Some(2));
        assert_eq!(window.content, "line3\nline4\n");
        assert_eq!(window.returned_lines, 2);
        assert!(window.truncated);
        assert!(window.hint.is_some());
    }

    #[test]
    fn test_apply_read_file_window_full_returns_all_content_without_hint() {
        let text = "line1\nline2\nline3\n";
        let window = apply_read_file_window(text, ReadFileMode::Full, Some(1));
        assert_eq!(window.content, text);
        assert_eq!(window.returned_lines, 3);
        assert!(!window.truncated);
        assert!(window.hint.is_none());
    }

    #[test]
    fn test_sanitize_read_file_stderr_snippet_normalizes_whitespace_and_controls() {
        let stderr = "line1\nline2\t\u{0007}bad\rline3";
        let snippet = sanitize_read_file_stderr_snippet(stderr)
            .expect("snippet should be present for non-empty stderr");
        assert_eq!(snippet, "line1 line2 bad line3");
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
        assert!(SshMcpServer::get_tool_documentation("read-file").is_some());
        assert!(SshMcpServer::get_tool_documentation("apply-file-edit").is_some());
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
    fn test_read_file_documentation_content() {
        let docs = SshMcpServer::get_tool_documentation("read-file").unwrap();
        assert!(docs.contains("READ-FILE TOOL"));
        assert!(docs.contains("remote_path"));
        assert!(docs.contains("mode"));
        assert!(docs.contains("UTF-8"));
    }

    #[test]
    fn test_apply_file_edit_documentation_content() {
        let docs = SshMcpServer::get_tool_documentation("apply-file-edit").unwrap();
        assert!(docs.contains("APPLY-FILE-EDIT TOOL"));
        assert!(docs.contains("expected_sha256"));
        assert!(docs.contains("atomic"));
    }

    #[test]
    fn test_compact_tool_descriptions() {
        // Verify that tool descriptions are compact (not verbose)
        let exec = SshMcpServer::exec_tool();
        let sudo_exec = SshMcpServer::sudo_exec_tool();
        let transfer = SshMcpServer::transfer_tool();
        let read_file = SshMcpServer::read_file_tool();
        let apply_file_edit = SshMcpServer::apply_file_edit_tool();

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
        if let Some(desc) = read_file.description {
            assert!(
                desc.len() < 100,
                "read-file description too long: {} chars",
                desc.len()
            );
        }
        if let Some(desc) = apply_file_edit.description {
            assert!(
                desc.len() < 100,
                "apply-file-edit description too long: {} chars",
                desc.len()
            );
        }
    }
}
