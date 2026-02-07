//! SSH connection management module
//!
//! This module provides persistent SSH connection handling with automatic
//! reconnection, authentication, and session management.

pub mod command;
pub mod config;
pub mod connection;
pub mod elevation;
pub mod handler;
pub mod sanitize;

// Re-exports
pub use command::{CommandOutput, TransferRawOutput, wrap_command_with_timeout};
pub use config::SshConfig;
pub use connection::SshConnectionManager;
pub use elevation::{escape_for_shell, sanitize_password, wrap_sudo_command};
pub use handler::SshHandler;
pub use sanitize::{escape_command_for_shell, escape_for_timeout_wrapper, sanitize_command};
