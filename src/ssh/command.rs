//! Command execution over SSH
//!
//! Provides the `CommandOutput` struct and `exec_command` functionality
//! for executing commands over an SSH connection with timeout support.
//!
//! This module is designed to be compatible with both GNU and BusyBox-based
//! systems (e.g., Debian/Ubuntu and Alpine Linux). All command detection and
//! process monitoring uses portable mechanisms that work across distributions.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use russh::ChannelMsg;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Mutex;
use tokio::time::timeout;
use tracing::{debug, error, warn};

use super::config::TIMEOUT_KILL_AFTER_SECS;
use super::connection::SshConnectionManager;
use super::sanitize::{escape_command_for_shell, escape_for_timeout_wrapper, wrap_in_posix_shell};
use crate::background::{JobRegistry, JobStatus, LocalLogSpooler, SharedJobState};
use crate::error::{Result, SshMcpError};
#[cfg(unix)]
use crate::platform::O_NOFOLLOW_FLAG;

const RAW_STREAM_BYTES_PER_TOKEN: usize = 4;
const RAW_STREAM_STDERR_HARD_MAX_BYTES: usize = 1024 * 1024;

/// Output from a command execution
#[derive(Debug, Clone, Default)]
pub struct CommandOutput {
    /// Standard output from the command
    pub stdout: String,

    /// Standard error from the command
    pub stderr: String,

    /// Exit code of the command (if available)
    pub exit_code: Option<u32>,

    /// Whether stdout was truncated due to output limits
    pub stdout_truncated: bool,

    /// Whether stderr was truncated due to output limits
    pub stderr_truncated: bool,

    /// Approximate total token count for stdout (including truncated content)
    pub stdout_total_tokens: usize,

    /// Approximate total token count for stderr (including truncated content)
    pub stderr_total_tokens: usize,
}

/// Output from a raw streaming command execution.
///
/// This is intended for binary-safe stdin/stdout streaming (e.g. file transfer).
#[derive(Debug, Clone, Default)]
pub struct TransferRawOutput {
    /// Total bytes written to remote stdout (as received).
    pub stdout_bytes: u64,

    /// Total bytes written to remote stdin.
    pub stdin_bytes: u64,

    /// Collected stderr (lossy UTF-8).
    pub stderr: String,

    /// Exit code of the remote command (if provided).
    pub exit_code: Option<u32>,
}

/// Process status check result
#[derive(Debug, Clone)]
pub struct ProcessStatus {
    /// PID of the background process on the remote host.
    pub pid: u32,
    /// Strict job state label: running, completed, failed, or state_lost.
    pub state: String,
    /// Whether the process is currently running
    pub running: bool,
    /// Exit code if process has completed
    pub exit_code: Option<u32>,
    /// Why the job entered state_lost, if known.
    pub state_reason: Option<String>,
    /// Elapsed time in ps format (e.g., "12:34" or "2-12:34:56")
    pub elapsed_time: String,
    /// Original command string tracked for this job.
    pub command: String,
    /// Absolute local log path on the MCP server.
    pub log_path: String,
    /// Whether the local log file currently exists.
    pub log_exists: bool,
    /// Tail of the log file (if log_path provided)
    pub log_tail: String,
}

impl CommandOutput {
    /// Create a new empty CommandOutput
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if the command succeeded (exit code 0 or no exit code available)
    pub fn success(&self) -> bool {
        self.exit_code.is_some_and(|code| code == 0)
    }

    /// Get combined output (stdout + stderr)
    pub fn combined_output(&self) -> String {
        if self.stderr.is_empty() {
            self.stdout.clone()
        } else if self.stdout.is_empty() {
            self.stderr.clone()
        } else {
            format!("{}\n{}", self.stdout, self.stderr)
        }
    }
}

/// Wrap a command with the timeout utility
///
/// Creates a wrapper command: `timeout -k {kill_after}s {duration}s sh -lc '{command}'`
///
/// The use of `sh -lc` ensures a login shell is used, which properly loads
/// environment variables like PATH from ~/.profile or /etc/profile.
///
/// # Arguments
/// * `command` - The command to wrap (should be pre-escaped)
/// * `duration_secs` - Timeout duration in seconds (supports fractional seconds like 0.5)
///
/// # Returns
/// A wrapped command string that includes timeout logic
pub fn wrap_command_with_timeout(command: &str, duration_secs: f64) -> String {
    let escaped_command = escape_for_timeout_wrapper(command);
    format!(
        "timeout -k {}s {}s sh -lc '{}'",
        TIMEOUT_KILL_AFTER_SECS, duration_secs, escaped_command
    )
}

fn wrap_command_for_channel_exec(command: &str) -> String {
    wrap_in_posix_shell(command, false)
}

fn validate_timeout_duration(timeout_duration: Duration) -> Result<f64> {
    // Convert duration to fractional seconds for millisecond precision.
    // as_secs_f64() preserves sub-second precision (e.g., 500ms -> 0.5, 1500ms -> 1.5).
    let duration_secs = timeout_duration.as_secs_f64();
    if !duration_secs.is_finite() || duration_secs <= 0.0 {
        return Err(SshMcpError::InvalidParams(
            "duration must be finite and > 0".to_string(),
        ));
    }
    Ok(duration_secs)
}

fn resolve_raw_stream_stderr_limit(max_output_tokens: Option<usize>) -> usize {
    max_output_tokens
        .and_then(|tokens| tokens.checked_mul(RAW_STREAM_BYTES_PER_TOKEN))
        .filter(|bytes| *bytes > 0)
        .unwrap_or(RAW_STREAM_STDERR_HARD_MAX_BYTES)
        .min(RAW_STREAM_STDERR_HARD_MAX_BYTES)
}

fn utf8_prefix_len(input: &str, max_bytes: usize) -> usize {
    if input.len() <= max_bytes {
        return input.len();
    }

    let mut end = max_bytes;
    while end > 0 && !input.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    end
}

fn append_bounded_lossy_stderr(stderr: &mut String, chunk: &[u8], max_len: usize) -> bool {
    if stderr.len() >= max_len {
        return true;
    }

    let chunk_str = String::from_utf8_lossy(chunk);
    let remaining = max_len.saturating_sub(stderr.len());
    if chunk_str.len() <= remaining {
        stderr.push_str(&chunk_str);
        return false;
    }

    let take = utf8_prefix_len(&chunk_str, remaining);
    if take > 0 {
        stderr.push_str(&chunk_str[..take]);
    }
    true
}

/// Errors that can occur before exec is successfully sent.
/// These errors are retryable since the command has not started executing yet.
enum PreExecError {
    ChannelOpen(String),
    ExecSend(String),
}

impl PreExecError {
    /// Convert the pre-exec error into an SSH connection error.
    fn into_ssh_error(self) -> SshMcpError {
        match self {
            PreExecError::ChannelOpen(msg) => SshMcpError::connection(msg),
            PreExecError::ExecSend(msg) => SshMcpError::connection(msg),
        }
    }
}

/// Errors that can occur when sending command to su shell channel.
/// These errors are retryable since the command has not started executing yet.
enum SuSendError {
    SendFailed(String),
}

