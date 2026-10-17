//! Command-only RouterOS contract over real stdio MCP and a controlled SSH peer.
//! The peer verifies client behavior, not firmware compatibility.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use russh::{Channel, ChannelId, Disconnect, Pty, server};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Notify;
use tokio::time::timeout;

#[derive(Default)]
struct Transcript {
    exec: Mutex<Vec<Vec<u8>>>,
    unsupported: Mutex<Vec<String>>,
    connections: AtomicUsize,
    opens: AtomicUsize,
    long_started: Notify,
}

struct Peer(Arc<Transcript>);

impl server::Handler for Peer {
    type Error = russh::Error;

    async fn auth_password(&mut self, _: &str, _: &str) -> Result<server::Auth, Self::Error> {
        Ok(server::Auth::Accept)
    }

    async fn auth_publickey(
        &mut self,
        _: &str,
        _: &russh::keys::PublicKey,
    ) -> Result<server::Auth, Self::Error> {
        Ok(server::Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        _: Channel<server::Msg>,
        _: &mut server::Session,
    ) -> Result<bool, Self::Error> {
        self.0.opens.fetch_add(1, Ordering::SeqCst);
        Ok(true)
    }

    #[allow(clippy::too_many_arguments)]
    async fn pty_request(
        &mut self,
        id: ChannelId,
        _: &str,
        _: u32,
        _: u32,
        _: u32,
        _: u32,
        _: &[(Pty, u32)],
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.0.unsupported.lock().unwrap().push("pty".to_string());
        session.channel_failure(id)
    }

    async fn shell_request(
        &mut self,
        id: ChannelId,
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.0.unsupported.lock().unwrap().push("shell".to_string());
        session.channel_failure(id)
    }

    async fn subsystem_request(
        &mut self,
        id: ChannelId,
        name: &str,
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.0
            .unsupported
            .lock()
            .unwrap()
            .push(format!("subsystem:{name}"));
        session.channel_failure(id)
    }

    async fn exec_request(
        &mut self,
        id: ChannelId,
        command: &[u8],
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.0.exec.lock().unwrap().push(command.to_vec());
        match command {
            b":put \"disconnect\"" => {
                session.disconnect(Disconnect::ByApplication, "test reconnect", "")?;
            }
            b":put \"reject\"" => {
                session.channel_failure(id)?;
            }
            b":delay 60s" => {
                session.channel_success(id)?;
                session.data(id, b"partial before timeout\n".to_vec())?;
                self.0.long_started.notify_one();
            }
            b":put \"status-first\"" => {
                // Completion must not require Success or EOF, nor stop at status.
                session.exit_status_request(id, 0)?;
                session.data(id, b"after status".to_vec())?;
                session.close(id)?;
            }
            b":put \"missing-status\"" => {
                session.channel_success(id)?;
                session.data(id, b"partial without status".to_vec())?;
                session.close(id)?;
            }
            b":error \"expected\"" => {
                session.channel_success(id)?;
                session.data(id, b"expected error on stdout".to_vec())?;
                session.exit_status_request(id, 1)?;
                session.close(id)?;
            }
            b":put \"large\"" => {
                session.channel_success(id)?;
                session.data(id, vec![b'x'; 512])?;
                session.extended_data(id, 1, vec![b'e'; 512])?;
                session.exit_status_request(id, 0)?;
                session.close(id)?;
            }
            _ => {
                session.channel_success(id)?;
                session.data(id, command.to_vec())?;
                session.exit_status_request(id, 0)?;
                session.close(id)?;
            }
        }
        Ok(())
    }
}

struct Fixture {
    port: u16,
    transcript: Arc<Transcript>,
    listener: tokio::task::JoinHandle<()>,
}

fn test_key() -> russh::keys::PrivateKey {
    russh::keys::PrivateKey::new(
        russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&[67; 32]).into(),
        "routeros-test",
    )
    .unwrap()
}

impl Fixture {
    async fn new() -> Self {
        let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = socket.local_addr().unwrap().port();
        let transcript = Arc::new(Transcript::default());
        let state = transcript.clone();
        let config = Arc::new(server::Config {
            server_id: russh::SshId::Standard("SSH-2.0-ROSSSH".into()),
            keys: vec![test_key()],
            ..Default::default()
        });
        let listener = tokio::spawn(async move {
            loop {
                let (stream, _) = socket.accept().await.unwrap();
                state.connections.fetch_add(1, Ordering::SeqCst);
                let config = config.clone();
                let peer = Peer(state.clone());
                tokio::spawn(async move {
                    if let Ok(session) = server::run_stream(config, stream, peer).await {
                        let _ = session.await;
                    }
                });
            }
        });
        Self {
            port,
            transcript,
            listener,
        }
    }

