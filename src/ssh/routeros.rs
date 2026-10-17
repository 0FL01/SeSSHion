//! One-shot, native RouterOS EXEC. No shell wrapping, elevation, or replay.

use std::sync::Arc;
use std::time::Duration;

use russh::{Channel, ChannelMsg, client};
use tokio::sync::oneshot;
use tokio::time::{Instant, sleep_until, timeout};
use tokio_util::sync::CancellationToken;

use super::command::CommandOutput;
use super::connection::{ChannelOpenFailureKind, SshConnectionManager};
use crate::config::CONNECTION_TIMEOUT_SECS;

const BYTES_PER_TOKEN: usize = 4;
const CLOSE_BUDGET: Duration = Duration::from_millis(100);

#[derive(Debug)]
pub(crate) struct RouterOsExecFailure {
    pub(crate) output: CommandOutput,
    pub(crate) message: String,
    pub(crate) may_have_run: bool,
}

type NativeResult = std::result::Result<CommandOutput, RouterOsExecFailure>;

/// Dropping the public request future signals its owner, rather than dropping a
/// bare Channel (or the receiver of an already queued OPEN).
struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

struct RequestStop {
    deadline: Instant,
    cancellation: CancellationToken,
    shutdown: CancellationToken,
}

impl RequestStop {
    fn reason(&self) -> Option<&'static str> {
        if self.shutdown.is_cancelled() {
            Some("SSH connection manager is shutting down")
        } else if self.cancellation.is_cancelled() {
            Some("RouterOS command cancelled")
        } else if Instant::now() >= self.deadline {
            Some("RouterOS command deadline exceeded")
        } else {
            None
        }
    }

    async fn stopped(&self) -> &'static str {
        tokio::select! {
            biased;
            _ = self.shutdown.cancelled() => "SSH connection manager is shutting down",
            _ = self.cancellation.cancelled() => "RouterOS command cancelled",
            _ = sleep_until(self.deadline) => "RouterOS command deadline exceeded",
        }
    }

    async fn wait<T>(
        &self,
        operation: impl std::future::Future<Output = T>,
    ) -> std::result::Result<T, &'static str> {
        // Explicit wall-clock checking also bounds an always-ready output flood.
        if let Some(reason) = self.reason() {
            return Err(reason);
        }
        tokio::select! {
            biased;
            reason = self.stopped() => Err(reason),
            result = operation => Ok(result),
        }
    }
}

#[derive(Default)]
struct BytePrefix {
    bytes: Vec<u8>,
    total_bytes: usize,
}

impl BytePrefix {
    fn append(&mut self, data: &[u8], limit: Option<usize>) {
        self.total_bytes = self.total_bytes.saturating_add(data.len());
        let take = limit
            .map(|limit| limit.saturating_sub(self.bytes.len()).min(data.len()))
            .unwrap_or(data.len());
        self.bytes.extend_from_slice(&data[..take]);
    }

    fn finish(self) -> (String, bool, usize) {
        let truncated = self.total_bytes > self.bytes.len();
        // Decode only after collecting bytes, so packet boundaries cannot break
        // otherwise valid UTF-8. A prefix ending mid-codepoint remains safe/lossy.
        (
            String::from_utf8_lossy(&self.bytes).into_owned(),
            truncated,
            self.total_bytes.div_ceil(BYTES_PER_TOKEN),
        )
    }
}

struct Collector {
    stdout: BytePrefix,
    stderr: BytePrefix,
    limit: Option<usize>,
    exit_code: Option<u32>,
    exit_signal: Option<String>,
    may_have_run: bool,
}

impl Collector {
    fn new(max_tokens: Option<usize>) -> Self {
        Self {
            stdout: BytePrefix::default(),
            stderr: BytePrefix::default(),
            limit: max_tokens.map(|tokens| tokens.saturating_mul(BYTES_PER_TOKEN)),
            exit_code: None,
            exit_signal: None,
            may_have_run: false,
        }
    }

