//! Startup-only rootless metadata/transfer advice, immutable instructions and bounded failures.

use super::common::*;
use rmcp::ServerHandler;
use serde_json::Value;
use std::time::Duration;
use testcontainers::{ContainerAsync, core::ExecCommand};
use tokio::io::AsyncReadExt;
use tokio::time::{Instant, timeout};
use tokio_util::sync::CancellationToken;

fn config(host: String, port: u16) -> Config {
    Config {
        host,
        port,
        user: "test".into(),
        password: Some("secret".into()),
        key: None,
        jump: None,
        su_password: None,
        sudo_password: None,
        timeout_ms: 30000,
        max_chars: Some(1000),
        max_output_tokens: Some(1),
        disable_sudo: false,
        keepalive_interval: 30,
        keepalive_max: 3,
        reconnect_retries: 2,
        reconnect_backoff_ms: 20,
        health_probe_timeout_ms: 500,
        strict_host_key_checking: ssh_mcp::HostKeyCheckMode::No,
        known_hosts: None,
    }
}

async fn container(image: &str) -> ContainerAsync<GenericImage> {
    init_test_env().unwrap();
    let container = GenericImage::new(image, "latest")
        .with_exposed_port(2222u16.into())
        .start()
        .await
        .unwrap();
    let host = container.get_host().await.unwrap().to_string();
    let port = container.get_host_port_ipv4(2222).await.unwrap();
    timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(mut stream) = tokio::net::TcpStream::connect((host.as_str(), port)).await {
                let mut banner = [0; 8];
                if matches!(
                    timeout(Duration::from_millis(500), stream.read_exact(&mut banner)).await,
                    Ok(Ok(_))
                ) && banner.starts_with(b"SSH-2.0-")
                {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    container
}

async fn control_bytes(container: &ContainerAsync<GenericImage>, script: &str) -> Vec<u8> {
    let mut output = container
        .exec(ExecCommand::new(["sh", "-c", script]))
        .await
        .unwrap();
    let bytes = output.stdout_to_vec().await.unwrap();
    assert_eq!(
        output.exit_code().await.unwrap(),
        Some(0),
        "fixture script failed: {script}"
    );
    bytes
}

async fn control(container: &ContainerAsync<GenericImage>, script: &str) -> String {
    String::from_utf8(control_bytes(container, script).await).unwrap()
}

async fn startup_files(container: &ContainerAsync<GenericImage>) -> String {
    control(
        container,
        "find /home/test /tmp -mindepth 1 -printf '%p\\n' | LC_ALL=C sort",
    )
    .await
}

async fn unprepared_server(container: &ContainerAsync<GenericImage>, su: bool) -> SshMcpServer {
    let mut config = config(
        container.get_host().await.unwrap().to_string(),
        container.get_host_port_ipv4(2222).await.unwrap(),
    );
    if su {
        config.su_password = Some("root-secret".into());
        config.sudo_password = Some("unused".into());
    }
    SshMcpServer::new(config).await.unwrap()
}

fn snapshot(server: &SshMcpServer) -> Value {
    let instructions = server.get_info().instructions.unwrap();
    let value: Value = serde_json::from_str(instructions.lines().last().unwrap()).unwrap();
    assert_eq!(value.as_object().unwrap().len(), 14);
    value
}

fn transfer_hint(server: &SshMcpServer) -> String {
    let instructions = server.get_info().instructions.unwrap();
    let hints: Vec<_> = instructions
        .lines()
        .filter(|line| line.starts_with("Transfer: "))
        .collect();
    assert_eq!(hints.len(), 1, "one frozen, single-line transfer hint");
    // Keep the compact 14-field JSON as the final instruction line.
    snapshot(server);
    hints[0].to_owned()
}

const PASSWORD_TRANSFER_HINT: &str = "Transfer: prefer exec-raw; startup preflight (may be stale): ok=exec-raw; blocked=rsync,sftp,scp.";

async fn working_command(server: &SshMcpServer) {
    let output = server
        .connection()
        .exec_command("printf OK", Duration::from_secs(3))
        .await
        .unwrap();
    assert_eq!(output.stdout.trim(), "OK");
}

const FIXTURE: &str = r#"
set -eu
mkdir -p /home/test/probebin
cat > /home/test/.bashrc <<'SCRIPT'
PATH=/home/test/probebin:$PATH; export PATH
if [ "$(cat /home/test/probe-mode)" = startup_stderr ]; then
    /usr/bin/head -c 131072 /dev/zero >&2
fi
SCRIPT
printf initial > /home/test/probe-host
printf normal > /home/test/probe-mode
: > /home/test/probe-calls
cat > /home/test/probebin/uname <<'SCRIPT'
#!/bin/sh
if [ "$1" = -n ]; then
    printf x >> /home/test/probe-calls
    cat /home/test/probe-host
else
    exec /usr/bin/uname "$@"
fi
SCRIPT
cat > /home/test/probebin/nproc <<'SCRIPT'
#!/bin/sh
case $(cat /home/test/probe-mode) in
  hold) touch /home/test/probe-entered
        while [ ! -e /home/test/probe-release ]; do /bin/sleep 0.02; done
        printf 7;;
  hang) /bin/sleep 20;;
  flood) /usr/bin/head -c 131072 /dev/zero | /usr/bin/tr '\000' x;;
  malformed) printf bad;;
  missing) exit 127;;
  *) printf 7;;
