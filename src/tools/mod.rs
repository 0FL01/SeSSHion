//! MCP Tools module
//!
//! This module previously provided separate tool classes with #[tool_router].
//! Now, tools are implemented directly in the SshMcpServer via ServerHandler trait.
//!
//! Available tools:
//! - `exec` - Execute shell commands on the remote SSH server
//! - `sudo-exec` - Execute shell commands with sudo privileges
//! - `check-process` - Check if a process is still running and read its log
//!
//! See `server.rs` for the implementation.

// The tools are now implemented directly in server.rs as part of ServerHandler.
// This module is kept for potential future expansion with additional tools
// or utility functions.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Parameters for the exec tool
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct ExecParams {
    /// Shell command to execute on the remote SSH server
    pub command: String,

    /// If true, start the command detached (nohup) and return immediately.
    /// Use this for long-running commands to avoid client timeouts.
    /// The command output is written to log_path and the tool returns JSON metadata (job_id/pid/log_path).
    #[serde(default)]
    pub background: bool,

    /// Optional timeout override in milliseconds for foreground execution
    pub timeout_ms: Option<u64>,

    /// Optional remote log path for background mode
    pub log_path: Option<String>,
}

/// Parameters for the sudo-exec tool
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct SudoExecParams {
    /// Shell command to execute with sudo on the remote SSH server
    pub command: String,

    /// If true, start the command detached (nohup) and return immediately.
    /// Use this for long-running commands to avoid client timeouts.
    /// The command output is written to log_path and the tool returns JSON metadata (job_id/pid/log_path).
    #[serde(default)]
    pub background: bool,

    /// Optional timeout override in milliseconds for foreground execution
    pub timeout_ms: Option<u64>,

    /// Optional remote log path for background mode
    pub log_path: Option<String>,
}

/// Parameters for the check-process tool
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct CheckProcessParams {
    /// Process ID to check
    pub pid: u32,

    /// Optional path to the log file to read tail from
    pub log_path: Option<String>,

    /// Number of last lines to read from log (default: 50)
    #[serde(default = "default_tail_lines")]
    pub tail_lines: usize,
}

fn default_tail_lines() -> usize {
    50
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exec_params_deserialize() {
        let json = r#"{"command": "echo hello"}"#;
        let params: ExecParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.command, "echo hello");
        assert!(!params.background);
        assert!(params.timeout_ms.is_none());
        assert!(params.log_path.is_none());
    }

    #[test]
    fn test_exec_params_deserialize_background() {
        let json = r#"{"command": "sleep 10", "background": true, "timeout_ms": 1000, "log_path": "/tmp/x.log"}"#;
        let params: ExecParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.command, "sleep 10");
        assert!(params.background);
        assert_eq!(params.timeout_ms, Some(1000));
        assert_eq!(params.log_path.as_deref(), Some("/tmp/x.log"));
    }

    #[test]
    fn test_sudo_exec_params_deserialize() {
        let json = r#"{"command": "apt update"}"#;
        let params: SudoExecParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.command, "apt update");
        assert!(!params.background);
        assert!(params.timeout_ms.is_none());
        assert!(params.log_path.is_none());
    }

    #[test]
    fn test_check_process_params_deserialize() {
        let json = r#"{"pid": 12345}"#;
        let params: CheckProcessParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.pid, 12345);
        assert!(params.log_path.is_none());
        assert_eq!(params.tail_lines, 50);
    }

    #[test]
    fn test_check_process_params_with_log() {
        let json = r#"{"pid": 12345, "log_path": "/tmp/test.log", "tail_lines": 100}"#;
        let params: CheckProcessParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.pid, 12345);
        assert_eq!(params.log_path.as_deref(), Some("/tmp/test.log"));
        assert_eq!(params.tail_lines, 100);
    }
}
