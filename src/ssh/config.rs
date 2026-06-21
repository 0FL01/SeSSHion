//! SSH configuration types
//!
//! Configuration for SSH connection parameters including authentication.

use std::path::PathBuf;

use clap::ValueEnum;

/// SSH host key verification policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum HostKeyCheckMode {
    /// Require an existing matching known_hosts entry.
    Yes,
    /// Learn unknown keys, but reject changed keys.
    #[default]
    AcceptNew,
    /// Disable host key verification.
    No,
}

impl HostKeyCheckMode {
    /// OpenSSH StrictHostKeyChecking value for this policy.
    pub fn as_openssh_value(self) -> &'static str {
        match self {
            Self::Yes => "yes",
            Self::AcceptNew => "accept-new",
            Self::No => "no",
        }
    }
}

/// Seconds to send SIGKILL after SIGTERM when using timeout
pub const TIMEOUT_KILL_AFTER_SECS: u64 = 2;

/// Timeout for timeout command availability detection (ms)
pub const TIMEOUT_DETECTION_TIMEOUT_MS: u64 = 5000;

/// SSH connection configuration
#[derive(Debug, Clone)]
pub struct SshConfig {
    /// Remote hostname or IP address
    pub host: String,

    /// SSH port (default: 22)
    pub port: u16,

    /// Username for authentication
    pub username: String,

    /// Password for password authentication
    pub password: Option<String>,

    /// Private key content (not path!) for key authentication
    pub private_key: Option<String>,

    /// Password for `su` elevation to root
    pub su_password: Option<String>,

    /// Password for `sudo` commands (if different from su_password)
    pub sudo_password: Option<String>,

    /// Keepalive interval in seconds (default: 30s)
    /// Sends keepalive packets to maintain connection like a human user
    pub keepalive_interval: u64,

    /// Maximum keepalive failures before disconnecting (default: 3)
    /// How many keepalive packets can be missed before connection drops
    pub keepalive_max: u64,

    /// Maximum output tokens for command execution (default: 16_000)
    /// Prevents OOM and context overflow for large outputs
    pub max_output_tokens: Option<usize>,

    /// Number of reconnect retries after the initial attempt (default: 3)
    pub reconnect_retries: u64,

    /// Base reconnect backoff in milliseconds (default: 250)
    pub reconnect_backoff_ms: u64,

    /// Health probe timeout in milliseconds for active session checks (default: 1500)
    pub health_probe_timeout_ms: u64,

    /// SSH host key verification policy.
    pub host_key_checking: HostKeyCheckMode,

    /// Optional known_hosts file path.
    pub known_hosts: Option<PathBuf>,
}

impl SshConfig {
    /// Create a new SSH configuration with minimal required fields
    pub fn new(host: impl Into<String>, username: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            port: 22,
            username: username.into(),
            password: None,
            private_key: None,
            su_password: None,
            sudo_password: None,
            keepalive_interval: 30,
            keepalive_max: 3,
            max_output_tokens: Some(16_000),
            reconnect_retries: 3,
            reconnect_backoff_ms: 250,
            health_probe_timeout_ms: 1500,
            host_key_checking: HostKeyCheckMode::default(),
            known_hosts: None,
        }
    }

    /// Set the SSH port
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Set password authentication
    pub fn with_password(mut self, password: impl Into<String>) -> Self {
        self.password = Some(password.into());
        self
    }

    /// Set private key authentication (key content, not path)
    pub fn with_private_key(mut self, key: impl Into<String>) -> Self {
        self.private_key = Some(key.into());
        self
    }

    /// Set su password for privilege elevation
    pub fn with_su_password(mut self, password: impl Into<String>) -> Self {
        self.su_password = Some(password.into());
        self
    }

    /// Set sudo password for sudo commands
    pub fn with_sudo_password(mut self, password: impl Into<String>) -> Self {
        self.sudo_password = Some(password.into());
        self
    }

    /// Set keepalive interval in seconds (default: 30s)
    /// Lower values = more frequent keepalives (detect dead connections faster)
    /// Higher values = less network overhead (more like idle human session)
    pub fn with_keepalive_interval(mut self, secs: u64) -> Self {
        self.keepalive_interval = secs;
        self
    }

    /// Set maximum keepalive failures before disconnecting (default: 3)
    /// Total idle timeout = keepalive_interval * keepalive_max
    /// Example: 30s * 3 = 90s of inactivity before disconnect
    pub fn with_keepalive_max(mut self, max: u64) -> Self {
        self.keepalive_max = max;
        self
    }

    /// Set maximum output tokens for command execution (default: 16_000)
    /// Set to None for unlimited output (not recommended for large outputs)
    pub fn with_max_output_tokens(mut self, tokens: Option<usize>) -> Self {
        self.max_output_tokens = tokens;
        self
    }

    /// Set reconnect retries after the initial attempt (default: 3)
    pub fn with_reconnect_retries(mut self, retries: u64) -> Self {
        self.reconnect_retries = retries;
        self
    }

    /// Set base reconnect backoff in milliseconds (default: 250)
    pub fn with_reconnect_backoff_ms(mut self, backoff_ms: u64) -> Self {
        self.reconnect_backoff_ms = backoff_ms;
        self
    }

    /// Set health probe timeout in milliseconds (default: 1500)
    pub fn with_health_probe_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.health_probe_timeout_ms = timeout_ms;
        self
    }

    /// Set SSH host key verification policy.
    pub fn with_host_key_checking(mut self, mode: HostKeyCheckMode) -> Self {
        self.host_key_checking = mode;
        self
    }

    /// Set a custom known_hosts file path.
    pub fn with_known_hosts(mut self, known_hosts: Option<PathBuf>) -> Self {
        self.known_hosts = known_hosts;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ssh_config_builder() {
        let config = SshConfig::new("192.168.1.1", "admin")
            .with_port(2222)
            .with_password("secret")
            .with_max_output_tokens(Some(5_000))
            .with_reconnect_retries(4)
            .with_reconnect_backoff_ms(500)
            .with_health_probe_timeout_ms(1_200);

        assert_eq!(config.host, "192.168.1.1");
        assert_eq!(config.port, 2222);
        assert_eq!(config.username, "admin");
        assert_eq!(config.password, Some("secret".to_string()));
        assert!(config.private_key.is_none());
        assert_eq!(config.reconnect_retries, 4);
        assert_eq!(config.reconnect_backoff_ms, 500);
        assert_eq!(config.health_probe_timeout_ms, 1_200);
        assert_eq!(config.host_key_checking, HostKeyCheckMode::AcceptNew);
        assert!(config.known_hosts.is_none());
    }
}
