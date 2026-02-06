//! MCP Server implementation
//!
//! This module provides the main MCP server that integrates SSH connection
//! management with the `exec` and `sudo-exec` tools.

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
use tracing::{debug, error, info};

use crate::config::Config;
use crate::error::{Result, SshMcpError};
use crate::ssh::{
    CommandOutput, SshConfig, SshConnectionManager, sanitize_command, wrap_sudo_command,
};

const BACKGROUND_START_TIMEOUT: Duration = Duration::from_secs(20);

static JOB_COUNTER: AtomicU64 = AtomicU64::new(0);

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

fn build_background_wrapper_script(job_id: &str, user_command: &str, log_path: &str) -> String {
    // The wrapper itself may be nested inside another `sh -lc '...'`.
    // Escape only for single-quoted contexts inside this wrapper.
    let escaped_user_command = crate::ssh::escape_for_shell(user_command);
    let escaped_log_path = crate::ssh::escape_for_shell(log_path);

    // Keep stdout deterministic: only emit markers.
    // Any error output should go to stderr.
    format!(
        "LOG='{escaped_log_path}'; \
 log_dir=\"$(dirname -- \"$LOG\")\" || {{ printf '%s\n' \"__SSH_MCP_ERR=dirname_failed\" >&2; exit 1; }}; \
 mkdir -p -- \"$log_dir\" || {{ printf '%s\n' \"__SSH_MCP_ERR=mkdir_failed\" >&2; exit 1; }}; \
 nohup sh -lc '{escaped_user_command}' >\"$LOG\" 2>&1 </dev/null & pid=$!; \
 printf '%s\n' \"__SSH_MCP_JOB_ID={job_id}\"; \
 printf '%s\n' \"__SSH_MCP_PID=$pid\"; \
 printf '%s\n' \"__SSH_MCP_LOG=$LOG\"",
    )
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

fn background_json_err(job_id: &str, log_path: &str, error: &str, stderr: &str) -> CallToolResult {
    // Keep the payload deterministic and single-line. Avoid echoing the original command.
    let stderr_snippet: String = stderr.chars().take(2048).collect();
    let error_snippet: String = error.chars().take(2048).collect();

    let body = serde_json::json!({
        "ok": false,
        "background": true,
        "job_id": job_id,
        "log_path": log_path,
        "error": error_snippet,
        "stderr": stderr_snippet,
    })
    .to_string();

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
}

impl SshMcpServer {
    /// Create a new SSH MCP Server
    ///
    /// This sets up the SSH connection manager based on the provided configuration.
    /// Connection is not established until a tool is actually used.
    pub async fn new(config: Config) -> Result<Self> {
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

        // Execute the command
        match self.connection.exec_command(&sanitized, timeout).await {
            Ok(output) => Ok(Self::calltool_from_command_output(output)),
            Err(e) => {
                error!("Command execution failed: {}", e);
                Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error: {}",
                    e
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
        let wrapper = build_background_wrapper_script(&job_id, &sanitized, &final_log_path);

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
                Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error: {}",
                    e
                ))]))
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
        let wrapper = build_background_wrapper_script(&job_id, &wrapped_command, &final_log_path);

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
                    "description": "If true, start the command detached via nohup and return immediately to avoid client timeouts. The tool response is a single-line deterministic JSON object: {ok,background,job_id,pid,log_path}. Command output is written to log_path.",
                    "default": false
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Optional timeout override in milliseconds for foreground execution (waits for completion). Ignored when background=true."
                },
                "log_path": {
                    "type": "string",
                    "description": "Optional remote log path for background mode. Defaults to /tmp/ssh-mcp/<job_id>.log. Use tail -n 50 <log_path> to view progress."
                }
            },
            "required": ["command"]
        });

        // Convert Value to JsonObject (Map<String, Value>)
        let schema_obj = schema.as_object().cloned().unwrap_or_default();

        Tool::new(name, tool_description, Arc::new(schema_obj))
    }

    /// Build exec tool definition
    fn exec_tool() -> Tool {
        Self::command_tool(
            "exec",
            "Execute a shell command on the remote SSH server and return the output. For long-running commands, set background=true to detach and avoid client timeouts; you will get a deterministic JSON response with {job_id,pid,log_path}. Check progress with: ps -p <pid> -o pid,etime,cmd; tail -n 50 <log_path>.",
            "Shell command to execute on the remote SSH server",
        )
    }

    /// Build sudo-exec tool definition
    fn sudo_exec_tool() -> Tool {
        Self::command_tool(
            "sudo-exec",
            "Execute a shell command on the remote SSH server using sudo. Will use sudo password if provided, otherwise assumes passwordless sudo. For long-running commands, set background=true to detach and avoid client timeouts; you will get a deterministic JSON response with {job_id,pid,log_path}. Check progress with: ps -p <pid> -o pid,etime,cmd; tail -n 50 <log_path>.",
            "Shell command to execute with sudo on the remote SSH server",
        )
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

    /// Internal method exposed for testing - executes a sudo command directly
    #[doc(hidden)]
    pub async fn test_execute_sudo_command(
        &self,
        command: &str,
    ) -> std::result::Result<CallToolResult, McpError> {
        self.execute_sudo_command(command).await
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
                        if s.is_empty() {
                            return Err(McpError::invalid_params("log_path cannot be empty", None));
                        }

                        if s != s.trim() {
                            return Err(McpError::invalid_params(
                                "log_path must not have leading/trailing whitespace",
                                None,
                            ));
                        }

                        if s.chars().any(|c| c.is_control()) {
                            return Err(McpError::invalid_params(
                                "log_path must not contain control characters",
                                None,
                            ));
                        }

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
    fn test_build_background_wrapper_escapes_single_quotes_in_user_command() {
        let script = build_background_wrapper_script(
            "job-1",
            "echo 'hello world'",
            "/tmp/ssh-mcp/job-1.log",
        );
        assert!(script.contains("nohup sh -lc 'echo '\"'\"'hello world'\"'\"''"));
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
}