    /// None means keep draining. Only Close (not EOF/status/ack) completes a
    /// successful request; an explicit rejection is an immediate failure.
    fn event(&mut self, message: Option<ChannelMsg>) -> Option<std::result::Result<(), String>> {
        match message {
            Some(ChannelMsg::Data { data }) => self.stdout.append(&data, self.limit),
            Some(ChannelMsg::ExtendedData { data, ext: 1 }) => {
                self.stderr.append(&data, self.limit);
            }
            Some(ChannelMsg::ExtendedData { data, .. }) => {
                self.stdout.append(&data, self.limit);
            }
            Some(ChannelMsg::ExitStatus { exit_status }) => self.exit_code = Some(exit_status),
            Some(ChannelMsg::ExitSignal { signal_name, .. }) => {
                self.exit_signal = Some(format!(
                    "RouterOS command exited with signal {signal_name:?}"
                ));
            }
            Some(ChannelMsg::Failure) => {
                self.may_have_run = false;
                return Some(Err(
                    "SSH peer rejected the RouterOS EXEC request".to_string()
                ));
            }
            Some(ChannelMsg::Close) => {
                return Some(if let Some(signal) = &self.exit_signal {
                    Err(signal.clone())
                } else if self.exit_code.is_none() {
                    Err("RouterOS channel closed without an exit status".to_string())
                } else {
                    Ok(())
                });
            }
            None => {
                return Some(Err("RouterOS channel ended without SSH Close".to_string()));
            }
            // In particular Success is optional, and EOF does not end draining.
            _ => {}
        }
        None
    }

    fn finish(self, result: std::result::Result<(), String>) -> NativeResult {
        let (stdout, stdout_truncated, stdout_total_tokens) = self.stdout.finish();
        let (stderr, stderr_truncated, stderr_total_tokens) = self.stderr.finish();
        let output = CommandOutput {
            stdout,
            stderr,
            exit_code: self.exit_code,
            stdout_truncated,
            stderr_truncated,
            stdout_total_tokens,
            stderr_total_tokens,
        };
        match result {
            Ok(()) => Ok(output),
            Err(message) => Err(RouterOsExecFailure {
                output,
                message,
                may_have_run: self.may_have_run,
            }),
        }
    }
}

fn send_result(sender: &mut Option<oneshot::Sender<NativeResult>>, result: NativeResult) {
    if let Some(sender) = sender.take() {
        let _ = sender.send(result);
    }
}

fn not_started(message: impl Into<String>) -> NativeResult {
    Err(RouterOsExecFailure {
        output: CommandOutput::default(),
        message: message.into(),
        may_have_run: false,
    })
}

async fn close_channel(channel: &Channel<client::Msg>) {
    let _ = timeout(CLOSE_BUDGET, channel.close()).await;
}

impl SshConnectionManager {
    pub(crate) async fn exec_routeros_command(
        self: &Arc<Self>,
        command: String,
        timeout: Duration,
        cancellation: CancellationToken,
    ) -> NativeResult {
        self.exec_routeros_with_open_budget(
            command,
            timeout,
            cancellation,
            Duration::from_secs(CONNECTION_TIMEOUT_SECS),
        )
        .await
    }

    async fn exec_routeros_with_open_budget(
        self: &Arc<Self>,
        command: String,
        budget: Duration,
        cancellation: CancellationToken,
        open_budget: Duration,
    ) -> NativeResult {
        // This absolute deadline precedes even the semaphore wait.
        let Some(deadline) = Instant::now().checked_add(budget) else {
            return not_started("RouterOS command timeout is too large");
        };
        let cancellation = cancellation.child_token();
        let _cancel_on_drop = CancelOnDrop(cancellation.clone());
        let stop = RequestStop {
            deadline,
            cancellation,
            shutdown: self.shutdown_token.clone(),
        };
        let (sender, receiver) = oneshot::channel();
        let manager = self.clone();
        // Only this task ever owns the pending OPEN and its resulting Channel.
        tokio::spawn(async move {
            manager
                .routeros_owner(command, stop, open_budget, sender)
                .await;
        });
        receiver.await.unwrap_or_else(|_| {
            Err(RouterOsExecFailure {
                output: CommandOutput::default(),
                message: "RouterOS execution owner ended unexpectedly".to_string(),
                may_have_run: true,
            })
        })
    }

    async fn routeros_owner(
        self: Arc<Self>,
        command: String,
        stop: RequestStop,
        open_budget: Duration,
        sender: oneshot::Sender<NativeResult>,
    ) {
        let mut sender = Some(sender);
        // The owner holds this slot until all channel/open cleanup has finished,
        // including when its caller has already returned or dropped its future.
        let _permit = match stop.wait(self.acquire_command_slot()).await {
            Ok(Ok(permit)) => permit,
            Ok(Err(error)) => {
                send_result(&mut sender, not_started(error.to_string()));
                return;
            }
            Err(reason) => {
                send_result(&mut sender, not_started(reason));
                return;
            }
        };
        match stop.wait(self.ensure_connected_transport_only()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                send_result(&mut sender, not_started(error.to_string()));
                return;
            }
            Err(reason) => {
                send_result(&mut sender, not_started(reason));
                return;
            }
        }
        if let Some(reason) = stop.reason() {
            send_result(&mut sender, not_started(reason));
            return;
        }

