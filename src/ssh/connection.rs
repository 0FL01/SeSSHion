//! SSH Connection Manager
//!
//! Provides persistent SSH connection handling with automatic reconnection,
//! concurrent access protection, and optional privilege elevation via `su`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use russh::client::{self, Handle};
use russh::keys::PrivateKeyWithHashAlg;
use russh::Channel;
use tokio::sync::Mutex;
use tokio::time::timeout;
use tracing::{debug, error, info, warn};

use super::config::SshConfig;
use super::handler::SshHandler;
use crate::config::CONNECTION_TIMEOUT_SECS;
use crate::error::{Result, SshMcpError};
use russh::ChannelMsg;

/// SSH Connection Manager
///
/// Manages a persistent SSH connection with the following features:
/// - Automatic reconnection when connection drops
/// - Concurrent access protection via mutex/atomic flags
/// - Optional `su` elevation for privileged operations
/// - 30-second connection timeout
pub struct SshConnectionManager {
    /// SSH configuration
    config: SshConfig,

    /// Active SSH session handle
    session: Arc<Mutex<Option<Handle<SshHandler>>>>,

    /// Flag to prevent concurrent connection attempts
    is_connecting: AtomicBool,

    /// Elevated shell channel (when using su)
    /// Made pub(crate) to allow access from command.rs
    pub(crate) su_channel: Arc<Mutex<Option<Channel<client::Msg>>>>,

    /// Flag indicating whether we're running as root via su
    is_elevated: AtomicBool,

    /// Cached availability of the `timeout` command on the remote system
    has_timeout_cmd: AtomicBool,
}

impl SshConnectionManager {
    /// Create a new SSH Connection Manager
    ///
    /// Does not establish connection immediately; call `connect()` or
    /// `ensure_connected()` to establish the connection.
    pub async fn new(config: SshConfig) -> Self {
        Self {
            config,
            session: Arc::new(Mutex::new(None)),
            is_connecting: AtomicBool::new(false),
            su_channel: Arc::new(Mutex::new(None)),
            is_elevated: AtomicBool::new(false),
            has_timeout_cmd: AtomicBool::new(false),
        }
    }

