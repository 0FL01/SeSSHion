//! Overwrite behavior tests for all transport types
#![cfg(unix)]

use super::common::*;
use std::path::PathBuf;
use std::time::SystemTime;

#[tokio::test]
async fn test_overwrite_false_prevents_overwrite_execraw() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("ssh_mcp=debug,info")
        .try_init();

    // Start SSH container with password auth
    let container = GenericImage::new("lscr.io/linuxserver/openssh-server", "latest")
        .with_env_var("USER_NAME", "test")
        .with_env_var("PASSWORD_ACCESS", "true")
        .with_env_var("USER_PASSWORD", "secret")
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

    // Create local file with original content
    let unique = format!(
        "{}-{}-execraw-overwrite",
        std::process::id(),
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let local_base = PathBuf::from("target/tmp").join(format!("execraw-overwrite-{unique}"));
    std::fs::create_dir_all(&local_base).expect("create local base");

    let local_file = local_base.join("test.txt");
    std::fs::write(&local_file, "original execraw content\n").expect("write local file");

    let local_path_param = local_file.to_string_lossy().to_string();
    let remote_file = format!("{}/execraw-overwrite-test.txt", remote_home);

    // First upload with overwrite: true should succeed
    let resp1 = server
        .test_transfer(TransferParams {
            operation: TransferOperation::Put,
            local_path: local_path_param.clone(),
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
        resp1.ok,
        "first ExecRaw file PUT should succeed: {:?}",
        resp1.error
    );
    assert_eq!(
        resp1.transport_used,
        TransferTransport::ExecRaw,
        "should use ExecRaw transport"
    );

    // Verify original content on remote
    let verify1 = server
        .test_execute_command(&format!(
            "sh -c 'cat < {}'",
            ssh_mcp::escape_for_shell(&remote_file)
        ))
        .await
        .expect("verify remote file");
    let verify1_text = extract_text_from_result(&verify1);
    assert!(
        verify1_text.contains("original execraw content"),
        "remote file should have original content"
    );

    // Update local file with new content
    std::fs::write(&local_file, "new execraw content\n").expect("write updated local file");

    // Second upload with overwrite: false should fail
    let resp2 = server
        .test_transfer(TransferParams {
            operation: TransferOperation::Put,
            local_path: local_path_param.clone(),
            remote_path: remote_file.clone(),
            transport: TransferTransport::ExecRaw,
            kind: Some(TransferKind::File),
            overwrite: false,
            timeout_ms: Some(30000),
            verbose: false,
            ..Default::default()
        })
        .await;

    assert!(
        !resp2.ok,
        "second ExecRaw file PUT with overwrite=false should fail: {:?}",
        resp2.error
    );

    // Verify remote file still has original content (not overwritten)
    let verify2 = server
        .test_execute_command(&format!(
            "sh -c 'cat < {}'",
            ssh_mcp::escape_for_shell(&remote_file)
        ))
        .await
        .expect("verify remote file after failed overwrite");
    let verify2_text = extract_text_from_result(&verify2);
    assert!(
        verify2_text.contains("original execraw content"),
        "remote file should still have original content after failed overwrite attempt"
    );
    assert!(
        !verify2_text.contains("new execraw content"),
        "remote file should NOT have new content"
    );

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(&local_base);
}

#[tokio::test]
async fn test_overwrite_false_prevents_overwrite_sftp() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("ssh_mcp=debug,info")
        .try_init();

    if !check_sftp() {
        tracing::warn!("skipping: local 'sftp' unavailable");
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

    // Create local file with original content
    let unique = format!(
        "{}-{}-sftp-overwrite",
        std::process::id(),
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let local_base = PathBuf::from("target/tmp").join(format!("sftp-overwrite-{unique}"));
    std::fs::create_dir_all(&local_base).expect("create local base");

    let local_file = local_base.join("test.txt");
    std::fs::write(&local_file, "original sftp content\n").expect("write local file");

    let local_path_param = local_file.to_string_lossy().to_string();
    let remote_file = format!("{}/sftp-overwrite-test.txt", remote_home);

    // First upload with overwrite: true should succeed
    let resp1 = server
        .test_transfer(TransferParams {
            operation: TransferOperation::Put,
            local_path: local_path_param.clone(),
            remote_path: remote_file.clone(),
            transport: TransferTransport::Sftp,
            kind: Some(TransferKind::File),
            overwrite: true,
            timeout_ms: Some(30000),
            verbose: false,
            ..Default::default()
        })
        .await;

    assert!(
        resp1.ok,
        "first SFTP file PUT should succeed: {:?}",
        resp1.error
    );
    assert_eq!(
        resp1.transport_used,
        TransferTransport::Sftp,
        "should use Sftp transport"
    );

    // Verify original content on remote
    let verify1 = server
        .test_execute_command(&format!(
            "sh -c 'cat < {}'",
            ssh_mcp::escape_for_shell(&remote_file)
        ))
        .await
        .expect("verify remote file");
    let verify1_text = extract_text_from_result(&verify1);
    assert!(
        verify1_text.contains("original sftp content"),
        "remote file should have original content"
    );

    // Update local file with new content
    std::fs::write(&local_file, "new sftp content\n").expect("write updated local file");

    // Second upload with overwrite: false should fail
    let resp2 = server
        .test_transfer(TransferParams {
            operation: TransferOperation::Put,
            local_path: local_path_param.clone(),
            remote_path: remote_file.clone(),
            transport: TransferTransport::Sftp,
            kind: Some(TransferKind::File),
            overwrite: false,
            timeout_ms: Some(30000),
            verbose: false,
            ..Default::default()
        })
        .await;

    assert!(
        !resp2.ok,
        "second SFTP file PUT with overwrite=false should fail: {:?}",
        resp2.error
    );

    // Verify remote file still has original content (not overwritten)
    let verify2 = server
        .test_execute_command(&format!(
            "sh -c 'cat < {}'",
            ssh_mcp::escape_for_shell(&remote_file)
        ))
        .await
        .expect("verify remote file after failed overwrite");
    let verify2_text = extract_text_from_result(&verify2);
    assert!(
        verify2_text.contains("original sftp content"),
        "remote file should still have original content after failed overwrite attempt"
    );
    assert!(
        !verify2_text.contains("new sftp content"),
        "remote file should NOT have new content"
    );

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(&local_base);
}

#[tokio::test]
async fn test_overwrite_false_prevents_overwrite_scp() {
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

    // Create local file with original content
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

    let local_file = local_base.join("test.txt");
    std::fs::write(&local_file, "original scp content\n").expect("write local file");

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
        "first SCP file PUT should succeed: {:?}",
        resp1.error
    );
    assert_eq!(
        resp1.transport_used,
        TransferTransport::Scp,
        "should use Scp transport"
    );

    // Verify original content on remote
    let verify1 = server
        .test_execute_command(&format!(
            "sh -c 'cat < {}'",
            ssh_mcp::escape_for_shell(&remote_file)
        ))
        .await
        .expect("verify remote file");
    let verify1_text = extract_text_from_result(&verify1);
    assert!(
        verify1_text.contains("original scp content"),
        "remote file should have original content"
    );

    // Update local file with new content
    std::fs::write(&local_file, "new scp content\n").expect("write updated local file");

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
        "second SCP file PUT with overwrite=false should fail: {:?}",
        resp2.error
    );

    // Verify remote file still has original content (not overwritten)
    let verify2 = server
        .test_execute_command(&format!(
            "sh -c 'cat < {}'",
            ssh_mcp::escape_for_shell(&remote_file)
        ))
        .await
        .expect("verify remote file after failed overwrite");
    let verify2_text = extract_text_from_result(&verify2);
    assert!(
        verify2_text.contains("original scp content"),
        "remote file should still have original content after failed overwrite attempt"
    );
    assert!(
        !verify2_text.contains("new scp content"),
        "remote file should NOT have new content"
    );

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(&local_base);
}