    fn commands(&self) -> Vec<Vec<u8>> {
        self.transcript.exec.lock().unwrap().clone()
    }

    fn assert_no_probes(&self) {
        assert!(
            self.commands().is_empty(),
            "unexpected startup EXEC: {:?}",
            self.commands()
        );
        assert!(self.transcript.unsupported.lock().unwrap().is_empty());
        assert_eq!(self.transcript.opens.load(Ordering::SeqCst), 0);
        assert_eq!(self.transcript.connections.load(Ordering::SeqCst), 1);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.listener.abort();
    }
}

struct Mcp {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    stderr: tokio::task::JoinHandle<String>,
    pending: HashMap<u64, Value>,
    directory: TempDir,
    spool: PathBuf,
    next_id: u64,
    modern: bool,
}

fn meta() -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion":"2026-07-28",
        "io.modelcontextprotocol/clientInfo":{"name":"routeros-test","version":"1"},
        "io.modelcontextprotocol/clientCapabilities":{}
    })
}

impl Mcp {
    async fn spawn(
        port: u16,
        modern: bool,
        arguments: &[&str],
        platform_env: Option<&str>,
    ) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let spool = directory.path().join("spool");
        let key_path = directory.path().join("key");
        std::fs::write(
            &key_path,
            test_key()
                .to_openssh(russh::keys::ssh_key::LineEnding::LF)
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let mut command = Command::new(env!("CARGO_BIN_EXE_ssh-mcp"));
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("SSH_MCP_") {
                command.env_remove(name);
            }
        }
        if let Some(profile) = platform_env {
            command.env("SSH_MCP_REMOTE_PLATFORM", profile);
        }
        command
            .args([
                "--host=127.0.0.1",
                "--user=test",
                "--strict-host-key-checking=no",
                "--reconnect-retries=0",
                "--max-output-tokens=16",
            ])
            .arg(format!("--port={port}"))
            .arg("--key")
            .arg(key_path)
            .arg("--spool-dir")
            .arg(&spool)
            .args(arguments)
            .current_dir(directory.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().unwrap();
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut stderr = child.stderr.take().unwrap();
        let stderr = tokio::spawn(async move {
            let mut bytes = Vec::new();
            stderr.read_to_end(&mut bytes).await.unwrap();
            String::from_utf8_lossy(&bytes).into_owned()
        });
        Self {
            child,
            stdin,
            stdout,
            stderr,
            pending: HashMap::new(),
            directory,
            spool,
            next_id: 1,
            modern,
        }
    }

    async fn send(&mut self, value: Value) {
        let input = self.stdin.as_mut().unwrap();
        input
            .write_all(format!("{value}\n").as_bytes())
            .await
            .unwrap();
        input.flush().await.unwrap();
    }

    async fn start(&mut self, method: &str, mut params: Value) -> u64 {
        if self.modern {
            params["_meta"] = meta();
        }
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await;
        id
    }

    async fn response(&mut self, id: u64) -> Value {
        if let Some(response) = self.pending.remove(&id) {
            return response;
        }
        timeout(Duration::from_secs(15), async {
            loop {
                let mut line = String::new();
                assert_ne!(
                    self.stdout.read_line(&mut line).await.unwrap(),
                    0,
                    "MCP exited before response {id}"
                );
                let response: Value = serde_json::from_str(&line).unwrap();
                if response["id"] == id {
                    return response;
                }
                if let Some(other) = response["id"].as_u64() {
                    self.pending.insert(other, response);
                }
            }
        })
        .await
        .expect("MCP response deadline")
    }

    async fn rpc(&mut self, method: &str, params: Value) -> Value {
        let id = self.start(method, params).await;
        self.response(id).await
    }

    async fn initialize(&mut self) -> Value {
        assert!(!self.modern);
        let result = self.rpc("initialize", json!({"protocolVersion":"2024-11-05", "capabilities":{}, "clientInfo":{"name":"routeros-test","version":"1"}})).await;
        assert!(result.get("error").is_none(), "{result}");
        self.send(json!({"jsonrpc":"2.0", "method":"notifications/initialized", "params":{}}))
            .await;
        result
    }

    async fn call(&mut self, name: &str, arguments: Value) -> Value {
        self.rpc("tools/call", json!({"name":name,"arguments":arguments}))
            .await
    }

