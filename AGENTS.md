# SSH MCP Server (Rust)

A high-performance Rust implementation of the SSH Model Context Protocol (MCP) server. This tool enables AI models to securely interact with remote Linux systems via SSH, providing persistent connections, command execution, and root elevation capabilities.

## Repository Structure

```text
.
├── Cargo.toml            # Project manifest and dependencies
├── README.md             # Detailed project overview and usage
├── AGENTS.md             # LLM-oriented project documentation (this file)
├── Docs/                 # Additional technical documentation
│   ├── rmcp-sdk.md       # Notes on the MCP SDK integration
│   └── russh-library.md  # Notes on the underlying SSH library (russh)
├── tests/                # Integration and functional tests
│   ├── integration_test.rs # Command formatting and connection logic tests
│   └── docker_integration_test.rs # Full E2E tests using Docker containers
└── src/                  # Source code
    ├── main.rs           # Application entry point
    ├── lib.rs            # Library root
    ├── server.rs         # MCP protocol server implementation
    ├── config.rs         # Configuration and CLI argument parsing
    ├── error.rs          # Centralized error handling
    ├── ssh/              # SSH core logic
    │   ├── mod.rs        # SSH module definition
    │   ├── connection.rs # SSH session and connection management
    │   ├── command.rs    # Command execution over SSH
    │   ├── handler.rs    # SSH event handlers (russh implementation)
    │   ├── elevation.rs  # Privileged execution (su/sudo) logic
    │   ├── sanitize.rs   # Input validation and command safety
    │   └── config.rs     # SSH-specific configuration structures
    └── tools/            # MCP tool definitions
        └── mod.rs        # Tool registration and dispatch
```
