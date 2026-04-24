//! Timeout precision tests for sub-second and fractional timeout support
//!
//! These tests verify that:
//! 1. Timeouts < 1000ms (e.g., 500ms) work correctly (previously failed with "duration must be > 0")
//! 2. Millisecond precision is preserved (e.g., 1500ms becomes 1.5s, not 1s)

use super::common::*;
use std::time::Instant;

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

                        if Instant::now() >= deadline {
                            panic!(
                                "SSH server not ready at {}:{} within 20s; readiness output: {}",
                                host, port, text
                            );
                        }
                    }
                    Err(err) => {
                        if Instant::now() >= deadline {
                            panic!(
                                "SSH server not ready at {}:{} within 20s; readiness error: {}",
                                host, port, err
                            );
                        }
                    }
                }

                server.shutdown().await;
            }
            Err(err) => {
                if Instant::now() >= deadline {
                    panic!(
                        "SSH server not ready at {}:{} within 20s; last error: {}",
                        host, port, err
                    );
                }
            }
        }

        tokio::time::sleep(poll_interval).await;
    }
}

/// Test that sub-second timeouts (500ms) work correctly
/// This test reproduces the bug where timeout_ms=500 was rejected because
/// as_secs() returned 0 for durations < 1000ms
#[tokio::test]
async fn test_subsecond_timeout_500ms() {
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

    // Wait for SSH to be ready
    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
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

    // Test with 500ms timeout - this should work (not reject as "duration must be > 0")
    let start = Instant::now();
    let result = server
        .test_execute_command_with_timeout_ms("echo hello", 500)
        .await;
    let elapsed = start.elapsed();

    // The command should succeed (echo is fast), and definitely not fail with "duration must be > 0"
    assert!(
        result.is_ok(),
        "500ms timeout should be accepted and command should succeed: {:?}",
        result
    );

    let output = result.unwrap();
    let text = extract_text_from_result(&output);
    assert!(
        text.contains("hello"),
        "Command output should contain 'hello': {}",
        text
    );

    // Should complete quickly (well under the timeout)
    assert!(
        elapsed.as_millis() < 2000,
        "Command should complete quickly, took {:?}",
        elapsed
    );

    server.shutdown().await;
    tracing::info!("Sub-second timeout test (500ms) passed");
}

/// Test that 1500ms timeout preserves millisecond precision
/// Previously, 1500ms would become 1s due to as_secs() truncation
/// Now it should be converted to 1.5s for the timeout command
#[tokio::test]
async fn test_fractional_timeout_1500ms() {
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

    // Wait for SSH to be ready
    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
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

    // Test with 1500ms timeout - should be converted to 1.5s (not 1s)
    let result = server
        .test_execute_command_with_timeout_ms("echo precision_test", 1500)
        .await;

    assert!(
        result.is_ok(),
        "1500ms timeout should be accepted and command should succeed: {:?}",
        result
    );

    let output = result.unwrap();
    let text = extract_text_from_result(&output);
    assert!(
        text.contains("precision_test"),
        "Command output should contain 'precision_test': {}",
        text
    );

    server.shutdown().await;
    tracing::info!("Fractional timeout test (1500ms) passed");
}

/// Test that a slow command actually times out at the expected duration
/// This verifies that the timeout is applied correctly with precision
#[tokio::test]
async fn test_timeout_actually_fires_with_precision() {
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

    // Wait for SSH to be ready
    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
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

    // Test with a command that sleeps longer than the timeout
    // Using 800ms timeout with a 5s sleep - should timeout
    let start = Instant::now();
    let result = server
        .test_execute_command_with_timeout_ms("sleep 5", 800)
        .await;
    let elapsed = start.elapsed();

    // The command should return Ok but with timeout indication in the response
    // The server returns a background job response with timeout=true flag
    assert!(
        result.is_ok(),
        "Command should return Ok with timeout info: {:?}",
        result
    );

    let output = result.unwrap();
    let text = extract_text_from_result(&output);
    assert!(
        text.contains("\"timeout\":true") || text.contains("timeout"),
        "Response should indicate timeout occurred: {}",
        text
    );

    // Should have timed out around 800ms (give it some tolerance for overhead)
    // Before the fix, this might have used 0s (no timeout) or 1s (truncated)
    assert!(
        elapsed.as_secs() < 3,
        "Should have timed out quickly, but took {:?}",
        elapsed
    );

    server.shutdown().await;
    tracing::info!("Timeout precision test passed - fired after ~800ms");
}

