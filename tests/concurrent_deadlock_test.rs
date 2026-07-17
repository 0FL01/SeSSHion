//! Concurrent crash-test harness for SSH MCP Server.
//!
//! Spawns the ssh-mcp binary as a child process, communicates via JSON-RPC
//! over stdin/stdout (the MCP stdio transport), and fires concurrent tool
//! calls to reproduce the permanent deadlock defect.
//!
//! Usage:
//!   cargo test --test concurrent_deadlock_test -- --nocapture --ignored
//!
//! Requires:
//!   - Docker container running on 127.0.0.1:2222 (docker compose up)
//!   - ssh-mcp binary built: cargo build --release

use std::collections::HashMap;
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

const TOOL_CALL_TIMEOUT: Duration = Duration::from_secs(30);
const DEADLOCK_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone)]
struct McpClient {
    stdin: Arc<Mutex<tokio::process::ChildStdin>>,
    responses: Arc<Mutex<HashMap<u64, mpsc::Sender<Value>>>>,
    child: Arc<Mutex<tokio::process::Child>>,
}

impl McpClient {
    async fn spawn(binary: &str, args: &[&str]) -> anyhow::Result<Self> {
        let mut child = Command::new(binary)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;

        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");

        let stdin = Arc::new(Mutex::new(stdin));
        let responses: Arc<Mutex<HashMap<u64, mpsc::Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
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

        match timeout(TOOL_CALL_TIMEOUT, rx.recv()).await {
            Ok(Some(response)) => {
                let mut map = self.responses.lock().await;
                map.remove(&id);
                Ok(response)
            }
            Ok(None) => anyhow::bail!("response channel closed for id={id}"),
            Err(_) => {
                let mut map = self.responses.lock().await;
                map.remove(&id);
                anyhow::bail!(
                    "request id={id} timed out after {}s",
                    TOOL_CALL_TIMEOUT.as_secs()
                )
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
                    "clientInfo": {
                        "name": "concurrent-test-harness",
                        "version": "1.0.0"
                    }
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
                json!({
                    "name": name,
                    "arguments": arguments,
                }),
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

struct CallResult {
    label: String,
    success: bool,
    error: Option<String>,
    elapsed: Duration,
    response_snippet: String,
}

/// Run concurrent tool calls, each as a separate tokio task sharing the client.
async fn run_concurrent_batch(
    client: &McpClient,
    calls: Vec<(String, String, Value)>, // (label, tool_name, arguments)
) -> Vec<CallResult> {
    let mut handles: Vec<tokio::task::JoinHandle<CallResult>> = Vec::new();

    for (label, tool_name, args) in calls {
        let client = client.clone();
        let handle: tokio::task::JoinHandle<CallResult> = tokio::spawn(async move {
            let start = Instant::now();
            let result = client.call_tool(&tool_name, args).await;
            let elapsed = start.elapsed();

            match result {
                Ok(resp) => {
                    let snippet = serde_json::to_string(&resp)
                        .unwrap_or_default()
                        .chars()
                        .take(200)
                        .collect();
                    CallResult {
                        label,
                        success: true,
                        error: None,
                        elapsed,
                        response_snippet: snippet,
                    }
                }
                Err(e) => CallResult {
                    label,
                    success: false,
                    error: Some(e.to_string()),
                    elapsed,
                    response_snippet: String::new(),
                },
            }
        });
        handles.push(handle);
    }

    let mut results = Vec::new();
    for handle in handles {
        match timeout(DEADLOCK_TIMEOUT, handle).await {
            Ok(Ok(r)) => results.push(r),
            Ok(Err(e)) => results.push(CallResult {
                label: "join_error".to_string(),
                success: false,
                error: Some(format!("task join error: {e}")),
                elapsed: DEADLOCK_TIMEOUT,
                response_snippet: String::new(),
            }),
            Err(_) => results.push(CallResult {
                label: "DEADLOCK".to_string(),
                success: false,
                error: Some(format!(
                    "task timed out after {}s (DEADLOCK)",
                    DEADLOCK_TIMEOUT.as_secs()
                )),
                elapsed: DEADLOCK_TIMEOUT,
                response_snippet: String::new(),
            }),
        }
    }
    results
}

fn print_results(title: &str, results: &[CallResult]) {
    let bar = "=".repeat(60);
    println!("\n{bar}");
    println!("  {title}");
    println!("{bar}");
    let pass = results.iter().filter(|r| r.success).count();
    let fail = results
        .iter()
        .filter(|r| !r.success && r.label != "DEADLOCK")
        .count();
    let deadlock = results.iter().filter(|r| r.label == "DEADLOCK").count();
    println!("  Summary: {pass} pass, {fail} fail, {deadlock} deadlock");
    println!();
    for r in results {
        let status = if r.success {
            "PASS"
        } else if r.label == "DEADLOCK" {
            "*** DEADLOCK ***"
        } else {
            "FAIL"
        };
        let detail = r.error.as_deref().unwrap_or(&r.response_snippet);
        println!("  [{status}] {} — {:?} — {detail}", r.label, r.elapsed);
    }
    println!();
}

async fn check_alive(client: &McpClient, label: &str) -> bool {
    let start = Instant::now();
    match timeout(
        Duration::from_secs(15),
        client.call_tool(
            "shell",
            json!({
                "command": "echo ALIVE_CHECK",
                "timeout_ms": 5000,
            }),
        ),
    )
    .await
    {
        Ok(Ok(resp)) => {
            let elapsed = start.elapsed();
            let contains = serde_json::to_string(&resp).is_ok_and(|s| s.contains("ALIVE_CHECK"));
            println!("  [ALIVE] {label} — {elapsed:?} — ok={contains}");
            contains
        }
        Ok(Err(e)) => {
            let elapsed = start.elapsed();
            println!("  [DEAD] {label} — {elapsed:?} — error: {e}");
            false
        }
        Err(_) => {
            let elapsed = start.elapsed();
            println!("  [DEAD] {label} — {elapsed:?} — TIMED OUT (server unresponsive)");
            false
        }
    }
}

async fn run_test() -> anyhow::Result<()> {
    println!("SSH MCP Server — Concurrent Deadlock Reproduction Test");
    println!("=========================================================\n");

    let binary = std::env::current_dir()
        .map(|d| d.join("target/release/ssh-mcp"))
        .unwrap_or_else(|_| std::path::PathBuf::from("target/release/ssh-mcp"));

    if !binary.exists() {
        eprintln!("ERROR: Binary not found at {binary:?}");
        eprintln!("Build it first: cargo build --release");
        std::process::exit(1);
    }

    let binary_str = binary.to_string_lossy().to_string();

    let args: &[&str] = &[
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

    println!("Spawning ssh-mcp: {binary_str}\n");

    let client = McpClient::spawn(&binary_str, args).await?;

    // Phase 1: Baseline
    println!("Phase 1: Baseline connectivity check");
    if !check_alive(&client, "baseline").await {
        eprintln!("FATAL: Server not responsive at baseline. Aborting.");
        client.kill().await;
        std::process::exit(1);
    }

    // Phase 2: Concurrent cascade — the agent's exact scenario
    println!("\nPhase 2: Concurrent cascade — 9 simultaneous exec calls");
    println!("  (reproducing: 9 concurrent exec → connection teardown → deadlock)\n");

    let commands = [
        "yes | tr -d '\\n' | head -c 5000000",
        "cat /dev/urandom | head -c 100000",
        "kill -SEGV $$",
        "kill -KILL $$",
        "cat",
        "yes | tr -d '\\n' | head -c 3000000",
        "sleep 10",
        "echo simple_7",
        "echo simple_8",
    ];

    let calls: Vec<(String, String, Value)> = commands
        .iter()
        .enumerate()
        .map(|(i, cmd)| {
            (
                format!("call_{i}: {cmd}"),
                "shell".to_string(),
                json!({
                    "command": cmd,
                    "timeout_ms": 10000,
                }),
            )
        })
        .collect();

    let results = run_concurrent_batch(&client, calls).await;
    print_results("Phase 2: Concurrent Cascade Results", &results);

    // Phase 3: Post-cascade liveness
    println!("Phase 3: Post-cascade liveness check");
    let alive = check_alive(&client, "post_cascade").await;

    if !alive {
        println!("\n*** DEFECT REPRODUCED: Server DEAD after concurrent cascade ***\n");
        for attempt in 1..=3 {
            tokio::time::sleep(Duration::from_secs(2)).await;
            check_alive(&client, &format!("recovery_attempt_{attempt}")).await;
        }
    } else {
        println!("\n  Server survived cascade. Trying semaphore saturation.\n");

        // Phase 4: Semaphore saturation — 12 concurrent (capacity=8)
        println!("Phase 4: Semaphore saturation — 12 concurrent calls (cap=8)");
        let calls2: Vec<(String, String, Value)> = (0..12)
            .map(|i| {
                let cmd = if i < 8 {
                    "yes | head -c 2000000"
                } else {
                    "echo overflow"
                };
                (
                    format!("sat_{i}: {cmd}"),
                    "shell".to_string(),
                    json!({
                        "command": cmd,
                        "timeout_ms": 8000,
                    }),
                )
            })
            .collect();

        let results2 = run_concurrent_batch(&client, calls2).await;
        print_results("Phase 4: Semaphore Saturation Results", &results2);

        let alive2 = check_alive(&client, "post_saturation").await;

        if !alive2 {
            println!("\n*** DEFECT REPRODUCED: Server dead after semaphore saturation ***\n");
            for attempt in 1..=3 {
                tokio::time::sleep(Duration::from_secs(2)).await;
                check_alive(&client, &format!("recovery_{attempt}")).await;
            }
        } else {
            // Phase 5: Connection drop during concurrent load
            println!("\nPhase 5: Connection drop during concurrent load");
            let calls3: Vec<(String, String, Value)> = (0..6)
                .map(|i| {
                    let cmd = if i == 3 {
                        "pgrep sshd | head -1 | xargs kill -9; echo KILLED"
                    } else {
                        "yes | head -c 1000000"
                    };
                    (
                        format!("drop_{i}: {cmd}"),
                        "shell".to_string(),
                        json!({
                            "command": cmd,
                            "timeout_ms": 10000,
                        }),
                    )
                })
                .collect();

            let results3 = run_concurrent_batch(&client, calls3).await;
            print_results("Phase 5: Connection Drop Results", &results3);

            tokio::time::sleep(Duration::from_secs(3)).await;
            let alive3 = check_alive(&client, "post_drop").await;

            if !alive3 {
                println!("\n*** DEFECT REPRODUCED: Server dead after connection drop ***\n");
                for attempt in 1..=5 {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    check_alive(&client, &format!("recovery_{attempt}")).await;
                }
            } else {
                println!("\n  Server survived all standard scenarios.");
            }
        }
    }

    // Phase 6: Host key change under concurrent load (agent's actual scenario)
    println!("\nPhase 6: Host key change under concurrent load");
    println!("  (agent reported: container recreation → host key mismatch → deadlock)\n");

    // Kill current server and restart with accept-new + custom known_hosts
    client.kill().await;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Create fresh known_hosts file
    let known_hosts_path = "/tmp/ssh_mcp_test_known_hosts";
    let _ = std::fs::remove_file(known_hosts_path);

    let args_accept: &[&str] = &[
        "--host",
        "127.0.0.1",
        "--port",
        "2222",
        "--user",
        "test",
        "--password",
        "secret",
        "--strict-host-key-checking",
        "accept-new",
        "--known-hosts",
        known_hosts_path,
        "--log-level",
        "warn",
        "--timeout",
        "30000",
    ];

    println!("  Spawning ssh-mcp with accept-new + custom known_hosts\n");
    let client2 = match McpClient::spawn(&binary_str, args_accept).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("  Failed to spawn server with accept-new: {e}");
            println!("\nCleanup: killing server process");
            return Ok(());
        }
    };

    // Baseline with accept-new (key gets added to known_hosts)
    if !check_alive(&client2, "accept_new_baseline").await {
        eprintln!("  FATAL: Server not responsive with accept-new. Skipping phase 6.");
        client2.kill().await;
        return Ok(());
    }

    println!("  Baseline OK. Key added to known_hosts.");

    // Recreate container completely to generate new host keys
    println!("\n  Recreating Docker container (docker compose down + up, changes host key)...");
    let down = tokio::process::Command::new("docker")
        .args(["compose", "down"])
        .output()
        .await;
    match &down {
        Ok(out) => {
            if !out.status.success() {
                println!(
                    "  docker compose down failed: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
            }
        }
        Err(e) => {
            println!("  Could not run docker compose down: {e}");
            println!("  Skipping host key change test.");
            client2.kill().await;
            return Ok(());
        }
    }

    let up = tokio::process::Command::new("docker")
        .args(["compose", "up", "-d"])
        .output()
        .await;
    match &up {
        Ok(out) => {
            if !out.status.success() {
                println!(
                    "  docker compose up failed: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
            }
        }
        Err(e) => {
            println!("  Could not run docker compose up: {e}");
            println!("  Skipping host key change test.");
            client2.kill().await;
            return Ok(());
        }
    }

    // Wait for container to come back up with new host key
    println!("  Waiting for new container to start...");
    tokio::time::sleep(Duration::from_secs(15)).await;

    // Now fire concurrent calls — host key has changed, known_hosts has old key
    // accept-new should REJECT the changed key → all calls fail
    println!("\n  Firing 9 concurrent calls with stale host key...\n");

    let commands6 = [
        "echo call_0",
        "yes | tr -d '\\n' | head -c 5000000",
        "kill -SEGV $$",
        "kill -KILL $$",
        "cat /dev/urandom | head -c 100000",
        "echo call_5",
        "sleep 5",
        "echo call_7",
        "echo call_8",
    ];

    let calls6: Vec<(String, String, Value)> = commands6
        .iter()
        .enumerate()
        .map(|(i, cmd)| {
            (
                format!("hostkey_{i}: {cmd}"),
                "shell".to_string(),
                json!({
                    "command": cmd,
                    "timeout_ms": 10000,
                }),
            )
        })
        .collect();

    let results6 = run_concurrent_batch(&client2, calls6).await;
    print_results("Phase 6: Host Key Mismatch Results", &results6);

    // Check liveness — this is where the agent saw permanent deadlock
    println!("Phase 6b: Post-host-key-change liveness check");
    let alive6 = check_alive(&client2, "post_hostkey_change").await;

    if !alive6 {
        println!(
            "\n*** DEFECT REPRODUCED: Server DEAD after host key change + concurrent load ***\n"
        );
        for attempt in 1..=5 {
            tokio::time::sleep(Duration::from_secs(3)).await;
            check_alive(&client2, &format!("recovery_{attempt}")).await;
        }
    } else {
        println!("\n  Server survived host key change scenario.");
    }

    println!("\nCleanup: killing server process");
    client2.kill().await;

    Ok(())
}

#[tokio::test]
#[ignore = "requires Docker container running on 127.0.0.1:2222 and cargo build --release"]
async fn concurrent_deadlock_reproduction() -> anyhow::Result<()> {
    run_test().await
}