impl SshConnectionManager {
    /// Execute a command over SSH
    ///
    /// This method:
    /// 1. Ensures the connection is active
    /// 2. If elevated (su shell), uses the PTY shell channel
    /// 3. Otherwise, opens a new exec channel
    /// 4. Collects stdout/stderr with timeout
    /// 5. On timeout, attempts graceful abort via pkill
    ///
    /// # Arguments
    /// * `command` - The command to execute (should be pre-sanitized)
    /// * `timeout_duration` - Maximum time to wait for command completion
    ///
    /// # Returns
    /// * `Ok(CommandOutput)` - Command output with stdout, stderr, and exit code
    /// * `Err(SshMcpError::Timeout)` - If command times out
    /// * `Err(SshMcpError::Connection)` - If connection issues occur
    pub async fn exec_command(
        &self,
        command: &str,
        timeout_duration: Duration,
    ) -> Result<CommandOutput> {
        // Acquire semaphore permit to limit concurrent command execution
        let _permit = self.acquire_command_slot().await?;

        // Ensure we're connected
        self.ensure_connected().await?;

        // Check if we have an elevated su shell
        if self.is_elevated() && self.has_su_channel().await {
            debug!("Using elevated su shell for command execution");
            return self.exec_via_su_shell(command, timeout_duration).await;
        }

        // Normal exec via new channel
        debug!("Using normal exec channel for command execution");
        self.exec_via_channel(command, timeout_duration).await
    }

    /// Execute command via the elevated su shell (PTY)
    ///
    /// Implements deterministic one-shot retry for pre-send failures:
    /// - If sending command to su channel fails: reset su state, re-elevate, retry once
    /// - If failure occurs after command is sent: no retry, reset su state and invalidate session
    async fn exec_via_su_shell(
        &self,
        command: &str,
        timeout_duration: Duration,
    ) -> Result<CommandOutput> {
        let duration_secs = validate_timeout_duration(timeout_duration)?;

        // Check timeout availability lazily on first use (same as exec_via_channel)
        let use_wrapper = self.determine_timeout_wrapper_usage().await;

        // Wrap command with timeout if available
        let wrapped_cmd = if use_wrapper {
            wrap_command_with_timeout(command, duration_secs)
        } else {
            command.to_string()
        };

        debug!(
            "Executing elevated command: cmd_len={}, wrapped_len={}, timeout_wrapped={}",
            command.len(),
            wrapped_cmd.len(),
            use_wrapper
        );

        // Attempt #1: try to send command via existing su channel
        let mut channel = match self.try_take_su_channel().await {
            Some(ch) => ch,
            None => {
                // No channel available - try to elevate and retry once
                warn!("No su channel available, attempting elevation");
                self.reset_su_state().await;
                self.ensure_elevated().await?;
                match self.try_take_su_channel().await {
                    Some(ch) => ch,
                    None => {
                        return Err(SshMcpError::connection(
                            "No su channel available after elevation",
                        ));
                    }
                }
            }
        };

        // Try to send the command
        match self
            .try_send_to_su_channel(&mut channel, &wrapped_cmd)
            .await
        {
            Ok(()) => {
                // Command sent successfully - collect output
                let result = self
                    .collect_su_output(&mut channel, timeout_duration, use_wrapper)
                    .await;

                // Put the channel back (even if collection failed)
                {
                    let mut guard = self.su_channel.lock().await;
                    *guard = Some(channel);
                }

                // Handle post-send failure: reset su state and invalidate session, no retry
                if let Err(ref e) = result {
                    warn!(error = ?e, "su channel failed after command sent");
                    self.reset_su_state().await;
                    self.invalidate_session("su channel failed after send")
                        .await;
                }

                result
            }
            Err(SuSendError::SendFailed(e)) => {
                // Pre-send failure: command was NOT sent
                // Drop the bad channel (don't put it back)
                drop(channel);

                // Reset su state and re-elevate once
                warn!(
                    error = ?e,
                    "su channel send failed (pre-send), resetting and re-elevating"
                );
                self.reset_su_state().await;
                self.ensure_elevated().await?;

                // Attempt #2: take new channel and send
                let mut channel = match self.try_take_su_channel().await {
                    Some(ch) => ch,
                    None => {
                        return Err(SshMcpError::connection(
                            "No su channel available after re-elevation",
                        ));
                    }
                };

                // Try to send again - if this fails, no more retries
                if let Err(SuSendError::SendFailed(e2)) = self
                    .try_send_to_su_channel(&mut channel, &wrapped_cmd)
                    .await
                {
                    // Second failure - drop channel, reset state, return error
                    drop(channel);
                    self.reset_su_state().await;
                    return Err(SshMcpError::connection(format!(
                        "Failed to send command to su channel after retry: {}",
                        e2
                    )));
                }

                // Second attempt succeeded - collect output
                let result = self
                    .collect_su_output(&mut channel, timeout_duration, use_wrapper)
                    .await;

                // Put the channel back
                {
                    let mut guard = self.su_channel.lock().await;
                    *guard = Some(channel);
                }

                // Handle post-send failure: reset su state and invalidate session, no retry
                if let Err(ref e) = result {
                    warn!(error = ?e, "su channel failed after command sent (retry)");
                    self.reset_su_state().await;
                    self.invalidate_session("su channel failed after send (retry)")
                        .await;
                }

                result
            }
        }
    }

    /// Try to take the su channel from the mutex
    async fn try_take_su_channel(&self) -> Option<russh::Channel<russh::client::Msg>> {
        let mut guard = self.su_channel.lock().await;
        guard.take()
    }

    /// Reset su state (clear channel and elevation flag)
    async fn reset_su_state(&self) {
        // Take channel out of mutex before awaiting to avoid deadlock
        let channel = {
            let mut guard = self.su_channel.lock().await;
            guard.take()
        };

        // Drop lock before awaiting EOF
        if let Some(ch) = channel {
            // Try to close gracefully, but don't wait
            let _ = ch.eof().await;
        }

        use std::sync::atomic::Ordering;
        self.is_elevated.store(false, Ordering::SeqCst);
        debug!("su state reset: channel cleared, is_elevated=false");
    }

    /// Try to send command to su channel
    /// Returns Ok(()) if sent successfully, Err(SuSendError) if send failed
    async fn try_send_to_su_channel(
        &self,
        channel: &mut russh::Channel<russh::client::Msg>,
        command: &str,
    ) -> std::result::Result<(), SuSendError> {
        let wrapped_command = wrap_command_for_channel_exec(command);
        channel
            .data(format!("{}\n", wrapped_command).as_bytes())
            .await
            .map_err(|e| SuSendError::SendFailed(e.to_string()))
    }

