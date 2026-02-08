//! SCP transfer tests using SSH key authentication
#![cfg(unix)]

use super::common::*;
use std::path::PathBuf;
use std::time::SystemTime;

#[tokio::test]
async fn test_scp_file_put_with_key_auth() {
    init_test_env().expect("Failed to initialize test environment");

    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("ssh_mcp=debug,info")
        .try_init();

    if !check_scp() {
        tracing::warn!("skipping: local 'scp' unavailable");
        return;
    }

    let (_key_dir, key_path) = setup_test_key();

    // Start SSH container configured for key auth
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

    // Create local file
    let unique = format!(
        "{}-{}-scp-put",
        std::process::id(),
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let local_base = PathBuf::from("target/tmp").join(format!("scp-put-{unique}"));
    std::fs::create_dir_all(&local_base).expect("create local base");

    let local_file = local_base.join("hello.txt");
    std::fs::write(&local_file, "hello via scp\n").expect("write local file");

    let local_path_param = local_file.to_string_lossy().to_string();
    let remote_file = format!("{}/scp-hello.txt", remote_home);

    let resp = server
        .test_transfer(TransferParams {
            operation: TransferOperation::Put,
            local_path: local_path_param,
            remote_path: remote_file.clone(),
            transport: TransferTransport::Scp,
            kind: Some(TransferKind::File),
            overwrite: true,
            timeout_ms: Some(30000),
            verbose: false,
            ..Default::default()
        })
        .await;

    assert!(resp.ok, "scp file PUT should succeed: {:?}", resp.error);
    assert_eq!(
        resp.transport_used,
        TransferTransport::Scp,
        "should use Scp transport"
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
    assert!(verify_text.contains("hello via scp"));

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(&local_base);
}

#[tokio::test]
async fn test_scp_file_get_with_key_auth() {
    init_test_env().expect("Failed to initialize test environment");

    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("ssh_mcp=debug,info")
        .try_init();

    if !check_scp() {
        tracing::warn!("skipping: local 'scp' unavailable");
        return;
    }

    let (_key_dir, key_path) = setup_test_key();

    // Start SSH container configured for key auth
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

    let remote_file = format!("{}/scp-get-test.txt", remote_home);

    // Create file on remote
    let _ = server
        .test_execute_command(&format!(
            "sh -c 'echo \"scp file content from remote\" > {}'",
            ssh_mcp::escape_for_shell(&remote_file)
        ))
        .await
        .expect("create remote file");

    // Create local download directory
    let unique = format!(
        "{}-{}-scp-get",
        std::process::id(),
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let local_base = PathBuf::from("target/tmp").join(format!("scp-get-{unique}"));
    std::fs::create_dir_all(&local_base).expect("create local base");

    let local_download = local_base.join("downloaded.txt");
    let local_path_param = local_download.to_string_lossy().to_string();

    let resp = server
        .test_transfer(TransferParams {
            operation: TransferOperation::Get,
            local_path: local_path_param,
            remote_path: remote_file.clone(),
            transport: TransferTransport::Scp,
            kind: Some(TransferKind::File),
            overwrite: true,
            timeout_ms: Some(30000),
            verbose: false,
            ..Default::default()
        })
        .await;

    assert!(resp.ok, "scp file GET should succeed: {:?}", resp.error);
    assert_eq!(
        resp.transport_used,
        TransferTransport::Scp,
        "should use Scp transport"
    );

    // Verify local file was created and has correct content
    assert!(local_download.exists(), "local file should exist after GET");
    let content = std::fs::read_to_string(&local_download).expect("read local file");
    assert!(content.contains("scp file content from remote"));

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(&local_base);
}

#[tokio::test]
async fn test_scp_directory_put_with_key_auth() {
    init_test_env().expect("Failed to initialize test environment");

    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("ssh_mcp=debug,info")
        .try_init();

    if !check_scp() {
        tracing::warn!("skipping: local 'scp' unavailable");
        return;
    }

    let (_key_dir, key_path) = setup_test_key();

    // Start SSH container configured for key auth
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

    // Create local directory tree
    let unique = format!(
        "{}-{}-scp-dirput",
        std::process::id(),
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let local_base = PathBuf::from("target/tmp").join(format!("scp-dirput-{unique}"));
    std::fs::create_dir_all(&local_base).expect("create local base");

    let local_dir = local_base.join("upload_dir");
    std::fs::create_dir_all(local_dir.join("subdir")).expect("create local subdir");
    std::fs::write(
        local_dir.join("subdir").join("file.txt"),
        "nested scp content\n",
    )
    .expect("write nested file");
    std::fs::write(local_dir.join("top.txt"), "top level scp\n").expect("write top file");

    let local_dir_param = local_dir.to_string_lossy().to_string();
    let remote_dir = format!("{}/scp_received_dir", remote_home);

    let resp = server
        .test_transfer(TransferParams {
            operation: TransferOperation::Put,
            local_path: local_dir_param,
            remote_path: remote_dir.clone(),
            transport: TransferTransport::Scp,
            kind: Some(TransferKind::Directory),
            overwrite: true,
            timeout_ms: Some(30000),
            verbose: false,
            ..Default::default()
        })
        .await;

    assert!(
        resp.ok,
        "scp directory PUT should succeed: {:?}",
        resp.error
    );
    assert_eq!(
        resp.transport_used,
        TransferTransport::Scp,
        "should use Scp transport"
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
    assert!(extract_text_from_result(&verify).contains("ok"));

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(&local_base);
}

#[tokio::test]
async fn test_scp_directory_get_with_key_auth() {
    init_test_env().expect("Failed to initialize test environment");

    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("ssh_mcp=debug,info")
        .try_init();

    if !check_scp() {
        tracing::warn!("skipping: local 'scp' unavailable");
        return;
    }

    let (_key_dir, key_path) = setup_test_key();

    // Start SSH container configured for key auth
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

    let remote_dir = format!("{}/scp_source_dir", remote_home);

    // Create remote directory tree
    let _ = server
        .test_execute_command(&format!(
            "sh -c 'mkdir -p {}/subdir && echo \"scp nested\" > {}/subdir/nested.txt && echo \"scp top\" > {}/top.txt'",
            ssh_mcp::escape_for_shell(&remote_dir),
            ssh_mcp::escape_for_shell(&remote_dir),
            ssh_mcp::escape_for_shell(&remote_dir)
        ))
        .await
        .expect("create remote dir structure");

    // Create local download directory
    let unique = format!(
        "{}-{}-scp-dirget",
        std::process::id(),
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let local_base = PathBuf::from("target/tmp").join(format!("scp-dirget-{unique}"));
    std::fs::create_dir_all(&local_base).expect("create local base");

    let local_download_dir = local_base.join("downloaded_dir");
    let local_path_param = local_download_dir.to_string_lossy().to_string();

    let resp = server
        .test_transfer(TransferParams {
            operation: TransferOperation::Get,
            local_path: local_path_param,
            remote_path: remote_dir.clone(),
            transport: TransferTransport::Scp,
            kind: Some(TransferKind::Directory),
            overwrite: true,
            timeout_ms: Some(30000),
            verbose: false,
            ..Default::default()
        })
        .await;

    assert!(
        resp.ok,
        "scp directory GET should succeed: {:?}",
        resp.error
    );
    assert_eq!(
        resp.transport_used,
        TransferTransport::Scp,
        "should use Scp transport"
    );

    // Verify local directory structure
    assert!(
        local_download_dir.exists(),
        "local directory should exist after GET"
    );
    assert!(
        local_download_dir
            .join("subdir")
            .join("nested.txt")
            .exists(),
        "nested file should exist"
    );
    assert!(
        local_download_dir.join("top.txt").exists(),
        "top file should exist"
    );

    // Verify content
    let nested_content =
        std::fs::read_to_string(local_download_dir.join("subdir").join("nested.txt"))
            .expect("read nested file");
    assert!(nested_content.contains("scp nested"));

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(&local_base);
}

#[tokio::test]
async fn test_scp_overwrite_false() {
    init_test_env().expect("Failed to initialize test environment");

    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("ssh_mcp=debug,info")
        .try_init();

    if !check_scp() {
        tracing::warn!("skipping: local 'scp' unavailable");
        return;
    }

    let (_key_dir, key_path) = setup_test_key();

    // Start SSH container configured for key auth
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

    // Create local file
    let unique = format!(
        "{}-{}-scp-overwrite",
        std::process::id(),
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let local_base = PathBuf::from("target/tmp").join(format!("scp-overwrite-{unique}"));
    std::fs::create_dir_all(&local_base).expect("create local base");

    let local_file = local_base.join("hello.txt");
    std::fs::write(&local_file, "original content\n").expect("write local file");

    let local_path_param = local_file.to_string_lossy().to_string();
    let remote_file = format!("{}/scp-overwrite-test.txt", remote_home);

    // First upload with overwrite: true should succeed
    let resp1 = server
        .test_transfer(TransferParams {
            operation: TransferOperation::Put,
            local_path: local_path_param.clone(),
            remote_path: remote_file.clone(),
            transport: TransferTransport::Scp,
            kind: Some(TransferKind::File),
            overwrite: true,
            timeout_ms: Some(30000),
            verbose: false,
            ..Default::default()
        })
        .await;

    assert!(
        resp1.ok,
        "first scp file PUT should succeed: {:?}",
        resp1.error
    );
    assert_eq!(
        resp1.transport_used,
        TransferTransport::Scp,
        "should use Scp transport"
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
    assert!(verify_text.contains("original content"));

    // Update local file with new content
    std::fs::write(&local_file, "new content\n").expect("write updated local file");

    // Second upload with overwrite: false should fail
    let resp2 = server
        .test_transfer(TransferParams {
            operation: TransferOperation::Put,
            local_path: local_path_param.clone(),
            remote_path: remote_file.clone(),
            transport: TransferTransport::Scp,
            kind: Some(TransferKind::File),
            overwrite: false,
            timeout_ms: Some(30000),
            verbose: false,
            ..Default::default()
        })
        .await;

    assert!(
        !resp2.ok,
        "second scp file PUT with overwrite=false should fail: {:?}",
        resp2.error
    );

    // Verify remote file still has original content
    let verify2 = server
        .test_execute_command(&format!(
            "sh -c 'cat < {}'",
            ssh_mcp::escape_for_shell(&remote_file)
        ))
        .await
        .expect("verify remote file after failed overwrite");
    let verify2_text = extract_text_from_result(&verify2);
    assert!(
        verify2_text.contains("original content"),
        "remote file should still have original content after failed overwrite attempt"
    );

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(&local_base);
}
