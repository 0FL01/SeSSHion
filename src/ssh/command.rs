//! Command execution over SSH
//!
//! Provides the `CommandOutput` struct and `exec_command` functionality
//! for executing commands over an SSH connection with timeout support.
//!
//! This module is designed to be compatible with both GNU and BusyBox-based
//! systems (e.g., Debian/Ubuntu and Alpine Linux). All command detection and
//! process monitoring uses portable mechanisms that work across distributions.

use std::time::Duration;

use russh::ChannelMsg;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::OwnedSemaphorePermit;
use tokio::time::timeout;
use tracing::{debug, error, warn};

use super::config::TIMEOUT_KILL_AFTER_SECS;
use super::connection::SshConnectionManager;
use super::sanitize::{escape_command_for_shell, escape_for_timeout_wrapper};
use crate::error::{Result, SshMcpError};

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
    /// Whether the process is currently running
    pub running: bool,
    /// Exit code if process has completed
    pub exit_code: Option<u32>,
    /// Elapsed time in ps format (e.g., "12:34" or "2-12:34:56")
    pub elapsed_time: String,
    /// Command line of the process (from /proc/PID/cmdline)
    pub command: String,
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
        self.exit_code.is_none_or(|code| code == 0)
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
        let _permit: OwnedSemaphorePermit = self
            .channel_semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| {
                SshMcpError::connection(format!("Failed to acquire command slot: {}", e))
            })?;

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
        // Convert duration to fractional seconds for millisecond precision
        // as_secs_f64() preserves sub-second precision (e.g., 500ms -> 0.5, 1500ms -> 1.5)
        let duration_secs = timeout_duration.as_secs_f64();
        if !duration_secs.is_finite() || duration_secs <= 0.0 {
            return Err(SshMcpError::InvalidParams(
                "duration must be finite and > 0".to_string(),
            ));
        }

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
                    warn!("su channel failed after command sent: {}", e);
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
                    "su channel send failed (pre-send), resetting and re-elevating: {}",
                    e
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
                    warn!("su channel failed after command sent (retry): {}", e);
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
        channel
            .data(format!("{}\n", command).as_bytes())
            .await
            .map_err(|e| SuSendError::SendFailed(e.to_string()))
    }

    /// Collect output from su channel until root prompt or error
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

                            // Check for root prompt - indicates command complete
                            // Match # which indicates root prompt (may be followed by spaces, escape codes, etc)
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
        // Convert duration to fractional seconds for millisecond precision
        // as_secs_f64() preserves sub-second precision (e.g., 500ms -> 0.5, 1500ms -> 1.5)
        let duration_secs = timeout_duration.as_secs_f64();
        if !duration_secs.is_finite() || duration_secs <= 0.0 {
            return Err(SshMcpError::InvalidParams(
                "duration must be finite and > 0".to_string(),
            ));
        }

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
        let (channel, _exec_sent) = match self.try_open_and_exec(&wrapped_cmd).await {
            Ok(result) => result,
            Err(PreExecError::ChannelOpen(e)) => {
                // Channel open failed - reconnect and retry once (Attempt #2)
                warn!("Channel open failed, attempting reconnect and retry: {}", e);
                self.reconnect().await?;
                match self.try_open_and_exec(&wrapped_cmd).await {
                    Ok(result) => result,
                    Err(retry_err) => {
                        // Return the retry error (second failure is the one we report)
                        return Err(retry_err.into_ssh_error());
                    }
                }
            }
            Err(PreExecError::ExecSend(e)) => {
                // Exec send failed - reconnect and retry once (Attempt #2)
                warn!("Exec send failed, attempting reconnect and retry: {}", e);
                self.reconnect().await?;
                match self.try_open_and_exec(&wrapped_cmd).await {
                    Ok(result) => result,
                    Err(retry_err) => {
                        // Return the retry error (second failure is the one we report)
                        return Err(retry_err.into_ssh_error());
                    }
                }
            }
        };

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
                let (channel, _) = match self.try_open_and_exec(command).await {
                    Ok(result) => result,
                    Err(PreExecError::ChannelOpen(e)) => {
                        return Err(SshMcpError::connection(format!(
                            "Failed to open channel (fallback): {}",
                            e
                        )));
                    }
                    Err(PreExecError::ExecSend(e)) => {
                        return Err(SshMcpError::connection(format!(
                            "Failed to exec command (fallback): {}",
                            e
                        )));
                    }
                };

                let result = timeout(timeout_duration, self.collect_channel_output(channel)).await;

                return match result {
                    Ok(inner_output) => inner_output,
                    Err(_) => {
                        warn!(
                            "Command timed out after {}ms (fallback), attempting abort",
                            timeout_duration.as_millis()
                        );
                        self.abort_command(command).await;
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
        channel
            .exec(true, command)
            .await
            .map_err(|e| PreExecError::ExecSend(format!("Failed to exec command: {}", e)))?;

        Ok((channel, true))
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
                                if remaining > 0 {
                                    // Safe slicing: we know remaining is within bounds since data_str.len() > remaining
                                    let safe_end = data_str
                                        .char_indices()
                                        .map(|(i, _)| i)
                                        .find(|&i| i > remaining)
                                        .unwrap_or(data_str.len());
                                    let take = std::cmp::min(safe_end, remaining);
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
                                    if remaining > 0 {
                                        // Safe slicing: find UTF-8 safe boundary
                                        let safe_end = data_str
                                            .char_indices()
                                            .map(|(i, _)| i)
                                            .find(|&i| i > remaining)
                                            .unwrap_or(data_str.len());
                                        let take = std::cmp::min(safe_end, remaining);
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
                error!("Failed to open channel for abort: {}", e);
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
            error!("Failed to exec abort command: {}", e);
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
        let _permit: OwnedSemaphorePermit = self
            .channel_semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| SshMcpError::connection(format!("Failed to acquire command slot: {e}")))?;

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
                                output.stderr.push_str(&String::from_utf8_lossy(&data));
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

    /// Check the status of a process by PID
    ///
    /// Uses `/proc/PID/stat` for process detection, which is portable across all Linux systems.
    /// This approach works reliably on Debian, Ubuntu, Alpine, and any Linux distribution with
    /// procfs support. Unlike the `ps` command, which has varying output formats between GNU
    /// and BusyBox implementations, `/proc/PID/stat` has a consistent format across all Linux
    /// distributions, making it the preferred method for portable process detection.
    ///
    /// Falls back to reading exit code from `{log_path}.exit` file if process not running.
    ///
    /// # Arguments
    /// * `pid` - Process ID to check
    /// * `log_path` - Optional path to log file to read tail from
    /// * `tail_lines` - Number of lines to read from log tail
    ///
    /// # Returns
    /// ProcessStatus with running state, exit code, elapsed time, command, and log tail
    pub async fn check_process(
        &self,
        pid: u32,
        log_path: Option<String>,
        tail_lines: usize,
    ) -> Result<ProcessStatus> {
        debug!("Checking process status: pid={}", pid);

        // Use /proc/PID/stat which works on both Alpine (BusyBox) and Debian/Ubuntu
        // Format: pid (comm) state ppid pgrp session tty_nr tpgid flags minflt cminflt majflt cmajflt utime stime ...
        // The command name (comm) is the 2nd field in parentheses
        let stat_cmd = format!("cat /proc/{}/stat 2>/dev/null || echo 'NOT_FOUND'", pid);
        debug!("Executing stat command for pid={}", pid);
        let stat_output = self.exec_command(&stat_cmd, Duration::from_secs(5)).await?;

        let stdout = stat_output.stdout.trim();
        debug!(
            "Stat output for pid={}: len={}, content={}",
            pid,
            stdout.len(),
            stdout
        );

        let (running, command) = if stdout.is_empty() || stdout == "NOT_FOUND" {
            debug!("Process {} not found in /proc", pid);
            (false, String::new())
        } else {
            // Parse /proc/PID/stat format
            // Example: "1234 (bash) S 1233 1234 1234 34816 1234 ..."
            // The command name is in parentheses, may contain spaces
            let cmd = parse_comm_from_stat(stdout);
            debug!("Process {} is running, command='{}'", pid, cmd);
            (true, cmd)
        };

        // Try to get exit code from exit file if process not running
        let exit_code = if !running {
            if let Some(ref path) = log_path {
                let exit_file = format!("{}.exit", path);
                let escaped_exit_file = escape_command_for_shell(&exit_file);
                let exit_cmd = format!("cat '{}' 2>/dev/null | head -1 || true", escaped_exit_file);
                debug!("Reading exit code from: {}", exit_file);
                let exit_output = self.exec_command(&exit_cmd, Duration::from_secs(2)).await?;
                let exit_str = exit_output.stdout.trim();
                if exit_str.is_empty() {
                    debug!("No exit code found in {}", exit_file);
                    None
                } else {
                    let code = exit_str.parse::<u32>().ok();
                    debug!("Found exit code: {:?}", code);
                    code
                }
            } else {
                debug!("No log_path provided, cannot determine exit code");
                None
            }
        } else {
            None
        };

        // Get log tail if path provided
        let log_tail = if let Some(ref path) = log_path {
            let escaped_path = escape_command_for_shell(path);
            let tail_cmd = format!(
                "tail -n {} '{}' 2>/dev/null || true",
                tail_lines, escaped_path
            );
            debug!("Reading log tail from: {} ({} lines)", path, tail_lines);
            let tail_output = self.exec_command(&tail_cmd, Duration::from_secs(5)).await?;
            tail_output.stdout
        } else {
            String::new()
        };

        debug!(
            "Process status for pid={}: running={}, exit_code={:?}, command_len={}",
            pid,
            running,
            exit_code,
            command.len()
        );

        Ok(ProcessStatus {
            running,
            exit_code,
            elapsed_time: String::new(), // Simplified: empty for MVP (BusyBox ps doesn't support etime)
            command,
            log_tail,
        })
    }
}

/// Parse the command name (comm) from /proc/PID/stat output.
///
/// The stat file format is:
/// `pid (comm) state ppid pgrp session tty_nr ...`
///
/// The command name is the second field, enclosed in parentheses.
/// It may contain spaces and even parentheses, so we need to parse carefully.
///
/// # Arguments
/// * `stat_line` - The content of /proc/PID/stat
///
/// # Returns
/// The extracted command name (without parentheses)
fn parse_comm_from_stat(stat_line: &str) -> String {
    // Find the first '(' and the last ')' before the space after the command name
    // Format: "1234 (bash) S ..." or "1234 (my cmd) S ..." or "1234 (my (nested)) S ..."
    if let Some(start) = stat_line.find('(') {
        // Find the matching ')' - it's the last one before the space that precedes the state
        // The state is always a single character after the command name
        if let Some(end) = stat_line.rfind(')') {
            // Extract between parentheses
            if start < end {
                return stat_line[start + 1..end].to_string();
            }
        }
    }
    String::new()
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
                            ChannelMsg::ExitStatus { exit_status } => {
                                if send_evt(RawStreamEvent::ExitStatus(exit_status)).await.is_err() {
                                    return Ok(());
                                }
                            }
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
                            ChannelMsg::Close | ChannelMsg::Eof => {
                                // Send Closed once but keep looping to capture trailing ExitStatus
                                if !sent_closed {
                                    sent_closed = true;
                                    let _ = send_evt(RawStreamEvent::Closed).await;
                                }
                            }
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
        // No exit code should be treated as success
        assert!(output.success());
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
}