    /// Collect output from su channel until root prompt or error
    ///
    /// NOTE: This method is only reachable when ALL of the following hold:
    /// 1. `--su-password` is configured
    /// 2. `ensure_elevated()` succeeded (su PTY channel is open)
    /// 3. `detach_mode == DirectOnly` (neither nohup nor setsid available on remote)
    ///
    /// In practice, condition 3 is near-impossible: nohup (coreutils) and
    /// setsid (util-linux) are present on virtually every Linux system. When
    /// detach mode is Full or Portable, the shell tool uses
    /// `execute_detachable_foreground_impl` which runs commands through a
    /// background wrapper (`sh -lc`), never touching the su PTY channel.
    ///
    /// Empirical testing with `--su-password` on a real deployment confirmed
    /// the su PTY path is not activated — commands run as the SSH user, not
    /// root, because detach mode resolves to Full. The `#`-sentinel and
    /// hardcoded `exit_code: Some(0)` below are therefore not exercised in
    /// normal operation.
    async fn collect_su_output(
        &self,
        channel: &mut russh::Channel<russh::client::Msg>,
        timeout_duration: Duration,
        use_wrapper: bool,
    ) -> Result<CommandOutput> {
        let mut buffer = String::new();
        // When using wrapper, timeout is handled remotely - no local deadline needed
        let deadline = if use_wrapper {
            None
        } else {
            Some(tokio::time::Instant::now() + timeout_duration)
        };

        loop {
            if let Some(deadline_ref) = deadline
                && tokio::time::Instant::now() > deadline_ref
            {
                return Err(SshMcpError::Timeout(timeout_duration.as_millis() as u64));
            }

            let wait_result =
                tokio::time::timeout(Duration::from_millis(500), channel.wait()).await;

            match wait_result {
                Ok(Some(msg)) => {
                    match msg {
                        ChannelMsg::Data { data } => {
                            let text = String::from_utf8_lossy(&data);
                            buffer.push_str(&text);

                            // Check for root prompt - indicates command complete.
                            // The `#` sentinel is inherently fragile (any `#` in
                            // command output would match), but this path is only
                            // reachable when detach_mode == DirectOnly — see the
                            // method-level NOTE above. In normal deployments with
                            // nohup/setsid available, this code is never executed.
                            if buffer.contains('#') {
                                // Extract output: remove the command echo and final prompt
                                let lines: Vec<&str> = buffer.lines().collect();
                                // First line is often the echoed command; last line is the prompt
                                let output = if lines.len() > 2 {
                                    lines[1..lines.len() - 1].join("\n")
                                } else {
                                    String::new()
                                };

                                return Ok(CommandOutput {
                                    stdout: if output.is_empty() {
                                        output
                                    } else {
                                        format!("{}\n", output)
                                    },
                                    stderr: String::new(),
                                    exit_code: Some(0), // Assume success in PTY mode
                                    ..Default::default()
                                });
                            }
                        }
                        ChannelMsg::Close => {
                            return Err(SshMcpError::connection(
                                "Channel closed during command execution",
                            ));
                        }
                        _ => {
                            // Ignore other messages
                        }
                    }
                }
                Ok(None) => {
                    return Err(SshMcpError::connection(
                        "Channel ended during command execution",
                    ));
                }
                Err(_) => {
                    // Timeout on wait, continue loop
                    continue;
                }
            }
        }
    }

    /// Execute command via a new exec channel
    ///
    /// Implements deterministic one-shot retry for pre-exec failures:
    /// - Channel open failure: reconnect and retry once
    /// - channel.exec() send failure: reconnect and retry once
    /// - Failures after exec starts (output collection, Close/Eof): no retry,
    ///   just invalidate session so next command reconnects
    /// - Timeout errors: no retry (command may have partially run)
    async fn exec_via_channel(
        &self,
        command: &str,
        timeout_duration: Duration,
    ) -> Result<CommandOutput> {
        let duration_secs = validate_timeout_duration(timeout_duration)?;

        // Wrap command with timeout if available
        // Check timeout availability lazily on first use
        let use_wrapper = self.determine_timeout_wrapper_usage().await;

        let wrapped_cmd = if use_wrapper {
            wrap_command_with_timeout(command, duration_secs)
        } else {
            // Fall back to old method: use tokio timeout + pkill
            command.to_string()
        };

        // Attempt #1: open channel and exec
        let (channel, _exec_sent) = self
            .open_and_exec_with_reconnect_retry(&wrapped_cmd)
            .await?;

        // At this point, exec has been sent successfully.
        // Collect output with appropriate timeout strategy.
        // Failures here do NOT trigger retry - we just invalidate the session.
        let output_result = if use_wrapper {
            // When using wrapper, timeout is handled remotely - no tokio timeout needed
            self.collect_channel_output(channel).await
        } else {
            // Fall back: use tokio timeout + pkill for abort
            let result = timeout(timeout_duration, self.collect_channel_output(channel)).await;

            match result {
                Ok(inner_result) => inner_result,
                Err(_) => {
                    // Timeout occurred - attempt graceful abort
                    warn!(
                        "Command timed out after {}ms, attempting abort",
                        timeout_duration.as_millis()
                    );
                    self.abort_command(command).await;
                    self.invalidate_session("command timed out after exec")
                        .await;
                    return Err(SshMcpError::Timeout(timeout_duration.as_millis() as u64));
                }
            }
        };

        let output = match output_result {
            Ok(out) => out,
            Err(e) => {
                // Failure after exec started - invalidate session, no retry
                // Do not retry: command may have partially executed
                if !matches!(e, SshMcpError::Timeout(_)) {
                    self.invalidate_session("channel failed after exec").await;
                }
                return Err(e);
            }
        };

        // Check if timeout command failed (e.g., not found) when using wrapper
        if use_wrapper {
            let stderr_lower = output.stderr.to_lowercase();
            // Check for timeout command not found errors (multiple languages)
            let timeout_not_found = stderr_lower.contains("timeout: command not found")
                || stderr_lower.contains("timeout: не найдена команда")
                || stderr_lower.contains("timeout: introuvable")
                || stderr_lower.contains("timeout: команда не найдена");

            if timeout_not_found {
                error!("timeout command not available on remote host, enabling fallback");
                self.disable_timeout_wrapper();

                // Execute the command again using fallback method (tokio timeout + pkill)
                // Note: This is a feature fallback, not a connection retry
                let (channel, _) = self
                    .open_and_exec_with_reconnect_retry(command)
                    .await
                    .map_err(|e| {
                        SshMcpError::connection(format!(
                            "Failed to start fallback execution after reconnect retry: {e}"
                        ))
                    })?;

                let result = timeout(timeout_duration, self.collect_channel_output(channel)).await;

                return match result {
                    Ok(inner_output) => inner_output,
                    Err(_) => {
                        warn!(
                            "Command timed out after {}ms (fallback), attempting abort",
                            timeout_duration.as_millis()
                        );
                        self.abort_command(command).await;
                        self.invalidate_session("fallback command timed out after exec")
                            .await;
                        Err(SshMcpError::Timeout(timeout_duration.as_millis() as u64))
                    }
                };
            }

            // Check if the command was killed by timeout
            // timeout returns 124 when it kills the command
            if output.exit_code == Some(124) {
                warn!("Command timed out (timeout wrapper returned 124)");
                return Err(SshMcpError::Timeout(timeout_duration.as_millis() as u64));
            }
        }

        Ok(output)
    }

    /// Try to open a channel and send exec command
    ///
    /// Returns the channel and a boolean indicating exec was sent successfully.
    /// Separates pre-exec failures (which can be retried) from post-exec state.
    async fn try_open_and_exec(
        &self,
        command: &str,
    ) -> std::result::Result<(russh::Channel<russh::client::Msg>, bool), PreExecError> {
        let channel = self
            .open_channel()
            .await
            .map_err(|e| PreExecError::ChannelOpen(e.to_string()))?;

        debug!("Executing command: cmd_len={}", command.len());
        let wrapped_command = wrap_command_for_channel_exec(command);
        channel
            .exec(true, wrapped_command.as_str())
            .await
            .map_err(|e| PreExecError::ExecSend(format!("Failed to exec command: {}", e)))?;

        Ok((channel, true))
    }

