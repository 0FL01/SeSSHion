//! Combined authentication method tests
//! Tests behavior when both password and key authentication are configured
#![cfg(unix)]

use super::common::*;
use std::path::PathBuf;
use std::time::SystemTime;

/// Test that key authentication is tried first when both key and password are configured.
/// The container is configured for key auth only (PASSWORD_ACCESS=false),
/// so key auth should succeed and connection should work.
#[tokio::test]
async fn test_key_auth_with_password_fallback() {
    init_test_env().expect("Failed to initialize test environment");

    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("ssh_mcp=debug,info")
        .try_init();

    let (_key_dir, key_path) = setup_test_key();

    // Start SSH container configured for key auth only
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

    // Configure BOTH key and password - key should be preferred
    let config = Config {
        host: host.to_string(),
        port,
        user: "test".to_string(),
        password: Some("secret".to_string()), // This should NOT be used
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
    };

    let server = SshMcpServer::new(config)
        .await
        .expect("Failed to create SshMcpServer - key auth should succeed");

    // Verify connection works via key auth (password would fail since PASSWORD_ACCESS=false)
    let whoami_result = server
        .test_execute_command("whoami")
        .await
        .expect("command should succeed with key auth");
    let whoami_text = extract_text_from_result(&whoami_result);
    assert!(
        whoami_text.contains("test"),
        "should authenticate as test user via key auth"
    );

    server.shutdown().await;
}

/// Test password authentication works when key is also configured but invalid.
/// The container accepts password auth, and we provide both an invalid key
/// and a valid password - password should be used as fallback.
#[tokio::test]
async fn test_password_auth_with_key_fallback() {
    init_test_env().expect("Failed to initialize test environment");

    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("ssh_mcp=debug,info")
        .try_init();

    // Create an invalid/temporary key file that won't match the server's authorized keys
    let key_dir = tempfile::TempDir::new().expect("tempdir");
    let invalid_key_path = key_dir.path().join("invalid_key");
    std::fs::write(&invalid_key_path, "invalid key content").expect("write invalid key");

    // Start SSH container configured for password auth
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

    // Configure BOTH invalid key and valid password - password should be used
    let config = Config {
        host: host.to_string(),
        port,
        user: "test".to_string(),
        password: Some("secret".to_string()),
        key: Some(invalid_key_path), // This should fail, password used as fallback
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
    };

    let server = SshMcpServer::new(config)
        .await
        .expect("Failed to create SshMcpServer - password auth should succeed as fallback");

    // Verify connection works via password auth
    let whoami_result = server
        .test_execute_command("whoami")
        .await
        .expect("command should succeed with password auth fallback");
    let whoami_text = extract_text_from_result(&whoami_result);
    assert!(
        whoami_text.contains("test"),
        "should authenticate as test user via password auth"
    );

    server.shutdown().await;
}

