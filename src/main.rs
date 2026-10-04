//! SeSSHion - Entry point
//!
//! This is the main entry point for SeSSHion.
//! It parses CLI arguments, validates configuration, starts the MCP server
//! on stdio transport, and handles graceful shutdown.

use std::time::Duration;

use clap::Parser;
use rmcp::service::ServiceExt;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use ssh_mcp::config::{Args, Config};
use ssh_mcp::error::{Result, SshMcpError};
use ssh_mcp::logging::init_logging;
use ssh_mcp::server::SshMcpServer;

const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);
const STARTUP_READINESS_TIMEOUT: Duration = Duration::from_secs(10);
const SSH_RECOVERY_INTERVAL: Duration = Duration::from_secs(5);

fn main() -> Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    let result = runtime.block_on(run());
    // ponytail: Tokio stdin can outlive cancellation; use nonblocking stdio if
    // shutdown must wait for every runtime task instead of bounding the wait.
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
    result
}

async fn run() -> Result<()> {
    // Parse CLI arguments
    let mut args = Args::parse();

    // Initialize logging (JSON to file if specified, text to stderr)
    let _guard = init_logging(&args)?;

    let spool_dir = args.spool_dir.take();

    // Validate and create config
    let config = Config::from_args(args)?;

    info!("SeSSHion v{} starting...", env!("CARGO_PKG_VERSION"));
    info!(
        "Connecting to {}@{}:{}",
        config.user, config.host, config.port
    );
    if let Some(jump) = &config.jump {
        info!(
            "Routing through jump host {}@{}:{}",
            jump.user, jump.host, jump.port
        );
    }
    info!(
        "Timeout: {}ms, Max chars: {}",
        config.timeout_ms,
        config
            .max_chars
            .map_or("unlimited".to_string(), |n| n.to_string())
    );
    info!(
        "Keepalive: interval={}s, max_failures={}",
        config.keepalive_interval, config.keepalive_max
    );
    info!(
        "Host key checking: {:?}, known_hosts={}",
        config.strict_host_key_checking,
        config
            .known_hosts
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "default".to_string())
    );

    if config.disable_sudo {
        info!("sudo_shell and sudo_apply_patch tools are disabled");
    }

    // Create MCP server
    let server = SshMcpServer::new_with_spool_dir(config, spool_dir).await?;

    let lifecycle = CancellationToken::new();
    let signal_lifecycle = lifecycle.clone();

    // Signals stop MCP ingress first. SSH is closed after the service drains.
    let signal_handle = tokio::spawn(async move {
        // Wait for Ctrl+C or SIGTERM
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("Received SIGINT (Ctrl+C), shutting down...");
            }
            _ = async {
                #[cfg(unix)]
                {
                    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    {
                        Ok(mut sigterm) => {
                            sigterm.recv().await;
                        }
                        Err(e) => {
                            error!(error = ?e, "Failed to register SIGTERM handler");
                            std::future::pending::<()>().await;
                        }
                    }
                }
                #[cfg(not(unix))]
                {
                    std::future::pending::<()>().await;
                }
            } => {
                info!("Received SIGTERM, shutting down...");
            }
        }
        signal_lifecycle.cancel();
    });

    let server_for_shutdown = server.clone();
    let connection = server.connection().clone();
    let mut recovery_handle = None;
    // Keep all startup failures inside this block so signals and SSH always get
    // the same cleanup. Gate the transport itself, not only legacy initialize.
    let service_result = async {
        let readiness = tokio::select! {
            biased;
            _ = lifecycle.cancelled() => return Ok(()),
            result = tokio::time::timeout(
                STARTUP_READINESS_TIMEOUT,
                connection.ensure_connected_transport_only(),
            ) => result.map_err(|_| SshMcpError::connection(
                "Initial SSH readiness timed out after 10000ms",
            ))?,
        };
        if let Err(e) = readiness {
            error!(error = ?e, "Initial SSH readiness failed; refusing MCP startup");
            return Err(e);
        }

        // Readiness is mandatory; environment collection remains rootless,
        // optional and frozen, with its own three-second budget.
        let server = server.with_startup_environment(lifecycle.clone()).await;
        if lifecycle.is_cancelled() {
            return Ok(());
        }
        let recovery_lifecycle = lifecycle.clone();
        recovery_handle = Some(tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = recovery_lifecycle.cancelled() => break,
                    _ = tokio::time::sleep(SSH_RECOVERY_INTERVAL) => {}
                }
                tokio::select! {
                    biased;
                    _ = recovery_lifecycle.cancelled() => break,
                    result = connection.ensure_connected_transport_only() => {
                        if let Err(e) = result {
                            warn!(error = ?e, "SSH unavailable; retaining MCP and retrying in background");
                        }
                    }
                }
            }
        }));
        info!("SeSSHion running on stdio");
        match server
            .serve_with_ct(rmcp::transport::io::stdio(), lifecycle.clone())
            .await
        {
            Ok(running_server) => {
                info!("MCP server is serving...");
                if let Err(e) = running_server.waiting().await {
                    error!(error = ?e, "Server error");
                }
                Ok(())
            }
            Err(_e) if lifecycle.is_cancelled() => {
                info!("MCP server initialization cancelled");
                Ok(())
            }
            Err(e) => {
                error!(error = ?e, "Failed to start MCP server");
                Err(SshMcpError::connection(e.to_string()))
            }
        }
    }
    .await;

    lifecycle.cancel();
    if let Some(handle) = recovery_handle {
        // Both sleeps and in-flight transport acquisition observe lifecycle.
        if let Err(e) = handle.await {
            error!(error = ?e, "SSH recovery task failed");
        }
    }
    signal_handle.abort();
    if let Err(e) = signal_handle.await
        && !e.is_cancelled()
    {
        error!(error = ?e, "Shutdown signal task failed");
    }
    server_for_shutdown.shutdown().await;

    info!("SeSSHion stopped");

    service_result
}
