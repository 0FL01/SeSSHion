//! Rsync transfer tests using SSH key authentication
#![cfg(unix)]

use super::common::*;
use std::path::PathBuf;
use std::time::SystemTime;

#[tokio::test]
async fn test_rsync_file_put_with_key_auth() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("ssh_mcp=debug,info")
        .try_init();

    if !check_rsync() {
        tracing::warn!("skipping: local 'rsync' unavailable");
        return;
    }

    let (_key_dir, key_path) = setup_test_key();

    // Start SSH container configured for key auth
    let container = GenericImage::new("lscr.io/linuxserver/openssh-server", "latest")
        .with_env_var("USER_NAME", "test")
        .with_env_var("PASSWORD_ACCESS", "false")
        .with_env_var("PUBLIC_KEY", TEST_PUBLIC_KEY)
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
        "{}-{}-rsync-put",
        std::process::id(),
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let local_base = PathBuf::from("target/tmp").join(format!("rsync-put-{unique}"));
    std::fs::create_dir_all(&local_base).expect("create local base");

    let local_file = local_base.join("hello.txt");
    std::fs::write(&local_file, "hello via rsync\n").expect("write local file");

    let local_path_param = local_file.to_string_lossy().to_string();
    let remote_file = format!("{}/rsync-hello.txt", remote_home);

    let resp = server
        .test_transfer(TransferParams {
            operation: TransferOperation::Put,
            local_path: local_path_param,
            remote_path: remote_file.clone(),
            transport: TransferTransport::Rsync,
            kind: Some(TransferKind::File),
            overwrite: true,
            timeout_ms: Some(30000),
            verbose: false,
            ..Default::default()
        })
        .await;

    assert!(resp.ok, "rsync file PUT should succeed: {:?}", resp.error);
    assert_eq!(
        resp.transport_used,
        TransferTransport::Rsync,
        "should use Rsync transport"
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
    assert!(verify_text.contains("hello via rsync"));

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(&local_base);
}

#[tokio::test]
async fn test_rsync_file_get_with_key_auth() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("ssh_mcp=debug,info")
        .try_init();

    if !check_rsync() {
        tracing::warn!("skipping: local 'rsync' unavailable");
        return;
    }

    let (_key_dir, key_path) = setup_test_key();

    // Start SSH container configured for key auth
    let container = GenericImage::new("lscr.io/linuxserver/openssh-server", "latest")
        .with_env_var("USER_NAME", "test")
        .with_env_var("PASSWORD_ACCESS", "false")
        .with_env_var("PUBLIC_KEY", TEST_PUBLIC_KEY)
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

    let remote_file = format!("{}/rsync-get-test.txt", remote_home);

    // Create file on remote
    let _ = server
        .test_execute_command(&format!(
            "sh -c 'echo \"rsync file content from remote\" > {}'",
            ssh_mcp::escape_for_shell(&remote_file)
        ))
        .await
        .expect("create remote file");

    // Create local download directory
    let unique = format!(
        "{}-{}-rsync-get",
        std::process::id(),
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let local_base = PathBuf::from("target/tmp").join(format!("rsync-get-{unique}"));
    std::fs::create_dir_all(&local_base).expect("create local base");

    let local_download = local_base.join("downloaded.txt");
    let local_path_param = local_download.to_string_lossy().to_string();

    let resp = server
        .test_transfer(TransferParams {
            operation: TransferOperation::Get,
            local_path: local_path_param,
            remote_path: remote_file.clone(),
            transport: TransferTransport::Rsync,
            kind: Some(TransferKind::File),
            overwrite: true,
            timeout_ms: Some(30000),
            verbose: false,
            ..Default::default()
        })
        .await;

    assert!(resp.ok, "rsync file GET should succeed: {:?}", resp.error);
    assert_eq!(
        resp.transport_used,
        TransferTransport::Rsync,
        "should use Rsync transport"
    );

    // Verify local file was created and has correct content
    assert!(local_download.exists(), "local file should exist after GET");
    let content = std::fs::read_to_string(&local_download).expect("read local file");
    assert!(content.contains("rsync file content from remote"));

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(&local_base);
}

