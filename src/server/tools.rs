use std::sync::Arc;

use rmcp::model::Tool;

mod tool_docs {
    pub const SHELL: &str = r#"SHELL TOOL
Execute commands on remote SSH server via POSIX-compatible sh.

PARAMETERS:
- command (string, required): Command string executed by POSIX-compatible sh (use portable shell syntax)
- background (boolean): Run in background. Returns immediately with {job_id,pid,log_path,log_exists}.
  Output is streamed to local log file on MCP server. Monitor via check_process using job_id.
- timeout_ms (integer): Server-side foreground SSH wait limit only, not the full tool-call deadline. MCP does not expose the client's deadline to the server, so the client may stop waiting earlier. If reached, shell hands off the running command and returns {ok:false, timeout:true, background:true, job_id, pid, state, still_running, log_exists, log_tail}. Ignored when background=true (not validated in that mode).
- log_path (string): Advanced background-only override. Omit normally. If provided, must be a .log file directly under the local spool directory (e.g., /tmp/ssh-mcp/name.log on Unix, %TEMP%\ssh-mcp\name.log on Windows). Invalid custom paths return a tool JSON error.

BACKGROUND MODE:
For potentially long-running commands, use background=true:
1. Command runs detached on the remote host
2. Returns immediately with job_id, pid, LOCAL log_path on the MCP server
3. Monitor: use check_process with job_id (preferred) or ps -p <pid> -o pid,etime,cmd
4. View output: use check_process with job_id; inspect `state`, `log_exists`, `log_tail`; or tail -n 50 '<log_path>' (local spool file)

NOTE:
- check_process returns strict states: `running`, `completed`, `failed`, or `state_lost`.
- If `state_lost`, the MCP server no longer has a trustworthy terminal outcome; inspect `log_path` / `log_tail` before retrying.
- Commands are evaluated by POSIX-compatible sh on the remote host. Prefer portable shell syntax over shell-specific extensions.
- Keep file reads bounded with tools such as `head`, `tail`, or `sed`; use transfer with operation=get to retrieve large files.

EXAMPLE:
{"command": "apt update && apt install -y nginx", "background": true}"#;

    pub const SUDO_SHELL: &str = r#"SUDO_SHELL TOOL
Execute commands with sudo privileges via POSIX-compatible sh.

Same parameters as shell tool, but timeout behavior differs.
Requires passwordless sudo or pre-configured sudo password.

PARAMETERS:
- command (string, required): Command string executed by POSIX-compatible sh under sudo (use portable shell syntax)
- background (boolean): Run in background. Returns immediately with {job_id,pid,log_path,log_exists}.
  Output is streamed to local log file on MCP server. Monitor via check_process using job_id.
- timeout_ms (integer): Server-side foreground SSH wait limit only, not the full tool-call deadline. MCP does not expose the client's deadline to the server, so the client may stop waiting earlier. If reached, sudo_shell hands off the running command and returns {ok:false, timeout:true, background:true, job_id, pid, state, still_running, log_exists, log_tail, log_path}. Ignored when background=true (not validated in that mode).
- log_path (string): Advanced background-only override. Omit normally. If provided, must be a .log file directly under the local spool directory (e.g., /tmp/ssh-mcp/name.log on Unix, %TEMP%\ssh-mcp\name.log on Windows). Invalid custom paths return a tool JSON error.

NOTE:
- check_process returns strict states: `running`, `completed`, `failed`, or `state_lost`.
- If `state_lost`, the MCP server no longer has a trustworthy terminal outcome; inspect `log_path` / `log_tail` before retrying.
- Commands are evaluated by POSIX-compatible sh on the remote host. Prefer portable shell syntax over shell-specific extensions.
- Start long-running sudo commands explicitly with background=true so the initial response is not limited by the MCP client deadline.

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
- timeout_ms (integer): Server-side transfer timeout override. It does not extend the MCP client's tool-call deadline, which may expire earlier.

TRANSPORTS:
- auto: Tries rsync → sftp → scp → exec-raw in order
- sftp/scp/rsync: Require local OpenSSH binaries and --key
- exec-raw: Streaming via SSH exec (no OpenSSH needed)

SAFETY:
- local_path resolved within local_root (prevents ../ attacks)
- remote_path rejects paths starting with '-' or containing NUL

EXAMPLE:
{"operation": "put", "local_path": "config.yml", "remote_path": "/etc/app/config.yml"}"#;

    pub const APPLY_PATCH: &str = r#"APPLY_PATCH TOOL
Create, update, or delete one remote UTF-8 text file as the SSH user with an exact patch.

PARAMETERS:
- patch (string, required): One-file patch envelope using an absolute remote path

PATCH FORMAT:
- *** Begin Patch / *** End Patch envelope
- Exactly one *** Add File, *** Update File, or *** Delete File section
- Add body lines start with +
- Update hunks start with @@ and use exact space/+/- prefixed lines
- Missing or ambiguous context is an error; Move and multi-file patches are unsupported

BEHAVIOR:
- Add requires a missing path; Update and Delete require an existing UTF-8 regular file
- Resulting content is limited to 1048576 bytes and the parent directory must exist
- The tool reads the current file itself and rejects concurrent changes before commit
- Never elevates privileges; use sudo_apply_patch only when explicitly authorized
- Patch planning and remote commit failures return structured tool errors

EXAMPLE:
{"patch":"*** Begin Patch\n*** Delete File: /tmp/old.conf\n*** End Patch"}"#;

