//! Configuration and CLI argument parsing for SSH MCP Server

use clap::Parser;
use std::path::PathBuf;

use crate::error::{Result, SshMcpError};

/// Default timeout for command execution in milliseconds
pub const DEFAULT_TIMEOUT_MS: u64 = 60_000; // 60 seconds

/// Default max characters for command output (None = unlimited)
pub const DEFAULT_MAX_CHARS: Option<usize> = Some(1000);

/// Connection timeout in seconds
pub const CONNECTION_TIMEOUT_SECS: u64 = 30;

/// SSH MCP Server CLI Arguments
#[derive(Parser, Debug, Clone)]
#[command(name = "ssh-mcp")]
#[command(author = "0FL01")]
#[command(version = "1.4.0")]
#[command(about = "MCP server exposing SSH control for Linux systems via Model Context Protocol")]
pub struct Args {
    /// SSH host to connect to
    #[arg(long, env = "SSH_MCP_HOST")]
    pub host: String,

    /// SSH port
    #[arg(long, default_value = "22", env = "SSH_MCP_PORT")]
    pub port: u16,

    /// SSH username
    #[arg(long, env = "SSH_MCP_USER")]
    pub user: String,

    /// SSH password (alternative to key)
    #[arg(long, env = "SSH_MCP_PASSWORD")]
    pub password: Option<String>,

    /// Path to SSH private key file (alternative to password)
    #[arg(long, env = "SSH_MCP_KEY")]
    pub key: Option<PathBuf>,

    /// Password for `su` elevation
    #[arg(long, env = "SSH_MCP_SU_PASSWORD")]
    pub su_password: Option<String>,

    /// Password for `sudo` commands (if different from su_password)
    #[arg(long, env = "SSH_MCP_SUDO_PASSWORD")]
    pub sudo_password: Option<String>,

    /// Command execution timeout in milliseconds
    #[arg(long, default_value = "60000", env = "SSH_MCP_TIMEOUT")]
    pub timeout: u64,

    /// Maximum characters for command length.
    /// Use "none", "0", or negative value to disable limit.
    /// Default: 1000
    #[arg(long = "maxChars", env = "SSH_MCP_MAX_CHARS")]
    pub max_chars: Option<String>,

    /// Disable the sudo-exec tool
    #[arg(long, default_value = "false", env = "SSH_MCP_DISABLE_SUDO")]
    pub disable_sudo: bool,

    /// Maximum output tokens for command execution.
    /// Use "none" or "0" to disable limit.
    /// Supports "k" suffix (e.g., "12k" for 12000).
    /// Default: 12000 (approximately 50KB)
    #[arg(long = "max-output-tokens", env = "SSH_MCP_MAX_OUTPUT_TOKENS")]
    pub max_output_tokens: Option<String>,

    /// Logging level: trace, debug, info, warn, error
    #[arg(long, default_value = "info", env = "SSH_MCP_LOG_LEVEL", value_parser = clap::builder::PossibleValuesParser::new(["trace", "debug", "info", "warn", "error"]))]
    pub log_level: String,

    /// Log file path (default: stdout only)
    #[arg(long, env = "SSH_MCP_LOG_FILE")]
    pub log_file: Option<PathBuf>,

    /// Log format: text or json
    #[arg(long, default_value = "text", env = "SSH_MCP_LOG_FORMAT", value_parser = clap::builder::PossibleValuesParser::new(["text", "json"]))]
    pub log_format: String,

    /// Log rotation strategy: daily, hourly, never
    #[arg(long, default_value = "daily", env = "SSH_MCP_LOG_ROTATION", value_parser = clap::builder::PossibleValuesParser::new(["daily", "hourly", "never"]))]
    pub log_rotation: String,

    /// Keepalive interval in seconds (default: 30)
    /// Sends keepalive packets to maintain connection like a human user
    #[arg(long, default_value = "30", env = "SSH_MCP_KEEPALIVE_INTERVAL")]
    pub keepalive_interval: u64,

