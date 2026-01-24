//! Command execution over SSH
//!
//! Provides the `CommandOutput` struct and `exec_command` functionality
//! for executing commands over an SSH connection with timeout support.

use std::time::Duration;

use russh::ChannelMsg;
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
/// * `duration_secs` - Timeout duration in seconds
///
/// # Returns
/// A wrapped command string that includes timeout logic
pub fn wrap_command_with_timeout(command: &str, duration_secs: u64) -> String {
    let escaped_command = escape_for_timeout_wrapper(command);
    format!(
        "timeout -k {}s {}s sh -lc '{}'",
        TIMEOUT_KILL_AFTER_SECS, duration_secs, escaped_command
    )
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
    async fn exec_via_su_shell(
        &self,
        command: &str,
        timeout_duration: Duration,
    ) -> Result<CommandOutput> {
        // Check duration edge case
        let duration_secs = timeout_duration.as_secs();
        if duration_secs == 0 {
            return Err(SshMcpError::InvalidParams(
                "duration must be > 0".to_string(),
            ));
        }

        // Take the channel from the mutex (we'll put it back after)
        let mut channel = {
            let mut guard = self.su_channel.lock().await;
            guard
                .take()
                .ok_or_else(|| SshMcpError::connection("No su channel available"))?
        };

        // Check timeout availability lazily on first use (same as exec_via_channel)
        let use_wrapper = if self.use_timeout_wrapper() {
            true
        } else {
            let _ = self.check_timeout_availability().await;
            self.use_timeout_wrapper()
        };

        // Wrap command with timeout if available
        let wrapped_cmd = if use_wrapper {
            wrap_command_with_timeout(command, duration_secs)
        } else {
            command.to_string()
        };

        debug!("Executing elevated command: {}", wrapped_cmd);

        // Send command
        if let Err(e) = channel.data(format!("{}\n", wrapped_cmd).as_bytes()).await {
            // Put channel back before returning error
            let mut guard = self.su_channel.lock().await;
            *guard = Some(channel);
            return Err(SshMcpError::connection(format!(
                "Failed to send command: {}",
                e
            )));
        }

        // Collect output until we see a root prompt (#)
        let mut buffer = String::new();
        // When using wrapper, timeout is handled remotely - no local deadline needed
        let deadline = if use_wrapper {
            None
        } else {
            Some(tokio::time::Instant::now() + timeout_duration)
        };

        let result = loop {
            if let Some(deadline_ref) = deadline && tokio::time::Instant::now() > deadline_ref {
                break Err(SshMcpError::Timeout(timeout_duration.as_millis() as u64));
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

                                break Ok(CommandOutput {
                                    stdout: if output.is_empty() {
                                        output
                                    } else {
                                        format!("{}\n", output)
                                    },
                                    stderr: String::new(),
                                    exit_code: Some(0), // Assume success in PTY mode
                                });
                            }
                        }
                        ChannelMsg::Close => {
                            break Err(SshMcpError::connection(
                                "Channel closed during command execution",
                            ));
                        }
                        _ => {
                            // Ignore other messages
                        }
                    }
                }
                Ok(None) => {
                    break Err(SshMcpError::connection(
                        "Channel ended during command execution",
                    ));
                }
                Err(_) => {
                    // Timeout on wait, continue loop
                    continue;
                }
            }
        };

        // Put the channel back
        {
            let mut guard = self.su_channel.lock().await;
            *guard = Some(channel);
        }

        result
    }

    /// Execute command via a new exec channel
    async fn exec_via_channel(
        &self,
        command: &str,
        timeout_duration: Duration,
    ) -> Result<CommandOutput> {
        // Check duration edge case
        let duration_secs = timeout_duration.as_secs();
        if duration_secs == 0 {
            return Err(SshMcpError::InvalidParams(
                "duration must be > 0".to_string(),
            ));
        }

        let channel = self.open_channel().await?;

        // Wrap command with timeout if available
        // Check timeout availability lazily on first use
        let use_wrapper = if self.use_timeout_wrapper() {
            // Already cached as available
            true
        } else {
            // Try detection - if it becomes available after check, use wrapper
            let _ = self.check_timeout_availability().await;
            self.use_timeout_wrapper()
        };

        let wrapped_cmd = if use_wrapper {
            wrap_command_with_timeout(command, duration_secs)
        } else {
            // Fall back to old method: use tokio timeout + pkill
            command.to_string()
        };

        debug!("Executing command: {}", wrapped_cmd);
        channel
            .exec(true, wrapped_cmd.as_str())
            .await
            .map_err(|e| SshMcpError::connection(format!("Failed to exec command: {}", e)))?;

        // Collect output with appropriate timeout strategy
        let output = if use_wrapper {
            // When using wrapper, timeout is handled remotely - no tokio timeout needed
            self.collect_channel_output(channel).await?
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
            }?
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
                let channel = self.open_channel().await?;
                channel.exec(true, command).await.map_err(|e| {
                    SshMcpError::connection(format!("Failed to exec command: {}", e))
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

    /// Collect output from a channel until it closes
    async fn collect_channel_output(
        &self,
        mut channel: russh::Channel<russh::client::Msg>,
    ) -> Result<CommandOutput> {
        let mut output = CommandOutput::new();

        while let Some(msg) = channel.wait().await {
            match msg {
                ChannelMsg::Data { data } => {
                    output.stdout.push_str(&String::from_utf8_lossy(&data));
                }
                ChannelMsg::ExtendedData { data, ext } => {
                    // ext == 1 is typically stderr
                    if ext == 1 {
                        output.stderr.push_str(&String::from_utf8_lossy(&data));
                    } else {
                        output.stdout.push_str(&String::from_utf8_lossy(&data));
                    }
                }
                ChannelMsg::ExitStatus { exit_status } => {
                    output.exit_code = Some(exit_status);
                }
                ChannelMsg::Close | ChannelMsg::Eof => {
                    break;
                }
                _ => {
                    // Ignore other messages
                }
            }
        }

        // If there's stderr and a non-zero exit code, we might want to handle it
        // For now, just return the output as-is
        debug!(
            "Command completed: exit_code={:?}, stdout_len={}, stderr_len={}",
            output.exit_code,
            output.stdout.len(),
            output.stderr.len()
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

        debug!("Sending abort command: {}", abort_cmd);

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
        };
        assert!(output.success());
    }

    #[test]
    fn test_command_output_failure() {
        let output = CommandOutput {
            stdout: String::new(),
            stderr: "error".to_string(),
            exit_code: Some(1),
        };
        assert!(!output.success());
    }

    #[test]
    fn test_command_output_no_exit_code() {
        let output = CommandOutput {
            stdout: "hello".to_string(),
            stderr: String::new(),
            exit_code: None,
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
        };
        assert_eq!(output.combined_output(), "stdout\nstderr");
    }

    #[test]
    fn test_command_output_combined_only_stdout() {
        let output = CommandOutput {
            stdout: "stdout".to_string(),
            stderr: String::new(),
            exit_code: Some(0),
        };
        assert_eq!(output.combined_output(), "stdout");
    }

    #[test]
    fn test_command_output_combined_only_stderr() {
        let output = CommandOutput {
            stdout: String::new(),
            stderr: "stderr".to_string(),
            exit_code: Some(1),
        };
        assert_eq!(output.combined_output(), "stderr");
    }

    #[test]
    fn test_wrap_command_with_timeout() {
        let cmd = wrap_command_with_timeout("sleep 10", 2);
        assert!(cmd.contains("timeout -k 2s 2s"));
        assert!(cmd.contains("sh -lc")); // Uses login shell
        assert!(cmd.contains("sleep 10"));
    }

    #[test]
    fn test_wrap_command_with_timeout_zero_duration() {
        // Edge case: wrapper accepts zero (validation is elsewhere)
        let cmd = wrap_command_with_timeout("echo test", 0);
        assert!(cmd.contains("timeout -k 2s 0s"));
        assert!(cmd.contains("sh -lc"));
        assert!(cmd.contains("echo test"));
    }

    #[test]
    fn test_wrap_command_with_timeout_complex_command() {
        let cmd = wrap_command_with_timeout("echo 'hello world'", 10);
        assert!(cmd.contains("timeout -k 2s 10s"));
        assert!(cmd.contains("sh -lc"));
        assert!(cmd.contains("echo"));
    }

    #[test]
    fn test_wrap_command_with_timeout_with_single_quotes() {
        let cmd = wrap_command_with_timeout("echo 'hello'", 10);
        assert!(cmd.contains("timeout -k 2s 10s"));
        assert!(cmd.contains("sh -lc"));
        // Single quotes are escaped as '"'"'
        assert!(cmd.contains("'\"'\"'"));
    }
}
