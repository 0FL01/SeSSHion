//! Bounded, optional metadata from an ordinary remote POSIX shell.
//! No probe values are ever evaluated as shell code or added to MCP discovery.

use std::time::Duration;

use russh::{Channel, ChannelMsg, client};
use serde::Serialize;
use tokio::time::{Instant, timeout_at};
use tokio_util::sync::CancellationToken;

use super::{SshConnectionManager, sanitize::wrap_in_posix_shell};
use crate::error::{Result, SshMcpError};

const COLLECTION_BUDGET: Duration = Duration::from_secs(3);
const CLEANUP_RESERVE: Duration = Duration::from_millis(50);
const STDOUT_LIMIT: usize = 64 * 1024;
const STDERR_LIMIT: usize = 4 * 1024;
const SCALAR_LIMIT: usize = 1024;
const FILE_LIMIT: usize = 16 * 1024;
const PATH_LIMIT: usize = 4096;

/// A snapshot of the SSH user's probe, including its namespaces and rootfs.
/// CPU parallelism is an estimate; process fields describe this probe's `sh`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct HostEnvironment {
    pub hostname: Option<String>,
    pub os: Option<String>,
    pub distribution: Option<String>,
    pub kernel_release: Option<String>,
    pub machine_architecture: Option<String>,
    pub process_architecture: Option<String>,
    pub pointer_width: Option<u16>,
    pub available_cpu_parallelism: Option<u64>,
    pub effective_uid: Option<u32>,
    pub effective_gid: Option<u32>,
    pub running_as_root: Option<bool>,
    pub shell_executable: Option<String>,
}

// Each text record is id NUL payload NUL exit-status NUL. ELF is last and has
// exactly 64 binary payload bytes, so embedded NULs cannot break its framing.
// A final builtin keeps $$ the probe shell's PID (no last-command exec shortcut).
const PROBE: &str = r#"
LC_ALL=C; export LC_ALL
unset OMP_NUM_THREADS OMP_THREAD_LIMIT
record() {
    key=$1; shift
    printf '%s\000' "$key"
    "$@" 2>/dev/null
    rc=$?
    printf '\000%s\000' "$rc"
}
release_file() {
    if [ -e /etc/os-release ] || [ -L /etc/os-release ]; then
        head -c 16385 /etc/os-release
    else
        head -c 16385 /usr/lib/os-release
    fi
}
printf 'SE1\000'
record hostname uname -n
record hostname_proc head -c 1025 /proc/sys/kernel/hostname
record os uname -s
record os_proc head -c 1025 /proc/sys/kernel/ostype
record distribution release_file
record kernel uname -r
record kernel_proc head -c 1025 /proc/sys/kernel/osrelease
record machine uname -m
record uid id -u
record gid id -g
record status head -c 16385 "/proc/$$/status"
record shell readlink "/proc/$$/exe"
record cpu nproc
record elf dd if="/proc/$$/exe" bs=64 count=1
printf 'done\000\0000\000'
"#;

// Even dropping the request future closes only its channel, never the route.
// russh's ChannelStream supplies the library's best-effort close-on-drop guard.
struct ProbeChannel(Option<Channel<client::Msg>>);

impl Drop for ProbeChannel {
    fn drop(&mut self) {
        if let Some(channel) = self.0.take() {
            drop(channel.into_stream());
        }
    }
}

