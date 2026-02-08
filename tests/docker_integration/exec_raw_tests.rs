//! ExecRaw transfer tests using password authentication (no SSH keys required)

use super::common::*;
use std::path::PathBuf;
use std::time::SystemTime;

#[derive(Debug, Clone)]
pub struct TestEnvConfig {
    pub name: &'static str,
}

/// Run file PUT test for a given environment
async fn test_file_put(env: &TestEnvConfig, port: u16) {
    let unique = format!(
        "{}-{}-execraw",
        std::process::id(),
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );

    let local_base = PathBuf::from("target/tmp").join(format!("{}-{}", env.name, unique));
    std::fs::create_dir_all(&local_base).expect("create local base");

    let local_file = local_base.join("hello.txt");
    std::fs::write(&local_file, "hello via exec-raw\n").expect("write local file");

    let host = std::net::Ipv4Addr::LOCALHOST.to_string();
    let config = Config {
        host,
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

    // Resolve remote home
    let home_result = server
        .test_execute_command(r#"sh -c 'printf %s "$HOME"'"#)
        .await
        .expect("failed to resolve remote HOME");
    let remote_home = extract_text_from_result(&home_result).trim().to_string();

    let local_path_param = local_file.to_string_lossy().to_string();
    let remote_file = format!("{}/hello-execraw.txt", remote_home);

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
        "{}: file PUT should succeed: {:?}",
        env.name, resp.error
    );
    assert_eq!(
        resp.transport_used,
        TransferTransport::ExecRaw,
        "{}: should use ExecRaw transport",
        env.name
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
        verify_text.contains("hello via exec-raw"),
        "{}: remote file should contain expected content",
        env.name
    );

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(&local_base);
}

/// Run file GET test for a given environment
async fn test_file_get(env: &TestEnvConfig, port: u16) {
    let unique = format!(
        "{}-{}-execraw",
        std::process::id(),
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );

    let local_base = PathBuf::from("target/tmp").join(format!("{}-get-{}", env.name, unique));
    std::fs::create_dir_all(&local_base).expect("create local base");

    let host = std::net::Ipv4Addr::LOCALHOST.to_string();
    let config = Config {
        host,
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

    // Resolve remote home and create a file
    let home_result = server
        .test_execute_command(r#"sh -c 'printf %s "$HOME"'"#)
        .await
        .expect("failed to resolve remote HOME");
    let remote_home = extract_text_from_result(&home_result).trim().to_string();

    let remote_file = format!("{}/get-test.txt", remote_home);

    // Create file on remote
    let create_result = server
        .test_execute_command(&format!(
            "sh -c 'echo \"file content from remote\" > {}'",
            ssh_mcp::escape_for_shell(&remote_file)
        ))
        .await
        .expect("create remote file");
    let create_text = extract_text_from_result(&create_result);
    assert!(
        !create_text.contains("error"),
        "{}: creating remote file should not error",
        env.name
    );

    let local_download = local_base.join("downloaded.txt");
    let local_path_param = local_download.to_string_lossy().to_string();

    let resp = server
        .test_transfer(TransferParams {
            operation: TransferOperation::Get,
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
        "{}: file GET should succeed: {:?}",
        env.name, resp.error
    );

    // Verify local file was created and has correct content
    assert!(
        local_download.exists(),
        "{}: local file should exist after GET",
        env.name
    );
    let content = std::fs::read_to_string(&local_download).expect("read local file");
    assert!(
        content.contains("file content from remote"),
        "{}: local file should have correct content",
        env.name
    );

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(&local_base);
}

/// Run directory PUT test for a given environment
async fn test_dir_put(env: &TestEnvConfig, port: u16) {
    let unique = format!(
        "{}-{}-execraw",
        std::process::id(),
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );

    let local_base = PathBuf::from("target/tmp").join(format!("{}-dirput-{}", env.name, unique));
    std::fs::create_dir_all(&local_base).expect("create local base");

    // Create local directory tree
    let local_dir = local_base.join("upload_dir");
    std::fs::create_dir_all(local_dir.join("subdir")).expect("create local subdir");
    std::fs::write(
        local_dir.join("subdir").join("file.txt"),
        "nested content\n",
    )
    .expect("write nested file");
    std::fs::write(local_dir.join("top.txt"), "top level\n").expect("write top file");

    let host = std::net::Ipv4Addr::LOCALHOST.to_string();
    let config = Config {
        host,
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

    // Resolve remote home
    let home_result = server
        .test_execute_command(r#"sh -c 'printf %s "$HOME"'"#)
        .await
        .expect("failed to resolve remote HOME");
    let remote_home = extract_text_from_result(&home_result).trim().to_string();

    let local_dir_param = local_dir.to_string_lossy().to_string();
    let remote_dir = format!("{}/received_dir", remote_home);

    let resp = server
        .test_transfer(TransferParams {
            operation: TransferOperation::Put,
            local_path: local_dir_param,
            remote_path: remote_dir.clone(),
            transport: TransferTransport::ExecRaw,
            kind: Some(TransferKind::Directory),
            overwrite: true,
            timeout_ms: Some(30000),
            verbose: false,
            ..Default::default()
        })
        .await;

    assert!(
        resp.ok,
        "{}: directory PUT should succeed: {:?}",
        env.name, resp.error
    );

    // Verify directory structure on remote
    let verify = server
        .test_execute_command(&format!(
            "sh -c 'test -f {}/subdir/file.txt && test -f {}/top.txt && printf ok'",
            ssh_mcp::escape_for_shell(&remote_dir),
            ssh_mcp::escape_for_shell(&remote_dir)
        ))
        .await
        .expect("verify remote dir");
    assert!(
        extract_text_from_result(&verify).contains("ok"),
        "{}: remote directory structure should be correct",
        env.name
    );

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(&local_base);
}

/// Run directory GET test for a given environment
async fn test_dir_get(env: &TestEnvConfig, port: u16) {
    let unique = format!(
        "{}-{}-execraw",
        std::process::id(),
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );

    let local_base = PathBuf::from("target/tmp").join(format!("{}-dirget-{}", env.name, unique));
    std::fs::create_dir_all(&local_base).expect("create local base");

    let host = std::net::Ipv4Addr::LOCALHOST.to_string();
    let config = Config {
        host,
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

    // Resolve remote home and create remote directory
    let home_result = server
        .test_execute_command(r#"sh -c 'printf %s "$HOME"'"#)
        .await
        .expect("failed to resolve remote HOME");
    let remote_home = extract_text_from_result(&home_result).trim().to_string();

    let remote_dir = format!("{}/source_dir", remote_home);

    // Create remote directory tree using tar-friendly commands
    let _ = server
        .test_execute_command(&format!(
            "sh -c 'mkdir -p {}/subdir && echo \"nested\" > {}/subdir/nested.txt && echo \"top\" > {}/top.txt'",
            ssh_mcp::escape_for_shell(&remote_dir),
            ssh_mcp::escape_for_shell(&remote_dir),
            ssh_mcp::escape_for_shell(&remote_dir)
        ))
        .await
        .expect("create remote dir structure");

    let local_download_dir = local_base.join("downloaded_dir");
    let local_path_param = local_download_dir.to_string_lossy().to_string();

    let resp = server
        .test_transfer(TransferParams {
            operation: TransferOperation::Get,
            local_path: local_path_param,
            remote_path: remote_dir.clone(),
            transport: TransferTransport::ExecRaw,
            kind: Some(TransferKind::Directory),
            overwrite: true,
            timeout_ms: Some(30000),
            verbose: false,
            ..Default::default()
        })
        .await;

    assert!(
        resp.ok,
        "{}: directory GET should succeed: {:?}",
        env.name, resp.error
    );

    // Verify local directory structure
    assert!(
        local_download_dir.exists(),
        "{}: local directory should exist after GET",
        env.name
    );
    assert!(
        local_download_dir
            .join("subdir")
            .join("nested.txt")
            .exists(),
        "{}: nested file should exist",
        env.name
    );
    assert!(
        local_download_dir.join("top.txt").exists(),
        "{}: top file should exist",
        env.name
    );

    // Verify content
    let nested_content =
        std::fs::read_to_string(local_download_dir.join("subdir").join("nested.txt"))
            .expect("read nested file");
    assert!(
        nested_content.contains("nested"),
        "{}: nested file should have correct content",
        env.name
    );

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(&local_base);
}

/// Run all transfer tests for a given environment
async fn run_all_transfer_tests(env: &TestEnvConfig, port: u16) {
    tracing::info!("Running ExecRaw transfer tests for: {}", env.name);
    test_file_put(env, port).await;
    tracing::info!("  file PUT: OK");
    test_file_get(env, port).await;
    tracing::info!("  file GET: OK");
    test_dir_put(env, port).await;
    tracing::info!("  directory PUT: OK");
    test_dir_get(env, port).await;
    tracing::info!("  directory GET: OK");
}

#[tokio::test]
async fn test_exec_raw_transfers_alpine_busybox() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("ssh_mcp=debug,info")
        .try_init();

    // Start linuxserver/openssh-server (Alpine with BusyBox)
    let container = GenericImage::new("lscr.io/linuxserver/openssh-server", "latest")
        .with_env_var("USER_NAME", "test")
        .with_env_var("PASSWORD_ACCESS", "true")
        .with_env_var("USER_PASSWORD", "secret")
        .start()
        .await
        .expect("Failed to start Alpine SSH container");

    let host = container
        .get_host()
        .await
        .expect("Failed to get container host");
    let port = container
        .get_host_port_ipv4(2222)
        .await
        .expect("Failed to get mapped SSH port");

    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
    tracing::info!("Alpine container ready at {}:{}", host, port);

    let env = TestEnvConfig { name: "alpine" };

    run_all_transfer_tests(&env, port).await;
    tracing::info!("Alpine/BusyBox tests completed successfully");
}

#[tokio::test]
async fn test_exec_raw_transfers_debian_gnu_tar() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("ssh_mcp=debug,info")
        .try_init();

    // Build the custom Debian SSH image if needed
    init_test_env().expect("Failed to initialize test environment");

    // Use custom Debian SSH image with test/secret credentials on port 2222
    let container = GenericImage::new("ssh-mcp-debian-sshd", "latest")
        .with_exposed_port(2222u16.into())
        .start()
        .await
        .expect("Failed to start Debian SSH container");

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
    tracing::info!("Debian container ready at {}:{}", host, port);

    let env = TestEnvConfig { name: "debian" };

    // Use test/secret credentials for the custom Debian image
    test_file_put_with_creds(&env, port, "test", "secret").await;
    tracing::info!("  file PUT: OK");
    test_file_get_with_creds(&env, port, "test", "secret").await;
    tracing::info!("  file GET: OK");
    test_dir_put_with_creds(&env, port, "test", "secret").await;
    tracing::info!("  directory PUT: OK");
    test_dir_get_with_creds(&env, port, "test", "secret").await;
    tracing::info!("  directory GET: OK");

    tracing::info!("Debian (GNU tar) tests completed successfully");
}

/// Test file PUT with specific credentials (uses /root as home for root user)
async fn test_file_put_with_creds(env: &TestEnvConfig, port: u16, user: &str, password: &str) {
    let unique = format!(
        "{}-{}-execraw",
        std::process::id(),
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );

    let local_base = PathBuf::from("target/tmp").join(format!("{}-{}", env.name, unique));
    std::fs::create_dir_all(&local_base).expect("create local base");

    let local_file = local_base.join("hello.txt");
    std::fs::write(&local_file, "hello via exec-raw\n").expect("write local file");

    let host = std::net::Ipv4Addr::LOCALHOST.to_string();
    let config = Config {
        host,
        port,
        user: user.to_string(),
        password: Some(password.to_string()),
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

    // Resolve remote home via command (works for any user including root)
    let home_result = server
        .test_execute_command(r#"sh -c 'printf %s "$HOME"'"#)
        .await
        .expect("failed to resolve remote HOME");
    let remote_home = extract_text_from_result(&home_result).trim().to_string();

    let local_path_param = local_file.to_string_lossy().to_string();
    let remote_file = format!("{}/hello-execraw.txt", remote_home);

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
        "{}: file PUT should succeed: {:?}",
        env.name, resp.error
    );
    assert_eq!(
        resp.transport_used,
        TransferTransport::ExecRaw,
        "{}: should use ExecRaw transport",
        env.name
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
        verify_text.contains("hello via exec-raw"),
        "{}: remote file should contain expected content",
        env.name
    );

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(&local_base);
}

/// Test file GET with specific credentials (uses /root as home for root user)
async fn test_file_get_with_creds(env: &TestEnvConfig, port: u16, user: &str, password: &str) {
    let unique = format!(
        "{}-{}-execraw",
        std::process::id(),
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );

    let local_base = PathBuf::from("target/tmp").join(format!("{}-get-{}", env.name, unique));
    std::fs::create_dir_all(&local_base).expect("create local base");

    let host = std::net::Ipv4Addr::LOCALHOST.to_string();
    let config = Config {
        host,
        port,
        user: user.to_string(),
        password: Some(password.to_string()),
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

    // Resolve remote home via command (works for any user including root)
    let home_result = server
        .test_execute_command(r#"sh -c 'printf %s "$HOME"'"#)
        .await
        .expect("failed to resolve remote HOME");
    let remote_home = extract_text_from_result(&home_result).trim().to_string();

    let remote_file = format!("{}/get-test.txt", remote_home);

    // Create file on remote
    let _create_result = server
        .test_execute_command(&format!(
            "sh -c 'echo \"file content from remote\" > {}'",
            ssh_mcp::escape_for_shell(&remote_file)
        ))
        .await
        .expect("create remote file");

    let local_download = local_base.join("downloaded.txt");
    let local_path_param = local_download.to_string_lossy().to_string();

    let resp = server
        .test_transfer(TransferParams {
            operation: TransferOperation::Get,
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
        "{}: file GET should succeed: {:?}",
        env.name, resp.error
    );

    // Verify local file was created and has correct content
    assert!(
        local_download.exists(),
        "{}: local file should exist after GET",
        env.name
    );
    let content = std::fs::read_to_string(&local_download).expect("read local file");
    assert!(
        content.contains("file content from remote"),
        "{}: local file should have correct content",
        env.name
    );

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(&local_base);
}

/// Test directory PUT with specific credentials (uses /root as home for root user)
async fn test_dir_put_with_creds(env: &TestEnvConfig, port: u16, user: &str, password: &str) {
    let unique = format!(
        "{}-{}-execraw",
        std::process::id(),
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );

    let local_base = PathBuf::from("target/tmp").join(format!("{}-dirput-{}", env.name, unique));
    std::fs::create_dir_all(&local_base).expect("create local base");

    // Create local directory tree
    let local_dir = local_base.join("upload_dir");
    std::fs::create_dir_all(local_dir.join("subdir")).expect("create local subdir");
    std::fs::write(
        local_dir.join("subdir").join("file.txt"),
        "nested content\n",
    )
    .expect("write nested file");
    std::fs::write(local_dir.join("top.txt"), "top level\n").expect("write top file");

    let host = std::net::Ipv4Addr::LOCALHOST.to_string();
    let config = Config {
        host,
        port,
        user: user.to_string(),
        password: Some(password.to_string()),
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

    // Resolve remote home via command (works for any user including root)
    let home_result = server
        .test_execute_command(r#"sh -c 'printf %s "$HOME"'"#)
        .await
        .expect("failed to resolve remote HOME");
    let remote_home = extract_text_from_result(&home_result).trim().to_string();

    let local_dir_param = local_dir.to_string_lossy().to_string();
    let remote_dir = format!("{}/received_dir", remote_home);

    let resp = server
        .test_transfer(TransferParams {
            operation: TransferOperation::Put,
            local_path: local_dir_param,
            remote_path: remote_dir.clone(),
            transport: TransferTransport::ExecRaw,
            kind: Some(TransferKind::Directory),
            overwrite: true,
            timeout_ms: Some(30000),
            verbose: false,
            ..Default::default()
        })
        .await;

    assert!(
        resp.ok,
        "{}: directory PUT should succeed: {:?}",
        env.name, resp.error
    );

    // Verify directory structure on remote
    let verify = server
        .test_execute_command(&format!(
            "sh -c 'test -f {}/subdir/file.txt && test -f {}/top.txt && printf ok'",
            ssh_mcp::escape_for_shell(&remote_dir),
            ssh_mcp::escape_for_shell(&remote_dir)
        ))
        .await
        .expect("verify remote dir");
    assert!(
        extract_text_from_result(&verify).contains("ok"),
        "{}: remote directory structure should be correct",
        env.name
    );

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(&local_base);
}

/// Test directory GET with specific credentials (uses /root as home for root user)
async fn test_dir_get_with_creds(env: &TestEnvConfig, port: u16, user: &str, password: &str) {
    let unique = format!(
        "{}-{}-execraw",
        std::process::id(),
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );

    let local_base = PathBuf::from("target/tmp").join(format!("{}-dirget-{}", env.name, unique));
    std::fs::create_dir_all(&local_base).expect("create local base");

    let host = std::net::Ipv4Addr::LOCALHOST.to_string();
    let config = Config {
        host,
        port,
        user: user.to_string(),
        password: Some(password.to_string()),
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

    // Resolve remote home via command (works for any user including root)
    let home_result = server
        .test_execute_command(r#"sh -c 'printf %s "$HOME"'"#)
        .await
        .expect("failed to resolve remote HOME");
    let remote_home = extract_text_from_result(&home_result).trim().to_string();

    let remote_dir = format!("{}/source_dir", remote_home);

    // Create remote directory tree using tar-friendly commands
    let _ = server
        .test_execute_command(&format!(
            "sh -c 'mkdir -p {}/subdir && echo \"nested\" > {}/subdir/nested.txt && echo \"top\" > {}/top.txt'",
            ssh_mcp::escape_for_shell(&remote_dir),
            ssh_mcp::escape_for_shell(&remote_dir),
            ssh_mcp::escape_for_shell(&remote_dir)
        ))
        .await
        .expect("create remote dir structure");

    let local_download_dir = local_base.join("downloaded_dir");
    let local_path_param = local_download_dir.to_string_lossy().to_string();

    let resp = server
        .test_transfer(TransferParams {
            operation: TransferOperation::Get,
            local_path: local_path_param,
            remote_path: remote_dir.clone(),
            transport: TransferTransport::ExecRaw,
            kind: Some(TransferKind::Directory),
            overwrite: true,
            timeout_ms: Some(30000),
            verbose: false,
            ..Default::default()
        })
        .await;

    assert!(
        resp.ok,
        "{}: directory GET should succeed: {:?}",
        env.name, resp.error
    );

    // Verify local directory structure
    assert!(
        local_download_dir.exists(),
        "{}: local directory should exist after GET",
        env.name
    );
    assert!(
        local_download_dir
            .join("subdir")
            .join("nested.txt")
            .exists(),
        "{}: nested file should exist",
        env.name
    );
    assert!(
        local_download_dir.join("top.txt").exists(),
        "{}: top file should exist",
        env.name
    );

    // Verify content
    let nested_content =
        std::fs::read_to_string(local_download_dir.join("subdir").join("nested.txt"))
            .expect("read nested file");
    assert!(
        nested_content.contains("nested"),
        "{}: nested file should have correct content",
        env.name
    );

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(&local_base);
}
