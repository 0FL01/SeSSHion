//! Error types for SeSSHion

use thiserror::Error;

/// Main error type for SeSSHion
#[derive(Debug, Error)]
pub enum SshMcpError {
    /// SSH connection failed
    #[error("SSH connection error: {0}")]
    Connection(String),

    /// Authentication failed (password, key, or su/sudo)
    #[error("Authentication failed: {0}")]
    Authentication(String),

    /// Command execution timed out
    #[error("Command timeout after {0}ms")]
    Timeout(u64),

    /// Invalid parameters provided
    #[error("Invalid parameters: {0}")]
    InvalidParams(String),

    /// su/sudo elevation failed
    #[error("Elevation failed: {0}")]
    ElevationFailed(String),

    /// Configuration error
    #[error("Configuration error: {0}")]
    Config(String),

    /// IO error
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// SSH key parsing error
    #[error("SSH key error: {0}")]
    SshKey(String),
}

/// Result type alias using SshMcpError
pub type Result<T> = std::result::Result<T, SshMcpError>;

impl SshMcpError {
    /// Create a connection error from a string
    pub fn connection(msg: impl Into<String>) -> Self {
        SshMcpError::Connection(msg.into())
    }

    /// Create an authentication error from a string
    pub fn auth(msg: impl Into<String>) -> Self {
        SshMcpError::Authentication(msg.into())
    }

    /// Create an invalid params error from a string
    pub fn invalid_params(msg: impl Into<String>) -> Self {
        SshMcpError::InvalidParams(msg.into())
    }

    /// Create an elevation failed error from a string
    pub fn elevation_failed(msg: impl Into<String>) -> Self {
        SshMcpError::ElevationFailed(msg.into())
    }

    /// Create a config error from a string
    pub fn config(msg: impl Into<String>) -> Self {
        SshMcpError::Config(msg.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        let err = SshMcpError::Connection("failed to connect".to_string());
        assert_eq!(err.to_string(), "SSH connection error: failed to connect");

        let err = SshMcpError::Timeout(5000);
        assert_eq!(err.to_string(), "Command timeout after 5000ms");
    }
}