    fn assert_empty_spool(&self) {
        assert_eq!(
            std::fs::read_dir(&self.spool).unwrap().count(),
            0,
            "RouterOS must not create job/log/state files"
        );
    }

    async fn finish(mut self) {
        self.stdin.take();
        let status = timeout(Duration::from_secs(5), self.child.wait())
            .await
            .unwrap()
            .unwrap();
        let stderr = self.stderr.await.unwrap();
        assert!(status.success(), "MCP exit {status}: {stderr}");
    }
}

fn text(response: &Value) -> String {
    response["result"]["content"]
        .as_array()
        .expect("tool content")
        .iter()
        .filter_map(|content| content["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn discovery_gating_and_raw_execution_match_for_legacy_and_modern_clients() {
    for modern in [false, true] {
        let fixture = Fixture::new().await;
        // Exercise key auth, CLI-over-env selection, and env-only selection.
        let mut mcp = if modern {
            Mcp::spawn(fixture.port, true, &[], Some("routeros")).await
        } else {
            Mcp::spawn(
                fixture.port,
                false,
                &["--remote-platform=routeros"],
                Some("posix"),
            )
            .await
        };
        let initial = if modern {
            mcp.rpc("tools/list", json!({})).await
        } else {
            let initialized = mcp.initialize().await;
            assert!(
                initialized["result"]["instructions"]
                    .as_str()
                    .unwrap()
                    .contains("RouterOS")
            );
            mcp.rpc("tools/list", json!({})).await
        };
        assert!(initial.get("error").is_none(), "{initial}");
        let tools = initial["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "shell");
        let schema = &tools[0]["inputSchema"];
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["properties"].as_object().unwrap().len(), 2);
        assert_eq!(schema["properties"]["timeout_ms"]["minimum"], 1);
        fixture.assert_no_probes();
        mcp.assert_empty_spool();

        for (name, args) in [
            (
                "sudo_shell",
                json!({"command": ":put 1", "background":true}),
            ),
            (
                "sudo_apply_patch",
                json!({"patch":"*** Begin Patch\n*** Add File: /example\n+text\n*** End Patch"}),
            ),
            (
                "apply_patch",
                json!({"patch":"*** Begin Patch\n*** Add File: /example\n+text\n*** End Patch"}),
            ),
            ("check_process", json!({"job_id":"missing"})),
            (
                "transfer",
                json!({"operation":"put", "local_path":mcp.directory.path().join("key"), "remote_path":"/example", "kind":"file", "transport":"exec-raw", "background":true}),
            ),
        ] {
            let response = mcp.call(name, args).await;
            assert_eq!(response["error"]["code"], -32602, "{response}");
            assert!(
                response["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("RouterOS")
            );
        }
        for args in [
            json!({"command": ":put 1", "background":false}),
            json!({"command": ":put 1", "log_path":null}),
            json!({"command": ":put 1", "timeout_ms":null}),
            json!({"command": ":put 1", "timeout_ms":0}),
            json!({"command": ":put 1\n:put 2"}),
            json!({"command": "bad\0command"}),
        ] {
            let response = mcp.call("shell", args).await;
            assert_eq!(response["error"]["code"], -32602, "{response}");
        }
        let unknown = mcp.call("read", json!({})).await;
        assert!(
            unknown["error"]["message"]
                .as_str()
                .unwrap()
                .contains("Unknown tool: read")
        );
        fixture.assert_no_probes();
        mcp.assert_empty_spool();

        let raw = r#":put "literal $ [ ] \\"; :put [/system resource get version]"#;
        let response = mcp.call("shell", json!({"command":raw})).await;
        assert_ne!(response["result"]["isError"], true, "{response}");
        // Deliberately above the 64-byte configured output cap: raw transport and
        // bounded display are separate invariants.
        assert_eq!(fixture.commands(), vec![raw.as_bytes().to_vec()]);
        assert!(text(&response).starts_with(&raw[..raw.len().min(64)]));

        let zero = mcp
            .call("shell", json!({"command":":put \"failure error\""}))
            .await;
        assert_ne!(zero["result"]["isError"], true, "{zero}");
        let failed = mcp
            .call("shell", json!({"command":":error \"expected\""}))
            .await;
        assert_eq!(failed["result"]["isError"], true);
        assert!(text(&failed).contains("expected error on stdout"));
        assert!(text(&failed).contains("exit code 1"));
        let ordered = mcp
            .call("shell", json!({"command":":put \"status-first\""}))
            .await;
        assert_ne!(ordered["result"]["isError"], true, "{ordered}");
        assert_eq!(text(&ordered), "after status");
        let missing = mcp
            .call("shell", json!({"command":":put \"missing-status\""}))
            .await;
        assert_eq!(missing["result"]["isError"], true);
        assert!(text(&missing).contains("partial without status"));
        assert!(text(&missing).contains("may have run"));
        let rejected = mcp
            .call(
                "shell",
                json!({"command":":put \"reject\"", "timeout_ms":1000}),
            )
            .await;
        assert_eq!(rejected["result"]["isError"], true);
        assert!(text(&rejected).contains("not started"));
        let large = mcp.call("shell", json!({"command":":put \"large\""})).await;
        assert_ne!(large["result"]["isError"], true);
        assert!(text(&large).contains("stdout truncated"));
        assert!(text(&large).contains("stderr truncated"));
        assert!(text(&large).len() < 500);
        assert!(fixture.transcript.unsupported.lock().unwrap().is_empty());
        mcp.assert_empty_spool();

        let relisted = mcp.rpc("tools/list", json!({})).await;
        assert_eq!(relisted["result"], initial["result"]);
        mcp.finish().await;
    }
}

#[tokio::test]
async fn timeout_and_reconnect_preserve_native_contract_without_jobs_or_replay() {
    let fixture = Fixture::new().await;
    let mut mcp = Mcp::spawn(fixture.port, true, &["--remote-platform=routeros"], None).await;
    let before = mcp.rpc("server/discover", json!({})).await;
    let schema = mcp.rpc("tools/list", json!({})).await;
    fixture.assert_no_probes();

    let long = mcp
        .start(
            "tools/call",
            json!({"name":"shell", "arguments":{"command":":delay 60s", "timeout_ms":500}}),
        )
        .await;
    timeout(
        Duration::from_secs(3),
        fixture.transcript.long_started.notified(),
    )
    .await
    .unwrap();
    let sibling = mcp
        .call("shell", json!({"command":":put \"sibling\""}))
        .await;
    assert_ne!(sibling["result"]["isError"], true, "{sibling}");
    let timed = mcp.response(long).await;
    assert_eq!(timed["result"]["isError"], true);
    assert!(text(&timed).contains("partial before timeout"));
    assert!(text(&timed).contains("may have run"));
    assert!(!text(&timed).contains("job_id"));
    let next = mcp
        .call("shell", json!({"command":":put \"after timeout\""}))
        .await;
    assert_ne!(next["result"]["isError"], true, "{next}");
    assert_eq!(
        fixture.transcript.connections.load(Ordering::SeqCst),
        1,
        "ordinary timeout must not reset sibling route"
    );

    let lost = mcp
        .call("shell", json!({"command":":put \"disconnect\""}))
        .await;
    assert_eq!(lost["result"]["isError"], true, "{lost}");
    let recovered = mcp
        .call("shell", json!({"command":":put \"after reconnect\""}))
        .await;
    assert_ne!(recovered["result"]["isError"], true, "{recovered}");
    assert_eq!(fixture.transcript.connections.load(Ordering::SeqCst), 2);
    assert_eq!(
        mcp.rpc("server/discover", json!({})).await["result"],
        before["result"]
    );
    assert_eq!(
        mcp.rpc("tools/list", json!({})).await["result"],
        schema["result"]
    );
    let expected = [
        ":delay 60s",
        ":put \"sibling\"",
        ":put \"after timeout\"",
        ":put \"disconnect\"",
        ":put \"after reconnect\"",
    ];
    assert_eq!(
        fixture.commands(),
        expected
            .iter()
            .map(|command| command.as_bytes().to_vec())
            .collect::<Vec<_>>()
    );
    assert!(fixture.transcript.unsupported.lock().unwrap().is_empty());
    mcp.assert_empty_spool();
    mcp.finish().await;
}

#[tokio::test]
async fn absent_profile_env_defaults_to_posix_and_cli_overrides_routeros_env() {
    for profile_env in [None, Some("routeros")] {
        let fixture = Fixture::new().await;
        let args: &[&str] = if profile_env.is_some() {
            &["--remote-platform=posix"]
        } else {
            &[]
        };
        let mut mcp = Mcp::spawn(fixture.port, true, args, profile_env).await;
        let result = mcp.rpc("tools/list", json!({})).await;
        assert!(result.get("error").is_none(), "{result}");
        let names = result["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "shell",
                "sudo_shell",
                "sudo_apply_patch",
                "check_process",
                "transfer",
                "apply_patch"
            ]
        );
        mcp.finish().await;
    }
}
