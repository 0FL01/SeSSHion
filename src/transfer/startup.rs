//! Frozen startup advice only; these results never select or disable runtime transports.
//!
//! `Ok` means a basic preflight succeeded, not that staging, permissions, or a
//! particular file/directory transfer will succeed.

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub(crate) enum StartupProbeStatus {
    Ok,
    Blocked,
    #[default]
    Unknown,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct StartupTransferPreflight {
    pub rsync: StartupProbeStatus,
    pub sftp: StartupProbeStatus,
    pub scp: StartupProbeStatus,
    pub exec_raw: StartupProbeStatus,
}

impl StartupTransferPreflight {
    /// Mirror the existing external-transport eligibility gates without I/O.
    pub(crate) fn for_options(options: &super::TransferSshOptions) -> Self {
        let blocked = options.key_path.is_none()
            || options
                .jump
                .as_ref()
                .is_some_and(|jump| !cfg!(unix) || jump.key_path.is_none());
        let external = if blocked {
            StartupProbeStatus::Blocked
        } else {
            StartupProbeStatus::Unknown
        };
        Self {
            rsync: external,
            sftp: external,
            scp: external,
            exec_raw: StartupProbeStatus::Unknown,
        }
    }

    pub(crate) fn hint(&self) -> String {
        use StartupProbeStatus::{Ok, Unknown};

        let transports = [
            ("rsync", self.rsync),
            ("sftp", self.sftp),
            ("scp", self.scp),
            ("exec-raw", self.exec_raw),
        ];
        if transports.iter().all(|(_, status)| *status == Unknown) {
            return "Transfer: startup preflight unknown; default auto.".to_string();
        }
        let preferred = if self.sftp == Ok {
            "sftp"
        } else if self.exec_raw == Ok {
            "exec-raw"
        } else {
            "auto"
        };
        let mut lists = Vec::new();
        for (label, status) in [
            ("ok", Ok),
            ("blocked", StartupProbeStatus::Blocked),
            ("unknown", Unknown),
        ] {
            let names: Vec<_> = transports
                .iter()
                .filter_map(|(name, value)| (*value == status).then_some(*name))
                .collect();
            if !names.is_empty() {
                lists.push(format!("{label}={}", names.join(",")));
            }
        }
        format!(
            "Transfer: prefer {preferred}; startup preflight (may be stale): {}.",
            lists.join("; ")
        )
    }
}

pub(crate) async fn probe_sftp(
    options: &super::TransferSshOptions,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> StartupProbeStatus {
    if StartupTransferPreflight::for_options(options).sftp == StartupProbeStatus::Blocked {
        return StartupProbeStatus::Blocked;
    }
    if cancellation.is_cancelled() || Instant::now() >= deadline {
        return StartupProbeStatus::Unknown;
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let Some(key_path) = &options.key_path else {
            return StartupProbeStatus::Blocked;
        };
        let endpoint = super::openssh::OpenSshEndpoint {
            host: options.host.clone(),
            port: options.port,
            user: options.user.clone(),
            key_path: key_path.clone(),
            host_key_checking: options.host_key_checking,
            known_hosts: options.known_hosts.clone(),
            jump: options.jump.clone(),
        };
        run_command(sftp_command(&endpoint), deadline, cancellation).await
    }
    // Skip platforms without this runner's non-reaping wait and group teardown.
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    StartupProbeStatus::Unknown
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn sftp_command(endpoint: &super::openssh::OpenSshEndpoint) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("sftp");
    command
        .arg("-P")
        .arg(endpoint.port.to_string())
        .args(super::openssh::common_ssh_options(endpoint))
        .args(["-b", "-"])
        .arg(format!("{}@{}", endpoint.user, endpoint.host))
        .env("LC_ALL", "C");
    command
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct ProbeChild {
    child: tokio::process::Child,
    // The leader stays owned and unreaped until this group has been signalled.
    group: rustix::process::Pid,
    kill_group_on_drop: bool,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl ProbeChild {
    fn spawn(command: &mut tokio::process::Command) -> std::io::Result<Self> {
        command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .process_group(0);
        let child = command.spawn()?;
        let group = child
            .id()
            .and_then(|id| i32::try_from(id).ok())
            .and_then(rustix::process::Pid::from_raw)
            .ok_or_else(|| std::io::Error::other("missing startup probe process group"))?;
        Ok(Self {
            child,
            group,
            kill_group_on_drop: true,
        })
    }

    fn kill(&mut self) {
        if self.kill_group_on_drop {
            let _ = rustix::process::kill_process_group(self.group, rustix::process::Signal::KILL);
            // Do not signal a potentially reused group ID after the reap await.
            self.kill_group_on_drop = false;
        }
        let _ = self.child.start_kill();
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Drop for ProbeChild {
    fn drop(&mut self) {
        // Also runs if the caller drops this future rather than cancelling its token.
        self.kill();
        // Child's kill_on_drop and Tokio's orphan reaper cover a deadline-expired
        // or dropped wait; no detached reader or cleanup task is started here.
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn run_command(
    mut command: tokio::process::Command,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> StartupProbeStatus {
    if cancellation.is_cancelled() || Instant::now() >= deadline {
        return StartupProbeStatus::Unknown;
    }
    let child = match ProbeChild::spawn(&mut command) {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tracing::debug!(?error, "Startup SFTP client unavailable");
            return StartupProbeStatus::Blocked;
        }
        Err(error) => {
            tracing::debug!(?error, "Startup SFTP client failed to start");
            return StartupProbeStatus::Unknown;
        }
    };
    match run_child(child, deadline, cancellation).await {
        Ok(status) if status.success() => StartupProbeStatus::Ok,
        _ => StartupProbeStatus::Unknown,
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn drain_output(
    mut pipe: impl tokio::io::AsyncRead + Unpin,
    captured: &mut Vec<u8>,
) -> std::io::Result<()> {
    use tokio::io::AsyncReadExt;

    // Retain only a bounded diagnostic prefix, but keep draining so the cap
    // cannot cause broken pipes. No detached reader or unbounded capture.
    let mut buffer = [0_u8; 4096];
    loop {
        let size = pipe.read(&mut buffer).await?;
        if size == 0 {
            break;
        }
        let take = size.min(buffer.len().saturating_sub(captured.len()));
        captured.extend_from_slice(&buffer[..take]);
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn wait_unreaped(group: rustix::process::Pid) -> std::io::Result<()> {
    use rustix::process::{WaitId, WaitIdOptions, waitid};

    // Observe exit without releasing the PID/PGID. Tokio wait/try_wait must not
    // run concurrently: otherwise cleanup could signal a reused numeric ID.
    loop {
        match waitid(
            WaitId::Pid(group),
            WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG,
        ) {
            Ok(Some(_)) => return Ok(()),
            Ok(None) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
            Err(rustix::io::Errno::INTR) => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn run_child(
    mut child: ProbeChild,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> std::io::Result<std::process::ExitStatus> {
    use tokio::io::AsyncWriteExt;

    // Leave time inside the shared budget to kill and reap the process group.
    let work_deadline = deadline - std::time::Duration::from_millis(50);

    let mut stdin = child
        .child
        .stdin
        .take()
        .ok_or_else(|| std::io::Error::other("missing startup probe stdin"))?;
    let stdout = child
        .child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("missing startup probe stdout"))?;
    let stderr = child
        .child
        .stderr
        .take()
        .ok_or_else(|| std::io::Error::other("missing startup probe stderr"))?;

    let mut stdout_prefix = Vec::with_capacity(4096);
    let mut stderr_prefix = Vec::with_capacity(4096);
    let result = {
        // These are inline futures: dropping the enclosing future closes all
        // pipes immediately, including readers left open by an exited parent.
        let work = async {
            tokio::try_join!(
                async move {
                    stdin.write_all(b"quit\n").await?;
                    stdin.shutdown().await
                },
                wait_unreaped(child.group),
                drain_output(stdout, &mut stdout_prefix),
                drain_output(stderr, &mut stderr_prefix),
            )?;
            Ok::<(), std::io::Error>(())
        };
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(std::io::Error::other("startup probe cancelled")),
            _ = tokio::time::sleep_until(work_deadline) => Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut, "startup probe deadline expired",
            )),
            result = work => result,
        }
    };

    // Kill while the leader is still unreaped, even when it exited first and a
    // descendant holds the pipes. Only then may Tokio release its PID/PGID.
    child.kill();
    let reaped = tokio::time::timeout_at(deadline, child.child.wait())
        .await
        .unwrap_or_else(|_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "startup probe reap deadline expired",
            ))
        });
    let result = result.and(reaped);
    if !result.as_ref().is_ok_and(|status| status.success()) {
        tracing::debug!(
            ?result,
            stderr = ?String::from_utf8_lossy(&stderr_prefix),
            "Startup SFTP preflight inconclusive",
        );
    }

    result
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::ssh::HostKeyCheckMode;

    use super::*;

    fn options() -> super::super::TransferSshOptions {
        super::super::TransferSshOptions {
            host: "target.example".to_string(),
            port: 2222,
            user: "target-user".to_string(),
            key_path: Some(PathBuf::from("/keys/target key")),
            host_key_checking: HostKeyCheckMode::AcceptNew,
            known_hosts: Some(PathBuf::from("/known hosts")),
            jump: None,
        }
    }

    fn jump(key_path: Option<PathBuf>) -> super::super::TransferJumpOptions {
        super::super::TransferJumpOptions {
            host: "jump.example".to_string(),
            port: 2200,
            user: "jump-user".to_string(),
            key_path,
        }
    }

    fn assert_external(preflight: &StartupTransferPreflight, status: StartupProbeStatus) {
        assert_eq!(preflight.rsync, status);
        assert_eq!(preflight.sftp, status);
        assert_eq!(preflight.scp, status);
        assert_eq!(preflight.exec_raw, StartupProbeStatus::Unknown);
    }

    #[test]
    fn hint_all_unknown_defaults_to_auto() {
        assert_eq!(
            StartupTransferPreflight::default().hint(),
            "Transfer: startup preflight unknown; default auto."
        );
    }

    #[test]
    fn hint_password_only_prefers_confirmed_raw() {
        let mut options = options();
        options.key_path = None;
        let mut preflight = StartupTransferPreflight::for_options(&options);
        assert_external(&preflight, StartupProbeStatus::Blocked);
        assert_eq!(
            preflight.hint(),
            "Transfer: prefer auto; startup preflight (may be stale): blocked=rsync,sftp,scp; unknown=exec-raw."
        );
        preflight.exec_raw = StartupProbeStatus::Ok;
        assert_eq!(
            preflight.hint(),
            "Transfer: prefer exec-raw; startup preflight (may be stale): ok=exec-raw; blocked=rsync,sftp,scp."
        );
    }

    #[test]
    fn hint_mixed_results_prioritize_sftp_and_keep_list_order() {
        let preflight = StartupTransferPreflight {
            rsync: StartupProbeStatus::Blocked,
            sftp: StartupProbeStatus::Ok,
            scp: StartupProbeStatus::Unknown,
            exec_raw: StartupProbeStatus::Ok,
        };
        assert_eq!(
            preflight.hint(),
            "Transfer: prefer sftp; startup preflight (may be stale): ok=sftp,exec-raw; blocked=rsync; unknown=scp."
        );
        let all_ok = StartupTransferPreflight {
            rsync: StartupProbeStatus::Ok,
            sftp: StartupProbeStatus::Ok,
            scp: StartupProbeStatus::Ok,
            exec_raw: StartupProbeStatus::Ok,
        };
        assert_eq!(
            all_ok.hint(),
            "Transfer: prefer sftp; startup preflight (may be stale): ok=rsync,sftp,scp,exec-raw."
        );
        assert!(preflight.hint().is_ascii());
    }

    #[test]
    fn eligibility_matches_target_and_jump_key_gates() {
        let mut options = options();
        assert_external(
            &StartupTransferPreflight::for_options(&options),
            StartupProbeStatus::Unknown,
        );
        options.jump = Some(jump(None));
        assert_external(
            &StartupTransferPreflight::for_options(&options),
            StartupProbeStatus::Blocked,
        );
        options.jump = Some(jump(Some(PathBuf::from("/keys/jump key"))));
        assert_external(
            &StartupTransferPreflight::for_options(&options),
            if cfg!(unix) {
                StartupProbeStatus::Unknown
            } else {
                StartupProbeStatus::Blocked
            },
        );
        options.key_path = None;
        assert_external(
            &StartupTransferPreflight::for_options(&options),
            StartupProbeStatus::Blocked,
        );
    }

    #[tokio::test]
    async fn probe_honors_gates_before_cancelled_or_expired_deadline() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let deadline = Instant::now();
        let mut options = options();
        assert_eq!(
            probe_sftp(&options, deadline, &cancellation).await,
            StartupProbeStatus::Unknown
        );
        options.key_path = None;
        assert_eq!(
            probe_sftp(&options, deadline, &cancellation).await,
            StartupProbeStatus::Blocked
        );
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    #[tokio::test]
    async fn unsupported_platform_without_jump_skips_external_probe() {
        assert_eq!(
            probe_sftp(
                &options(),
                Instant::now() + std::time::Duration::from_secs(1),
                &CancellationToken::new(),
            )
            .await,
            StartupProbeStatus::Unknown
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    mod lifecycle {
        use std::time::Duration;

        use tokio::process::Command;

        use super::*;

        fn shell(script: &str) -> Command {
            let mut command = Command::new("sh");
            command.args(["-c", script]);
            command
        }

        async fn assert_reaped(group: rustix::process::Pid) {
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    match rustix::process::test_kill_process(group) {
                        Err(rustix::io::Errno::SRCH) => return,
                        Ok(()) => tokio::time::sleep(Duration::from_millis(10)).await,
                        Err(error) => panic!("checking startup probe group: {error}"),
                    }
                }
            })
            .await
            .expect("startup probe parent must be reaped");
        }

        async fn read_descendant_pid(path: &std::path::Path) -> i32 {
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if let Ok(pid) = tokio::fs::read_to_string(path).await
                        && let Ok(pid) = pid.parse()
                    {
                        return pid;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("child must publish its descendant pid")
        }

        #[cfg(target_os = "linux")]
        async fn assert_descendant_stopped(pid: i32) {
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    match tokio::fs::read_to_string(format!("/proc/{pid}/stat")).await {
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
                        Ok(stat) => {
                            // A container's PID 1 may leave killed orphans as zombies.
                            let state = stat.rsplit_once(") ").expect("proc stat state").1;
                            if state.starts_with('Z') || state.starts_with('X') {
                                return;
                            }
                        }
                        Err(error) => panic!("checking descendant: {error}"),
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("startup probe descendant must be killed");
        }

        #[test]
        fn command_reuses_target_jump_and_accept_new_options() {
            let mut options = options();
            options.jump = Some(jump(Some(PathBuf::from("/keys/jump key"))));
            let endpoint = super::super::super::openssh::OpenSshEndpoint {
                host: options.host,
                port: options.port,
                user: options.user,
                key_path: options.key_path.expect("target key"),
                host_key_checking: options.host_key_checking,
                known_hosts: options.known_hosts,
                jump: options.jump,
            };
            let command = sftp_command(&endpoint);
            let mut expected = vec!["-P".to_string(), "2222".to_string()];
            expected.extend(super::super::super::openssh::common_ssh_options(&endpoint));
            expected.extend(["-b", "-", "target-user@target.example"].map(str::to_string));
            let args: Vec<_> = command
                .as_std()
                .get_args()
                .map(|arg| arg.to_str().expect("ASCII command argument").to_string())
                .collect();
            assert_eq!(args, expected);
            assert!(
                args.iter()
                    .any(|arg| arg == "StrictHostKeyChecking=accept-new")
            );
            assert!(
                args.iter()
                    .any(|arg| arg == "UserKnownHostsFile=/known hosts")
            );
            let proxy = args
                .iter()
                .find(|arg| arg.starts_with("ProxyCommand="))
                .expect("jump proxy");
            assert!(proxy.contains("-i '/keys/jump key'"));
            assert!(proxy.contains("-p 2200"));
            assert!(proxy.contains("-W 'target.example:2222'"));
            assert!(proxy.contains("StrictHostKeyChecking=accept-new"));
        }

        #[tokio::test]
        async fn quit_session_success_and_nonzero_results() {
            let cancellation = CancellationToken::new();
            assert_eq!(
                run_command(
                    shell("IFS= read -r line; [ \"$line\" = quit ] && ! IFS= read -r extra"),
                    Instant::now() + Duration::from_secs(2),
                    &cancellation,
                )
                .await,
                StartupProbeStatus::Ok
            );
            for script in [
                "IFS= read -r line; printf 'subsystem request failed\\n' >&2; exit 255",
                "IFS= read -r line; exit 1",
            ] {
                assert_eq!(
                    run_command(
                        shell(script),
                        Instant::now() + Duration::from_secs(2),
                        &cancellation
                    )
                    .await,
                    StartupProbeStatus::Unknown
                );
            }
        }

        #[tokio::test]
        async fn closed_stdin_is_unknown_and_reaped() {
            let directory = tempfile::tempdir().expect("isolated readiness directory");
            let ready_path = directory.path().join("ready");
            // Re-exec before publishing readiness so the first shell's saved
            // redirection descriptors cannot still keep the stdin pipe open.
            let mut command = shell(
                "exec 0<&-; exec sh -c 'printf %s \"$$\" > \"$1\"; exec sleep 30' probe \"$1\"",
            );
            command.arg("probe").arg(&ready_path);
            let child = ProbeChild::spawn(&mut command).expect("spawn closed stdin child");
            let group = child.group;
            read_descendant_pid(&ready_path).await;
            let result = run_child(
                child,
                Instant::now() + Duration::from_secs(2),
                &CancellationToken::new(),
            )
            .await;
            assert_eq!(
                result.expect_err("closed stdin must fail").kind(),
                std::io::ErrorKind::BrokenPipe
            );
            assert_reaped(group).await;
        }

        #[tokio::test]
        async fn missing_binary_is_blocked_but_spawn_errors_are_unknown() {
            let directory = tempfile::tempdir().expect("isolated command directory");
            let cancellation = CancellationToken::new();
            assert_eq!(
                run_command(
                    Command::new(directory.path().join("missing-sftp")),
                    Instant::now() + Duration::from_secs(2),
                    &cancellation,
                )
                .await,
                StartupProbeStatus::Blocked
            );
            assert_eq!(
                run_command(
                    Command::new(directory.path()),
                    Instant::now() + Duration::from_secs(2),
                    &cancellation,
                )
                .await,
                StartupProbeStatus::Unknown
            );
        }

        #[tokio::test]
        async fn cancelled_or_expired_command_does_not_spawn() {
            let directory = tempfile::tempdir().expect("isolated command directory");
            let cancellation = CancellationToken::new();
            assert_eq!(
                run_command(
                    Command::new(directory.path().join("missing-sftp")),
                    Instant::now(),
                    &cancellation,
                )
                .await,
                StartupProbeStatus::Unknown
            );
            cancellation.cancel();
            assert_eq!(
                run_command(
                    Command::new(directory.path().join("missing-sftp")),
                    Instant::now() + Duration::from_secs(2),
                    &cancellation,
                )
                .await,
                StartupProbeStatus::Unknown
            );
        }

        #[tokio::test]
        async fn flood_is_drained_with_bounded_memory_and_deadline() {
            let started = Instant::now();
            let mut command = shell(
                "IFS= read -r line; while :; do printf '%04096d' 0; printf '%04096d' 0 >&2; done",
            );
            let child = ProbeChild::spawn(&mut command).expect("spawn flooding child");
            let group = child.group;
            let result = run_child(
                child,
                started + Duration::from_millis(250),
                &CancellationToken::new(),
            )
            .await;
            assert_eq!(
                result.expect_err("flood must time out").kind(),
                std::io::ErrorKind::TimedOut
            );
            assert!(started.elapsed() < Duration::from_secs(2));
            assert_reaped(group).await;
        }

        #[tokio::test]
        async fn finite_output_above_buffer_cap_still_succeeds() {
            assert_eq!(
                run_command(
                    shell("IFS= read -r line; i=0; while [ \"$i\" -lt 32 ]; do printf '%04096d' 0; printf '%04096d' 0 >&2; i=$((i+1)); done"),
                    Instant::now() + Duration::from_secs(2),
                    &CancellationToken::new(),
                )
                .await,
                StartupProbeStatus::Ok
            );
        }

        #[tokio::test]
        async fn diagnostic_capture_retains_only_prefix_and_drains_the_rest() {
            let bytes = vec![b'x'; 16 * 1024];
            let mut input = bytes.as_slice();
            let mut captured = Vec::new();
            drain_output(&mut input, &mut captured).await.unwrap();
            assert!(input.is_empty());
            assert_eq!(captured, bytes[..4096]);
        }

        #[tokio::test]
        async fn exited_parent_with_inherited_pipes_keeps_deadline_and_group_cleanup() {
            let directory = tempfile::tempdir().expect("isolated descendant pid directory");
            let pid_path = directory.path().join("pid");
            let mut command =
                shell("IFS= read -r line; sleep 30 & printf '%s' \"$!\" > \"$1\"; exit 0");
            command.arg("probe").arg(&pid_path);
            let child = ProbeChild::spawn(&mut command).expect("spawn parent");
            let group = child.group;
            let started = Instant::now();
            let result = run_child(
                child,
                started + Duration::from_millis(250),
                &CancellationToken::new(),
            )
            .await;
            assert_eq!(
                result.expect_err("inherited pipes must time out").kind(),
                std::io::ErrorKind::TimedOut
            );
            assert!(started.elapsed() < Duration::from_secs(2));
            assert_reaped(group).await;
            let descendant = read_descendant_pid(&pid_path).await;
            #[cfg(target_os = "linux")]
            assert_descendant_stopped(descendant).await;
            #[cfg(not(target_os = "linux"))]
            let _ = descendant;
        }

        #[tokio::test]
        async fn exited_leader_remains_waitable_until_group_cleanup() {
            use rustix::process::{WaitId, WaitIdOptions, waitid};

            let mut command = shell("IFS= read -r line; sleep 30 & exit 0");
            let child = ProbeChild::spawn(&mut command).expect("spawn exiting leader");
            let group = child.group;
            let cancellation = CancellationToken::new();
            let (result, ()) = tokio::join!(
                run_child(
                    child,
                    Instant::now() + Duration::from_secs(2),
                    &cancellation,
                ),
                async {
                    tokio::time::timeout(Duration::from_secs(1), async {
                        loop {
                            if waitid(
                                WaitId::Pid(group),
                                WaitIdOptions::EXITED
                                    | WaitIdOptions::NOWAIT
                                    | WaitIdOptions::NOHANG,
                            )
                            .expect("the group leader must not be reaped while pipes are open")
                            .is_some()
                            {
                                break;
                            }
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                    })
                    .await
                    .expect("leader must exit while its descendant retains pipes");
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    assert!(
                        waitid(
                            WaitId::Pid(group),
                            WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG,
                        )
                        .expect("cleanup must still own the exited leader's PID/PGID")
                        .is_some()
                    );
                    cancellation.cancel();
                },
            );
            assert!(
                result
                    .expect_err("cancelled probe must fail")
                    .to_string()
                    .contains("cancelled")
            );
            assert_reaped(group).await;
        }

        #[tokio::test]
        async fn token_cancellation_kills_and_reaps_parent() {
            let mut command = shell("exec sleep 30");
            let child = ProbeChild::spawn(&mut command).expect("spawn cancellable child");
            let group = child.group;
            let cancellation = CancellationToken::new();
            let started = Instant::now();
            let (result, ()) = tokio::join!(
                run_child(child, started + Duration::from_secs(2), &cancellation),
                async {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    cancellation.cancel();
                },
            );
            assert!(
                result
                    .expect_err("cancelled child must fail")
                    .to_string()
                    .contains("cancelled")
            );
            assert!(started.elapsed() < Duration::from_secs(1));
            assert_reaped(group).await;
        }

        #[tokio::test]
        async fn dropping_future_kills_parent_and_descendant() {
            let directory = tempfile::tempdir().expect("isolated descendant pid directory");
            let pid_path = directory.path().join("pid");
            let mut command = shell("sleep 30 & printf '%s' \"$!\" > \"$1\"; wait");
            command.arg("probe").arg(&pid_path);
            let child = ProbeChild::spawn(&mut command).expect("spawn droppable child");
            let group = child.group;
            let descendant = read_descendant_pid(&pid_path).await;
            let cancellation = CancellationToken::new();
            let result = tokio::time::timeout(
                Duration::from_millis(50),
                run_child(
                    child,
                    Instant::now() + Duration::from_secs(30),
                    &cancellation,
                ),
            )
            .await;
            assert!(result.is_err(), "outer timeout must drop the probe future");
            assert_reaped(group).await;
            #[cfg(target_os = "linux")]
            assert_descendant_stopped(descendant).await;
            #[cfg(not(target_os = "linux"))]
            let _ = descendant;
        }
    }
}
