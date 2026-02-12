use crate::shell_escape::escape_for_shell;

pub(crate) fn remote_job_log_path(job_id: &str) -> String {
    // Transitional behavior:
    // - Kept for API compatibility (exec/sudo-exec responses may still include remote_log_path).
    // - Current versions serve logs from local spool files on the MCP server.
    format!("/tmp/.ssh-mcp-job-{job_id}.log")
}

pub(crate) fn build_background_wrapper_script_full(
    job_id: &str,
    user_command: &str,
    log_path: &str,
) -> String {
    // The wrapper itself may be nested inside another `sh -lc '...'`.
    // Escape only for single-quoted contexts inside this wrapper.
    let escaped_user_command = escape_for_shell(user_command);
    let escaped_log_path = escape_for_shell(log_path);

    // Emit markers first, then `exec` the user command.
    // The wrapper PID becomes the command PID after `exec`.
    format!(
        "LOG='{escaped_log_path}'; \
  printf '%s\n' \"__SSH_MCP_JOB_ID={job_id}\"; \
  printf '%s\n' \"__SSH_MCP_PID=$$\"; \
  printf '%s\n' \"__SSH_MCP_LOG=$LOG\"; \
  exec sh -lc 'set +m; {escaped_user_command}'",
    )
}

pub(crate) fn build_background_wrapper_script_portable(
    job_id: &str,
    user_command: &str,
    log_path: &str,
) -> String {
    // The wrapper itself may be nested inside another `sh -lc '...'`.
    // Escape only for single-quoted contexts inside this wrapper.
    let escaped_user_command = escape_for_shell(user_command);
    let escaped_log_path = escape_for_shell(log_path);

    // BusyBox-friendly: avoid `sh -l`.
    // Emit markers first, then `exec` the user command.
    format!(
        "LOG='{escaped_log_path}'; \
  printf '%s\n' \"__SSH_MCP_JOB_ID={job_id}\"; \
  printf '%s\n' \"__SSH_MCP_PID=$$\"; \
  printf '%s\n' \"__SSH_MCP_LOG=$LOG\"; \
  exec sh -c 'set +m; {escaped_user_command}'",
    )
}
