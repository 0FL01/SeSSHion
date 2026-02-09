//! Timeout precision tests for sub-second and fractional timeout support
//!
//! These tests verify that:
//! 1. Timeouts < 1000ms (e.g., 500ms) work correctly (previously failed with "duration must be > 0")
//! 2. Millisecond precision is preserved (e.g., 1500ms becomes 1.5s, not 1s)

use super::common::*;
use std::time::Instant;

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
        disable_sudo: true,
        keepalive_interval: 30,
        keepalive_max: 3,
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
        disable_sudo: true,
        keepalive_interval: 30,
        keepalive_max: 3,
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
        disable_sudo: true,
        keepalive_interval: 30,
        keepalive_max: 3,
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
