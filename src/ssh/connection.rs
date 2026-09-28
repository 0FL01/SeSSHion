//! SSH Connection Manager
//!
//! Provides persistent SSH connection handling with automatic reconnection,
//! concurrent access protection, and optional privilege elevation via `su`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use russh::Channel;
use russh::client::{self, Handle};
use russh::keys::{HashAlg, PrivateKeyWithHashAlg};
use tokio::sync::{Mutex, Notify, OwnedSemaphorePermit, Semaphore};
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::config::SshConfig;
use super::environment::HostEnvironment;
use super::handler::SshHandler;
use crate::config::CONNECTION_TIMEOUT_SECS;
use crate::error::{Result, SshMcpError};
use russh::ChannelMsg;

/// Default capacity for the channel semaphore (max concurrent commands)
pub const CHANNEL_SEMAPHORE_CAPACITY: usize = 8;
const AUTH_TIMEOUT_SECS: u64 = 20;
const MAX_RECONNECT_BACKOFF_MS: u64 = 30_000;
const MIN_HEALTH_PROBE_TTL_MS: u64 = 250;
const MAX_HEALTH_PROBE_TTL_MS: u64 = 5_000;

struct ConnectAttemptGuard<'a> {
    is_connecting: &'a AtomicBool,
    connect_notify: &'a Notify,
}

pub(super) struct ActiveRoute {
    pub(super) target: Handle<SshHandler>,
    jump: Option<Handle<SshHandler>>,
    pub(super) generation: u64,
    pub(super) environment: Option<HostEnvironment>,
    auto_elevation_attempted: bool,
}