        let open = self.open_channel_attempt_detailed(open_budget, false);
        tokio::pin!(open);
        let opened = match stop.wait(&mut open).await {
            Ok(opened) => opened,
            Err(reason) => {
                // Returning the request does not cancel russh's queued OPEN.
                send_result(&mut sender, not_started(reason));
                open.await
            }
        };
        let (_, mut channel) = match opened {
            Ok(opened) => opened,
            Err(failure) => {
                send_result(&mut sender, not_started(failure.error.to_string()));
                if failure.kind == ChannelOpenFailureKind::BudgetElapsed
                    && let Some(generation) = failure.generation
                {
                    // Only an unresolved OPEN's captured generation is retired.
                    // Response delivery must not await potentially contended locks.
                    self.invalidate_session_for_generation(
                        generation,
                        "native channel OPEN acquisition budget exhausted",
                    )
                    .await;
                }
                return;
            }
        };

        let mut collector = Collector::new(self.config.max_output_tokens);
        let result = if let Some(reason) = stop.reason() {
            Err(reason.to_string())
        } else {
            match stop.wait(channel.exec(true, command.into_bytes())).await {
                Ok(Ok(())) => {
                    collector.may_have_run = true;
                    loop {
                        let event = match stop.wait(channel.wait()).await {
                            Ok(event) => event,
                            Err(reason) => break Err(reason.to_string()),
                        };
                        if let Some(result) = collector.event(event) {
                            break result;
                        }
                    }
                }
                Ok(Err(error)) => Err(format!("Failed to enqueue RouterOS EXEC: {error}")),
                // russh exec is a single cancellation-safe mpsc send: until it
                // completes successfully, no EXEC request has been enqueued.
                Err(reason) => Err(reason.to_string()),
            }
        };
        close_channel(&channel).await;
        send_result(&mut sender, collector.finish(result));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use russh::server;
    use tokio::sync::Semaphore;
    use tokio::task::JoinHandle;

    use super::*;
    use crate::ssh::config::{HostKeyCheckMode, SshConfig};
    use crate::ssh::connection::CHANNEL_SEMAPHORE_CAPACITY;

    fn data(bytes: &[u8]) -> ChannelMsg {
        ChannelMsg::Data {
            data: bytes.to_vec().into(),
        }
    }

    fn extended(ext: u32, bytes: &[u8]) -> ChannelMsg {
        ChannelMsg::ExtendedData {
            data: bytes.to_vec().into(),
            ext,
        }
    }

    fn collect(events: impl IntoIterator<Item = ChannelMsg>) -> NativeResult {
        let mut collector = Collector::new(None);
        collector.may_have_run = true;
        for event in events {
            if let Some(result) = collector.event(Some(event)) {
                return collector.finish(result);
            }
        }
        let result = collector.event(None).unwrap();
        collector.finish(result)
    }

    #[test]
    fn test_native_collector_event_orders_and_optional_ack() {
        for events in [
            vec![
                data(b"before-ack "),
                ChannelMsg::ExitStatus { exit_status: 0 },
                ChannelMsg::Eof,
                ChannelMsg::Success,
                data(b"after-status"),
                extended(1, b"stderr"),
                ChannelMsg::Close,
            ],
            vec![
                ChannelMsg::ExitStatus { exit_status: 0 },
                data(b"before-ack after-status"),
                extended(1, b"stderr"),
                ChannelMsg::Close,
            ],
        ] {
            let output = collect(events).unwrap();
            assert_eq!(output.stdout, "before-ack after-status");
            assert_eq!(output.stderr, "stderr");
            assert_eq!(output.exit_code, Some(0));
        }
        assert_eq!(
            collect([ChannelMsg::ExitStatus { exit_status: 7 }, ChannelMsg::Close])
                .unwrap()
                .exit_code,
            Some(7)
        );
    }

