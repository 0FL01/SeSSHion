use rmcp::ErrorData as McpError;
use serde::Deserialize;

use serde_json::{Map, Value};

use super::SshMcpServer;
use crate::tools::CheckProcessParams;

#[derive(Debug, Clone)]
pub(super) struct CommonToolArgs {
    pub(super) command: String,
    pub(super) background: bool,
    pub(super) timeout_ms: Option<u64>,
    pub(super) log_path: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct CheckProcessToolArgs {
    #[serde(flatten)]
    pub(super) check: CheckProcessParams,
    #[serde(default)]
    pub(super) wait_for: u64,
}

pub(super) fn parse_common_tool_args(
    args: &Map<String, Value>,
) -> std::result::Result<CommonToolArgs, McpError> {
    let command = args
        .get("command")
        .and_then(|v| v.as_str())
        .ok_or_else(|| McpError::invalid_params("Missing required parameter: command", None))?
        .to_string();

    let background = args
        .get("background")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let invalid_timeout =
        || McpError::invalid_params("timeout_ms must be a positive integer", None);

    let timeout_ms: Option<u64> = if background {
        // Backward-compat: timeout_ms is ignored for background runs.
        // Intentionally do not validate type/value when background=true.
        None
    } else {
        match args.get("timeout_ms") {
            None => None,
            Some(v) if v.is_null() => None,
            Some(v) => {
                if let Some(u) = v.as_u64() {
                    if u == 0 {
                        return Err(invalid_timeout());
                    }
                    Some(u)
                } else if let Some(i) = v.as_i64() {
                    if i <= 0 {
                        return Err(invalid_timeout());
                    }
                    Some(i as u64)
                } else {
                    return Err(invalid_timeout());
                }
            }
        }
    };

    let log_path: Option<String> = if background {
        match args.get("log_path") {
            None => None,
            Some(v) if v.is_null() => None,
            Some(v) => {
                let s = v
                    .as_str()
                    .ok_or_else(|| McpError::invalid_params("log_path must be a string", None))?;

                // Path safety validation is handled in the background executor so
                // invalid custom paths become normal tool errors instead of MCP
                // protocol-level -32602 invalid_params responses.
                Some(s.to_string())
            }
        }
    } else {
        // Backward-compat: log_path is ignored for foreground runs.
        // Intentionally do not validate type/value when background=false.
        None
    };

    Ok(CommonToolArgs {
        command,
        background,
        timeout_ms,
        log_path,
    })
}

impl SshMcpServer {
    pub(super) fn parse_common_tool_args(
        &self,
        args: &serde_json::Map<String, serde_json::Value>,
    ) -> std::result::Result<CommonToolArgs, McpError> {
        parse_common_tool_args(args)
    }
}

#[cfg(test)]
mod tests {
    use rmcp::model::ErrorCode;
    use serde_json::json;

    use super::{CheckProcessToolArgs, parse_common_tool_args};

    #[test]
    fn check_process_wait_for_defaults_to_zero() {
        let parsed: CheckProcessToolArgs = serde_json::from_value(json!({
            "job_id": "job-123",
            "tail_lines": 10,
        }))
        .unwrap();

        assert_eq!(parsed.check.job_id, "job-123");
        assert_eq!(parsed.check.tail_lines, 10);
        assert_eq!(parsed.wait_for, 0);
    }

    #[test]
    fn check_process_wait_for_accepts_non_negative_seconds() {
        let parsed: CheckProcessToolArgs = serde_json::from_value(json!({
            "job_id": "job-123",
            "wait_for": 600,
        }))
        .unwrap();

        assert_eq!(parsed.wait_for, 600);
        assert!(
            serde_json::from_value::<CheckProcessToolArgs>(json!({
                "job_id": "job-123",
                "wait_for": -1,
            }))
            .is_err()
        );
    }

    #[test]
    fn background_true_ignores_timeout_ms_without_validation() {
        for timeout_val in [json!("oops"), json!(0), json!(-1)] {
            let params = json!({
                "command": "echo hi",
                "background": true,
                "timeout_ms": timeout_val,
            });
            let parsed = parse_common_tool_args(params.as_object().unwrap()).unwrap();
            assert!(parsed.background);
            assert!(parsed.timeout_ms.is_none());
        }
    }

    #[test]
    fn background_false_ignores_log_path_without_validation() {
        for log_path_val in [json!(123), json!("bad")] {
            let params = json!({
                "command": "echo hi",
                "background": false,
                "log_path": log_path_val,
            });
            let parsed = parse_common_tool_args(params.as_object().unwrap()).unwrap();
            assert!(!parsed.background);
            assert!(parsed.log_path.is_none());
        }
    }

    #[test]
    fn background_true_log_path_wrong_type_is_invalid_params() {
        let params = json!({
            "command": "echo hi",
            "background": true,
            "log_path": 123,
        });
        let err = parse_common_tool_args(params.as_object().unwrap()).unwrap_err();
        assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
    }

    #[test]
    fn background_true_log_path_invalid_is_parsed_for_tool_error() {
        let params = json!({
            "command": "echo hi",
            "background": true,
            "log_path": "/tmp/ssh-mcp/subdir/test.log",
        });
        let parsed = parse_common_tool_args(params.as_object().unwrap()).unwrap();
        assert_eq!(
            parsed.log_path.as_deref(),
            Some("/tmp/ssh-mcp/subdir/test.log")
        );
    }

    #[test]
    fn foreground_timeout_ms_invalid_has_single_message() {
        for timeout_val in [json!("oops"), json!(0), json!(-1)] {
            let params = json!({
                "command": "echo hi",
                "background": false,
                "timeout_ms": timeout_val,
            });
            let err = parse_common_tool_args(params.as_object().unwrap()).unwrap_err();
            assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
            assert_eq!(
                err.message.as_ref(),
                "timeout_ms must be a positive integer"
            );
        }
    }
}