    pub const SUDO_APPLY_PATCH: &str = r#"SUDO_APPLY_PATCH TOOL
Apply the same exact, conflict-checked one-file patch as apply_patch, but read and commit under sudo.

PARAMETERS:
- patch (string, required): One-file Add/Update/Delete patch using an absolute remote path

BEHAVIOR:
- Uses the same parser, snapshot SHA check, lock, staging, and atomic commit as apply_patch
- Privilege elevation is explicit and never used as an automatic fallback
- Requires passwordless sudo or a configured sudo password
- Disabled together with sudo_shell by --disable-sudo

EXAMPLE:
{"patch":"*** Begin Patch\n*** Update File: /etc/example.conf\n@@\n-old\n+new\n*** End Patch"}"#;
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
                "default": false,
                "description": "Run asynchronously and return a job_id immediately. Use for potentially long commands because MCP client deadlines are client-specific and may expire before timeout_ms."
            },
            "timeout_ms": {
                "type": "integer",
                "description": "Server-side foreground SSH wait limit only, not the full tool-call deadline. The MCP client may stop waiting earlier; use background=true for potentially long-running commands."
            },
            "log_path": {
                "type": "string",
                "description": "Advanced background-only override. Omit normally; defaults to /tmp/ssh-mcp/<job_id>.log. If provided, must be a .log file directly under the local spool directory."
            }
        },
        "required": ["command"]
    });

    // Convert Value to JsonObject (Map<String, Value>)
    let schema_obj = schema.as_object().cloned().unwrap_or_default();

    Tool::new(name, tool_description, Arc::new(schema_obj))
}

pub(super) fn shell_tool() -> Tool {
    command_tool(
        "shell",
        "Run via POSIX sh; use head/tail/sed for bounded file reads and background=true for long commands.",
        "Command string executed by POSIX-compatible sh",
    )
}

pub(super) fn sudo_shell_tool() -> Tool {
    command_tool(
        "sudo_shell",
        "Run via POSIX sh under sudo; keep file reads bounded and use background=true for long commands.",
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
                "type": "integer",
                "description": "Server-side transfer timeout only. It does not extend the MCP client tool-call deadline, which may expire earlier."
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
                "description": "Job ID returned by shell/sudo_shell (required)"
            },
            "tail_lines": {
                "type": "integer",
                "default": 50,
                "description": "Number of lines to read from local log (default 50)"
            },
            "wait_for": {
                "type": "integer",
                "minimum": 0,
                "default": 0,
                "description": "If the initial state is running, wait locally for this many seconds before one final snapshot. Terminal states and errors return immediately. Cancelling the wait sends no stop signal to the remote job."
            }
        },
        "required": ["job_id"]
    });

    let schema_obj = schema.as_object().cloned().unwrap_or_default();
    Tool::new(
        "check_process",
        "Check status of a background process, optionally after a local passive wait. Cancelling the wait does not stop the remote job.",
        Arc::new(schema_obj),
    )
}

pub(super) fn apply_patch_tool() -> Tool {
    patch_tool(
        "apply_patch",
        "Apply one exact patch as the SSH user; never elevates privileges.",
    )
}

pub(super) fn sudo_apply_patch_tool() -> Tool {
    patch_tool(
        "sudo_apply_patch",
        "Apply one exact, conflict-checked remote file patch under sudo.",
    )
}

fn patch_tool(name: &'static str, description: &'static str) -> Tool {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "patch": {
                "type": "string",
                "description": "One-file Add/Update/Delete patch using an absolute remote path"
            }
        },
        "required": ["patch"],
        "additionalProperties": false
    });

    let schema_obj = schema.as_object().cloned().unwrap_or_default();
    Tool::new(name, description, Arc::new(schema_obj))
}

pub(super) fn get_tool_documentation(tool_name: &str) -> Option<&'static str> {
    match tool_name {
        "shell" => Some(tool_docs::SHELL),
        "sudo_shell" => Some(tool_docs::SUDO_SHELL),
        "transfer" => Some(tool_docs::TRANSFER),
        "apply_patch" => Some(tool_docs::APPLY_PATCH),
        "sudo_apply_patch" => Some(tool_docs::SUDO_APPLY_PATCH),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{check_process_tool, shell_tool, sudo_shell_tool, transfer_tool};

    #[test]
    fn check_process_schema_exposes_wait_for_seconds() {
        let tool = check_process_tool();
        let wait_for = &tool.input_schema["properties"]["wait_for"];

        assert_eq!(wait_for["type"], "integer");
        assert_eq!(wait_for["minimum"], 0);
        assert_eq!(wait_for["default"], 0);
    }

    #[test]
    fn command_tools_explain_client_specific_deadlines() {
        for tool in [shell_tool(), sudo_shell_tool()] {
            let description = tool.description.as_deref().expect("tool description");
            let timeout_description = tool.input_schema["properties"]["timeout_ms"]["description"]
                .as_str()
                .expect("timeout_ms description");

            assert!(description.contains("background=true"));
            assert!(timeout_description.contains("not the full tool-call deadline"));
            assert!(timeout_description.contains("may stop waiting earlier"));
            assert!(timeout_description.contains("background=true"));
            assert!(!timeout_description.contains("30s"));
        }
    }

    #[test]
    fn non_background_tools_do_not_promise_client_deadlines() {
        let tool = transfer_tool();
        let timeout_description = tool.input_schema["properties"]["timeout_ms"]["description"]
            .as_str()
            .expect("timeout_ms description");

        assert!(timeout_description.contains("does not extend"));
        assert!(timeout_description.contains("may expire earlier"));
        assert!(!timeout_description.contains("30s"));
    }
}
