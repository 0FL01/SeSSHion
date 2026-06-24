use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tracing::{debug, error, warn};

use crate::background::OutputStreamer;
use crate::background::detach::DetachMode;
use crate::background::marker::read_background_markers_from_channel;
use crate::background::response::{
    BackgroundTimeoutSnapshot, background_json_err, background_json_ok, background_json_timeout,
};
use crate::background::wrapper::remote_job_log_path;
use crate::ssh::command::read_local_log_tail;
use crate::ssh::sanitize::wrap_in_posix_shell;
use crate::ssh::{CommandOutput, sanitize_command, wrap_sudo_command};
use crate::tools::DEFAULT_CHECK_PROCESS_TAIL_LINES;

use super::SshMcpServer;
use super::validation::validate_background_log_path;

pub(super) enum BackgroundPrivilege<'a> {
    Normal,
    Sudo { password: Option<&'a str> },
}

struct TimeoutRecoverySnapshot {
    state: String,
    still_running: bool,
    exit_code: Option<u32>,
    state_reason: Option<String>,
    elapsed_time: String,
    log_exists: bool,
    log_tail: String,
    tail_lines_used: usize,
}

impl SshMcpServer {
    pub(super) async fn execute_detachable_foreground_impl(
        &self,
        detach_mode: DetachMode,
        command_for_exec: &str,
        command_for_registry: &str,
        timeout: Duration,
    ) -> std::result::Result<CallToolResult, McpError> {
        let job_id = super::make_job_id();

        let (final_log_path_buf, final_log_path) = match self.default_local_log_path(&job_id) {
            Ok(v) => v,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error: {e}"
                ))]));
            }
        };
        if let Err(e) = self.ensure_local_log_file(&final_log_path_buf).await {
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "Error: {e}"
            ))]));
        }

        let remote_log_path = remote_job_log_path(&job_id);
        let wrapper = super::build_background_wrapper_script(
            detach_mode,
            &job_id,
            command_for_exec,
            &remote_log_path,
        );

        let permit = match self.connection.acquire_command_slot_raw().await {
            Ok(p) => p,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error: Failed to acquire command slot: {e}"
                ))]));
            }
        };

        let wrapped_wrapper = wrap_in_posix_shell(&wrapper, false);
        let mut channel = match self
            .open_background_wrapper_channel_with_retry(wrapped_wrapper.as_str())
            .await
        {
            Ok(ch) => ch,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error: {e}"
                ))]));
            }
        };

        let (markers, initial_stdout) = match read_background_markers_from_channel(
            &mut channel,
            &job_id,
            &remote_log_path,
            super::BACKGROUND_START_TIMEOUT,
        )
        .await
        {
            Ok(v) => v,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error: {e}"
                ))]));
            }
        };

        self.register_running_job(
            &job_id,
            markers.pid,
            final_log_path_buf.clone(),
            command_for_registry,
        )
        .await;

        let streamer = OutputStreamer::new(
            job_id.clone(),
            final_log_path_buf.clone(),
            Arc::clone(&self.job_registry),
            self.spooler.clone(),
        );

        // The permit is only needed for opening the channel and reading markers
        // (both completed above). Streaming reads from an already-owned channel
        // and writes to a local file — it does not touch the SSH session or the
        // semaphore. Releasing the permit here prevents long-running detachable
        // commands from starving the command-slot pool and blocking check-process.
        drop(permit);
        let join =
            tokio::spawn(async move { streamer.stream_channel(channel, initial_stdout).await });

        let completed = tokio::time::timeout(timeout, join).await;
        let join_exit_code: Option<i32> = match completed {
            Ok(joined) => match joined {
                Ok(Ok(code)) => code,
                Ok(Err(e)) => {
                    error!(job_id = ?job_id, error = ?e, "streaming failed");
                    return Ok(CallToolResult::error(vec![Content::text(format!(
                        "Error: streaming failed: {e}"
                    ))]));
                }
                Err(e) => {
                    error!(job_id = ?job_id, error = ?e, "streaming task join failed");
                    return Ok(CallToolResult::error(vec![Content::text(format!(
                        "Error: streaming task join failed: {e}"
                    ))]));
                }
            },
            Err(_) => {
                let snapshot = self
                    .build_timeout_recovery_snapshot(&job_id, markers.pid, &final_log_path_buf)
                    .await;
                return Ok(background_json_timeout(
                    &job_id,
                    markers.pid,
                    &final_log_path,
                    &BackgroundTimeoutSnapshot {
                        state: &snapshot.state,
                        still_running: snapshot.still_running,
                        exit_code: snapshot.exit_code,
                        state_reason: snapshot.state_reason.as_deref(),
                        elapsed_time: &snapshot.elapsed_time,
                        log_exists: snapshot.log_exists,
                        log_tail: &snapshot.log_tail,
                        tail_lines_used: snapshot.tail_lines_used,
                    },
                ));
            }
        };

        let registry_exit_code: Option<i32> = match self.job_registry.get(&job_id).await {
            Some(job) => {
                let job_guard = job.lock().await;
                job_guard.exit_code
            }
            None => None,
        };

        let exit_code_u32 = registry_exit_code
            .or(join_exit_code)
            .and_then(|code| u32::try_from(code).ok())
            .unwrap_or(255)
            .min(255);

        let mut file = match tokio::fs::File::open(&final_log_path_buf).await {
            Ok(f) => f,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error: failed to read local log: {e}"
                ))]));
            }
        };

        let max_bytes = self.config.max_output_tokens.map(|t| t.saturating_mul(4));

        const TAIL_BYTES: u64 = 512;

        let stdout = match max_bytes {
            Some(limit) => {
                let meta = file.metadata().await.map_err(|e| {
                    McpError::internal_error(format!("failed to stat local log: {e}"), None)
                })?;
                let file_len = meta.len();

                if file_len <= limit as u64 {
                    let mut buf = Vec::new();
                    file.read_to_end(&mut buf).await.map_err(|e| {
                        McpError::internal_error(format!("failed to read local log: {e}"), None)
                    })?;
                    String::from_utf8_lossy(&buf).to_string()
                } else {
                    let mut head = vec![0u8; limit];
                    let mut read_total = 0usize;
                    while read_total < limit {
                        let n = file.read(&mut head[read_total..]).await.map_err(|e| {
                            McpError::internal_error(format!("failed to read local log: {e}"), None)
                        })?;
                        if n == 0 {
                            break;
                        }
                        read_total = read_total.saturating_add(n);
                    }
                    head.truncate(read_total);

                    let tail_len = std::cmp::min(TAIL_BYTES, file_len);
                    file.seek(std::io::SeekFrom::Start(file_len.saturating_sub(tail_len)))
                        .await
                        .map_err(|e| {
                            McpError::internal_error(format!("failed to seek local log: {e}"), None)
                        })?;

                    let mut tail = vec![0u8; tail_len as usize];
                    let mut tail_read = 0usize;
                    while tail_read < tail.len() {
                        let n = file.read(&mut tail[tail_read..]).await.map_err(|e| {
                            McpError::internal_error(
                                format!("failed to read local log tail: {e}"),
                                None,
                            )
                        })?;
                        if n == 0 {
                            break;
                        }
                        tail_read = tail_read.saturating_add(n);
                    }
                    tail.truncate(tail_read);

                    let total_tokens = (file_len as usize).saturating_div(4);
                    let mut out = String::from_utf8_lossy(&head).to_string();
                    out.push_str(&format!(
                        "\n[Output truncated: {} tokens total]",
                        total_tokens
                    ));
                    out.push_str(
                        "\n[Tip: Use 'head -n 100' for first lines, 'tail -n 100' for last lines]",
                    );
                    out.push_str("\n[Tip: For large output use SFTP/SCP tools to download files]");
                    if !tail.is_empty() {
                        out.push('\n');
                        out.push_str(&String::from_utf8_lossy(&tail));
                    }
                    out
                }
            }
            None => {
                let mut buf = Vec::new();
                file.read_to_end(&mut buf).await.map_err(|e| {
                    McpError::internal_error(format!("failed to read local log: {e}"), None)
                })?;
                String::from_utf8_lossy(&buf).to_string()
            }
        };

        let output = CommandOutput {
            stdout,
            stderr: String::new(),
            exit_code: Some(exit_code_u32),
            ..Default::default()
        };
        Ok(Self::calltool_from_command_output(output))
    }

    async fn build_timeout_recovery_snapshot(
        &self,
        job_id: &str,
        pid: u32,
        final_log_path_buf: &PathBuf,
    ) -> TimeoutRecoverySnapshot {
        let tail_lines_used = DEFAULT_CHECK_PROCESS_TAIL_LINES;

        match self
            .connection
            .check_process(
                job_id,
                tail_lines_used,
                self.job_registry.as_ref(),
                &self.spooler,
            )
            .await
        {
            Ok(status) => TimeoutRecoverySnapshot {
                state: status.state,
                still_running: status.running,
                exit_code: status.exit_code,
                state_reason: status.state_reason,
                elapsed_time: status.elapsed_time,
                log_exists: status.log_exists,
                log_tail: status.log_tail,
                tail_lines_used,
            },
            Err(e) => {
                warn!(
                    job_id = ?job_id,
                    pid,
                    error = ?e,
                    "Failed to gather timeout recovery snapshot via check-process path"
                );

                let mut still_running = true;
                let mut state = crate::background::JobStatus::Running.as_str().to_string();
                let mut exit_code = None;
                let mut state_reason = None;
                let mut elapsed_time = String::new();

                if let Some(job) = self.job_registry.get(job_id).await {
                    let job_guard = job.lock().await;
                    still_running =
                        matches!(job_guard.status, crate::background::JobStatus::Running);
                    state = job_guard.status.as_str().to_string();
                    exit_code = if matches!(
                        job_guard.status,
                        crate::background::JobStatus::Completed
                            | crate::background::JobStatus::Failed
                    ) {
                        job_guard
                            .exit_code
                            .and_then(|code| u32::try_from(code).ok())
                    } else {
                        None
                    };
                    state_reason = job_guard.state_reason.clone();
                    elapsed_time = job_guard.elapsed_time();
                }

                if !still_running && exit_code.is_none() {
                    state = crate::background::JobStatus::StateLost.as_str().to_string();
                    state_reason.get_or_insert_with(|| {
                        "timeout_recovery_missing_terminal_exit_code".to_string()
                    });
                }

                let log_exists = match tokio::fs::symlink_metadata(final_log_path_buf).await {
                    Ok(meta) => !meta.file_type().is_symlink() && meta.is_file(),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                    Err(log_err) => {
                        warn!(
                            job_id = ?job_id,
                            pid,
                            error = ?log_err,
                            "Failed to stat local log for timeout recovery snapshot"
                        );
                        false
                    }
                };

                let log_tail = match read_local_log_tail(
                    final_log_path_buf.as_path(),
                    tail_lines_used,
                )
                .await
                {
                    Ok(log_tail) => log_tail,
                    Err(log_err) => {
                        warn!(
                            job_id = ?job_id,
                            pid,
                            error = ?log_err,
                            "Failed to read local log tail for timeout recovery snapshot"
                        );
                        String::new()
                    }
                };

                TimeoutRecoverySnapshot {
                    state,
                    still_running,
                    exit_code: if still_running { None } else { exit_code },
                    state_reason,
                    elapsed_time,
                    log_exists,
                    log_tail,
                    tail_lines_used,
                }
            }
        }
    }

    async fn try_open_and_exec_background_wrapper(
        &self,
        wrapped_wrapper: &str,
    ) -> std::result::Result<russh::Channel<russh::client::Msg>, String> {
        let channel = self
            .connection
            .open_channel()
            .await
            .map_err(|e| format!("failed to open background channel: {e}"))?;

        channel
            .exec(true, wrapped_wrapper)
            .await
            .map_err(|e| format!("failed to send background exec request: {e}"))?;

        Ok(channel)
    }

    pub(super) async fn open_background_wrapper_channel_with_retry(
        &self,
        wrapped_wrapper: &str,
    ) -> std::result::Result<russh::Channel<russh::client::Msg>, String> {
        match self
            .try_open_and_exec_background_wrapper(wrapped_wrapper)
            .await
        {
            Ok(channel) => Ok(channel),
            Err(first_err) => {
                warn!(
                    error = ?first_err,
                    "Background wrapper pre-exec failed, reconnecting once"
                );

                if let Err(reconnect_err) = self.connection.reconnect().await {
                    return Err(format!(
                        "background pre-exec failed ({first_err}); reconnect failed: {reconnect_err}"
                    ));
                }

                self.try_open_and_exec_background_wrapper(wrapped_wrapper)
                    .await
                    .map_err(|retry_err| {
                        format!(
                            "background pre-exec failed ({first_err}); retry failed: {retry_err}"
                        )
                    })
            }
        }
    }

    pub(super) async fn execute_background_impl(
        &self,
        command: &str,
        log_path: Option<&str>,
        privilege: BackgroundPrivilege<'_>,
    ) -> std::result::Result<CallToolResult, McpError> {
        let job_id = super::make_job_id();
        let remote_log_path = remote_job_log_path(&job_id);

        let (final_log_path_buf, final_log_path) = match log_path {
            Some(p) => (PathBuf::from(p), p.to_string()),
            None => match self.default_local_log_path(&job_id) {
                Ok(v) => v,
                Err(e) => {
                    return Ok(background_json_err(&job_id, "", &e, ""));
                }
            },
        };

        if let Some(p) = log_path
            && let Err(e) = validate_background_log_path(self.spooler.base_dir(), p)
        {
            return Ok(background_json_err(&job_id, &final_log_path, &e, ""));
        }

        if let Err(e) = self.ensure_local_log_file(&final_log_path_buf).await {
            return Ok(background_json_err(
                &job_id,
                &final_log_path,
                &e.to_string(),
                "",
            ));
        }

        let sanitized = match sanitize_command(command, self.max_chars) {
            Ok(cmd) => cmd,
            Err(e) => {
                return Ok(background_json_err(
                    &job_id,
                    &final_log_path,
                    &e.to_string(),
                    "",
                ));
            }
        };

        // Ensure connection is established
        if let Err(e) = self.connection.ensure_connected().await {
            return Ok(background_json_err(
                &job_id,
                &final_log_path,
                &e.to_string(),
                "",
            ));
        }

        let (command_for_exec, command_for_registry, attempt_su_elevation, log_msg) =
            match privilege {
                BackgroundPrivilege::Normal => (
                    sanitized.clone(),
                    sanitized.clone(),
                    true,
                    "streaming failed for background job",
                ),
                BackgroundPrivilege::Sudo { password } => {
                    let wrapped_command = wrap_sudo_command(&sanitized, password);
                    debug!(
                        "Wrapped sudo command (password hidden): sudo -n sh -c '...' or printf '...' | sudo ..."
                    );
                    (
                        wrapped_command,
                        format!("sudo {sanitized}"),
                        false,
                        "streaming failed for background sudo job",
                    )
                }
            };

        // If su elevation is configured and available, ensure we're elevated (best-effort)
        if attempt_su_elevation
            && self.connection.get_su_password().is_some()
            && let Err(e) = self.connection.ensure_elevated().await
        {
            debug!(error = ?e, "Elevation failed, will run as normal user");
        }

        let detach_mode = match self.determine_detach_mode().await {
            Ok(mode) => mode,
            Err(e) => {
                return Ok(background_json_err(
                    &job_id,
                    &final_log_path,
                    &format!("Failed to determine background detach support: {e}"),
                    "",
                ));
            }
        };
        if detach_mode == DetachMode::DirectOnly {
            return Ok(background_json_err(
                &job_id,
                &final_log_path,
                "Background detach is not supported on this target; run with background=false.",
                "",
            ));
        }

        let wrapper = super::build_background_wrapper_script(
            detach_mode,
            &job_id,
            &command_for_exec,
            &remote_log_path,
        );

        let permit = match self.connection.acquire_command_slot_raw().await {
            Ok(p) => p,
            Err(e) => {
                return Ok(background_json_err(
                    &job_id,
                    &final_log_path,
                    &format!("Failed to acquire command slot: {e}"),
                    "",
                ));
            }
        };

        let wrapped_wrapper = wrap_in_posix_shell(&wrapper, false);
        let mut channel = match self
            .open_background_wrapper_channel_with_retry(wrapped_wrapper.as_str())
            .await
        {
            Ok(ch) => ch,
            Err(e) => {
                return Ok(background_json_err(&job_id, &final_log_path, &e, ""));
            }
        };

        let (markers, initial_stdout) = match read_background_markers_from_channel(
            &mut channel,
            &job_id,
            &remote_log_path,
            super::BACKGROUND_START_TIMEOUT,
        )
        .await
        {
            Ok(v) => v,
            Err(e) => {
                return Ok(background_json_err(&job_id, &final_log_path, &e, ""));
            }
        };

        self.register_running_job(
            &job_id,
            markers.pid,
            final_log_path_buf.clone(),
            &command_for_registry,
        )
        .await;

        let streamer = OutputStreamer::new(
            job_id.clone(),
            final_log_path_buf.clone(),
            Arc::clone(&self.job_registry),
            self.spooler.clone(),
        );

        let job_id_for_log = job_id.clone();

        // The permit is only needed for opening the channel and reading markers
        // (both completed above). Streaming reads from an already-owned channel
        // and writes to a local file — it does not touch the SSH session or the
        // semaphore. Releasing the permit here prevents long-running background
        // jobs from starving the command-slot pool and blocking check-process.
        drop(permit);
        tokio::spawn(async move {
            if let Err(e) = streamer.stream_channel(channel, initial_stdout).await {
                error!(job_id = ?job_id_for_log, error = ?e, "{log_msg}");
            }
        });

        Ok(background_json_ok(&job_id, markers.pid, &final_log_path))
    }
}