impl SshConnectionManager {
    /// Collect or reuse metadata without initiating su/sudo, even on cold connect.
    pub async fn host_environment(
        &self,
        refresh: bool,
        cancellation: CancellationToken,
    ) -> Result<HostEnvironment> {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(SshMcpError::connection("Host environment request cancelled")),
            _ = self.shutdown_token.cancelled() => Err(SshMcpError::connection("SSH connection manager is shutting down")),
            result = self.host_environment_inner(refresh, &cancellation) => result,
        }
    }

    async fn host_environment_inner(
        &self,
        refresh: bool,
        cancellation: &CancellationToken,
    ) -> Result<HostEnvironment> {
        // Existing establishment/health/retry budgets apply only to this phase.
        self.ensure_connected_transport_only().await?;
        let deadline = Instant::now() + COLLECTION_BUDGET;
        timeout_at(
            deadline,
            self.host_environment_on_route(refresh, deadline, cancellation),
        )
        .await
        .map_err(|_| SshMcpError::Timeout(COLLECTION_BUDGET.as_millis() as u64))?
    }

    async fn host_environment_on_route(
        &self,
        refresh: bool,
        deadline: Instant,
        cancellation: &CancellationToken,
    ) -> Result<HostEnvironment> {
        let (generation, cached) = self.environment_cache().await?;
        if !refresh && let Some(cached) = cached {
            return Ok(cached);
        }

        let _gate = timeout_at(deadline, self.environment_gate.lock())
            .await
            .map_err(|_| SshMcpError::Timeout(COLLECTION_BUDGET.as_millis() as u64))?;
        let (current, cached) = self.environment_cache().await?;
        if current != generation {
            return Err(SshMcpError::connection(
                "SSH route changed during environment collection",
            ));
        }
        if !refresh && let Some(cached) = cached {
            return Ok(cached);
        }

        let _permit = timeout_at(deadline, self.acquire_command_slot())
            .await
            .map_err(|_| SshMcpError::Timeout(COLLECTION_BUDGET.as_millis() as u64))??;
        let (opened_generation, channel) =
            timeout_at(deadline, self.open_channel_with_generation())
                .await
                .map_err(|_| SshMcpError::Timeout(COLLECTION_BUDGET.as_millis() as u64))??;
        let mut channel = ProbeChannel(Some(channel));
        if opened_generation != generation {
            return Err(SshMcpError::connection(
                "SSH route changed during environment collection",
            ));
        }
        let snapshot = collect(
            channel.0.as_mut().expect("live probe channel"),
            PROBE,
            deadline,
        )
        .await?;
        self.publish_environment(generation, snapshot, cancellation)
            .await
    }

    async fn environment_cache(&self) -> Result<(u64, Option<HostEnvironment>)> {
        let session = self.session.lock().await;
        let route = session
            .as_ref()
            .filter(|route| !route.target.is_closed())
            .ok_or_else(|| SshMcpError::connection("SSH connection not established or closed"))?;
        if self.is_shutting_down() {
            return Err(SshMcpError::connection(
                "SSH connection manager is shutting down",
            ));
        }
        Ok((route.generation, route.environment.clone()))
    }

    async fn publish_environment(
        &self,
        generation: u64,
        snapshot: HostEnvironment,
        cancellation: &CancellationToken,
    ) -> Result<HostEnvironment> {
        let mut session = self.session.lock().await;
        // Cancellation can occur while a ready channel loop/parser is being polled,
        // without another select! poll. Recheck at the cache commit boundary.
        if cancellation.is_cancelled() {
            return Err(SshMcpError::connection(
                "Host environment request cancelled",
            ));
        }
        let route = session
            .as_mut()
            .filter(|route| {
                route.generation == generation
                    && !route.target.is_closed()
                    && !self.is_shutting_down()
            })
            .ok_or_else(|| {
                SshMcpError::connection("SSH route changed during environment collection")
            })?;
        route.environment = Some(snapshot.clone());
        Ok(snapshot)
    }
}

async fn collect(
    channel: &mut Channel<client::Msg>,
    script: &str,
    deadline: Instant,
) -> Result<HostEnvironment> {
    let mut stdout = Vec::with_capacity(STDOUT_LIMIT);
    let mut stderr_bytes = 0usize;
    let operation = async {
        channel
            .exec(true, wrap_in_posix_shell(script, false))
            .await
            .map_err(|error| {
                SshMcpError::connection(format!("Environment exec failed: {error}"))
            })?;
        loop {
            match channel.wait().await {
                Some(ChannelMsg::Data { data }) => {
                    if append_bounded(&mut stdout, &data, STDOUT_LIMIT) {
                        break;
                    }
                }
                Some(ChannelMsg::ExtendedData { data, .. }) => {
                    stderr_bytes = stderr_bytes.saturating_add(data.len());
                    if stderr_bytes > STDERR_LIMIT {
                        break;
                    }
                }
                Some(ChannelMsg::Failure) => break,
                Some(ChannelMsg::Close) | None => break,
                _ => {}
            }
        }
        Ok::<(), SshMcpError>(())
    };
    // Local deadline/overflow only produce unknown fields, not route invalidation.
    let result = timeout_at(deadline - CLEANUP_RESERVE, operation).await;
    let _ = timeout_at(deadline, channel.close()).await;
    if let Ok(result) = result {
        result?;
    }
    Ok(parse_snapshot(&stdout))
}

