//! SSH MCP Server - A Model Context Protocol server for remote SSH command execution
//!
//! This crate provides an MCP server that allows executing commands on remote Linux
//! systems via SSH. It supports both password and key-based authentication, as well
//! as privilege elevation via `su` and `sudo`.
//!
//! # Features
//!
//! - Execute shell commands on remote SSH servers (`shell` tool)
//! - Support for `sudo` command execution with password (`sudo_shell` tool)
//! - Persistent SSH connection with auto-reconnect
//! - Configurable command timeout
//! - Command length limits for safety
//!
//! # MCP Tools
//!
//! - `shell` - Execute a shell command on the remote SSH server
//! - `sudo_shell` - Execute a command with sudo privileges (can be disabled with `--disable-sudo`)
//! - `check_process` - Monitor background command and transfer jobs
//! - `transfer` - Transfer files and directories over SSH, optionally in the background
//! - `apply_patch` - Create, update, or delete one remote UTF-8 text file
//! - `sudo_apply_patch` - Apply an exact remote file patch under sudo (can be disabled with `--disable-sudo`)
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

pub mod background;
pub mod config;
pub mod error;
pub mod logging;
pub(crate) mod patch;
#[cfg(unix)]
pub(crate) mod platform;
pub mod server;
mod shell_escape;
pub mod ssh;
pub mod tools;
pub mod transfer;
pub(crate) mod validate;

// Re-exports for convenience
pub use config::{Args, Config};
pub use error::{Result, SshMcpError};
pub use server::SshMcpServer;
pub use ssh::{
    CommandOutput, HostKeyCheckMode, SshConfig, SshConnectionManager, SshHandler,
    escape_command_for_shell, escape_for_shell, escape_for_timeout_wrapper, sanitize_command,
    sanitize_password, wrap_command_with_timeout, wrap_sudo_command,
};
pub use tools::{CheckProcessParams, ExecParams, SudoExecParams};
