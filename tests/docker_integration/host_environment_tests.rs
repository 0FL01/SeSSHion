//! Real SSH coverage for the rootless snapshot/cache and channel-local failures.

use super::common::*;
use rmcp::ServerHandler;
use serde_json::Value;
use std::{sync::Arc, time::Duration};
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

async fn control(container: &ContainerAsync<GenericImage>, script: &str) -> String {
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
    String::from_utf8(bytes).unwrap()
}

async fn server(container: &ContainerAsync<GenericImage>, su: bool) -> SshMcpServer {
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

async fn snapshot(server: &SshMcpServer, refresh: bool) -> Value {
    let result = server
        .test_host_environment(refresh, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        result.is_error,
        Some(false),
        "{}",
        extract_text_from_result(&result)
    );
    assert_eq!(result.content.len(), 1);
    let text: Value = serde_json::from_str(&extract_text_from_result(&result)).unwrap();
    assert_eq!(result.structured_content.as_ref(), Some(&text));
    text
}

async fn working_command(server: &SshMcpServer) {
    let output = server
        .connection()
        .exec_command("printf OK", Duration::from_secs(3))
        .await
        .unwrap();
    assert_eq!(output.stdout.trim(), "OK");
}

// Test-controlled utility behavior, without adding arbitrary commands to the tool.
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
  stderr) /usr/bin/head -c 131072 /dev/zero >&2;;
  malformed) printf bad;;
  missing) exit 127;;
  *) printf 7;;
