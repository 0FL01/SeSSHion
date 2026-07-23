//! Integration tests for check_process tool
//!
//! These tests verify that the check_process tool correctly:
//! 1. Reports running status for active processes
//! 2. Reports exit codes for completed processes
//! 3. Reads log tails from log files
//! 4. Reports non-existent processes
//! 5. Works correctly with the full background workflow

use super::common::*;
use serde::Deserialize;
use std::path::Path;
use std::time::{Duration, Instant};

/// Response from check_process tool
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

/// Response from exec background=true
#[derive(Debug, Deserialize)]
struct BackgroundExecResponse {
    ok: bool,
    background: bool,
    job_id: String,
    pid: u32,
    log_path: String,
    log_exists: bool,
}

/// Response from timeout foreground command that gets backgrounded
#[derive(Debug, Deserialize)]
struct TimeoutBackgroundResponse {
    ok: bool,
    timeout: bool,
    background: bool,
    job_id: String,
    pid: u32,
    state: String,
    still_running: bool,
    exit_code: Option<u32>,
    state_reason: Option<String>,
    elapsed_time: String,
    log_exists: bool,
    log_tail: String,
    tail_lines_used: usize,
    log_path: String,
    hint: String,
}

/// Helper to parse check_process JSON response
fn parse_check_process_response(result: &rmcp::model::CallToolResult) -> CheckProcessResponse {
    let text = extract_text_from_result(result);
    serde_json::from_str(&text).expect("check_process response should be valid JSON")
}

fn assert_local_log_file_present(log_path: &str) {
    let p = Path::new(log_path);
    assert!(p.is_absolute(), "log_path should be absolute: {log_path}");
    let meta = std::fs::metadata(p).expect("log_path should exist on local filesystem");
    assert!(meta.is_file(), "log_path should be a file: {log_path}");
}

#[tokio::test]
async fn test_check_process_running() {
    init_test_env().expect("Failed to initialize test environment");

    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("ssh_mcp=debug,info")
        .try_init();

    // Start ssh-mcp-debian-sshd container
    let container = GenericImage::new("ssh-mcp-debian-sshd", "latest")
        .with_exposed_port(2222u16.into())
        .start()
        .await
        .expect("Failed to start SSH container");

    let host = container
        .get_host()
        .await
        .expect("Failed to get container host");
    let port = container
        .get_host_port_ipv4(2222)
        .await
        .expect("Failed to get mapped SSH port");

    tokio::time::sleep(Duration::from_secs(5)).await;
    tracing::info!("Container ready at {}:{}", host, port);

    let config = Config {
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
    };

    let server = SshMcpServer::new(config)
        .await
        .expect("Failed to create SshMcpServer");

    let bg_result = server
        .test_execute_background_command("sleep 60")
        .await
        .expect("Failed to start background command");

    let bg_text = extract_text_from_result(&bg_result);
    let bg_resp: BackgroundExecResponse =
        serde_json::from_str(&bg_text).expect("Failed to parse background response");
    assert!(bg_resp.ok);
    assert!(bg_resp.background);
    assert!(!bg_resp.job_id.is_empty());
    assert!(!bg_resp.log_path.is_empty());
    assert!(
        bg_resp.log_exists,
        "background exec should report log_exists=true"
    );
    assert_local_log_file_present(&bg_resp.log_path);

    tracing::info!(
        "Started background job_id={} pid={}",
        bg_resp.job_id,
        bg_resp.pid
    );

    tokio::time::sleep(Duration::from_millis(300)).await;

    let check_result = server
        .test_check_process(&bg_resp.job_id, 50)
        .await
        .expect("Failed to check process");

    let status = parse_check_process_response(&check_result);
    assert!(
        status.running,
        "Process {} should be running but got: {:?}",
        bg_resp.pid, status
    );
    assert_eq!(status.state, "running");
    assert!(
        status.exit_code.is_none(),
        "Running process should not have exit code"
    );
    assert_eq!(status.state_reason, None);
    assert!(
        !status.command.is_empty(),
        "Command name should be captured"
    );
    assert_eq!(status.log_path, bg_resp.log_path);
    assert!(status.log_exists, "running job should keep local log path");
    assert!(
        !status.elapsed_time.is_empty() && status.elapsed_time.chars().any(|c| c.is_ascii_digit()),
        "elapsed_time should be populated and include digits; got: '{}'",
        status.elapsed_time
    );

    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
    cancel_tx.send(()).expect("send wait cancellation");
    let cancelled_result = tokio::time::timeout(
        Duration::from_secs(2),
        server.test_check_process_with_wait_cancellation(&bg_resp.job_id, 10, 30, cancel_rx),
    )
    .await
    .expect("cancelled wait should return without waiting 30 seconds")
    .expect("cancelled check_process should return its initial snapshot");
    let cancelled_status = parse_check_process_response(&cancelled_result);
    assert_eq!(cancelled_status.state, "running");

    let after_cancel = server
        .test_check_process(&bg_resp.job_id, 10)
        .await
        .expect("Failed to check process after cancelling wait");
    assert!(
        parse_check_process_response(&after_cancel).running,
        "cancelling the local wait must not stop the remote job"
    );

    server.shutdown().await;
    tracing::info!("test_check_process_running passed");
}