impl Drop for ConnectAttemptGuard<'_> {
    fn drop(&mut self) {
        self.is_connecting.store(false, Ordering::SeqCst);
        self.connect_notify.notify_waiters();
    }
}

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

    /// Active target session and its optional jump session.
    pub(super) session: Arc<Mutex<Option<ActiveRoute>>>,

    next_generation: AtomicU64,
    pub(super) environment_gate: Mutex<()>,
    pub(super) shutdown_token: CancellationToken,
    auto_elevation_lock: Mutex<()>,
    elevation_lock: Mutex<()>,

    /// Flag to prevent concurrent connection attempts
    is_connecting: AtomicBool,

    /// Terminal gate preventing new SSH work after shutdown begins.
    shutting_down: AtomicBool,

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
            next_generation: AtomicU64::new(1),
            environment_gate: Mutex::new(()),
            shutdown_token: CancellationToken::new(),
            auto_elevation_lock: Mutex::new(()),
            elevation_lock: Mutex::new(()),
            is_connecting: AtomicBool::new(false),
            shutting_down: AtomicBool::new(false),
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

    pub(crate) fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }

    fn ensure_not_shutting_down(&self) -> Result<()> {
        if self.is_shutting_down() {
            Err(SshMcpError::connection(
                "SSH connection manager is shutting down",
            ))
        } else {
            Ok(())
        }
    }

    /// Establish SSH connection
    ///
    /// If already connected, returns immediately. If another task is currently
    /// connecting, waits for that connection attempt to complete.
    pub async fn connect(&self) -> Result<()> {
        self.connect_transport_only().await?;
        self.initialize_auto_elevation().await;
        Ok(())
    }

    /// Establish the shared route without initiating privilege elevation.
    async fn connect_transport_only(&self) -> Result<()> {
        self.ensure_not_shutting_down()?;

        // Check if already connected
        if self.is_connected().await {
            debug!("Already connected to SSH server");
            return Ok(());
        }

        // Register before inspecting the flag so completion cannot race the waiter.
        let notified = self.connect_notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        // Prevent concurrent connection attempts
        if self
            .is_connecting
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            debug!("Another connection attempt in progress, waiting...");
            // Every establishment phase has its own timeout. Waiting for the
            // guarded owner avoids a shorter guessed timeout for multi-hop routes.
            notified.await;
            return if self.is_shutting_down() {
                self.ensure_not_shutting_down()
            } else if self.is_connected().await {
                Ok(())
            } else {
                Err(SshMcpError::connection("Connection failed by another task"))
            };
        }

        // Cancellation must release the owner flag and wake waiters.
        let _attempt_guard = ConnectAttemptGuard {
            is_connecting: &self.is_connecting,
            connect_notify: self.connect_notify.as_ref(),
        };

        self.do_connect().await
    }

    /// Internal connection logic.
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

        let (target, jump) = if let Some(jump_config) = &self.config.jump {
            info!(
                "Connecting through jump host {}@{}:{}",
                jump_config.username, jump_config.host, jump_config.port
            );
            let mut jump = self
                .connect_tcp_endpoint(
                    &ssh_config,
                    &jump_config.host,
                    jump_config.port,
                    "jump connection",
                    connection_timeout,
                )
                .await?;
            if let Err(error) = Self::authenticate_session(
                &mut jump,
                &jump_config.username,
                jump_config.password.as_deref(),
                jump_config.private_key.as_deref(),
                "jump authentication",
            )
            .await
            {
                Self::disconnect_handle(jump).await;
                return Err(error);
            }

            let tunnel = match timeout(
                connection_timeout,
                jump.channel_open_direct_tcpip(
                    self.config.host.clone(),
                    u32::from(self.config.port),
                    "127.0.0.1",
                    0,
                ),
            )
            .await
            {
                Ok(Ok(channel)) => channel,
                Ok(Err(error)) => {
                    Self::disconnect_handle(jump).await;
                    return Err(SshMcpError::connection(format!(
                        "jump forwarding failed: {error}"
                    )));
                }
                Err(_) => {
                    Self::disconnect_handle(jump).await;
                    return Err(SshMcpError::connection(format!(
                        "jump forwarding timed out after {}s",
                        CONNECTION_TIMEOUT_SECS
                    )));
                }
            };

            let target_handler = self.target_handler();
            let target = match timeout(
                connection_timeout,
                client::connect_stream(ssh_config.clone(), tunnel.into_stream(), target_handler),
            )
            .await
            {
                Ok(Ok(session)) => session,
                Ok(Err(error)) => {
                    Self::disconnect_handle(jump).await;
                    return Err(SshMcpError::connection(format!(
                        "target connection through jump failed: {error}"
                    )));
                }
                Err(_) => {
                    Self::disconnect_handle(jump).await;
                    return Err(SshMcpError::connection(format!(
                        "target connection through jump timed out after {}s",
                        CONNECTION_TIMEOUT_SECS
                    )));
                }
            };
            (target, Some(jump))
        } else {
            let target = self
                .connect_tcp_endpoint(
                    &ssh_config,
                    &self.config.host,
                    self.config.port,
                    "target connection",
                    connection_timeout,
                )
                .await?;
            (target, None)
        };

        self.finish_connect(target, jump).await
    }

    fn target_handler(&self) -> SshHandler {
        SshHandler::new(
            self.config.host.clone(),
            self.config.port,
            self.config.host_key_checking,
            self.config.known_hosts.clone(),
        )
    }

    async fn connect_tcp_endpoint(
        &self,
        ssh_config: &Arc<client::Config>,
        host: &str,
        port: u16,
        stage: &str,
        connection_timeout: Duration,
    ) -> Result<Handle<SshHandler>> {
        let addr = format!("{host}:{port}");
        let handler = SshHandler::new(
            host.to_string(),
            port,
            self.config.host_key_checking,
            self.config.known_hosts.clone(),
        );
        timeout(
            connection_timeout,
            client::connect(ssh_config.clone(), &addr, handler),
        )
        .await
        .map_err(|_| {
            SshMcpError::connection(format!(
                "{stage} timed out after {}s",
                CONNECTION_TIMEOUT_SECS
            ))
        })?
        .map_err(|error| SshMcpError::connection(format!("{stage} failed: {error}")))
    }

    /// Authenticate and publish transport; legacy callers initialize elevation.
    async fn finish_connect(
        &self,
        mut target: Handle<SshHandler>,
        jump: Option<Handle<SshHandler>>,
    ) -> Result<()> {
        if let Err(error) = Self::authenticate_session(
            &mut target,
            &self.config.username,
            self.config.password.as_deref(),
            self.config.private_key.as_deref(),
            "target authentication",
        )
        .await
        {
            Self::disconnect_handle(target).await;
            if let Some(jump) = jump {
                Self::disconnect_handle(jump).await;
            }
            return Err(error);
        }

        // Do not publish a connection that completed after shutdown began.
        let mut route = Some(ActiveRoute {
            target,
            jump,
            generation: self.next_generation.fetch_add(1, Ordering::SeqCst),
            environment: None,
            auto_elevation_attempted: false,
        });
        {
            let mut session_guard = self.session.lock().await;
            if !self.is_shutting_down() {
                *session_guard = route.take();
            }
        }
        if let Some(route) = route {
            Self::disconnect_route(route).await;
            return self.ensure_not_shutting_down();
        }
        {
            let mut probe_guard = self.last_health_probe_ok_at.lock().await;
            *probe_guard = None;
        }

        info!(
            "Successfully connected to {}@{}:{}",
            self.config.username, self.config.host, self.config.port
        );

        Ok(())
    }

    /// Preserve best-effort automatic su even when a rootless probe connected first.
    async fn initialize_auto_elevation(&self) {
        if self.config.su_password.is_none() {
            return;
        }
        let _guard = self.auto_elevation_lock.lock().await;
        let generation = {
            let mut session = self.session.lock().await;
            let Some(route) = session.as_mut() else {
                return;
            };
            if route.auto_elevation_attempted || self.is_shutting_down() {
                return;
            }
            route.auto_elevation_attempted = true;
            route.generation
        };
        if let Err(error) = self.ensure_elevated_for_generation(Some(generation)).await {
            warn!(
                ?error,
                "Failed to elevate to root. Commands will run as normal user."
            );
        }
    }

    /// Authenticate with the SSH server
    async fn authenticate_session(
        session: &mut Handle<SshHandler>,
        username: &str,
        password: Option<&str>,
        private_key: Option<&str>,
        stage: &str,
    ) -> Result<()> {
        // Try password authentication first
        if let Some(password) = password {
            debug!("Attempting {stage} with password for user '{username}'");
            let auth_result = timeout(
                Duration::from_secs(AUTH_TIMEOUT_SECS),
                session.authenticate_password(username, password),
            )
            .await
            .map_err(|_| {
                SshMcpError::auth(format!("{stage} timed out after {}s", AUTH_TIMEOUT_SECS))
            })?
            .map_err(|error| SshMcpError::auth(format!("{stage} failed: {error}")))?;

            if auth_result.success() {
                info!("{stage} with password successful");
                return Ok(());
            } else {
                return Err(SshMcpError::auth(format!("{stage} rejected password")));
            }
        }

        // Try key authentication
        if let Some(key_content) = private_key {
            debug!("Attempting {stage} with key for user '{username}'");

            // Parse the private key using russh::keys
            let key = Arc::new(
                russh::keys::PrivateKey::from_openssh(key_content.as_bytes())
                    .map_err(|e| SshMcpError::SshKey(format!("{stage} key parsing failed: {e}")))?,
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
                    session.authenticate_publickey(username, key_with_alg),
                )
                .await
                .map_err(|_| {
                    SshMcpError::auth(format!("{stage} timed out after {}s", AUTH_TIMEOUT_SECS))
                })?
                .map_err(|error| SshMcpError::auth(format!("{stage} failed: {error}")))?;

                if auth_result.success() {
                    info!("{stage} with key successful");
                    return Ok(());
                }
            }

            return Err(SshMcpError::auth(format!("{stage} rejected key")));
        }

        Err(SshMcpError::auth(format!(
            "{stage} has no authentication method"
        )))
    }

    async fn disconnect_handle(session: Handle<SshHandler>) {
        let _ = session
            .disconnect(russh::Disconnect::ByApplication, "", "")
            .await;
        let _ = timeout(Duration::from_millis(500), session).await;
    }

    async fn disconnect_route(route: ActiveRoute) {
        Self::disconnect_handle(route.target).await;
        if let Some(jump) = route.jump {
            Self::disconnect_handle(jump).await;
        }
    }

    /// Check if the connection is active
    pub async fn is_connected(&self) -> bool {
        let session_guard = self.session.lock().await;
        session_guard.is_some()
    }

    /// Ensure connection is established, reconnecting if necessary
    pub async fn ensure_connected(&self) -> Result<()> {
        self.ensure_connected_transport_only().await?;
        self.initialize_auto_elevation().await;
        Ok(())
    }

    pub(super) async fn ensure_connected_transport_only(&self) -> Result<()> {
        self.ensure_not_shutting_down()?;

        let transport_closed = self
            .session
            .lock()
            .await
            .as_ref()
            .is_some_and(|route| route.target.is_closed());
        if transport_closed {
            self.invalidate_session("SSH transport closed").await;
        }

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
            let route = session_guard
                .as_ref()
                .ok_or_else(|| SshMcpError::connection("SSH connection not established"))?;

            timeout(
                Duration::from_millis(self.config.health_probe_timeout_ms),
                route.target.send_ping(),
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
            match self.connect_transport_only().await {
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
        self.ensure_not_shutting_down()?;
        let session_guard = self.session.lock().await;
        match session_guard.as_ref() {
            Some(route) => Ok(f(&route.target)),
            None => Err(SshMcpError::connection("SSH connection not established")),
        }
    }

    /// Open a new session channel
    pub async fn open_channel(&self) -> Result<Channel<client::Msg>> {
        self.open_channel_with_generation()
            .await
            .map(|(_, channel)| channel)
    }

    pub(super) async fn open_channel_with_generation(&self) -> Result<(u64, Channel<client::Msg>)> {
        self.ensure_not_shutting_down()?;
        let session_guard = self.session.lock().await;
        let route = session_guard
            .as_ref()
            .ok_or_else(|| SshMcpError::connection("SSH connection not established"))?;

        let channel = route
            .target
            .channel_open_session()
            .await
            .map_err(|e| SshMcpError::connection(format!("Failed to open channel: {}", e)))?;

        Ok((route.generation, channel))
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
        self.ensure_not_shutting_down()?;
        let mut channel_guard = self.su_channel.lock().await;
        f(&mut channel_guard).await
    }

    /// Ensure we have an elevated shell via `su`
    ///
    /// This starts an interactive PTY session, runs `su -`, sends the password,
    /// and waits for the root prompt (#).
    pub async fn ensure_elevated(&self) -> Result<()> {
        self.ensure_elevated_for_generation(None).await
    }

    async fn ensure_elevated_for_generation(&self, expected: Option<u64>) -> Result<()> {
        let _elevation_guard = self.elevation_lock.lock().await;
        self.ensure_not_shutting_down()?;

        if let Some(expected) = expected {
            let session = self.session.lock().await;
            if session.as_ref().map(|route| route.generation) != Some(expected) {
                return Err(SshMcpError::elevation_failed(
                    "SSH route changed before elevation",
                ));
            }
        }

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
        let (generation, channel) = self
            .open_channel_with_generation()
            .await
            .map_err(|e| SshMcpError::elevation_failed(format!("Failed to open channel: {}", e)))?;

        if expected.is_some_and(|expected| expected != generation) {
            let _ = timeout(Duration::from_millis(100), channel.close()).await;
            return Err(SshMcpError::elevation_failed(
                "SSH route changed before elevation",
            ));
        }

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
                let session_guard = self.session.lock().await;
                if self.is_shutting_down()
                    || session_guard.as_ref().map(|route| route.generation) != Some(generation)
                {
                    drop(session_guard);
                    drop(channel_guard);
                    let _ = timeout(Duration::from_millis(100), elevated_channel.close()).await;
                    return Err(SshMcpError::elevation_failed(
                        "SSH route changed during elevation",
                    ));
                }
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
        self.shutting_down.store(true, Ordering::SeqCst);
        self.shutdown_token.cancel();

        // Remove the route/cache before asynchronous teardown. Lock order: su -> route.
        let (su_channel, route) = {
            let mut channel_guard = self.su_channel.lock().await;
            let mut session_guard = self.session.lock().await;
            self.is_elevated.store(false, Ordering::SeqCst);
            (channel_guard.take(), session_guard.take())
        };
        if let Some(ch) = su_channel {
            let _ = ch.eof().await;
        }
        // Close target first, then the jump session that carries it.
        if let Some(route) = route {
            Self::disconnect_route(route).await;
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
        let (channel, route) = {
            let mut channel_guard = self.su_channel.lock().await;
            let mut session_guard = self.session.lock().await;
            self.is_elevated.store(false, Ordering::SeqCst);
            (channel_guard.take(), session_guard.take())
        };

        // Drop lock before awaiting EOF
        if let Some(ch) = channel {
            let _ = ch.eof().await;
        }
        // Attempt best-effort graceful disconnect with short timeout.

        if let Some(route) = route {
            Self::disconnect_route(route).await;
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
        self.ensure_not_shutting_down()?;
        self.invalidate_session("explicit reconnect requested")
            .await;
        self.connect_with_retry("explicit reconnect requested")
            .await?;
        self.initialize_auto_elevation().await;
        Ok(())
    }
}

impl std::fmt::Debug for SshConnectionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshConnectionManager")
            .field("host", &self.config.host)
            .field("port", &self.config.port)
            .field("username", &self.config.username)
            .field("is_connecting", &self.is_connecting.load(Ordering::SeqCst))
            .field("shutting_down", &self.shutting_down.load(Ordering::SeqCst))
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

    #[tokio::test]
    async fn test_close_prevents_new_ssh_work() {
        let config = SshConfig::new("127.0.0.1", "testuser").with_port(9);
        let manager = SshConnectionManager::new(config).await;

        manager.close().await;
        manager.close().await;

        assert!(manager.is_shutting_down());
        assert!(manager.connect().await.is_err());
        assert!(manager.ensure_connected().await.is_err());
        assert!(manager.open_channel().await.is_err());
        assert!(manager.reconnect().await.is_err());
    }

    #[tokio::test]
    async fn test_cancelled_connect_releases_owner_flag() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind test listener");
        let port = listener.local_addr().expect("test listener address").port();
        let accept_task = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.expect("accept test connection");
            std::future::pending::<()>().await;
        });
        let config = SshConfig::new("127.0.0.1", "testuser")
            .with_port(port)
            .with_password("testpass");
        let manager = SshConnectionManager::new(config).await;

        let result = timeout(Duration::from_millis(100), manager.connect()).await;
        assert!(result.is_err(), "silent peer should keep connect in flight");
        assert!(
            !manager.is_connecting.load(Ordering::SeqCst),
            "cancelling connect must release the owner flag"
        );

        accept_task.abort();
    }
}