esac
SCRIPT
chmod +x /home/test/probebin/*
chown -R test:test /home/test/probebin /home/test/probe-* /home/test/.bashrc
"#;

#[tokio::test]
async fn startup_environment_is_rootless_once_and_frozen_across_commands_reconnect() {
    let container = container("ssh-mcp-debian-sshd").await;
    control(&container, FIXTURE).await;
    control(
        &container,
        r#"
printf 'root:root-secret\n' | chpasswd
cat > /home/test/probebin/su <<'SCRIPT'
#!/bin/sh
printf x >> /home/test/su-calls
exec /usr/bin/su "$@"
SCRIPT
cat > /home/test/probebin/sudo <<'SCRIPT'
#!/bin/sh
printf x >> /home/test/sudo-calls
exec /usr/bin/sudo "$@"
SCRIPT
chmod +x /home/test/probebin/su /home/test/probebin/sudo
"#,
    )
    .await;
    let files_before = startup_files(&container).await;
    let server = unprepared_server(&container, true)
        .await
        .with_startup_environment(CancellationToken::new())
        .await;
    let instructions = serde_json::to_vec(&server.get_info()).unwrap();
    let value = snapshot(&server);
    assert_eq!(transfer_hint(&server), PASSWORD_TRANSFER_HINT);
    assert_eq!(startup_files(&container).await, files_before);
    assert_eq!(value.as_object().unwrap().len(), 14);
    assert_eq!(value["hostname"], "initial");
    assert_eq!(value["os"], "Linux");
    assert!(value["distribution"].as_str().unwrap().contains("Debian"));
    assert_eq!(value["effective_uid"], 1000);
    assert_eq!(value["effective_gid"], 1000);
    assert_eq!(value["running_as_root"], false);
    assert_eq!(value["pointer_width"], 64);
    assert_eq!(value["available_cpu_parallelism"], 7);
    assert_eq!(value["virtualization"]["container"], "docker");
    if value["machine_architecture"] == "x86_64" {
        let models = value["cpu_models"].as_array().unwrap();
        assert!(!models.is_empty() && models.len() <= 4);
        assert!(
            models
                .iter()
                .all(|model| model.as_str().unwrap().len() <= 256)
        );
    }
    assert!(
        value["shell_executable"]
            .as_str()
            .unwrap()
            .ends_with("dash")
    );
    assert!(!server.connection().is_elevated());
    control(
        &container,
        "test ! -e /home/test/su-calls; test ! -e /home/test/sudo-calls",
    )
    .await;

    control(&container, "printf changed > /home/test/probe-host; mkdir -p /run/systemd; printf podman > /run/systemd/container").await;
    let server = server
        .with_startup_environment(CancellationToken::new())
        .await;
    assert_eq!(
        serde_json::to_vec(&server.get_info()).unwrap(),
        instructions
    );
    let output = server
        .connection()
        .exec_command("id -u", Duration::from_secs(5))
        .await
        .unwrap();
    assert!(
        server.connection().is_elevated(),
        "legacy command must initialize deferred su"
    );
    // Preserve existing PTY terminal prefixes in the legacy execution contract.
    assert_eq!(output.stdout.trim_end().rsplit('\r').next(), Some("0"));
    assert_eq!(
        control(&container, "wc -c < /home/test/su-calls")
            .await
            .trim(),
        "1"
    );
    server.connection().reconnect().await.unwrap();
    assert_eq!(
        serde_json::to_vec(&server.get_info()).unwrap(),
        instructions
    );
    assert_eq!(snapshot(&server), value);
    assert_eq!(
        control(&container, "wc -c < /home/test/probe-calls")
            .await
            .trim(),
        "1"
    );
    server.shutdown().await;

    // A new server startup recollects, without a refresh tool or a route cache.
    let next = unprepared_server(&container, false)
        .await
        .with_startup_environment(CancellationToken::new())
        .await;
    assert_eq!(transfer_hint(&next), PASSWORD_TRANSFER_HINT);
    assert_eq!(snapshot(&next)["hostname"], "changed");
    assert_eq!(snapshot(&next)["virtualization"]["container"], "podman");
    assert_eq!(
        control(&container, "wc -c < /home/test/probe-calls")
            .await
            .trim(),
        "2"
    );
    next.shutdown().await;
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn startup_transfer_key_sftp_success_keeps_unprobed_transports_unknown() {
    if !check_sftp() || !check_openssh_client("ssh") {
        tracing::warn!("skipping: local OpenSSH ssh/sftp clients unavailable");
        return;
    }
    let container = container("ssh-mcp-debian-sshd").await;
    control(&container, FIXTURE).await;
    let (_key_dir, key_path) = setup_test_key();
    let mut config = config(
        container.get_host().await.unwrap().to_string(),
        container.get_host_port_ipv4(2222).await.unwrap(),
    );
    config.password = None;
    config.key = Some(key_path);
    let files_before = startup_files(&container).await;
    let unprepared = SshMcpServer::new(config).await.unwrap();
    let start = Instant::now();
    let server = unprepared
        .with_startup_environment(CancellationToken::new())
        .await;
    assert!(start.elapsed() < Duration::from_millis(3500));
    assert_eq!(
        transfer_hint(&server),
        "Transfer: prefer sftp; startup preflight (may be stale): ok=sftp,exec-raw; unknown=rsync,scp."
    );
    let value = snapshot(&server);
    assert_eq!(value["hostname"], "initial");
    assert_eq!(value["effective_uid"], 1000);
    assert_eq!(startup_files(&container).await, files_before);
    working_command(&server).await;
    server.shutdown().await;
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn startup_transfer_closed_sftp_is_unknown_and_recommended_raw_file_put_works() {
    if !check_sftp() || !check_openssh_client("ssh") {
        tracing::warn!("skipping: local OpenSSH ssh/sftp clients unavailable");
        return;
    }
    let container = container("ssh-mcp-debian-sshd").await;
    control(&container, FIXTURE).await;
    // Only this disposable container is changed. Validate before asking the
    // foreground sshd to re-exec; leave shell/exec channels and key auth enabled.
    control(
        &container,
        r#"
set -eu
sed -i 's|^[[:space:]]*Subsystem[[:space:]][[:space:]]*sftp[[:space:]].*$|Subsystem sftp /bin/false|' /etc/ssh/sshd_config
/usr/sbin/sshd -t
kill -HUP "$(cat /run/sshd.pid)"
"#,
    )
    .await;
    let (_key_dir, key_path) = setup_test_key();
    let mut config = config(
        container.get_host().await.unwrap().to_string(),
        container.get_host_port_ipv4(2222).await.unwrap(),
    );
    config.password = None;
    config.key = Some(key_path);
    // The metadata fixture deliberately limits ordinary command output to one
    // token. This test also transfers a file, whose existing HOME/staging
    // commands need their normal output budget; startup probes stay bounded
    // independently of that setting.
    config.max_output_tokens = Some(1000);
    let files_before = startup_files(&container).await;
    let unprepared = SshMcpServer::new(config).await.unwrap();
    let start = Instant::now();
    let server = unprepared
        .with_startup_environment(CancellationToken::new())
        .await;
    assert!(start.elapsed() < Duration::from_millis(3500));
    assert_eq!(
        transfer_hint(&server),
        "Transfer: prefer exec-raw; startup preflight (may be stale): ok=exec-raw; unknown=rsync,sftp,scp."
    );
    let value = snapshot(&server);
    assert_eq!(value["hostname"], "initial");
    assert_eq!(value["effective_uid"], 1000);
    assert_eq!(startup_files(&container).await, files_before);
    working_command(&server).await;

    // The server's transfer root is the current directory; keep the payload
    // inside it, with TempDir owning cleanup even if an assertion fails.
    let local_root = std::env::current_dir().unwrap();
    assert!(local_root.is_dir());
    let local_dir = tempfile::tempdir_in(&local_root).unwrap();
    let local_file = local_dir.path().join("hint-transfer.bin");
    let payload = b"hint-directed exec-raw transfer\0\xff\r\n";
    std::fs::write(&local_file, payload).unwrap();
    let instructions = server.get_info().instructions;
    let response = server
        .test_transfer(TransferParams {
            operation: TransferOperation::Put,
            local_path: local_file.to_string_lossy().into_owned(),
            remote_path: "/home/test/hint-transfer.bin".into(),
            transport: TransferTransport::ExecRaw,
            kind: Some(TransferKind::File),
            overwrite: false,
            timeout_ms: Some(5000),
            ..Default::default()
        })
        .await;
    assert!(response.ok, "one explicit raw PUT: {:?}", response.error);
    assert_eq!(response.transport_used, TransferTransport::ExecRaw);
    assert_eq!(
        control_bytes(&container, "cat /home/test/hint-transfer.bin").await,
        payload
    );
    assert_eq!(server.get_info().instructions, instructions);
    server.shutdown().await;
}

#[tokio::test]
async fn startup_environment_partial_timeout_flood_and_cancellation_preserve_ssh() {
    let container = container("ssh-mcp-debian-sshd").await;
    control(&container, FIXTURE).await;
    for mode in ["hang", "flood", "malformed", "missing", "startup_stderr"] {
        control(
            &container,
            &format!("printf {mode} > /home/test/probe-mode"),
        )
        .await;
        let unprepared = unprepared_server(&container, false).await;
        let start = Instant::now();
        let prepared = unprepared
            .with_startup_environment(CancellationToken::new())
            .await;
        assert!(
            start.elapsed() < Duration::from_millis(3500),
            "{mode} escaped bootstrap deadline"
        );
        let partial = snapshot(&prepared);
        if mode != "startup_stderr" {
            assert_eq!(partial["hostname"], "initial");
            assert_eq!(partial["effective_uid"], 1000);
        }
        assert!(partial["available_cpu_parallelism"].is_null());
        if mode == "hang" {
            assert_eq!(
                transfer_hint(&prepared),
                PASSWORD_TRANSFER_HINT,
                "metadata timeout must not erase the completed raw preflight"
            );
        }
        let instructions = prepared.get_info().instructions;
        control(&container, "printf normal > /home/test/probe-mode").await;
        working_command(&prepared).await;
        assert_eq!(prepared.get_info().instructions, instructions);
        prepared.shutdown().await;
    }

    let unprepared = unprepared_server(&container, false).await;
    let ct = CancellationToken::new();
    ct.cancel();
    let unprepared = unprepared.with_startup_environment(ct).await;
    assert!(!unprepared.connection().is_connected().await);
    unprepared.shutdown().await;

    control(&container, "printf hold > /home/test/probe-mode").await;
    let unprepared = unprepared_server(&container, false).await;
    let cancellation = CancellationToken::new();
    let producer = {
        let ct = cancellation.clone();
        tokio::spawn(async move { unprepared.with_startup_environment(ct).await })
    };
    timeout(Duration::from_secs(2), async {
        while control(
            &container,
            "if [ -e /home/test/probe-entered ]; then printf yes; fi",
        )
        .await
            != "yes"
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    cancellation.cancel();
    let cancelled = timeout(Duration::from_secs(1), producer)
        .await
        .unwrap()
        .unwrap();
    let unknown = snapshot(&cancelled);
    assert_eq!(unknown.as_object().unwrap().len(), 14);
    assert_eq!(
        unknown["virtualization"],
        serde_json::json!({"container":null,"vm":null})
    );
    for (key, value) in unknown.as_object().unwrap() {
        if key != "virtualization" {
            assert!(
                value.is_null(),
                "cancelled bootstrap must not publish {key}"
            );
        }
    }
    control(
        &container,
        "touch /home/test/probe-release; printf normal > /home/test/probe-mode",
    )
    .await;
    working_command(&cancelled).await;
    cancelled.shutdown().await;
}

#[tokio::test]
async fn startup_raw_preflight_requires_exact_binary_output_and_zero_exit() {
    let container = container("ssh-mcp-debian-sshd").await;
    control(
        &container,
        "mkdir -p /home/test/probebin; printf 'PATH=/home/test/probebin:$PATH; export PATH\n' > /home/test/.bashrc; chown test:test /home/test/.bashrc",
    )
    .await;
    for script in [
        "/bin/cat >/dev/null; printf wrong",
        "/bin/cat; exit 7",
        "/bin/sleep 20",
    ] {
        control(
            &container,
            &format!(
                "printf '#!/bin/sh\\n%s\\n' '{script}' > /home/test/probebin/cat; chmod +x /home/test/probebin/cat"
            ),
        )
        .await;
        let unprepared = unprepared_server(&container, false).await;
        let start = Instant::now();
        let server = unprepared
            .with_startup_environment(CancellationToken::new())
            .await;
        assert!(start.elapsed() < Duration::from_millis(3500));
        assert_eq!(
            transfer_hint(&server),
            "Transfer: prefer auto; startup preflight (may be stale): blocked=rsync,sftp,scp; unknown=exec-raw.",
            "inconclusive raw channel must not be recommended: {script}"
        );
        assert_eq!(snapshot(&server)["effective_uid"], 1000);
        working_command(&server).await;
        server.shutdown().await;
    }
}

#[tokio::test]
async fn startup_environment_missing_unreadable_and_exotic_sources() {
    let container = container("ssh-mcp-debian-sshd").await;
    control(&container, FIXTURE).await;
    control(
        &container,
        r#"
for utility in uname id readlink dd; do
    printf '#!/bin/sh\nexit 127\n' > /home/test/probebin/$utility
    chmod +x /home/test/probebin/$utility
done
rm -f /etc/os-release
printf 'PRETTY_NAME="Private Linux"\n' > /etc/os-release
chmod 600 /etc/os-release
"#,
    )
    .await;
    let first = unprepared_server(&container, false)
        .await
        .with_startup_environment(CancellationToken::new())
        .await;
    let fallback = snapshot(&first);
    assert!(fallback["hostname"].is_string());
    assert_eq!(fallback["os"], "Linux");
    assert!(fallback["kernel_release"].is_string());
    assert!(
        fallback["distribution"].is_null(),
        "unreadable primary must not merge/fall back to /usr"
    );
    assert_eq!(fallback["effective_uid"], 1000);
    assert_eq!(fallback["effective_gid"], 1000);
    assert_eq!(fallback["running_as_root"], false);
    for field in [
        "machine_architecture",
        "process_architecture",
        "pointer_width",
        "shell_executable",
    ] {
        assert!(fallback[field].is_null(), "{field} is unavailable");
    }
    assert_eq!(fallback["virtualization"]["container"], "docker");
    if cfg!(target_arch = "x86_64") {
        assert!(
            fallback["cpu_models"].is_array(),
            "missing dd must not hide CPU models"
        );
    }
    first.shutdown().await;
    control(&container, "rm /etc/os-release").await;
    let next = unprepared_server(&container, false)
        .await
        .with_startup_environment(CancellationToken::new())
        .await;
    assert!(
        snapshot(&next)["distribution"]
            .as_str()
            .unwrap()
            .contains("Debian")
    );
    next.shutdown().await;
    control(
        &container,
        r#"
printf 'NAME="Odd Linux"\nVERSION_ID=42\n' > /etc/os-release
cat > /home/test/probebin/head <<'SCRIPT'
#!/bin/sh
case "$*" in *'/proc/'*) exit 1;; esac
exec /usr/bin/head "$@"
SCRIPT
chmod +x /home/test/probebin/head
"#,
    )
    .await;
    let exotic = unprepared_server(&container, false)
        .await
        .with_startup_environment(CancellationToken::new())
        .await;
    let value = snapshot(&exotic);
    assert_eq!(value["distribution"], "Odd Linux 42");
    for field in [
        "hostname",
        "os",
        "kernel_release",
        "effective_uid",
        "effective_gid",
        "running_as_root",
    ] {
        assert!(value[field].is_null(), "{field} must remain unknown");
    }
    working_command(&exotic).await;
    exotic.shutdown().await;
}

#[tokio::test]
async fn startup_environment_fish_describes_the_actual_posix_probe() {
    let container = container("ssh-mcp-debian-sshd-fish").await;
    let server = unprepared_server(&container, false)
        .await
        .with_startup_environment(CancellationToken::new())
        .await;
    let value = snapshot(&server);
    assert_eq!(value["effective_uid"], 1000);
    assert_eq!(value["os"], "Linux");
    assert!(
        value["shell_executable"]
            .as_str()
            .unwrap()
            .ends_with("dash")
    );
    working_command(&server).await;
    server.shutdown().await;
}