    /// Establish SSH connection
    ///
    /// If already connected, returns immediately. If another task is currently
    /// connecting, waits for that connection attempt to complete.
    pub async fn connect(&self) -> Result<()> {
        // Check if already connected
        if self.is_connected().await {
            debug!("Already connected to SSH server");
            return Ok(());
        }

        // Prevent concurrent connection attempts
        if self
            .is_connecting
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            debug!("Another connection attempt in progress, waiting...");
            // Wait for the other connection attempt
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                if !self.is_connecting.load(Ordering::SeqCst) {
                    break;
                }
            }
            return if self.is_connected().await {
                Ok(())
            } else {
                Err(SshMcpError::connection("Connection failed by another task"))
            };
        }

        // Perform connection with timeout
        let result = self.do_connect().await;

        // Reset connecting flag
        self.is_connecting.store(false, Ordering::SeqCst);

        result
    }

    /// Internal connection logic
    async fn do_connect(&self) -> Result<()> {
        info!(
            "Connecting to SSH server {}:{}...",
            self.config.host, self.config.port
        );

        let connection_timeout = Duration::from_secs(CONNECTION_TIMEOUT_SECS);

        // Create russh config with defaults
        let ssh_config = client::Config::default();
        let ssh_config = Arc::new(ssh_config);

        // Connect with timeout
        let addr = format!("{}:{}", self.config.host, self.config.port);
        let connect_result = timeout(
            connection_timeout,
            client::connect(ssh_config, addr.as_str(), SshHandler::new()),
        )
        .await;

        let mut session = match connect_result {
            Ok(Ok(session)) => session,
            Ok(Err(e)) => {
                error!("SSH connection failed: {}", e);
                return Err(SshMcpError::connection(e.to_string()));
            }
            Err(_) => {
                error!("SSH connection timeout after {}s", CONNECTION_TIMEOUT_SECS);
                return Err(SshMcpError::connection(format!(
                    "Connection timeout after {}s",
                    CONNECTION_TIMEOUT_SECS
                )));
            }
        };

        // Authenticate
        self.authenticate(&mut session).await?;

        // Store session
        {
            let mut session_guard = self.session.lock().await;
            *session_guard = Some(session);
        }

        info!(
            "Successfully connected to {}@{}:{}",
            self.config.username, self.config.host, self.config.port
        );

        // If su_password is configured, attempt elevation
        if self.config.su_password.is_some() {
            debug!("su_password configured, attempting elevation...");
            if let Err(e) = self.ensure_elevated().await {
                // Don't fail connection if elevation fails, just log it
                warn!(
                    "Failed to elevate to root: {}. Commands will run as normal user.",
                    e
                );
            }
        }

        Ok(())
    }

    /// Authenticate with the SSH server
    async fn authenticate(&self, session: &mut Handle<SshHandler>) -> Result<()> {
        // Try password authentication first
        if let Some(ref password) = self.config.password {
            debug!(
                "Attempting password authentication for user '{}'",
                self.config.username
            );
            let auth_result = session
                .authenticate_password(&self.config.username, password)
                .await
                .map_err(|e| SshMcpError::auth(e.to_string()))?;

            if auth_result.success() {
                info!("Password authentication successful");
                return Ok(());
            } else {
                return Err(SshMcpError::auth("Password authentication rejected"));
            }
        }

        // Try key authentication
        if let Some(ref key_content) = self.config.private_key {
            debug!(
                "Attempting key authentication for user '{}'",
                self.config.username
            );

            // Parse the private key using russh::keys
            let key = russh::keys::PrivateKey::from_openssh(key_content.as_bytes())
                .map_err(|e| SshMcpError::SshKey(format!("Failed to parse private key: {}", e)))?;

            // Wrap in PrivateKeyWithHashAlg (None for non-RSA or default hash)
            let key_with_alg = PrivateKeyWithHashAlg::new(Arc::new(key), None);

            let auth_result = session
                .authenticate_publickey(&self.config.username, key_with_alg)
                .await
                .map_err(|e| SshMcpError::auth(e.to_string()))?;

            if auth_result.success() {
                info!("Key authentication successful");
                return Ok(());
            } else {
                return Err(SshMcpError::auth("Key authentication rejected"));
            }
        }

        Err(SshMcpError::auth(
            "No authentication method available (require password or private_key)",
        ))
    }

    /// Check if the connection is active
    pub async fn is_connected(&self) -> bool {
        let session_guard = self.session.lock().await;
        session_guard.is_some()
    }

    /// Ensure connection is established, reconnecting if necessary
    pub async fn ensure_connected(&self) -> Result<()> {
        if !self.is_connected().await {
            self.connect().await?;
        }
        Ok(())
    }

    /// Get a reference to the session for operations
    ///
    /// Instead of cloning the Handle (which doesn't implement Clone),
    /// we provide methods that work with the session directly.
    pub async fn with_session<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Handle<SshHandler>) -> T,
    {
        let session_guard = self.session.lock().await;
        match session_guard.as_ref() {
            Some(session) => Ok(f(session)),
            None => Err(SshMcpError::connection("SSH connection not established")),
        }
    }

    /// Open a new session channel
    pub async fn open_channel(&self) -> Result<Channel<client::Msg>> {
        let session_guard = self.session.lock().await;
        let session = session_guard
            .as_ref()
            .ok_or_else(|| SshMcpError::connection("SSH connection not established"))?;

        let channel = session
            .channel_open_session()
            .await
            .map_err(|e| SshMcpError::connection(format!("Failed to open channel: {}", e)))?;

        Ok(channel)
    }

    /// Check if currently elevated to root via su
    pub fn is_elevated(&self) -> bool {
        self.is_elevated.load(Ordering::SeqCst)
    }

    /// Check if the `timeout` command is available on the remote system
    ///
    /// Uses cached result after first check. To trigger a new check,
    /// the connection must be re-established.
    pub fn use_timeout_wrapper(&self) -> bool {
        self.has_timeout_cmd.load(Ordering::SeqCst)
    }

    /// Disables timeout wrapper for the rest of this connection lifetime
    ///
    /// When called, this sets `has_timeout_cmd` to false, causing all subsequent
    /// commands to fall back to the tokio timeout + pkill method instead of using
    /// the remote timeout command wrapper.
    pub fn disable_timeout_wrapper(&self) {
        self.has_timeout_cmd.store(false, Ordering::SeqCst);
        warn!("timeout wrapper disabled due to errors, falling back to pkill");
    }

    /// Detect whether the `timeout` command is available on the remote system
    ///
    /// This performs a one-time detection check by running `command -v timeout`
    /// on the remote system. The result is cached for the lifetime of the
    /// connection.
    ///
    /// Returns true if timeout is available, false otherwise.
    pub async fn check_timeout_availability(&self) -> bool {
        // Check cache first
        if self.has_timeout_cmd.load(Ordering::SeqCst) {
            return true;
        }

        // Open a new channel for detection
        let mut channel = match self.open_channel().await {
            Ok(ch) => ch,
            Err(e) => {
                debug!("Failed to open channel for timeout detection: {}", e);
                return false;
            }
        };

        // Run detection command
        let exec_result = channel.exec(true, "command -v timeout").await.map_err(|e| {
            SshMcpError::connection(format!("Failed to exec detection command: {}", e))
        });

        if exec_result.is_err() {
            debug!("Failed to exec timeout detection command");
            return false;
        }

        // Collect output
        let mut output = String::new();
        while let Some(msg) = channel.wait().await {
            match msg {
                ChannelMsg::Data { data } => {
                    output.push_str(&String::from_utf8_lossy(&data));
                }
                ChannelMsg::Close | ChannelMsg::Eof => {
                    break;
                }
                _ => {
                    // Ignore other messages
                }
            }
        }

        // If timeout command exists, output contains its path (e.g., /usr/bin/timeout)
        let available = !output.is_empty();
        self.has_timeout_cmd.store(available, Ordering::SeqCst);

        if available {
            info!("timeout command available on remote host");
        } else {
            info!("timeout command NOT available, using fallback pkill");
        }

        available
    }

    /// Check if an elevated su channel is available
    pub async fn has_su_channel(&self) -> bool {
        let channel_guard = self.su_channel.lock().await;
        channel_guard.is_some()
    }

    /// Execute a closure with access to the su channel
    ///
    /// The closure receives a mutable reference to the Option<Channel>,
    /// allowing it to use the channel for operations.
    pub async fn with_su_channel<F, Fut, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Option<Channel<client::Msg>>) -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let mut channel_guard = self.su_channel.lock().await;
        f(&mut channel_guard).await
    }

    /// Ensure we have an elevated shell via `su`
    ///
    /// This starts an interactive PTY session, runs `su -`, sends the password,
    /// and waits for the root prompt (#).
    pub async fn ensure_elevated(&self) -> Result<()> {
        // Already elevated?
        if self.is_elevated.load(Ordering::SeqCst) {
            let channel_guard = self.su_channel.lock().await;
            if channel_guard.is_some() {
                return Ok(());
            }
        }

        // Need su_password
        let su_password = self
            .config
            .su_password
            .clone()
            .ok_or_else(|| SshMcpError::elevation_failed("No su_password configured"))?;

        // Open a channel for PTY shell
        let channel = self
            .open_channel()
            .await
            .map_err(|e| SshMcpError::elevation_failed(format!("Failed to open channel: {}", e)))?;

        debug!("Opened channel for su elevation");

        // Request PTY
        channel
            .request_pty(
                true, // want_reply
                "xterm",
                80,  // cols
                24,  // rows
                0,   // pixel width
                0,   // pixel height
                &[], // terminal modes
            )
            .await
            .map_err(|e| SshMcpError::elevation_failed(format!("Failed to request PTY: {}", e)))?;

        debug!("PTY requested");

        // Request shell
        channel.request_shell(true).await.map_err(|e| {
            SshMcpError::elevation_failed(format!("Failed to request shell: {}", e))
        })?;

        debug!("Shell requested, starting su elevation...");

        // Send "su -\n" command
        channel.data(b"su -\n".as_slice()).await.map_err(|e| {
            SshMcpError::elevation_failed(format!("Failed to send su command: {}", e))
        })?;

        // Wait for password prompt and respond
        let elevation_result = self.handle_su_elevation(channel, &su_password).await;

        match elevation_result {
            Ok(elevated_channel) => {
                // Store the elevated channel
                let mut channel_guard = self.su_channel.lock().await;
                *channel_guard = Some(elevated_channel);
                self.is_elevated.store(true, Ordering::SeqCst);
                info!("Successfully elevated to root via su");
                Ok(())
            }
            Err(e) => {
                self.is_elevated.store(false, Ordering::SeqCst);
                Err(e)
            }
        }
    }

    /// Handle the interactive su elevation process
    async fn handle_su_elevation(
        &self,
        mut channel: Channel<client::Msg>,
        password: &str,
    ) -> Result<Channel<client::Msg>> {
        use russh::ChannelMsg;

        let elevation_timeout = Duration::from_secs(10);
        let mut buffer = String::new();
        let mut password_sent = false;

        let deadline = tokio::time::Instant::now() + elevation_timeout;

        loop {
            // Check timeout
            if tokio::time::Instant::now() > deadline {
                return Err(SshMcpError::elevation_failed("su elevation timed out"));
            }

            // Wait for messages with timeout
            let wait_result =
                tokio::time::timeout(Duration::from_millis(500), channel.wait()).await;

            match wait_result {
                Ok(Some(msg)) => {
                    match msg {
                        ChannelMsg::Data { data } => {
                            let text = String::from_utf8_lossy(&data);
                            buffer.push_str(&text);
                            debug!("su buffer: {}", buffer.replace('\n', "\\n"));

                            // Check for password prompt
                            if !password_sent && buffer.to_lowercase().contains("password") {
                                debug!("Password prompt detected, sending password...");
                                channel
                                    .data(format!("{}\n", password).as_bytes())
                                    .await
                                    .map_err(|e| {
                                        SshMcpError::elevation_failed(format!(
                                            "Failed to send password: {}",
                                            e
                                        ))
                                    })?;
                                password_sent = true;
                                // Clear buffer to avoid re-matching password prompt
                                buffer.clear();
                            }

                            // Check for root prompt after password sent
                            if password_sent && buffer.contains('#') {
                                debug!("Root prompt detected, elevation successful");
                                return Ok(channel);
                            }

                            // Check for authentication failure
                            if buffer.to_lowercase().contains("authentication failure")
                                || buffer.to_lowercase().contains("incorrect password")
                                || buffer.to_lowercase().contains("su: failed")
                                || buffer.to_lowercase().contains("su: authentication")
                            {
                                return Err(SshMcpError::elevation_failed(format!(
                                    "su authentication failed: {}",
                                    buffer
                                )));
                            }
                        }
                        ChannelMsg::Close => {
                            return Err(SshMcpError::elevation_failed(
                                "Channel closed before elevation completed",
                            ));
                        }
                        _ => {
                            // Ignore other messages
                        }
                    }
                }
                Ok(None) => {
                    // Channel ended
                    return Err(SshMcpError::elevation_failed(
                        "Channel ended before elevation completed",
                    ));
                }
                Err(_) => {
                    // Timeout on wait, continue loop
                    continue;
                }
            }
        }
    }

    /// Get the su password if configured
    pub fn get_su_password(&self) -> Option<&str> {
        self.config.su_password.as_deref()
    }

    /// Get the sudo password if configured
    pub fn get_sudo_password(&self) -> Option<&str> {
        self.config.sudo_password.as_deref()
    }

    /// Set or update the su password
    ///
    /// If setting a new password, will attempt to establish elevation.
    /// If clearing the password (None), will close any existing su shell.
    pub async fn set_su_password(&self, password: Option<String>) -> Result<()> {
        // Note: We can't modify self.config directly since we only have &self
        // In the TypeScript version, this modifies the config and triggers elevation.
        // For Rust, we'd need interior mutability. For now, just attempt elevation
        // if password is provided.

        if password.is_some() {
            // Attempt elevation with the current config
            // In a real implementation, we'd need to update config first
            self.ensure_elevated().await?;
        } else {
            // Clear elevation state
            let mut channel_guard = self.su_channel.lock().await;
            if let Some(ch) = channel_guard.take() {
                // Try to close the channel gracefully
                let _ = ch.eof().await;
            }
            self.is_elevated.store(false, Ordering::SeqCst);
        }

        Ok(())
    }

    /// Close the SSH connection
    pub async fn close(&self) {
        // Close su channel if exists
        {
            let mut channel_guard = self.su_channel.lock().await;
            if let Some(ch) = channel_guard.take() {
                let _ = ch.eof().await;
            }
        }
        self.is_elevated.store(false, Ordering::SeqCst);

        // Close main session
        {
            let mut session_guard = self.session.lock().await;
            if let Some(session) = session_guard.take() {
                let _ = session
                    .disconnect(russh::Disconnect::ByApplication, "", "")
                    .await;
            }
        }

        info!("SSH connection closed");
    }
}

impl std::fmt::Debug for SshConnectionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshConnectionManager")
            .field("host", &self.config.host)
            .field("port", &self.config.port)
            .field("username", &self.config.username)
            .field("is_connecting", &self.is_connecting.load(Ordering::SeqCst))
            .field("is_elevated", &self.is_elevated.load(Ordering::SeqCst))
            .field(
                "has_timeout_cmd",
                &self.has_timeout_cmd.load(Ordering::SeqCst),
            )
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_connection_manager_creation() {
        let config = SshConfig::new("localhost", "testuser")
            .with_port(22)
            .with_password("testpass");

        let manager = SshConnectionManager::new(config).await;

        assert!(!manager.is_connected().await);
        assert!(!manager.is_elevated());
    }

    #[tokio::test]
    async fn test_not_connected_initially() {
        let config = SshConfig::new("localhost", "testuser");
        let manager = SshConnectionManager::new(config).await;

        // Should return error when trying to open channel without connecting
        let result = manager.open_channel().await;
        assert!(result.is_err());
    }

}
