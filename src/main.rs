//! SSH MCP Server - Entry point
//!
//! This is the main entry point for the SSH MCP Server.
//! It parses CLI arguments, validates configuration, starts the MCP server
//! on stdio transport, and handles graceful shutdown.

use std::time::Duration;

use clap::Parser;
use rmcp::service::ServiceExt;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use ssh_mcp::config::{Args, Config};
use ssh_mcp::error::Result;
use ssh_mcp::logging::init_logging;
use ssh_mcp::server::SshMcpServer;

const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

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
    let args = Args::parse();

    // Initialize logging (JSON to file if specified, text to stderr)
    let _guard = init_logging(&args)?;

    // Validate and create config
    let config = Config::from_args(args)?;

    info!("SSH MCP Server v{} starting...", env!("CARGO_PKG_VERSION"));
    info!(
        "Connecting to {}@{}:{}",
        config.user, config.host, config.port
    );
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
    let server = SshMcpServer::new(config).await?;

    info!("SSH MCP Server running on stdio");

    // Keep a clone for cleanup after the MCP service has stopped.
    let server_for_shutdown = server.clone();
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

    // Start the MCP server on stdio transport
    // Note: rmcp's stdio() returns a transport that connects stdin/stdout for JSON-RPC
    let service_result = match server
        .serve_with_ct(rmcp::transport::io::stdio(), lifecycle.clone())
        .await
    {
        Ok(running_server) => {
            // Wait for the server to finish (it will run until the transport closes)
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
            Err(ssh_mcp::SshMcpError::connection(e.to_string()))
        }
    };

    lifecycle.cancel();
    signal_handle.abort();
    if let Err(e) = signal_handle.await
        && !e.is_cancelled()
    {
        error!(error = ?e, "Shutdown signal task failed");
    }
    server_for_shutdown.shutdown().await;

    info!("SSH MCP Server stopped");

    service_result
}