    /// Maximum keepalive failures before disconnecting (default: 3)
    /// Total idle timeout = keepalive_interval * keepalive_max
    #[arg(long, default_value = "3", env = "SSH_MCP_KEEPALIVE_MAX")]
    pub keepalive_max: u64,
}

/// Parsed and validated configuration
#[derive(Debug, Clone)]
pub struct Config {
    /// SSH host
    pub host: String,

    /// SSH port
    pub port: u16,

    /// SSH username
    pub user: String,

    /// SSH password
    pub password: Option<String>,

    /// Path to SSH private key
    pub key: Option<PathBuf>,

    /// Password for su elevation
    pub su_password: Option<String>,

    /// Password for sudo commands
    pub sudo_password: Option<String>,

    /// Command timeout in milliseconds
    pub timeout_ms: u64,

    /// Maximum command length (None = unlimited)
    pub max_chars: Option<usize>,

    /// Maximum output tokens for command execution (None = unlimited)
    pub max_output_tokens: Option<usize>,

    /// Whether sudo-exec tool is disabled
    pub disable_sudo: bool,

    /// Keepalive interval in seconds
    pub keepalive_interval: u64,

    /// Maximum keepalive failures before disconnecting
    pub keepalive_max: u64,
}

impl Config {
    /// Create Config from CLI Args
    pub fn from_args(args: Args) -> Result<Self> {
        validate_args(&args)?;

        let max_chars = parse_max_chars(args.max_chars.as_deref());
        let max_output_tokens = parse_max_output_tokens(args.max_output_tokens.as_deref());

        Ok(Config {
            host: args.host,
            port: args.port,
            user: args.user,
            password: sanitize_password(args.password),
            key: args.key,
            su_password: sanitize_password(args.su_password),
            sudo_password: sanitize_password(args.sudo_password),
            timeout_ms: args.timeout,
            max_chars,
            max_output_tokens,
            disable_sudo: args.disable_sudo,
            keepalive_interval: args.keepalive_interval,
            keepalive_max: args.keepalive_max,
        })
    }
}

/// Validate CLI arguments
fn validate_args(args: &Args) -> Result<()> {
    let mut errors = Vec::new();

    if args.host.is_empty() {
        errors.push("Missing required --host".to_string());
    }

    if args.user.is_empty() {
        errors.push("Missing required --user".to_string());
    }

    // Must have either password or key
    if args.password.is_none() && args.key.is_none() {
        errors.push("Must provide either --password or --key".to_string());
    }

    // If key is provided, check if file exists
    if let Some(ref key_path) = args.key
        && !key_path.exists()
    {
        errors.push(format!("SSH key file not found: {}", key_path.display()));
    }

    if !errors.is_empty() {
        return Err(SshMcpError::Config(format!(
            "Configuration error:\n{}",
            errors.join("\n")
        )));
    }

    Ok(())
}

/// Default max output tokens (12_000 ≈ 50KB)
pub const DEFAULT_MAX_OUTPUT_TOKENS: Option<usize> = Some(12_000);

/// Parse max_chars argument
///
/// - "none" (case-insensitive) → None (unlimited)
/// - "0" or negative → None (unlimited)
/// - positive integer → Some(value)
/// - None (not provided) → DEFAULT_MAX_CHARS
pub fn parse_max_chars(value: Option<&str>) -> Option<usize> {
    match value {
        None => DEFAULT_MAX_CHARS,
        Some(s) => {
            let lowered = s.to_lowercase();
            if lowered == "none" {
                return None;
            }

            match s.parse::<i64>() {
                Ok(n) if n <= 0 => None,
                Ok(n) => Some(n as usize),
                Err(_) => DEFAULT_MAX_CHARS,
            }
        }
    }
}

