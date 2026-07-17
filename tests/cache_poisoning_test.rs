//! E2E regression test for Bug #2: Detach Mode Cache Poisoning.
//!
//! Root cause: `determine_detach_mode()` used to treat transient/negative probe
//! results as permanent `DirectOnly` and store that value in the process-wide
//! `AtomicU8` cache forever. After the transient failure was gone, subsequent
//! `background=true` calls returned cached `DirectOnly` without re-probing.
//!
//! Deterministic reproduction strategy:
//!   1. Start a fresh MCP server (detach cache = Unknown).
//!   2. Temporarily hide `/bin/sh` inside the SSH container.
//!   3. Call `exec(background=true)`. SSH health still succeeds because the
//!      MCP server's liveness probe is SSH ping and the test user's login shell
//!      is `/bin/bash`, but the detach wrappers invoke `sh -lc` / `sh -c`, so
//!      Full and Portable probes fail.
//!   4. Restore `/bin/sh` in the same container.
//!   5. On the same MCP process, call `exec(background=true)` again. It must
//!      re-probe and start successfully, proving `DirectOnly` was not cached.
//!
//! Usage:
//!   cargo test --test cache_poisoning_test -- --nocapture --ignored
//!
//! Requires:
//!   - Docker container running on 127.0.0.1:2222 (`docker compose up -d`)
//!   - release binary built (`cargo build --release`)

use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Mutex, mpsc};
use tokio::time::timeout;

static REQUEST_ID: AtomicU64 = AtomicU64::new(1);

const MCP_ARGS: &[&str] = &[
    "--host",
    "127.0.0.1",
    "--port",
    "2222",
    "--user",
    "test",
    "--password",
    "secret",
    "--strict-host-key-checking",
    "no",
    "--known-hosts",
    "/dev/null",
    "--log-level",
    "warn",
    "--timeout",
    "30000",
];

const SSH_CONTAINER: &str = "ssh-mcp-ssh-1";
const SH_BACKUP: &str = "/bin/sh.ssh-mcp-cache-poisoning-test";

#[derive(Clone)]
struct McpClient {
    stdin: Arc<Mutex<tokio::process::ChildStdin>>,
    responses: Arc<Mutex<std::collections::HashMap<u64, mpsc::Sender<Value>>>>,
    child: Arc<Mutex<tokio::process::Child>>,
}

impl McpClient {
    async fn spawn(binary: &str) -> anyhow::Result<Self> {
        let mut child = Command::new(binary)
            .args(MCP_ARGS)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;

        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");

        let stdin = Arc::new(Mutex::new(stdin));
        let responses: Arc<Mutex<std::collections::HashMap<u64, mpsc::Sender<Value>>>> =
            Arc::new(Mutex::new(std::collections::HashMap::new()));
        let child = Arc::new(Mutex::new(child));

        let client = Self {
            stdin,
            responses: responses.clone(),
            child,
        };

        client.start_reader(stdout);
        client.initialize().await?;

        Ok(client)
    }

    fn start_reader(&self, stdout: tokio::process::ChildStdout) {
        let responses = self.responses.clone();
        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.is_empty() {
                    continue;
                }
                if let Ok(msg) = serde_json::from_str::<Value>(&line)
                    && let Some(id) = msg.get("id").and_then(|v| v.as_u64())
                {
                    let map = responses.lock().await;
                    if let Some(tx) = map.get(&id) {
                        let _ = tx.send(msg.clone()).await;
                    }
                }
            }
        });
    }

    async fn send_request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = REQUEST_ID.fetch_add(1, Ordering::SeqCst);
        let (tx, mut rx) = mpsc::channel(1);

        {
            let mut map = self.responses.lock().await;
            map.insert(id, tx);
        }

        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let line = format!("{}\n", request);
        {
            let mut stdin = self.stdin.lock().await;
            stdin.write_all(line.as_bytes()).await?;
            stdin.flush().await?;
        }

        match timeout(Duration::from_secs(30), rx.recv()).await {
            Ok(Some(response)) => {
                let mut map = self.responses.lock().await;
                map.remove(&id);
                Ok(response)
            }
            _ => {
                let mut map = self.responses.lock().await;
                map.remove(&id);
                anyhow::bail!("request id={id} timed out")
            }
        }
    }

    async fn send_notification(&self, method: &str, params: Value) -> anyhow::Result<()> {
        let notification = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let line = format!("{}\n", notification);
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(line.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }

    async fn initialize(&self) -> anyhow::Result<()> {
        let response = self
            .send_request(
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": { "name": "cache-poisoning-test", "version": "1.0.0" }
                }),
            )
            .await?;

        if response.get("error").is_some() {
            anyhow::bail!("initialize failed: {}", response);
        }

        self.send_notification("notifications/initialized", json!({}))
            .await?;
        Ok(())
    }

    async fn call_tool(&self, name: &str, arguments: Value) -> anyhow::Result<Value> {
        let response = self
            .send_request(
                "tools/call",
                json!({ "name": name, "arguments": arguments }),
            )
            .await?;

        if let Some(err) = response.get("error") {
            anyhow::bail!("tool error: {}", err);
        }

        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn kill(&self) {
        let mut child = self.child.lock().await;
        let _ = child.kill().await;
    }
}

fn extract_text(result: &Value) -> String {
    if let Some(content) = result.get("content").and_then(|c| c.as_array()) {
        content
            .iter()
            .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        serde_json::to_string(result).unwrap_or_default()
    }
}