#[tokio::test]
async fn test_overwrite_false_prevents_overwrite_rsync() {
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

    // Create local file with original content
    let unique = format!(
        "{}-{}-rsync-overwrite",
        std::process::id(),
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let local_base = PathBuf::from("target/tmp").join(format!("rsync-overwrite-{unique}"));
    std::fs::create_dir_all(&local_base).expect("create local base");

    let local_file = local_base.join("test.txt");
    std::fs::write(&local_file, "original rsync content\n").expect("write local file");

    let local_path_param = local_file.to_string_lossy().to_string();
    let remote_file = format!("{}/rsync-overwrite-test.txt", remote_home);

    // First upload with overwrite: true should succeed
    let resp1 = server
        .test_transfer(TransferParams {
            operation: TransferOperation::Put,
            local_path: local_path_param.clone(),
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
        resp1.ok,
        "first Rsync file PUT should succeed: {:?}",
        resp1.error
    );
    assert_eq!(
        resp1.transport_used,
        TransferTransport::Rsync,
        "should use Rsync transport"
    );

    // Verify original content on remote
    let verify1 = server
        .test_execute_command(&format!(
            "sh -c 'cat < {}'",
            ssh_mcp::escape_for_shell(&remote_file)
        ))
        .await
        .expect("verify remote file");
    let verify1_text = extract_text_from_result(&verify1);
    assert!(
        verify1_text.contains("original rsync content"),
        "remote file should have original content"
    );

    // Update local file with new content
    std::fs::write(&local_file, "new rsync content\n").expect("write updated local file");

    // Second upload with overwrite: false should fail
    let resp2 = server
        .test_transfer(TransferParams {
            operation: TransferOperation::Put,
            local_path: local_path_param.clone(),
            remote_path: remote_file.clone(),
            transport: TransferTransport::Rsync,
            kind: Some(TransferKind::File),
            overwrite: false,
            timeout_ms: Some(30000),
            verbose: false,
            ..Default::default()
        })
        .await;

    assert!(
        !resp2.ok,
        "second Rsync file PUT with overwrite=false should fail: {:?}",
        resp2.error
    );

    // Verify remote file still has original content (not overwritten)
    let verify2 = server
        .test_execute_command(&format!(
            "sh -c 'cat < {}'",
            ssh_mcp::escape_for_shell(&remote_file)
        ))
        .await
        .expect("verify remote file after failed overwrite");
    let verify2_text = extract_text_from_result(&verify2);
    assert!(
        verify2_text.contains("original rsync content"),
        "remote file should still have original content after failed overwrite attempt"
    );
    assert!(
        !verify2_text.contains("new rsync content"),
        "remote file should NOT have new content"
    );

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(&local_base);
}

#[tokio::test]
async fn test_overwrite_false_then_true_succeeds() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("ssh_mcp=debug,info")
        .try_init();

    // Start SSH container with password auth (using ExecRaw for simplicity)
    let container = GenericImage::new("lscr.io/linuxserver/openssh-server", "latest")
        .with_env_var("USER_NAME", "test")
        .with_env_var("PASSWORD_ACCESS", "true")
        .with_env_var("USER_PASSWORD", "secret")
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

    // Create local file with original content
    let unique = format!(
        "{}-{}-overwrite-sequence",
        std::process::id(),
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let local_base = PathBuf::from("target/tmp").join(format!("overwrite-sequence-{unique}"));
    std::fs::create_dir_all(&local_base).expect("create local base");

    let local_file = local_base.join("test.txt");
    std::fs::write(&local_file, "first version content\n").expect("write local file");

    let local_path_param = local_file.to_string_lossy().to_string();
    let remote_file = format!("{}/overwrite-seq-test.txt", remote_home);

    // Step 1: Upload with overwrite: true (initial upload)
    let resp1 = server
        .test_transfer(TransferParams {
            operation: TransferOperation::Put,
            local_path: local_path_param.clone(),
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
        resp1.ok,
        "initial file PUT should succeed: {:?}",
        resp1.error
    );

    // Verify first version content
    let verify1 = server
        .test_execute_command(&format!(
            "sh -c 'cat < {}'",
            ssh_mcp::escape_for_shell(&remote_file)
        ))
        .await
        .expect("verify remote file");
    let verify1_text = extract_text_from_result(&verify1);
    assert!(
        verify1_text.contains("first version content"),
        "remote file should have first version content"
    );

    // Step 2: Update local file and try to upload with overwrite: false (should fail)
    std::fs::write(&local_file, "second version content\n").expect("write second version");

    let resp2 = server
        .test_transfer(TransferParams {
            operation: TransferOperation::Put,
            local_path: local_path_param.clone(),
            remote_path: remote_file.clone(),
            transport: TransferTransport::ExecRaw,
            kind: Some(TransferKind::File),
            overwrite: false,
            timeout_ms: Some(30000),
            verbose: false,
            ..Default::default()
        })
        .await;

    assert!(
        !resp2.ok,
        "second PUT with overwrite=false should fail: {:?}",
        resp2.error
    );

    // Verify first version is still on remote (not overwritten)
    let verify2 = server
        .test_execute_command(&format!(
            "sh -c 'cat < {}'",
            ssh_mcp::escape_for_shell(&remote_file)
        ))
        .await
        .expect("verify remote file after failed overwrite");
    let verify2_text = extract_text_from_result(&verify2);
    assert!(
        verify2_text.contains("first version content"),
        "remote file should still have first version after failed overwrite"
    );
    assert!(
        !verify2_text.contains("second version content"),
        "remote file should NOT have second version yet"
    );

    // Step 3: Update local file again and upload with overwrite: true (should succeed)
    std::fs::write(&local_file, "third version content\n").expect("write third version");

    let resp3 = server
        .test_transfer(TransferParams {
            operation: TransferOperation::Put,
            local_path: local_path_param.clone(),
            remote_path: remote_file.clone(),
            transport: TransferTransport::ExecRaw,
            kind: Some(TransferKind::File),
            overwrite: true, // Now allowing overwrite
            timeout_ms: Some(30000),
            verbose: false,
            ..Default::default()
        })
        .await;

    assert!(
        resp3.ok,
        "third PUT with overwrite=true should succeed: {:?}",
        resp3.error
    );

    // Verify third version is now on remote
    let verify3 = server
        .test_execute_command(&format!(
            "sh -c 'cat < {}'",
            ssh_mcp::escape_for_shell(&remote_file)
        ))
        .await
        .expect("verify remote file after successful overwrite");
    let verify3_text = extract_text_from_result(&verify3);
    assert!(
        verify3_text.contains("third version content"),
        "remote file should have third version after successful overwrite"
    );
    assert!(
        !verify3_text.contains("first version content"),
        "remote file should NOT have first version anymore"
    );

    server.shutdown().await;
    let _ = std::fs::remove_dir_all(&local_base);
}
