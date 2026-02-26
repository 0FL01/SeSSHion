use std::sync::Arc;

use rmcp::model::Tool;

mod tool_docs {
    pub const EXEC: &str = r#"EXEC TOOL
Execute commands on remote SSH server via POSIX-compatible sh.

PARAMETERS:
- command (string, required): Command string executed by POSIX-compatible sh (use portable shell syntax)
- background (boolean): Run in background. Returns immediately with {job_id,pid,log_path}.
  Output is streamed to local log file on MCP server. Monitor via check-process using job_id.
  (+ remote_log_path deprecated)
- timeout_ms (integer): Foreground-only. Ignored when background=true (not validated in that mode).
- log_path (string): Background-only. Ignored when background=false (not validated in that mode). Must be under the system temp directory (e.g., /tmp/ssh-mcp on Unix, %TEMP%\ssh-mcp on Windows).

BACKGROUND MODE:
For commands longer than RPC timeout, use background=true:
1. Command runs detached on the remote host
2. Returns immediately with job_id, pid, LOCAL log_path on the MCP server
3. Monitor: use check-process with job_id (preferred) or ps -p <pid> -o pid,etime,cmd
4. View output: use check-process with job_id; or tail -n 50 '<log_path>' (local spool file)

NOTE:
- remote_log_path is kept for backward compatibility only (deprecated) and will be removed in a future version.
- Commands are evaluated by POSIX-compatible sh on the remote host. Prefer portable shell syntax over shell-specific extensions.

EXAMPLE:
{"command": "apt update && apt install -y nginx", "background": true}"#;

    pub const SUDO_EXEC: &str = r#"SUDO-EXEC TOOL
Execute commands with sudo privileges via POSIX-compatible sh.

Same parameters as exec tool, but timeout behavior differs.
Requires passwordless sudo or pre-configured sudo password.

PARAMETERS:
- command (string, required): Command string executed by POSIX-compatible sh under sudo (use portable shell syntax)
- background (boolean): Run in background. Returns immediately with {job_id,pid,log_path}.
  Output is streamed to local log file on MCP server. Monitor via check-process using job_id.
- timeout_ms (integer): Foreground-only. If timeout is reached in foreground, sudo-exec returns a timeout error (no auto-detach). Ignored when background=true (not validated in that mode).
- log_path (string): Background-only. Ignored when background=false (not validated in that mode). Must be under the system temp directory (e.g., /tmp/ssh-mcp on Unix, %TEMP%\ssh-mcp on Windows).

NOTE:
- Commands are evaluated by POSIX-compatible sh on the remote host. Prefer portable shell syntax over shell-specific extensions.
- Auto-detach on foreground timeout applies to exec only. For long-running sudo commands, set background=true.

EXAMPLE:
{"command": "systemctl restart nginx", "background": false}"#;

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

pub(super) fn exec_tool() -> Tool {
    command_tool(
        "exec",
        "Execute command via POSIX-compatible sh on remote host. Use background=true for long tasks.",
        "Command string executed by POSIX-compatible sh",
    )
}

pub(super) fn sudo_exec_tool() -> Tool {
    command_tool(
        "sudo-exec",
        "Execute command via POSIX-compatible sh under sudo. Use background=true for long tasks.",
        "Command string executed by POSIX-compatible sh under sudo",
    )
}

pub(super) fn transfer_tool() -> Tool {
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

pub(super) fn check_process_tool() -> Tool {
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

pub(super) fn get_tool_documentation(tool_name: &str) -> Option<&'static str> {
    match tool_name {
        "exec" => Some(tool_docs::EXEC),
        "sudo-exec" => Some(tool_docs::SUDO_EXEC),
        "transfer" => Some(tool_docs::TRANSFER),
        _ => None,
    }
}
