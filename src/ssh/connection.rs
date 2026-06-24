//! SSH Connection Manager
//!
//! Provides persistent SSH connection handling with automatic reconnection,
//! concurrent access protection, and optional privilege elevation via `su`.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use russh::Channel;
use russh::client::{self, Handle};
use russh::keys::{HashAlg, PrivateKeyWithHashAlg};
use tokio::sync::{Mutex, Notify, OwnedSemaphorePermit, Semaphore};
use tokio::time::{sleep, timeout};
use tracing::{debug, error, info, warn};

use super::config::{HostKeyCheckMode, SshConfig};
use super::handler::{
    KeyCheckOutcome, SshHandler, default_known_hosts_path, remove_known_hosts_entry,
};
use crate::config::CONNECTION_TIMEOUT_SECS;
use crate::error::{Result, SshMcpError};
use russh::ChannelMsg;

/// Default capacity for the channel semaphore (max concurrent commands)
pub const CHANNEL_SEMAPHORE_CAPACITY: usize = 8;
const AUTH_TIMEOUT_SECS: u64 = 20;
const CONNECT_WAIT_TIMEOUT_SECS: u64 = CONNECTION_TIMEOUT_SECS + AUTH_TIMEOUT_SECS;
const MAX_RECONNECT_BACKOFF_MS: u64 = 30_000;
const MIN_HEALTH_PROBE_TTL_MS: u64 = 250;
const MAX_HEALTH_PROBE_TTL_MS: u64 = 5_000;

/// SSH Connection Manager
///
/// Manages a persistent SSH connection with the following features:
/// - Automatic reconnection when connection drops
/// - Concurrent access protection via mutex/atomic flags
/// - Optional `su` elevation for privileged operations
/// - 30-second connection timeout
pub struct SshConnectionManager {
    /// SSH configuration
    /// Made pub(crate) to allow access from command.rs for output limiting
    pub(crate) config: SshConfig,

    /// Active SSH session handle
    session: Arc<Mutex<Option<Handle<SshHandler>>>>,

    /// Flag to prevent concurrent connection attempts
    is_connecting: AtomicBool,

    /// Notification for waiters when connection attempt completes
    connect_notify: Arc<Notify>,

    /// Elevated shell channel (when using su)
    /// Made pub(crate) to allow access from command.rs
    pub(crate) su_channel: Arc<Mutex<Option<Channel<client::Msg>>>>,

    /// Flag indicating whether we're running as root via su
    /// Made pub(crate) to allow access from command.rs for su state reset
    pub(crate) is_elevated: AtomicBool,

    /// Cached availability of the `timeout` command on the remote system
    has_timeout_cmd: AtomicBool,

    /// Semaphore to limit concurrent command execution
    /// Made pub(crate) to allow access from command.rs
    pub(crate) channel_semaphore: Arc<Semaphore>,

    /// Last successful active health probe timestamp
    last_health_probe_ok_at: Arc<Mutex<Option<tokio::time::Instant>>>,

