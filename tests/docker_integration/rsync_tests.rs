//! Rsync transfer tests using SSH key authentication
#![cfg(unix)]

use super::common::*;
use std::path::PathBuf;
use std::time::SystemTime;
use testcontainers::core::ExecCommand;

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

    // Build the custom Debian SSH image if needed
    init_test_env().expect("Failed to build test image");

    // Start SSH container using custom Debian image with password auth
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
        jump: None,
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

    // Build the custom Debian SSH image if needed
    init_test_env().expect("Failed to build test image");

    // Start SSH container using custom Debian image with password auth
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
        jump: None,
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

    // Build the custom Debian SSH image if needed
    init_test_env().expect("Failed to build test image");

    // Start SSH container using custom Debian image with password auth
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
        jump: None,
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

    // Build the custom Debian SSH image if needed
    init_test_env().expect("Failed to build test image");

    // Start SSH container using custom Debian image with password auth
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
        jump: None,
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

    // Resolve remote home and create a file
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
async fn test_explicit_rsync_missing_remote_binary_is_terminal() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("ssh_mcp=debug,info")
        .try_init();

    if !check_rsync() {
        tracing::warn!("skipping: local 'rsync' unavailable");
        return;
    }

    // Reuse the standard SSH image and its existing key authentication setup.
    init_test_env().expect("Failed to build test image");

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

    // Docker exec controls only this test's container as root, outside the SSH user.
    let mut disable_rsync = container
        .exec(ExecCommand::new([
            "sh",
            "-c",
            "set -eu; test \"$(id -u)\" = 0; mv /usr/bin/rsync /usr/bin/rsync.disabled; ! command -v rsync",
        ]))
        .await
        .expect("disable rsync in the owned test container");
    let setup_stdout = disable_rsync
        .stdout_to_vec()
        .await
        .expect("read rsync fixture setup output");
    assert_eq!(
        disable_rsync
            .exit_code()
            .await
            .expect("rsync setup exit code"),
        Some(0),
        "rsync fixture setup failed: {}",
        String::from_utf8_lossy(&setup_stdout)
    );

    // Generate test key
    let (_key_dir, key_path) = setup_test_key();

    let config = Config {
        host: host.to_string(),
        port,
        user: "test".to_string(),
        password: None,
        key: Some(key_path),
        jump: None,
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

    // Create local file
    let unique = format!(
        "{}-{}-rsync-missing-remote",
        std::process::id(),
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let local_base = PathBuf::from("target/tmp").join(format!("rsync-missing-remote-{unique}"));
    std::fs::create_dir_all(&local_base).expect("create local base");

    let local_file = local_base.join("terminal-test.txt");
    std::fs::write(
        &local_file,
        "explicit rsync must not start another writer\n",
    )
    .expect("write local file");

    let local_path_param = local_file.to_string_lossy().to_string();
    let remote_file = format!("{remote_home}/rsync-missing-remote-{unique}.txt");

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

    assert!(
        !resp.ok,
        "explicit rsync must fail when the remote binary is missing: {:?}",
        resp.error
    );
    assert_eq!(
        resp.transport_used,
        TransferTransport::Rsync,
        "a failed explicit rsync request must retain its selected transport"
    );
    assert!(
        resp.error
            .as_deref()
            .is_some_and(|error| error.contains("rsync not found on remote host")),
        "unexpected missing-rsync error: {:?}",
        resp.error
    );

    // Neither SFTP nor exec-raw may write automatically after the rsync failure.
    let verify = server
        .test_execute_command(&format!(
            "test ! -e '{}' && test ! -L '{}' && printf absent",
            ssh_mcp::escape_for_shell(&remote_file),
            ssh_mcp::escape_for_shell(&remote_file)
        ))
        .await
        .expect("verify destination remains absent");
    assert_eq!(
        extract_text_from_result(&verify),
        "absent",
        "a terminal rsync failure must not create the destination"
    );

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
        jump: None,
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
            background: false,
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
