//! Command sanitization and escaping utilities
//!
//! Provides functions for validating and escaping commands before SSH execution.

use crate::error::{Result, SshMcpError};

/// Sanitize a command before execution
///
/// This function:
/// - Validates that the command is not empty
/// - Trims whitespace
/// - Checks length against max_chars limit
///
/// # Arguments
/// * `command` - The raw command string
/// * `max_chars` - Optional maximum character limit (None = unlimited)
///
/// # Returns
/// * `Ok(String)` - The sanitized command
/// * `Err(SshMcpError::InvalidParams)` - If command is empty or too long
///
/// # Examples
/// ```
/// use ssh_mcp::ssh::sanitize::sanitize_command;
///
/// let cmd = sanitize_command("  ls -la  ", Some(1000)).unwrap();
/// assert_eq!(cmd, "ls -la");
///
/// // Too long command
/// let result = sanitize_command("a".repeat(100).as_str(), Some(50));
/// assert!(result.is_err());
/// ```
pub fn sanitize_command(command: &str, max_chars: Option<usize>) -> Result<String> {
    let trimmed = command.trim();

    if trimmed.is_empty() {
        return Err(SshMcpError::invalid_params("Command cannot be empty"));
    }

    // Check length limit
    if let Some(max) = max_chars
        && trimmed.len() > max
    {
        return Err(SshMcpError::invalid_params(format!(
            "Command is too long (max {} characters, got {})",
            max,
            trimmed.len()
        )));
    }

    Ok(trimmed.to_string())
}

/// Escape a command for safe execution in shell (for pkill -f patterns)
///
/// This escapes characters that could cause shell injection when used
/// inside single-quoted shell strings (like in `pkill -f 'command'`).
///
/// Escaped characters:
/// - Single quotes: `'` → `'"'"'`
/// - Dollar signs: `$` → `\$` (prevents variable expansion)
/// - Backticks: `` ` `` → `\`` (prevents command substitution)
/// - Backslashes: `\` → `\\` (prevents escape sequences)
/// - Parentheses: `(` and `)` → `\(` and `\)` (prevents subshells)
/// - Pipes: `|` → `\|` (prevents command chaining)
///
/// # Example
/// ```
/// use ssh_mcp::ssh::sanitize::escape_command_for_shell;
///
/// let escaped = escape_command_for_shell("echo 'hello' | cat");
/// assert_eq!(escaped, "echo '\"'\"'hello'\"'\"' \\| cat");
/// ```
pub fn escape_command_for_shell(command: &str) -> String {
    // Escape order matters for backslash - we escape it first
    // to avoid double-escaping subsequent characters
    command
        .replace('\\', "\\\\")
        .replace('$', "\\$")
        .replace('`', "\\`")
        .replace('(', "\\(")
        .replace(')', "\\)")
        .replace('|', "\\|")
        .replace('\'', "'\"'\"'")
}