    async fn open_and_exec_with_reconnect_retry(
        &self,
        command: &str,
    ) -> Result<(russh::Channel<russh::client::Msg>, bool)> {
        match self.try_open_and_exec(command).await {
            Ok(result) => Ok(result),
            Err(pre_exec_err) => {
                match &pre_exec_err {
                    PreExecError::ChannelOpen(e) => {
                        warn!(
                            error = ?e,
                            "Channel open failed, attempting reconnect and retry"
                        );
                    }
                    PreExecError::ExecSend(e) => {
                        warn!(error = ?e, "Exec send failed, attempting reconnect and retry");
                    }
                }

                self.reconnect().await?;
                self.try_open_and_exec(command)
                    .await
                    .map_err(|retry_err| retry_err.into_ssh_error())
            }
        }
    }

    /// Collect output from a channel until it closes
    ///
    /// Implements output limiting to prevent OOM and context overflow.
    /// Approximate token count: 1 token ≈ 4 bytes for UTF-8 text.
    async fn collect_channel_output(
        &self,
        mut channel: russh::Channel<russh::client::Msg>,
    ) -> Result<CommandOutput> {
        // Approximate: 1 token ≈ 4 bytes for estimation
        const BYTES_PER_TOKEN: usize = 4;
        // Keep a small tail of truncated output so callers can still see
        // end-of-command markers (e.g. "done").
        const TAIL_BYTES: usize = 512;

        let mut output = CommandOutput::new();

        // Calculate byte limit from config (if set)
        let max_bytes = self
            .config
            .max_output_tokens
            .map(|tokens| tokens.saturating_mul(BYTES_PER_TOKEN));

        // Track total tokens received (including what was truncated)
        let mut total_stdout_tokens: usize = 0;
        let mut total_stderr_tokens: usize = 0;

        // Flags to track if we've already added truncation messages
        let mut stdout_truncation_added = false;
        let mut stderr_truncation_added = false;

        let mut stdout_tail: String = String::new();
        let mut stderr_tail: String = String::new();

        let push_tail = |buf: &mut String, chunk: &str| {
            if chunk.is_empty() {
                return;
            }
            buf.push_str(chunk);
            if buf.len() > TAIL_BYTES {
                let start = buf.len().saturating_sub(TAIL_BYTES);
                let mut safe_start = start;
                while safe_start > 0 && !buf.is_char_boundary(safe_start) {
                    safe_start = safe_start.saturating_sub(1);
                }
                if safe_start > 0 {
                    buf.drain(..safe_start);
                }
            }
        };

        while let Some(msg) = channel.wait().await {
            match msg {
                ChannelMsg::Data { data } => {
                    let data_len = data.len();
                    total_stdout_tokens =
                        total_stdout_tokens.saturating_add(data_len / BYTES_PER_TOKEN);
                    let data_str = String::from_utf8_lossy(&data);

                    if let Some(limit) = max_bytes {
                        let current_len = output.stdout.len();

                        // Check if we need to truncate
                        if current_len.saturating_add(data_str.len()) > limit {
                            if !stdout_truncation_added {
                                // Calculate how much we can take
                                let remaining = limit.saturating_sub(current_len);
                                let mut take: usize = 0;
                                if remaining > 0 {
                                    // Safe slicing: we know remaining is within bounds since data_str.len() > remaining
                                    let safe_end = data_str
                                        .char_indices()
                                        .map(|(i, _)| i)
                                        .find(|&i| i > remaining)
                                        .unwrap_or(data_str.len());
                                    take = std::cmp::min(safe_end, remaining);
                                    output.stdout.push_str(&data_str[..take]);
                                }
                                output.stdout_truncated = true;
                                output.stdout_total_tokens = total_stdout_tokens;

                                // Add truncation notice with tips
                                output.stdout.push_str(&format!(
                                    "\n[Output truncated: {} tokens total]",
                                    total_stdout_tokens
                                ));
                                output.stdout.push_str(
                                    "\n[Tip: Use 'head -n 100' for first lines, 'tail -n 100' for last lines]",
                                );
                                output.stdout.push_str(
                                    "\n[Tip: For large output use SFTP/SCP tools to download files]",
                                 );

                                stdout_truncation_added = true;
                                warn!(
                                    "stdout truncated: total_tokens={}, limit_tokens={}",
                                    total_stdout_tokens,
                                    max_bytes.map(|b| b / BYTES_PER_TOKEN).unwrap_or(0)
                                );

                                push_tail(&mut stdout_tail, &data_str[take..]);
                            } else {
                                push_tail(&mut stdout_tail, &data_str);
                            }
                            // Skip remaining stdout data
                        } else {
                            output.stdout.push_str(&data_str);
                        }
                    } else {
                        // No limit - add all data
                        output.stdout.push_str(&data_str);
                    }
                }
                ChannelMsg::ExtendedData { data, ext } => {
                    let data_len = data.len();
                    total_stderr_tokens =
                        total_stderr_tokens.saturating_add(data_len / BYTES_PER_TOKEN);

                    // ext == 1 is typically stderr
                    if ext == 1 {
                        let data_str = String::from_utf8_lossy(&data);
                        if let Some(limit) = max_bytes {
                            let current_len = output.stderr.len();

                            // Check if we need to truncate
                            if current_len.saturating_add(data_str.len()) > limit {
                                if !stderr_truncation_added {
                                    // Calculate how much we can take
                                    let remaining = limit.saturating_sub(current_len);
                                    let mut take: usize = 0;
                                    if remaining > 0 {
                                        // Safe slicing: find UTF-8 safe boundary
                                        let safe_end = data_str
                                            .char_indices()
                                            .map(|(i, _)| i)
                                            .find(|&i| i > remaining)
                                            .unwrap_or(data_str.len());
                                        take = std::cmp::min(safe_end, remaining);
                                        output.stderr.push_str(&data_str[..take]);
                                    }
                                    output.stderr_truncated = true;
                                    output.stderr_total_tokens = total_stderr_tokens;

                                    // Add truncation notice
                                    output.stderr.push_str(&format!(
                                        "\n[Output truncated: {} tokens total]",
                                        total_stderr_tokens
                                    ));
                                    output.stderr.push_str(
                                        "\n[Tip: Use 'head -n 100' for first lines, 'tail -n 100' for last lines]",
                                    );
                                    output.stderr.push_str(
                                        "\n[Tip: For large output use SFTP/SCP tools to download files]",
                                    );

                                    stderr_truncation_added = true;
                                    warn!(
                                        "stderr truncated: total_tokens={}, limit_tokens={}",
                                        total_stderr_tokens,
                                        max_bytes.map(|b| b / BYTES_PER_TOKEN).unwrap_or(0)
                                    );

                                    push_tail(&mut stderr_tail, &data_str[take..]);
                                } else {
                                    push_tail(&mut stderr_tail, &data_str);
                                }
                                // Skip remaining stderr data
                            } else {
                                output.stderr.push_str(&data_str);
                            }
                        } else {
                            // No limit - add all data
                            output.stderr.push_str(&data_str);
                        }
                    } else {
                        // Non-stderr extended data goes to stdout
                        output.stdout.push_str(&String::from_utf8_lossy(&data));
                    }
                }
                ChannelMsg::ExitStatus { exit_status } => {
                    output.exit_code = Some(exit_status);
                }
                ChannelMsg::ExitSignal { signal_name, .. } => {
                    // Map signal to conventional shell exit code (128 + signal),
                    // matching stream_channel_inner in background/stream.rs.
                    let code = match signal_name {
                        russh::Sig::HUP => 129,
                        russh::Sig::INT => 130,
                        russh::Sig::QUIT => 131,
                        russh::Sig::ILL => 132,
                        russh::Sig::ABRT => 134,
                        russh::Sig::FPE => 136,
                        russh::Sig::KILL => 137,
                        russh::Sig::USR1 => 138,
                        russh::Sig::SEGV => 139,
                        russh::Sig::PIPE => 141,
                        russh::Sig::ALRM => 142,
                        russh::Sig::TERM => 143,
                        russh::Sig::Custom(_) => 128,
                    };
                    output.exit_code = Some(code);
                }
                ChannelMsg::Close | ChannelMsg::Eof => {
                    // Don't break - ExitStatus may arrive after Close/Eof
                    // Loop will exit naturally when channel.wait() returns None
                }
                _ => {
                    // Ignore other messages
                }
            }
        }

        // Store final token counts (if not already set from truncation)
        if output.stdout_total_tokens == 0 {
            output.stdout_total_tokens = total_stdout_tokens;
        }
        if output.stderr_total_tokens == 0 {
            output.stderr_total_tokens = total_stderr_tokens;
        }

        if output.stdout_truncated && !stdout_tail.is_empty() {
            output.stdout.push('\n');
            output.stdout.push_str(&stdout_tail);
        }

        if output.stderr_truncated && !stderr_tail.is_empty() {
            output.stderr.push('\n');
            output.stderr.push_str(&stderr_tail);
        }

        // If there's stderr and a non-zero exit code, we might want to handle it
        // For now, just return the output as-is
        debug!(
            "Command completed: exit_code={:?}, stdout_len={}, stderr_len={}, stdout_truncated={}, stderr_truncated={}",
            output.exit_code,
            output.stdout.len(),
            output.stderr.len(),
            output.stdout_truncated,
            output.stderr_truncated
        );

        // A channel that closed without an exit status or exit signal indicates
        // the SSH session was torn down (e.g. concurrent invalidate_session,
        // network drop, server kill).  Returning Ok with exit_code=None would
        // be treated as success by calltool_from_command_output — a silent
        // failure.  Return an explicit error instead so the caller can surface
        // it and trigger reconnection.
        if output.exit_code.is_none() {
            return Err(SshMcpError::connection(
                "SSH channel closed without exit status (session may have been torn down)",
            ));
        }

        Ok(output)
    }

