//! Fish-shell compatibility tests for SSH command execution.
//!
//! These tests validate that command payloads are executed via POSIX `sh`
//! even when the remote account login shell is fish.

use super::common::*;
use serde::Deserialize;
use std::path::Path;
use std::time::{Duration, Instant};

#[derive(Debug, Deserialize)]
struct BackgroundExecResponse {
    ok: bool,
    background: bool,
    job_id: String,
    pid: u32,
    log_path: String,
    log_exists: bool,
}

#[derive(Debug, Deserialize)]
struct CheckProcessResponse {
    state: String,
    running: bool,
    exit_code: Option<u32>,
    state_reason: Option<String>,
    elapsed_time: String,
    command: String,
    log_path: String,
    log_exists: bool,
    log_tail: String,
}

fn parse_check_process_response(result: &rmcp::model::CallToolResult) -> CheckProcessResponse {
    let text = extract_text_from_result(result);
    serde_json::from_str(&text).expect("check_process response should be valid JSON")
}

fn assert_local_log_file_present(log_path: &str) {
    let path = Path::new(log_path);
    assert!(
        path.is_absolute(),
        "log_path should be absolute: {log_path}"
    );
    let metadata = std::fs::metadata(path).expect("log_path should exist on local filesystem");
    assert!(metadata.is_file(), "log_path should be a file: {log_path}");
}

fn docker_test_config(host: &str, port: u16) -> Config {
    Config {
        host: host.to_string(),
        port,
        user: "test".to_string(),
        password: Some("secret".to_string()),
        key: None,
        su_password: None,
        sudo_password: None,
        timeout_ms: 30000,
        max_chars: Some(1000),
        max_output_tokens: Some(12000),
        disable_sudo: true,
        keepalive_interval: 30,
        keepalive_max: 3,
        reconnect_retries: 3,
        reconnect_backoff_ms: 250,
        health_probe_timeout_ms: 1500,
        strict_host_key_checking: ssh_mcp::HostKeyCheckMode::No,
        known_hosts: None,
    }
}

async fn create_server_with_readiness_poll(host: &str, port: u16) -> SshMcpServer {
    let deadline = Instant::now() + std::time::Duration::from_secs(20);
    let poll_interval = tokio::time::Duration::from_millis(250);

    loop {
        match SshMcpServer::new(docker_test_config(host, port)).await {
            Ok(server) => {
                let readiness = server
                    .test_execute_command_with_timeout_ms("echo READY", 3000)
                    .await;

                match readiness {
                    Ok(output) => {
                        let text = extract_text_from_result(&output);
                        if text.contains("READY") && !text.contains("SSH connection error") {
                            return server;
                        }

                        assert!(
                            Instant::now() < deadline,
                            "SSH server not ready at {}:{} within 20s; readiness output: {}",
                            host,
                            port,
                            text
                        );
                    }
                    Err(err) => {
                        assert!(
                            Instant::now() < deadline,
                            "SSH server not ready at {}:{} within 20s; readiness error: {}",
                            host,
                            port,
                            err
                        );
                    }
                }

                server.shutdown().await;
            }
            Err(err) => {
                assert!(
                    Instant::now() < deadline,
                    "SSH server not ready at {}:{} within 20s; last error: {}",
                    host,
                    port,
                    err
                );
            }
        }

        tokio::time::sleep(poll_interval).await;
    }
}