/// Escapes a command for safe inclusion inside single quotes in timeout wrapper
///
/// This function escapes characters that would break out of single quotes
/// when used inside the timeout wrapper: `timeout -k 2s 10s sh -lc '{cmd}'`
///
/// The escaping order is important:
/// 1. First escape backslashes (so `\$HOME` becomes `\\$HOME`)
/// 2. Then escape single quotes (so `'` becomes `'"'"'`)
///
/// # Arguments
/// * `command` - The raw command string
///
/// # Returns
/// A safely escaped command string
///
/// # Examples
/// ```
/// use ssh_mcp::ssh::sanitize::escape_for_timeout_wrapper;
///
/// // Simple command
/// assert_eq!(escape_for_timeout_wrapper("sleep 10"), "sleep 10");
///
/// // Command with single quotes
/// assert_eq!(escape_for_timeout_wrapper("echo 'hello'"), "echo '\"'\"'hello'\"'\"'");
///
/// // Command with backslashes
/// assert_eq!(escape_for_timeout_wrapper(r"echo \$HOME"), r"echo \\$HOME");
/// ```
pub fn escape_for_timeout_wrapper(command: &str) -> String {
    // First escape backslashes, then single quotes
    // This order is important: we need to preserve backslash escaping
    // for the shell while also escaping single quotes
    command.replace('\\', "\\\\").replace('\'', "'\"'\"'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_command_valid() {
        let result = sanitize_command("ls -la", Some(1000));
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "ls -la");
    }

    #[test]
    fn test_sanitize_command_trims_whitespace() {
        let result = sanitize_command("  ls -la  ", Some(1000));
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "ls -la");
    }

    #[test]
    fn test_sanitize_command_empty() {
        let result = sanitize_command("", Some(1000));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("cannot be empty"));
    }

    #[test]
    fn test_sanitize_command_whitespace_only() {
        let result = sanitize_command("   ", Some(1000));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("cannot be empty"));
    }

    #[test]
    fn test_sanitize_command_too_long() {
        let long_cmd = "a".repeat(100);
        let result = sanitize_command(&long_cmd, Some(50));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("too long"));
    }

    #[test]
    fn test_sanitize_command_exactly_at_limit() {
        let cmd = "a".repeat(50);
        let result = sanitize_command(&cmd, Some(50));
        assert!(result.is_ok());
    }

    #[test]
    fn test_sanitize_command_unlimited() {
        let long_cmd = "a".repeat(10000);
        let result = sanitize_command(&long_cmd, None);
        assert!(result.is_ok());
    }

    #[test]
    fn test_escape_command_for_shell_no_quotes() {
        let escaped = escape_command_for_shell("ls -la");
        assert_eq!(escaped, "ls -la");
    }

    #[test]
    fn test_escape_command_for_shell_with_quotes() {
        let escaped = escape_command_for_shell("echo 'hello'");
        assert_eq!(escaped, "echo '\"'\"'hello'\"'\"'");
    }

    #[test]
    fn test_escape_command_for_shell_dollar_sign() {
        let escaped = escape_command_for_shell("echo $HOME");
        assert_eq!(escaped, "echo \\$HOME");
    }

    #[test]
    fn test_escape_command_for_shell_backtick() {
        let escaped = escape_command_for_shell("echo `date`");
        assert_eq!(escaped, "echo \\`date\\`");
    }

    #[test]
    fn test_escape_command_for_shell_backslash() {
        let escaped = escape_command_for_shell("echo \\n");
        assert_eq!(escaped, "echo \\\\n");
    }

    #[test]
    fn test_escape_command_for_shell_parentheses() {
        let escaped = escape_command_for_shell("echo (test)");
        assert_eq!(escaped, "echo \\(test\\)");
    }

    #[test]
    fn test_escape_command_for_shell_pipe() {
        let escaped = escape_command_for_shell("cat file | grep test");
        assert_eq!(escaped, "cat file \\| grep test");
    }

    #[test]
    fn test_escape_command_for_shell_combined_special_chars() {
        let escaped = escape_command_for_shell("echo '$HOME' | cat");
        assert_eq!(escaped, "echo '\"'\"'\\$HOME'\"'\"' \\| cat");
    }

    #[test]
    fn test_escape_command_for_shell_multiple_quotes() {
        let escaped = escape_command_for_shell("echo 'a' 'b'");
        assert_eq!(escaped, "echo '\"'\"'a'\"'\"' '\"'\"'b'\"'\"'");
    }

    #[test]
    fn test_escape_command_for_shell_empty() {
        let escaped = escape_command_for_shell("");
        assert_eq!(escaped, "");
    }

    #[test]
    fn test_escape_for_timeout_wrapper_no_special_chars() {
        let escaped = escape_for_timeout_wrapper("sleep 10");
        assert_eq!(escaped, "sleep 10");
    }

    #[test]
    fn test_escape_for_timeout_wrapper_with_single_quotes() {
        let escaped = escape_for_timeout_wrapper("echo 'hello'");
        assert_eq!(escaped, "echo '\"'\"'hello'\"'\"'");
    }

    #[test]
    fn test_escape_for_timeout_wrapper_with_backslashes() {
        let escaped = escape_for_timeout_wrapper("echo \\$HOME");
        assert_eq!(escaped, "echo \\\\$HOME");
    }

    #[test]
    fn test_escape_for_timeout_wrapper_with_both_quotes_and_backslashes() {
        let escaped = escape_for_timeout_wrapper("echo '$HOME'");
        assert_eq!(escaped, "echo '\"'\"'$HOME'\"'\"'");
    }

    #[test]
    fn test_escape_for_timeout_wrapper_empty() {
        let escaped = escape_for_timeout_wrapper("");
        assert_eq!(escaped, "");
    }

    #[test]
    fn test_escape_for_timeout_wrapper_multiple_quotes() {
        let escaped = escape_for_timeout_wrapper("echo 'a' 'b'");
        assert_eq!(escaped, "echo '\"'\"'a'\"'\"' '\"'\"'b'\"'\"'");
    }
}
