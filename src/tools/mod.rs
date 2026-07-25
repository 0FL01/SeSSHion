//! MCP Tools module
//!
//! This module previously provided separate tool classes with #[tool_router].
//! Now, tools are implemented directly in the SshMcpServer via ServerHandler trait.
//!
//! Available tools:
//! - `shell` - Execute shell commands on the remote SSH server
//! - `sudo_shell` - Execute shell commands with sudo privileges
//! - `check_process` - Check if a process is still running and read its log
//! - `transfer` - Transfer files and directories over SSH
//! - `apply_patch` - Create, update, or delete one remote UTF-8 text file
//! - `sudo_apply_patch` - Apply the same exact patch under sudo
//!
//! See `server.rs` for the implementation.

// The tools are now implemented directly in server.rs as part of ServerHandler.
// This module is kept for potential future expansion with additional tools
// or utility functions.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub(crate) const DEFAULT_CHECK_PROCESS_TAIL_LINES: usize = 50;

/// Parameters for the shell tool
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct ExecParams {
    /// Shell command to execute on the remote SSH server
    pub command: String,

    /// Background execution mode.
    ///
    /// If true, run the command in background and return immediately.
    /// The server continues streaming output into a local log file on the MCP server and
    /// tracks the job via an in-memory registry keyed by job_id.
    /// The tool returns JSON metadata (job_id/pid/log_path/log_exists).
    #[serde(default)]
    pub background: bool,

    /// Optional timeout override in milliseconds for foreground execution
    pub timeout_ms: Option<u64>,

    /// Local log path for background mode output (stored on MCP server)
    ///
    /// Defaults to ssh-mcp/<job_id>.log in the system temp directory.
    pub log_path: Option<String>,
}

/// Parameters for the sudo_shell tool
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct SudoExecParams {
    /// Shell command to execute with sudo on the remote SSH server
    pub command: String,

    /// Background execution mode.
    ///
    /// If true, run the command in background and return immediately.
    /// The server continues streaming output into a local log file on the MCP server and
    /// tracks the job via an in-memory registry keyed by job_id.
    /// The tool returns JSON metadata (job_id/pid/log_path/log_exists).
    #[serde(default)]
    pub background: bool,

    /// Optional timeout override in milliseconds for foreground execution
    pub timeout_ms: Option<u64>,

    /// Local log path for background mode output (stored on MCP server)
    ///
    /// Defaults to ssh-mcp/<job_id>.log in the system temp directory.
    pub log_path: Option<String>,
}

/// Parameters for the check_process tool
///
/// # Migration from old API
/// Previously required `pid` and `log_path`. Now uses `job_id` only.
/// The job_id is returned by shell/sudo_shell when background=true.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct CheckProcessParams {
    /// Job ID returned by shell/sudo_shell background execution
    pub job_id: String,

    /// Number of last lines to read from log (default: 50)
    #[serde(default = "default_tail_lines")]
    pub tail_lines: usize,
}

/// Parameters for the apply_patch tool
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApplyPatchParams {
    /// One-file patch envelope with an absolute remote path
    pub patch: String,
}

fn default_tail_lines() -> usize {
    DEFAULT_CHECK_PROCESS_TAIL_LINES
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
        let json = r#"{"job_id": "job-123"}"#;
        let params: CheckProcessParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.job_id, "job-123");
        assert_eq!(params.tail_lines, 50);
    }

    #[test]
    fn test_check_process_params_with_tail_lines() {
        let json = r#"{"job_id": "job-123", "tail_lines": 100}"#;
        let params: CheckProcessParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.job_id, "job-123");
        assert_eq!(params.tail_lines, 100);
    }

    #[test]
    fn test_apply_patch_params_deserialize_and_reject_unknown_fields() {
        let json = r#"{"patch":"*** Begin Patch\n*** Delete File: /tmp/old\n*** End Patch"}"#;
        let params: ApplyPatchParams = serde_json::from_str(json).unwrap();
        assert!(params.patch.contains("*** Delete File"));

        let err =
            serde_json::from_str::<ApplyPatchParams>(r#"{"patch":"x","remote_path":"/tmp/x"}"#)
                .unwrap_err();
        assert!(err.to_string().contains("unknown field `remote_path`"));
    }
}