    /// Attempt to abort a running command by killing matching processes
    ///
    /// Sends `timeout 3s pkill -f 'command' 2>/dev/null || true` to kill
    /// any processes matching the command pattern.
    async fn abort_command(&self, command: &str) {
        // Try to open a new channel for the abort command
        let channel = match self.open_channel().await {
            Ok(ch) => ch,
            Err(e) => {
                error!(error = ?e, "Failed to open channel for abort");
                return;
            }
        };

        let escaped_command = escape_command_for_shell(command);
        let abort_cmd = format!(
            "timeout 3s pkill -f '{}' 2>/dev/null || true",
            escaped_command
        );

        debug!(
            "Sending abort command: pattern_len={}, abort_len={}",
            command.len(),
            abort_cmd.len()
        );

        if let Err(e) = channel.exec(true, abort_cmd.as_str()).await {
            error!(error = ?e, "Failed to exec abort command");
            return;
        }

        // Wait briefly for abort to complete (max 5 seconds)
        let abort_timeout = Duration::from_secs(5);
        let _ = timeout(abort_timeout, async {
            let mut channel = channel;
            while let Some(msg) = channel.wait().await {
                match msg {
                    ChannelMsg::Close | ChannelMsg::Eof => break,
                    _ => continue,
                }
            }
        })
        .await;

        debug!("Abort command completed");
    }

    /// Execute a command over SSH with binary-safe streaming.
    ///
    /// This method is designed for use-cases like file transfer where stdout must
    /// be treated as bytes and forwarded to a sink without UTF-8 decoding.
    ///
    /// Notes:
    /// - This does not use the interactive su shell.
    /// - Timeouts are enforced locally via tokio timeout.
    pub async fn exec_raw_streaming<R, W>(
        &self,
        command: &str,
        mut stdin: Option<&mut R>,
        mut stdout: Option<&mut W>,
        timeout_duration: Duration,
    ) -> Result<TransferRawOutput>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let _permit = self.acquire_command_slot().await?;

        self.ensure_connected().await?;

        // Raw transfers must not reuse the PTY/su channel.
        let fut = async {
            let channel = self.open_channel().await?;
            channel
                .exec(true, command)
                .await
                .map_err(|e| SshMcpError::connection(format!("Failed to exec command: {e}")))?;

            // Prevent deadlocks by pumping stdin and stdout/stderr concurrently.
            // stdin/stdout are borrowed, so we keep IO in this task and run the SSH channel
            // event loop in a spawned task (owned channel).
            let (stdin_tx, mut stdin_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
            let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<RawStreamEvent>(8);

            let task_guard = JoinAbortGuard::new(tokio::spawn(async move {
                raw_channel_task(channel, &mut stdin_rx, out_tx).await
            }));

            let mut output = TransferRawOutput::default();
            let stderr_limit_bytes = resolve_raw_stream_stderr_limit(self.config.max_output_tokens);
            let mut total_stderr_bytes = 0usize;
            let mut stderr_truncated = false;
            let mut stdin_done = stdin.is_none();
            let mut stdin_tx: Option<tokio::sync::mpsc::Sender<Vec<u8>>> =
                if stdin_done { None } else { Some(stdin_tx) };
            let mut channel_closed = false;
            let mut out_rx_closed = false;

            let mut buf = vec![0u8; 32 * 1024];

            loop {
                if stdin_done && channel_closed && out_rx_closed {
                    break;
                }

                tokio::select! {
                    read_res = async {
                        match stdin.as_mut() {
                            Some(r) => r.read(&mut buf).await,
                            None => Ok(0),
                        }
                    }, if !stdin_done => {
                        let n = read_res?;
                        if n == 0 {
                            stdin_done = true;
                            stdin_tx = None; // drop -> EOF
                        } else {
                            let chunk = buf[..n].to_vec();
                            match stdin_tx.as_mut() {
                                Some(tx) => {
                                    tx.send(chunk).await.map_err(|_| {
                                        SshMcpError::connection("raw channel task ended while sending stdin".to_string())
                                    })?;
                                    output.stdin_bytes += n as u64;
                                }
                                None => {
                                    return Err(SshMcpError::connection(
                                        "raw stdin channel closed unexpectedly".to_string(),
                                    ));
                                }
                            }
                        }
                    }
                    maybe_evt = out_rx.recv() => {
                        match maybe_evt {
                            Some(RawStreamEvent::Stdout(data)) => {
                                output.stdout_bytes += data.len() as u64;
                                if let Some(writer) = stdout.as_mut() {
                                    writer.write_all(&data).await?;
                                }
                            }
                            Some(RawStreamEvent::Stderr(data)) => {
                                total_stderr_bytes = total_stderr_bytes.saturating_add(data.len());
                                if !stderr_truncated {
                                    stderr_truncated = append_bounded_lossy_stderr(
                                        &mut output.stderr,
                                        &data,
                                        stderr_limit_bytes,
                                    );
                                    if stderr_truncated {
                                        warn!(
                                            total_stderr_bytes,
                                            stderr_limit_bytes,
                                            "raw streaming stderr truncated"
                                        );
                                    }
                                }
                            }
                            Some(RawStreamEvent::ExitStatus(code)) => {
                                output.exit_code = Some(code);
                            }
                            Some(RawStreamEvent::Closed) => {
                                channel_closed = true;
                            }
                            None => {
                                out_rx_closed = true;
                            }
                        }
                    }
                }
            }

            if stderr_truncated {
                output.stderr.push_str(&format!(
                    "\n[stderr truncated: {} bytes total, limit {} bytes]",
                    total_stderr_bytes, stderr_limit_bytes
                ));
            }

            if let Some(writer) = stdout.as_mut() {
                writer.flush().await?;
            }

            let join_handle = match task_guard.into_handle() {
                Some(h) => h,
                None => {
                    return Err(SshMcpError::connection(
                        "raw channel task handle missing".to_string(),
                    ));
                }
            };

            match join_handle.await {
                Ok(Ok(())) => Ok(output),
                Ok(Err(e)) => Err(e),
                Err(e) => Err(SshMcpError::connection(format!(
                    "raw channel task join failed: {e}"
                ))),
            }
        };

        match timeout(timeout_duration, fut).await {
            Ok(res) => res,
            Err(_) => {
                self.invalidate_session("raw command timed out").await;
                Err(SshMcpError::Timeout(timeout_duration.as_millis() as u64))
            }
        }
    }

