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
    identification: Arc<Mutex<String>>,
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
        Self::with_identification("SSH-2.0-ROSSSH").await
    }

    async fn with_identification(identification: &str) -> Self {
        let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = socket.local_addr().unwrap().port();
        let transcript = Arc::new(Transcript::default());
        let state = transcript.clone();
        let identification = Arc::new(Mutex::new(identification.to_string()));
        let server_id = identification.clone();
        let listener = tokio::spawn(async move {
            loop {
                let (stream, _) = socket.accept().await.unwrap();
                state.connections.fetch_add(1, Ordering::SeqCst);
                let config = Arc::new(server::Config {
                    server_id: russh::SshId::Standard(server_id.lock().unwrap().clone().into()),
                    keys: vec![test_key()],
                    ..Default::default()
                });
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
            identification,
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
        // Key-authenticated native startup needs no profile flag or environment.
        let mut mcp = Mcp::spawn(fixture.port, modern, &[], None).await;
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
    let mut mcp = Mcp::spawn(fixture.port, true, &[], None).await;
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
async fn non_routeros_identification_retains_posix_tools_without_profile_configuration() {
    for (identification, args, profile_env) in [
        ("SSH-2.0-OpenSSH_9.0", vec![], None),
        (
            "SSH-2.0-test ROSSSH in a comment",
            vec!["--disable-sudo"],
            Some("routeros"),
        ),
    ] {
        let fixture = Fixture::with_identification(identification).await;
        let mut mcp = Mcp::spawn(fixture.port, true, &args, profile_env).await;
        let result = mcp.rpc("tools/list", json!({})).await;
        assert!(result.get("error").is_none(), "{result}");
        let names = result["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        let expected = if args.is_empty() {
            vec![
                "shell",
                "sudo_shell",
                "sudo_apply_patch",
                "check_process",
                "transfer",
                "apply_patch",
            ]
        } else {
            vec!["shell", "check_process", "transfer", "apply_patch"]
        };
        assert_eq!(names, expected);
        // POSIX retains its separate optional OpenSSH SFTP startup preflight.
        mcp.finish().await;
    }
}

#[tokio::test]
async fn old_profile_environment_cannot_override_detected_routeros() {
    let fixture = Fixture::new().await;
    let mut mcp = Mcp::spawn(fixture.port, true, &[], Some("posix")).await;
    let result = mcp.rpc("tools/list", json!({})).await;
    let tools = result["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "shell");
    assert!(
        tools[0]["inputSchema"]["properties"]
            .get("background")
            .is_none()
    );
    fixture.assert_no_probes();
    mcp.finish().await;
}

#[tokio::test]
async fn changed_backend_is_rejected_without_changing_schema_or_sending_exec() {
    let fixture = Fixture::new().await;
    let mut mcp = Mcp::spawn(fixture.port, true, &[], None).await;
    let before = mcp.rpc("server/discover", json!({})).await;
    let schema = mcp.rpc("tools/list", json!({})).await;
    *fixture.identification.lock().unwrap() = "SSH-2.0-OpenSSH_9.0".to_string();
    let disconnected = mcp
        .call("shell", json!({"command":":put \"disconnect\""}))
        .await;
    assert_eq!(disconnected["result"]["isError"], true);
    let refused = mcp
        .call(
            "shell",
            json!({"command":":put \"must not run\"", "timeout_ms":2000}),
        )
        .await;
    assert_eq!(refused["result"]["isError"], true, "{refused}");
    assert!(text(&refused).contains("not started"));
    assert_eq!(fixture.commands(), vec![b":put \"disconnect\"".to_vec()]);
    assert_eq!(
        mcp.rpc("server/discover", json!({})).await["result"],
        before["result"]
    );
    assert_eq!(
        mcp.rpc("tools/list", json!({})).await["result"],
        schema["result"]
    );
    mcp.assert_empty_spool();
    mcp.finish().await;
}

#[tokio::test]
async fn detected_routeros_rejects_nonempty_elevation_before_mcp_serving() {
    for args in [
        vec!["--su-password= "],
        vec!["--sudo-password=secret", "--disable-sudo"],
    ] {
        let fixture = Fixture::new().await;
        let mut mcp = Mcp::spawn(fixture.port, false, &args, None).await;
        let mut line = String::new();
        assert_eq!(
            timeout(Duration::from_secs(15), mcp.stdout.read_line(&mut line))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        let status = mcp.child.wait().await.unwrap();
        let stderr = mcp.stderr.await.unwrap();
        assert!(!status.success(), "MCP must reject RouterOS elevation");
        assert!(stderr.contains("RouterOS"), "{stderr}");
        fixture.assert_no_probes();
    }
}

#[tokio::test]
async fn lazy_library_server_resolves_platform_before_each_supported_opening_request() {
    use clap::Parser;
    use rmcp::{ServerHandler, ServiceExt};
    use ssh_mcp::{
        config::{Args, Config},
        server::SshMcpServer,
    };
    use tokio_util::sync::CancellationToken;

    for method in ["initialize", "server/discover", "tools/list", "tools/call"] {
        let fixture = Fixture::new().await;
        let directory = tempfile::tempdir().unwrap();
        let config = Config::from_args(
            Args::try_parse_from([
                "ssh-mcp".to_string(),
                "--host=127.0.0.1".to_string(),
                format!("--port={}", fixture.port),
                "--user=test".to_string(),
                "--password=test".to_string(),
                "--strict-host-key-checking=no".to_string(),
            ])
            .unwrap(),
        )
        .unwrap();
        let server = SshMcpServer::new_with_spool_dir(config, Some(directory.path().join("spool")))
            .await
            .unwrap();
        assert!(!server.connection().is_connected().await);
        assert!(
            server
                .get_info()
                .instructions
                .unwrap()
                .contains("not determined yet")
        );
        let (client, transport) = tokio::io::duplex(65536);
        let cancellation = CancellationToken::new();
        let served = server.clone();
        let token = cancellation.clone();
        let service = tokio::spawn(async move {
            let running = served
                .serve_with_ct(transport, token)
                .await
                .map_err(|error| error.to_string())?;
            running.waiting().await.map_err(|error| error.to_string())
        });
        let (reader, mut writer) = tokio::io::split(client);
        let mut reader = BufReader::new(reader);
        let params = match method {
            "initialize" => {
                json!({"protocolVersion":"2024-11-05", "capabilities":{}, "clientInfo":{"name":"lazy-test","version":"1"}})
            }
            "tools/call" => {
                json!({"_meta":meta(), "name":"shell", "arguments":{"command":":put \"lazy\""}})
            }
            _ => json!({"_meta":meta()}),
        };
        writer
            .write_all(
                format!(
                    "{}\n",
                    json!({"jsonrpc":"2.0", "id":1, "method":method, "params":params})
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut line = String::new();
        timeout(Duration::from_secs(10), reader.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        let response: Value = serde_json::from_str(&line).unwrap();
        assert!(response.get("error").is_none(), "{method}: {response}");
        assert!(server.get_info().instructions.unwrap().contains("RouterOS"));
        if method == "initialize" {
            writer.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\",\"params\":{}}\n").await.unwrap();
        } else if method == "tools/list" {
            assert_eq!(response["result"]["tools"].as_array().unwrap().len(), 1);
        } else if method == "tools/call" {
            assert_ne!(response["result"]["isError"], true);
            assert_eq!(fixture.commands(), vec![b":put \"lazy\"".to_vec()]);
        }
        if method != "tools/call" {
            fixture.assert_no_probes();
        }
        // A response after notifications/initialized is the handshake barrier;
        // do not cancel a still-pending legacy handshake during test teardown.
        let params = if method == "initialize" {
            json!({})
        } else {
            json!({"_meta":meta()})
        };
        writer
            .write_all(
                format!(
                    "{}\n",
                    json!({"jsonrpc":"2.0", "id":2, "method":"tools/list", "params":params})
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        line.clear();
        timeout(Duration::from_secs(5), reader.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        let response: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            response["result"]["tools"].as_array().unwrap().len(),
            1,
            "{response}"
        );
        cancellation.cancel();
        drop(writer);
        drop(reader);
        timeout(Duration::from_secs(5), service)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        server.shutdown().await;
    }
}
