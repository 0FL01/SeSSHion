use rmcp::model::{CallToolResult, Content};

pub(crate) const BACKGROUND_JSON_SNIPPET_LIMIT_CHARS: usize = 2048;

fn truncate_with_flag(input: &str, limit_chars: usize) -> (String, bool) {
    let mut iter = input.chars();
    let snippet: String = iter.by_ref().take(limit_chars).collect();
    let truncated = iter.next().is_some();
    (snippet, truncated)
}

pub(crate) fn background_json_ok(
    job_id: &str,
    pid: u32,
    local_log_path: &str,
    remote_log_path: &str,
) -> CallToolResult {
    let body = serde_json::json!({
        "ok": true,
        "background": true,
        "job_id": job_id,
        "pid": pid,
        "log_path": local_log_path,
        "remote_log_path": remote_log_path,
    })
    .to_string();

    CallToolResult::success(vec![Content::text(body)])
}

pub(crate) fn background_json_timeout(
    job_id: &str,
    pid: u32,
    local_log_path: &str,
    remote_log_path: &str,
) -> CallToolResult {
    let hint = format!(
        "TIMEOUT_RECOVERY: Process still running in background. DO NOT restart the command! Use check-process tool with job_id={job_id} to retrieve output."
    );
    let body = serde_json::json!({
        "ok": false,
        "timeout": true,
        "background": true,
        "job_id": job_id,
        "pid": pid,
        "log_path": local_log_path,
        "remote_log_path": remote_log_path,
        "hint": hint,
    })
    .to_string();

    CallToolResult::success(vec![Content::text(body)])
}

pub(crate) fn background_json_err(
    job_id: &str,
    local_log_path: &str,
    remote_log_path: Option<&str>,
    error: &str,
    stderr: &str,
) -> CallToolResult {
    // Keep the payload deterministic and single-line. Avoid echoing the original command.
    let (error_snippet, error_truncated) =
        truncate_with_flag(error, BACKGROUND_JSON_SNIPPET_LIMIT_CHARS);
    let (stderr_snippet, stderr_truncated) =
        truncate_with_flag(stderr, BACKGROUND_JSON_SNIPPET_LIMIT_CHARS);

    let truncated = error_truncated || stderr_truncated;

    let mut obj = serde_json::Map::new();
    obj.insert("ok".to_string(), serde_json::Value::Bool(false));
    obj.insert("background".to_string(), serde_json::Value::Bool(true));
    obj.insert(
        "job_id".to_string(),
        serde_json::Value::String(job_id.to_string()),
    );
    obj.insert(
        "log_path".to_string(),
        serde_json::Value::String(local_log_path.to_string()),
    );
    if let Some(remote) = remote_log_path {
        obj.insert(
            "remote_log_path".to_string(),
            serde_json::Value::String(remote.to_string()),
        );
    }
    obj.insert(
        "error".to_string(),
        serde_json::Value::String(error_snippet),
    );
    obj.insert(
        "stderr".to_string(),
        serde_json::Value::String(stderr_snippet),
    );
    obj.insert("truncated".to_string(), serde_json::Value::Bool(truncated));
    obj.insert(
        "truncated_fields".to_string(),
        serde_json::json!({
            "error": error_truncated,
            "stderr": stderr_truncated,
        }),
    );
    if truncated {
        obj.insert(
            "hint".to_string(),
            serde_json::Value::String(format!(
                "Response fields were truncated to {} chars. Hint: inspect full output using log_path or check-process with job_id={job_id}.",
                BACKGROUND_JSON_SNIPPET_LIMIT_CHARS
            )),
        );
    }

    let body = serde_json::Value::Object(obj).to_string();

    CallToolResult::success(vec![Content::text(body)])
}
