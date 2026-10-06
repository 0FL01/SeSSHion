//! Bounded, optional metadata from an ordinary remote POSIX shell.
//! Probe values are data, never evaluated as shell code.

use std::time::Duration;

use russh::{Channel, ChannelMsg, client};
use serde::Serialize;
use tokio::time::{Instant, timeout_at};
use tokio_util::sync::CancellationToken;

use super::{SshConnectionManager, sanitize::wrap_in_posix_shell};
use crate::error::{Result, SshMcpError};

pub(crate) const BOOTSTRAP_BUDGET: Duration = Duration::from_secs(3);
const CLEANUP_RESERVE: Duration = Duration::from_millis(50);
const STDOUT_LIMIT: usize = 64 * 1024;
const STDERR_LIMIT: usize = 4 * 1024;
const SCALAR_LIMIT: usize = 1024;
const FILE_LIMIT: usize = 16 * 1024;
const PATH_LIMIT: usize = 4096;
const CPU_MODEL_LIMIT: usize = 256;
const CPU_MODEL_COUNT: usize = 4;

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
    pub cpu_models: Option<Vec<String>>,
    pub virtualization: Virtualization,
}

/// Positive observed evidence, not an absence or guest-role assertion.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Virtualization {
    pub container: Option<String>,
    pub vm: Option<String>,
}

// Each text record is id NUL payload NUL exit-status NUL. ELF is last and has
// exactly 64 binary payload bytes, so embedded NULs cannot break its framing.
// A final builtin keeps $$ the probe shell's PID (no last-command exec shortcut).
const PROBE: &str = r#"
LC_ALL=C; export LC_ALL
unset OMP_NUM_THREADS OMP_THREAD_LIMIT
set -f
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
cpu_info() {
    model1=; model2=; model3=; model4=; hypervisor=
    lines=0; bytes=0
    # read consumes a whole line; these are post-read processing limits.
    { while IFS= read -r line || [ -n "$line" ]; do
        lines=$((lines + 1)); bytes=$((bytes + ${#line} + 1))
        [ "$lines" -le 128 ] && [ "$bytes" -le 65536 ] || break
        case "$line" in *:*) ;; *) continue;; esac
        cpu_key=${line%%:*}; value=${line#*:}
        cpu_key=${cpu_key#"${cpu_key%%[![:space:]]*}"}
        cpu_key=${cpu_key%"${cpu_key##*[![:space:]]}"}
        value=${value#"${value%%[![:space:]]*}"}
        value=${value%"${value##*[![:space:]]}"}
        case "$cpu_key" in
            'model name')
                [ -n "$value" ] && [ "${#value}" -le 256 ] || continue
                case "$value" in *[[:cntrl:]]*) continue;; esac
                [ "$value" != "$model1" ] && [ "$value" != "$model2" ] &&
                    [ "$value" != "$model3" ] && [ "$value" != "$model4" ] || continue
                if [ -z "$model1" ]; then model1=$value
                elif [ -z "$model2" ]; then model2=$value
                elif [ -z "$model3" ]; then model3=$value
                elif [ -z "$model4" ]; then model4=$value
                fi;;
            flags)
                for flag in $value; do
                    [ "$flag" != hypervisor ] || hypervisor=1
                done;;
        esac
    done; } 2>/dev/null < /proc/cpuinfo
}
scalar_file() {
    # These proc/sys/runtime scalars have one line; reject extra lines or overflow.
    value=; extra=
    {
        IFS= read -r value || [ -n "$value" ] || return 1
        [ "${#value}" -le 1024 ] || return 1
        if IFS= read -r extra || [ -n "$extra" ]; then return 1; fi
        printf '%s' "$value"
    } < "$1"
}
container_marker() {
    if [ -e /run/.containerenv ]; then printf podman
    elif [ -e /.dockerenv ]; then printf docker
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
cpu_info
record cpu_model1 printf '%s' "$model1"
record cpu_model2 printf '%s' "$model2"
record cpu_model3 printf '%s' "$model3"
record cpu_model4 printf '%s' "$model4"
record cpu_hypervisor printf '%s' "$hypervisor"
record container_decl scalar_file /run/systemd/container
record container_marker container_marker
record hypervisor_type scalar_file /sys/hypervisor/type
record dmi_product scalar_file /sys/class/dmi/id/product_name
record dmi_vendor scalar_file /sys/class/dmi/id/sys_vendor
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
    /// One bounded startup probe, including transport establishment, without su/sudo.
    pub(crate) async fn collect_startup_environment(
        &self,
        cancellation: CancellationToken,
        deadline: Instant,
    ) -> Result<HostEnvironment> {
        let operation = async {
            self.ensure_connected_transport_only().await?;
            let _permit = self.acquire_command_slot().await?;
            let (generation, channel) = self.open_channel_with_generation().await?;
            let mut channel = ProbeChannel(Some(channel));
            let snapshot = collect(
                channel.0.as_mut().expect("live probe channel"),
                PROBE,
                deadline,
            )
            .await?;
            let session = self.session.lock().await;
            if cancellation.is_cancelled() {
                return Err(SshMcpError::connection(
                    "Startup environment collection cancelled",
                ));
            }
            if !session.as_ref().is_some_and(|route| {
                route.generation == generation
                    && !route.target.is_closed()
                    && !self.is_shutting_down()
            }) {
                return Err(SshMcpError::connection(
                    "SSH route changed during startup environment collection",
                ));
            }
            Ok(snapshot)
        };
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(SshMcpError::connection("Startup environment collection cancelled")),
            _ = self.shutdown_token.cancelled() => Err(SshMcpError::connection("SSH connection manager is shutting down")),
            result = timeout_at(deadline, operation) => result
                .map_err(|_| SshMcpError::Timeout(BOOTSTRAP_BUDGET.as_millis() as u64))?,
        }
    }

    /// Check the binary file channel without PTY, auto-su, or transfer payload files.
    /// It shares the optional startup deadline with metadata and external probes.
    pub(crate) async fn probe_startup_exec_raw(
        &self,
        cancellation: CancellationToken,
        deadline: Instant,
    ) -> Result<bool> {
        let operation = async {
            self.ensure_connected_transport_only().await?;
            let _permit = self.acquire_command_slot().await?;
            let (generation, channel) = self.open_channel_with_generation().await?;
            let mut channel = ProbeChannel(Some(channel));
            let passed =
                collect_binary_echo(channel.0.as_mut().expect("live probe channel"), deadline)
                    .await?;
            let session = self.session.lock().await;
            Ok(passed
                && !cancellation.is_cancelled()
                && session.as_ref().is_some_and(|route| {
                    route.generation == generation
                        && !route.target.is_closed()
                        && !self.is_shutting_down()
                }))
        };
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(SshMcpError::connection("Startup raw preflight cancelled")),
            _ = self.shutdown_token.cancelled() => Err(SshMcpError::connection("SSH connection manager is shutting down")),
            result = timeout_at(deadline, operation) => result
                .map_err(|_| SshMcpError::Timeout(BOOTSTRAP_BUDGET.as_millis() as u64))?,
        }
    }
}

