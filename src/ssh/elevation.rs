//! Privilege elevation utilities for SSH command execution
//!
//! This module provides utilities for:
//! - Wrapping commands with `sudo` for privilege escalation
//! - Escaping passwords and commands for safe shell execution
//!
//! The elevation logic for `su` shells is implemented directly in
//! [`SshConnectionManager`](super::connection::SshConnectionManager).

/// Wraps a command for execution with sudo privileges.
///
/// # Arguments
/// * `command` - The command to wrap with sudo
/// * `password` - Optional sudo password. If None, uses `sudo -n` (passwordless).
///   If Some, uses `printf | sudo -S` to pipe the password.
///
/// # Returns
/// A string containing the wrapped command ready for execution.
///
/// # Examples
///
/// ```
/// use ssh_mcp::ssh::elevation::wrap_sudo_command;
///
/// // Passwordless sudo
/// let cmd = wrap_sudo_command("apt update", None);
/// assert_eq!(cmd, "sudo -n sh -c 'apt update'");
///
/// // Sudo with password
/// let cmd = wrap_sudo_command("apt update", Some("mypassword"));
/// assert_eq!(cmd, "printf '%s\\n' 'mypassword' | sudo -S -p '' sh -c 'apt update'");
/// ```
pub fn wrap_sudo_command(command: &str, password: Option<&str>) -> String {
    let escaped_command = escape_for_shell(command);

    match password {
        None => {
            // No password provided, use -n to fail if sudo requires a password
            format!("sudo -n sh -c '{}'", escaped_command)
        }
        Some(pwd) => {
            // Password provided — pipe it into sudo using printf. This avoids complex
            // PTY/stdin handling on the SSH channel and is simpler and more reliable.
            let escaped_pwd = escape_for_shell(pwd);
            format!(
                "printf '%s\\n' '{}' | sudo -S -p '' sh -c '{}'",
                escaped_pwd, escaped_command
            )
        }
    }
}

/// Escapes a string for safe use in single-quoted shell contexts.
///
/// Replaces single quotes with the pattern `'"'"'` which:
/// 1. Ends the current single-quoted string
/// 2. Adds an escaped single quote
/// 3. Starts a new single-quoted string
///
/// This is the standard POSIX-compliant method for escaping single quotes
/// within single-quoted strings.
///
/// # Examples
///
/// ```
/// use ssh_mcp::ssh::elevation::escape_for_shell;
///
/// assert_eq!(escape_for_shell("hello"), "hello");
/// assert_eq!(escape_for_shell("it's"), "it'\"'\"'s");
/// assert_eq!(escape_for_shell("a'b'c"), "a'\"'\"'b'\"'\"'c");
/// ```
pub fn escape_for_shell(s: &str) -> String {
    crate::shell_escape::escape_for_shell(s)
}

/// Checks if a password is valid for use in sudo commands.
///
/// A valid password:
/// - Is not empty after trimming
/// - Does not contain null bytes
///
/// # Arguments
/// * `password` - The password to validate
///
/// # Returns
/// `true` if the password is valid, `false` otherwise.
pub fn is_valid_password(password: &str) -> bool {
    !password.trim().is_empty() && !password.contains('\0')
}

/// Sanitizes a password by trimming whitespace.
///
/// Returns `None` if the password is empty or only whitespace.
///
/// # Arguments
/// * `password` - The password to sanitize
///
/// # Returns
/// `Some(String)` with the sanitized password, or `None` if invalid.
pub fn sanitize_password(password: Option<&str>) -> Option<String> {
    password
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .map(|p| p.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wrap_sudo_command_without_password() {
        let result = wrap_sudo_command("apt update", None);
        assert_eq!(result, "sudo -n sh -c 'apt update'");
    }

    #[test]
    fn test_wrap_sudo_command_with_password() {
        let result = wrap_sudo_command("apt update", Some("secret123"));
        assert_eq!(
            result,
            "printf '%s\\n' 'secret123' | sudo -S -p '' sh -c 'apt update'"
        );
    }

    #[test]
    fn test_wrap_sudo_command_with_quotes_in_command() {
        let result = wrap_sudo_command("echo 'hello world'", None);
        assert_eq!(result, "sudo -n sh -c 'echo '\"'\"'hello world'\"'\"''");
    }

    #[test]
    fn test_wrap_sudo_command_with_quotes_in_password() {
        let result = wrap_sudo_command("apt update", Some("pass'word"));
        assert_eq!(
            result,
            "printf '%s\\n' 'pass'\"'\"'word' | sudo -S -p '' sh -c 'apt update'"
        );
    }

    #[test]
    fn test_wrap_sudo_command_complex() {
        let result = wrap_sudo_command("cat /etc/shadow | grep root", Some("admin123"));
        assert_eq!(
            result,
            "printf '%s\\n' 'admin123' | sudo -S -p '' sh -c 'cat /etc/shadow | grep root'"
        );
    }

    #[test]
    fn test_escape_for_shell_no_quotes() {
        assert_eq!(escape_for_shell("hello world"), "hello world");
    }

    #[test]
    fn test_escape_for_shell_single_quote() {
        assert_eq!(escape_for_shell("it's"), "it'\"'\"'s");
    }

    #[test]
    fn test_escape_for_shell_multiple_quotes() {
        assert_eq!(
            escape_for_shell("'a' and 'b'"),
            "'\"'\"'a'\"'\"' and '\"'\"'b'\"'\"'"
        );
    }

    #[test]
    fn test_is_valid_password() {
        assert!(is_valid_password("secret123"));
        assert!(is_valid_password("with spaces"));
        assert!(!is_valid_password(""));
        assert!(!is_valid_password("   "));
        assert!(!is_valid_password("has\0null"));
    }

    #[test]
    fn test_sanitize_password() {
        assert_eq!(
            sanitize_password(Some("secret")),
            Some("secret".to_string())
        );
        assert_eq!(
            sanitize_password(Some("  secret  ")),
            Some("secret".to_string())
        );
        assert_eq!(sanitize_password(Some("")), None);
        assert_eq!(sanitize_password(Some("   ")), None);
        assert_eq!(sanitize_password(None), None);
    }
}