/// Copy at most the remaining capacity; return true only if bytes were discarded.
fn append_bounded(output: &mut Vec<u8>, data: &[u8], limit: usize) -> bool {
    let remaining = limit.saturating_sub(output.len());
    output.extend_from_slice(&data[..data.len().min(remaining)]);
    data.len() > remaining
}

fn nul_value<'a>(input: &mut &'a [u8]) -> Option<&'a [u8]> {
    let end = input.iter().position(|byte| *byte == 0)?;
    let value = &input[..end];
    *input = &input[end + 1..];
    Some(value)
}

fn parse_snapshot(raw: &[u8]) -> HostEnvironment {
    let mut snapshot = HostEnvironment::default();
    let Some(mut input) = raw.strip_prefix(b"SE1\0") else {
        return snapshot;
    };
    // Fixed order and ids: never search arbitrary output for a marker or resync
    // across a truncated/malformed record. Earlier completed records survive.
    for id in [
        "hostname",
        "hostname_proc",
        "os",
        "os_proc",
        "distribution",
        "kernel",
        "kernel_proc",
        "machine",
        "uid",
        "gid",
        "status",
        "shell",
        "cpu",
        "elf",
    ] {
        if nul_value(&mut input) != Some(id.as_bytes()) {
            break;
        }
        let payload = if id == "elf" {
            if input.len() < 65 || input[64] != 0 {
                break;
            }
            let payload = &input[..64];
            input = &input[65..];
            payload
        } else {
            let Some(payload) = nul_value(&mut input) else {
                break;
            };
            payload
        };
        let Some(status) = nul_value(&mut input) else {
            break;
        };
        if status != b"0" {
            continue;
        }
        match id {
            "hostname" | "hostname_proc" if snapshot.hostname.is_none() => {
                snapshot.hostname = scalar(payload, SCALAR_LIMIT)
            }
            "os" | "os_proc" if snapshot.os.is_none() => {
                snapshot.os = scalar(payload, SCALAR_LIMIT)
            }
            "kernel" | "kernel_proc" if snapshot.kernel_release.is_none() => {
                snapshot.kernel_release = scalar(payload, SCALAR_LIMIT)
            }
            "distribution" if payload.len() <= FILE_LIMIT => {
                snapshot.distribution = distribution(payload)
            }
            "machine" => snapshot.machine_architecture = scalar(payload, SCALAR_LIMIT),
            "uid" => snapshot.effective_uid = decimal(payload).and_then(|id| id.try_into().ok()),
            "gid" => snapshot.effective_gid = decimal(payload).and_then(|id| id.try_into().ok()),
            "status" if payload.len() <= FILE_LIMIT => {
                snapshot.effective_uid = snapshot
                    .effective_uid
                    .or_else(|| effective_id(payload, "Uid:"));
                snapshot.effective_gid = snapshot
                    .effective_gid
                    .or_else(|| effective_id(payload, "Gid:"));
            }
            "shell" => {
                snapshot.shell_executable =
                    scalar(payload, PATH_LIMIT).filter(|path| path.starts_with('/'))
            }
            "cpu" => {
                snapshot.available_cpu_parallelism = decimal(payload).filter(|count| *count > 0)
            }
            "elf" => {
                if let Some((architecture, width)) = elf_abi(payload) {
                    snapshot.process_architecture = Some(architecture.to_owned());
                    snapshot.pointer_width = Some(width);
                }
            }
            _ => {}
        }
    }
    snapshot.running_as_root = snapshot.effective_uid.map(|uid| uid == 0);
    snapshot
}

fn scalar(payload: &[u8], limit: usize) -> Option<String> {
    if payload.len() > limit {
        return None;
    }
    let text = std::str::from_utf8(payload).ok()?.trim();
    (!text.is_empty() && !text.chars().any(char::is_control)).then(|| text.to_owned())
}