    #[test]
    fn test_native_collector_incomplete_signal_and_rejection_keep_partial_output() {
        for terminal in [ChannelMsg::Close, ChannelMsg::Failure] {
            let rejection = matches!(terminal, ChannelMsg::Failure);
            let failure = collect([data(b"partial"), extended(1, b"error"), terminal]).unwrap_err();
            assert_eq!(failure.output.stdout, "partial");
            assert_eq!(failure.output.stderr, "error");
            assert_eq!(failure.output.exit_code, None);
            assert_eq!(failure.may_have_run, !rejection);
        }
        let incomplete = collect([
            data(b"partial"),
            ChannelMsg::ExitStatus { exit_status: 0 },
            ChannelMsg::Eof,
        ])
        .unwrap_err();
        assert!(incomplete.message.contains("without SSH Close"));
        assert_eq!(incomplete.output.exit_code, Some(0));
        assert_eq!(incomplete.output.stdout, "partial");
        assert!(incomplete.may_have_run);

        for exit_code in [None, Some(0)] {
            let mut events = vec![data(b"before signal")];
            if let Some(exit_status) = exit_code {
                events.push(ChannelMsg::ExitStatus { exit_status });
            }
            events.extend([
                ChannelMsg::ExitSignal {
                    signal_name: russh::Sig::TERM,
                    core_dumped: false,
                    error_message: "signal".to_string(),
                    lang_tag: String::new(),
                },
                data(b" after signal"),
                ChannelMsg::Close,
            ]);
            let failure = collect(events).unwrap_err();
            assert_eq!(failure.output.exit_code, exit_code);
            assert_eq!(failure.output.stdout, "before signal after signal");
            assert!(failure.message.contains("signal"));
            assert!(failure.may_have_run);
        }
        let rejection = collect([
            data(b"partial"),
            ChannelMsg::ExitStatus { exit_status: 0 },
            ChannelMsg::Failure,
        ])
        .unwrap_err();
        assert_eq!(rejection.output.exit_code, Some(0));
        assert!(!rejection.may_have_run);
    }

    #[test]
    fn test_native_prefix_caps_unknown_data_and_split_utf8() {
        let mut collector = Collector::new(Some(2));
        for event in [
            data(&[b'A', 0xf0, 0x9f]),
            data(&[0x99, 0x82, b'B']),
            extended(42, b"CDEFG"),
            extended(1, &[0xe2]),
            extended(1, &[0x82, 0xac, b'1', b'2', b'3', b'4', b'5', b'6']),
        ] {
            assert!(collector.event(Some(event)).is_none());
        }
        assert_eq!(collector.stdout.bytes.len(), 8);
        assert_eq!(collector.stderr.bytes.len(), 8);
        let output = collector.finish(Ok(())).unwrap();
        assert_eq!(output.stdout, "A🙂BCD");
        assert_eq!(output.stderr, "€12345");
        assert!(output.stdout_truncated);
        assert!(output.stderr_truncated);
        assert_eq!(output.stdout_total_tokens, 3);
        assert_eq!(output.stderr_total_tokens, 3);

        let mut zero = Collector::new(Some(0));
        zero.event(Some(data(b"discard")));
        assert_eq!(zero.stdout.bytes.len(), 0);
        assert!(zero.finish(Ok(())).unwrap().stdout_truncated);
        let mut unlimited = Collector::new(None);
        let bytes = vec![b'x'; 70_000];
        unlimited.event(Some(data(&bytes)));
        unlimited.event(Some(extended(1, &bytes)));
        let output = unlimited.finish(Ok(())).unwrap();
        assert_eq!(output.stdout.len(), bytes.len());
        assert_eq!(output.stderr.len(), bytes.len());
        assert!(!output.stdout_truncated);
        assert!(!output.stderr_truncated);
    }

    struct PeerState {
        commands: Mutex<Vec<Vec<u8>>>,
        opened: AtomicUsize,
        closed: AtomicUsize,
        unexpected: AtomicUsize,
        delay_next_open: AtomicBool,
        reject_next_open: AtomicBool,
        open_gate: Semaphore,
        stop: CancellationToken,
    }

    impl Default for PeerState {
        fn default() -> Self {
            Self {
                commands: Mutex::default(),
                opened: AtomicUsize::new(0),
                closed: AtomicUsize::new(0),
                unexpected: AtomicUsize::new(0),
                delay_next_open: AtomicBool::new(false),
                reject_next_open: AtomicBool::new(false),
                open_gate: Semaphore::new(0),
                stop: CancellationToken::new(),
            }
        }
    }

    struct Peer(Arc<PeerState>);

    impl server::Handler for Peer {
        type Error = russh::Error;