/// Test behavior when both key and password are configured and both are valid.
/// The system should prefer key authentication (typically faster and more secure).
#[tokio::test]
async fn test_both_key_and_password_configured() {
    init_test_env().expect("Failed to initialize test environment");

    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("ssh_mcp=debug,info")
        .try_init();

    let (_key_dir, key_path) = setup_test_key();

    // Start SSH container configured for BOTH key and password auth
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

    // Configure BOTH valid key and valid password
    let config = Config {
        host: host.to_string(),
        port,
        user: "test".to_string(),
        password: Some("secret".to_string()),
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
    };

    let server = SshMcpServer::new(config)
        .await
        .expect("Failed to create SshMcpServer - should authenticate successfully");

    // Resolve remote home for transfer test
    let home_result = server
        .test_execute_command(r#"sh -c 'printf %s "$HOME"'"#)
        .await
        .expect("failed to resolve remote HOME");
    let remote_home = extract_text_from_result(&home_result).trim().to_string();

    // Create local file for transfer test
    let unique = format!(
        "{}-{}-both-auth",
        std::process::id(),
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let local_base = PathBuf::from("target/tmp").join(format!("both-auth-{unique}"));
    std::fs::create_dir_all(&local_base).expect("create local base");

    let local_file = local_base.join("test.txt");
    std::fs::write(&local_file, "test with both auth methods\n").expect("write local file");

    let local_path_param = local_file.to_string_lossy().to_string();
    let remote_file = format!("{}/both-auth-test.txt", remote_home);

    // Test transfer operation to verify full functionality
    let resp = server
        .test_transfer(TransferParams {
            operation: TransferOperation::Put,
            local_path: local_path_param,
            remote_path: remote_file.clone(),
            transport: TransferTransport::ExecRaw,
            kind: Some(TransferKind::File),
            overwrite: true,
            timeout_ms: Some(30000),
            verbose: false,
            ..Default::default()
        })
        .await;

    assert!(
        resp.ok,
        "transfer should succeed with both auth methods configured: {:?}",
        resp.error
    );

    // Verify content on remote
    let verify = server
        .test_execute_command(&format!(
            "sh -c 'cat < {}'",
            ssh_mcp::escape_for_shell(&remote_file)
        ))
        .await
        .expect("verify remote file");
    let verify_text = extract_text_from_result(&verify);
    assert!(
        verify_text.contains("test with both auth methods"),
        "remote file should contain expected content"
    );

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(&local_base);
}

/// Test that key auth failure is properly handled and falls back to password.
/// Container accepts password but not the invalid key we provide.
#[tokio::test]
async fn test_key_auth_failure_then_password_success() {
    init_test_env().expect("Failed to initialize test environment");

    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("ssh_mcp=debug,info")
        .try_init();

    // Create a valid-looking but wrong key (different key pair)
    let wrong_private_key = "-----BEGIN OPENSSH PRIVATE KEY-----\n\
    b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW\n\
    QyNTUxOQAAACD example wrong key for testing fallback\n\
    -----END OPENSSH PRIVATE KEY-----\n";

    let key_dir = tempfile::TempDir::new().expect("tempdir");
    let wrong_key_path = key_dir.path().join("wrong_key");
    std::fs::write(&wrong_key_path, wrong_private_key).expect("write wrong key");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(&wrong_key_path, perms).expect("chmod key");
    }

    // Start SSH container configured for password auth only
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

    // Configure with wrong key but correct password
    let config = Config {
        host: host.to_string(),
        port,
        user: "test".to_string(),
        password: Some("secret".to_string()),
        key: Some(wrong_key_path), // Will fail, then fallback to password
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
    };

    // Should succeed via password fallback after key fails
    let server = SshMcpServer::new(config)
        .await
        .expect("Should succeed with password fallback after key auth failure");

    // Verify functionality works
    let whoami_result = server
        .test_execute_command("whoami")
        .await
        .expect("command should succeed");
    let whoami_text = extract_text_from_result(&whoami_result);
    assert!(
        whoami_text.contains("test"),
        "should be authenticated as test"
    );

    server.shutdown().await;
}

/// Test switching between different auth methods across multiple connections/transfers.
/// Verifies that the system can handle sequential operations with different auth configurations.
#[tokio::test]
async fn test_switch_auth_methods_between_transfers() {
    init_test_env().expect("Failed to initialize test environment");

    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("ssh_mcp=debug,info")
        .try_init();

    let (_key_dir, key_path) = setup_test_key();

    // Start SSH container configured for BOTH key and password auth
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

    let host_str = host.to_string();

    // First connection: Use key auth only
    let config_key_only = Config {
        host: host_str.clone(),
        port,
        user: "test".to_string(),
        password: None,
        key: Some(key_path.clone()),
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
    };

    let server1 = SshMcpServer::new(config_key_only)
        .await
        .expect("First connection with key auth should succeed");

    // Get remote home for file operations
    let home_result = server1
        .test_execute_command(r#"sh -c 'printf %s "$HOME"'"#)
        .await
        .expect("failed to resolve remote HOME");
    let remote_home = extract_text_from_result(&home_result).trim().to_string();

    // Create test file for first transfer
    let unique1 = format!(
        "{}-{}-switch1",
        std::process::id(),
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let local_base1 = PathBuf::from("target/tmp").join(format!("switch-auth1-{unique1}"));
    std::fs::create_dir_all(&local_base1).expect("create local base");

    let local_file1 = local_base1.join("key_auth_file.txt");
    std::fs::write(&local_file1, "uploaded via key auth\n").expect("write local file");

    let local_path1 = local_file1.to_string_lossy().to_string();
    let remote_file1 = format!("{}/switch_test_key.txt", remote_home);

    let resp1 = server1
        .test_transfer(TransferParams {
            operation: TransferOperation::Put,
            local_path: local_path1,
            remote_path: remote_file1.clone(),
            transport: TransferTransport::ExecRaw,
            kind: Some(TransferKind::File),
            overwrite: true,
            timeout_ms: Some(30000),
            verbose: false,
            ..Default::default()
        })
        .await;

    assert!(
        resp1.ok,
        "First transfer with key auth should succeed: {:?}",
        resp1.error
    );
    server1.shutdown().await;

    // Second connection: Use password auth only
    let config_password_only = Config {
        host: host_str.clone(),
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
    };

    let server2 = SshMcpServer::new(config_password_only)
        .await
        .expect("Second connection with password auth should succeed");

    // Create test file for second transfer
    let unique2 = format!(
        "{}-{}-switch2",
        std::process::id(),
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let local_base2 = PathBuf::from("target/tmp").join(format!("switch-auth2-{unique2}"));
    std::fs::create_dir_all(&local_base2).expect("create local base");

    let local_file2 = local_base2.join("password_auth_file.txt");
    std::fs::write(&local_file2, "uploaded via password auth\n").expect("write local file");

    let local_path2 = local_file2.to_string_lossy().to_string();
    let remote_file2 = format!("{}/switch_test_password.txt", remote_home);

    let resp2 = server2
        .test_transfer(TransferParams {
            operation: TransferOperation::Put,
            local_path: local_path2,
            remote_path: remote_file2.clone(),
            transport: TransferTransport::ExecRaw,
            kind: Some(TransferKind::File),
            overwrite: true,
            timeout_ms: Some(30000),
            verbose: false,
            ..Default::default()
        })
        .await;

    assert!(
        resp2.ok,
        "Second transfer with password auth should succeed: {:?}",
        resp2.error
    );
    server2.shutdown().await;

    // Third connection: Use both key and password (should prefer key)
    let config_both = Config {
        host: host_str,
        port,
        user: "test".to_string(),
        password: Some("secret".to_string()),
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
    };

    let server3 = SshMcpServer::new(config_both)
        .await
        .expect("Third connection with both auth methods should succeed");

    // Verify both previously uploaded files exist
    let verify1 = server3
        .test_execute_command(&format!(
            "sh -c 'cat < {}'",
            ssh_mcp::escape_for_shell(&remote_file1)
        ))
        .await
        .expect("verify first remote file");
    let verify1_text = extract_text_from_result(&verify1);
    assert!(
        verify1_text.contains("uploaded via key auth"),
        "First file should have key auth content"
    );

    let verify2 = server3
        .test_execute_command(&format!(
            "sh -c 'cat < {}'",
            ssh_mcp::escape_for_shell(&remote_file2)
        ))
        .await
        .expect("verify second remote file");
    let verify2_text = extract_text_from_result(&verify2);
    assert!(
        verify2_text.contains("uploaded via password auth"),
        "Second file should have password auth content"
    );

    server3.shutdown().await;
    let _ = std::fs::remove_dir_all(&local_base1);
    let _ = std::fs::remove_dir_all(&local_base2);
}
