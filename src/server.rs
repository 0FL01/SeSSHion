//! MCP Server implementation
//!
//! This module provides the main MCP server that integrates SSH connection
//! management with the `exec` and `sudo-exec` tools.

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
use tokio::sync::Mutex;
use tracing::{debug, error, info};

use crate::config::Config;
use crate::error::{Result, SshMcpError};
use crate::ssh::{
    CommandOutput, SshConfig, SshConnectionManager, sanitize_command, wrap_sudo_command,
};
use crate::transfer::{TransferEngine, TransferParams, TransferRunContext, TransferSshOptions};

const BACKGROUND_START_TIMEOUT: Duration = Duration::from_secs(20);
const BACKGROUND_JSON_SNIPPET_LIMIT_CHARS: usize = 2048;
const FOREGROUND_EXIT_POLL_INTERVAL: Duration = Duration::from_millis(200);
const FOREGROUND_EXIT_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const FOREGROUND_LOG_READ_TIMEOUT: Duration = Duration::from_secs(30);
const DETACH_PROBE_EXIT_WAIT: Duration = Duration::from_secs(5);
const DETACH_PROBE_POLL_INTERVAL: Duration = Duration::from_millis(100);

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
    log_path: String,
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

    // Keep stdout deterministic: only emit markers.
    // Any error output should go to stderr.
    format!(
        "LOG='{escaped_log_path}'; \
 EXIT=\"$LOG.exit\"; export EXIT; \
 log_dir=\"$(dirname -- \"$LOG\")\" || {{ printf '%s\n' \"__SSH_MCP_ERR=dirname_failed\" >&2; exit 1; }}; \
 mkdir -p -- \"$log_dir\" || {{ printf '%s\n' \"__SSH_MCP_ERR=mkdir_failed\" >&2; exit 1; }}; \
  nohup sh -lc '{escaped_user_command}; ec=$?; printf '\"'\"'%s\\n'\"'\"' \"$ec\" >\"$EXIT\"' >\"$LOG\" 2>&1 </dev/null & pid=$!; \
 printf '%s\n' \"__SSH_MCP_JOB_ID={job_id}\"; \
 printf '%s\n' \"__SSH_MCP_PID=$pid\"; \
 printf '%s\n' \"__SSH_MCP_LOG=$LOG\"",
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

    // BusyBox-friendly: avoid GNU `--` flags; avoid `sh -l`.
    // Keep stdout deterministic: only emit markers.
    format!(
        "LOG='{escaped_log_path}'; \
 EXIT=\"$LOG.exit\"; export EXIT; \
 log_dir=\"${{LOG%/*}}\"; [ \"$log_dir\" = \"$LOG\" ] && log_dir='.'; \
 mkdir -p \"$log_dir\" || {{ printf '%s\n' \"__SSH_MCP_ERR=mkdir_failed\" >&2; exit 1; }}; \
  if command -v nohup >/dev/null 2>&1; then \
    nohup sh -c '{escaped_user_command}; ec=$?; printf '\"'\"'%s\\n'\"'\"' \"$ec\" >\"$EXIT\"' >\"$LOG\" 2>&1 </dev/null & \
  else \
    sh -c '{escaped_user_command}; ec=$?; printf '\"'\"'%s\\n'\"'\"' \"$ec\" >\"$EXIT\"' >\"$LOG\" 2>&1 </dev/null & \
  fi; pid=$!; \
  printf '%s\n' \"__SSH_MCP_JOB_ID={job_id}\"; \
  printf '%s\n' \"__SSH_MCP_PID=$pid\"; \
  printf '%s\n' \"__SSH_MCP_LOG=$LOG\"",
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
        DetachMode::DirectOnly => String::new(),
    }
}

fn build_exit_probe_command(exit_path: &str) -> String {
    let escaped_exit_path = crate::ssh::escape_for_shell(exit_path);
    format!("cat '{escaped_exit_path}' 2>/dev/null || true")
}

fn build_log_read_command(log_path: &str) -> String {
    let escaped_log_path = crate::ssh::escape_for_shell(log_path);
    format!("cat < '{escaped_log_path}'")
}