async fn collect_binary_echo(
    channel: &mut Channel<client::Msg>,
    deadline: Instant,
) -> Result<bool> {
    const PAYLOAD: &[u8] = b"SeSSHion\0\xff\x80\r\n";
    let operation = async {
        channel
            .exec(true, wrap_in_posix_shell("exec cat", false))
            .await
            .map_err(|error| {
                SshMcpError::connection(format!("Raw preflight exec failed: {error}"))
            })?;
        channel.data(PAYLOAD).await.map_err(|error| {
            SshMcpError::connection(format!("Raw preflight stdin failed: {error}"))
        })?;
        channel.eof().await.map_err(|error| {
            SshMcpError::connection(format!("Raw preflight EOF failed: {error}"))
        })?;
        let mut stdout = Vec::with_capacity(PAYLOAD.len());
        let mut stderr_bytes = 0usize;
        let mut exit_code = None;
        loop {
            match channel.wait().await {
                Some(ChannelMsg::Data { data }) => {
                    if append_bounded(&mut stdout, &data, PAYLOAD.len()) {
                        return Ok(false);
                    }
                }
                Some(ChannelMsg::ExtendedData { data, .. }) => {
                    stderr_bytes = stderr_bytes.saturating_add(data.len());
                    if stderr_bytes > STDERR_LIMIT {
                        return Ok(false);
                    }
                }
                Some(ChannelMsg::ExitStatus { exit_status }) => exit_code = Some(exit_status),
                Some(ChannelMsg::Failure | ChannelMsg::ExitSignal { .. }) => return Ok(false),
                Some(ChannelMsg::Close) | None => break,
                _ => {}
            }
        }
        Ok(stdout == PAYLOAD && exit_code == Some(0))
    };
    let result = timeout_at(deadline - CLEANUP_RESERVE, operation).await;
    let _ = timeout_at(deadline, channel.close()).await;
    result.unwrap_or(Ok(false))
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
    let mut models = Vec::with_capacity(CPU_MODEL_COUNT);
    let mut cpu_hypervisor = false;
    let mut container_decl = None;
    let mut container_marker = None;
    let mut hypervisor_type = None;
    let mut dmi_product = None;
    let mut dmi_vendor = None;
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
        "cpu_model1",
        "cpu_model2",
        "cpu_model3",
        "cpu_model4",
        "cpu_hypervisor",
        "container_decl",
        "container_marker",
        "hypervisor_type",
        "dmi_product",
        "dmi_vendor",
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
            "cpu_model1" | "cpu_model2" | "cpu_model3" | "cpu_model4" => {
                if let Some(model) = cpu_model(payload) {
                    models.push(model);
                }
            }
            "cpu_hypervisor" => cpu_hypervisor = payload == b"1",
            "container_decl" => container_decl = scalar(payload, SCALAR_LIMIT),
            "container_marker" => container_marker = scalar(payload, SCALAR_LIMIT),
            "hypervisor_type" => hypervisor_type = scalar(payload, SCALAR_LIMIT),
            "dmi_product" => dmi_product = scalar(payload, SCALAR_LIMIT),
            "dmi_vendor" => dmi_vendor = scalar(payload, SCALAR_LIMIT),
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
    models.sort_unstable();
    models.dedup();
    snapshot.cpu_models = (!models.is_empty()).then_some(models);
    snapshot.virtualization.container =
        container_kind(container_decl.as_deref(), container_marker.as_deref());
    snapshot.virtualization.vm = vm_kind(
        hypervisor_type.as_deref(),
        dmi_product.as_deref(),
        dmi_vendor.as_deref(),
        cpu_hypervisor,
    );
    snapshot
}