#[tokio::test]
async fn test_check_process_completed() {
    init_test_env().expect("Failed to initialize test environment");

    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("ssh_mcp=debug,info")
        .try_init();

    // Start ssh-mcp-debian-sshd container
    let container = GenericImage::new("ssh-mcp-debian-sshd", "latest")
        .with_exposed_port(2222u16.into())
        .start()
        .await
        .expect("Failed to start SSH container");

    let host = container
        .get_host()
        .await
        .expect("Failed to get container host");
    let port = container
        .get_host_port_ipv4(2222)
        .await
        .expect("Failed to get mapped SSH port");

    tokio::time::sleep(Duration::from_secs(5)).await;
    tracing::info!("Container ready at {}:{}", host, port);

    let config = Config {
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
    };

    let server = SshMcpServer::new(config)
        .await
        .expect("Failed to create SshMcpServer");

    let bg_result = server
        .test_execute_background_command("sh -c 'sleep 1; echo WAIT_FOR_DONE; exit 7'")
        .await
        .expect("Failed to start background command");

    let bg_text = extract_text_from_result(&bg_result);
    let bg_resp: BackgroundExecResponse =
        serde_json::from_str(&bg_text).expect("Failed to parse background response");
    assert!(!bg_resp.log_path.is_empty());
    assert_local_log_file_present(&bg_resp.log_path);

    tracing::info!(
        "Started background job_id={} pid={}",
        bg_resp.job_id,
        bg_resp.pid
    );

    let wait_started = Instant::now();
    let check_result = server
        .test_check_process_with_wait(&bg_resp.job_id, 50, 2)
        .await
        .expect("Failed to wait before checking process");
    let waited = wait_started.elapsed();
    let status = parse_check_process_response(&check_result);

    assert!(
        waited >= Duration::from_secs(2),
        "running job should wait for the full interval; elapsed: {waited:?}"
    );
    assert!(
        !status.running,
        "Process {} should not be running but got: {:?}",
        bg_resp.pid, status
    );

    assert_eq!(status.state, "failed");
    assert_eq!(status.exit_code, Some(7));
    assert_eq!(status.state_reason, None);
    assert_eq!(status.log_path, bg_resp.log_path);
    assert!(status.log_exists, "completed job should preserve log path");
    assert!(status.log_tail.contains("WAIT_FOR_DONE"));

    let terminal_started = Instant::now();
    let terminal_result = server
        .test_check_process_with_wait(&bg_resp.job_id, 10, 2)
        .await
        .expect("terminal job check should succeed");
    assert!(
        terminal_started.elapsed() < Duration::from_secs(2),
        "terminal job should not wait for wait_for"
    );
    assert_eq!(
        parse_check_process_response(&terminal_result).state,
        "failed"
    );

    // Command name might be empty for completed processes
    tracing::info!("Completed process status: {:?}", status);

    server.shutdown().await;
    tracing::info!("test_check_process_completed passed");
}