fn validate_background_log_path(log_path: &str) -> std::result::Result<(), &'static str> {
    if log_path.is_empty() {
        return Err("log_path cannot be empty");
    }

    if log_path != log_path.trim() {
        return Err("log_path must not have leading/trailing whitespace");
    }

    // Be explicit for readability even though these are control chars.
    if log_path.contains(['\n', '\r']) {
        return Err("log_path must not contain newlines");
    }

    // Without GNU `cat --`, a path starting with '-' may be treated as an option.
    // Disallow this for user-provided paths.
    if log_path.starts_with('-') {
        return Err("log_path must not start with '-'");
    }

    if log_path.chars().any(|c| c.is_control()) {
        return Err("log_path must not contain control characters");
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
        log_path,
    })
}

fn truncate_with_flag(input: &str, limit_chars: usize) -> (String, bool) {
    let mut iter = input.chars();
    let snippet: String = iter.by_ref().take(limit_chars).collect();
    let truncated = iter.next().is_some();
    (snippet, truncated)
}

fn background_json_ok(markers: BackgroundMarkers) -> CallToolResult {
    let body = serde_json::json!({
        "ok": true,
        "background": true,
        "job_id": markers.job_id,
        "pid": markers.pid,
        "log_path": markers.log_path,
    })
    .to_string();

    CallToolResult::success(vec![Content::text(body)])
}

fn background_json_timeout(markers: BackgroundMarkers) -> CallToolResult {
    let hint = "Command timed out; job continues in background. Hint: use JSON fields pid/log_path; check progress with: ps -p <pid> -o pid,etime,cmd; tail -n 50 '<log_path>'";
    let body = serde_json::json!({
        "ok": false,
        "timeout": true,
        "background": true,
        "job_id": markers.job_id,
        "pid": markers.pid,
        "log_path": markers.log_path,
        "hint": hint,
    })
    .to_string();

    CallToolResult::success(vec![Content::text(body)])
}

fn background_json_err(job_id: &str, log_path: &str, error: &str, stderr: &str) -> CallToolResult {
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
        serde_json::Value::String(log_path.to_string()),
    );
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
                "Response fields were truncated to {} chars. Hint: inspect full output using the JSON log_path field; tail -n 50 '<log_path>'",
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

    transfer: TransferEngine,
}

/// Extended documentation for tools (available on-demand to save tokens in tool definitions)
pub mod tool_docs {
    /// Documentation for the exec tool
    pub const EXEC: &str = r#"EXEC TOOL
Execute shell commands on remote SSH server.

PARAMETERS:
- command (string, required): Shell command to execute
- background (boolean): Run via nohup, returns {job_id,pid,log_path}
- timeout_ms (integer): Wait timeout in milliseconds (ignored if background=true)
- log_path (string): Custom log path for background mode

BACKGROUND MODE:
For commands longer than RPC timeout, use background=true:
1. Command runs detached via nohup
2. Returns immediately with job_id, pid, and log_path
3. Monitor: ps -p <pid> -o pid,etime,cmd
4. View output: tail -n 50 '<log_path>'
5. Check exit code: cat '<log_path>.exit'

EXAMPLE:
{"command": "apt update && apt install -y nginx", "background": true}"#;

    /// Documentation for the sudo-exec tool  
    pub const SUDO_EXEC: &str = r#"SUDO-EXEC TOOL
Execute shell commands with sudo privileges.

Same parameters and behavior as exec tool, but runs with sudo.
Requires passwordless sudo or pre-configured sudo password.

PARAMETERS:
- command (string, required): Shell command to execute with sudo
- background (boolean): Run via nohup
- timeout_ms (integer): Wait timeout
- log_path (string): Custom log path

EXAMPLE:
{"command": "systemctl restart nginx", "background": false}"#;