    /// Check the status of a background job by job_id.
    ///
    /// Uses `kill -0` for process detection (existence/permission check without sending a signal).
    /// This avoids parsing `ps` output (GNU vs BusyBox differences) and works on common Linux
    /// distributions.
    ///
    /// # Arguments
    /// * `job_id` - Job id returned by background exec
    /// * `tail_lines` - Number of lines to read from log tail
    /// * `registry` - Job registry holding current job state
    ///
    /// # Returns
    /// ProcessStatus with running state, exit code, elapsed time, command, and log tail
    pub async fn check_process(
        &self,
        job_id: &str,
        tail_lines: usize,
        registry: &JobRegistry,
        spooler: &LocalLogSpooler,
    ) -> Result<ProcessStatus> {
        debug!(job_id = ?job_id, "Checking process status");

        let job = match registry.get(job_id).await {
            Some(job) => job,
            None => match spooler.load_job_state(job_id).await {
                Ok(Some(recovered)) => {
                    let shared = Arc::new(Mutex::new(recovered));
                    registry
                        .insert(job_id.to_string(), Arc::clone(&shared))
                        .await;
                    shared
                }
                Ok(None) => {
                    return Err(SshMcpError::invalid_params(format!(
                        "job not found: {job_id}"
                    )));
                }
                Err(e) => {
                    return Err(SshMcpError::invalid_params(format!(
                        "failed to recover job state for {job_id}: {e}"
                    )));
                }
            },
        };

        let job_guard = job.lock().await;
        let pid = job_guard.pid;
        let command = job_guard.command.clone();
        let log_path = job_guard.log_path.clone();
        let status = job_guard.status;
        let exit_code_i32 = job_guard.exit_code;
        let stored_state_reason = job_guard.state_reason.clone();
        let elapsed_time = job_guard.elapsed_time();
        drop(job_guard);

        let (running, effective_status, effective_exit_code_i32, effective_reason) = match status {
            JobStatus::Running => {
                self.ensure_connected().await?;
                if self.is_pid_running(pid).await? {
                    (true, JobStatus::Running, None, None)
                } else if let Some(code) = exit_code_i32 {
                    (false, job_status_from_exit_code(code), Some(code), None)
                } else {
                    let (settled_status, settled_exit_code, settled_reason) =
                        await_running_job_settle(&job).await;
                    match settled_status {
                        JobStatus::Running => match settled_exit_code {
                            Some(code) => {
                                (false, job_status_from_exit_code(code), Some(code), None)
                            }
                            None => (
                                false,
                                JobStatus::StateLost,
                                None,
                                Some(settled_reason.unwrap_or_else(|| {
                                    "pid_not_running_and_no_exit_status".to_string()
                                })),
                            ),
                        },
                        JobStatus::Completed | JobStatus::Failed => match settled_exit_code {
                            Some(code) => {
                                (false, job_status_from_exit_code(code), Some(code), None)
                            }
                            None => (
                                false,
                                JobStatus::StateLost,
                                None,
                                Some(settled_reason.unwrap_or_else(|| {
                                    "missing_exit_code_for_terminal_state".to_string()
                                })),
                            ),
                        },
                        JobStatus::StateLost => (
                            false,
                            JobStatus::StateLost,
                            None,
                            Some(settled_reason.unwrap_or_else(|| "state_lost".to_string())),
                        ),
                    }
                }
            }
            JobStatus::Completed | JobStatus::Failed => match exit_code_i32 {
                Some(code) => (false, job_status_from_exit_code(code), Some(code), None),
                None => (
                    false,
                    JobStatus::StateLost,
                    None,
                    Some(
                        stored_state_reason
                            .clone()
                            .unwrap_or_else(|| "missing_exit_code_for_terminal_state".to_string()),
                    ),
                ),
            },
            JobStatus::StateLost => (
                false,
                JobStatus::StateLost,
                None,
                Some(
                    stored_state_reason
                        .clone()
                        .unwrap_or_else(|| "state_lost".to_string()),
                ),
            ),
        };

        if status != effective_status
            || exit_code_i32 != effective_exit_code_i32
            || stored_state_reason != effective_reason
        {
            let mut guard = job.lock().await;
            match effective_status {
                JobStatus::Running => {
                    guard.status = JobStatus::Running;
                    guard.exit_code = None;
                    guard.state_reason = None;
                }
                JobStatus::Completed | JobStatus::Failed => {
                    if let Some(code) = effective_exit_code_i32 {
                        guard.mark_exit(code);
                    }
                }
                JobStatus::StateLost => {
                    guard.mark_state_lost(
                        effective_reason
                            .clone()
                            .unwrap_or_else(|| "state_lost".to_string()),
                    );
                }
            }

            let persisted = guard.clone();
            drop(guard);

            if let Err(e) = spooler.persist_job_state(&persisted).await {
                warn!(job_id = ?job_id, error = ?e, "failed to persist reconciled job state");
            }
        }

        let exit_code = if running || effective_status == JobStatus::StateLost {
            None
        } else {
            effective_exit_code_i32.and_then(|code| u32::try_from(code).ok())
        };

        let log_exists = log_file_exists(&log_path).await?;

        let log_tail = read_local_log_tail(&log_path, tail_lines).await?;

        Ok(ProcessStatus {
            pid,
            state: effective_status.as_str().to_string(),
            running,
            exit_code,
            state_reason: effective_reason,
            elapsed_time,
            command,
            log_path: log_path.to_string_lossy().to_string(),
            log_exists,
            log_tail,
        })
    }