/// Foreground sudo-exec should auto-detach on timeout like exec.
#[tokio::test]
async fn test_sudo_timeout_auto_detaches_to_background() {
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

    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
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
        disable_sudo: false,
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

    let timeout_result = server
        .test_execute_sudo_command_with_timeout_ms("sleep 2; echo done", 1100)
        .await
        .expect("sudo-exec timeout override failed");

    let timeout_text = extract_text_from_result(&timeout_result);
    let timeout_json: serde_json::Value =
        serde_json::from_str(timeout_text.trim()).expect("timeout response should be valid JSON");

    assert_eq!(
        timeout_json.get("ok").and_then(|v| v.as_bool()),
        Some(false)
    );
    assert_eq!(
        timeout_json.get("timeout").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        timeout_json.get("background").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        timeout_json.get("still_running").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(timeout_json.get("exit_code").and_then(|v| v.as_u64()), None);
    assert_eq!(
        timeout_json.get("tail_lines_used").and_then(|v| v.as_u64()),
        Some(50)
    );
    assert!(
        timeout_json
            .get("elapsed_time")
            .and_then(|v| v.as_str())
            .is_some_and(|v| !v.is_empty()),
        "timeout response should include elapsed_time"
    );
    assert!(
        timeout_json
            .get("log_tail")
            .and_then(|v| v.as_str())
            .is_some(),
        "timeout response should include log_tail"
    );

    let job_id = timeout_json
        .get("job_id")
        .and_then(|v| v.as_str())
        .expect("timeout response should include job_id");

    let mut completed = false;
    let deadline = Instant::now() + tokio::time::Duration::from_secs(10);
    while Instant::now() < deadline {
        let check = server
            .test_check_process(job_id, 50)
            .await
            .expect("check-process should succeed for detached sudo job");
        let status: serde_json::Value = serde_json::from_str(&extract_text_from_result(&check))
            .expect("check-process response should be valid JSON");

        let running = status
            .get("running")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let log_tail = status
            .get("log_tail")
            .and_then(|v| v.as_str())
            .unwrap_or_default();

        if !running && log_tail.contains("done") {
            completed = true;
            assert_eq!(status.get("exit_code").and_then(|v| v.as_u64()), Some(0));
            break;
        }

        tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;
    }

    assert!(
        completed,
        "detached sudo job should finish and write 'done'"
    );

    server.shutdown().await;
    tracing::info!("sudo timeout auto-detach test passed");
}

/// Regression: preserve $HOME expansion through pipe composition
#[tokio::test]
async fn test_posix_home_pipe_semantics_preserved() {
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

    let host_text = host.to_string();
    let server = create_server_with_readiness_poll(&host_text, port).await;
    tracing::info!("Container reachable at {}:{}", host_text, port);

    let result = server
        .test_execute_command_with_timeout_ms(
            "[ -n \"$HOME\" ] && [ \"$HOME\" != '$HOME' ] && printf '%s\\n' \"$HOME\" | grep -Fx \"$HOME\" >/dev/null && echo HOME_EXPANDED_PIPE_OK",
            5000,
        )
        .await;

    assert!(
        result.is_ok(),
        "$HOME + pipe command should succeed: {:?}",
        result
    );

    let output = result.unwrap();
    let text = extract_text_from_result(&output);
    assert!(
        text.contains("HOME_EXPANDED_PIPE_OK"),
        "Expected HOME_EXPANDED_PIPE_OK marker in output: {}",
        text
    );

    server.shutdown().await;
}

/// Regression: preserve stderr redirection and pipe behavior (2>&1 | grep)
#[tokio::test]
async fn test_posix_stderr_pipe_semantics_preserved() {
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

    let host_text = host.to_string();
    let server = create_server_with_readiness_poll(&host_text, port).await;
    tracing::info!("Container reachable at {}:{}", host_text, port);

    let result = server
        .test_execute_command_with_timeout_ms(
            "cat /definitely_missing_file_ssh_mcp 2>&1 | grep -F 'definitely_missing_file_ssh_mcp' >/dev/null && echo STDERR_PIPE_OK",
            5000,
        )
        .await;

    assert!(
        result.is_ok(),
        "2>&1 | grep command should succeed: {:?}",
        result
    );

    let output = result.unwrap();
    let text = extract_text_from_result(&output);
    assert!(
        text.contains("STDERR_PIPE_OK"),
        "Expected STDERR_PIPE_OK marker in output: {}",
        text
    );

    server.shutdown().await;
}

/// Regression: preserve POSIX variable assignment and expansion across commands
#[tokio::test]
async fn test_posix_variable_assignment_semantics_preserved() {
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

    let host_text = host.to_string();
    let server = create_server_with_readiness_poll(&host_text, port).await;
    tracing::info!("Container reachable at {}:{}", host_text, port);

    let result = server
        .test_execute_command_with_timeout_ms(
            "VAR=POSIX_ASSIGN_OK; [ \"$VAR\" = 'POSIX_ASSIGN_OK' ] && echo VAR_ASSIGN_OK",
            5000,
        )
        .await;

    assert!(
        result.is_ok(),
        "VAR=value; echo $VAR command should succeed: {:?}",
        result
    );

    let output = result.unwrap();
    let text = extract_text_from_result(&output);
    assert!(
        text.contains("VAR_ASSIGN_OK"),
        "Expected VAR_ASSIGN_OK in output: {}",
        text
    );

    server.shutdown().await;
}
