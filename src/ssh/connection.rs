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
    auto_elevation_attempted: bool,
}

impl Drop for ConnectAttemptGuard<'_> {
    fn drop(&mut self) {
        self.is_connecting.store(false, Ordering::SeqCst);
        self.connect_notify.notify_waiters();
    }
}

impl ActiveRoute {
    pub(super) fn is_closed(&self) -> bool {
        self.target.is_closed() || self.jump.as_ref().is_some_and(Handle::is_closed)
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

    // Protected by su_channel's mutex, including while its channel is taken.
    pub(super) su_generation: AtomicU64,

    /// Flag indicating whether we're running as root via su
    /// Made pub(crate) to allow access from command.rs for su state reset
    pub(crate) is_elevated: AtomicBool,

    /// Cached availability of the `timeout` command on the remote system
    has_timeout_cmd: AtomicBool,

    /// Semaphore to limit concurrent command execution
    /// Made pub(crate) to allow access from command.rs
    pub(crate) channel_semaphore: Arc<Semaphore>,

    /// Last successful active health probe timestamp
    last_health_probe_ok_at: Arc<Mutex<Option<(u64, tokio::time::Instant)>>>,

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
            shutdown_token: CancellationToken::new(),
            auto_elevation_lock: Mutex::new(()),
            elevation_lock: Mutex::new(()),
            is_connecting: AtomicBool::new(false),
            shutting_down: AtomicBool::new(false),
            connect_notify: Arc::new(Notify::new()),
            su_channel: Arc::new(Mutex::new(None)),
            su_generation: AtomicU64::new(0),
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
        self.bounded_transport(
            Duration::from_secs(CONNECTION_TIMEOUT_SECS),
            self.connect_transport_only(),
        )
        .await?;
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
            // The caller's total transport budget also bounds this owner wait.
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

        // The first check may precede another owner's publication. Never replace
        // the route that owner established while this task was waiting to run.
        self.ensure_not_shutting_down()?;
        if self.is_connected().await {
            return Ok(());
        }
        let stale_generation = self
            .session
            .lock()
            .await
            .as_ref()
            .map(|route| route.generation);
        if let Some(generation) = stale_generation {
            self.invalidate_session_for_generation(generation, "closed route before connect")
                .await;
        }
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
            auto_elevation_attempted: false,
        });
        {
            let mut session_guard = self.session.lock().await;
            if !self.is_shutting_down() {
                let mut probe_guard = self.last_health_probe_ok_at.lock().await;
                *session_guard = route.take();
                *probe_guard = None;
            }
        }
        if let Some(route) = route {
            Self::disconnect_route(route).await;
            return self.ensure_not_shutting_down();
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
                russh::keys::PrivateKey::from_openssh(key_content.trim_end().as_bytes())
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
        // Bound both enqueueing the disconnect and waiting for its completion.
        let _ = timeout(Duration::from_millis(500), async {
            let _ = session
                .disconnect(russh::Disconnect::ByApplication, "", "")
                .await;
            let _ = session.await;
        })
        .await;
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
        session_guard
            .as_ref()
            .is_some_and(|route| !route.is_closed())
    }

    /// Ensure connection is established, reconnecting if necessary
    pub async fn ensure_connected(&self) -> Result<()> {
        self.ensure_connected_transport_only().await?;
        self.initialize_auto_elevation().await;
        Ok(())
    }

    /// Acquire a healthy authenticated route without initiating su/sudo.
    /// The total budget includes locks, owner waits, retries and teardown.
    pub async fn ensure_connected_transport_only(&self) -> Result<()> {
        self.bounded_transport(
            Duration::from_secs(CONNECTION_TIMEOUT_SECS),
            self.ensure_transport_inner(),
        )
        .await
    }

    async fn bounded_transport<T>(
        &self,
        budget: Duration,
        operation: impl std::future::Future<Output = Result<T>>,
    ) -> Result<T> {
        self.ensure_not_shutting_down()?;
        tokio::select! {
            biased;
            _ = self.shutdown_token.cancelled() => Err(SshMcpError::connection("SSH connection manager is shutting down")),
            result = timeout(budget, operation) => result.map_err(|_| {
                SshMcpError::connection(format!("SSH transport acquisition timed out after {}ms", budget.as_millis()))
            })?,
        }
    }

    async fn ensure_transport_inner(&self) -> Result<()> {
        loop {
            self.ensure_not_shutting_down()?;
            let current = self
                .session
                .lock()
                .await
                .as_ref()
                .map(|route| (route.generation, route.is_closed()));
            let Some((generation, closed)) = current else {
                return self
                    .connect_with_retry("no active session found during ensure_connected")
                    .await;
            };
            if closed {
                self.invalidate_session_for_generation(generation, "SSH transport closed")
                    .await;
                continue;
            }
            if self.is_health_probe_fresh(generation).await {
                return Ok(());
            }

            let probe_guard = self.health_probe_lock.lock().await;
            if self.is_health_probe_fresh(generation).await {
                return Ok(());
            }
            if let Err(probe_error) = self.run_health_probe(generation).await {
                warn!(error = ?probe_error, "SSH health probe failed");
                self.invalidate_session_for_generation(generation, "health probe failed")
                    .await;
                drop(probe_guard);
                // A newer owner may already have recovered; recheck that route.
                if self.is_connected().await {
                    continue;
                }
                return self
                    .connect_with_retry("health probe failed during ensure_connected")
                    .await;
            }
            if self.mark_health_probe_ok(generation).await {
                return Ok(());
            }
        }
    }

    fn health_probe_ttl(&self) -> Duration {
        let ttl_ms = self
            .config
            .health_probe_timeout_ms
            .saturating_mul(2)
            .clamp(MIN_HEALTH_PROBE_TTL_MS, MAX_HEALTH_PROBE_TTL_MS);
        Duration::from_millis(ttl_ms)
    }

    async fn is_health_probe_fresh(&self, generation: u64) -> bool {
        let session = self.session.lock().await;
        if !session
            .as_ref()
            .is_some_and(|route| route.generation == generation && !route.is_closed())
        {
            return false;
        }
        let guard = self.last_health_probe_ok_at.lock().await;
        if let Some((cached_generation, last_ok_at)) = guard.as_ref() {
            return *cached_generation == generation
                && last_ok_at.elapsed() < self.health_probe_ttl();
        }

        false
    }

    async fn mark_health_probe_ok(&self, generation: u64) -> bool {
        let session = self.session.lock().await;
        if !session
            .as_ref()
            .is_some_and(|route| route.generation == generation && !route.is_closed())
        {
            return false;
        }
        let mut guard = self.last_health_probe_ok_at.lock().await;
        *guard = Some((generation, tokio::time::Instant::now()));
        true
    }

    async fn run_health_probe(&self, generation: u64) -> Result<()> {
        let ping_result = {
            let session_guard = self.session.lock().await;
            let route = session_guard
                .as_ref()
                .ok_or_else(|| SshMcpError::connection("SSH connection not established"))?;

            if route.generation != generation || route.is_closed() {
                return Err(SshMcpError::connection(
                    "SSH route changed or closed before health probe",
                ));
            }

            let result = timeout(
                Duration::from_millis(self.config.health_probe_timeout_ms),
                route.target.send_ping(),
            )
            .await;
            // russh 0.61.2 ignores a lost ping oneshot receiver and returns Ok.
            // The handle's sender closure must also be checked after the ping.
            if route.is_closed() {
                return Err(SshMcpError::connection(
                    "SSH transport closed during health probe",
                ));
            }
            result
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
        self.open_channel_attempt()
            .await
            .map_err(|(_, error)| error)
    }

    pub(crate) async fn open_channel_attempt(
        &self,
    ) -> std::result::Result<(u64, Channel<client::Msg>), (Option<u64>, SshMcpError)> {
        let mut generation = None;
        let result = self
            .bounded_transport(Duration::from_secs(CONNECTION_TIMEOUT_SECS), async {
                let session_guard = self.session.lock().await;
                let route = session_guard
                    .as_ref()
                    .ok_or_else(|| SshMcpError::connection("SSH connection not established"))?;
                generation = Some(route.generation);
                if route.is_closed() {
                    return Err(SshMcpError::connection(
                        "SSH transport closed before channel open",
                    ));
                }
                let channel =
                    route.target.channel_open_session().await.map_err(|e| {
                        SshMcpError::connection(format!("Failed to open channel: {e}"))
                    })?;
                Ok((route.generation, channel))
            })
            .await;
        result.map_err(|error| (generation, error))
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
        let session = self.session.lock().await;
        channel_guard.is_some()
            && session.as_ref().is_some_and(|route| {
                route.generation == self.su_generation.load(Ordering::SeqCst) && !route.is_closed()
            })
    }

    pub(super) async fn reset_su_state_for_generation(&self, generation: u64) {
        let channel = {
            let mut guard = self.su_channel.lock().await;
            let session = self.session.lock().await;
            if session.as_ref().map(|route| route.generation) != Some(generation) {
                return;
            }
            self.is_elevated.store(false, Ordering::SeqCst);
            self.su_generation.store(0, Ordering::SeqCst);
            guard.take()
        };
        if let Some(channel) = channel {
            let _ = timeout(Duration::from_millis(100), channel.eof()).await;
        }
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
                self.su_generation.store(generation, Ordering::SeqCst);
                self.is_elevated.store(true, Ordering::SeqCst);
                info!("Successfully elevated to root via su");
                Ok(())
            }
            Err(e) => {
                self.reset_su_state_for_generation(generation).await;
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
                let _ = timeout(Duration::from_millis(100), ch.eof()).await;
            }
            self.su_generation.store(0, Ordering::SeqCst);
            self.is_elevated.store(false, Ordering::SeqCst);
        }

        Ok(())
    }

    /// Close the SSH connection
    pub async fn close(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        self.shutdown_token.cancel();

        // Remove the route before asynchronous teardown. Lock order: su -> route.
        let (su_channel, route) = {
            let mut channel_guard = self.su_channel.lock().await;
            let mut session_guard = self.session.lock().await;
            let mut probe_guard = self.last_health_probe_ok_at.lock().await;
            self.is_elevated.store(false, Ordering::SeqCst);
            self.su_generation.store(0, Ordering::SeqCst);
            *probe_guard = None;
            (channel_guard.take(), session_guard.take())
        };
        if let Some(ch) = su_channel {
            let _ = timeout(Duration::from_millis(100), ch.eof()).await;
        }
        // Close target first, then the jump session that carries it.
        if let Some(route) = route {
            Self::disconnect_route(route).await;
        }

        info!("SSH connection closed");
    }

    /// Invalidate the current session and clear elevation state
    ///
    /// This clears the session handle, su_channel, and resets elevation state.
    /// Used when a connection is detected as broken and needs reconnection.
    pub async fn invalidate_session(&self, reason: &str) {
        self.invalidate_session_inner(None, reason).await;
    }

    /// An old operation may only remove the route on which it ran.
    pub(crate) async fn invalidate_session_for_generation(&self, generation: u64, reason: &str) {
        self.invalidate_session_inner(Some(generation), reason)
            .await;
    }

    async fn invalidate_session_inner(&self, expected: Option<u64>, reason: &str) {
        // Take channel out of mutex before awaiting to avoid deadlock
        let (channel, route) = {
            let mut channel_guard = self.su_channel.lock().await;
            let mut session_guard = self.session.lock().await;
            if expected.is_some()
                && session_guard.as_ref().map(|route| route.generation) != expected
            {
                return;
            }
            warn!(reason = ?reason, "Invalidating SSH session");
            let mut probe_guard = self.last_health_probe_ok_at.lock().await;
            self.is_elevated.store(false, Ordering::SeqCst);
            self.su_generation.store(0, Ordering::SeqCst);
            *probe_guard = None;
            (channel_guard.take(), session_guard.take())
        };

        // Drop lock before awaiting EOF
        if let Some(ch) = channel {
            let _ = timeout(Duration::from_millis(100), ch.eof()).await;
        }
        // Attempt best-effort graceful disconnect with short timeout.

        if let Some(route) = route {
            Self::disconnect_route(route).await;
        }

        debug!(reason = ?reason, "Session invalidated");
    }

    /// Force a reconnection by invalidating the current session and reconnecting
    ///
    /// This is used when the connection is known to be broken and a fresh
    /// connection is required. It clears all session state and performs
    /// a new connection attempt.
    pub async fn reconnect(&self) -> Result<()> {
        self.bounded_transport(Duration::from_secs(CONNECTION_TIMEOUT_SECS), async {
            self.invalidate_session("explicit reconnect requested")
                .await;
            self.connect_with_retry("explicit reconnect requested")
                .await
        })
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

    struct TestPeer {
        stall_channel_open: bool,
        channel_opened: Arc<Notify>,
    }

    impl russh::server::Handler for TestPeer {
        type Error = russh::Error;

        async fn auth_password(
            &mut self,
            _: &str,
            _: &str,
        ) -> std::result::Result<russh::server::Auth, Self::Error> {
            Ok(russh::server::Auth::Accept)
        }

        async fn channel_open_session(
            &mut self,
            _: Channel<russh::server::Msg>,
            _: &mut russh::server::Session,
        ) -> std::result::Result<bool, Self::Error> {
            self.channel_opened.notify_one();
            if self.stall_channel_open {
                std::future::pending::<()>().await;
            }
            Ok(true)
        }
    }

    // Real russh handles over an in-memory transport; no Docker or SSH daemon.
    async fn test_route(stall_channel_open: bool, channel_opened: Arc<Notify>) -> ActiveRoute {
        let key = russh::keys::PrivateKey::new(
            russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&[42; 32]).into(),
            "unit-test",
        )
        .unwrap();
        let config = Arc::new(russh::server::Config {
            keys: vec![key],
            ..Default::default()
        });
        let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            let session = russh::server::run_stream(
                config,
                server_stream,
                TestPeer {
                    stall_channel_open,
                    channel_opened,
                },
            )
            .await
            .unwrap();
            let _ = session.await;
        });
        let handler = SshHandler::new(
            "unit-test",
            22,
            super::super::config::HostKeyCheckMode::No,
            None,
        );
        let mut target =
            client::connect_stream(Arc::new(client::Config::default()), client_stream, handler)
                .await
                .unwrap();
        assert!(
            target
                .authenticate_password("test", "test")
                .await
                .unwrap()
                .success()
        );
        ActiveRoute {
            target,
            jump: None,
            generation: 1,
            auto_elevation_attempted: false,
        }
    }

    #[tokio::test]
    async fn test_connect_owner_rechecks_route_after_first_check() {
        let manager =
            SshConnectionManager::new(SshConfig::new("127.0.0.1", "test").with_port(9)).await;
        let route = test_route(false, Arc::new(Notify::new())).await;
        let guard = manager.session.lock().await;
        let connect = manager.connect_transport_only();
        tokio::pin!(connect);
        // Queue the first connection check, then a publication, on the route lock.
        tokio::select! {
            biased;
            _ = &mut connect => panic!("connection should wait on the route lock"),
            _ = std::future::ready(()) => {},
        }
        let publish = async {
            *manager.session.lock().await = Some(route);
        };
        tokio::pin!(publish);
        tokio::select! {
            biased;
            _ = &mut publish => panic!("publication should wait on the route lock"),
            _ = std::future::ready(()) => {},
        }
        drop(guard);
        let (result, ()) = tokio::join!(connect, publish);
        result.expect("reuse the route published after the first connection check");
        assert_eq!(manager.session.lock().await.as_ref().unwrap().generation, 1);
        manager.close().await;
    }

    #[tokio::test]
    async fn test_old_generation_cannot_reset_health_or_restore_su() {
        let manager = SshConnectionManager::new(SshConfig::new("unit-test", "test")).await;
        *manager.session.lock().await = Some(test_route(false, Arc::new(Notify::new())).await);
        let (old_generation, channel) = manager.open_channel_with_generation().await.unwrap();
        *manager.su_channel.lock().await = Some(channel);
        manager
            .su_generation
            .store(old_generation, Ordering::SeqCst);
        manager.is_elevated.store(true, Ordering::SeqCst);
        let (generation, borrowed) = manager.try_take_su_channel().await.unwrap();
        let (_, spare_old_channel) = manager.open_channel_with_generation().await.unwrap();
        manager
            .invalidate_session_for_generation(generation, "test replacement")
            .await;
        let mut route = test_route(false, Arc::new(Notify::new())).await;
        route.generation = 2;
        *manager.session.lock().await = Some(route);
        manager.restore_su_channel(generation, borrowed).await;
        assert!(
            manager.su_channel.lock().await.is_none(),
            "old shell must not populate the new route's empty SU slot"
        );
        let (_, new_channel) = manager.open_channel_with_generation().await.unwrap();
        *manager.su_channel.lock().await = Some(new_channel);
        manager.su_generation.store(2, Ordering::SeqCst);
        manager.is_elevated.store(true, Ordering::SeqCst);
        assert!(manager.mark_health_probe_ok(2).await);

        manager
            .restore_su_channel(generation, spare_old_channel)
            .await;
        manager.reset_su_state_for_generation(generation).await;
        manager
            .invalidate_session_for_generation(generation, "late old failure")
            .await;
        assert!(!manager.mark_health_probe_ok(generation).await);
        assert!(!manager.is_health_probe_fresh(generation).await);
        assert!(manager.is_health_probe_fresh(2).await);
        assert!(manager.is_elevated());
        assert!(manager.has_su_channel().await);
        assert_eq!(manager.su_generation.load(Ordering::SeqCst), 2);
        assert_eq!(manager.session.lock().await.as_ref().unwrap().generation, 2);
        manager.close().await;
    }

    #[tokio::test]
    async fn test_transport_readiness_is_rootless_and_closed_route_bypasses_cache() {
        let manager = SshConnectionManager::new(
            SshConfig::new("unit-test", "test").with_su_password("unused"),
        )
        .await;
        *manager.session.lock().await = Some(test_route(false, Arc::new(Notify::new())).await);
        timeout(
            Duration::from_secs(2),
            manager.ensure_connected_transport_only(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(manager.is_health_probe_fresh(1).await);
        assert!(!manager.is_elevated());
        {
            let session = manager.session.lock().await;
            let route = session.as_ref().unwrap();
            assert!(!route.auto_elevation_attempted);
            route
                .target
                .disconnect(russh::Disconnect::ByApplication, "test", "")
                .await
                .unwrap();
        }
        timeout(Duration::from_secs(2), async {
            while manager.is_connected().await {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!manager.is_health_probe_fresh(1).await);
        assert!(!manager.mark_health_probe_ok(1).await);
        assert!(manager.run_health_probe(1).await.is_err());
        manager.close().await;
    }

    #[tokio::test]
    async fn test_transport_budget_includes_route_lock_owner_wait_and_backoff() {
        let mut config = SshConfig::new("127.0.0.1", "test").with_port(9);
        config.reconnect_backoff_ms = 30_000;
        let manager = SshConnectionManager::new(config).await;
        let budget = Duration::from_millis(30);
        let guard = manager.session.lock().await;
        assert!(
            manager
                .bounded_transport(budget, manager.ensure_transport_inner())
                .await
                .is_err()
        );
        drop(guard);
        manager.is_connecting.store(true, Ordering::SeqCst);
        let owner = ConnectAttemptGuard {
            is_connecting: &manager.is_connecting,
            connect_notify: &manager.connect_notify,
        };
        assert!(
            manager
                .bounded_transport(budget, manager.ensure_transport_inner())
                .await
                .is_err()
        );
        drop(owner);
        let error = manager
            .bounded_transport(budget, manager.ensure_transport_inner())
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("transport acquisition timed out")
        );
        assert!(!manager.is_connecting.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn test_channel_open_blackhole_releases_route_lock_at_budget() {
        let manager =
            Arc::new(SshConnectionManager::new(SshConfig::new("unit-test", "test")).await);
        let opened = Arc::new(Notify::new());
        *manager.session.lock().await = Some(test_route(true, opened.clone()).await);
        let task_manager = manager.clone();
        let open = tokio::spawn(async move { task_manager.open_channel_attempt().await });
        timeout(Duration::from_secs(5), opened.notified())
            .await
            .unwrap();
        assert!(
            timeout(Duration::from_millis(20), manager.session.lock())
                .await
                .is_err()
        );
        let (generation, error) = timeout(Duration::from_secs(CONNECTION_TIMEOUT_SECS + 2), open)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert_eq!(generation, Some(1));
        assert!(
            error
                .to_string()
                .contains("transport acquisition timed out")
        );
        let guard = timeout(Duration::from_millis(100), manager.session.lock())
            .await
            .unwrap();
        assert!(guard.is_some());
        drop(guard);
        manager.close().await;
    }

    #[tokio::test]
    async fn test_shutdown_cancels_connect_owner_and_waiter() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let manager = Arc::new(
            SshConnectionManager::new(
                SshConfig::new("127.0.0.1", "test")
                    .with_port(listener.local_addr().unwrap().port())
                    .with_password("test"),
            )
            .await,
        );
        let owner_manager = manager.clone();
        let owner =
            tokio::spawn(async move { owner_manager.ensure_connected_transport_only().await });
        let (_stream, _) = timeout(Duration::from_secs(5), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let waiter_manager = manager.clone();
        let waiter =
            tokio::spawn(async move { waiter_manager.ensure_connected_transport_only().await });
        manager.close().await;
        assert!(
            timeout(Duration::from_millis(100), owner)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        assert!(
            timeout(Duration::from_millis(100), waiter)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        assert!(!manager.is_connecting.load(Ordering::SeqCst));
        assert!(!manager.is_connected().await);
    }

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
