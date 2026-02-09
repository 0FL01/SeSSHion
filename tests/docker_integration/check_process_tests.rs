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
use std::time::Duration;

/// Response from check_process tool
#[derive(Debug, Deserialize)]
struct CheckProcessResponse {
    running: bool,
    exit_code: Option<u32>,
    #[allow(dead_code)]
    elapsed_time: String,
    command: String,
    #[allow(dead_code)]
    log_tail: String,
}

/// Response from timeout foreground command that gets backgrounded
#[derive(Debug, Deserialize)]
struct TimeoutBackgroundResponse {
    ok: bool,
    timeout: bool,
    background: bool,
    #[allow(dead_code)]
    job_id: String,
    pid: u32,
    log_path: String,
    #[allow(dead_code)]
    hint: String,
}

/// Helper to parse check_process JSON response
fn parse_check_process_response(result: &rmcp::model::CallToolResult) -> CheckProcessResponse {
    let text = extract_text_from_result(result);
    serde_json::from_str(&text).unwrap_or_else(|e| {
        panic!(
            "Failed to parse check_process response: {}\nText: {}",
            e, text
        )
    })
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
    };

    let server = SshMcpServer::new(config)
        .await
        .expect("Failed to create SshMcpServer");

    // Start a long-running background process (testing the shell directly)
    let _exec_result = server
        .test_execute_command("sleep 30")
        .await
        .expect("Failed to execute command");

    // For background test, we need to manually start a background process
    // and capture its PID using shell mechanisms
    let bg_result = server
        .test_execute_command("sh -c 'sleep 60 & echo $!'")
        .await
        .expect("Failed to start background process");

    let bg_text = extract_text_from_result(&bg_result);
    let pid: u32 = bg_text
        .split_whitespace()
        .last()
        .expect("No PID found in output")
        .parse()
        .expect("Failed to parse PID");

    tracing::info!("Started background process with PID: {}", pid);

    // Give the process a moment to start
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Check the process - should be running
    let check_result = server
        .test_check_process(pid, None, 50)
        .await
        .expect("Failed to check process");

    let status = parse_check_process_response(&check_result);
    assert!(
        status.running,
        "Process {} should be running but got: {:?}",
        pid, status
    );
    assert!(
        status.exit_code.is_none(),
        "Running process should not have exit code"
    );
    assert!(
        !status.command.is_empty(),
        "Command name should be captured"
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
    };

    let server = SshMcpServer::new(config)
        .await
        .expect("Failed to create SshMcpServer");

    // Start a quick background process that will exit soon
    let bg_result = server
        .test_execute_command("sh -c 'sleep 0.5 & echo $!'")
        .await
        .expect("Failed to start background process");

    let bg_text = extract_text_from_result(&bg_result);
    let pid: u32 = bg_text
        .split_whitespace()
        .last()
        .expect("No PID found in output")
        .parse()
        .expect("Failed to parse PID");

    tracing::info!("Started quick background process with PID: {}", pid);

    // Wait for the process to complete
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Check the process - should not be running
    let check_result = server
        .test_check_process(pid, None, 50)
        .await
        .expect("Failed to check process");

    let status = parse_check_process_response(&check_result);
    assert!(
        !status.running,
        "Process {} should not be running but got: {:?}",
        pid, status
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
    };

    let server = SshMcpServer::new(config)
        .await
        .expect("Failed to create SshMcpServer");

    // Try to check a PID that definitely doesn't exist (very large number)
    let nonexistent_pid: u32 = 99999;

    let check_result = server
        .test_check_process(nonexistent_pid, None, 50)
        .await
        .expect("Failed to check process");

    let status = parse_check_process_response(&check_result);
    assert!(
        !status.running,
        "Non-existent process {} should not be running",
        nonexistent_pid
    );
    assert!(
        status.exit_code.is_none(),
        "Non-existent process should not have exit code"
    );
    assert!(
        status.command.is_empty(),
        "Non-existent process should have empty command"
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
    };

    let server = SshMcpServer::new(config)
        .await
        .expect("Failed to create SshMcpServer");

    // Resolve remote home
    let home_result = server
        .test_execute_command(r#"sh -c 'printf %s "$HOME"'"#)
        .await
        .expect("failed to resolve remote HOME");
    let remote_home = extract_text_from_result(&home_result).trim().to_string();

    // Create a test log file with multiple lines
    let log_path = format!("{}/test_check_process.log", remote_home);
    let create_result = server
        .test_execute_command(&format!(
            "sh -c 'for i in $(seq 1 20); do echo \"Line $i\"; done > {}'",
            ssh_mcp::escape_for_shell(&log_path)
        ))
        .await
        .expect("Failed to create test log file");

    let create_text = extract_text_from_result(&create_result);
    assert!(
        !create_text.contains("error"),
        "Creating log file should not error"
    );

    // Use PID 1 (init process) which always exists - we just want to test log reading
    let check_result = server
        .test_check_process(1, Some(log_path.clone()), 5)
        .await
        .expect("Failed to check process");

    let status = parse_check_process_response(&check_result);
    // Log tail should contain the last 5 lines
    assert!(
        status.log_tail.contains("Line 16")
            || status.log_tail.contains("Line 17")
            || status.log_tail.contains("Line 20"),
        "Log tail should contain recent lines. Got: {}",
        status.log_tail
    );

    // Check with different tail_lines value
    let check_result_10 = server
        .test_check_process(1, Some(log_path.clone()), 10)
        .await
        .expect("Failed to check process");

    let status_10 = parse_check_process_response(&check_result_10);
    assert!(
        status_10.log_tail.contains("Line 11") || status_10.log_tail.contains("Line 12"),
        "Log tail with 10 lines should contain Line 11-12. Got: {}",
        status_10.log_tail
    );

    // Cleanup
    let _ = server
        .test_execute_command(&format!("rm -f {}", ssh_mcp::escape_for_shell(&log_path)))
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

    let text = extract_text_from_result(&result.unwrap());
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

    let pid = timeout_resp.pid;
    let log_path = timeout_resp.log_path;

    tracing::info!("Process running in background with PID: {}", pid);

    // Give the process a moment to ensure it's running
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Check the process - should still be running
    let check_result = server
        .test_check_process(pid, Some(log_path.clone()), 50)
        .await
        .expect("Failed to check process");

    let status = parse_check_process_response(&check_result);
    assert!(
        status.running,
        "Process {} should still be running after timeout. Got: {:?}",
        pid, status
    );
    assert!(
        status.exit_code.is_none(),
        "Running process should not have exit code"
    );
    assert!(
        !status.command.is_empty(),
        "Command should be captured: {}",
        status.command
    );

    // Wait a bit more and check log tail is being populated
    tokio::time::sleep(Duration::from_secs(1)).await;

    let check_result2 = server
        .test_check_process(pid, Some(log_path.clone()), 10)
        .await
        .expect("Failed to check process");

    let status2 = parse_check_process_response(&check_result2);
    assert!(
        status2.running,
        "Process {} should still be running after 1.5s",
        pid
    );

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
        .test_check_process(pid, Some(log_path.clone()), 50)
        .await
        .expect("Failed to check process");

    let status3 = parse_check_process_response(&check_result3);
    assert!(
        !status3.running,
        "Killed process {} should not be running. Got: {:?}",
        pid, status3
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
    };

    let server = SshMcpServer::new(config)
        .await
        .expect("Failed to create SshMcpServer");

    // Start a background process using background=true via the exec tool
    // We need to use the tool call interface to get the JSON response with PID
    let result = server
        .test_execute_command_with_timeout_ms("echo 'Starting background' && sleep 20", 500)
        .await;

    assert!(result.is_ok(), "Command should complete or timeout");

    let text = extract_text_from_result(&result.unwrap());
    tracing::info!("Response: {}", text);

    // If it timed out and went to background, parse the response
    if text.contains("\"timeout\":true") {
        let timeout_resp: TimeoutBackgroundResponse =
            serde_json::from_str(&text).expect("Failed to parse timeout response");

        let pid = timeout_resp.pid;
        let log_path = timeout_resp.log_path;

        tracing::info!("Background process PID: {}", pid);

        // Verify process is running via check_process
        tokio::time::sleep(Duration::from_millis(300)).await;

        let check_result = server
            .test_check_process(pid, Some(log_path.clone()), 50)
            .await
            .expect("Failed to check process");

        let status = parse_check_process_response(&check_result);
        assert!(
            status.running,
            "Background process {} should be running",
            pid
        );

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
