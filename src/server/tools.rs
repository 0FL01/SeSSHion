use std::sync::Arc;

use rmcp::model::Tool;

fn command_tool(name: &'static str, tool_description: &'static str) -> Tool {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "command": {
                "type": "string"
            },
            "background": {
                "type": "boolean",
                "default": false,
                "description": "Run asynchronously and return job_id immediately; use for long commands."
            },
            "timeout_ms": {
                "type": "integer",
                "description": "Server-side foreground SSH wait limit; the client may stop waiting earlier."
            },
            "log_path": {
                "type": "string",
                "description": "Background-only local spool override; omit normally. Must be an absolute .log file directly in the spool directory."
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
        "POSIX sh: use explicit paths, not {a,b}; bound output and file reads.",
    )
}

pub(super) fn sudo_shell_tool() -> Tool {
    command_tool(
        "sudo_shell",
        "POSIX sh under sudo: use explicit paths, not {a,b}; bound output and file reads.",
    )
}

pub(super) fn transfer_tool(preferred: Option<&'static str>) -> Tool {
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
                "enum": ["exec-raw", "sftp", "scp", "rsync"],
                "description": "Explicit; no fallback. rsync/sftp/scp need keys on target+jump; exec-raw supports passwords."
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
                "description": "Server-side whole-transfer limit; client deadline may expire earlier."
            },
            "background": {
                "type": "boolean",
                "default": false,
                "description": "Return job_id; poll."
            }
        },
        "required": ["operation", "local_path", "remote_path", "transport"]
    });

    let schema_obj = schema.as_object().cloned().unwrap_or_default();
    let description = match preferred {
        Some(transport) => {
            format!("Files/dirs. Prefer transport={transport} (startup; may be stale).")
        }
        None => "Files/dirs. Choose transport explicitly (startup unverified).".to_string(),
    };
    Tool::new("transfer", description, Arc::new(schema_obj))
}

pub(super) fn check_process_tool() -> Tool {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "job_id": {
                "type": "string",
                "description": "job_id from any background tool"
            },
            "tail_lines": {
                "type": "integer",
                "default": 50
            },
            "wait_for": {
                "type": "integer",
                "minimum": 0,
                "default": 0,
                "description": "If initially running, wait locally this many seconds, then return one snapshot. Terminal states/errors return immediately; cancellation does not stop the remote job."
            }
        },
        "required": ["job_id"]
    });

    let schema_obj = schema.as_object().cloned().unwrap_or_default();
    Tool::new(
        "check_process",
        "Check a background job by job_id.",
        Arc::new(schema_obj),
    )
}

pub(super) fn apply_patch_tool() -> Tool {
    patch_tool(
        "apply_patch",
        "Exact multi-file patch as SSH user; never elevates.",
    )
}

pub(super) fn sudo_apply_patch_tool() -> Tool {
    patch_tool("sudo_apply_patch", "Exact multi-file patch under sudo.")
}

fn patch_tool(name: &'static str, description: &'static str) -> Tool {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "patch": {
                "type": "string",
                "description": "Add/Update/Delete; absolute paths. 1 MiB/file; no file/hunk cap or rollback."
            }
        },
        "required": ["patch"],
        "additionalProperties": false
    });

    let schema_obj = schema.as_object().cloned().unwrap_or_default();
    Tool::new(name, description, Arc::new(schema_obj))
}

#[cfg(test)]
mod tests {
    use super::{
        apply_patch_tool, check_process_tool, shell_tool, sudo_apply_patch_tool, sudo_shell_tool,
        transfer_tool,
    };

    #[test]
    fn check_process_schema_exposes_wait_for_seconds() {
        let tool = check_process_tool();
        let wait_for = &tool.input_schema["properties"]["wait_for"];

        assert_eq!(wait_for["type"], "integer");
        assert_eq!(wait_for["minimum"], 0);
        assert_eq!(wait_for["default"], 0);
        let description = wait_for["description"]
            .as_str()
            .expect("wait_for description");
        assert!(description.contains("one snapshot"));
        assert!(description.contains("does not stop the remote job"));
    }