esac
SCRIPT
chmod +x /home/test/probebin/*
chown -R test:test /home/test/probebin /home/test/probe-* /home/test/.bashrc
"#;

async fn entered(container: &ContainerAsync<GenericImage>) {
    timeout(Duration::from_secs(2), async {
        loop {
            if control(
                container,
                "if [ -e /home/test/probe-entered ]; then printf yes; fi",
            )
            .await
                == "yes"
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("probe did not reach controlled barrier");
}

#[tokio::test]
async fn host_environment_smoke_rootless_cold_reconnect_and_legacy_elevation() {
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
    let server = server(&container, true).await;
    let instructions = serde_json::to_vec(&server.get_info()).unwrap();
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert_eq!(
        server
            .test_host_environment(false, cancelled)
            .await
            .unwrap()
            .is_error,
        Some(true)
    );
    assert!(
        !server.connection().is_connected().await,
        "pre-cancelled probe must not connect"
    );
    for _ in 0..2 {
        let value = snapshot(&server, false).await;
        assert_eq!(value["hostname"], "initial");
        assert_eq!(value["os"], "Linux");
        assert!(value["distribution"].as_str().unwrap().contains("Debian"));
        assert_eq!(value["effective_uid"], 1000);
        assert_eq!(value["effective_gid"], 1000);
        assert_eq!(value["running_as_root"], false);
        assert_eq!(value["pointer_width"], 64);
        assert_eq!(value["available_cpu_parallelism"], 7);
        assert!(
            value["shell_executable"]
                .as_str()
                .unwrap()
                .ends_with("dash")
        );
        assert!(!server.connection().is_elevated());
        assert_eq!(
            control(
                &container,
                "test ! -e /home/test/su-calls; test ! -e /home/test/sudo-calls"
            )
            .await,
            ""
        );
        server
            .connection()
            .invalidate_session("test rootless reconnect")
            .await;
    }
    snapshot(&server, false).await;
    let output = server
        .connection()
        .exec_command("id -u", Duration::from_secs(5))
        .await
        .unwrap();
    assert!(
        server.connection().is_elevated(),
        "legacy command must initialize deferred su"
    );
    // PTY-backed legacy execution may retain bracketed-paste terminal prefixes.
    assert_eq!(output.stdout.trim_end().rsplit('\r').next(), Some("0"));
    assert_eq!(
        control(&container, "wc -c < /home/test/su-calls")
            .await
            .trim(),
        "1"
    );
    snapshot(&server, true).await;
    assert_eq!(
        snapshot(&server, false).await["effective_uid"],
        1000,
        "probe must bypass existing su channel"
    );
    assert_eq!(
        serde_json::to_vec(&server.get_info()).unwrap(),
        instructions
    );
    server.shutdown().await;
}

#[tokio::test]
async fn host_environment_cache_refresh_concurrency_and_generation_race() {
    let container = container("ssh-mcp-debian-sshd").await;
    control(&container, FIXTURE).await;
    let server = Arc::new(server(&container, false).await);
    let mut tasks = tokio::task::JoinSet::new();
    let barrier = Arc::new(tokio::sync::Barrier::new(9));
    for _ in 0..8 {
        let server = server.clone();
        let barrier = barrier.clone();
        tasks.spawn(async move {
            barrier.wait().await;
            snapshot(&server, false).await
        });
    }
    barrier.wait().await;
    while let Some(result) = tasks.join_next().await {
        assert_eq!(result.unwrap()["hostname"], "initial");
    }
    assert_eq!(
        control(&container, "wc -c < /home/test/probe-calls")
            .await
            .trim(),
        "1"
    );
    control(
        &container,
        "printf updated > /home/test/probe-host; printf malformed > /home/test/probe-mode",
    )
    .await;
    assert_eq!(snapshot(&server, false).await["hostname"], "initial");
    let refreshed = snapshot(&server, true).await;
    assert_eq!(refreshed["hostname"], "updated");
    assert!(
        refreshed["available_cpu_parallelism"].is_null(),
        "refresh must replace old values with null"
    );
    assert_eq!(snapshot(&server, false).await, refreshed);
    server.connection().reconnect().await.unwrap();
    control(
        &container,
        "printf reconnected > /home/test/probe-host; printf normal > /home/test/probe-mode",
    )
    .await;
    assert_eq!(snapshot(&server, false).await["hostname"], "reconnected");

    // A deterministic barrier, not a sleep, keeps the old producer in flight.
    control(&container, "printf hold > /home/test/probe-mode").await;
    let producer = {
        let server = server.clone();
        tokio::spawn(async move {
            server
                .test_host_environment(true, CancellationToken::new())
                .await
                .unwrap()
        })
    };
    entered(&container).await;
    server
        .connection()
        .invalidate_session("test stale producer")
        .await;
    control(&container, "printf newest > /home/test/probe-host; printf normal > /home/test/probe-mode; touch /home/test/probe-release").await;
    let result = producer.await.unwrap();
    assert_eq!(
        result.is_error,
        Some(true),
        "old producer cannot publish after route removal"
    );
    assert_eq!(snapshot(&server, false).await["hostname"], "newest");
    server.shutdown().await;
}

#[tokio::test]
async fn host_environment_partial_timeout_flood_and_cancel_preserve_session_and_cache() {
    let container = container("ssh-mcp-debian-sshd").await;
    control(&container, FIXTURE).await;
    let server = Arc::new(server(&container, false).await);
    let initial = snapshot(&server, false).await;
    for mode in ["hang", "flood", "stderr", "missing"] {
        control(
            &container,
            &format!("printf {mode} > /home/test/probe-mode"),
        )
        .await;
        let start = Instant::now();
        let partial = snapshot(&server, true).await;
        assert!(
            start.elapsed() < Duration::from_millis(3500),
            "{mode} exceeded metadata budget"
        );
        assert_eq!(partial["hostname"], initial["hostname"]);
        assert_eq!(partial["effective_uid"], 1000);
        assert!(partial["available_cpu_parallelism"].is_null());
        assert_eq!(
            snapshot(&server, false).await,
            partial,
            "partial snapshots are cached"
        );
        assert!(server.connection().is_connected().await);
        working_command(&server).await;
    }
    control(&container, "printf startup_stderr > /home/test/probe-mode").await;
    let start = Instant::now();
    let stderr_partial = snapshot(&server, true).await;
    assert!(start.elapsed() < Duration::from_millis(3500));
    assert_eq!(snapshot(&server, false).await, stderr_partial);
    control(&container, "printf normal > /home/test/probe-mode").await;
    working_command(&server).await;
    let previous = snapshot(&server, true).await;
    control(
        &container,
        "printf cancelled > /home/test/probe-host; printf hold > /home/test/probe-mode",
    )
    .await;
    let cancellation = CancellationToken::new();
    let producer = {
        let server = server.clone();
        let ct = cancellation.clone();
        tokio::spawn(async move { server.test_host_environment(true, ct).await.unwrap() })
    };
    entered(&container).await;
    // Cancel a waiter without cancelling the producer, then cancel the refresh.
    let waiter_ct = CancellationToken::new();
    let waiter = {
        let server = server.clone();
        let ct = waiter_ct.clone();
        tokio::spawn(async move { server.test_host_environment(true, ct).await.unwrap() })
    };
    waiter_ct.cancel();
    assert_eq!(waiter.await.unwrap().is_error, Some(true));
    assert!(!producer.is_finished());
    cancellation.cancel();
    assert_eq!(
        timeout(Duration::from_secs(1), producer)
            .await
            .unwrap()
            .unwrap()
            .is_error,
        Some(true)
    );
    assert_eq!(
        snapshot(&server, false).await,
        previous,
        "cancelled refresh must retain old cache"
    );
    control(
        &container,
        "touch /home/test/probe-release; printf normal > /home/test/probe-mode",
    )
    .await;
    assert_eq!(
        snapshot(&server, true).await["hostname"],
        "cancelled",
        "gate and permit released"
    );
    working_command(&server).await;
    server.shutdown().await;
    assert!(
        server
            .connection()
            .host_environment(false, CancellationToken::new())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn host_environment_missing_sources_unreadable_release_and_proc_fallbacks() {
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
    let server = server(&container, false).await;
    let fallback = snapshot(&server, false).await;
    assert!(fallback["hostname"].is_string());
    assert_eq!(fallback["os"], "Linux");
    assert!(fallback["kernel_release"].is_string());
    assert!(
        fallback["distribution"].is_null(),
        "unreadable existing primary must not merge/fall back to /usr"
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
    control(&container, "rm /etc/os-release").await;
    assert!(
        snapshot(&server, true).await["distribution"]
            .as_str()
            .unwrap()
            .contains("Debian")
    );
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
    let exotic = snapshot(&server, true).await;
    assert_eq!(exotic["distribution"], "Odd Linux 42");
    for field in [
        "hostname",
        "os",
        "kernel_release",
        "effective_uid",
        "effective_gid",
        "running_as_root",
    ] {
        assert!(
            exotic[field].is_null(),
            "{field} must remain unknown, not an invented default"
        );
    }
    working_command(&server).await;
    server.shutdown().await;
}

#[tokio::test]
async fn host_environment_absolute_budget_includes_gate_and_slot_waits() {
    let container = container("ssh-mcp-debian-sshd").await;
    control(&container, FIXTURE).await;
    let server = Arc::new(server(&container, false).await);
    let previous = snapshot(&server, false).await;
    let mut permits = Vec::new();
    for _ in 0..8 {
        permits.push(server.test_acquire_command_slot().await.unwrap());
    }
    let start = Instant::now();
    let blocked = server
        .test_host_environment(true, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(blocked.is_error, Some(true));
    assert!(
        start.elapsed() < Duration::from_millis(3500),
        "slot wait escaped metadata deadline"
    );
    assert_eq!(snapshot(&server, false).await, previous);
    drop(permits);
    working_command(&server).await;

    control(&container, "printf hold > /home/test/probe-mode").await;
    let producer = {
        let server = server.clone();
        tokio::spawn(async move { snapshot(&server, true).await })
    };
    entered(&container).await;
    let start = Instant::now();
    // Waiting for the producer does not reset this caller's collection budget.
    let waiter = server
        .test_host_environment(true, CancellationToken::new())
        .await
        .unwrap();
    assert!(
        start.elapsed() < Duration::from_millis(3500),
        "gate wait reset the metadata deadline"
    );
    // Either a partial collection or a deadline error at the boundary is valid.
    assert!(waiter.is_error == Some(true) || waiter.structured_content.is_some());
    assert!(producer.await.unwrap()["available_cpu_parallelism"].is_null());
    control(
        &container,
        "touch /home/test/probe-release; printf normal > /home/test/probe-mode",
    )
    .await;
    assert_eq!(
        snapshot(&server, true).await["available_cpu_parallelism"],
        7
    );
    working_command(&server).await;
    server.shutdown().await;
}

#[tokio::test]
async fn host_environment_fish_uses_the_actual_posix_probe_shell() {
    let container = container("ssh-mcp-debian-sshd-fish").await;
    let server = server(&container, false).await;
    let value = snapshot(&server, false).await;
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
