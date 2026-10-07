#![cfg(unix)]

use super::common::*;
use std::ffi::{OsStr, OsString};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use serde_json::{Value, json};
use tempfile::TempDir;
use testcontainers::{ContainerAsync, core::ExecCommand};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::timeout;

const PROCESS_EXIT_TIMEOUT: Duration = Duration::from_secs(5);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);
// Cold readiness (10s) precedes the optional metadata probe (3s).
const STARTUP_RESPONSE_TIMEOUT: Duration = Duration::from_secs(15);
const COLD_FAILURE_TIMEOUT: Duration = Duration::from_secs(13);
const RAW_TRANSFER_DESCRIPTION: &str =
    "Files/dirs. Prefer transport=exec-raw (startup; may be stale).";

struct McpProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    stderr: tokio::task::JoinHandle<String>,
    first_response: bool,
    started: tokio::time::Instant,
    spool_dir: PathBuf,
    _temp_dir: TempDir,
}

impl McpProcess {
    async fn spawn(host: &str, port: u16) -> Self {
        let auth_args = [
            OsString::from("--user=test"),
            OsString::from("--password=secret"),
            OsString::from("--strict-host-key-checking=no"),
        ];
        Self::spawn_with_auth(host, port, None, None, &auth_args).await
    }

    async fn spawn_with_auth(
        host: &str,
        port: u16,
        current_dir: Option<&Path>,
        home: Option<&Path>,
        auth_args: &[OsString],
    ) -> Self {
        let temp_dir = tempfile::tempdir().expect("create isolated lifecycle temp dir");
        let spool_dir = temp_dir.path().join("spool");
        let mut command = Command::new(env!("CARGO_BIN_EXE_ssh-mcp"));
        // Inherited keys, jump credentials and timing overrides must not turn a
        // rejected-password case into successful authentication.
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("SSH_MCP_") {
                command.env_remove(name);
            }
        }
        command
            .arg("--host")
            .arg(host)
            .arg("--port")
            .arg(port.to_string())
            .args(auth_args)
            .env("SSH_MCP_SPOOL_DIR", &spool_dir)
            .current_dir(current_dir.unwrap_or_else(|| temp_dir.path()))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(home) = home {
            command.env("HOME", home);
        }
        let started = tokio::time::Instant::now();
        let mut child = command.spawn().expect("spawn ssh-mcp binary");

        let stdin = child.stdin.take().expect("child stdin");
        let stdout = child.stdout.take().expect("child stdout");
        let mut stderr = child.stderr.take().expect("child stderr");
        let stderr = tokio::spawn(async move {
            let mut bytes = Vec::new();
            stderr.read_to_end(&mut bytes).await.expect("read stderr");
            String::from_utf8_lossy(&bytes).into_owned()
        });