#[tokio::test]
async fn test_fish_shell_uses_posix_wrapper_for_assignment_and_pipe() {
    init_test_env().expect("Failed to initialize test environment");

    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("ssh_mcp=debug,info")
        .try_init();

    let container = GenericImage::new("ssh-mcp-debian-sshd-fish", "latest")
        .with_exposed_port(2222u16.into())
        .start()
        .await
        .expect("Failed to start fish-shell SSH container");

    let host = container
        .get_host()
        .await
        .expect("Failed to get container host");
    let port = container
        .get_host_port_ipv4(2222)
        .await
        .expect("Failed to get mapped SSH port");

    let host_text = host.to_string();
    let server = create_server_with_readiness_poll(&host_text, port).await;

    let shell_result = server
        .test_execute_command("getent passwd test | cut -d: -f7")
        .await
        .expect("Failed to query login shell from passwd database");
    let shell_text = extract_text_from_result(&shell_result);
    assert!(
        shell_text.contains("fish"),
        "Expected test user login shell to be fish, got: {}",
        shell_text
    );

    let result = server
        .test_execute_command_with_timeout_ms(
            "VAR=fish_value; [ \"$VAR\" = 'fish_value' ] && printf '%s\\n' \"$VAR\" | grep -Fx 'fish_value' >/dev/null && echo FISH_POSIX_OK",
            5000,
        )
        .await;

    assert!(
        result.is_ok(),
        "POSIX assignment + pipe command should succeed under fish login shell: {:?}",
        result
    );

    let output = result.expect("POSIX fish compatibility command unexpectedly failed");
    let text = extract_text_from_result(&output);
    assert!(
        text.contains("FISH_POSIX_OK"),
        "Expected FISH_POSIX_OK marker in output: {}",
        text
    );

    server.shutdown().await;
}

#[tokio::test]
async fn test_fish_shell_background_exec_and_check_process_workflow() {
    init_test_env().expect("Failed to initialize test environment");

    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("ssh_mcp=debug,info")
        .try_init();

    let container = GenericImage::new("ssh-mcp-debian-sshd-fish", "latest")
        .with_exposed_port(2222u16.into())
        .start()
        .await
        .expect("Failed to start fish-shell SSH container");

    let host = container
        .get_host()
        .await
        .expect("Failed to get container host");
    let port = container
        .get_host_port_ipv4(2222)
        .await
        .expect("Failed to get mapped SSH port");

    let host_text = host.to_string();
    let server = create_server_with_readiness_poll(&host_text, port).await;

    let shell_result = server
        .test_execute_command("getent passwd test | cut -d: -f7")
        .await
        .expect("Failed to query login shell from passwd database");
    let shell_text = extract_text_from_result(&shell_result);
    assert!(
        shell_text.contains("fish"),
        "Expected test user login shell to be fish, got: {}",
        shell_text
    );

    let bg_result = server
        .test_execute_background_command(
            "VAR=fish_bg_value; [ \"$VAR\" = 'fish_bg_value' ] && printf '%s\\n' \"$VAR\" | grep -Fx 'fish_bg_value' >/dev/null && sleep 1 && echo FISH_BG_OK",
        )
        .await
        .expect("Failed to start background command under fish login shell");

    let bg_text = extract_text_from_result(&bg_result);
    let bg_response: BackgroundExecResponse =
        serde_json::from_str(&bg_text).expect("Failed to parse background exec response");
    assert!(bg_response.ok, "background exec should return ok=true");
    assert!(
        bg_response.background,
        "background exec should return background=true"
    );
    assert!(!bg_response.job_id.is_empty(), "job_id should be present");
    assert!(bg_response.pid > 0, "pid should be greater than zero");
    assert!(
        !bg_response.log_path.is_empty(),
        "log_path should be present"
    );
    assert!(bg_response.log_exists, "log_exists should be true");
    assert_local_log_file_present(&bg_response.log_path);

    let mut last_status = None;
    for _ in 0..60 {
        let check_result = server
            .test_check_process(&bg_response.job_id, 50)
            .await
            .expect("Failed to check background process status");
        let status = parse_check_process_response(&check_result);

        if status.exit_code.is_some() {
            last_status = Some(status);
            break;
        }

        last_status = Some(status);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let status = last_status.expect("check_process status should be captured");
    assert!(
        !status.running,
        "Background job should be completed: {:?}",
        status
    );
    assert_eq!(
        status.exit_code,
        Some(0),
        "Background job should exit successfully: {:?}",
        status
    );
    assert_eq!(status.state, "completed");
    assert_eq!(status.state_reason, None);
    assert_eq!(status.log_path, bg_response.log_path);
    assert!(status.log_exists);
    assert!(
        status.log_tail.contains("FISH_BG_OK"),
        "Expected marker in background log tail: {}",
        status.log_tail
    );
    assert!(
        !status.elapsed_time.is_empty() || !status.command.is_empty(),
        "check_process should populate elapsed_time or command for completed job: {:?}",
        status
    );

    server.shutdown().await;
}