#[tokio::test]
async fn test_check_process_not_exists() {
    init_test_env().expect("Failed to initialize test environment");

    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("ssh_mcp=debug,info")
        .try_init();

    // Start ssh-mcp-debian-sshd container
    let container = GenericImage::new("ssh-mcp-debian-sshd", "latest")
        .with_exposed_port(2222u16.into())
        .start()
        .await
        .expect("Failed to start SSH container");

    let host = container
        .get_host()
        .await
        .expect("Failed to get container host");
    let port = container
        .get_host_port_ipv4(2222)
        .await
        .expect("Failed to get mapped SSH port");

    tokio::time::sleep(Duration::from_secs(5)).await;
    tracing::info!("Container ready at {}:{}", host, port);

    let config = Config {
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
    };

    let server = SshMcpServer::new(config)
        .await
        .expect("Failed to create SshMcpServer");

    let job_id = "job-does-not-exist";

    let check_started = Instant::now();
    let check_result = server
        .test_check_process_with_wait(job_id, 50, 2)
        .await
        .expect("Failed to call check_process");
    assert!(
        check_started.elapsed() < Duration::from_secs(2),
        "unknown job should fail before wait_for"
    );

    assert!(
        check_result.is_error.unwrap_or(false),
        "expected check_process to error"
    );
    let text = extract_text_from_result(&check_result);
    assert!(
        text.contains("job not found") || text.contains("not found"),
        "unexpected error text: {text}"
    );

    server.shutdown().await;
    tracing::info!("test_check_process_not_exists passed");
}