        Self {
            child,
            stdin: Some(stdin),
            stdout: BufReader::new(stdout),
            stderr,
            first_response: true,
            started,
            spool_dir,
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
        let budget = if self.first_response {
            STARTUP_RESPONSE_TIMEOUT
        } else {
            RESPONSE_TIMEOUT
        };
        self.first_response = false;
        timeout(budget, async {
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

    async fn initialize(&mut self) -> Value {
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
        response
    }

    fn signal(&self, signal: Signal) {
        let pid = self.child.id().expect("child pid");
        kill(Pid::from_raw(pid as i32), signal).expect("send shutdown signal");
    }

    fn assert_spool_dir_created(&self) {
        assert!(
            self.spool_dir.is_dir(),
            "configured spool directory was not created: {}",
            self.spool_dir.display()
        );
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

    async fn assert_cold_failure(&mut self, reasons: &[&str], secrets: &[&str]) {
        let (status, stdout, stderr) = timeout(
            COLD_FAILURE_TIMEOUT.saturating_sub(self.started.elapsed()),
            async {
                let status = self.child.wait().await.expect("wait for cold failure");
                let mut stdout = String::new();
                self.stdout.read_to_string(&mut stdout).await.unwrap();
                let stderr = (&mut self.stderr).await.expect("stderr reader");
                (status, stdout, stderr)
            },
        )
        .await
        .expect("cold failure exceeded the readiness deadline and shutdown slack");
        assert!(!status.success(), "cold startup unexpectedly succeeded");
        assert!(
            stdout.is_empty(),
            "cold failure emitted MCP output: {stdout}"
        );
        assert!(
            stderr.lines().any(|line| {
                let line = line.to_lowercase();
                [
                    "error", "failed", "failure", "rejected", "refused", "deadline",
                ]
                .iter()
                .any(|marker| line.contains(marker))
                    && reasons.iter().any(|reason| line.contains(reason))
            }),
            "cold failure omitted its diagnostic reason: {stderr}"
        );
        for secret in secrets {
            assert!(!stderr.contains(secret), "stderr disclosed credentials");
        }
    }
}

fn tool_text(response: &Value) -> &str {
    response["result"]["content"][0]["text"]
        .as_str()
        .expect("tool response text")
}

fn transfer_description(definitions: &Value) -> &str {
    definitions["tools"]
        .as_array()
        .expect("tools/list definitions")
        .iter()
        .find(|tool| tool["name"] == "transfer")
        .expect("transfer tool")["description"]
        .as_str()
        .expect("transfer tool description")
}

fn assert_six_tool_budget(definitions: &Value) {
    assert_eq!(definitions["tools"].as_array().unwrap().len(), 6);
    let bytes = serde_json::to_vec(&definitions["tools"]).unwrap().len();
    assert!(
        bytes <= 3200,
        "six-tool definitions exceeded 3200 bytes: {bytes}"
    );
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

async fn ssh_fixture() -> (ContainerAsync<GenericImage>, String, u16) {
    init_test_env().expect("initialize Docker fixtures");
    let container = GenericImage::new("ssh-mcp-debian-sshd", "latest")
        .with_exposed_port(2222u16.into())
        .start()
        .await
        .expect("start SSH fixture");
    let host = container.get_host().await.unwrap().to_string();
    let port = container.get_host_port_ipv4(2222).await.unwrap();
    wait_for_tcp(&host, port).await;
    (container, host, port)
}

async fn control(container: &ContainerAsync<GenericImage>, script: &str) -> String {
    let mut output = container
        .exec(ExecCommand::new(["sh", "-c", script]))
        .await
        .expect("control SSH fixture");
    let bytes = output.stdout_to_vec().await.unwrap();
    assert_eq!(output.exit_code().await.unwrap(), Some(0), "{script}");
    String::from_utf8(bytes).unwrap()
}

const START_SSHD: &str = "/usr/sbin/sshd -E /run/lifecycle-sshd.log -o LogLevel=VERBOSE -o PidFile=/run/lifecycle-sshd.pid";
const STOP_SSHD: &str = r#"
set -eu
for file in /proc/[0-9]*/comm; do
    name=$(cat "$file" 2>/dev/null) || continue
    case "$name" in sshd*) pid=${file#/proc/}; pid=${pid%/comm}; kill -KILL "$pid" 2>/dev/null || true;; esac
done
"#;

async fn controlled_ssh_fixture() -> (ContainerAsync<GenericImage>, String, u16) {
    init_test_env().expect("initialize Docker fixtures");
    // Keep PID 1 alive while killing the listener AND authenticated children.
    // Restarting sshd in this container preserves mapped port and host keys.
    let container = GenericImage::new("ssh-mcp-debian-sshd", "latest")
        .with_exposed_port(2222u16.into())
        .with_entrypoint("sh")
        .with_cmd(["-c", "sleep infinity"])
        .start()
        .await
        .unwrap();
    let host = container.get_host().await.unwrap().to_string();
    let port = container.get_host_port_ipv4(2222).await.unwrap();
    control(&container, START_SSHD).await;
    wait_for_tcp(&host, port).await;
    (container, host, port)
}

async fn authenticated_sessions(container: &ContainerAsync<GenericImage>) -> usize {
    control(
        container,
        "grep -c 'Accepted password for test ' /run/lifecycle-sshd.log || true",
    )
    .await
    .trim()
    .parse()
    .expect("authenticated session counter")
}

#[tokio::test]
async fn sigterm_stops_server_during_initialization() {
    let (_container, host, port) = ssh_fixture().await;
    let mut process = McpProcess::spawn(&host, port).await;
    process
        .send(json!({"jsonrpc": "2.0", "id": 7, "method": "ping", "params": {}}))
        .await;
    let response = process.response(7).await;
    assert!(
        response.get("error").is_none(),
        "pre-init ping failed: {response}"
    );
    process.assert_spool_dir_created();

    process.signal(Signal::SIGTERM);
    process.assert_successful_exit().await;
}

#[tokio::test]
async fn sigint_stops_initialized_server() {
    let (_container, host, port) = ssh_fixture().await;
    let mut process = McpProcess::spawn(&host, port).await;
    process.initialize().await;

    process.signal(Signal::SIGINT);
    process.assert_successful_exit().await;
}

#[tokio::test]
async fn stdin_eof_stops_initialized_server() {
    let (_container, host, port) = ssh_fixture().await;
    let mut process = McpProcess::spawn(&host, port).await;
    process.initialize().await;

    process.close_stdin().await;
    process.assert_successful_exit().await;
}

#[tokio::test]
async fn modern_stdio_discovery_and_tool_results() {
    let (_container, host, port) = ssh_fixture().await;
    let mut process = McpProcess::spawn(&host, port).await;
    let meta = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": {
            "name": "lifecycle-test", "version": "1.0.0"
        },
        "io.modelcontextprotocol/clientCapabilities": {}
    });

    // Modern stdio requests carry their own metadata instead of initialize.
    process
        .send(json!({
            "jsonrpc": "2.0", "id": 1, "method": "server/discover",
            "params": {"_meta": meta}
        }))
        .await;
    let response = process.response(1).await;
    assert!(response.get("error").is_none(), "{response}");
    assert!(
        response["result"]["supportedVersions"]
            .as_array()
            .expect("supported versions")
            .contains(&json!("2026-07-28"))
    );
    assert_eq!(response["result"]["capabilities"], json!({"tools": {}}));
    assert_eq!(
        response["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "ssh-mcp"
    );

    process
        .send(json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/list",
            "params": {"_meta": meta}
        }))
        .await;
    let response = process.response(2).await;
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(response["result"]["resultType"], "complete");
    let tools = response["result"]["tools"].as_array().expect("tools");
    assert_eq!(tools.len(), 6);
    assert!(tools.iter().any(|tool| tool["name"] == "check_process"));

    process
        .send(json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {
                "_meta": meta,
                "name": "check_process",
                "arguments": {"job_id": "missing-job"}
            }
        }))
        .await;
    let response = process.response(3).await;
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(response["result"]["resultType"], "complete");
    assert_eq!(response["result"]["isError"], true);
    assert!(tool_text(&response).contains("job not found: missing-job"));

    process.close_stdin().await;
    process.assert_successful_exit().await;
}

