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

    pub const READ_FILE: &str = r#"READ-FILE TOOL
Read UTF-8 text file contents from the remote host via deterministic raw SSH streaming.

PARAMETERS:
- remote_path (string, required): Absolute remote file path to read
- mode (string): "preview" (default), "head", "tail", or "full"
- lines (integer): Line count for preview/head/tail (default: 800, max: 10000)
- timeout_ms (integer): Optional timeout override in milliseconds

BEHAVIOR:
- Uses raw streaming transport (no exec output token truncation)
- Default preview mode returns the first 800 lines to avoid context bombs
- Returns JSON with path/content/returned_lines/truncated/token estimates and optional hint
- Enforces deterministic size limits (aligned with max_output_tokens, hard-capped)
- Returns an error for missing path, non-file paths, oversized files, or invalid UTF-8 content

EXAMPLE:
{"remote_path": "/etc/nginx/nginx.conf", "mode": "preview"}"#;

    pub const APPLY_FILE_EDIT: &str = r#"APPLY-FILE-EDIT TOOL
Atomically overwrite/create or patch-replace a remote UTF-8 file with optional optimistic locking.

PARAMETERS:
- remote_path (string, required): Absolute remote file path to overwrite/create
- new_content (string): Full replacement content (max 1048576 bytes, full mode)
- old_text (string): Source text to replace (partial mode)
- new_text (string): Replacement text (partial mode)
- replace_all (boolean): Partial mode flag; default false. If false, exactly one match is required.
- expected_sha256 (string): Optional 64-char hex SHA-256 precondition of current file
- timeout_ms (integer): Optional timeout override in milliseconds

BEHAVIOR:
- Validates remote_path like read-file
- Runtime mode is selected by fields:
  - Full mode: set new_content only
  - Partial mode: set old_text and new_text only
  - Mixed/invalid combinations return invalid_params
- Acquires a per-file remote lock, computes SHA-256, and performs compare+replace in one transaction
- Creates the target file atomically when it is missing and the parent directory already exists
- Partial mode requires an existing regular file and never creates a new file
- Partial mode returns an error when old_text is not found
- Partial mode with replace_all=false requires exactly one match; otherwise returns an error asking to use replace_all
- If expected_sha256 is set and does not match, returns conflict and does not modify file
- If expected_sha256 is set while file is missing, returns conflict and does not create file
- Uses a same-directory sibling staging file and atomic rename
- Returns JSON: {"path":"...","previous_sha256":"...","new_sha256":"...","bytes_written":123,"changed":true}

EXAMPLE:
{"remote_path":"/etc/app.conf","new_content":"key=value\n","expected_sha256":"d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2"}"#;
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

pub(super) fn read_file_tool() -> Tool {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "remote_path": {
                "type": "string",
                "description": "Absolute remote file path to read"
            },
            "mode": {
                "type": "string",
                "enum": ["preview", "head", "tail", "full"],
                "default": "preview",
                "description": "Read mode: safe preview (default), head, tail, or full"
            },
            "lines": {
                "type": "integer",
                "minimum": 1,
                "maximum": 10000,
                "default": 800,
                "description": "Line count for preview/head/tail modes"
            },
            "timeout_ms": {
                "type": "integer",
                "description": "Optional timeout override in milliseconds"
            }
        },
        "required": ["remote_path"]
    });

    let schema_obj = schema.as_object().cloned().unwrap_or_default();
    Tool::new(
        "read-file",
        "Read a remote UTF-8 text file with safe preview/head/tail/full modes.",
        Arc::new(schema_obj),
    )
}

pub(super) fn apply_file_edit_tool() -> Tool {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "remote_path": {
                "type": "string",
                "description": "Absolute remote file path to overwrite/create"
            },
            "new_content": {
                "type": "string",
                "description": "Full replacement UTF-8 content (max 1048576 bytes)",
                "maxLength": 1048576
            },
            "old_text": {
                "type": "string",
                "description": "Partial mode: source text to replace"
            },
            "new_text": {
                "type": "string",
                "description": "Partial mode: replacement text"
            },
            "replace_all": {
                "type": "boolean",
                "default": false,
                "description": "Partial mode: replace all matches (default false requires exactly one match)"
            },
            "expected_sha256": {
                "type": "string",
                "description": "Optional 64-char hex SHA-256 precondition of current file"
            },
            "timeout_ms": {
                "type": "integer",
                "description": "Optional timeout override in milliseconds"
            }
        },
        "required": ["remote_path"],
        "oneOf": [
            {
                "required": ["remote_path", "new_content"],
                "not": {
                    "anyOf": [
                        {"required": ["old_text"]},
                        {"required": ["new_text"]},
                        {"required": ["replace_all"]}
                    ]
                }
            },
            {
                "required": ["remote_path", "old_text", "new_text"],
                "not": {
                    "required": ["new_content"]
                }
            }
        ]
    });

    let schema_obj = schema.as_object().cloned().unwrap_or_default();
    Tool::new(
        "apply-file-edit",
        "Atomically overwrite/create or patch-replace a remote file with optional SHA-256 precondition.",
        Arc::new(schema_obj),
    )
}

pub(super) fn get_tool_documentation(tool_name: &str) -> Option<&'static str> {
    match tool_name {
        "exec" => Some(tool_docs::EXEC),
        "sudo-exec" => Some(tool_docs::SUDO_EXEC),
        "transfer" => Some(tool_docs::TRANSFER),
        "read-file" => Some(tool_docs::READ_FILE),
        "apply-file-edit" => Some(tool_docs::APPLY_FILE_EDIT),
        _ => None,
    }
}
