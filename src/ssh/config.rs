//! SSH configuration types
//!
//! Configuration for SSH connection parameters including authentication.

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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ssh_config_builder() {
        let config = SshConfig::new("192.168.1.1", "admin")
            .with_port(2222)
            .with_password("secret");

        assert_eq!(config.host, "192.168.1.1");
        assert_eq!(config.port, 2222);
        assert_eq!(config.username, "admin");
        assert_eq!(config.password, Some("secret".to_string()));
        assert!(config.private_key.is_none());
    }
}
