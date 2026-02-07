# SSH MCP Server (Rust)

A high-performance Rust implementation of the SSH Model Context Protocol (MCP) server. This tool enables AI models to securely interact with remote Linux systems via SSH, providing persistent connections, command execution, and root elevation capabilities.

## Available MCP Tools

This server provides the following tools for AI agents:

- **`ssh-test-env_exec`** - Execute shell commands on the remote SSH server
- **`ssh-test-env_sudo-exec`** - Execute shell commands with sudo privileges (if passwordless sudo is configured)

> **Note:** These MCP tools are intended for testing the SSH MCP server binary itself. Do not use them unless explicitly requested.

## Repository Structure

```text
.
├── Cargo.toml            # Project manifest and dependencies
├── README.md             # Detailed project overview and usage
├── AGENTS.md             # LLM-oriented project documentation (this file)
├── .github/              # CI/CD and GitHub configuration
│   └── workflows/        # GitHub Actions definitions
│       └── main.yml      # Unified CI/CD pipeline (Test, Lint, Build, Release)
├── Docs/                 # Additional technical documentation
│   ├── rmcp-sdk.md       # Notes on the MCP SDK integration
│   └── russh-library.md  # Notes on the underlying SSH library (russh)
├── tests/                # Integration and functional tests
│   ├── integration_test.rs # Command formatting and connection logic tests
│   ├── docker_integration_test.rs # Full E2E tests using Docker containers
│   └── logging_test.rs      # Logging configuration and initialization tests
    └── src/                  # Source code
        ├── main.rs           # Application entry point
        ├── lib.rs            # Library root
        ├── server.rs         # MCP protocol server implementation
        ├── config.rs         # Configuration and CLI argument parsing
        ├── error.rs          # Centralized error handling
        ├── logging.rs        # Logging configuration and initialization
        ├── ssh/              # SSH core logic
        │   ├── mod.rs        # SSH module definition
        │   ├── connection.rs # SSH session and connection management
        │   ├── command.rs    # Command execution over SSH
        │   ├── handler.rs    # SSH event handlers (russh implementation)
        │   ├── elevation.rs  # Privileged execution (su/sudo) logic
        │   ├── sanitize.rs   # Input validation and command safety
        │   └── config.rs     # SSH-specific configuration structures
        ├── tools/            # MCP tool definitions
        │   └── mod.rs        # Tool registration and dispatch
        └── transfer/         # File transfer operations
            ├── mod.rs        # Transfer module definition
            ├── exec_raw.rs   # Raw command execution for transfers
            ├── openssh.rs    # OpenSSH compatibility layer
            ├── tar.rs        # TAR archive operations
            ├── types.rs      # Transfer type definitions
            └── local_root.rs # Local filesystem root operations
```