#[tokio::test]
async fn test_check_process_log_tail() {
    init_test_env().expect("Failed to initialize test environment");

    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("ssh_mcp=debug,info")
        .try_init();

    // Start ssh-mcp-debian-sshd container
    let container = GenericImage::new("ssh-mcp-debian-sshd", "latest")
        .with_exposed_port(2222u16.into())
        .start()
        .await
        .expect("Failed to start SSH container");

    let host = container
        .get_host()
        .await
        .expect("Failed to get container host");
    let port = container
        .get_host_port_ipv4(2222)
        .await
        .expect("Failed to get mapped SSH port");

    tokio::time::sleep(Duration::from_secs(5)).await;
    tracing::info!("Container ready at {}:{}", host, port);

    let config = Config {
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
    };

    let server = SshMcpServer::new(config)
        .await
        .expect("Failed to create SshMcpServer");

    let bg_result = server
        .test_execute_background_command(
            "sh -c 'for i in $(seq 1 20); do echo \"Line $i\"; done; sleep 2'",
        )
        .await
        .expect("Failed to start background command");

    let bg_text = extract_text_from_result(&bg_result);
    let bg_resp: BackgroundExecResponse =
        serde_json::from_str(&bg_text).expect("Failed to parse background response");
    assert!(bg_resp.ok);
    assert!(bg_resp.background);
    assert!(!bg_resp.log_path.is_empty());
    assert!(bg_resp.log_exists);
    assert_local_log_file_present(&bg_resp.log_path);

    let mut tail5 = String::new();
    for _ in 0..20 {
        let check_result = server
            .test_check_process(&bg_resp.job_id, 5)
            .await
            .expect("Failed to check process");
        let status = parse_check_process_response(&check_result);
        tail5 = status.log_tail;
        if tail5.contains("Line 20") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    assert!(
        tail5.contains("Line 16") || tail5.contains("Line 17") || tail5.contains("Line 20"),
        "Log tail should contain recent lines. Got: {tail5}"
    );

    let check_result_10 = server
        .test_check_process(&bg_resp.job_id, 10)
        .await
        .expect("Failed to check process");
    let status_10 = parse_check_process_response(&check_result_10);
    assert_eq!(status_10.log_path, bg_resp.log_path);
    assert!(status_10.log_exists);
    assert!(
        status_10.log_tail.contains("Line 11") || status_10.log_tail.contains("Line 12"),
        "Log tail with 10 lines should contain Line 11-12. Got: {}",
        status_10.log_tail
    );

    let _ = server
        .test_execute_command(&format!("kill {} 2>/dev/null || true", bg_resp.pid))
        .await;

    server.shutdown().await;
    tracing::info!("test_check_process_log_tail passed");
}

#[tokio::test]
async fn test_check_process_full_workflow_timeout() {
    init_test_env().expect("Failed to initialize test environment");

    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("ssh_mcp=debug,info")
        .try_init();

    // Start ssh-mcp-debian-sshd container
    let container = GenericImage::new("ssh-mcp-debian-sshd", "latest")
        .with_exposed_port(2222u16.into())
        .start()
        .await
        .expect("Failed to start SSH container");

    let host = container
        .get_host()
        .await
        .expect("Failed to get container host");
    let port = container
        .get_host_port_ipv4(2222)
        .await
        .expect("Failed to get mapped SSH port");

    tokio::time::sleep(Duration::from_secs(5)).await;
    tracing::info!("Container ready at {}:{}", host, port);

    let config = Config {
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
    };

    let server = SshMcpServer::new(config)
        .await
        .expect("Failed to create SshMcpServer");

    // Start a long-running command that will timeout
    // We use execute_command_with_timeout to simulate timeout behavior
    let result = server
        .test_execute_command_with_timeout_ms("sleep 30", 500)
        .await;

    assert!(result.is_ok(), "Command should return Ok with timeout info");

    let result = result.expect("should get timeout/background exec result");
    let text = extract_text_from_result(&result);
    tracing::info!("Timeout response: {}", text);

    // Parse the timeout response
    let timeout_resp: TimeoutBackgroundResponse =
        serde_json::from_str(&text).expect("Failed to parse timeout response");

    assert!(!timeout_resp.ok, "Timeout response should have ok=false");
    assert!(
        timeout_resp.timeout,
        "Timeout response should have timeout=true"
    );
    assert!(
        timeout_resp.background,
        "Timeout response should have background=true"
    );

    assert!(!timeout_resp.job_id.is_empty(), "job_id should be present");
    assert_eq!(timeout_resp.state, "running");
    assert!(
        timeout_resp.still_running,
        "timeout handoff should report running=true"
    );
    assert_eq!(
        timeout_resp.exit_code, None,
        "running timeout handoff should not have exit code"
    );
    assert_eq!(timeout_resp.state_reason, None);
    assert!(
        !timeout_resp.elapsed_time.is_empty(),
        "timeout handoff should include elapsed_time"
    );
    assert_eq!(
        timeout_resp.tail_lines_used, 50,
        "timeout handoff should report the tail_lines_used snapshot"
    );
    assert!(
        !timeout_resp.log_path.is_empty(),
        "local log_path should be present"
    );
    assert!(
        timeout_resp.log_exists,
        "timeout handoff should confirm log exists"
    );
    assert_local_log_file_present(&timeout_resp.log_path);
    assert!(
        timeout_resp.log_tail.is_empty(),
        "sleep-only timeout handoff should not have unexpected log output: {:?}",
        timeout_resp.log_tail
    );
    assert!(
        timeout_resp.hint.contains("TIMEOUT_RECOVERY"),
        "hint should contain TIMEOUT_RECOVERY"
    );

    let pid = timeout_resp.pid;

    tracing::info!("Process running in background with PID: {}", pid);

    // Give the process a moment to ensure it's running
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Check the process - should still be running
    let check_result = server
        .test_check_process(&timeout_resp.job_id, 50)
        .await
        .expect("Failed to check process");

    let status = parse_check_process_response(&check_result);
    assert!(
        status.running,
        "Process {} should still be running after timeout. Got: {:?}",
        pid, status
    );
    assert_eq!(status.state, "running");
    assert!(
        status.exit_code.is_none(),
        "Running process should not have exit code"
    );
    assert_eq!(status.state_reason, None);
    assert!(
        !status.command.is_empty(),
        "Command should be captured: {}",
        status.command
    );
    assert_eq!(status.log_path, timeout_resp.log_path);
    assert!(status.log_exists);

    // Wait a bit more and check log tail is being populated
    tokio::time::sleep(Duration::from_secs(1)).await;

    let check_result2 = server
        .test_check_process(&timeout_resp.job_id, 10)
        .await
        .expect("Failed to check process");

    let status2 = parse_check_process_response(&check_result2);
    assert!(
        status2.running,
        "Process {} should still be running after 1.5s",
        pid
    );
    assert_eq!(status2.state, "running");

    tracing::info!("Process status after 1.5s: {:?}", status2);

    // Kill the process to clean up
    let kill_result = server
        .test_execute_command(&format!("kill {} 2>/dev/null || true", pid))
        .await
        .expect("Failed to kill process");

    let kill_text = extract_text_from_result(&kill_result);
    tracing::info!("Kill result: {}", kill_text);

    // Wait for process to exit
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Check again - should not be running
    let check_result3 = server
        .test_check_process(&timeout_resp.job_id, 50)
        .await
        .expect("Failed to check process");

    let status3 = parse_check_process_response(&check_result3);
    assert!(
        !status3.running,
        "Killed process {} should not be running. Got: {:?}",
        pid, status3
    );
    assert!(
        matches!(status3.state.as_str(), "failed" | "state_lost"),
        "killed job should end as failed or state_lost, got: {:?}",
        status3
    );

    server.shutdown().await;
    tracing::info!("test_check_process_full_workflow_timeout passed");
}

#[tokio::test]
async fn test_check_process_background_exec_workflow() {
    init_test_env().expect("Failed to initialize test environment");

    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("ssh_mcp=debug,info")
        .try_init();

    // Start ssh-mcp-debian-sshd container
    let container = GenericImage::new("ssh-mcp-debian-sshd", "latest")
        .with_exposed_port(2222u16.into())
        .start()
        .await
        .expect("Failed to start SSH container");

    let host = container
        .get_host()
        .await
        .expect("Failed to get container host");
    let port = container
        .get_host_port_ipv4(2222)
        .await
        .expect("Failed to get mapped SSH port");

    tokio::time::sleep(Duration::from_secs(5)).await;
    tracing::info!("Container ready at {}:{}", host, port);

    let config = Config {
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
    };

    let server = SshMcpServer::new(config)
        .await
        .expect("Failed to create SshMcpServer");

    // Start a background process using background=true via shell
    // We need to use the tool call interface to get the JSON response with PID
    let result = server
        .test_execute_command_with_timeout_ms("echo 'Starting background' && sleep 20", 500)
        .await;

    assert!(result.is_ok(), "Command should complete or timeout");

    let result = result.expect("should get background exec result");
    let text = extract_text_from_result(&result);
    tracing::info!("Response: {}", text);

    // If it timed out and went to background, parse the response
    if text.contains("\"timeout\":true") {
        let timeout_resp: TimeoutBackgroundResponse =
            serde_json::from_str(&text).expect("Failed to parse timeout response");

        assert!(!timeout_resp.job_id.is_empty(), "job_id should be present");
        assert_eq!(timeout_resp.state, "running");
        assert!(
            timeout_resp.still_running,
            "timeout handoff should initially report the job as still running"
        );
        assert_eq!(timeout_resp.exit_code, None);
        assert_eq!(timeout_resp.state_reason, None);
        assert_eq!(timeout_resp.tail_lines_used, 50);
        assert!(
            !timeout_resp.log_path.is_empty(),
            "local log_path should be present"
        );
        assert!(timeout_resp.log_exists);
        assert_local_log_file_present(&timeout_resp.log_path);
        assert!(
            timeout_resp.hint.contains("TIMEOUT_RECOVERY"),
            "hint should contain TIMEOUT_RECOVERY"
        );

        let pid = timeout_resp.pid;

        tracing::info!("Background process PID: {}", pid);

        // Verify process is running via check_process
        tokio::time::sleep(Duration::from_millis(300)).await;

        let check_result = server
            .test_check_process(&timeout_resp.job_id, 50)
            .await
            .expect("Failed to check process");

        let status = parse_check_process_response(&check_result);
        assert!(
            status.running,
            "Background process {} should be running",
            pid
        );
        assert_eq!(status.state, "running");

        // Kill it
        let _ = server
            .test_execute_command(&format!("kill {} 2>/dev/null || true", pid))
            .await;
    } else {
        // Command completed quickly (within timeout), no background
        tracing::info!("Command completed within timeout, no background process to check");
    }

    server.shutdown().await;
    tracing::info!("test_check_process_background_exec_workflow passed");
}

#[tokio::test]
async fn test_check_process_reports_missing_local_log() {
    init_test_env().expect("Failed to initialize test environment");

    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("ssh_mcp=debug,info")
        .try_init();

    let container = GenericImage::new("ssh-mcp-debian-sshd", "latest")
        .with_exposed_port(2222u16.into())
        .start()
        .await
        .expect("Failed to start SSH container");

    let host = container
        .get_host()
        .await
        .expect("Failed to get container host");
    let port = container
        .get_host_port_ipv4(2222)
        .await
        .expect("Failed to get mapped SSH port");

    tokio::time::sleep(Duration::from_secs(5)).await;

    let config = Config {
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
    };

    let server = SshMcpServer::new(config)
        .await
        .expect("Failed to create SshMcpServer");

    let bg_result = server
        .test_execute_background_command("sleep 10")
        .await
        .expect("Failed to start background command");
    let bg_resp: BackgroundExecResponse =
        serde_json::from_str(&extract_text_from_result(&bg_result)).expect("background JSON");

    tokio::time::sleep(Duration::from_millis(200)).await;
    std::fs::remove_file(&bg_resp.log_path).expect("remove local log path");

    let check_result = server
        .test_check_process(&bg_resp.job_id, 50)
        .await
        .expect("Failed to check process");
    let status = parse_check_process_response(&check_result);

    assert_eq!(status.state, "running");
    assert!(status.running);
    assert_eq!(status.log_path, bg_resp.log_path);
    assert!(
        !status.log_exists,
        "missing local log should be reported explicitly"
    );
    assert!(
        status.log_tail.is_empty(),
        "missing log path should yield empty tail"
    );

    let _ = server
        .test_execute_command(&format!("kill {} 2>/dev/null || true", bg_resp.pid))
        .await;
    server.shutdown().await;
}

#[tokio::test]
async fn test_check_process_recovers_persisted_state_in_new_server() {
    init_test_env().expect("Failed to initialize test environment");

    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("ssh_mcp=debug,info")
        .try_init();

    let container = GenericImage::new("ssh-mcp-debian-sshd", "latest")
        .with_exposed_port(2222u16.into())
        .start()
        .await
        .expect("Failed to start SSH container");

    let host = container
        .get_host()
        .await
        .expect("Failed to get container host");
    let port = container
        .get_host_port_ipv4(2222)
        .await
        .expect("Failed to get mapped SSH port");

    tokio::time::sleep(Duration::from_secs(5)).await;

    let config = Config {
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
    };

    let server = SshMcpServer::new(config.clone())
        .await
        .expect("Failed to create SshMcpServer");

    let bg_result = server
        .test_execute_background_command("sh -c 'sleep 1; echo RECOVERED_OK'")
        .await
        .expect("Failed to start background command");
    let bg_resp: BackgroundExecResponse =
        serde_json::from_str(&extract_text_from_result(&bg_result)).expect("background JSON");

    tokio::time::sleep(Duration::from_secs(2)).await;

    let recovered_server = SshMcpServer::new(config)
        .await
        .expect("Failed to create second SshMcpServer");
    let check_result = recovered_server
        .test_check_process(&bg_resp.job_id, 50)
        .await
        .expect("Recovered server should load persisted job state");
    let status = parse_check_process_response(&check_result);

    assert_eq!(status.state, "completed");
    assert!(!status.running);
    assert_eq!(status.exit_code, Some(0));
    assert_eq!(status.state_reason, None);
    assert_eq!(status.log_path, bg_resp.log_path);
    assert!(status.log_exists);
    assert!(status.log_tail.contains("RECOVERED_OK"));

    recovered_server.shutdown().await;
    server.shutdown().await;
}