fn cpu_model(payload: &[u8]) -> Option<String> {
    let value = std::str::from_utf8(payload).ok()?.trim();
    (!value.is_empty() && value.len() <= CPU_MODEL_LIMIT && !value.chars().any(char::is_control))
        .then(|| value.to_owned())
}

fn container_kind(declaration: Option<&str>, marker: Option<&str>) -> Option<String> {
    let declaration = declaration.filter(|value| {
        value.len() <= 64
            && !value.is_empty()
            && value.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"-_".contains(&byte)
            })
    });
    let known_marker = marker.filter(|value| matches!(*value, "podman" | "docker"));
    let kind = match declaration {
        Some("docker" | "podman" | "lxc" | "systemd-nspawn" | "openvz") => declaration,
        Some("lxc-libvirt") => Some("lxc"),
        Some("oci") => known_marker.or(Some("unknown")),
        Some(_) => Some("unknown"),
        None => known_marker,
    };
    kind.map(str::to_owned)
}

fn dmi_kind(value: &str) -> Option<&'static str> {
    match value {
        "KVM" => Some("kvm"),
        "QEMU" => Some("qemu"),
        "VMware" | "VMware, Inc." | "VMware Virtual Platform" => Some("vmware"),
        "VirtualBox" | "innotek GmbH" => Some("virtualbox"),
        "Xen" | "HVM domU" => Some("xen"),
        _ => None,
    }
}