    /// Lock to avoid concurrent active health probes
    health_probe_lock: Arc<Mutex<()>>,
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
            connect_notify: Arc::new(Notify::new()),
            su_channel: Arc::new(Mutex::new(None)),
            is_elevated: AtomicBool::new(false),
            has_timeout_cmd: AtomicBool::new(false),
            channel_semaphore: Arc::new(Semaphore::new(CHANNEL_SEMAPHORE_CAPACITY)),
            last_health_probe_ok_at: Arc::new(Mutex::new(None)),
            health_probe_lock: Arc::new(Mutex::new(())),
        }
    }

    pub(crate) async fn acquire_command_slot_raw(
        &self,
    ) -> std::result::Result<OwnedSemaphorePermit, tokio::sync::AcquireError> {
        self.channel_semaphore.clone().acquire_owned().await
    }

    pub(crate) async fn acquire_command_slot(&self) -> Result<OwnedSemaphorePermit> {
        self.acquire_command_slot_raw()
            .await
            .map_err(|e| SshMcpError::connection(format!("Failed to acquire command slot: {e}")))
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
            // Wait for the other connection attempt to complete using Notify
            let wait_result = timeout(
                Duration::from_secs(CONNECT_WAIT_TIMEOUT_SECS),
                self.connect_notify.notified(),
            )
            .await;
            if wait_result.is_err() {
                warn!(
                    "Timed out waiting for in-flight connection attempt after {}s",
                    CONNECT_WAIT_TIMEOUT_SECS
                );
                return Err(SshMcpError::connection(format!(
                    "Timed out waiting for in-flight connection attempt after {}s",
                    CONNECT_WAIT_TIMEOUT_SECS
                )));
            }
            return if self.is_connected().await {
                Ok(())
            } else {
                Err(SshMcpError::connection("Connection failed by another task"))
            };
        }

        // Perform connection with timeout
        let result = self.do_connect().await;

        // Reset connecting flag and notify all waiters
        self.is_connecting.store(false, Ordering::SeqCst);
        self.connect_notify.notify_waiters();

        result
    }

    /// Internal connection logic
    ///
    /// On a changed host key in `accept-new` mode, removes the stale
    /// known_hosts entry and retries once.  All other failures are
    /// returned immediately.
    async fn do_connect(&self) -> Result<()> {
        info!(
            "Connecting to SSH server {}:{}...",
            self.config.host, self.config.port
        );

        let connection_timeout = Duration::from_secs(CONNECTION_TIMEOUT_SECS);

        let ssh_config = Arc::new(client::Config {
            keepalive_interval: Some(Duration::from_secs(self.config.keepalive_interval)),
            keepalive_max: self.config.keepalive_max as usize,
            ..Default::default()
        });

        let addr = format!("{}:{}", self.config.host, self.config.port);

        // First attempt — record key check outcome for recovery decisions.
        let key_outcome = Arc::new(std::sync::Mutex::new(None::<KeyCheckOutcome>));
        let handler = SshHandler::new(
            self.config.host.clone(),
            self.config.port,
            self.config.host_key_checking,
            self.config.known_hosts.clone(),
        )
        .with_key_check_outcome(key_outcome.clone());

        match self
            .attempt_connect(&ssh_config, &addr, handler, connection_timeout)
            .await
        {
            Ok(session) => self.finish_connect(session).await,
            Err(first_err) => {
                // If the failure was a changed host key in accept-new mode,
                // remove the stale entry and retry once.
                let outcome = key_outcome.lock().unwrap().take();
                if matches!(outcome, Some(KeyCheckOutcome::KeyChanged))
                    && self.config.host_key_checking == HostKeyCheckMode::AcceptNew
                {
                    let Some(path) = self.resolve_known_hosts_path() else {
                        error!(
                            host = %self.config.host,
                            port = self.config.port,
                            "Host key changed but cannot resolve known_hosts path for recovery"
                        );
                        return Err(first_err);
                    };

                    warn!(
                        host = %self.config.host,
                        port = self.config.port,
                        path = %path.display(),
                        "Host key changed in accept-new mode; \
                         removing stale known_hosts entry and retrying once"
                    );
                    remove_known_hosts_entry(&self.config.host, self.config.port, &path)
                        .map_err(|e| {
                            SshMcpError::connection(format!(
                                "Failed to remove stale known_hosts entry: {e}"
                            ))
                        })?;

                    // Single retry — fresh handler, no outcome recording.
                    let retry_handler = SshHandler::new(
                        self.config.host.clone(),
                        self.config.port,
                        self.config.host_key_checking,
                        self.config.known_hosts.clone(),
                    );
                    match self
                        .attempt_connect(&ssh_config, &addr, retry_handler, connection_timeout)
                        .await
                    {
                        Ok(session) => {
                            info!(
                                host = %self.config.host,
                                port = self.config.port,
                                "SSH reconnection succeeded after host key rotation"
                            );
                            self.finish_connect(session).await
                        }
                        Err(retry_err) => {
                            error!(
                                error = ?retry_err,
                                "SSH connection failed on retry after key rotation"
                            );
                            Err(retry_err)
                        }
                    }
                } else {
                    error!(error = ?first_err, "SSH connection failed");
                    Err(first_err)
                }
            }
        }
    }

    /// Attempt a single SSH connection (no retry logic).
    async fn attempt_connect(
        &self,
        ssh_config: &Arc<client::Config>,
        addr: &str,
        handler: SshHandler,
        connection_timeout: Duration,
    ) -> Result<Handle<SshHandler>> {
        timeout(
            connection_timeout,
            client::connect(ssh_config.clone(), addr, handler),
        )
        .await
        .map_err(|_| {
            error!("SSH connection timeout after {}s", CONNECTION_TIMEOUT_SECS);
            SshMcpError::connection(format!(
                "Connection timeout after {}s",
                CONNECTION_TIMEOUT_SECS
            ))
        })?
        .map_err(|e| {
            SshMcpError::connection(e.to_string())
        })
    }

    /// Authenticate, store the session, and optionally elevate.
    async fn finish_connect(&self, mut session: Handle<SshHandler>) -> Result<()> {
        // Authenticate
        self.authenticate(&mut session).await?;

        // Store session
        {
            let mut session_guard = self.session.lock().await;
            *session_guard = Some(session);
        }
        {
            let mut probe_guard = self.last_health_probe_ok_at.lock().await;
            *probe_guard = None;
        }

        info!(
            "Successfully connected to {}@{}:{}",
            self.config.username, self.config.host, self.config.port
        );

        // If su_password is configured, attempt elevation
        if self.config.su_password.is_some() {
            debug!("su_password configured, attempting elevation...");
            if let Err(e) = self.ensure_elevated().await {
                warn!(error = ?e, "Failed to elevate to root. Commands will run as normal user.");
            }
        }

        Ok(())
    }

    /// Resolve the known_hosts file path — explicit config or default.
    fn resolve_known_hosts_path(&self) -> Option<PathBuf> {
        self.config
            .known_hosts
            .clone()
            .or_else(default_known_hosts_path)
    }

    /// Authenticate with the SSH server
    async fn authenticate(&self, session: &mut Handle<SshHandler>) -> Result<()> {
        // Try password authentication first
        if let Some(ref password) = self.config.password {
            debug!(
                "Attempting password authentication for user '{}'",
                self.config.username
            );
            let auth_result = timeout(
                Duration::from_secs(AUTH_TIMEOUT_SECS),
                session.authenticate_password(&self.config.username, password),
            )
            .await
            .map_err(|_| {
                SshMcpError::auth(format!(
                    "Authentication timed out after {}s",
                    AUTH_TIMEOUT_SECS
                ))
            })?
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
            let key = Arc::new(
                russh::keys::PrivateKey::from_openssh(key_content.as_bytes()).map_err(|e| {
                    SshMcpError::SshKey(format!("Failed to parse private key: {}", e))
                })?,
            );

            // For RSA, try modern rsa-sha2-256/512 first, then legacy ssh-rsa (SHA-1) as fallback.
            // For non-RSA keys, the hash algorithm is ignored by russh.
            let hash_attempts: &[Option<HashAlg>] = if key.algorithm().is_rsa() {
                &[Some(HashAlg::Sha256), Some(HashAlg::Sha512), None]
            } else {
                &[None]
            };

            for hash_alg in hash_attempts {
                debug!(
                    alg = %key.algorithm(),
                    ?hash_alg,
                    "Attempting publickey authentication"
                );

                let key_with_alg = PrivateKeyWithHashAlg::new(Arc::clone(&key), *hash_alg);

                let auth_result = timeout(
                    Duration::from_secs(AUTH_TIMEOUT_SECS),
                    session.authenticate_publickey(&self.config.username, key_with_alg),
                )
                .await
                .map_err(|_| {
                    SshMcpError::auth(format!(
                        "Authentication timed out after {}s",
                        AUTH_TIMEOUT_SECS
                    ))
                })?
                .map_err(|e| SshMcpError::auth(e.to_string()))?;

                if auth_result.success() {
                    info!("Key authentication successful");
                    return Ok(());
                }
            }

            return Err(SshMcpError::auth("Key authentication rejected"));
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
            return self
                .connect_with_retry("no active session found during ensure_connected")
                .await;
        }

        if self.is_health_probe_fresh().await {
            return Ok(());
        }

        let _probe_guard = self.health_probe_lock.lock().await;
        if self.is_health_probe_fresh().await {
            return Ok(());
        }

        if let Err(probe_error) = self.run_health_probe().await {
            warn!(
                error = ?probe_error,
                "SSH health probe failed, invalidating session before reconnect"
            );
            self.invalidate_session("health probe failed").await;
            self.connect_with_retry("health probe failed during ensure_connected")
                .await?;
        } else {
            self.mark_health_probe_ok().await;
        }

        Ok(())
    }

    fn health_probe_ttl(&self) -> Duration {
        let ttl_ms = self
            .config
            .health_probe_timeout_ms
            .saturating_mul(2)
            .clamp(MIN_HEALTH_PROBE_TTL_MS, MAX_HEALTH_PROBE_TTL_MS);
        Duration::from_millis(ttl_ms)
    }

    async fn is_health_probe_fresh(&self) -> bool {
        let guard = self.last_health_probe_ok_at.lock().await;
        if let Some(last_ok_at) = guard.as_ref() {
            return last_ok_at.elapsed() < self.health_probe_ttl();
        }

        false
    }

    async fn mark_health_probe_ok(&self) {
        let mut guard = self.last_health_probe_ok_at.lock().await;
        *guard = Some(tokio::time::Instant::now());
    }

    async fn run_health_probe(&self) -> Result<()> {
        let ping_result = {
            let session_guard = self.session.lock().await;
            let session = session_guard
                .as_ref()
                .ok_or_else(|| SshMcpError::connection("SSH connection not established"))?;

            timeout(
                Duration::from_millis(self.config.health_probe_timeout_ms),
                session.send_ping(),
            )
            .await
        };

        match ping_result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(SshMcpError::connection(format!(
                "SSH health probe ping failed: {e}"
            ))),
            Err(_) => Err(SshMcpError::connection(format!(
                "SSH health probe timed out after {}ms",
                self.config.health_probe_timeout_ms
            ))),
        }
    }

    async fn connect_with_retry(&self, reason: &str) -> Result<()> {
        let max_attempts = self.config.reconnect_retries.saturating_add(1);
        let mut attempt: u64 = 1;
        let mut last_error: Option<SshMcpError> = None;

        while attempt <= max_attempts {
            match self.connect().await {
                Ok(()) => {
                    if attempt > 1 {
                        info!(
                            attempts = attempt,
                            reason = reason,
                            "SSH reconnect succeeded"
                        );
                    }
                    return Ok(());
                }
                Err(err) => {
                    let backoff_ms = self.backoff_for_attempt(attempt);
                    warn!(
                        attempt = attempt,
                        max_attempts = max_attempts,
                        backoff_ms = backoff_ms,
                        reason = reason,
                        error = ?err,
                        "SSH reconnect attempt failed"
                    );
                    last_error = Some(err);

                    if attempt < max_attempts && backoff_ms > 0 {
                        sleep(Duration::from_millis(backoff_ms)).await;
                    }
                }
            }

            attempt = attempt.saturating_add(1);
        }

        if let Some(err) = last_error {
            return Err(err);
        }

        Err(SshMcpError::connection(
            "Reconnect retry loop ended without connection result",
        ))
    }

    fn backoff_for_attempt(&self, attempt: u64) -> u64 {
        let exponent = attempt.saturating_sub(1).min(63) as u32;
        let factor = 1_u64 << exponent;
        self.config
            .reconnect_backoff_ms
            .saturating_mul(factor)
            .min(MAX_RECONNECT_BACKOFF_MS)
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

    /// Lazily check and return whether to use the remote timeout wrapper
    ///
    /// This performs a one-time remote detection on first need and then returns
    /// the cached decision for the lifetime of the connection.
    pub(crate) async fn determine_timeout_wrapper_usage(&self) -> bool {
        if self.use_timeout_wrapper() {
            return true;
        }

        let _ = self.check_timeout_availability().await;
        self.use_timeout_wrapper()
    }

    /// Detect whether the `timeout` command is available on the remote system
    ///
    /// This performs a one-time detection check by running
    /// `sh -c 'command -v timeout'`
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
                debug!(error = ?e, "Failed to open channel for timeout detection");
                return false;
            }
        };

        // Run detection command
        let exec_result = channel
            .exec(true, "sh -c 'command -v timeout'")
            .await
            .map_err(|e| {
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
                            debug!(su_buffer_len = buffer.len(), "su buffer received");

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

        {
            let mut probe_guard = self.last_health_probe_ok_at.lock().await;
            *probe_guard = None;
        }

        info!("SSH connection closed");
    }

    /// Invalidate the current session and clear elevation state
    ///
    /// This clears the session handle, su_channel, and resets elevation state.
    /// Used when a connection is detected as broken and needs reconnection.
    pub async fn invalidate_session(&self, reason: &str) {
        warn!(reason = ?reason, "Invalidating SSH session");

        // Take channel out of mutex before awaiting to avoid deadlock
        let channel = {
            let mut channel_guard = self.su_channel.lock().await;
            channel_guard.take()
        };

        // Drop lock before awaiting EOF
        if let Some(ch) = channel {
            let _ = ch.eof().await;
        }
        self.is_elevated.store(false, Ordering::SeqCst);

        // Attempt best-effort graceful disconnect with short timeout.
        let session = {
            let mut session_guard = self.session.lock().await;
            session_guard.take()
        };

        if let Some(session) = session {
            let _ = tokio::time::timeout(
                Duration::from_millis(500),
                session.disconnect(russh::Disconnect::ByApplication, "", ""),
            )
            .await;
        }

        {
            let mut probe_guard = self.last_health_probe_ok_at.lock().await;
            *probe_guard = None;
        }

        debug!(reason = ?reason, "Session invalidated");
    }

    /// Force a reconnection by invalidating the current session and reconnecting
    ///
    /// This is used when the connection is known to be broken and a fresh
    /// connection is required. It clears all session state and performs
    /// a new connection attempt.
    pub async fn reconnect(&self) -> Result<()> {
        self.invalidate_session("explicit reconnect requested")
            .await;
        self.connect_with_retry("explicit reconnect requested")
            .await
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