    async fn is_pid_running(&self, pid: u32) -> Result<bool> {
        // `kill -0` checks for existence/permission without sending a signal.
        let cmd = format!("sh -c 'kill -0 {pid} 2>/dev/null'");
        let output = self.exec_command(&cmd, Duration::from_secs(5)).await?;
        Ok(output.exit_code == Some(0))
    }
}

fn job_status_from_exit_code(exit_code: i32) -> JobStatus {
    if exit_code == 0 {
        JobStatus::Completed
    } else {
        JobStatus::Failed
    }
}

async fn await_running_job_settle(
    job: &SharedJobState,
) -> (JobStatus, Option<i32>, Option<String>) {
    tokio::time::sleep(Duration::from_millis(150)).await;
    let guard = job.lock().await;
    (guard.status, guard.exit_code, guard.state_reason.clone())
}

pub(crate) async fn read_local_log_tail(path: &Path, lines: usize) -> Result<String> {
    if lines == 0 {
        return Ok(String::new());
    }

    let mut file = match open_log_read_no_symlink(path).await? {
        Some(f) => f,
        None => return Ok(String::new()),
    };

    let meta = file.metadata().await?;
    let mut pos = meta.len();

    const CHUNK_SIZE: u64 = 8192;
    const MAX_READ_BYTES: usize = 1024 * 1024;

    let mut buf: Vec<u8> = Vec::new();
    let mut newlines = 0usize;

    while pos > 0 && newlines <= lines && buf.len() < MAX_READ_BYTES {
        let read_len = std::cmp::min(CHUNK_SIZE, pos) as usize;
        pos = pos.saturating_sub(read_len as u64);

        file.seek(std::io::SeekFrom::Start(pos)).await?;

        let mut chunk = vec![0u8; read_len];
        let mut got = 0usize;
        while got < read_len {
            let n = file.read(&mut chunk[got..]).await?;
            if n == 0 {
                break;
            }
            got = got.saturating_add(n);
        }
        if got == 0 {
            break;
        }
        chunk.truncate(got);

        newlines = newlines.saturating_add(chunk.iter().filter(|&&b| b == b'\n').count());

        // Prepend chunk to existing buffer (bounded by MAX_READ_BYTES).
        if chunk.len().saturating_add(buf.len()) > MAX_READ_BYTES {
            let allowed = MAX_READ_BYTES.saturating_sub(buf.len());
            chunk.truncate(allowed);
        }
        chunk.extend_from_slice(&buf);
        buf = chunk;
    }

    let text = String::from_utf8_lossy(&buf);
    let all_lines: Vec<&str> = text.lines().collect();
    if all_lines.is_empty() {
        return Ok(String::new());
    }

    let start = all_lines.len().saturating_sub(lines);
    Ok(all_lines[start..].join("\n"))
}

async fn log_file_exists(path: &Path) -> Result<bool> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "log path is a symlink (refusing to follow it)",
                )
                .into());
            }
            Ok(meta.is_file())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

async fn open_log_read_no_symlink(path: &Path) -> Result<Option<tokio::fs::File>> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "log path is a symlink (refusing to follow it)",
                )
                .into());
            }
            if !meta.is_file() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "log path is not a regular file",
                )
                .into());
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(e) => return Err(e.into()),
    }

    let mut opts = tokio::fs::OpenOptions::new();
    opts.read(true);

    #[cfg(unix)]
    {
        opts.custom_flags(O_NOFOLLOW_FLAG);
    }

    match opts.open(path).await {
        Ok(f) => {
            // Re-check based on the opened file handle to avoid TOCTOU.
            let meta = f.metadata().await?;
            if !meta.is_file() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "log path is not a regular file",
                )
                .into());
            }
            Ok(Some(f))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => {
            if let Ok(meta) = tokio::fs::symlink_metadata(path).await
                && meta.file_type().is_symlink()
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "log path is a symlink (refusing to follow it)",
                )
                .into());
            }
            Err(e.into())
        }
    }
}

#[derive(Debug)]
enum RawStreamEvent {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    ExitStatus(u32),
    Closed,
}

struct JoinAbortGuard<T> {
    handle: Option<tokio::task::JoinHandle<T>>,
}

impl<T> JoinAbortGuard<T> {
    fn new(handle: tokio::task::JoinHandle<T>) -> Self {
        Self {
            handle: Some(handle),
        }
    }

    fn into_handle(mut self) -> Option<tokio::task::JoinHandle<T>> {
        self.handle.take()
    }
}

impl<T> Drop for JoinAbortGuard<T> {
    fn drop(&mut self) {
        if let Some(handle) = &self.handle {
            handle.abort();
        }
    }
}

