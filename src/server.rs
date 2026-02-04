//! MCP Server implementation
//!
//! This module provides the main MCP server that integrates SSH connection
//! management with the `exec` and `sudo-exec` tools.

use std::sync::Arc;
use std::time::Duration;

use rmcp::{
    ErrorData as McpError,
    handler::server::ServerHandler,
    model::*,
    service::{RequestContext, RoleServer},
};
use tracing::{debug, error, info};

use crate::config::Config;
use crate::error::{Result, SshMcpError};
use crate::ssh::{SshConfig, SshConnectionManager, sanitize_command, wrap_sudo_command};

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
    async fn execute_command(
        &self,
        command: &str,
    ) -> std::result::Result<CallToolResult, McpError> {
        debug!("exec tool called with command: {}", command);

        // Sanitize the command
        let sanitized = match sanitize_command(command, self.max_chars) {
            Ok(cmd) => cmd,
            Err(e) => {
                error!("Command sanitization failed: {}", e);
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error: {}",
                    e
                ))]));
            }
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
        match self.connection.exec_command(&sanitized, self.timeout).await {
            Ok(output) => {
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
                    Ok(CallToolResult::error(vec![Content::text(result_text)]))
                } else {
                    Ok(CallToolResult::success(vec![Content::text(result_text)]))
                }
            }
            Err(e) => {
                error!("Command execution failed: {}", e);
                Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error: {}",
                    e
                ))]))
            }
        }
    }

    /// Execute a command with sudo (used by sudo-exec tool)
    async fn execute_sudo_command(
        &self,
        command: &str,
    ) -> std::result::Result<CallToolResult, McpError> {
        debug!("sudo-exec tool called with command: {}", command);

        // Sanitize the command
        let sanitized = match sanitize_command(command, self.max_chars) {
            Ok(cmd) => cmd,
            Err(e) => {
                error!("Command sanitization failed: {}", e);
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error: {}",
                    e
                ))]));
            }
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
            .exec_command(&wrapped_command, self.timeout)
            .await
        {
            Ok(output) => {
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
                    Ok(CallToolResult::error(vec![Content::text(result_text)]))
                } else {
                    Ok(CallToolResult::success(vec![Content::text(result_text)]))
                }
            }
            Err(e) => {
                error!("Sudo command execution failed: {}", e);
                Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error: {}",
                    e
                ))]))
            }
        }
    }

    /// Build exec tool definition
    fn exec_tool() -> Tool {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Shell command to execute on the remote SSH server"
                }
            },
            "required": ["command"]
        });

        // Convert Value to JsonObject (Map<String, Value>)
        let schema_obj = schema.as_object().cloned().unwrap_or_default();

        Tool::new(
            "exec",
            "Execute a shell command on the remote SSH server and return the output.",
            Arc::new(schema_obj),
        )
    }

    /// Build sudo-exec tool definition
    fn sudo_exec_tool() -> Tool {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Shell command to execute with sudo on the remote SSH server"
                }
            },
            "required": ["command"]
        });

        // Convert Value to JsonObject (Map<String, Value>)
        let schema_obj = schema.as_object().cloned().unwrap_or_default();

        Tool::new(
            "sudo-exec",
            "Execute a shell command on the remote SSH server using sudo. Will use sudo password if provided, otherwise assumes passwordless sudo.",
            Arc::new(schema_obj),
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

        // Route to the appropriate tool
        match tool_name {
            "exec" => {
                // Extract command from arguments
                let command = args
                    .get("command")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        McpError::invalid_params("Missing required parameter: command", None)
                    })?;

                self.execute_command(command).await
            }
            "sudo_exec" | "sudo-exec" => {
                // Check if sudo is enabled
                if self.config.disable_sudo {
                    return Err(McpError::invalid_params("sudo-exec tool is disabled", None));
                }

                // Extract command from arguments
                let command = args
                    .get("command")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        McpError::invalid_params("Missing required parameter: command", None)
                    })?;

                self.execute_sudo_command(command).await
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
}