#[tokio::test]
async fn test_rsync_directory_put_with_key_auth() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("ssh_mcp=debug,info")
        .try_init();

    if !check_rsync() {
        tracing::warn!("skipping: local 'rsync' unavailable");
        return;
    }

    let (_key_dir, key_path) = setup_test_key();

    // Start SSH container configured for key auth
    let container = GenericImage::new("lscr.io/linuxserver/openssh-server", "latest")
        .with_env_var("USER_NAME", "test")
        .with_env_var("PASSWORD_ACCESS", "false")
        .with_env_var("PUBLIC_KEY", TEST_PUBLIC_KEY)
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
        "{}-{}-rsync-dirput",
        std::process::id(),
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let local_base = PathBuf::from("target/tmp").join(format!("rsync-dirput-{unique}"));
    std::fs::create_dir_all(&local_base).expect("create local base");

    let local_dir = local_base.join("upload_dir");
    std::fs::create_dir_all(local_dir.join("subdir")).expect("create local subdir");
    std::fs::write(
        local_dir.join("subdir").join("file.txt"),
        "nested rsync content\n",
    )
    .expect("write nested file");
    std::fs::write(local_dir.join("top.txt"), "top level rsync\n").expect("write top file");

    let local_dir_param = local_dir.to_string_lossy().to_string();
    let remote_dir = format!("{}/rsync_received_dir", remote_home);

    let resp = server
        .test_transfer(TransferParams {
            operation: TransferOperation::Put,
            local_path: local_dir_param,
            remote_path: remote_dir.clone(),
            transport: TransferTransport::Rsync,
            kind: Some(TransferKind::Directory),
            overwrite: true,
            timeout_ms: Some(30000),
            verbose: false,
            ..Default::default()
        })
        .await;

    assert!(
        resp.ok,
        "rsync directory PUT should succeed: {:?}",
        resp.error
    );
    assert_eq!(
        resp.transport_used,
        TransferTransport::Rsync,
        "should use Rsync transport"
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
async fn test_rsync_directory_get_with_key_auth() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("ssh_mcp=debug,info")
        .try_init();

    if !check_rsync() {
        tracing::warn!("skipping: local 'rsync' unavailable");
        return;
    }

    let (_key_dir, key_path) = setup_test_key();

    // Start SSH container configured for key auth
    let container = GenericImage::new("lscr.io/linuxserver/openssh-server", "latest")
        .with_env_var("USER_NAME", "test")
        .with_env_var("PASSWORD_ACCESS", "false")
        .with_env_var("PUBLIC_KEY", TEST_PUBLIC_KEY)
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

    let remote_dir = format!("{}/rsync_source_dir", remote_home);

    // Create remote directory tree
    let _ = server
        .test_execute_command(&format!(
            "sh -c 'mkdir -p {}/subdir && echo \"rsync nested\" > {}/subdir/nested.txt && echo \"rsync top\" > {}/top.txt'",
            ssh_mcp::escape_for_shell(&remote_dir),
            ssh_mcp::escape_for_shell(&remote_dir),
            ssh_mcp::escape_for_shell(&remote_dir)
        ))
        .await
        .expect("create remote dir structure");

    // Create local download directory
    let unique = format!(
        "{}-{}-rsync-dirget",
        std::process::id(),
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let local_base = PathBuf::from("target/tmp").join(format!("rsync-dirget-{unique}"));
    std::fs::create_dir_all(&local_base).expect("create local base");

    let local_download_dir = local_base.join("downloaded_dir");
    let local_path_param = local_download_dir.to_string_lossy().to_string();

    let resp = server
        .test_transfer(TransferParams {
            operation: TransferOperation::Get,
            local_path: local_path_param,
            remote_path: remote_dir.clone(),
            transport: TransferTransport::Rsync,
            kind: Some(TransferKind::Directory),
            overwrite: true,
            timeout_ms: Some(30000),
            verbose: false,
            ..Default::default()
        })
        .await;

    assert!(
        resp.ok,
        "rsync directory GET should succeed: {:?}",
        resp.error
    );
    assert_eq!(
        resp.transport_used,
        TransferTransport::Rsync,
        "should use Rsync transport"
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
    assert!(nested_content.contains("rsync nested"));

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(&local_base);
}

#[tokio::test]
async fn test_auto_transport_prefers_rsync() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("ssh_mcp=debug,info")
        .try_init();

    if !check_rsync() {
        tracing::warn!("skipping: local 'rsync' unavailable");
        return;
    }

    let (_key_dir, key_path) = setup_test_key();

    // Start SSH container configured for key auth
    let container = GenericImage::new("lscr.io/linuxserver/openssh-server", "latest")
        .with_env_var("USER_NAME", "test")
        .with_env_var("PASSWORD_ACCESS", "false")
        .with_env_var("PUBLIC_KEY", TEST_PUBLIC_KEY)
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
        "{}-{}-rsync-auto",
        std::process::id(),
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let local_base = PathBuf::from("target/tmp").join(format!("rsync-auto-{unique}"));
    std::fs::create_dir_all(&local_base).expect("create local base");

    let local_file = local_base.join("auto-test.txt");
    std::fs::write(&local_file, "auto transport test via rsync\n").expect("write local file");

    let local_path_param = local_file.to_string_lossy().to_string();
    let remote_file = format!("{}/auto-transport-test.txt", remote_home);

    let resp = server
        .test_transfer(TransferParams {
            operation: TransferOperation::Put,
            local_path: local_path_param,
            remote_path: remote_file.clone(),
            transport: TransferTransport::Auto,
            kind: Some(TransferKind::File),
            overwrite: true,
            timeout_ms: Some(30000),
            verbose: false,
            ..Default::default()
        })
        .await;

    assert!(
        resp.ok,
        "auto transport file PUT should succeed: {:?}",
        resp.error
    );
    assert_eq!(
        resp.transport_used,
        TransferTransport::Rsync,
        "auto should prefer Rsync transport when available"
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
    assert!(verify_text.contains("auto transport test via rsync"));

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(&local_base);
}

#[tokio::test]
async fn test_rsync_with_options() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("ssh_mcp=debug,info")
        .try_init();

    if !check_rsync() {
        tracing::warn!("skipping: local 'rsync' unavailable");
        return;
    }

    let (_key_dir, key_path) = setup_test_key();

    // Start SSH container configured for key auth
    let container = GenericImage::new("lscr.io/linuxserver/openssh-server", "latest")
        .with_env_var("USER_NAME", "test")
        .with_env_var("PASSWORD_ACCESS", "false")
        .with_env_var("PUBLIC_KEY", TEST_PUBLIC_KEY)
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
        "{}-{}-rsync-opts",
        std::process::id(),
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let local_base = PathBuf::from("target/tmp").join(format!("rsync-opts-{unique}"));
    std::fs::create_dir_all(&local_base).expect("create local base");

    let local_file = local_base.join("options-test.txt");
    std::fs::write(&local_file, "rsync with custom options\n").expect("write local file");

    let local_path_param = local_file.to_string_lossy().to_string();
    let remote_file = format!("{}/rsync-options-test.txt", remote_home);

    let resp = server
        .test_transfer(TransferParams {
            operation: TransferOperation::Put,
            local_path: local_path_param,
            remote_path: remote_file.clone(),
            transport: TransferTransport::Rsync,
            kind: Some(TransferKind::File),
            overwrite: true,
            timeout_ms: Some(30000),
            verbose: false,
            rsync_options: ssh_mcp::transfer::RsyncOptions {
                checksum: true,
                compress: true,
                delete: false,
                inplace: true,
                partial: true,
                bwlimit: Some(1024), // 1 MB/s limit
            },
        })
        .await;

    assert!(
        resp.ok,
        "rsync with options file PUT should succeed: {:?}",
        resp.error
    );
    assert_eq!(
        resp.transport_used,
        TransferTransport::Rsync,
        "should use Rsync transport"
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
    assert!(verify_text.contains("rsync with custom options"));

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(&local_base);
}