/// Parse max_output_tokens argument
///
/// - "none" (case-insensitive) → None (unlimited)
/// - "0" or negative → None (unlimited)
/// - positive integer with optional "k" suffix (e.g., "12k") → Some(value)
/// - None (not provided) → DEFAULT_MAX_OUTPUT_TOKENS
pub fn parse_max_output_tokens(value: Option<&str>) -> Option<usize> {
    match value {
        None => DEFAULT_MAX_OUTPUT_TOKENS,
        Some(s) => {
            let lowered = s.to_lowercase().replace(" ", "");
            if lowered == "none" {
                return None;
            }

            // Try to parse with k suffix
            if lowered.ends_with('k') {
                let num_part = &lowered[..lowered.len() - 1];
                match num_part.parse::<i64>() {
                    Ok(n) if n <= 0 => None,
                    Ok(n) => Some((n as usize).saturating_mul(1_000)),
                    Err(_) => DEFAULT_MAX_OUTPUT_TOKENS,
                }
            } else {
                match lowered.parse::<i64>() {
                    Ok(n) if n <= 0 => None,
                    Ok(n) => Some(n as usize),
                    Err(_) => DEFAULT_MAX_OUTPUT_TOKENS,
                }
            }
        }
    }
}

/// Sanitize password: return None if empty
fn sanitize_password(password: Option<String>) -> Option<String> {
    password.filter(|p| !p.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_max_chars_none_string() {
        assert_eq!(parse_max_chars(Some("none")), None);
        assert_eq!(parse_max_chars(Some("None")), None);
        assert_eq!(parse_max_chars(Some("NONE")), None);
    }

    #[test]
    fn test_parse_max_chars_zero_or_negative() {
        assert_eq!(parse_max_chars(Some("0")), None);
        assert_eq!(parse_max_chars(Some("-1")), None);
        assert_eq!(parse_max_chars(Some("-100")), None);
    }

    #[test]
    fn test_parse_max_chars_positive() {
        assert_eq!(parse_max_chars(Some("500")), Some(500));
        assert_eq!(parse_max_chars(Some("2000")), Some(2000));
    }

    #[test]
    fn test_parse_max_chars_invalid() {
        // Invalid strings should return default
        assert_eq!(parse_max_chars(Some("abc")), DEFAULT_MAX_CHARS);
        assert_eq!(parse_max_chars(Some("")), DEFAULT_MAX_CHARS);
    }

    #[test]
    fn test_parse_max_chars_not_provided() {
        assert_eq!(parse_max_chars(None), DEFAULT_MAX_CHARS);
    }

    #[test]
    fn test_sanitize_password() {
        assert_eq!(
            sanitize_password(Some("secret".to_string())),
            Some("secret".to_string())
        );
        assert_eq!(sanitize_password(Some("".to_string())), None);
        assert_eq!(sanitize_password(None), None);
    }

    #[test]
    fn test_parse_max_output_tokens_none_string() {
        assert_eq!(parse_max_output_tokens(Some("none")), None);
        assert_eq!(parse_max_output_tokens(Some("None")), None);
        assert_eq!(parse_max_output_tokens(Some("NONE")), None);
    }

    #[test]
    fn test_parse_max_output_tokens_zero_or_negative() {
        assert_eq!(parse_max_output_tokens(Some("0")), None);
        assert_eq!(parse_max_output_tokens(Some("-1")), None);
        assert_eq!(parse_max_output_tokens(Some("-100")), None);
    }

    #[test]
    fn test_parse_max_output_tokens_positive() {
        assert_eq!(parse_max_output_tokens(Some("500")), Some(500));
        assert_eq!(parse_max_output_tokens(Some("12000")), Some(12_000));
    }

    #[test]
    fn test_parse_max_output_tokens_with_k_suffix() {
        assert_eq!(parse_max_output_tokens(Some("12k")), Some(12_000));
        assert_eq!(parse_max_output_tokens(Some("5K")), Some(5_000));
        assert_eq!(parse_max_output_tokens(Some("100k")), Some(100_000));
    }

    #[test]
    fn test_parse_max_output_tokens_invalid() {
        // Invalid strings should return default
        assert_eq!(
            parse_max_output_tokens(Some("abc")),
            DEFAULT_MAX_OUTPUT_TOKENS
        );
        assert_eq!(parse_max_output_tokens(Some("")), DEFAULT_MAX_OUTPUT_TOKENS);
    }

    #[test]
    fn test_parse_max_output_tokens_not_provided() {
        assert_eq!(parse_max_output_tokens(None), DEFAULT_MAX_OUTPUT_TOKENS);
    }
}