#[tokio::test]
async fn default_tool_surface_is_exact_and_read_is_unknown() {
    let (_container, host, port) = ssh_fixture().await;
    let mut process = McpProcess::spawn(&host, port).await;
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
    assert_eq!(
        names,
        [
            "shell",
            "sudo_shell",
            "sudo_apply_patch",
            "check_process",
            "transfer",
            "apply_patch",
        ]
    );

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
async fn cold_unavailable_ssh_fails_then_fresh_launch_succeeds_on_same_endpoint() {
    let (container, host, port) = controlled_ssh_fixture().await;
    for modern in [false, true] {
        control(&container, STOP_SSHD).await;
        let mut failed = McpProcess::spawn(&host, port).await;
        failed.send(opening_request(modern)).await;
        failed
            .assert_cold_failure(&["connect", "refused", "unavailable"], &["secret"])
            .await;

        control(&container, START_SSHD).await;
        wait_for_tcp(&host, port).await;
        let mut fresh = McpProcess::spawn(&host, port).await;
        fresh.send(opening_request(modern)).await;
        let response = fresh.response(1).await;
        assert!(response.get("error").is_none(), "{response}");
        if !modern {
            fresh
                .send(json!({"jsonrpc":"2.0", "method":"notifications/initialized", "params":{}}))
                .await;
        }
        let mut params = opening_request(modern)["params"].clone();
        params
            .as_object_mut()
            .unwrap()
            .retain(|key, _| key == "_meta");
        params["name"] = json!("shell");
        params["arguments"] = json!({"command":"printf fresh"});
        fresh
            .send(json!({"jsonrpc":"2.0", "id":2, "method":"tools/call", "params":params}))
            .await;
        let result = fresh.response(2).await;
        assert_eq!(tool_text(&result), "fresh", "{result}");
        fresh.close_stdin().await;
        fresh.assert_successful_exit().await;
    }
}

fn opening_request(modern: bool) -> Value {
    if modern {
        json!({"jsonrpc":"2.0", "id":1, "method":"tools/list", "params":{"_meta":{
            "io.modelcontextprotocol/protocolVersion":"2026-07-28",
            "io.modelcontextprotocol/clientInfo":{"name":"cold-test", "version":"1"},
            "io.modelcontextprotocol/clientCapabilities":{}
        }}})
    } else {
        json!({"jsonrpc":"2.0", "id":1, "method":"initialize", "params":{
            "protocolVersion":"2024-11-05", "capabilities":{},
            "clientInfo":{"name":"cold-test", "version":"1"}
        }})
    }
}

#[tokio::test]
async fn password_only_tools_list_recommends_exec_raw_without_initialize() {
    let (_container, host, port) = ssh_fixture().await;
    let mut process = McpProcess::spawn(&host, port).await;
    // A client can expose tools to the model without requesting instructions.
    process.send(opening_request(true)).await;
    let listed = process.response(1).await;
    assert!(listed.get("error").is_none(), "{listed}");
    assert_six_tool_budget(&listed["result"]);
    assert_eq!(
        transfer_description(&listed["result"]),
        RAW_TRANSFER_DESCRIPTION
    );

    process.close_stdin().await;
    process.assert_successful_exit().await;
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn key_authenticated_tools_list_recommends_sftp() {
    assert!(
        check_sftp() && check_openssh_client("ssh"),
        "startup SFTP wire coverage requires local OpenSSH ssh and sftp clients"
    );
    let (_container, host, port) = ssh_fixture().await;
    let (_key_dir, key_path) = setup_test_key();
    let auth = [
        OsString::from("--user=test"),
        OsString::from(format!("--key={}", key_path.display())),
        OsString::from("--strict-host-key-checking=no"),
    ];
    let mut process = McpProcess::spawn_with_auth(&host, port, None, None, &auth).await;
    process.send(opening_request(true)).await;
    let listed = process.response(1).await;
    assert!(listed.get("error").is_none(), "{listed}");
    assert_six_tool_budget(&listed["result"]);
    assert_eq!(
        transfer_description(&listed["result"]),
        "Files/dirs. Prefer transport=sftp (startup; may be stale)."
    );

    process.close_stdin().await;
    process.assert_successful_exit().await;
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn key_authenticated_closed_sftp_tools_list_recommends_exec_raw() {
    assert!(
        check_sftp() && check_openssh_client("ssh"),
        "closed SFTP wire coverage requires local OpenSSH ssh and sftp clients"
    );
    let (container, host, port) = ssh_fixture().await;
    // Reconfigure only this disposable fixture, keeping key auth and exec open.
    control(
        &container,
        r#"
set -eu
sed -i 's|^[[:space:]]*Subsystem[[:space:]][[:space:]]*sftp[[:space:]].*$|Subsystem sftp /bin/false|; s|^PasswordAuthentication .*|PasswordAuthentication no|' /etc/ssh/sshd_config
/usr/sbin/sshd -t
kill -HUP "$(cat /run/sshd.pid)"
"#,
    )
    .await;
    wait_for_tcp(&host, port).await;
    let (_key_dir, key_path) = setup_test_key();
    let auth = [
        OsString::from("--user=test"),
        OsString::from(format!("--key={}", key_path.display())),
        OsString::from("--strict-host-key-checking=no"),
    ];
    let mut process = McpProcess::spawn_with_auth(&host, port, None, None, &auth).await;
    process.send(opening_request(true)).await;
    let listed = process.response(1).await;
    assert!(listed.get("error").is_none(), "{listed}");
    assert_six_tool_budget(&listed["result"]);
    assert_eq!(
        transfer_description(&listed["result"]),
        RAW_TRANSFER_DESCRIPTION
    );

    process.close_stdin().await;
    process.assert_successful_exit().await;
}

#[tokio::test]
async fn inconclusive_tools_list_keeps_auto_after_probe_repair() {
    let (container, host, port) = ssh_fixture().await;
    control(&container, METADATA_FIXTURE).await;
    control(
        &container,
        r#"
set -eu
cat > /home/test/probebin/cat <<'SCRIPT'
#!/bin/sh
/bin/cat >/dev/null
printf wrong
SCRIPT
chmod +x /home/test/probebin/cat
"#,
    )
    .await;
    let mut process = McpProcess::spawn(&host, port).await;
    process.send(opening_request(true)).await;
    let listed = process.response(1).await;
    assert!(listed.get("error").is_none(), "{listed}");
    let definitions = &listed["result"];
    assert_six_tool_budget(definitions);
    assert_eq!(
        transfer_description(definitions),
        "Files/dirs. Startup unverified; default transport=auto."
    );

    control(&container, "rm /home/test/probebin/cat").await;
    let mut repeated = opening_request(true);
    repeated["id"] = json!(2);
    process.send(repeated).await;
    let repeated = process.response(2).await;
    assert!(repeated.get("error").is_none(), "{repeated}");
    assert_eq!(&repeated["result"], definitions);

    process.close_stdin().await;
    process.assert_successful_exit().await;
}

#[tokio::test]
async fn cold_rejected_password_has_no_mcp_response_or_credential_leak() {
    let (_container, host, port) = ssh_fixture().await;
    let password = "lifecycle-wrong-password-private";
    let auth = [
        OsString::from("--user=test"),
        OsString::from(format!("--password={password}")),
        OsString::from("--strict-host-key-checking=no"),
    ];
    let mut process = McpProcess::spawn_with_auth(&host, port, None, None, &auth).await;
    process.send(opening_request(false)).await;
    process
        .assert_cold_failure(&["auth", "credentials", "rejected"], &[password])
        .await;
}

#[tokio::test]
async fn cold_silent_tcp_handshake_exits_at_total_readiness_deadline() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut process = McpProcess::spawn("127.0.0.1", listener.local_addr().unwrap().port()).await;
    process.send(opening_request(true)).await;
    let (_silent_socket, _) = timeout(Duration::from_secs(2), listener.accept())
        .await
        .unwrap()
        .unwrap();
    process
        .assert_cold_failure(&["timed", "timeout", "deadline"], &["secret"])
        .await;
    assert!(
        process.started.elapsed() >= Duration::from_secs(9),
        "silent SSH did not exercise the total readiness deadline"
    );
}

#[tokio::test]
async fn startup_environment_optional_metadata_keeps_init_and_tools_working() {
    let (container, host, port) = ssh_fixture().await;
    // The existing environment fixture pattern intercepts probe utilities from
    // .bashrc. Authentication still succeeds; only optional nproc metadata hangs.
    control(&container, METADATA_FIXTURE).await;
    for disable_sudo in [false, true] {
        control(&container, "printf hang > /home/test/probe-mode").await;
        let mut auth = vec![
            OsString::from("--user=test"),
            OsString::from("--password=secret"),
            OsString::from("--strict-host-key-checking=no"),
        ];
        if disable_sudo {
            auth.push(OsString::from("--disable-sudo"));
        }
        let mut process = McpProcess::spawn_with_auth(&host, port, None, None, &auth).await;
        let initialized = process.initialize().await;
        let instructions = initialized["result"]["instructions"].as_str().unwrap();
        let snapshot: Value = serde_json::from_str(instructions.lines().last().unwrap()).unwrap();
        assert_eq!(snapshot.as_object().unwrap().len(), 14);
        assert_eq!(snapshot["effective_uid"], 1000);
        assert!(snapshot["available_cpu_parallelism"].is_null());
        control(&container, "printf normal > /home/test/probe-mode").await;
        process
            .send(json!({"jsonrpc":"2.0", "id":2, "method":"tools/list", "params":{}}))
            .await;
        let before = process.response(2).await["result"].clone();
        assert_eq!(
            before["tools"].as_array().unwrap().len(),
            if disable_sudo { 4 } else { 6 }
        );
        process
            .send(
                json!({"jsonrpc":"2.0", "id":3, "method":"tools/call", "params":{
                    "name":"host_environment", "arguments":{}
                }}),
            )
            .await;
        let unknown = process.response(3).await;
        assert_eq!(unknown["error"]["code"], -32602);
        assert_eq!(
            unknown["error"]["message"],
            "Unknown tool: host_environment"
        );
        process
            .send(json!({"jsonrpc":"2.0", "id":5, "method":"ping", "params":{}}))
            .await;
        assert!(process.response(5).await.get("error").is_none());
        process
            .send(json!({"jsonrpc":"2.0", "id":6, "method":"tools/list", "params":{}}))
            .await;
        assert_eq!(process.response(6).await["result"], before);
        process.close_stdin().await;
        process.assert_successful_exit().await;
    }
}

const METADATA_FIXTURE: &str = r#"
set -eu
mkdir -p /home/test/probebin
printf 'PATH=/home/test/probebin:$PATH; export PATH\n' > /home/test/.bashrc
printf normal > /home/test/probe-mode
cat > /home/test/probebin/nproc <<'SCRIPT'
#!/bin/sh
if [ "$(cat /home/test/probe-mode)" = hang ]; then /bin/sleep 20; else printf 7; fi
SCRIPT
chmod +x /home/test/probebin/nproc
chown -R test:test /home/test/probebin /home/test/probe-mode /home/test/.bashrc
"#;

#[tokio::test]
async fn warm_outage_keeps_mcp_alive_and_authenticates_again_while_idle() {
    let (container, host, port) = controlled_ssh_fixture().await;
    let known_hosts = tempfile::NamedTempFile::new().unwrap();
    let auth = [
        OsString::from("--user=test"),
        OsString::from("--password=secret"),
        OsString::from("--strict-host-key-checking=accept-new"),
        OsString::from(format!("--known-hosts={}", known_hosts.path().display())),
        OsString::from("--reconnect-retries=2"),
        OsString::from("--reconnect-backoff-ms=20"),
    ];
    let mut process = McpProcess::spawn_with_auth(&host, port, None, None, &auth).await;
    process.initialize().await;
    process
        .send(json!({"jsonrpc":"2.0", "id":2, "method":"tools/list", "params":{}}))
        .await;
    let listed = process.response(2).await;
    assert!(listed.get("error").is_none(), "{listed}");
    let startup_definitions = listed["result"].clone();
    assert_six_tool_budget(&startup_definitions);
    assert_eq!(
        transfer_description(&startup_definitions),
        RAW_TRANSFER_DESCRIPTION
    );
    let pinned_host_key = std::fs::read(known_hosts.path()).unwrap();
    assert!(
        !pinned_host_key.is_empty(),
        "cold startup did not enroll its host key"
    );
    process.send(json!({"jsonrpc":"2.0", "id":3, "method":"tools/call", "params":{
        "name":"shell", "arguments":{"command":"printf x >> /home/test/lifecycle-once; printf warm"}
    }})).await;
    assert_eq!(tool_text(&process.response(3).await), "warm");
    process.send(json!({"jsonrpc":"2.0", "id":4, "method":"tools/call", "params":{
        "name":"shell", "arguments":{"command":"printf x >> /home/test/lifecycle-inflight; sleep 120", "timeout_ms":10000}
    }})).await;
    // Witness the side effect before severing SSH, but never obtain a trustworthy
    // terminal outcome for this operation. Recovery must not replay its payload.
    timeout(Duration::from_secs(5), async {
        loop {
            if control(&container, "if [ -f /home/test/lifecycle-inflight ]; then cat /home/test/lifecycle-inflight; fi").await == "x" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }).await.expect("in-flight command did not reach its side effect");
    let before = authenticated_sessions(&container).await;
    assert!(
        before >= 1,
        "cold startup was not authenticated in sshd logs"
    );

    control(&container, STOP_SSHD).await;
    control(
        &container,
        r#"
set -eu
for file in /proc/[0-9]*/comm; do
    name=$(cat "$file" 2>/dev/null) || continue
    case "$name" in sshd*)
        status=${file%/comm}/status
        grep -q '^State:.*Z' "$status" || exit 1;;
    esac
done
"#,
    )
    .await;
    // The listener and every session were SIGKILLed, rather than merely paused.
    // Remain down across multiple 5s recovery ticks and finite retry bursts.
    let outage_started = tokio::time::Instant::now();
    let interrupted = process.response(4).await;
    assert!(interrupted.get("error").is_none(), "{interrupted}");
    assert_eq!(interrupted["result"]["isError"], true, "{interrupted}");
    assert!(
        tool_text(&interrupted).contains("exit status unavailable"),
        "{interrupted}"
    );
    let mut request_id = 5;
    while outage_started.elapsed() < Duration::from_secs(16) {
        process
            .send(json!({"jsonrpc":"2.0", "id":request_id, "method":"ping", "params":{}}))
            .await;
        assert!(process.response(request_id).await.get("error").is_none());
        request_id += 1;
        process
            .send(json!({"jsonrpc":"2.0", "id":request_id, "method":"tools/list", "params":{}}))
            .await;
        let listed = process.response(request_id).await;
        assert!(listed.get("error").is_none(), "{listed}");
        assert_eq!(listed["result"], startup_definitions);
        request_id += 1;
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert_eq!(authenticated_sessions(&container).await, before);
    control(&container, START_SSHD).await;
    wait_for_tcp(&host, port).await;

    // Crucially no tools/call appears between restoration and this witness:
    // a new Accepted-password log line proves idle transport authentication.
    let after = timeout(Duration::from_secs(20), async {
        loop {
            let count = authenticated_sessions(&container).await;
            if count > before {
                break count;
            }
            process
                .send(json!({"jsonrpc":"2.0", "id":request_id, "method":"ping", "params":{}}))
                .await;
            assert!(process.response(request_id).await.get("error").is_none());
            request_id += 1;
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("no new authenticated SSH session appeared before an SSH tool call");
    assert!(after > before);
    assert_eq!(
        std::fs::read(known_hosts.path()).unwrap(),
        pinned_host_key,
        "recovery changed the known host identity"
    );
    eprintln!(
        "idle recovery witness: authenticated sshd sessions {before} -> {after}; no SSH tool call after restoration"
    );
    assert_eq!(
        control(&container, "cat /home/test/lifecycle-once").await,
        "x",
        "recovery replayed a completed operation"
    );
    assert_eq!(
        control(&container, "cat /home/test/lifecycle-inflight").await,
        "x",
        "recovery replayed an operation with an unknown terminal outcome"
    );
    process
        .send(json!({"jsonrpc":"2.0", "id":request_id, "method":"tools/list", "params":{}}))
        .await;
    let listed = process.response(request_id).await;
    assert!(listed.get("error").is_none(), "{listed}");
    assert_eq!(listed["result"], startup_definitions);
    request_id += 1;
    process
        .send(
            json!({"jsonrpc":"2.0", "id":request_id, "method":"tools/call", "params":{
                "name":"shell", "arguments":{"command":"printf recovered"}
            }}),
        )
        .await;
    let recovered = process.response(request_id).await;
    assert_eq!(tool_text(&recovered), "recovered", "{recovered}");
    process.close_stdin().await;
    process.assert_successful_exit().await;

    let shutdown_sessions = authenticated_sessions(&container).await;
    control(&container, STOP_SSHD).await;
    control(&container, START_SSHD).await;
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert_eq!(
        authenticated_sessions(&container).await,
        shutdown_sessions,
        "recovery continued after process shutdown"
    );
}

#[tokio::test]
async fn cold_strict_host_key_rejection_precedes_mcp_serving() {
    let (_container, host, port) = ssh_fixture().await;
    let known_hosts = tempfile::NamedTempFile::new().unwrap();
    let auth = [
        OsString::from("--user=test"),
        OsString::from("--password=secret"),
        OsString::from("--strict-host-key-checking=yes"),
        OsString::from(format!("--known-hosts={}", known_hosts.path().display())),
    ];
    let mut process = McpProcess::spawn_with_auth(&host, port, None, None, &auth).await;
    process.send(opening_request(true)).await;
    process
        .assert_cold_failure(
            &["host key", "hostkey", "known_hosts", "unknownkey"],
            &["secret"],
        )
        .await;
    assert_eq!(
        std::fs::read(known_hosts.path()).unwrap(),
        b"",
        "strict verification enrolled an untrusted key"
    );
}

#[tokio::test]
async fn cold_jump_authentication_is_required_and_uses_configured_identity() {
    let (_container, host, port) = ssh_fixture().await;
    let wrong_password = "lifecycle-wrong-jump-password-private";
    for password in [wrong_password, "jump-secret"] {
        let auth = [
            OsString::from("--user=test"),
            OsString::from("--password=secret"),
            OsString::from("--strict-host-key-checking=no"),
            OsString::from(format!("--jump=jump@{host}:{port}")),
            OsString::from(format!("--jump-password={password}")),
        ];
        let mut process = McpProcess::spawn_with_auth("127.0.0.1", 2222, None, None, &auth).await;
        if password == wrong_password {
            process.send(opening_request(false)).await;
            process
                .assert_cold_failure(
                    &["auth", "credentials", "rejected"],
                    &[wrong_password, "secret"],
                )
                .await;
        } else {
            process.initialize().await;
            process
                .send(
                    json!({"jsonrpc":"2.0", "id":2, "method":"tools/call", "params":{
                        "name":"shell", "arguments":{"command":"id -un"}
                    }}),
                )
                .await;
            assert_eq!(tool_text(&process.response(2).await).trim(), "test");
            process.close_stdin().await;
            process.assert_successful_exit().await;
        }
    }
}

#[tokio::test]
async fn startup_environment_stdio_init_and_discovery_are_frozen_without_a_tool() {
    init_test_env().unwrap();
    let container = GenericImage::new("ssh-mcp-debian-sshd", "latest")
        .with_exposed_port(2222u16.into())
        .start()
        .await
        .unwrap();
    let host = container.get_host().await.unwrap().to_string();
    let port = container.get_host_port_ipv4(2222).await.unwrap();
    wait_for_tcp(&host, port).await;
    let meta = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": {"name":"snapshot-test", "version":"1"},
        "io.modelcontextprotocol/clientCapabilities": {}
    });
    control(&container, METADATA_FIXTURE).await;
    for (modern, failed_metadata) in [(false, false), (true, false), (true, true)] {
        if failed_metadata {
            control(&container, "printf hang > /home/test/probe-mode").await;
        }
        let mut process = McpProcess::spawn(&host, port).await;
        let initial = if modern {
            process.send(json!({"jsonrpc":"2.0", "id":1, "method":"server/discover", "params":{"_meta":meta}})).await;
            process.response(1).await
        } else {
            process.initialize().await
        };
        let instructions = initial["result"]["instructions"]
            .as_str()
            .unwrap()
            .to_owned();
        let snapshot: Value = serde_json::from_str(instructions.lines().last().unwrap()).unwrap();
        assert_eq!(snapshot.as_object().unwrap().len(), 14);
        if failed_metadata {
            assert_eq!(snapshot["effective_uid"], 1000);
            assert!(snapshot["available_cpu_parallelism"].is_null());
            control(&container, "printf normal > /home/test/probe-mode").await;
        } else {
            assert_eq!(snapshot["effective_uid"], 1000);
            assert_eq!(snapshot["running_as_root"], false);
            assert_eq!(snapshot["os"], "Linux");
            assert_eq!(snapshot["virtualization"]["container"], "docker");
            if snapshot["machine_architecture"] == "x86_64" {
                assert!(snapshot["cpu_models"].is_array());
            }
        }
        let metadata = if modern {
            json!({"_meta": meta})
        } else {
            json!({})
        };
        process
            .send(json!({"jsonrpc":"2.0", "id":2, "method":"tools/list", "params":metadata}))
            .await;
        let listed = process.response(2).await;
        assert!(listed.get("error").is_none(), "{listed}");
        let definitions = listed["result"].clone();
        assert_six_tool_budget(&definitions);
        assert_eq!(transfer_description(&definitions), RAW_TRANSFER_DESCRIPTION);
        for id in 3..5 {
            let mut params = metadata.clone();
            params["name"] = json!("shell");
            params["arguments"] = json!({"command":"printf wire"});
            process
                .send(json!({"jsonrpc":"2.0", "id":id, "method":"tools/call", "params":params}))
                .await;
            let response = process.response(id).await;
            assert!(response.get("error").is_none(), "{response}");
            assert_eq!(tool_text(&response), "wire");
        }
        if modern {
            process
                .send(
                    json!({"jsonrpc":"2.0", "id":5, "method":"server/discover", "params":metadata}),
                )
                .await;
            assert_eq!(
                process.response(5).await["result"]["instructions"],
                instructions
            );
        }
        process
            .send(json!({"jsonrpc":"2.0", "id":6, "method":"tools/list", "params":metadata}))
            .await;
        assert_eq!(process.response(6).await["result"], definitions);
        process.close_stdin().await;
        process.assert_successful_exit().await;
    }
}

#[tokio::test]
async fn startup_environment_signals_cancel_stalled_ssh_before_mcp_serving() {
    for signal in [Signal::SIGTERM, Signal::SIGINT] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut process =
            McpProcess::spawn("127.0.0.1", listener.local_addr().unwrap().port()).await;
        // Cancel while startup is awaiting the SSH greeting.
        let (_socket, _) = timeout(Duration::from_secs(2), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let started = tokio::time::Instant::now();
        process.signal(signal);
        process.assert_successful_exit().await;
        assert!(
            started.elapsed() < Duration::from_millis(2500),
            "signal did not cancel bootstrap"
        );
        let mut stdout = String::new();
        process.stdout.read_to_string(&mut stdout).await.unwrap();
        assert!(stdout.is_empty(), "cancelled startup served MCP: {stdout}");
    }
}

#[tokio::test]
async fn cli_key_paths_authenticate_with_absolute_relative_and_tilde_forms() {
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

    let home = tempfile::tempdir().expect("create isolated home");
    let key_path = home.path().join(".ssh/id_ed25519");
    std::fs::create_dir_all(key_path.parent().expect("key parent")).expect("create .ssh");
    std::fs::write(&key_path, TEST_PRIVATE_KEY).expect("write private key");
    std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))
        .expect("chmod private key");
    let other_cwd = home.path().join("work");
    std::fs::create_dir(&other_cwd).expect("create alternate cwd");

    let cases = [
        ("absolute", key_path.as_os_str(), other_cwd.as_path()),
        ("relative", OsStr::new(".ssh/id_ed25519"), home.path()),
        (
            "home-relative",
            OsStr::new("~/.ssh/id_ed25519"),
            other_cwd.as_path(),
        ),
    ];
    for (case, key, current_dir) in cases {
        let mut key_arg = OsString::from("--key=");
        key_arg.push(key);
        let auth_args = [
            OsString::from("--user=test"),
            key_arg,
            OsString::from("--strict-host-key-checking=no"),
        ];
        let mut process = McpProcess::spawn_with_auth(
            &host.to_string(),
            port,
            Some(current_dir),
            Some(home.path()),
            &auth_args,
        )
        .await;
        process.initialize().await;
        process
            .send(json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "shell",
                    "arguments": {"command": "id -un"}
                }
            }))
            .await;
        let response = process.response(2).await;
        assert!(
            response.get("error").is_none(),
            "{case} key path failed: {response}"
        );
        assert_ne!(
            response["result"]["isError"].as_bool(),
            Some(true),
            "{case} key path returned a tool error: {response}"
        );
        assert_eq!(tool_text(&response).trim(), "test", "{case} key path");

        process.close_stdin().await;
        process.assert_successful_exit().await;
    }
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

