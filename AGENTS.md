# SSH MCP Server (Rust)

A high-performance Rust implementation of the SSH Model Context Protocol (MCP) server. This tool enables AI models to securely interact with remote Linux systems via SSH, providing persistent connections, command execution, and root elevation capabilities.

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
│   ├── integration_test.rs       # Command formatting and connection logic tests
│   ├── logging_test.rs           # Logging configuration and initialization tests
│   ├── compact_response_test.rs  # Compact response formatting and MCP protocol compliance
│   ├── docker_integration/       # Modular E2E tests using Docker containers
│   │   ├── mod.rs                # Module exports
│   │   ├── common.rs             # Shared test utilities and helpers
│   │   ├── check_process_tests.rs # Process monitoring tests
│   │   ├── exec_raw_tests.rs     # ExecRaw transport tests
│   │   ├── sftp_tests.rs         # SFTP transport tests
│   │   ├── scp_tests.rs          # SCP transport tests
│   │   ├── rsync_tests.rs        # Rsync transfer tests
│   │   ├── rsync_timeout_tests.rs # Rsync timeout and process cleanup tests
│   │   ├── overwrite_tests.rs    # File overwrite behavior tests
│   │   ├── fallback_tests.rs     # Transport fallback chain tests
│   │   ├── auth_tests.rs         # Authentication method tests
│   │   ├── timeout_tests.rs      # Command timeout behavior tests
│   │   └── oom_tests.rs          # Output truncation and OOM protection tests
│   └── fixtures/                 # Test fixtures and Docker images
│       └── debian-sshd/          # Custom Debian SSHD image for E2E tests
│           ├── Dockerfile        # Lightweight debian:trixie-slim with GNU tar
│           └── README.md         # Build and usage instructions
├── src/                        # Source code
│   ├── main.rs                 # Application entry point
│   ├── lib.rs                  # Library root
│   ├── server.rs               # MCP protocol server implementation (orchestrator)
│   ├── server/                 # Server submodules (extracted from server.rs)
│   │   ├── tools.rs            # MCP tool schemas and documentation
│   │   ├── args.rs             # Common tool argument parsing
│   │   └── exec.rs             # Shared background execution (exec/sudo-exec deduplication)
│   ├── shell_escape.rs         # Shell string escaping utilities (neutral, no ssh/background deps)
│   ├── config.rs               # Configuration and CLI argument parsing
│   ├── error.rs                # Centralized error handling
│   ├── logging.rs              # Logging configuration and initialization
│   ├── platform.rs             # Platform-specific constants (O_NOFOLLOW_FLAG)
│   ├── ssh/                    # SSH core logic
│   │   ├── mod.rs              # SSH module definition
│   │   ├── connection.rs       # SSH session and connection management
│   │   ├── command.rs          # Command execution over SSH
│   │   ├── handler.rs          # SSH event handlers (russh implementation)
│   │   ├── elevation.rs        # Privileged execution (su/sudo) logic
│   │   ├── sanitize.rs         # Input validation and command safety
│   │   └── config.rs           # SSH-specific configuration structures
│   ├── background/             # Background job subsystem (extracted from server.rs)
│   │   ├── mod.rs              # Module exports
│   │   ├── job.rs              # Job state and management
│   │   ├── registry.rs         # Job registry (in-memory tracking)
│   │   ├── spooler.rs          # Local log file spooling
│   │   ├── streamer.rs         # Output streaming for background jobs
│   │   ├── detach.rs           # Detach mode detection and caching (Full/Portable/DirectOnly)
│   │   ├── marker.rs           # Background marker parsing (stdout marker extraction)
│   │   ├── response.rs         # JSON response formatting for background tools
│   │   └── wrapper.rs          # Background wrapper script generation
│   ├── tools/                  # MCP tool parameter structs
│   │   └── mod.rs              # Parameter definitions (ExecParams, SudoExecParams, CheckProcessParams)
│   └── transfer/               # File transfer operations
│       ├── mod.rs              # Transfer module definition
│       ├── skeleton.rs         # Shared staging/orchestration helpers (put/get file/dir deduplication)
│       ├── process.rs          # Shared process spawn/capture/timeout helpers
│       ├── walk.rs             # Shared directory traversal helpers (no-symlink)
│       ├── exec_raw.rs         # Raw command execution for transfers
│       ├── openssh.rs          # OpenSSH compatibility layer
│       ├── tar.rs              # TAR archive operations
│       ├── rsync.rs            # Rsync transfer implementation with delta sync support
│       ├── staging.rs          # Atomic staging operations for transfers
│       ├── types.rs            # Transfer type definitions
│       └── local_root.rs       # Local filesystem root operations
```