fn decimal(payload: &[u8]) -> Option<u64> {
    if payload.len() > SCALAR_LIMIT {
        return None;
    }
    let text = std::str::from_utf8(payload).ok()?.trim();
    (!text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| text.parse().ok())
        .flatten()
}

fn effective_id(payload: &[u8], label: &str) -> Option<u32> {
    let line = std::str::from_utf8(payload)
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix(label))?;
    let ids = line
        .split_ascii_whitespace()
        .map(|value| decimal(value.as_bytes()).and_then(|id| u32::try_from(id).ok()))
        .collect::<Option<Vec<_>>>()?;
    (ids.len() == 4).then(|| ids[1])
}

fn distribution(payload: &[u8]) -> Option<String> {
    let mut fields = std::collections::BTreeMap::new();
    for line in std::str::from_utf8(payload).ok()?.lines() {
        let Some((key, value)) = line.trim().split_once('=') else {
            continue;
        };
        if ["PRETTY_NAME", "NAME", "VERSION", "VERSION_ID", "ID"].contains(&key) {
            // Last assignment wins, including a malformed/empty last value.
            fields.insert(key, release_value(value).filter(|value| !value.is_empty()));
        }
    }
    let field = |key| fields.get(key).and_then(Option::as_ref);
    let result = if let Some(pretty) = field("PRETTY_NAME") {
        pretty.clone()
    } else if let Some(name) = field("NAME") {
        if let Some(version) = field("VERSION").or_else(|| field("VERSION_ID")) {
            format!("{name} {version}")
        } else {
            name.clone()
        }
    } else {
        field("ID")?.clone()
    };
    scalar(result.as_bytes(), SCALAR_LIMIT)
}

// Parse assignment quoting/escapes as data, with no expansion, source or eval.
fn release_value(value: &str) -> Option<String> {
    let value = value.trim();
    let quote = value
        .chars()
        .next()
        .filter(|quote| *quote == '\'' || *quote == '"');
    let body = if quote.is_some() { &value[1..] } else { value };
    let mut chars = body.char_indices();
    let mut result = String::new();
    while let Some((index, character)) = chars.next() {
        if Some(character) == quote {
            let rest = body[index + 1..].trim_start();
            return (rest.is_empty() || rest.starts_with('#')).then_some(result);
        }
        if character == '\\' && quote != Some('\'') {
            let (_, next) = chars.next()?;
            if quote == Some('"') && !['$', '`', '"', '\\'].contains(&next) {
                result.push('\\');
            }
            result.push(next);
        } else if quote.is_some()
            || character.is_ascii_alphanumeric()
            || "-_.:/+@".contains(character)
        {
            // Unescaped expansion syntax is not part of os-release's grammar.
            if quote == Some('"') && (character == '$' || character == '`') {
                return None;
            }
            result.push(character);
        } else if character.is_ascii_whitespace() {
            let rest = body[index..].trim_start();
            return (quote.is_none() && (rest.is_empty() || rest.starts_with('#')))
                .then_some(result);
        } else {
            return None;
        }
    }
    quote.is_none().then_some(result)
}

