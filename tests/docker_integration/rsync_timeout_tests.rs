//! Rsync timeout E2E tests
//!
//! These tests verify that:
//! 1. Rsync operations properly timeout when the transfer takes too long
//! 2. No zombie rsync processes remain after timeout
//! 3. Timeout errors are properly propagated to the caller

#![cfg(unix)]

use super::common::*;
use std::path::PathBuf;
use std::time::SystemTime;

/// Test that rsync put operation times out correctly with a large file and short timeout
#[tokio::test]
async fn test_rsync_put_file_timeout() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("ssh_mcp=debug,info")
        .try_init();

    if !check_rsync() {
        tracing::warn!("skipping: local 'rsync' unavailable");
        return;
    }

    // Build the custom Debian SSH image if needed
    init_test_env().expect("Failed to build test image");

    // Start SSH container using custom Debian image with key auth
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

    // Generate test key
    let (_key_dir, key_path) = setup_test_key();

    let config = Config {
        host: host.to_string(),
        port,
        user: "test".to_string(),
        password: None,
        key: Some(key_path),
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

    // Resolve remote home
    let home_result = server
        .test_execute_command(r#"sh -c 'printf %s "$HOME"'"#)
        .await
        .expect("failed to resolve remote HOME");
    let remote_home = extract_text_from_result(&home_result).trim().to_string();

    // Create a large local file (10MB of random data to prevent compression)
    let unique = format!(
        "{}-{}-rsync-timeout-put",
        std::process::id(),
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let local_base = PathBuf::from("target/tmp").join(format!("rsync-timeout-{unique}"));
    std::fs::create_dir_all(&local_base).expect("create local base");

    let local_file = local_base.join("large_file.bin");

    // Generate 10MB of random data using /dev/urandom to prevent compression
    let output = std::process::Command::new("sh")
        .args([
            "-c",
            &format!(
                "dd if=/dev/urandom of={} bs=1M count=10 2>/dev/null",
                local_file.display()
            ),
        ])
        .output();
    match output {
        Ok(result) if result.status.success() => {}
        Ok(result) => {
            tracing::warn!("dd command failed, using fallback pattern: {:?}", result);
            // Fallback: create a file with non-compressible pattern
            let mut data = Vec::with_capacity(10 * 1024 * 1024);
            for i in 0..(10 * 1024 * 1024) {
                data.push((i % 256) as u8 ^ ((i / 256) % 256) as u8);
            }
            std::fs::write(&local_file, &data).expect("write local file with pattern");
        }
        Err(e) => {
            tracing::warn!("Failed to spawn dd: {}, using fallback pattern", e);
            let mut data = Vec::with_capacity(10 * 1024 * 1024);
            for i in 0..(10 * 1024 * 1024) {
                data.push((i % 256) as u8 ^ ((i / 256) % 256) as u8);
            }
            std::fs::write(&local_file, &data).expect("write local file with pattern");
        }
    }

    let local_path_param = local_file.to_string_lossy().to_string();
    let remote_file = format!("{}/rsync-timeout-test.bin", remote_home);

    // Attempt rsync put with a very short timeout (100ms) - should timeout
    let resp = server
        .test_transfer(TransferParams {
            operation: TransferOperation::Put,
            local_path: local_path_param,
            remote_path: remote_file.clone(),
            transport: TransferTransport::Rsync,
            kind: Some(TransferKind::File),
            overwrite: true,
            timeout_ms: Some(100), // Very short timeout to force timeout
            verbose: false,
            ..Default::default()
        })
        .await;

    // Verify the transfer failed
    assert!(!resp.ok, "rsync file PUT should fail with short timeout");

    // Verify error message indicates timeout
    let error_msg = resp.error.unwrap_or_default();
    assert!(
        error_msg.to_lowercase().contains("timeout"),
        "Error should indicate timeout, got: {}",
        error_msg
    );

    // Verify no rsync processes remain in the container
    // Give a small delay for process cleanup
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    let check_cmd =
        r#"sh -c 'pgrep -x rsync > /dev/null 2>&1 && echo "rsync_running" || echo "no_rsync"'"#;
    let check_result = server
        .test_execute_command(check_cmd)
        .await
        .expect("Failed to check for rsync processes");
    let check_output = extract_text_from_result(&check_result);

    assert!(
        check_output.contains("no_rsync"),
        "rsync process should not be running after timeout, but got: {}",
        check_output
    );

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(&local_base);
}

/// Test that rsync get operation times out correctly with a large remote file
#[tokio::test]
async fn test_rsync_get_file_timeout() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("ssh_mcp=debug,info")
        .try_init();

    if !check_rsync() {
        tracing::warn!("skipping: local 'rsync' unavailable");
        return;
    }

    // Build the custom Debian SSH image if needed
    init_test_env().expect("Failed to build test image");

    // Start SSH container using custom Debian image with key auth
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

    // Generate test key
    let (_key_dir, key_path) = setup_test_key();

    let config = Config {
        host: host.to_string(),
        port,
        user: "test".to_string(),
        password: None,
        key: Some(key_path),
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

    // Resolve remote home
    let home_result = server
        .test_execute_command(r#"sh -c 'printf %s "$HOME"'"#)
        .await
        .expect("failed to resolve remote HOME");
    let remote_home = extract_text_from_result(&home_result).trim().to_string();

    // Create a large remote file using dd
    let remote_file = format!("{}/rsync-large-remote.bin", remote_home);
    let create_result = server
        .test_execute_command(&format!(
            "sh -c 'dd if=/dev/urandom of={} bs=1M count=10 2>/dev/null && echo done'",
            ssh_mcp::escape_for_shell(&remote_file)
        ))
        .await
        .expect("failed to create large remote file");
    let create_output = extract_text_from_result(&create_result);
    assert!(
        create_output.contains("done"),
        "Failed to create large remote file"
    );

    // Create local download directory
    let unique = format!(
        "{}-{}-rsync-timeout-get",
        std::process::id(),
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let local_base = PathBuf::from("target/tmp").join(format!("rsync-timeout-{unique}"));
    std::fs::create_dir_all(&local_base).expect("create local base");

    let local_download = local_base.join("downloaded.bin");
    let local_path_param = local_download.to_string_lossy().to_string();

    // Attempt rsync get with a very short timeout (100ms) - should timeout
    let resp = server
        .test_transfer(TransferParams {
            operation: TransferOperation::Get,
            local_path: local_path_param,
            remote_path: remote_file.clone(),
            transport: TransferTransport::Rsync,
            kind: Some(TransferKind::File),
            overwrite: true,
            timeout_ms: Some(100), // Very short timeout to force timeout
            verbose: false,
            ..Default::default()
        })
        .await;

    // Verify the transfer failed
    assert!(!resp.ok, "rsync file GET should fail with short timeout");

    // Verify error message indicates timeout
    let error_msg = resp.error.unwrap_or_default();
    assert!(
        error_msg.to_lowercase().contains("timeout"),
        "Error should indicate timeout, got: {}",
        error_msg
    );

    // Verify no rsync processes remain in the container
    // Give a small delay for process cleanup
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    let check_cmd =
        r#"sh -c 'pgrep -x rsync > /dev/null 2>&1 && echo "rsync_running" || echo "no_rsync"'"#;
    let check_result = server
        .test_execute_command(check_cmd)
        .await
        .expect("Failed to check for rsync processes");
    let check_output = extract_text_from_result(&check_result);

    assert!(
        check_output.contains("no_rsync"),
        "rsync process should not be running after timeout, but got: {}",
        check_output
    );

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(&local_base);
}

/// Test that rsync directory put operation times out correctly
#[tokio::test]
async fn test_rsync_put_directory_timeout() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("ssh_mcp=debug,info")
        .try_init();

    if !check_rsync() {
        tracing::warn!("skipping: local 'rsync' unavailable");
        return;
    }

    // Build the custom Debian SSH image if needed
    init_test_env().expect("Failed to build test image");

    // Start SSH container using custom Debian image with key auth
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

    // Generate test key
    let (_key_dir, key_path) = setup_test_key();

    let config = Config {
        host: host.to_string(),
        port,
        user: "test".to_string(),
        password: None,
        key: Some(key_path),
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

    // Resolve remote home
    let home_result = server
        .test_execute_command(r#"sh -c 'printf %s "$HOME"'"#)
        .await
        .expect("failed to resolve remote HOME");
    let remote_home = extract_text_from_result(&home_result).trim().to_string();

    // Create local directory with multiple large files
    let unique = format!(
        "{}-{}-rsync-timeout-dirput",
        std::process::id(),
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let local_base = PathBuf::from("target/tmp").join(format!("rsync-timeout-{unique}"));
    std::fs::create_dir_all(&local_base).expect("create local base");

    let local_dir = local_base.join("upload_dir");
    std::fs::create_dir_all(&local_dir).expect("create local dir");

    // Create several files with random data from /dev/urandom
    for i in 0..5 {
        let file_path = local_dir.join(format!("large_file_{}.bin", i));
        let output = std::process::Command::new("sh")
            .args([
                "-c",
                &format!(
                    "dd if=/dev/urandom of={} bs=1M count=2 2>/dev/null",
                    file_path.display()
                ),
            ])
            .output();
        match output {
            Ok(result) if result.status.success() => {}
            _ => {
                // Fallback: create file with non-compressible pattern
                let mut data = Vec::with_capacity(2 * 1024 * 1024);
                let offset = i * 10000;
                for j in 0..(2 * 1024 * 1024) {
                    data.push(((j + offset) % 256) as u8 ^ (((j + offset) / 256) % 256) as u8);
                }
                std::fs::write(&file_path, &data).expect("write local file with pattern");
            }
        }
    }

    let local_dir_param = local_dir.to_string_lossy().to_string();
    let remote_dir = format!("{}/rsync_timeout_dir", remote_home);

    // Attempt rsync put with a very short timeout (100ms) - should timeout
    let resp = server
        .test_transfer(TransferParams {
            operation: TransferOperation::Put,
            local_path: local_dir_param,
            remote_path: remote_dir.clone(),
            transport: TransferTransport::Rsync,
            kind: Some(TransferKind::Directory),
            overwrite: true,
            timeout_ms: Some(100), // Very short timeout to force timeout
            verbose: false,
            ..Default::default()
        })
        .await;

    // Verify the transfer failed
    assert!(
        !resp.ok,
        "rsync directory PUT should fail with short timeout"
    );

    // Verify error message indicates timeout
    let error_msg = resp.error.unwrap_or_default();
    assert!(
        error_msg.to_lowercase().contains("timeout"),
        "Error should indicate timeout, got: {}",
        error_msg
    );

    // Verify no rsync processes remain in the container
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    let check_cmd =
        r#"sh -c 'pgrep -x rsync > /dev/null 2>&1 && echo "rsync_running" || echo "no_rsync"'"#;
    let check_result = server
        .test_execute_command(check_cmd)
        .await
        .expect("Failed to check for rsync processes");
    let check_output = extract_text_from_result(&check_result);

    assert!(
        check_output.contains("no_rsync"),
        "rsync process should not be running after timeout, but got: {}",
        check_output
    );

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(&local_base);
}
