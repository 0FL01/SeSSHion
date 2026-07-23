use rmcp::model::{CallToolResult, ContentBlock};

pub(crate) const BACKGROUND_JSON_SNIPPET_LIMIT_CHARS: usize = 2048;

pub(crate) struct BackgroundTimeoutSnapshot<'a> {
    pub still_running: bool,
    pub state: &'a str,
    pub exit_code: Option<u32>,
    pub state_reason: Option<&'a str>,
    pub elapsed_time: &'a str,
    pub log_exists: bool,
    pub log_tail: &'a str,
    pub tail_lines_used: usize,
}

fn truncate_with_flag(input: &str, limit_chars: usize) -> (String, bool) {
    let mut iter = input.chars();
    let snippet: String = iter.by_ref().take(limit_chars).collect();
    let truncated = iter.next().is_some();
    (snippet, truncated)
}

pub(crate) fn background_json_ok(job_id: &str, pid: u32, local_log_path: &str) -> CallToolResult {
    let body = serde_json::json!({
        "ok": true,
        "background": true,
        "job_id": job_id,
        "pid": pid,
        "log_path": local_log_path,
        "log_exists": true,
    })
    .to_string();

    CallToolResult::success(vec![ContentBlock::text(body)])
}

pub(crate) fn background_json_timeout(
    job_id: &str,
    pid: u32,
    local_log_path: &str,
    snapshot: &BackgroundTimeoutSnapshot<'_>,
) -> CallToolResult {
    let deadline_guidance = "MCP client deadlines may be shorter than timeout_ms; start potentially long commands with background=true.";
    let hint = if snapshot.still_running {
        format!(
            "TIMEOUT_RECOVERY: Process still running in background. DO NOT restart the command! Use check_process tool with job_id={job_id} to retrieve output. {deadline_guidance}"
        )
    } else if snapshot.state == "state_lost" {
        format!(
            "TIMEOUT_RECOVERY: Background job state is lost after handoff. Inspect log_path/log_tail and use check_process with job_id={job_id} before deciding whether to retry. {deadline_guidance}"
        )
    } else {
        format!(
            "TIMEOUT_RECOVERY: Foreground timeout elapsed after handoff, but the background job is no longer running. Inspect exit_code/log_tail or use check_process tool with job_id={job_id} before retrying. {deadline_guidance}"
        )
    };
    let body = serde_json::json!({
        "ok": false,
        "timeout": true,
        "background": true,
        "job_id": job_id,
        "pid": pid,
        "state": snapshot.state,
        "still_running": snapshot.still_running,
        "exit_code": snapshot.exit_code,
        "state_reason": snapshot.state_reason,
        "elapsed_time": snapshot.elapsed_time,
        "log_exists": snapshot.log_exists,
        "log_tail": snapshot.log_tail,
        "tail_lines_used": snapshot.tail_lines_used,
        "log_path": local_log_path,
        "hint": hint,
    })
    .to_string();

    CallToolResult::success(vec![ContentBlock::text(body)])
}

pub(crate) fn background_json_err(error: &str, stderr: &str) -> CallToolResult {
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
    let body = serde_json::Value::Object(obj).to_string();

    CallToolResult::success(vec![ContentBlock::text(body)])
}