async fn docker_compose_up() -> anyhow::Result<()> {
    let output = Command::new("docker")
        .args(["compose", "up", "-d"])
        .current_dir(std::env::current_dir()?)
        .output()
        .await?;

    if !output.status.success() {
        anyhow::bail!(
            "docker compose up -d failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

async fn docker_exec_bash(script: &str) -> anyhow::Result<String> {
    let output = Command::new("docker")
        .args([
            "exec",
            "--user",
            "root",
            SSH_CONTAINER,
            "/bin/bash",
            "-lc",
            script,
        ])
        .output()
        .await?;

    if !output.status.success() {
        anyhow::bail!(
            "docker exec failed (status={}): stdout={} stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

async fn restore_remote_sh() -> anyhow::Result<()> {
    docker_exec_bash(&format!(
        r#"set -e
if [ -e '{backup}' ] && [ ! -e /bin/sh ]; then
  mv '{backup}' /bin/sh
fi
test -e /bin/sh
"#,
        backup = SH_BACKUP
    ))
    .await
    .map(|_| ())
}

async fn hide_remote_sh() -> anyhow::Result<()> {
    restore_remote_sh().await?;
    docker_exec_bash(&format!(
        r#"set -e
test -e /bin/sh
rm -f '{backup}'
mv /bin/sh '{backup}'
test ! -e /bin/sh
"#,
        backup = SH_BACKUP
    ))
    .await
    .map(|_| ())
}

async fn wait_for_ssh(max_wait: Duration) -> anyhow::Result<()> {
    let deadline = Instant::now() + max_wait;
    loop {
        if Instant::now() > deadline {
            anyhow::bail!("SSH did not become ready within {:?}", max_wait);
        }
        match tokio::net::TcpStream::connect("127.0.0.1:2222").await {
            Ok(_) => {
                tokio::time::sleep(Duration::from_millis(500)).await;
                return Ok(());
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
        }
    }
}

async fn call_background_exec(client: &McpClient, command: &str) -> String {
    match client
        .call_tool(
            "shell",
            json!({
                "command": command,
                "background": true,
                "timeout_ms": 10000,
            }),
        )
        .await
    {
        Ok(result) => extract_text(&result),
        Err(err) => format!("(tool error: {err})"),
    }
}

async fn call_foreground_exec(client: &McpClient, command: &str) -> String {
    match client
        .call_tool(
            "shell",
            json!({
                "command": command,
                "timeout_ms": 5000,
            }),
        )
        .await
    {
        Ok(result) => extract_text(&result),
        Err(err) => format!("(tool error: {err})"),
    }
}

async fn run_test() -> anyhow::Result<()> {
    println!("╔══════════════════════════════════════════════════════════╗");
    println!("║  Bug #2: Detach Mode Cache Poisoning — E2E Regression    ║");
    println!("╚══════════════════════════════════════════════════════════╝\n");

    let binary = std::env::current_dir()
        .map(|d| d.join("target/release/ssh-mcp"))
        .unwrap_or_else(|_| std::path::PathBuf::from("target/release/ssh-mcp"));

    if !binary.exists() {
        anyhow::bail!("binary not found at {binary:?}; run cargo build --release first");
    }
    let binary_str = binary.to_string_lossy().to_string();

    docker_compose_up().await?;
    wait_for_ssh(Duration::from_secs(20)).await?;
    restore_remote_sh().await?;

    let client = McpClient::spawn(&binary_str).await?;

    println!("Phase 1: create a transient negative detach probe while SSH remains reachable");
    hide_remote_sh().await?;
    println!("  /bin/sh hidden; SSH ping/login shell still available");

    let transient_failure_text = call_background_exec(&client, "sleep 1").await;
    println!("  First background exec: {transient_failure_text}");

    restore_remote_sh().await?;
    println!("  /bin/sh restored");

    println!("\nPhase 2: prove same MCP process re-probes after recovery");
    let recovered_bg_text = call_background_exec(&client, "sleep 1").await;
    println!("  Background exec after restore: {recovered_bg_text}");

    let fg_text = call_foreground_exec(&client, "echo SSH_WORKS").await;
    println!("  Foreground exec after restore: {fg_text}");

    client.kill().await;
    restore_remote_sh().await.ok();

    let transient_failure_observed = transient_failure_text
        .contains("Background detach is not supported")
        || transient_failure_text.contains("not supported");
    let background_recovers = recovered_bg_text.contains("\"ok\":true")
        && !recovered_bg_text.contains("Background detach is not supported")
        && !recovered_bg_text.contains("not supported");
    let foreground_works = fg_text.contains("SSH_WORKS");

    println!("\nAssertions:");
    println!("  transient failure observed before restore: {transient_failure_observed}");
    println!("  background recovered after /bin/sh restore: {background_recovers}");
    println!("  foreground proves SSH recovered: {foreground_works}");

    if !(transient_failure_observed && background_recovers && foreground_works) {
        anyhow::bail!(
            "Bug #2 E2E regression failed: transient_failure_observed={transient_failure_observed}, \
             background_recovers={background_recovers}, foreground_works={foreground_works}"
        );
    }

    println!("\nBUG #2 FIX CONFIRMED: DirectOnly is not cached across recovery");
    Ok(())
}

#[tokio::test]
#[ignore]
async fn cache_poisoning_e2e() {
    if let Err(e) = run_test().await {
        eprintln!("Test error: {e:#}");
        let _ = restore_remote_sh().await;
        let _ = docker_compose_up().await;
        std::process::exit(1);
    }
}
