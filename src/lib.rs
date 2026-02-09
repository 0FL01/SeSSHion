//! SSH MCP Server - A Model Context Protocol server for remote SSH command execution
//!
//! This crate provides an MCP server that allows executing commands on remote Linux
//! systems via SSH. It supports both password and key-based authentication, as well
//! as privilege elevation via `su` and `sudo`.
//!
//! # Features
//!
//! - Execute shell commands on remote SSH servers (`exec` tool)
//! - Support for `sudo` command execution with password (`sudo-exec` tool)
//! - Persistent SSH connection with auto-reconnect
//! - Configurable command timeout
//! - Command length limits for safety
//!
//! # MCP Tools
//!
//! - `exec` - Execute a shell command on the remote SSH server
//! - `sudo-exec` - Execute a command with sudo privileges (can be disabled with `--disable-sudo`)
//!
//! # Example Usage (CLI)
//!
//! ```bash
//! ssh-mcp --host=192.168.1.100 --user=admin --password=secret
//! ```
//!
//! # Example Usage (MCP Inspector)
//!
//! ```bash
//! npx @modelcontextprotocol/inspector ./target/release/ssh-mcp -- \
//!   --host=YOUR_HOST --user=root --password=pass
//! ```

pub mod config;
pub mod error;
pub mod logging;
pub mod server;
pub mod ssh;
pub mod tools;
pub mod transfer;

// Re-exports for convenience
pub use config::{Args, Config};
pub use error::{Result, SshMcpError};
pub use server::SshMcpServer;
pub use ssh::{
    CommandOutput, SshConfig, SshConnectionManager, SshHandler, escape_command_for_shell,
    escape_for_shell, escape_for_timeout_wrapper, sanitize_command, sanitize_password,
    wrap_command_with_timeout, wrap_sudo_command,
};
pub use tools::{CheckProcessParams, ExecParams, SudoExecParams};
