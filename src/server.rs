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

use crate::background::job::NewRunningJob;
use crate::background::{JobRegistry, JobState, LocalLogSpooler, OutputStreamer, SharedJobState};
use crate::config::Config;
use crate::error::{Result, SshMcpError};
#[cfg(unix)]
use crate::platform::O_NOFOLLOW_FLAG;
use crate::ssh::{
    CommandOutput, SshConfig, SshConnectionManager, sanitize_command, wrap_sudo_command,
};
use crate::tools::CheckProcessParams;
use crate::transfer::{TransferEngine, TransferParams, TransferRunContext, TransferSshOptions};

const BACKGROUND_START_TIMEOUT: Duration = Duration::from_secs(20);
const BACKGROUND_JSON_SNIPPET_LIMIT_CHARS: usize = 2048;

const JOB_COMPLETED_RETENTION: Duration = Duration::from_secs(60 * 60);

static JOB_COUNTER: AtomicU64 = AtomicU64::new(0);

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetachMode {
    Unknown = 0,
    Full = 1,
    Portable = 2,
    DirectOnly = 3,
}

impl DetachMode {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Full,
            2 => Self::Portable,
            3 => Self::DirectOnly,
            _ => Self::Unknown,
        }
    }

    fn as_u8(self) -> u8 {
        self as u8
    }
}