async fn raw_channel_task(
    mut channel: russh::Channel<russh::client::Msg>,
    stdin_rx: &mut tokio::sync::mpsc::Receiver<Vec<u8>>,
    out_tx: tokio::sync::mpsc::Sender<RawStreamEvent>,
) -> Result<()> {
    let mut stdin_closed = false;
    let mut sent_closed = false;
    loop {
        tokio::select! {
            maybe_chunk = stdin_rx.recv(), if !stdin_closed => {
                match maybe_chunk {
                    Some(chunk) => {
                        channel.data(chunk.as_slice()).await.map_err(|e| {
                            SshMcpError::connection(format!("Failed to send stdin: {e}"))
                        })?;
                    }
                    None => {
                        stdin_closed = true;
                        let _ = channel.eof().await;
                    }
                }
            }
            maybe_msg = channel.wait() => {
                match maybe_msg {
                    Some(msg) => {
                        let send_evt = |evt: RawStreamEvent| async {
                            out_tx.send(evt).await.map_err(|_| ())
                        };

                        match msg {
                            ChannelMsg::Data { data } => {
                                let bytes = data.as_ref().to_vec();
                                if send_evt(RawStreamEvent::Stdout(bytes)).await.is_err() {
                                    return Ok(());
                                }
                            }
                            ChannelMsg::ExtendedData { data, ext } => {
                                let bytes = data.as_ref().to_vec();
                                let evt = if ext == 1 {
                                    RawStreamEvent::Stderr(bytes)
                                } else {
                                    RawStreamEvent::Stdout(bytes)
                                };
                                if send_evt(evt).await.is_err() {
                                    return Ok(());
                                }
                            }
                            ChannelMsg::ExitStatus { exit_status }
                                if send_evt(RawStreamEvent::ExitStatus(exit_status)).await.is_err() =>
                            {
                                return Ok(());
                            }
                            ChannelMsg::ExitStatus { .. } => {}
                            ChannelMsg::ExitSignal { signal_name, .. } => {
                                // Map signal to exit code (128 + signal number)
                                // Common signals: HUP=1, INT=2, QUIT=3, ILL=4, TRAP=5, ABRT=6, BUS=7, FPE=8, KILL=9
                                let code = match signal_name {
                                    russh::Sig::HUP => 129,
                                    russh::Sig::INT => 130,
                                    russh::Sig::QUIT => 131,
                                    russh::Sig::ILL => 132,
                                    russh::Sig::ABRT => 134,
                                    russh::Sig::FPE => 136,
                                    russh::Sig::KILL => 137,
                                    russh::Sig::USR1 => 138,
                                    russh::Sig::SEGV => 139,
                                    russh::Sig::PIPE => 141,
                                    russh::Sig::ALRM => 142,
                                    russh::Sig::TERM => 143,
                                    russh::Sig::Custom(_) => 128,
                                };
                                if send_evt(RawStreamEvent::ExitStatus(code)).await.is_err() {
                                    return Ok(());
                                }
                            }
                            ChannelMsg::Close | ChannelMsg::Eof if !sent_closed => {
                                // Send Closed once but keep looping to capture trailing ExitStatus
                                sent_closed = true;
                                let _ = send_evt(RawStreamEvent::Closed).await;
                            }
                            ChannelMsg::Close | ChannelMsg::Eof => {}
                            _ => {}
                        }
                    }
                    None => {
                        // Channel fully closed - ensure we send Closed before exiting
                        if !sent_closed {
                            let _ = out_tx.send(RawStreamEvent::Closed).await;
                        }
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_command_output_success() {
        let output = CommandOutput {
            stdout: "hello".to_string(),
            stderr: String::new(),
            exit_code: Some(0),
            ..Default::default()
        };
        assert!(output.success());
    }

    #[test]
    fn test_command_output_failure() {
        let output = CommandOutput {
            stdout: String::new(),
            stderr: "error".to_string(),
            exit_code: Some(1),
            ..Default::default()
        };
        assert!(!output.success());
    }

    #[test]
    fn test_command_output_no_exit_code() {
        let output = CommandOutput {
            stdout: "hello".to_string(),
            stderr: String::new(),
            exit_code: None,
            ..Default::default()
        };
        // No exit code means the channel was torn down — not success
        assert!(!output.success());
    }

    #[test]
    fn test_command_output_combined() {
        let output = CommandOutput {
            stdout: "stdout".to_string(),
            stderr: "stderr".to_string(),
            exit_code: Some(0),
            ..Default::default()
        };
        assert_eq!(output.combined_output(), "stdout\nstderr");
    }

    #[test]
    fn test_command_output_combined_only_stdout() {
        let output = CommandOutput {
            stdout: "stdout".to_string(),
            stderr: String::new(),
            exit_code: Some(0),
            ..Default::default()
        };
        assert_eq!(output.combined_output(), "stdout");
    }

    #[test]
    fn test_command_output_combined_only_stderr() {
        let output = CommandOutput {
            stdout: String::new(),
            stderr: "stderr".to_string(),
            exit_code: Some(1),
            ..Default::default()
        };
        assert_eq!(output.combined_output(), "stderr");
    }

    #[test]
    fn test_wrap_command_with_timeout() {
        let cmd = wrap_command_with_timeout("sleep 10", 2.0);
        assert!(cmd.contains("timeout -k 2s 2s"));
        assert!(cmd.contains("sh -lc")); // Uses login shell
        assert!(cmd.contains("sleep 10"));
    }

    #[test]
    fn test_wrap_command_with_timeout_zero_duration() {
        // Edge case: wrapper accepts zero (validation is elsewhere)
        let cmd = wrap_command_with_timeout("echo test", 0.0);
        assert!(cmd.contains("timeout -k 2s 0s"));
        assert!(cmd.contains("sh -lc"));
        assert!(cmd.contains("echo test"));
    }

    #[test]
    fn test_wrap_command_with_timeout_fractional() {
        // Test fractional seconds for sub-second precision
        let cmd = wrap_command_with_timeout("sleep 1", 0.5);
        assert!(cmd.contains("timeout -k 2s 0.5s"));
        assert!(cmd.contains("sh -lc"));
        assert!(cmd.contains("sleep 1"));
    }

    #[test]
    fn test_wrap_command_with_timeout_complex_command() {
        let cmd = wrap_command_with_timeout("echo 'hello world'", 10.0);
        assert!(cmd.contains("timeout -k 2s 10s"));
        assert!(cmd.contains("sh -lc"));
        assert!(cmd.contains("echo"));
    }

    #[test]
    fn test_wrap_command_with_timeout_with_single_quotes() {
        let cmd = wrap_command_with_timeout("echo 'hello'", 10.0);
        assert!(cmd.contains("timeout -k 2s 10s"));
        assert!(cmd.contains("sh -lc"));
        // Single quotes are escaped as '"'"'
        assert!(cmd.contains("'\"'\"'"));
    }

    #[test]
    fn test_wrap_command_for_channel_exec_non_login_shell() {
        let cmd = wrap_command_for_channel_exec("echo hello");
        assert_eq!(cmd, "sh -c 'echo hello'");
    }

    #[test]
    fn test_wrap_command_for_channel_exec_timeout_wrapper_payload() {
        let timeout_wrapped = wrap_command_with_timeout("echo hello", 1.0);
        assert_eq!(timeout_wrapped, "timeout -k 2s 1s sh -lc 'echo hello'");

        let cmd = wrap_command_for_channel_exec(&timeout_wrapped);
        assert_eq!(
            cmd,
            "sh -c 'timeout -k 2s 1s sh -lc '\"'\"'echo hello'\"'\"''"
        );
    }

    #[test]
    fn test_wrap_command_for_channel_exec_timeout_wrapper_payload_with_single_quotes() {
        let timeout_wrapped = wrap_command_with_timeout("echo 'hello'", 1.0);
        assert_eq!(
            timeout_wrapped,
            "timeout -k 2s 1s sh -lc 'echo '\"'\"'hello'\"'\"''"
        );

        let cmd = wrap_command_for_channel_exec(&timeout_wrapped);
        assert!(cmd.starts_with("sh -c '"));
        assert!(cmd.ends_with('\''));

        let inner = &cmd[7..cmd.len() - 1];
        let unescaped_once = inner.replace("'\"'\"'", "'");
        assert_eq!(unescaped_once, timeout_wrapped);
        assert!(cmd.contains("hello"));
    }

    #[test]
    fn test_resolve_raw_stream_stderr_limit_uses_token_limit() {
        assert_eq!(
            resolve_raw_stream_stderr_limit(Some(12_000)),
            12_000 * RAW_STREAM_BYTES_PER_TOKEN
        );
    }

    #[test]
    fn test_resolve_raw_stream_stderr_limit_none_uses_hard_cap() {
        assert_eq!(
            resolve_raw_stream_stderr_limit(None),
            RAW_STREAM_STDERR_HARD_MAX_BYTES
        );
    }

    #[test]
    fn test_resolve_raw_stream_stderr_limit_applies_hard_cap() {
        assert_eq!(
            resolve_raw_stream_stderr_limit(Some(RAW_STREAM_STDERR_HARD_MAX_BYTES)),
            RAW_STREAM_STDERR_HARD_MAX_BYTES
        );
    }

    #[test]
    fn test_append_bounded_lossy_stderr_no_truncation() {
        let mut stderr = String::new();
        let truncated = append_bounded_lossy_stderr(&mut stderr, b"hello", 16);
        assert!(!truncated);
        assert_eq!(stderr, "hello");
    }

    #[test]
    fn test_append_bounded_lossy_stderr_truncates_at_utf8_boundary() {
        let mut stderr = String::new();
        let truncated = append_bounded_lossy_stderr(&mut stderr, "абв".as_bytes(), 3);
        assert!(truncated);
        assert_eq!(stderr, "а");
    }
}
