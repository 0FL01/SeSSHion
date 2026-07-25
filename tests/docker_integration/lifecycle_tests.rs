#![cfg(unix)]

use super::common::*;
use std::process::Stdio;
use std::time::Duration;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::timeout;

const PROCESS_EXIT_TIMEOUT: Duration = Duration::from_secs(5);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);

struct McpProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    _temp_dir: TempDir,
}

impl McpProcess {
    async fn spawn(host: &str, port: u16) -> Self {
        let temp_dir = tempfile::tempdir().expect("create isolated lifecycle temp dir");
        let mut child = Command::new(env!("CARGO_BIN_EXE_ssh-mcp"))
            .arg("--host")
            .arg(host)
            .arg("--port")
            .arg(port.to_string())
            .args([
                "--user",
                "test",
                "--password",
                "secret",
                "--strict-host-key-checking",
                "no",
                "--disable-sudo",
            ])
            .env("TMPDIR", temp_dir.path())
            .current_dir(temp_dir.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn ssh-mcp binary");

        let stdin = child.stdin.take().expect("child stdin");
        let stdout = child.stdout.take().expect("child stdout");

        Self {
            child,
            stdin: Some(stdin),
            stdout: BufReader::new(stdout),
            _temp_dir: temp_dir,
        }
    }

    async fn send(&mut self, message: Value) {
        let stdin = self.stdin.as_mut().expect("child stdin is open");
        stdin
            .write_all(format!("{message}\n").as_bytes())
            .await
            .expect("write MCP message");
        stdin.flush().await.expect("flush MCP message");
    }

    async fn response(&mut self, expected_id: u64) -> Value {
        timeout(RESPONSE_TIMEOUT, async {
            loop {
                let mut line = String::new();
                let read = self
                    .stdout
                    .read_line(&mut line)
                    .await
                    .expect("read MCP response");
                assert_ne!(read, 0, "MCP stdout closed before response {expected_id}");

                let response: Value = serde_json::from_str(&line).expect("valid MCP response");
                if response.get("id").and_then(Value::as_u64) == Some(expected_id) {
                    return response;
                }
            }
        })
        .await
        .expect("timed out waiting for MCP response")
    }

    async fn initialize(&mut self) {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "lifecycle-test", "version": "1.0.0"}
            }
        }))
        .await;
        let response = self.response(1).await;
        assert!(
            response.get("error").is_none(),
            "initialize failed: {response}"
        );
        self.send(json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        }))
        .await;
    }

    fn signal(&self, signal: Signal) {
        let pid = self.child.id().expect("child pid");
        kill(Pid::from_raw(pid as i32), signal).expect("send shutdown signal");
    }

    async fn close_stdin(&mut self) {
        self.stdin.take();
    }

    async fn assert_successful_exit(&mut self) {
        let status = timeout(PROCESS_EXIT_TIMEOUT, self.child.wait())
            .await
            .expect("ssh-mcp did not exit after lifecycle shutdown")
            .expect("wait for ssh-mcp process");
        assert!(status.success(), "ssh-mcp exited with {status}");
    }
}

fn tool_text(response: &Value) -> &str {
    response["result"]["content"][0]["text"]
        .as_str()
        .expect("tool response text")
}

async fn wait_for_tcp(host: &str, port: u16) {
    timeout(Duration::from_secs(10), async {
        loop {
            if tokio::net::TcpStream::connect((host, port)).await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("SSH test container did not become ready");
}

#[tokio::test]
async fn sigterm_stops_server_during_initialization() {
    let mut process = McpProcess::spawn("127.0.0.1", 9).await;
    process
        .send(json!({"jsonrpc": "2.0", "id": 7, "method": "ping", "params": {}}))
        .await;
    let response = process.response(7).await;
    assert!(
        response.get("error").is_none(),
        "pre-init ping failed: {response}"
    );

    process.signal(Signal::SIGTERM);
    process.assert_successful_exit().await;
}

#[tokio::test]
async fn sigint_stops_initialized_server() {
    let mut process = McpProcess::spawn("127.0.0.1", 9).await;
    process.initialize().await;

    process.signal(Signal::SIGINT);
    process.assert_successful_exit().await;
}

#[tokio::test]
async fn stdin_eof_stops_initialized_server() {
    let mut process = McpProcess::spawn("127.0.0.1", 9).await;
    process.initialize().await;

    process.close_stdin().await;
    process.assert_successful_exit().await;
}

#[tokio::test]
async fn read_tool_is_not_advertised_or_callable() {
    let mut process = McpProcess::spawn("127.0.0.1", 9).await;
    process.initialize().await;

    process
        .send(json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        }))
        .await;
    let response = process.response(2).await;
    let tools = response["result"]["tools"]
        .as_array()
        .expect("tools/list result");
    let names = tools
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name"))
        .collect::<Vec<_>>();
    assert_eq!(names, ["shell", "check_process", "transfer", "apply_patch"]);

    process
        .send(json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {"name": "read", "arguments": {}}
        }))
        .await;
    let response = process.response(3).await;
    assert!(
        response["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("Unknown tool: read")),
        "unexpected read response: {response}"
    );

    process.close_stdin().await;
    process.assert_successful_exit().await;
}

#[tokio::test]
async fn signal_cancels_scheduled_check_before_ssh_cleanup() {
    init_test_env().expect("Failed to initialize test environment");
    let container = GenericImage::new("ssh-mcp-debian-sshd", "latest")
        .with_exposed_port(2222u16.into())
        .start()
        .await
        .expect("start SSH test container");
    let host = container.get_host().await.expect("get container host");
    let port = container
        .get_host_port_ipv4(2222)
        .await
        .expect("get mapped SSH port");
    wait_for_tcp(&host.to_string(), port).await;

    let mut process = McpProcess::spawn(&host.to_string(), port).await;
    process.initialize().await;
    process
        .send(json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "shell",
                "arguments": {"command": "sleep 20", "background": true}
            }
        }))
        .await;
    let background = process.response(2).await;
    assert!(
        background.get("error").is_none(),
        "shell failed: {background}"
    );
    let background: Value =
        serde_json::from_str(tool_text(&background)).expect("background response JSON");
    let job_id = background["job_id"].as_str().expect("background job id");

    process
        .send(json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "check_process",
                "arguments": {"job_id": job_id, "wait_for": 600, "tail_lines": 10}
            }
        }))
        .await;
    process
        .send(json!({"jsonrpc": "2.0", "id": 4, "method": "ping", "params": {}}))
        .await;
    process.response(4).await;

    process.signal(Signal::SIGTERM);
    let cancelled = process.response(3).await;
    assert!(
        cancelled.get("error").is_none(),
        "scheduled check failed during shutdown: {cancelled}"
    );
    let status: Value =
        serde_json::from_str(tool_text(&cancelled)).expect("check_process response JSON");
    assert_eq!(status["state"], "running");
    assert_eq!(status["running"], true);
    process.assert_successful_exit().await;
}