#[derive(Debug, Clone)]
struct CommonToolArgs {
    command: String,
    background: bool,
    timeout_ms: Option<u64>,
    log_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BackgroundMarkers {
    job_id: String,
    pid: u32,
    remote_log_path: String,
}

fn remote_job_log_path(job_id: &str) -> String {
    // Transitional behavior:
    // - Kept for API compatibility (exec/sudo-exec responses may still include remote_log_path).
    // - Current versions serve logs from local spool files on the MCP server.
    format!("/tmp/.ssh-mcp-job-{job_id}.log")
}

fn make_job_id() -> String {
    let counter = JOB_COUNTER.fetch_add(1, Ordering::Relaxed);
    let epoch_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{}-{}", epoch_ms, counter)
}

fn build_background_wrapper_script_full(
    job_id: &str,
    user_command: &str,
    log_path: &str,
) -> String {
    // The wrapper itself may be nested inside another `sh -lc '...'`.
    // Escape only for single-quoted contexts inside this wrapper.
    let escaped_user_command = crate::ssh::escape_for_shell(user_command);
    let escaped_log_path = crate::ssh::escape_for_shell(log_path);

    // Emit markers first, then `exec` the user command.
    // The wrapper PID becomes the command PID after `exec`.
    format!(
        "LOG='{escaped_log_path}'; \
  printf '%s\n' \"__SSH_MCP_JOB_ID={job_id}\"; \
  printf '%s\n' \"__SSH_MCP_PID=$$\"; \
  printf '%s\n' \"__SSH_MCP_LOG=$LOG\"; \
  exec sh -lc 'set +m; {escaped_user_command}'",
    )
}

fn build_background_wrapper_script_portable(
    job_id: &str,
    user_command: &str,
    log_path: &str,
) -> String {
    // The wrapper itself may be nested inside another `sh -lc '...'`.
    // Escape only for single-quoted contexts inside this wrapper.
    let escaped_user_command = crate::ssh::escape_for_shell(user_command);
    let escaped_log_path = crate::ssh::escape_for_shell(log_path);

    // BusyBox-friendly: avoid `sh -l`.
    // Emit markers first, then `exec` the user command.
    format!(
        "LOG='{escaped_log_path}'; \
  printf '%s\n' \"__SSH_MCP_JOB_ID={job_id}\"; \
  printf '%s\n' \"__SSH_MCP_PID=$$\"; \
  printf '%s\n' \"__SSH_MCP_LOG=$LOG\"; \
  exec sh -c 'set +m; {escaped_user_command}'",
    )
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

async fn read_background_markers_from_channel(
    channel: &mut russh::Channel<russh::client::Msg>,
    expected_job_id: &str,
    expected_log_path: &str,
    timeout_duration: Duration,
) -> std::result::Result<(BackgroundMarkers, Vec<u8>), String> {
    let mut stdout_buf: Vec<u8> = Vec::with_capacity(256);
    let mut marker_stdout = String::new();
    let mut parsed_lines = 0usize;
    let mut line_start = 0usize;

    let fut = async {
        while parsed_lines < 3 {
            let Some(msg) = channel.wait().await else {
                return Err("channel ended before background markers".to_string());
            };

            match msg {
                russh::ChannelMsg::Data { data } => {
                    stdout_buf.extend_from_slice(data.as_ref());

                    while parsed_lines < 3 {
                        let Some(rel_nl) =
                            stdout_buf[line_start..].iter().position(|b| *b == b'\n')
                        else {
                            break;
                        };

                        let nl = line_start.saturating_add(rel_nl);
                        let line_bytes = &stdout_buf[line_start..nl];
                        let line = std::str::from_utf8(line_bytes)
                            .map_err(|e| format!("invalid UTF-8 in marker stream: {e}"))?;
                        marker_stdout.push_str(line);
                        marker_stdout.push('\n');

                        parsed_lines = parsed_lines.saturating_add(1);
                        line_start = nl.saturating_add(1);
                    }
                }
                russh::ChannelMsg::ExtendedData { data, .. } => {
                    let snippet = String::from_utf8_lossy(data.as_ref());
                    let snippet: String = snippet.chars().take(256).collect();
                    return Err(format!(
                        "unexpected stderr while reading background markers: {snippet}"
                    ));
                }
                russh::ChannelMsg::ExitStatus { exit_status } => {
                    return Err(format!(
                        "channel exited before background markers (exit_status={exit_status})"
                    ));
                }
                russh::ChannelMsg::Close | russh::ChannelMsg::Eof => {
                    // Keep reading: ExitStatus may still arrive.
                }
                _ => {}
            }
        }

        let markers = parse_background_markers(&marker_stdout, expected_job_id, expected_log_path)
            .map_err(|e| format!("failed to parse background markers: {e}"))?;

        let remaining = if line_start < stdout_buf.len() {
            stdout_buf.split_off(line_start)
        } else {
            Vec::new()
        };
        Ok((markers, remaining))
    };

    match tokio::time::timeout(timeout_duration, fut).await {
        Ok(r) => r,
        Err(_) => Err(format!(
            "timed out waiting for background markers after {}ms",
            timeout_duration.as_millis()
        )),
    }
}

fn validate_background_log_path(
    base_dir: &Path,
    log_path: &str,
) -> std::result::Result<(), String> {
    if log_path.is_empty() {
        return Err("log_path cannot be empty".to_string());
    }

    if log_path != log_path.trim() {
        return Err("log_path must not have leading/trailing whitespace".to_string());
    }

    // Be explicit for readability even though these are control chars.
    if log_path.contains(['\n', '\r']) {
        return Err("log_path must not contain newlines".to_string());
    }

    // Without GNU `cat --`, a path starting with '-' may be treated as an option.
    // Disallow this for user-provided paths.
    if log_path.starts_with('-') {
        return Err("log_path must not start with '-'.".to_string());
    }

    if log_path.chars().any(|c| c.is_control()) {
        return Err("log_path must not contain control characters".to_string());
    }

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

fn select_detach_mode(full_supported: bool, portable_supported: bool) -> DetachMode {
    if full_supported {
        DetachMode::Full
    } else if portable_supported {
        DetachMode::Portable
    } else {
        DetachMode::DirectOnly
    }
}

fn parse_background_markers(
    stdout: &str,
    expected_job_id: &str,
    expected_log_path: &str,
) -> std::result::Result<BackgroundMarkers, String> {
    let mut job_id: Option<String> = None;
    let mut pid: Option<u32> = None;
    let mut log_path: Option<String> = None;

    for raw_line in stdout.lines() {
        let line = raw_line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("__SSH_MCP_JOB_ID=") {
            if job_id.is_some() {
                return Err("Duplicate __SSH_MCP_JOB_ID marker".to_string());
            }
            job_id = Some(rest.to_string());
            continue;
        }
        if let Some(rest) = line.strip_prefix("__SSH_MCP_PID=") {
            if pid.is_some() {
                return Err("Duplicate __SSH_MCP_PID marker".to_string());
            }
            let parsed_pid: u32 = rest
                .parse()
                .map_err(|e| format!("Invalid pid marker value '{rest}': {e}"))?;
            pid = Some(parsed_pid);
            continue;
        }
        if let Some(rest) = line.strip_prefix("__SSH_MCP_LOG=") {
            if log_path.is_some() {
                return Err("Duplicate __SSH_MCP_LOG marker".to_string());
            }
            log_path = Some(rest.to_string());
            continue;
        }
    }

    let job_id = job_id.ok_or_else(|| "Missing __SSH_MCP_JOB_ID marker".to_string())?;
    let pid = pid.ok_or_else(|| "Missing __SSH_MCP_PID marker".to_string())?;
    let log_path = log_path.ok_or_else(|| "Missing __SSH_MCP_LOG marker".to_string())?;

    if pid == 0 {
        return Err("Invalid pid marker value '0'".to_string());
    }

    if job_id != expected_job_id {
        return Err(format!(
            "Unexpected job id marker value '{job_id}', expected '{expected_job_id}'"
        ));
    }

    if log_path != expected_log_path {
        return Err(format!(
            "Unexpected log path marker value '{log_path}', expected '{expected_log_path}'"
        ));
    }

    Ok(BackgroundMarkers {
        job_id,
        pid,
        remote_log_path: log_path,
    })
}

fn truncate_with_flag(input: &str, limit_chars: usize) -> (String, bool) {
    let mut iter = input.chars();
    let snippet: String = iter.by_ref().take(limit_chars).collect();
    let truncated = iter.next().is_some();
    (snippet, truncated)
}

fn background_json_ok(
    job_id: &str,
    pid: u32,
    local_log_path: &str,
    remote_log_path: &str,
) -> CallToolResult {
    let body = serde_json::json!({
        "ok": true,
        "background": true,
        "job_id": job_id,
        "pid": pid,
        "log_path": local_log_path,
        "remote_log_path": remote_log_path,
    })
    .to_string();

    CallToolResult::success(vec![Content::text(body)])
}

fn background_json_timeout(
    job_id: &str,
    pid: u32,
    local_log_path: &str,
    remote_log_path: &str,
) -> CallToolResult {
    let hint = format!(
        "TIMEOUT_RECOVERY: Process still running in background. DO NOT restart the command! Use check-process tool with job_id={job_id} to retrieve output."
    );
    let body = serde_json::json!({
        "ok": false,
        "timeout": true,
        "background": true,
        "job_id": job_id,
        "pid": pid,
        "log_path": local_log_path,
        "remote_log_path": remote_log_path,
        "hint": hint,
    })
    .to_string();

    CallToolResult::success(vec![Content::text(body)])
}

fn background_json_err(
    job_id: &str,
    local_log_path: &str,
    remote_log_path: Option<&str>,
    error: &str,
    stderr: &str,
) -> CallToolResult {
    // Keep the payload deterministic and single-line. Avoid echoing the original command.
    let (error_snippet, error_truncated) =
        truncate_with_flag(error, BACKGROUND_JSON_SNIPPET_LIMIT_CHARS);
    let (stderr_snippet, stderr_truncated) =
        truncate_with_flag(stderr, BACKGROUND_JSON_SNIPPET_LIMIT_CHARS);

    let truncated = error_truncated || stderr_truncated;

    let mut obj = serde_json::Map::new();
    obj.insert("ok".to_string(), serde_json::Value::Bool(false));
    obj.insert("background".to_string(), serde_json::Value::Bool(true));
    obj.insert(
        "job_id".to_string(),
        serde_json::Value::String(job_id.to_string()),
    );
    obj.insert(
        "log_path".to_string(),
        serde_json::Value::String(local_log_path.to_string()),
    );
    if let Some(remote) = remote_log_path {
        obj.insert(
            "remote_log_path".to_string(),
            serde_json::Value::String(remote.to_string()),
        );
    }
    obj.insert(
        "error".to_string(),
        serde_json::Value::String(error_snippet),
    );
    obj.insert(
        "stderr".to_string(),
        serde_json::Value::String(stderr_snippet),
    );
    obj.insert("truncated".to_string(), serde_json::Value::Bool(truncated));
    obj.insert(
        "truncated_fields".to_string(),
        serde_json::json!({
            "error": error_truncated,
            "stderr": stderr_truncated,
        }),
    );
    if truncated {
        obj.insert(
            "hint".to_string(),
                serde_json::Value::String(format!(
                "Response fields were truncated to {} chars. Hint: inspect full output using log_path or check-process with job_id={job_id}.",
                BACKGROUND_JSON_SNIPPET_LIMIT_CHARS
            )),
        );
    }

    let body = serde_json::Value::Object(obj).to_string();

    CallToolResult::success(vec![Content::text(body)])
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

/// Extended documentation for tools (available on-demand to save tokens in tool definitions)
pub mod tool_docs {
    /// Documentation for the exec tool
    pub const EXEC: &str = r#"EXEC TOOL
Execute shell commands on remote SSH server.

PARAMETERS:
- command (string, required): Shell command to execute
- background (boolean): Run in background. Returns immediately with {job_id,pid,log_path}.
  Output is streamed to local log file on MCP server. Monitor via check-process using job_id.
  (+ remote_log_path deprecated)
- timeout_ms (integer): Wait timeout in milliseconds (ignored if background=true)
- log_path (string): Custom LOCAL log path for background mode (must be under /tmp/ssh-mcp)

BACKGROUND MODE:
For commands longer than RPC timeout, use background=true:
1. Command runs detached on the remote host
2. Returns immediately with job_id, pid, LOCAL log_path on the MCP server
3. Monitor: use check-process with job_id (preferred) or ps -p <pid> -o pid,etime,cmd
4. View output: use check-process with job_id; or tail -n 50 '<log_path>' (local spool file)

NOTE:
- remote_log_path is kept for backward compatibility only (deprecated) and will be removed in a future version.

EXAMPLE:
{"command": "apt update && apt install -y nginx", "background": true}"#;

    /// Documentation for the sudo-exec tool  
    pub const SUDO_EXEC: &str = r#"SUDO-EXEC TOOL
Execute shell commands with sudo privileges.

Same parameters and behavior as exec tool, but runs with sudo.
Requires passwordless sudo or pre-configured sudo password.

PARAMETERS:
- command (string, required): Shell command to execute with sudo
- background (boolean): Run in background. Returns immediately with {job_id,pid,log_path}.
  Output is streamed to local log file on MCP server. Monitor via check-process using job_id.
- timeout_ms (integer): Wait timeout
- log_path (string): Custom LOCAL log path (must be under /tmp/ssh-mcp)

EXAMPLE:
{"command": "systemctl restart nginx", "background": false}"#;

    /// Documentation for the transfer tool
    pub const TRANSFER: &str = r#"TRANSFER TOOL
Transfer files or directories between local and remote hosts.

PARAMETERS:
- operation (string, required): "put" (local→remote) or "get" (remote→local)
- local_path (string, required): Local file path (relative to local_root or absolute path within local_root)
- remote_path (string, required): Absolute remote path
- transport (string): "auto" (default), "sftp", "scp", "rsync", or "exec-raw"
- kind (string): "file" or "directory" (auto-detected if omitted)
- overwrite (boolean): Allow overwriting destination (default: false)
- timeout_ms (integer): Transfer timeout override

TRANSPORTS:
- auto: Tries rsync → sftp → scp → exec-raw in order
- sftp/scp/rsync: Require local OpenSSH binaries and --key
- exec-raw: Streaming via SSH exec (no OpenSSH needed)

SAFETY:
- local_path resolved within local_root (prevents ../ attacks)
- remote_path rejects paths starting with '-' or containing NUL

EXAMPLE:
{"operation": "put", "local_path": "config.yml", "remote_path": "/etc/app/config.yml"}"#;
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
        let cached = DetachMode::from_u8(self.detach_mode.load(Ordering::Acquire));
        if cached != DetachMode::Unknown {
            return cached;
        }

        let _guard = self.detach_mode_lock.lock().await;
        let cached = DetachMode::from_u8(self.detach_mode.load(Ordering::Acquire));
        if cached != DetachMode::Unknown {
            return cached;
        }

        let full_supported = match self.probe_detach_mode(DetachMode::Full).await {
            Ok(true) => true,
            Ok(false) => false,
            Err(e) => {
                debug!("full detach probe failed: {e}");
                false
            }
        };

        let portable_supported = if full_supported {
            false
        } else {
            match self.probe_detach_mode(DetachMode::Portable).await {
                Ok(true) => true,
                Ok(false) => false,
                Err(e) => {
                    debug!("portable detach probe failed: {e}");
                    false
                }
            }
        };

        let selected = select_detach_mode(full_supported, portable_supported);
        // Cache a non-Unknown decision to avoid repeated probes (and /tmp litter).
        self.detach_mode.store(selected.as_u8(), Ordering::Release);
        selected
    }

    async fn probe_detach_mode(&self, mode: DetachMode) -> Result<bool> {
        if matches!(mode, DetachMode::Unknown | DetachMode::DirectOnly) {
            return Ok(false);
        }

        let job_id = make_job_id();
        let marker = format!("__SSH_MCP_AUTODETECT_OK={job_id}");
        let probe_command = format!("printf '%s\\n' \"{marker}\"");

        let (log_path_buf, _log_path_str) = self
            .default_local_log_path(&format!("autodetect-{job_id}"))
            .map_err(SshMcpError::config)?;
        self.ensure_local_log_file(&log_path_buf).await?;

        let remote_log_path = remote_job_log_path(&job_id);
        let wrapper =
            build_background_wrapper_script(mode, &job_id, &probe_command, &remote_log_path);

        let start_output = self
            .connection
            .exec_command(&wrapper, Duration::from_secs(5))
            .await?;

        let markers = match parse_background_markers(
            &start_output.stdout,
            &job_id,
            &remote_log_path,
        ) {
            Ok(m) => m,
            Err(parse_err) => {
                debug!(
                    "detach probe markers parse failed ({mode:?}): {parse_err}; exit_code={:?}; stderr_len={}",
                    start_output.exit_code,
                    start_output.stderr.len()
                );
                return Ok(false);
            }
        };

        Ok(start_output.exit_code == Some(0)
            && start_output.stderr.is_empty()
            && markers.remote_log_path == remote_log_path
            && start_output.stdout.contains(&marker))
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
            error!("Failed to ensure SSH connection: {}", e);
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "SSH connection error: {}",
                e
            ))]));
        }

        // If su elevation is configured and available, ensure we're elevated
        if self.connection.get_su_password().is_some()
            && let Err(e) = self.connection.ensure_elevated().await
        {
            debug!("Elevation failed, will run as normal user: {}", e);
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
                    error!("Command execution failed: {}", e);
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

        let permit = match self
            .connection
            .channel_semaphore
            .clone()
            .acquire_owned()
            .await
        {
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

        if let Err(e) = channel.exec(true, wrapper.as_str()).await {
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
                    error!("streaming failed for job {}: {}", job_id, e);
                    return Ok(CallToolResult::error(vec![Content::text(format!(
                        "Error: streaming failed: {e}"
                    ))]));
                }
                Err(e) => {
                    error!("streaming task join failed for job {}: {}", job_id, e);
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
        let job_id = make_job_id();
        let remote_log_path = remote_job_log_path(&job_id);

        let (final_log_path_buf, final_log_path) = match log_path {
            Some(p) => (PathBuf::from(p), p.to_string()),
            None => match self.default_local_log_path(&job_id) {
                Ok(v) => v,
                Err(e) => {
                    return Ok(background_json_err(
                        &job_id,
                        "",
                        Some(&remote_log_path),
                        &e,
                        "",
                    ));
                }
            },
        };

        if let Err(e) = self.ensure_local_log_file(&final_log_path_buf).await {
            return Ok(background_json_err(
                &job_id,
                &final_log_path,
                Some(&remote_log_path),
                &e.to_string(),
                "",
            ));
        }

        // Sanitize the command
        let sanitized = match sanitize_command(command, self.max_chars) {
            Ok(cmd) => cmd,
            Err(e) => {
                return Ok(background_json_err(
                    &job_id,
                    &final_log_path,
                    Some(&remote_log_path),
                    &e.to_string(),
                    "",
                ));
            }
        };

        // Ensure connection is established
        if let Err(e) = self.connection.ensure_connected().await {
            return Ok(background_json_err(
                &job_id,
                &final_log_path,
                Some(&remote_log_path),
                &format!("SSH connection error: {}", e),
                "",
            ));
        }

        // If su elevation is configured and available, ensure we're elevated (best-effort)
        if self.connection.get_su_password().is_some()
            && let Err(e) = self.connection.ensure_elevated().await
        {
            debug!("Elevation failed, will run as normal user: {}", e);
        }

        let detach_mode = self.determine_detach_mode().await;
        if detach_mode == DetachMode::DirectOnly {
            return Ok(background_json_err(
                &job_id,
                &final_log_path,
                Some(&remote_log_path),
                "Background detach is not supported on this target; run with background=false.",
                "",
            ));
        }

        let wrapper =
            build_background_wrapper_script(detach_mode, &job_id, &sanitized, &remote_log_path);

        let permit = match self
            .connection
            .channel_semaphore
            .clone()
            .acquire_owned()
            .await
        {
            Ok(p) => p,
            Err(e) => {
                return Ok(background_json_err(
                    &job_id,
                    &final_log_path,
                    Some(&remote_log_path),
                    &format!("Failed to acquire command slot: {e}"),
                    "",
                ));
            }
        };

        let mut channel = match self.connection.open_channel().await {
            Ok(ch) => ch,
            Err(e) => {
                return Ok(background_json_err(
                    &job_id,
                    &final_log_path,
                    Some(&remote_log_path),
                    &e.to_string(),
                    "",
                ));
            }
        };

        if let Err(e) = channel.exec(true, wrapper.as_str()).await {
            return Ok(background_json_err(
                &job_id,
                &final_log_path,
                Some(&remote_log_path),
                &format!("Failed to exec background wrapper: {e}"),
                "",
            ));
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
                return Ok(background_json_err(
                    &job_id,
                    &final_log_path,
                    Some(&remote_log_path),
                    &e,
                    "",
                ));
            }
        };

        self.register_running_job(&job_id, markers.pid, final_log_path_buf.clone(), &sanitized)
            .await;

        let streamer = OutputStreamer::new(
            job_id.clone(),
            final_log_path_buf.clone(),
            Arc::clone(&self.job_registry),
        );

        let job_id_for_log = job_id.clone();

        tokio::spawn(async move {
            let _permit = permit;
            if let Err(e) = streamer.stream_channel(channel, initial_stdout).await {
                error!(
                    "streaming failed for background job {}: {}",
                    job_id_for_log, e
                );
            }
        });

        Ok(background_json_ok(
            &job_id,
            markers.pid,
            &final_log_path,
            &markers.remote_log_path,
        ))
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
            error!("Failed to ensure SSH connection: {}", e);
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
                error!("Sudo command execution failed: {}", e);
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
        debug!("check-process tool called: job_id={}", params.job_id);

        // Ensure connection is established
        if let Err(e) = self.connection.ensure_connected().await {
            error!("Failed to ensure SSH connection: {}", e);
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
                error!("Check process failed: {}", e);
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
        let job_id = make_job_id();
        let remote_log_path = remote_job_log_path(&job_id);

        let (final_log_path_buf, final_log_path) = match log_path {
            Some(p) => (PathBuf::from(p), p.to_string()),
            None => match self.default_local_log_path(&job_id) {
                Ok(v) => v,
                Err(e) => {
                    return Ok(background_json_err(
                        &job_id,
                        "",
                        Some(&remote_log_path),
                        &e,
                        "",
                    ));
                }
            },
        };

        if let Err(e) = self.ensure_local_log_file(&final_log_path_buf).await {
            return Ok(background_json_err(
                &job_id,
                &final_log_path,
                Some(&remote_log_path),
                &e.to_string(),
                "",
            ));
        }

        let sanitized = match sanitize_command(command, self.max_chars) {
            Ok(cmd) => cmd,
            Err(e) => {
                return Ok(background_json_err(
                    &job_id,
                    &final_log_path,
                    Some(&remote_log_path),
                    &e.to_string(),
                    "",
                ));
            }
        };

        if let Err(e) = self.connection.ensure_connected().await {
            return Ok(background_json_err(
                &job_id,
                &final_log_path,
                Some(&remote_log_path),
                &format!("SSH connection error: {}", e),
                "",
            ));
        }

        // Wrap the command with sudo, then start it in the background.
        let sudo_password = self.connection.get_sudo_password();
        let wrapped_command = wrap_sudo_command(&sanitized, sudo_password);
        debug!(
            "Wrapped sudo command (password hidden): sudo -n sh -c '...' or printf '...' | sudo ..."
        );

        let detach_mode = self.determine_detach_mode().await;
        if detach_mode == DetachMode::DirectOnly {
            return Ok(background_json_err(
                &job_id,
                &final_log_path,
                Some(&remote_log_path),
                "Background detach is not supported on this target; run with background=false.",
                "",
            ));
        }

        let wrapper = build_background_wrapper_script(
            detach_mode,
            &job_id,
            &wrapped_command,
            &remote_log_path,
        );

        let permit = match self
            .connection
            .channel_semaphore
            .clone()
            .acquire_owned()
            .await
        {
            Ok(p) => p,
            Err(e) => {
                return Ok(background_json_err(
                    &job_id,
                    &final_log_path,
                    Some(&remote_log_path),
                    &format!("Failed to acquire command slot: {e}"),
                    "",
                ));
            }
        };

        let mut channel = match self.connection.open_channel().await {
            Ok(ch) => ch,
            Err(e) => {
                return Ok(background_json_err(
                    &job_id,
                    &final_log_path,
                    Some(&remote_log_path),
                    &e.to_string(),
                    "",
                ));
            }
        };

        if let Err(e) = channel.exec(true, wrapper.as_str()).await {
            return Ok(background_json_err(
                &job_id,
                &final_log_path,
                Some(&remote_log_path),
                &format!("Failed to exec background wrapper: {e}"),
                "",
            ));
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
                return Ok(background_json_err(
                    &job_id,
                    &final_log_path,
                    Some(&remote_log_path),
                    &e,
                    "",
                ));
            }
        };

        self.register_running_job(
            &job_id,
            markers.pid,
            final_log_path_buf.clone(),
            &format!("sudo {sanitized}"),
        )
        .await;

        let streamer = OutputStreamer::new(
            job_id.clone(),
            final_log_path_buf.clone(),
            Arc::clone(&self.job_registry),
        );

        let job_id_for_log = job_id.clone();

        tokio::spawn(async move {
            let _permit = permit;
            if let Err(e) = streamer.stream_channel(channel, initial_stdout).await {
                error!(
                    "streaming failed for background sudo job {}: {}",
                    job_id_for_log, e
                );
            }
        });

        Ok(background_json_ok(
            &job_id,
            markers.pid,
            &final_log_path,
            &markers.remote_log_path,
        ))
    }

    fn sanitize_or_tool_error(&self, command: &str) -> std::result::Result<String, CallToolResult> {
        sanitize_command(command, self.max_chars).map_err(|e| {
            error!("Command sanitization failed: {}", e);
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

    fn command_tool(
        name: &'static str,
        tool_description: &'static str,
        command_description: &'static str,
    ) -> Tool {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": command_description
                },
                "background": {
                    "type": "boolean",
                    "default": false
                },
                "timeout_ms": {
                    "type": "integer"
                },
                "log_path": {
                    "type": "string"
                }
            },
            "required": ["command"]
        });

        // Convert Value to JsonObject (Map<String, Value>)
        let schema_obj = schema.as_object().cloned().unwrap_or_default();

        Tool::new(name, tool_description, Arc::new(schema_obj))
    }

    /// Build exec tool definition (compact)
    fn exec_tool() -> Tool {
        Self::command_tool(
            "exec",
            "Execute shell command on remote host. Use background=true for long tasks.",
            "Shell command to execute",
        )
    }

    /// Build sudo-exec tool definition (compact)
    fn sudo_exec_tool() -> Tool {
        Self::command_tool(
            "sudo-exec",
            "Execute shell command via sudo. Use background=true for long tasks.",
            "Shell command to execute with sudo",
        )
    }

    /// Build transfer tool definition (compact)
    fn transfer_tool() -> Tool {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "enum": ["put", "get"]
                },
                "local_path": {
                    "type": "string"
                },
                "remote_path": {
                    "type": "string"
                },
                "transport": {
                    "type": "string",
                    "enum": ["auto", "exec-raw", "sftp", "scp", "rsync"],
                    "default": "auto",
                    "description": "Transfer method: auto (fallback chain), sftp/scp/rsync (need --key), exec-raw (pure SSH)"
                },
                "kind": {
                    "type": "string",
                    "enum": ["file", "directory"]
                },
                "overwrite": {
                    "type": "boolean",
                    "default": false
                },
                "timeout_ms": {
                    "type": "integer"
                }
            },
            "required": ["operation", "local_path", "remote_path"]
        });

        let schema_obj = schema.as_object().cloned().unwrap_or_default();
        Tool::new(
            "transfer",
            "Transfer files via SSH. Supports: auto/sftp/scp/rsync/exec-raw. Requires --key for sftp/scp/rsync.",
            Arc::new(schema_obj),
        )
    }

    /// Build check-process tool definition
    fn check_process_tool() -> Tool {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "job_id": {
                    "type": "string",
                    "description": "Job ID returned by exec/sudo-exec (required)"
                },
                "tail_lines": {
                    "type": "integer",
                    "default": 50,
                    "description": "Number of lines to read from local log (default 50)"
                }
            },
            "required": ["job_id"]
        });

        let schema_obj = schema.as_object().cloned().unwrap_or_default();
        Tool::new(
            "check-process",
            "Check status of a background process started by exec/sudo-exec tools. Useful for monitoring long-running commands and retrieving results after timeout.",
            Arc::new(schema_obj),
        )
    }

    /// Get extended documentation for a tool by name
    ///
    /// Returns the full documentation text that was removed from compact tool definitions
    /// to save tokens in the MCP protocol.
    pub fn get_tool_documentation(tool_name: &str) -> Option<&'static str> {
        match tool_name {
            "exec" => Some(tool_docs::EXEC),
            "sudo-exec" => Some(tool_docs::SUDO_EXEC),
            "transfer" => Some(tool_docs::TRANSFER),
            _ => None,
        }
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

        tools.push(Self::transfer_tool());
        tools.push(Self::check_process_tool());

        // Add sudo-exec tool if enabled
        if !self.config.disable_sudo {
            tools.push(Self::sudo_exec_tool());
        }

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

        let common_args = |args: &serde_json::Map<String, serde_json::Value>| {
            let command = args
                .get("command")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    McpError::invalid_params("Missing required parameter: command", None)
                })?
                .to_string();

            let background = args
                .get("background")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let timeout_ms: Option<u64> = match args.get("timeout_ms") {
                None => None,
                Some(v) if v.is_null() => None,
                Some(v) => {
                    if let Some(u) = v.as_u64() {
                        Some(u)
                    } else if let Some(i) = v.as_i64() {
                        if i < 0 {
                            return Err(McpError::invalid_params(
                                "timeout_ms must be a non-negative integer",
                                None,
                            ));
                        }
                        Some(i as u64)
                    } else {
                        return Err(McpError::invalid_params(
                            "timeout_ms must be an integer",
                            None,
                        ));
                    }
                }
            };

            if let Some(0) = timeout_ms {
                return Err(McpError::invalid_params("timeout_ms must be > 0", None));
            }

            let log_path: Option<String> = if background {
                match args.get("log_path") {
                    None => None,
                    Some(v) if v.is_null() => None,
                    Some(v) => {
                        let s = v.as_str().ok_or_else(|| {
                            McpError::invalid_params("log_path must be a string", None)
                        })?;
                        validate_background_log_path(self.spooler.base_dir(), s)
                            .map_err(|msg| McpError::invalid_params(msg, None))?;

                        Some(s.to_string())
                    }
                }
            } else {
                // Foreground behavior: keep existing permissive parsing even though log_path is
                // ignored when background=false.
                match args.get("log_path") {
                    None => None,
                    Some(v) if v.is_null() => None,
                    Some(v) => {
                        let s = v.as_str().ok_or_else(|| {
                            McpError::invalid_params("log_path must be a string", None)
                        })?;
                        let trimmed = s.trim();
                        if trimmed.is_empty() {
                            return Err(McpError::invalid_params("log_path cannot be empty", None));
                        }
                        Some(trimmed.to_string())
                    }
                }
            };

            Ok(CommonToolArgs {
                command,
                background,
                timeout_ms,
                log_path,
            })
        };

        // Route to the appropriate tool
        match tool_name {
            "exec" => {
                let parsed = common_args(&args)?;

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

                let parsed = common_args(&args)?;

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
    fn test_select_detach_mode_ladder() {
        assert_eq!(select_detach_mode(true, true), DetachMode::Full);
        assert_eq!(select_detach_mode(true, false), DetachMode::Full);
        assert_eq!(select_detach_mode(false, true), DetachMode::Portable);
        assert_eq!(select_detach_mode(false, false), DetachMode::DirectOnly);
    }

    #[test]
    fn test_parse_background_markers() {
        let remote_log = remote_job_log_path("abc-123");
        let stdout =
            format!("__SSH_MCP_JOB_ID=abc-123\n__SSH_MCP_PID=456\n__SSH_MCP_LOG={remote_log}\n");
        let markers = parse_background_markers(&stdout, "abc-123", &remote_log).unwrap();
        assert_eq!(
            markers,
            BackgroundMarkers {
                job_id: "abc-123".to_string(),
                pid: 456,
                remote_log_path: remote_log,
            }
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
