//! Reconnect behavior tests after transient container disruption.

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
        timeout_ms: 15_000,
        max_chars: Some(1000),
        max_output_tokens: Some(12_000),
        disable_sudo: true,
        keepalive_interval: 30,
        keepalive_max: 3,
        reconnect_retries: 4,
        reconnect_backoff_ms: 200,
        health_probe_timeout_ms: 400,
    }
}

async fn wait_until_recovered(server: &SshMcpServer, deadline: Instant) -> String {
    let mut last_output = String::new();

    while Instant::now() < deadline {
        let result = server
            .test_execute_command_with_timeout_ms("echo RECOVERED", 4_000)
            .await;

        if let Ok(call_result) = result {
            let text = extract_text_from_result(&call_result);
            if text.contains("RECOVERED") && !text.contains("SSH connection error") {
                return text;
            }
            last_output = text;
        }

        tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;
    }

    panic!("server did not recover within deadline; last output: {last_output}");
}

/// Simulates a short VPN-like network disruption with container pause/unpause.
/// Verifies that the same SshMcpServer instance recovers automatically.
#[tokio::test]
async fn test_reconnect_after_container_pause_unpause() {
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
    let host_string = host.to_string();
    let port = container
        .get_host_port_ipv4(2222)
        .await
        .expect("Failed to get mapped SSH port");

    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

    let server = SshMcpServer::new(docker_test_config(&host_string, port))
        .await
        .expect("Failed to create SshMcpServer");

    let warmup = server
        .test_execute_command("echo warmup")
        .await
        .expect("Warmup command failed");
    let warmup_text = extract_text_from_result(&warmup);
    assert!(
        warmup_text.contains("warmup"),
        "Warmup command should succeed, got: {warmup_text}"
    );

    container.pause().await.expect("Failed to pause container");
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

    let during_disruption = tokio::time::timeout(
        tokio::time::Duration::from_millis(800),
        server.test_execute_command_with_timeout_ms("echo after-pause", 4_000),
    )
    .await;
    match during_disruption {
        Err(_) => {
            tracing::info!("Paused-state check: command attempt stalled as expected");
        }
        Ok(Err(err)) => {
            tracing::info!(error = ?err, "Paused-state check: command returned transport error");
        }
        Ok(Ok(result)) => {
            let text = extract_text_from_result(&result);
            assert!(
                result.is_error.unwrap_or(false),
                "Expected timeout or explicit command error while paused; got successful output: {text}"
            );
            tracing::info!(
                output = %text,
                "Paused-state check: command returned explicit tool error"
            );
        }
    }

    container
        .unpause()
        .await
        .expect("Failed to unpause container");
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    let recovered_text =
        wait_until_recovered(&server, Instant::now() + std::time::Duration::from_secs(15)).await;
    assert!(
        recovered_text.contains("RECOVERED"),
        "Expected recovered output from same server instance, got: {recovered_text}"
    );

    server.shutdown().await;
}
