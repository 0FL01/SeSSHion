//! E2E tests for output truncation behavior
//!
//! These tests verify that large command output is properly truncated when
//! it exceeds the configured token limit (default: 12,000 tokens ~ 48KB).
//!
//! The truncation behavior includes:
//! - Setting `stdout_truncated`/`stderr_truncated` flags
//! - Tracking total tokens in `stdout_total_tokens`/`stderr_total_tokens`
//! - Adding a truncation notice with helpful tips to the output

use super::common::*;
use std::time::Duration;

/// Approximate bytes per token (4 bytes = 1 token)
const BYTES_PER_TOKEN: usize = 4;

/// Default token limit for tests (12,000 tokens)
const DEFAULT_TOKEN_LIMIT: usize = 12_000;

/// Default token limit as the expected type for Config
const DEFAULT_TOKEN_LIMIT_CONFIG: Option<usize> = Some(DEFAULT_TOKEN_LIMIT);

/// Approximate byte limit (48KB)
const DEFAULT_BYTE_LIMIT: usize = DEFAULT_TOKEN_LIMIT * BYTES_PER_TOKEN;

/// Command to generate ~800KB of output (well above 48KB limit)
/// Each line is "y\n" = 2 bytes, so 400,000 lines = ~800KB
const LARGE_OUTPUT_LINES: usize = 400_000;

#[tokio::test]
async fn test_stdout_truncation() {
    init_test_env().expect("Failed to initialize test environment");

    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("ssh_mcp=debug,info")
        .try_init();

    tracing::info!(
        "Starting stdout truncation test - generating ~{}KB of output (limit: {}KB)",
        (LARGE_OUTPUT_LINES * 2) / 1024,
        DEFAULT_BYTE_LIMIT / 1024
    );

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
        timeout_ms: 120000,
        max_chars: Some(1000),
        max_output_tokens: DEFAULT_TOKEN_LIMIT_CONFIG,
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

    // Generate large output: yes | head -n N produces "y\n" repeated N times
    // Each line is 2 bytes ("y\n"), so this generates ~800KB
    let cmd = format!("yes | head -n {}", LARGE_OUTPUT_LINES);
    tracing::info!("Executing command: {}", cmd);

    let start_time = std::time::Instant::now();
    let result = server
        .test_execute_command(&cmd)
        .await
        .expect("Failed to execute large output command");
    let elapsed = start_time.elapsed();

    let output = extract_text_from_result(&result);
    let output_len = output.len();

    tracing::info!(
        "Command completed in {:?} - collected {} bytes ({} KB)",
        elapsed,
        output_len,
        output_len / 1024
    );

    // Verify 1: Output was truncated (should be ≤ 48KB + truncation message overhead)
    assert!(
        output_len <= DEFAULT_BYTE_LIMIT + 1000,
        "Output should be truncated to ~{} bytes, but got {} bytes",
        DEFAULT_BYTE_LIMIT,
        output_len
    );

    // Verify 2: Truncation notice is present
    assert!(
        output.contains("[Output truncated:"),
        "Truncation notice should be present in output"
    );

    // Verify 3: Output contains helpful tips
    assert!(
        output.contains("head -n 100") || output.contains("tail -n 100"),
        "Output should contain head/tail tips"
    );
    assert!(
        output.contains("SFTP") || output.contains("SCP"),
        "Output should mention SFTP/SCP for large files"
    );

    // Verify 4: Output is significantly smaller than the original ~800KB
    assert!(
        output_len < 100_000,
        "Output should be significantly truncated from original ~800KB, got {} bytes",
        output_len
    );

    server.shutdown().await;
    tracing::info!(
        "test_stdout_truncation passed - output correctly truncated to {} bytes",
        output_len
    );
}