    #[test]
    fn command_tools_explain_client_specific_deadlines() {
        for tool in [shell_tool(), sudo_shell_tool()] {
            let background_description =
                tool.input_schema["properties"]["background"]["description"]
                    .as_str()
                    .expect("background description");
            let timeout_description = tool.input_schema["properties"]["timeout_ms"]["description"]
                .as_str()
                .expect("timeout_ms description");
            let log_path_description = tool.input_schema["properties"]["log_path"]["description"]
                .as_str()
                .expect("log_path description");

            assert!(background_description.contains("job_id immediately"));
            assert!(background_description.contains("long commands"));
            assert!(timeout_description.contains("Server-side"));
            assert!(timeout_description.contains("client may stop waiting earlier"));
            assert!(log_path_description.contains("Background-only local spool"));
            assert!(log_path_description.contains("absolute .log file directly"));
            assert!(!timeout_description.contains("30s"));
        }
    }

    #[test]
    fn non_background_tools_do_not_promise_client_deadlines() {
        let tool = transfer_tool(None);
        let timeout_description = tool.input_schema["properties"]["timeout_ms"]["description"]
            .as_str()
            .expect("timeout_ms description");

        assert!(timeout_description.contains("Server-side"));
        assert!(timeout_description.contains("client deadline may expire earlier"));
        assert!(!timeout_description.contains("30s"));
    }

    #[test]
    fn transfer_requires_explicit_transport_and_describes_key_requirements() {
        let tool = transfer_tool(None);
        let schema = serde_json::to_value(&tool.input_schema).unwrap();
        let transport = &schema["properties"]["transport"];
        let transport_description = transport["description"]
            .as_str()
            .expect("transport description");

        assert_eq!(
            transport["enum"],
            serde_json::json!(["exec-raw", "sftp", "scp", "rsync"])
        );
        assert!(transport.get("default").is_none());
        assert_eq!(
            schema["required"],
            serde_json::json!(["operation", "local_path", "remote_path", "transport"])
        );
        assert!(transport_description.contains("Explicit; no fallback"));
        assert!(transport_description.contains("rsync/sftp/scp need keys on target+jump"));
        assert!(transport_description.contains("exec-raw supports passwords"));
    }

    #[test]
    fn transfer_description_exposes_preference_without_changing_schema() {
        let baseline = transfer_tool(None);
        for (preferred, description) in [
            (
                None,
                "Files/dirs. Choose transport explicitly (startup unverified).",
            ),
            (
                Some("sftp"),
                "Files/dirs. Prefer transport=sftp (startup; may be stale).",
            ),
            (
                Some("exec-raw"),
                "Files/dirs. Prefer transport=exec-raw (startup; may be stale).",
            ),
        ] {
            let tool = transfer_tool(preferred);
            assert_eq!(tool.name, baseline.name);
            assert_eq!(tool.input_schema, baseline.input_schema);
            assert_eq!(tool.description.as_deref(), Some(description));
            assert_eq!(
                serde_json::to_value(&tool).unwrap()["description"],
                description
            );
        }
    }

    #[test]
    fn default_tool_surface_stays_within_wire_budget() {
        const WIRE_BUDGET_BYTES: usize = 3200;
        for preferred in [None, Some("sftp"), Some("exec-raw")] {
            let tools = vec![
                shell_tool(),
                sudo_shell_tool(),
                sudo_apply_patch_tool(),
                check_process_tool(),
                transfer_tool(preferred),
                apply_patch_tool(),
            ];
            let names = tools
                .iter()
                .map(|tool| tool.name.as_ref())
                .collect::<Vec<_>>();
            assert_eq!(
                names,
                [
                    "shell",
                    "sudo_shell",
                    "sudo_apply_patch",
                    "check_process",
                    "transfer",
                    "apply_patch",
                ]
            );

            let bytes = serde_json::to_vec(&tools).expect("serialize default tool surface");
            println!(
                "tool surface ({preferred:?}): {} bytes (budget {WIRE_BUDGET_BYTES})",
                bytes.len()
            );
            assert!(
                bytes.len() <= WIRE_BUDGET_BYTES,
                "tool surface is {} bytes; budget is {WIRE_BUDGET_BYTES}",
                bytes.len()
            );
        }
    }
}