        async fn auth_password(
            &mut self,
            _: &str,
            _: &str,
        ) -> std::result::Result<server::Auth, Self::Error> {
            Ok(server::Auth::Accept)
        }

        async fn channel_open_session(
            &mut self,
            _: Channel<server::Msg>,
            _: &mut server::Session,
        ) -> std::result::Result<bool, Self::Error> {
            self.0.opened.fetch_add(1, Ordering::SeqCst);
            if self.0.delay_next_open.swap(false, Ordering::SeqCst) {
                tokio::select! {
                    _ = self.0.stop.cancelled() => return Ok(false),
                    permit = self.0.open_gate.acquire() => permit.unwrap().forget(),
                }
            }
            Ok(!self.0.reject_next_open.swap(false, Ordering::SeqCst))
        }

        async fn channel_close(
            &mut self,
            _: russh::ChannelId,
            _: &mut server::Session,
        ) -> std::result::Result<(), Self::Error> {
            self.0.closed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        #[allow(clippy::too_many_arguments)]
        async fn pty_request(
            &mut self,
            _: russh::ChannelId,
            _: &str,
            _: u32,
            _: u32,
            _: u32,
            _: u32,
            _: &[(russh::Pty, u32)],
            _: &mut server::Session,
        ) -> std::result::Result<(), Self::Error> {
            self.0.unexpected.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn shell_request(
            &mut self,
            _: russh::ChannelId,
            _: &mut server::Session,
        ) -> std::result::Result<(), Self::Error> {
            self.0.unexpected.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn subsystem_request(
            &mut self,
            _: russh::ChannelId,
            _: &str,
            _: &mut server::Session,
        ) -> std::result::Result<(), Self::Error> {
            self.0.unexpected.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn exec_request(
            &mut self,
            channel: russh::ChannelId,
            command: &[u8],
            session: &mut server::Session,
        ) -> std::result::Result<(), Self::Error> {
            self.0.commands.lock().unwrap().push(command.to_vec());
            match command {
                b"hang" => {
                    session.channel_success(channel)?;
                    session.data(channel, b"partial stdout".to_vec())?;
                    session.extended_data(channel, 1, b"partial stderr".to_vec())?;
                    session.exit_status_request(channel, 0)?;
                }
                b"reject" => {
                    session.data(channel, b"before rejection".to_vec())?;
                    session.exit_status_request(channel, 0)?;
                    session.channel_failure(channel)?;
                    // No Close: the client must reject immediately, not time out.
                }
                b"no-status" => {
                    session.channel_success(channel)?;
                    session.data(channel, b"partial".to_vec())?;
                    session.close(channel)?;
                }
                b"signal" => {
                    session.channel_success(channel)?;
                    session.exit_signal_request(channel, russh::Sig::TERM, false, "", "")?;
                    session.data(channel, b"after signal".to_vec())?;
                    session.close(channel)?;
                }
                b"disconnect" => {
                    session.data(channel, b"before disconnect".to_vec())?;
                    session.exit_status_request(channel, 0)?;
                    session.disconnect(russh::Disconnect::ByApplication, "test", "")?;
                }
                b"flood" => {
                    session.channel_success(channel)?;
                    session.exit_status_request(channel, 0)?;
                    let handle = session.handle();
                    let state = self.0.clone();
                    tokio::spawn(async move {
                        loop {
                            tokio::select! {
                                _ = state.stop.cancelled() => break,
                                result = handle.extended_data(channel, 42, vec![b'x'; 1024]) => {
                                    if result.is_err() { break; }
                                }
                            }
                            tokio::task::yield_now().await;
                        }
                    });
                }
                _ => {
                    // Deliberately send status/data before ack, and no EOF.
                    session.exit_status_request(channel, 0)?;
                    session.data(channel, vec![b'A', 0xf0, 0x9f])?;
                    session.data(channel, vec![0x99, 0x82, b'B'])?;
                    if command != b"no-ack" {
                        session.channel_success(channel)?;
                    }
                    session.extended_data(channel, 1, b"stderr".to_vec())?;
                    session.close(channel)?;
                }
            }
            Ok(())
        }
    }

    struct Fixture {
        manager: Arc<SshConnectionManager>,
        state: Arc<PeerState>,
        server: JoinHandle<()>,
    }

    impl Fixture {
        async fn new(max_tokens: Option<usize>) -> Self {
            let key = russh::keys::PrivateKey::new(
                russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&[43; 32]).into(),
                "routeros-test",
            )
            .unwrap();
            let config = Arc::new(server::Config {
                keys: vec![key],
                ..Default::default()
            });
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .unwrap();
            let port = listener.local_addr().unwrap().port();
            let state = Arc::new(PeerState::default());
            let peer_state = state.clone();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let running = server::run_stream(config, stream, Peer(peer_state))
                    .await
                    .unwrap();
                let _ = running.await;
            });
            let mut config = SshConfig::new("127.0.0.1", "test")
                .with_port(port)
                .with_password("test")
                .with_max_output_tokens(max_tokens)
                .with_su_password("must-not-be-used");
            config.host_key_checking = HostKeyCheckMode::No;
            let manager = Arc::new(
                SshConnectionManager::new(config)
                    .await
                    .with_target_openssh_keepalives(false),
            );
            manager.ensure_connected_transport_only().await.unwrap();
            Self {
                manager,
                state,
                server,
            }
        }

        async fn exec(&self, command: &str, budget: Duration) -> NativeResult {
            self.manager
                .exec_routeros_command(command.to_string(), budget, CancellationToken::new())
                .await
        }

        async fn wait_for(&self, ready: impl Fn() -> bool) {
            timeout(Duration::from_secs(2), async {
                while !ready() {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .unwrap();
        }

        async fn wait_closed(&self, count: usize) {
            self.wait_for(|| self.state.closed.load(Ordering::SeqCst) >= count)
                .await;
        }

        async fn wait_slots_released(&self) {
            self.wait_for(|| {
                self.manager.channel_semaphore.available_permits() == CHANNEL_SEMAPHORE_CAPACITY
            })
            .await;
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.state.stop.cancel();
            self.server.abort();
        }
    }

    #[tokio::test]
    async fn test_native_raw_exec_reuses_transport_without_pty_probes_or_elevation() {
        let fixture = Fixture::new(None).await;
        let command = r#":put "literal ' ; $() {}""#;
        for command in [command, "no-ack"] {
            let output = fixture.exec(command, Duration::from_secs(2)).await.unwrap();
            assert_eq!(output.stdout, "A🙂B");
            assert_eq!(output.stderr, "stderr");
            assert_eq!(output.exit_code, Some(0));
        }
        assert_eq!(
            *fixture.state.commands.lock().unwrap(),
            [command.as_bytes().to_vec(), b"no-ack".to_vec()]
        );
        assert_eq!(fixture.state.opened.load(Ordering::SeqCst), 2);
        assert_eq!(fixture.state.unexpected.load(Ordering::SeqCst), 0);
        assert!(!fixture.manager.is_elevated());
        assert!(fixture.manager.is_connected().await);
        fixture.manager.close().await;
    }

    #[tokio::test]
    async fn test_native_fake_peer_failure_orders_preserve_output_and_status() {
        let fixture = Fixture::new(None).await;
        for (command, stdout, code, may_have_run) in [
            ("reject", "before rejection", Some(0), false),
            ("no-status", "partial", None, true),
            ("signal", "after signal", None, true),
        ] {
            let failure = timeout(
                Duration::from_secs(1),
                fixture.exec(command, Duration::from_secs(5)),
            )
            .await
            .unwrap()
            .unwrap_err();
            assert_eq!(failure.output.stdout, stdout);
            assert_eq!(failure.output.exit_code, code);
            assert_eq!(failure.may_have_run, may_have_run);
        }
        assert_eq!(fixture.state.commands.lock().unwrap().len(), 3);
        assert!(fixture.manager.is_connected().await);
        fixture.manager.close().await;
    }

    #[tokio::test]
    async fn test_native_disconnect_without_close_is_error_even_after_zero_status() {
        let fixture = Fixture::new(None).await;
        let failure = fixture
            .exec("disconnect", Duration::from_secs(2))
            .await
            .unwrap_err();
        assert_eq!(failure.output.stdout, "before disconnect");
        assert_eq!(failure.output.exit_code, Some(0));
        assert!(failure.message.contains("without SSH Close"));
        assert!(failure.may_have_run);
        assert_eq!(fixture.state.commands.lock().unwrap().len(), 1);
        fixture.manager.close().await;
    }

    #[tokio::test]
    async fn test_native_timeout_and_cancellation_keep_partial_output_without_replay() {
        for cancel in [false, true] {
            let fixture = Fixture::new(None).await;
            let cancellation = CancellationToken::new();
            let request_token = cancellation.clone();
            let manager = fixture.manager.clone();
            let request = tokio::spawn(async move {
                manager
                    .exec_routeros_command(
                        "hang".to_string(),
                        Duration::from_millis(150),
                        request_token,
                    )
                    .await
            });
            fixture
                .wait_for(|| !fixture.state.commands.lock().unwrap().is_empty())
                .await;
            // Wait for the partial stream to reach the collector before cancellation.
            tokio::time::sleep(Duration::from_millis(30)).await;
            if cancel {
                cancellation.cancel();
            }
            let failure = timeout(Duration::from_secs(1), request)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err();
            assert_eq!(failure.output.stdout, "partial stdout");
            assert_eq!(failure.output.stderr, "partial stderr");
            assert_eq!(failure.output.exit_code, Some(0));
            assert!(failure.may_have_run);
            assert!(
                failure
                    .message
                    .contains(if cancel { "cancelled" } else { "deadline" })
            );
            fixture.wait_closed(1).await;
            fixture.wait_slots_released().await;
            assert!(fixture.manager.is_connected().await);
            fixture
                .exec(":put sibling", Duration::from_secs(2))
                .await
                .unwrap();
            assert_eq!(fixture.state.commands.lock().unwrap().len(), 2);
            fixture.manager.close().await;
        }
    }

    #[tokio::test]
    async fn test_native_dropped_request_closes_owned_channel_and_preserves_route() {
        let fixture = Fixture::new(None).await;
        let manager = fixture.manager.clone();
        let request = tokio::spawn(async move {
            manager
                .exec_routeros_command(
                    "hang".to_string(),
                    Duration::from_secs(5),
                    CancellationToken::new(),
                )
                .await
        });
        fixture
            .wait_for(|| !fixture.state.commands.lock().unwrap().is_empty())
            .await;
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        fixture.wait_closed(1).await;
        fixture.wait_slots_released().await;
        fixture
            .exec(":put still-healthy", Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(fixture.state.commands.lock().unwrap().len(), 2);
        fixture.manager.close().await;
    }

    #[tokio::test]
    async fn test_native_shutdown_cancels_owned_channel() {
        let fixture = Fixture::new(None).await;
        let manager = fixture.manager.clone();
        let request = tokio::spawn(async move {
            manager
                .exec_routeros_command(
                    "hang".to_string(),
                    Duration::from_secs(5),
                    CancellationToken::new(),
                )
                .await
        });
        fixture
            .wait_for(|| !fixture.state.commands.lock().unwrap().is_empty())
            .await;
        fixture.manager.close().await;
        let failure = timeout(Duration::from_secs(1), request)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(failure.message.contains("shutting down"));
        assert!(failure.may_have_run);
        fixture.wait_slots_released().await;
    }

    #[tokio::test]
    async fn test_native_slot_wait_is_inside_deadline_and_precancel_does_not_exec() {
        let fixture = Fixture::new(None).await;
        let slots = fixture
            .manager
            .channel_semaphore
            .clone()
            .acquire_many_owned(CHANNEL_SEMAPHORE_CAPACITY as u32)
            .await
            .unwrap();
        let failure = timeout(
            Duration::from_millis(500),
            fixture.exec(":put never", Duration::from_millis(30)),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(!failure.may_have_run);
        assert!(failure.message.contains("deadline"));
        drop(slots);
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let failure = fixture
            .manager
            .exec_routeros_command(
                ":put never".to_string(),
                Duration::from_secs(1),
                cancellation,
            )
            .await
            .unwrap_err();
        assert!(!failure.may_have_run);
        assert!(fixture.state.commands.lock().unwrap().is_empty());
        assert_eq!(fixture.state.opened.load(Ordering::SeqCst), 0);
        fixture.manager.close().await;
    }

    #[tokio::test]
    async fn test_native_late_open_is_closed_without_exec_and_sibling_survives() {
        for drop_request in [false, true] {
            let fixture = Fixture::new(None).await;
            let mut sibling = fixture.manager.open_channel().await.unwrap();
            fixture.state.delay_next_open.store(true, Ordering::SeqCst);
            let manager = fixture.manager.clone();
            let request = tokio::spawn(async move {
                manager
                    .exec_routeros_with_open_budget(
                        ":put never".to_string(),
                        Duration::from_millis(60),
                        CancellationToken::new(),
                        Duration::from_secs(2),
                    )
                    .await
            });
            fixture
                .wait_for(|| fixture.state.opened.load(Ordering::SeqCst) == 2)
                .await;
            if drop_request {
                request.abort();
                assert!(request.await.unwrap_err().is_cancelled());
            } else {
                let failure = timeout(Duration::from_millis(500), request)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap_err();
                assert!(!failure.may_have_run);
                assert!(failure.message.contains("deadline"));
            }
            assert_eq!(
                fixture.manager.channel_semaphore.available_permits(),
                CHANNEL_SEMAPHORE_CAPACITY - 1
            );
            fixture.state.open_gate.add_permits(1);
            fixture.wait_closed(1).await;
            fixture.wait_slots_released().await;
            assert!(fixture.state.commands.lock().unwrap().is_empty());
            assert!(fixture.manager.is_connected().await);
            sibling.exec(true, b":put sibling".to_vec()).await.unwrap();
            let output = timeout(Duration::from_secs(2), async {
                let mut collector = Collector::new(None);
                loop {
                    if let Some(result) = collector.event(sibling.wait().await) {
                        break collector.finish(result);
                    }
                }
            })
            .await
            .unwrap()
            .unwrap();
            assert_eq!(output.exit_code, Some(0));
            assert_eq!(
                *fixture.state.commands.lock().unwrap(),
                [b":put sibling".to_vec()]
            );
            fixture.manager.close().await;
        }
    }

    #[tokio::test]
    async fn test_native_open_rejection_does_not_retire_route() {
        let fixture = Fixture::new(None).await;
        fixture.state.reject_next_open.store(true, Ordering::SeqCst);
        let failure = fixture
            .exec(":put rejected-open", Duration::from_secs(2))
            .await
            .unwrap_err();
        assert!(!failure.may_have_run);
        assert!(fixture.state.commands.lock().unwrap().is_empty());
        assert!(fixture.manager.is_connected().await);
        fixture
            .exec(":put retained-route", Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(
            fixture
                .manager
                .session
                .lock()
                .await
                .as_ref()
                .unwrap()
                .generation,
            1
        );
        fixture.manager.close().await;
    }

    #[tokio::test]
    async fn test_native_unresolved_open_retires_generation_after_early_deadline_return() {
        let fixture = Fixture::new(None).await;
        fixture.state.delay_next_open.store(true, Ordering::SeqCst);
        let failure = timeout(
            Duration::from_millis(500),
            fixture.manager.exec_routeros_with_open_budget(
                ":put never".to_string(),
                Duration::from_millis(30),
                CancellationToken::new(),
                Duration::from_millis(120),
            ),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(!failure.may_have_run);
        assert_eq!(
            fixture.manager.channel_semaphore.available_permits(),
            CHANNEL_SEMAPHORE_CAPACITY - 1
        );
        fixture.wait_slots_released().await;
        assert!(fixture.manager.session.lock().await.is_none());
        assert!(fixture.state.commands.lock().unwrap().is_empty());
        fixture.state.open_gate.add_permits(1);
        fixture.manager.close().await;
    }

    #[tokio::test]
    async fn test_native_open_timeout_response_does_not_await_invalidation_locks() {
        let fixture = Fixture::new(None).await;
        fixture.state.delay_next_open.store(true, Ordering::SeqCst);
        let su_lock = fixture.manager.su_channel.lock().await;
        let failure = timeout(
            Duration::from_millis(500),
            fixture.manager.exec_routeros_with_open_budget(
                ":put never".to_string(),
                Duration::from_secs(2),
                CancellationToken::new(),
                Duration::from_millis(30),
            ),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(!failure.may_have_run);
        assert!(failure.message.contains("acquisition timed out"));
        assert!(fixture.manager.session.lock().await.is_some());
        drop(su_lock);
        fixture.wait_slots_released().await;
        assert!(fixture.manager.session.lock().await.is_none());
        fixture.state.open_gate.add_permits(1);
        fixture.manager.close().await;
    }

    #[tokio::test]
    async fn test_native_output_flood_is_capped_drained_and_cannot_starve_deadline() {
        let fixture = Fixture::new(Some(2)).await;
        let failure = timeout(
            Duration::from_secs(1),
            fixture.exec("flood", Duration::from_millis(100)),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(failure.message.contains("deadline"));
        assert_eq!(failure.output.stdout, "xxxxxxxx");
        assert!(failure.output.stdout_truncated);
        assert!(failure.output.stdout_total_tokens > 2);
        assert_eq!(failure.output.exit_code, Some(0));
        assert!(failure.may_have_run);
        fixture.wait_closed(1).await;
        fixture.state.stop.cancel();
        fixture.manager.close().await;
    }
}
