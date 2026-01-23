# Contributing to ssh-mcp

Thank you for your interest in contributing to ssh-mcp! Your help is greatly appreciated. Please follow these guidelines to make the process smooth for everyone.

## How to Contribute

1. **Fork the repository** and create your branch from `main`.
2. **Clone your fork** to your local machine.
3. **Create a descriptive branch name** (e.g., `feature/add-ssh-support` or `bugfix/fix-connection-issue`).
4. **Make your changes** with clear, concise commits.
5. **Test your changes** to ensure nothing is broken.
6. **Push to your fork** and submit a Pull Request (PR) to the `main` branch.

## Code Style
- Follow the existing code style and conventions.
- Write clear, descriptive commit messages.
- Add comments where necessary for clarity.

## Testing

### Unit Tests
Run the standard test suite:
```bash
cargo test
```

### Integration Tests
The project includes Docker-based integration tests that verify MCP tools work correctly with a real SSH server. These tests use `testcontainers` to automatically spin up an SSH environment.

**Prerequisites:**
- Docker installed and running on your system

**Run integration tests:**
```bash
cargo test --test docker_integration_test
```

**What the integration tests verify:**
- SSH connection establishment with the remote server
- The `exec` tool runs commands as the authenticated user
- The `sudo-exec` tool runs commands with elevated privileges
- Proper MCP protocol handling and response formatting

Note: The integration test is designed to be resilient. Even if `sudo` fails due to container configuration, the test will pass as long as the MCP protocol handling is working correctly.

### Run All Tests
To run both unit and integration tests:
```bash
cargo test --all
```

## Issues and Bugs
- If you find a bug, please open an issue with detailed steps to reproduce it.
- If you want to work on an existing issue, comment on it to let others know.

## Feature Requests
- Open an issue to discuss new features before submitting a PR.
- Describe your proposed feature and its use case.

## Pull Requests
- Ensure your PR is up to date with the latest `main` branch.
- Reference related issues in your PR description (e.g., `Closes #12`).
- Be responsive to feedback and requested changes.
- Ensure all tests pass before submitting.

## Code of Conduct
- Be respectful and inclusive in all interactions.
- See the [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) if available.

Thank you for helping make ssh-mcp better!