    /// Documentation for the transfer tool
    pub const TRANSFER: &str = r#"TRANSFER TOOL
Transfer files or directories between local and remote hosts.

PARAMETERS:
- operation (string, required): "put" (local→remote) or "get" (remote→local)
- local_path (string, required): Local file path (relative to local_root or absolute path within local_root)
- remote_path (string, required): Absolute remote path
- transport (string): "auto" (default), "sftp", "scp", or "exec-raw"
- kind (string): "file" or "directory" (auto-detected if omitted)
- overwrite (boolean): Allow overwriting destination (default: false)
- timeout_ms (integer): Transfer timeout override

TRANSPORTS:
- auto: Tries sftp → scp → exec-raw in order
- sftp/scp: Require local OpenSSH binaries and --key
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
            transfer: TransferEngine::new(local_root),
        })
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

        let log_path = format!("/tmp/ssh-mcp/autodetect-{job_id}.log");
        let exit_path = format!("{}.exit", log_path);
        let wrapper = build_background_wrapper_script(mode, &job_id, &probe_command, &log_path);

        let start_output = self
            .connection
            .exec_command(&wrapper, Duration::from_secs(5))
            .await?;

        let markers = match parse_background_markers(&start_output.stdout, &job_id, &log_path) {
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

        let deadline = tokio::time::Instant::now() + DETACH_PROBE_EXIT_WAIT;
        let mut exit_code: Option<u32> = None;
        while tokio::time::Instant::now() < deadline {
            let probe_cmd = build_exit_probe_command(&exit_path);
            let out = self
                .connection
                .exec_command(&probe_cmd, Duration::from_secs(2))
                .await?;
            let trimmed = out.stdout.trim();
            if let Ok(code) = trimmed.parse::<u32>() {
                exit_code = Some(code);
                break;
            }
            tokio::time::sleep(DETACH_PROBE_POLL_INTERVAL).await;
        }

        if exit_code != Some(0) {
            return Ok(false);
        }

        let read_log_cmd = build_log_read_command(&markers.log_path);
        let log_out = self
            .connection
            .exec_command(&read_log_cmd, Duration::from_secs(5))
            .await?;

        if log_out.exit_code.is_some_and(|code| code != 0) || !log_out.stderr.is_empty() {
            return Ok(false);
        }

        Ok(log_out.stdout.contains(&marker))
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
        // - Start the command immediately using the background wrapper (nohup + log + pid markers)
        // - Poll for an exit-code file until timeout
        // - If timeout elapses, return a deterministic single-line JSON payload and keep the job running

        let detach_mode = self.determine_detach_mode().await;
        if detach_mode == DetachMode::DirectOnly {
            match self.connection.exec_command(&sanitized, timeout).await {
                Ok(output) => return Ok(Self::calltool_from_command_output(output)),
                Err(e) => {
                    error!("Command execution failed: {}", e);
                    let mut msg = format!("Error: {}", e);
                    if matches!(e, SshMcpError::Timeout(_)) {
                        msg.push_str("\nHint: background detach is not supported on this target; rerun with a larger timeout_ms or split the command.");
                    }
                    return Ok(CallToolResult::error(vec![Content::text(msg)]));
                }
            }
        }

        let job_id = make_job_id();
        let final_log_path = format!("/tmp/ssh-mcp/{}.log", job_id);
        let exit_path = format!("{}.exit", final_log_path);

        let wrapper =
            build_background_wrapper_script(detach_mode, &job_id, &sanitized, &final_log_path);
        let start_output = match self
            .connection
            .exec_command(&wrapper, BACKGROUND_START_TIMEOUT)
            .await
        {
            Ok(out) => out,
            Err(e) => {
                error!("Failed to start detached foreground command: {}", e);
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error: {}",
                    e
                ))]));
            }
        };

        let markers = match parse_background_markers(&start_output.stdout, &job_id, &final_log_path)
        {
            Ok(m) => m,
            Err(parse_err) => {
                let mut msg = format!("Failed to parse background markers: {parse_err}");
                if let Some(code) = start_output.exit_code {
                    msg.push_str(&format!("; exit_code={}", code));
                }
                if !start_output.stderr.is_empty() {
                    let stderr_snippet: String = start_output.stderr.chars().take(2048).collect();
                    msg.push_str("; stderr=");
                    msg.push_str(&stderr_snippet);
                }
                error!("{}", msg);
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error: {}",
                    msg
                ))]));
            }
        };

        let start = tokio::time::Instant::now();
        let mut last_probe_error: Option<String> = None;

        let exit_code: u32 = loop {
            if start.elapsed() >= timeout {
                return Ok(background_json_timeout(markers));
            }

            let probe_cmd = build_exit_probe_command(&exit_path);

            match self
                .connection
                .exec_command(&probe_cmd, FOREGROUND_EXIT_PROBE_TIMEOUT)
                .await
            {
                Ok(out) => {
                    let trimmed = out.stdout.trim();
                    if trimmed.is_empty() {
                        // Not ready yet (or remote cat produced no stdout).
                        // Keep polling until the effective timeout.
                    } else {
                        match trimmed.parse::<u32>() {
                            Ok(code) => break code,
                            Err(e) => {
                                last_probe_error =
                                    Some(format!("failed to parse exit code '{trimmed}': {e}"));
                            }
                        }
                    }
                }
                Err(e) => {
                    last_probe_error = Some(e.to_string());
                }
            }

            tokio::time::sleep(FOREGROUND_EXIT_POLL_INTERVAL).await;
        };

        let read_log_cmd = build_log_read_command(&final_log_path);
        let log_output = self
            .connection
            .exec_command(&read_log_cmd, FOREGROUND_LOG_READ_TIMEOUT)
            .await;

        match log_output {
            Ok(out) => {
                // Treat log read failures as tool errors. `exec_command` returns Ok(CommandOutput)
                // even for non-zero exit codes, so we must inspect the result.
                if out.exit_code.is_some_and(|code| code != 0) || !out.stderr.is_empty() {
                    let mut msg = format!(
                        "Command finished (exit_code={}), but reading log failed: cat_exit_code={:?}",
                        exit_code, out.exit_code
                    );
                    if let Some(probe_err) = last_probe_error {
                        msg.push_str(&format!("; last_probe_error={probe_err}"));
                    }
                    let combined = out.combined_output();
                    if !combined.is_empty() {
                        msg.push_str("; cat_output=");
                        msg.push_str(&combined);
                    }
                    error!("{}", msg);
                    return Ok(CallToolResult::error(vec![Content::text(format!(
                        "Error: {}",
                        msg
                    ))]));
                }

                let output = CommandOutput {
                    stdout: out.stdout,
                    stderr: out.stderr,
                    exit_code: Some(exit_code),
                };
                Ok(Self::calltool_from_command_output(output))
            }
            Err(e) => {
                let mut msg = format!(
                    "Command finished (exit_code={}), but reading log failed: {}",
                    exit_code, e
                );
                if let Some(probe_err) = last_probe_error {
                    msg.push_str(&format!("; last_probe_error={probe_err}"));
                }
                error!("{}", msg);
                Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error: {}",
                    msg
                ))]))
            }
        }
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
        // Sanitize the command
        let sanitized = match sanitize_command(command, self.max_chars) {
            Ok(cmd) => cmd,
            Err(e) => {
                let job_id = make_job_id();
                let default_log = format!("/tmp/ssh-mcp/{}.log", job_id);
                let path = log_path.unwrap_or(&default_log);
                return Ok(background_json_err(&job_id, path, &e.to_string(), ""));
            }
        };

        // Ensure connection is established
        if let Err(e) = self.connection.ensure_connected().await {
            let job_id = make_job_id();
            let default_log = format!("/tmp/ssh-mcp/{}.log", job_id);
            let path = log_path.unwrap_or(&default_log);
            return Ok(background_json_err(
                &job_id,
                path,
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

        let job_id = make_job_id();
        let default_log = format!("/tmp/ssh-mcp/{}.log", job_id);
        let final_log_path: String = log_path.unwrap_or(&default_log).to_string();

        let detach_mode = self.determine_detach_mode().await;
        if detach_mode == DetachMode::DirectOnly {
            return Ok(background_json_err(
                &job_id,
                &final_log_path,
                "Background detach is not supported on this target; run with background=false.",
                "",
            ));
        }

        let wrapper =
            build_background_wrapper_script(detach_mode, &job_id, &sanitized, &final_log_path);

        let output = match self
            .connection
            .exec_command(&wrapper, BACKGROUND_START_TIMEOUT)
            .await
        {
            Ok(out) => out,
            Err(e) => {
                return Ok(background_json_err(
                    &job_id,
                    &final_log_path,
                    &e.to_string(),
                    "",
                ));
            }
        };

        match parse_background_markers(&output.stdout, &job_id, &final_log_path) {
            Ok(markers) => Ok(background_json_ok(markers)),
            Err(parse_err) => {
                let mut err = parse_err;
                if let Some(code) = output.exit_code {
                    err.push_str(&format!("; exit_code={}", code));
                }
                Ok(background_json_err(
                    &job_id,
                    &final_log_path,
                    &err,
                    &output.stderr,
                ))
            }
        }
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
                        "\nHint: rerun with background=true; watch log_path (e.g. tail -n 50 '<log_path>').",
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

    async fn execute_background_sudo_command(
        &self,
        command: &str,
        log_path: Option<&str>,
    ) -> std::result::Result<CallToolResult, McpError> {
        // Sanitize the command
        let sanitized = match sanitize_command(command, self.max_chars) {
            Ok(cmd) => cmd,
            Err(e) => {
                let job_id = make_job_id();
                let default_log = format!("/tmp/ssh-mcp/{}.log", job_id);
                let path = log_path.unwrap_or(&default_log);
                return Ok(background_json_err(&job_id, path, &e.to_string(), ""));
            }
        };

        // Ensure connection is established
        if let Err(e) = self.connection.ensure_connected().await {
            let job_id = make_job_id();
            let default_log = format!("/tmp/ssh-mcp/{}.log", job_id);
            let path = log_path.unwrap_or(&default_log);
            return Ok(background_json_err(
                &job_id,
                path,
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

        let job_id = make_job_id();
        let default_log = format!("/tmp/ssh-mcp/{}.log", job_id);
        let final_log_path: String = log_path.unwrap_or(&default_log).to_string();

        let detach_mode = self.determine_detach_mode().await;
        if detach_mode == DetachMode::DirectOnly {
            return Ok(background_json_err(
                &job_id,
                &final_log_path,
                "Background detach is not supported on this target; run with background=false.",
                "",
            ));
        }

        let wrapper = build_background_wrapper_script(
            detach_mode,
            &job_id,
            &wrapped_command,
            &final_log_path,
        );

        let output = match self
            .connection
            .exec_command(&wrapper, BACKGROUND_START_TIMEOUT)
            .await
        {
            Ok(out) => out,
            Err(e) => {
                return Ok(background_json_err(
                    &job_id,
                    &final_log_path,
                    &e.to_string(),
                    "",
                ));
            }
        };

        match parse_background_markers(&output.stdout, &job_id, &final_log_path) {
            Ok(markers) => Ok(background_json_ok(markers)),
            Err(parse_err) => {
                let mut err = parse_err;
                if let Some(code) = output.exit_code {
                    err.push_str(&format!("; exit_code={}", code));
                }
                Ok(background_json_err(
                    &job_id,
                    &final_log_path,
                    &err,
                    &output.stderr,
                ))
            }
        }
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
                    "enum": ["auto", "exec-raw", "sftp", "scp"],
                    "default": "auto"
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
            "Transfer files via SSH (put/get). Requires --key for sftp/scp.",
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
                        validate_background_log_path(s)
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
        let script = build_background_wrapper_script_full(
            "job-1",
            "echo 'hello world'",
            "/tmp/ssh-mcp/job-1.log",
        );
        assert!(script.contains("nohup sh -lc 'echo '\"'\"'hello world'\"'\"'; ec=$?;"));
    }

    #[test]
    fn test_build_background_wrapper_portable_is_busybox_friendly() {
        let script = build_background_wrapper_script_portable(
            "job-1",
            "echo test",
            "/tmp/ssh-mcp/job-1.log",
        );
        assert!(!script.contains("dirname --"));
        assert!(!script.contains("mkdir -p --"));
        assert!(!script.contains("sh -lc"));
        assert!(script.contains("sh -c"));
        assert!(script.contains("command -v nohup"));
    }

    fn normalize_single_quote_escapes(script: &str) -> String {
        script.replace("'\"'\"'", "'")
    }

    #[test]
    fn test_background_wrappers_use_single_quoted_printf_format_for_exit_code() {
        let full =
            build_background_wrapper_script_full("job-1", "echo test", "/tmp/ssh-mcp/job-1.log");
        let portable = build_background_wrapper_script_portable(
            "job-1",
            "echo test",
            "/tmp/ssh-mcp/job-1.log",
        );

        let full_normalized = normalize_single_quote_escapes(&full);
        let portable_normalized = normalize_single_quote_escapes(&portable);

        assert!(
            full_normalized.contains("printf '%s\\n' \"$ec\" >\"$EXIT\""),
            "full wrapper should write exit code using printf with a single-quoted format"
        );
        assert!(
            portable_normalized.contains("printf '%s\\n' \"$ec\" >\"$EXIT\""),
            "portable wrapper should write exit code using printf with a single-quoted format"
        );
    }

    #[test]
    fn test_exit_probe_and_log_read_commands_are_portable() {
        let exit_cmd = build_exit_probe_command("/tmp/ssh-mcp/x.log.exit");
        assert!(!exit_cmd.contains("cat --"));
        assert!(exit_cmd.contains("2>/dev/null"));

        let log_cmd = build_log_read_command("/tmp/ssh-mcp/x.log");
        assert!(!log_cmd.contains("cat --"));
        assert!(log_cmd.contains("cat < "));
    }

    #[test]
    fn test_validate_background_log_path_rejects_leading_dash() {
        let err = validate_background_log_path("-not-a-path").unwrap_err();
        assert!(err.contains("start with '-'") || err.contains("start with"));
    }

    #[test]
    fn test_validate_background_log_path_rejects_newlines() {
        assert!(validate_background_log_path("/tmp/x\nrm -rf /").is_err());
        assert!(validate_background_log_path("/tmp/x\rrm -rf /").is_err());
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
        let stdout =
            "__SSH_MCP_JOB_ID=abc-123\n__SSH_MCP_PID=456\n__SSH_MCP_LOG=/tmp/ssh-mcp/abc.log\n";
        let markers = parse_background_markers(stdout, "abc-123", "/tmp/ssh-mcp/abc.log").unwrap();
        assert_eq!(
            markers,
            BackgroundMarkers {
                job_id: "abc-123".to_string(),
                pid: 456,
                log_path: "/tmp/ssh-mcp/abc.log".to_string(),
            }
        );
    }

    #[test]
    fn test_background_json_err_sets_truncation_flag_and_hint() {
        let long_error = "e".repeat(BACKGROUND_JSON_SNIPPET_LIMIT_CHARS + 10);
        let long_stderr = "s".repeat(BACKGROUND_JSON_SNIPPET_LIMIT_CHARS + 10);

        let result =
            background_json_err("job-1", "/tmp/ssh-mcp/job-1.log", &long_error, &long_stderr);
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
            hint.contains("tail -n 50 '<log_path>'"),
            "hint should include a safe tail snippet with a placeholder path; got: '{hint}'"
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
    fn test_background_json_timeout_hint_uses_placeholders_not_marker_values() {
        let markers = BackgroundMarkers {
            job_id: "job-42".to_string(),
            pid: 4242,
            log_path: "/tmp/ssh-mcp/VERY_DISTINCT_LOG_PATH_9b9e3c.log".to_string(),
        };

        let result = background_json_timeout(markers.clone());
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

        assert!(
            hint.contains("<pid>"),
            "hint should reference pid placeholder; got: '{hint}'"
        );
        assert!(
            hint.contains("<log_path>"),
            "hint should reference log_path placeholder; got: '{hint}'"
        );
        assert!(
            !hint.contains(&markers.log_path),
            "hint should not contain concrete log_path marker value; got: '{hint}'"
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