#[tokio::test]
async fn background_transfer_is_immediately_pollable_and_completes() {
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

    let local_root = tempfile::tempdir().expect("local transfer root");
    std::fs::write(
        local_root.path().join("payload.txt"),
        b"background transfer\n",
    )
    .expect("write local payload");
    let auth_args = [
        OsString::from("--user=test"),
        OsString::from("--password=secret"),
        OsString::from("--strict-host-key-checking=no"),
    ];
    let mut process = McpProcess::spawn_with_auth(
        &host.to_string(),
        port,
        Some(local_root.path()),
        None,
        &auth_args,
    )
    .await;
    process.initialize().await;

    let remote_path = format!("/home/test/background-transfer-{}.txt", std::process::id());
    process
        .send(json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "transfer",
                "arguments": {
                    "operation": "put",
                    "local_path": "payload.txt",
                    "remote_path": remote_path.clone(),
                    "transport": "exec-raw",
                    "kind": "file",
                    "background": true,
                    "timeout_ms": 30000
                }
            }
        }))
        .await;
    let started = process.response(2).await;
    assert!(started.get("error").is_none(), "transfer failed: {started}");
    let started: Value =
        serde_json::from_str(tool_text(&started)).expect("background transfer response JSON");
    assert_eq!(started["job_type"], "transfer");
    assert_eq!(started["state"], "running");
    let job_id = started["job_id"]
        .as_str()
        .expect("background transfer job id")
        .to_string();

    let mut terminal = None;
    for request_id in 3..103 {
        process
            .send(json!({
                "jsonrpc": "2.0",
                "id": request_id,
                "method": "tools/call",
                "params": {
                    "name": "check_process",
                    "arguments": {"job_id": job_id.clone(), "wait_for": 0, "tail_lines": 0}
                }
            }))
            .await;
        let response = process.response(request_id).await;
        assert!(
            response.get("error").is_none(),
            "check_process failed: {response}"
        );
        let status: Value =
            serde_json::from_str(tool_text(&response)).expect("transfer status JSON");
        assert_eq!(status["job_type"], "transfer");
        if status["running"] == false {
            terminal = Some(status);
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let terminal = terminal.expect("background transfer should finish");
    assert_eq!(terminal["state"], "completed", "{terminal}");
    assert_eq!(terminal["result"]["ok"], true, "{terminal}");

    process
        .send(json!({
            "jsonrpc": "2.0",
            "id": 103,
            "method": "tools/call",
            "params": {
                "name": "transfer",
                "arguments": {
                    "operation": "put",
                    "local_path": "payload.txt",
                    "remote_path": remote_path.clone(),
                    "transport": "exec-raw",
                    "kind": "file",
                    "overwrite": false,
                    "timeout_ms": 30000
                }
            }
        }))
        .await;
    let rejected = process.response(103).await;
    assert!(
        rejected.get("error").is_none(),
        "transfer RPC failed: {rejected}"
    );
    assert_eq!(rejected["result"]["isError"], true, "{rejected}");
    let rejected_body: Value =
        serde_json::from_str(tool_text(&rejected)).expect("failed transfer response JSON");
    assert_eq!(rejected_body["ok"], false, "{rejected_body}");

    process
        .send(json!({
            "jsonrpc": "2.0",
            "id": 104,
            "method": "tools/call",
            "params": {
                "name": "shell",
                "arguments": {"command": format!("cat -- '{}' && rm -f -- '{}'", remote_path, remote_path)}
            }
        }))
        .await;
    let remote = process.response(104).await;
    assert_eq!(tool_text(&remote), "background transfer\n");

    process.close_stdin().await;
    process.assert_successful_exit().await;
}