#[tokio::test]
async fn test_stderr_truncation() {
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

    let config = Config {
        host: host.to_string(),
        port,
        user: "test".to_string(),
        password: Some("secret".to_string()),
        key: None,
        su_password: None,
        sudo_password: None,
        timeout_ms: 60000,
        max_chars: Some(1000),
        max_output_tokens: DEFAULT_TOKEN_LIMIT_CONFIG,
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

    // Generate large stderr output - write ~400KB to stderr
    // Each line is "err\n" = 4 bytes, so 100,000 lines = ~400KB
    let stderr_lines = 100_000;
    let cmd = format!(
        "for i in $(seq 1 {}); do echo 'err' >&2; done; echo 'done'",
        stderr_lines
    );

    tracing::info!("Executing command with large stderr output");

    let result = server
        .test_execute_command(&cmd)
        .await
        .expect("Failed to execute command");

    let output = extract_text_from_result(&result);
    let output_len = output.len();

    tracing::info!(
        "Command completed - collected {} bytes of combined stdout/stderr",
        output_len
    );

    // Verify 1: Output contains stderr truncation notice
    // Note: stderr is merged into the result via "--- stderr ---" separator
    assert!(
        output.contains("[Output truncated:"),
        "Truncation notice should be present for large stderr output"
    );

    // Verify 2: Output is truncated (should be ≤ ~50KB including overhead)
    assert!(
        output_len <= DEFAULT_BYTE_LIMIT + 2000,
        "Output should be truncated, got {} bytes",
        output_len
    );

    // Verify 3: Command completed marker is present (in non-truncated portion)
    assert!(
        output.contains("done"),
        "Output should contain the stdout 'done' marker"
    );

    server.shutdown().await;
    tracing::info!("test_stderr_truncation passed");
}

#[tokio::test]
async fn test_truncation_notice_present() {
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

    let config = Config {
        host: host.to_string(),
        port,
        user: "test".to_string(),
        password: Some("secret".to_string()),
        key: None,
        su_password: None,
        sudo_password: None,
        timeout_ms: 60000,
        max_chars: Some(1000),
        max_output_tokens: DEFAULT_TOKEN_LIMIT_CONFIG,
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

    // Generate output that exceeds the limit
    let cmd = "seq 1 20000"; // Generates ~100KB of output
    tracing::info!("Executing command: {}", cmd);

    let result = server
        .test_execute_command(cmd)
        .await
        .expect("Failed to execute command");

    let output = extract_text_from_result(&result);

    // Verify 1: Contains "truncated" keyword
    assert!(
        output.to_lowercase().contains("truncated"),
        "Output should contain 'truncated' keyword"
    );

    // Verify 2: Contains "head" tip
    assert!(
        output.contains("head"),
        "Truncation notice should mention 'head' command"
    );

    // Verify 3: Contains "tail" tip
    assert!(
        output.contains("tail"),
        "Truncation notice should mention 'tail' command"
    );

    // Verify 4: Contains "SFTP" or "SCP" tip
    assert!(
        output.contains("SFTP") || output.contains("SCP"),
        "Truncation notice should mention SFTP/SCP for large file transfers"
    );

    // Verify 5: Contains token count information
    assert!(
        output.contains("tokens total"),
        "Truncation notice should include token count"
    );

    server.shutdown().await;
    tracing::info!("test_truncation_notice_present passed");
}

#[tokio::test]
async fn test_no_truncation_small_output() {
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

    let config = Config {
        host: host.to_string(),
        port,
        user: "test".to_string(),
        password: Some("secret".to_string()),
        key: None,
        su_password: None,
        sudo_password: None,
        timeout_ms: 60000,
        max_chars: Some(1000),
        max_output_tokens: DEFAULT_TOKEN_LIMIT_CONFIG,
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

    // Small command that should NOT be truncated
    let cmd = "echo 'hello world'";
    tracing::info!("Executing small command: {}", cmd);

    let result = server
        .test_execute_command(cmd)
        .await
        .expect("Failed to execute command");

    let output = extract_text_from_result(&result);

    // Verify 1: Output is not truncated
    assert!(
        !output.contains("[Output truncated:"),
        "Small output should NOT contain truncation notice"
    );

    // Verify 2: Output contains expected content
    assert!(
        output.contains("hello world"),
        "Output should contain the original command output"
    );

    // Verify 3: Output size is small
    assert!(
        output.len() < 100,
        "Small command output should be under 100 bytes, got {} bytes",
        output.len()
    );

    // Test with slightly larger output that still fits within limit
    let cmd2 = "seq 1 100"; // ~300 bytes, well under 48KB limit
    let result2 = server
        .test_execute_command(cmd2)
        .await
        .expect("Failed to execute command");

    let output2 = extract_text_from_result(&result2);

    // Verify 4: Medium output is also not truncated
    assert!(
        !output2.contains("[Output truncated:"),
        "Medium output (seq 1 100) should NOT be truncated"
    );

    // Verify 5: Contains expected numbers
    assert!(
        output2.contains("1\n") && output2.contains("100"),
        "Output should contain sequence numbers"
    );

    server.shutdown().await;
    tracing::info!("test_no_truncation_small_output passed");
}