fn vm_kind(
    hypervisor_type: Option<&str>,
    product: Option<&str>,
    vendor: Option<&str>,
    cpu_hypervisor: bool,
) -> Option<String> {
    if hypervisor_type == Some("xen") {
        return Some("xen".into()); // Also covers dom0; no domain-role assertion.
    }
    let product_kind =
        if product == Some("Virtual Machine") && vendor == Some("Microsoft Corporation") {
            Some("hyperv")
        } else {
            product.and_then(dmi_kind)
        };
    let vendor_kind = vendor.and_then(dmi_kind);
    let kind = match (product_kind, vendor_kind) {
        (Some("kvm"), Some("qemu")) | (Some("qemu"), Some("kvm")) => Some("kvm"),
        (Some(product), Some(vendor)) if product != vendor => Some("unknown"),
        (Some(kind), _) | (_, Some(kind)) => Some(kind),
        _ if cpu_hypervisor || hypervisor_type.is_some() => Some("unknown"),
        _ => None,
    };
    kind.map(str::to_owned)
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
        assert_eq!(value.as_object().unwrap().len(), 14);
        assert_eq!(
            value["virtualization"],
            serde_json::json!({"container":null,"vm":null})
        );
        for (key, value) in value.as_object().unwrap() {
            if key != "virtualization" {
                assert!(value.is_null(), "{key} must be unknown");
            }
        }
    }

    #[test]
    fn cpu_names_are_trimmed_bounded_and_data_only() {
        assert_eq!(cpu_model(b"  CPU: Model  \n"), Some("CPU: Model".into()));
        assert_eq!(
            cpu_model("ARM Cortex-A53".as_bytes()),
            Some("ARM Cortex-A53".into())
        );
        assert_eq!(cpu_model(&vec![b'x'; 256]).unwrap().len(), 256);
        assert_eq!(
            cpu_model(format!("  {}  ", "x".repeat(256)).as_bytes())
                .unwrap()
                .len(),
            256
        );
        for invalid in [
            b"".as_slice(),
            b" \n",
            b"bad\tmodel",
            b"\xff",
            &vec![b'x'; 257],
        ] {
            assert!(cpu_model(invalid).is_none());
        }
    }

    #[test]
    fn virtualization_uses_positive_evidence_and_source_precedence() {
        for (declaration, marker, expected) in [
            (Some("podman"), Some("docker"), Some("podman")),
            (Some("unsupported-runtime"), Some("docker"), Some("unknown")),
            (Some("oci"), Some("docker"), Some("docker")),
            (Some("oci"), None, Some("unknown")),
            (Some("not a runtime"), Some("podman"), Some("podman")),
            (None, Some("podman"), Some("podman")),
            (None, None, None),
        ] {
            assert_eq!(container_kind(declaration, marker).as_deref(), expected);
        }
        for (hypervisor, product, vendor, flag, expected) in [
            (None, Some("KVM"), Some("QEMU"), false, Some("kvm")),
            (None, None, Some("QEMU"), false, Some("qemu")),
            (
                None,
                Some("VMware Virtual Platform"),
                Some("VMware, Inc."),
                false,
                Some("vmware"),
            ),
            (
                None,
                Some("VirtualBox"),
                Some("Oracle Corporation"),
                false,
                Some("virtualbox"),
            ),
            (
                None,
                Some("Virtual Machine"),
                Some("Microsoft Corporation"),
                false,
                Some("hyperv"),
            ),
            (None, None, Some("Microsoft Corporation"), false, None),
            (Some("xen"), Some("KVM"), Some("QEMU"), false, Some("xen")),
            (Some("unidentified"), None, None, false, Some("unknown")),
            (
                None,
                Some("KVM"),
                Some("VMware, Inc."),
                false,
                Some("unknown"),
            ),
            (None, Some("83AR"), Some("LENOVO"), true, Some("unknown")),
            (None, Some("83AR"), Some("LENOVO"), false, None),
            (None, None, Some("Amazon EC2"), false, None),
        ] {
            assert_eq!(
                vm_kind(hypervisor, product, vendor, flag).as_deref(),
                expected
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn builtin_cpu_and_virtualization_summaries_survive_missing_utilities_and_sources() {
        let fixture = tempfile::tempdir().unwrap();
        let path = |name| fixture.path().join(name);
        let mut script = PROBE.to_owned();
        for (source, name) in [
            ("/proc/cpuinfo", "cpuinfo"),
            ("/run/systemd/container", "container"),
            ("/run/.containerenv", "podman"),
            ("/.dockerenv", "docker"),
            ("/sys/hypervisor/type", "hypervisor"),
            ("/sys/class/dmi/id/product_name", "product"),
            ("/sys/class/dmi/id/sys_vendor", "vendor"),
        ] {
            script = script.replace(source, path(name).to_str().unwrap());
        }
        let run = |utilities: bool| {
            let mut command = std::process::Command::new("/bin/sh");
            command.args(["-c", &script]);
            if !utilities {
                command.env("PATH", "/nonexistent");
            }
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(output.stdout.len() < STDOUT_LIMIT);
            output.stdout
        };
        std::fs::write(path("cpuinfo"), b"model name : Z CPU\nmodel name\t: A: CPU\nmodel name: Z CPU\nmodel name: B CPU\nmodel name: C CPU\nmodel name: D CPU\nflags\t: vmx\thypervisor svm\n").unwrap();
        std::fs::write(path("container"), "oci\n").unwrap();
        std::fs::write(path("podman"), "").unwrap();
        std::fs::write(path("docker"), "").unwrap();
        std::fs::write(path("product"), "KVM\n").unwrap();
        std::fs::write(path("vendor"), "QEMU\n").unwrap();
        let raw = run(false);
        let snapshot = parse_snapshot(&raw);
        assert_eq!(
            snapshot.cpu_models,
            Some(vec![
                "A: CPU".into(),
                "B CPU".into(),
                "C CPU".into(),
                "Z CPU".into()
            ])
        );
        assert_eq!(snapshot.virtualization.container.as_deref(), Some("podman"));
        assert_eq!(snapshot.virtualization.vm.as_deref(), Some("kvm"));
        assert!(
            snapshot.process_architecture.is_none(),
            "missing dd must not hide earlier summaries"
        );

        // A truncated extension retains completed model records but no later fields.
        let end = raw
            .windows(b"cpu_model3\0".len())
            .position(|value| value == b"cpu_model3\0")
            .unwrap();
        let partial = parse_snapshot(&raw[..end + 3]);
        assert_eq!(
            partial.cpu_models,
            Some(vec!["A: CPU".into(), "Z CPU".into()])
        );
        assert_eq!(partial.virtualization, Virtualization::default());

        std::fs::remove_file(path("product")).unwrap();
        std::fs::remove_file(path("vendor")).unwrap();
        std::fs::write(path("container"), "unsupported-runtime\n").unwrap();
        assert_eq!(
            parse_snapshot(&run(false)).virtualization,
            Virtualization {
                container: Some("unknown".into()),
                vm: Some("unknown".into())
            }
        );

        // Cap the models, not the scan: a later exact hypervisor token still counts.
        // Capability flags and substrings never manufacture positive VM evidence.
        for flags in [
            "vmx svm",
            "not-hypervisor hypervisor-extra",
            "hypervisor\tvmx",
        ] {
            std::fs::write(
                path("cpuinfo"),
                format!("Hardware: Board name\nCPU part: 0xd03\nflags: {flags}\n"),
            )
            .unwrap();
            let observed = parse_snapshot(&run(false));
            assert!(observed.cpu_models.is_none());
            assert_eq!(
                observed.virtualization.vm.as_deref(),
                if flags.starts_with("hypervisor\t") {
                    Some("unknown")
                } else {
                    None
                }
            );
        }
        std::fs::write(
            path("cpuinfo"),
            format!("{}flags: hypervisor\n", "processor: 0\n".repeat(128)),
        )
        .unwrap();
        assert!(parse_snapshot(&run(false)).virtualization.vm.is_none());
        std::fs::write(
            path("cpuinfo"),
            format!(
                "model name: valid\nflags: {}hypervisor\n",
                " ".repeat(65536)
            ),
        )
        .unwrap();
        let observed = parse_snapshot(&run(false));
        assert_eq!(observed.cpu_models, Some(vec!["valid".into()]));
        assert!(
            observed.virtualization.vm.is_none(),
            "discard a crossing line, never a truncated token"
        );

        std::fs::remove_file(path("cpuinfo")).unwrap();
        let observed = parse_snapshot(&run(true));
        assert!(observed.cpu_models.is_none());
        assert!(
            observed.process_architecture.is_some(),
            "unreadable optional sources preserve complete framing and ELF"
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
        assert_eq!(snapshot.virtualization.container.as_deref(), Some("docker"));
        if snapshot.machine_architecture.as_deref() == Some("x86_64") {
            assert!(!snapshot.cpu_models.as_ref().unwrap().is_empty());
        }
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
                tokio::spawn(async move {
                    manager
                        .collect_startup_environment(
                            cancellation,
                            Instant::now() + BOOTSTRAP_BUDGET,
                        )
                        .await
                })
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
    async fn startup_deadline_includes_stalled_transport_establishment() {
        use super::super::{HostKeyCheckMode, SshConfig};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let manager = SshConnectionManager::new(
            SshConfig::new("127.0.0.1", "test")
                .with_port(listener.local_addr().unwrap().port())
                .with_password("fixture-only")
                .with_host_key_checking(HostKeyCheckMode::No),
        )
        .await;
        let started = Instant::now();
        assert!(matches!(
            manager
                .collect_startup_environment(CancellationToken::new(), started + BOOTSTRAP_BUDGET)
                .await,
            Err(SshMcpError::Timeout(3000))
        ));
        assert!(started.elapsed() < Duration::from_millis(3500));
        assert!(!manager.is_connected().await);
        manager.close().await;
    }
}