fn elf_abi(header: &[u8]) -> Option<(&'static str, u16)> {
    if header.len() != 64
        || &header[..4] != b"\x7fELF"
        || header[6] != 1
        || ![0, 3].contains(&header[7])
    {
        return None;
    }
    let class = header[4];
    let endian = header[5];
    let read_u16 = |offset| match endian {
        1 => Some(u16::from_le_bytes([header[offset], header[offset + 1]])),
        2 => Some(u16::from_be_bytes([header[offset], header[offset + 1]])),
        _ => None,
    };
    let version = match endian {
        1 => u32::from_le_bytes(header[20..24].try_into().ok()?),
        2 => u32::from_be_bytes(header[20..24].try_into().ok()?),
        _ => return None,
    };
    if version != 1 || ![2, 3].contains(&read_u16(16)?) {
        return None;
    }
    match (read_u16(18)?, class, endian) {
        (3, 1, 1) => Some(("x86", 32)),
        (62, 1, 1) => Some(("x86_64", 32)), // Linux x32 ABI
        (62, 2, 1) => Some(("x86_64", 64)),
        (40, 1, 1 | 2) => Some(("arm", 32)),
        (183, 2, 1 | 2) => Some(("aarch64", 64)),
        (20, 1, 1 | 2) => Some(("powerpc", 32)),
        (21, 2, 1 | 2) => Some(("powerpc64", 64)),
        (22, 1, 2) => Some(("s390", 32)),
        (22, 2, 2) => Some(("s390x", 64)),
        (243, 1, 1 | 2) => Some(("riscv32", 32)),
        (243, 2, 1 | 2) => Some(("riscv64", 64)),
        (258, 2, 1) => Some(("loongarch64", 64)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknowns_are_null_and_root_is_not_invented() {
        let value = serde_json::to_value(parse_snapshot(b"malformed")).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 12);
        assert!(
            value
                .as_object()
                .unwrap()
                .values()
                .all(serde_json::Value::is_null)
        );
    }

    #[test]
    fn bounded_capture_retains_only_completed_records() {
        let prefix = b"SE1\0hostname\0host\0";
        let mut bytes = Vec::new();
        for chunk in prefix.chunks(2) {
            assert!(!append_bounded(&mut bytes, chunk, 100));
        }
        assert!(parse_snapshot(&bytes).hostname.is_none());
        assert!(!append_bounded(&mut bytes, b"0\0", 100));
        let expected = parse_snapshot(&bytes);
        assert_eq!(expected.hostname.as_deref(), Some("host"));
        assert!(append_bounded(&mut bytes, &[b'x'; 200], 100));
        assert_eq!(bytes.len(), 100);
        assert_eq!(parse_snapshot(&bytes), expected);
        assert!(
            parse_snapshot(b"noiseSE1\0hostname\0host\x00\x30\0")
                .hostname
                .is_none()
        );
        assert!(
            parse_snapshot(b"SE1\0hostname\0host\x00\x31\0")
                .hostname
                .is_none()
        );
    }

    #[test]
    fn release_parsing_is_data_only_and_last_assignment_wins() {
        assert_eq!(
            distribution(b"PRETTY_NAME=\"Odd \\\"Linux\\\"\"\n"),
            Some("Odd \"Linux\"".into())
        );
        assert_eq!(
            distribution(b"NAME='Odd Linux'\nVERSION_ID=42\n"),
            Some("Odd Linux 42".into())
        );
        assert_eq!(
            distribution(b"ID=odd\nNAME=\"unterminated\n"),
            Some("odd".into())
        );
        assert_eq!(
            distribution(b"PRETTY_NAME=Old\nPRETTY_NAME=\nID=new\n"),
            Some("new".into())
        );
        assert_eq!(
            release_value("'$(touch /not-executed)'"),
            Some("$(touch /not-executed)".into())
        );
        assert_eq!(
            release_value("\"\\$distro\" # comment"),
            Some("$distro".into())
        );
        assert!(release_value("\"$EXPANSION\"").is_none());
        assert!(release_value("two words").is_none());
        assert!(distribution(b"\xff").is_none());
    }

    #[test]
    fn numeric_and_effective_ids_are_strict() {
        assert_eq!(effective_id(b"Uid:\t11\t22\t33\t44\n", "Uid:"), Some(22));
        assert!(effective_id(b"Uid: 1 2 3\n", "Uid:").is_none());
        for invalid in ["-1", "+1", "1.0", "0x10", "4294967296"] {
            assert!(
                decimal(invalid.as_bytes())
                    .and_then(|id| u32::try_from(id).ok())
                    .is_none()
            );
        }
        assert_eq!(decimal(b"0\n"), Some(0));
        assert!(scalar(&vec![b'x'; SCALAR_LIMIT + 1], SCALAR_LIMIT).is_none());
        assert!(scalar(b"multi\nline", SCALAR_LIMIT).is_none());
    }

    #[test]
    fn elf_allowlist_handles_width_endianness_and_unknown_abis() {
        let mut header = [0; 64];
        header[..4].copy_from_slice(b"\x7fELF");
        header[4..7].copy_from_slice(&[2, 1, 1]);
        header[16] = 3;
        header[18] = 62;
        header[20] = 1;
        assert_eq!(elf_abi(&header), Some(("x86_64", 64)));
        header[4] = 1;
        assert_eq!(elf_abi(&header), Some(("x86_64", 32)));
        header[18] = 255;
        assert!(elf_abi(&header).is_none());
        header[4] = 2;
        header[5] = 2;
        header[16] = 0;
        header[17] = 3;
        header[18] = 0;
        header[19] = 22;
        header[20] = 0;
        header[23] = 1;
        assert_eq!(elf_abi(&header), Some(("s390x", 64)));
        assert!(elf_abi(&header[..63]).is_none());
    }

    #[test]
    #[cfg(unix)]
    fn fixed_probe_runs_in_a_real_non_login_posix_shell() {
        let output = std::process::Command::new("sh")
            .args(["-c", PROBE])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(output.stdout.len() < STDOUT_LIMIT);
        let snapshot = parse_snapshot(&output.stdout);
        assert!(snapshot.hostname.is_some());
        assert!(snapshot.effective_uid.is_some());
        assert_eq!(
            snapshot.running_as_root,
            snapshot.effective_uid.map(|uid| uid == 0)
        );
        #[cfg(target_os = "linux")]
        {
            assert!(snapshot.shell_executable.is_some());
            assert!(snapshot.process_architecture.is_some());
            assert!(snapshot.pointer_width.is_some());
        }
    }

    #[test]
    #[cfg(unix)]
    fn host_environment_real_busybox_probe_smoke() {
        // Genuine Alpine/BusyBox, unlike the existing Alpine-named Debian test.
        let output = std::process::Command::new("docker")
            .args(["run", "--rm", "alpine:latest", "sh", "-c", PROBE])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let snapshot = parse_snapshot(&output.stdout);
        assert_eq!(snapshot.os.as_deref(), Some("Linux"));
        assert!(snapshot.distribution.as_deref().unwrap().contains("Alpine"));
        assert_eq!(snapshot.effective_uid, Some(0));
        assert_eq!(snapshot.running_as_root, Some(true));
        assert!(
            snapshot
                .shell_executable
                .as_deref()
                .unwrap()
                .contains("busybox")
        );
        assert!(snapshot.pointer_width.is_some());
        assert!(snapshot.available_cpu_parallelism.is_some());
    }

    #[tokio::test]
    async fn host_environment_cancellation_covers_connect_and_releases_owner() {
        use super::super::{HostKeyCheckMode, SshConfig};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let manager = std::sync::Arc::new(
            SshConnectionManager::new(
                SshConfig::new("127.0.0.1", "test")
                    .with_port(listener.local_addr().unwrap().port())
                    .with_password("fixture-only")
                    .with_host_key_checking(HostKeyCheckMode::No),
            )
            .await,
        );
        for _ in 0..2 {
            let cancellation = CancellationToken::new();
            let call = {
                let manager = manager.clone();
                let cancellation = cancellation.clone();
                tokio::spawn(async move { manager.host_environment(false, cancellation).await })
            };
            // A real TCP connection with no SSH greeting stalls establishment.
            let (_socket, _) = tokio::time::timeout(Duration::from_secs(1), listener.accept())
                .await
                .unwrap()
                .unwrap();
            cancellation.cancel();
            assert!(
                tokio::time::timeout(Duration::from_secs(1), call)
                    .await
                    .unwrap()
                    .unwrap()
                    .is_err()
            );
            assert!(!manager.is_connected().await);
        }
        manager.close().await;
    }

    #[tokio::test]
    async fn cancelled_publication_rechecks_token_after_route_lock() {
        let manager =
            SshConnectionManager::new(super::super::SshConfig::new("unused.test", "test")).await;
        let route_guard = manager.session.lock().await;
        let cancellation = CancellationToken::new();
        let mut publication = std::pin::pin!(manager.publish_environment(
            1,
            HostEnvironment::default(),
            &cancellation
        ));
        // Deterministically suspend the publisher at the commit lock, then cancel.
        std::future::poll_fn(|context| {
            assert!(std::future::Future::poll(publication.as_mut(), context).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        cancellation.cancel();
        drop(route_guard);
        assert!(
            matches!(publication.await, Err(SshMcpError::Connection(message))
            if message == "Host environment request cancelled")
        );
        assert!(manager.session.lock().await.is_none());
        manager.close().await;
    }
}
