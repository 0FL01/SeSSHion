//! SSH MCP Server - Entry point
//!
//! This is the main entry point for the SSH MCP Server.
//! It parses CLI arguments, validates configuration, starts the MCP server
//! on stdio transport, and handles graceful shutdown.

use clap::Parser;
use rmcp::service::ServiceExt;
use tracing::{error, info};

use ssh_mcp::config::{Args, Config};
use ssh_mcp::error::Result;
use ssh_mcp::logging::init_logging;
use ssh_mcp::server::SshMcpServer;

#[tokio::main]
async fn main() -> Result<()> {
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

    if config.disable_sudo {
        info!("sudo-exec tool is disabled");
    }

    // Create MCP server
    let server = SshMcpServer::new(config).await?;

    info!("SSH MCP Server running on stdio");

    // Create a clone for the shutdown handler
    let server_for_shutdown = server.clone();

    // Spawn a task to handle shutdown signals
    let shutdown_handle = tokio::spawn(async move {
        // Wait for Ctrl+C or SIGTERM
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("Received SIGINT (Ctrl+C), shutting down...");
            }
            _ = async {
                #[cfg(unix)]
                {
                    let mut sigterm = tokio::signal::unix::signal(
                        tokio::signal::unix::SignalKind::terminate()
                    ).expect("Failed to register SIGTERM handler");
                    sigterm.recv().await;
                }
                #[cfg(not(unix))]
                {
                    std::future::pending::<()>().await;
                }
            } => {
                info!("Received SIGTERM, shutting down...");
            }
        }

        // Cleanup
        server_for_shutdown.shutdown().await;
    });

    // Start the MCP server on stdio transport
    // Note: rmcp's stdio() returns a transport that connects stdin/stdout for JSON-RPC
    match server.serve(rmcp::transport::io::stdio()).await {
        Ok(running_server) => {
            // Wait for the server to finish (it will run until the transport closes)
            info!("MCP server is serving...");
            if let Err(e) = running_server.waiting().await {
                error!("Server error: {}", e);
            }
        }
        Err(e) => {
            error!("Failed to start MCP server: {}", e);
            return Err(ssh_mcp::SshMcpError::connection(e.to_string()));
        }
    }

    // Cancel the shutdown handler if we exit normally
    shutdown_handle.abort();

    info!("SSH MCP Server stopped");

    Ok(())
}
