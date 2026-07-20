# SSH MCP Server

[![Rust](https://img.shields.io/badge/rust-stable-brightgreen.svg)](https://www.rust-lang.org/)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Protocol: MCP](https://img.shields.io/badge/Protocol-MCP-blue.svg)](https://modelcontextprotocol.io)
[![crates.io](https://img.shields.io/crates/v/ssh-mcp-rs.svg)](https://crates.io/crates/ssh-mcp-rs)

Capability-bound SSH MCP for autonomous DevOps agents: six composable primitives, deterministic long-running jobs, bounded context, and atomic remote edits.

`ssh-mcp` is a Rust [Model Context Protocol](https://modelcontextprotocol.io) server that gives an AI agent secure, narrowly-scoped control of a remote Linux host over a single persistent SSH session. It exposes exactly six tools — nothing more — and is built to keep agent context small and operations deterministic.

## Why

- **Capability-bound surface.** Six composable primitives, no open-ended remote API. The agent can only run commands, read files, patch one file, transfer files, and inspect background jobs.
- **Deterministic long-running jobs.** `background=true` (or a foreground timeout auto-detach) returns `{job_id, pid, log_path}` immediately; output streams to a local log you poll with `check_process`. No client RPC timeouts on long work.
- **Bounded context.** `read` defaults to a safe preview with token estimates; shell output is capped by `--max-output-tokens`. Large files cannot bomb the agent's context window.
- **Atomic remote edits.** `apply_patch` applies one exact Add/Update/Delete patch with conflict detection and atomic commit — no fuzzy matching, no silent overwrites.

## The six primitives

| Tool | Purpose |
|------|---------|
| `shell` | Run a command via POSIX `sh` as the connected user; `background=true` for long tasks. |
| `sudo_shell` | Same, under `sudo` (uses `--sudo-password`); can be disabled with `--disable-sudo`. |
| `check_process` | Poll a background job by `job_id` and read the tail of its local log. |
| `read` | Read a remote UTF-8 file with `preview` / `head` / `tail` / `full` modes and token estimates. |
| `apply_patch` | Create, update, or delete one remote UTF-8 file with an exact patch (atomic, conflict-checked). |
| `transfer` | Move files/directories (`put`/`get`) via `auto` → `rsync` → `sftp` → `scp` → `exec-raw`. |

Full parameter schemas are served to the client at runtime; deeper references live in [`Docs/`](#documentation).

## Installation

### Pre-built binaries (recommended)

Download the latest rolling release from the [Releases page](https://github.com/0FL01/ssh-mcp-rs/releases/tag/rolling):

| Platform | Download |
|----------|----------|
| Linux x86_64 | [ssh-mcp-linux-x86_64](https://github.com/0FL01/ssh-mcp-rs/releases/download/rolling/ssh-mcp-linux-x86_64) |
| Windows x86_64 | [ssh-mcp-windows-x86_64.exe](https://github.com/0FL01/ssh-mcp-rs/releases/download/rolling/ssh-mcp-windows-x86_64.exe) |
| macOS ARM64 | [ssh-mcp-macos-aarch64](https://github.com/0FL01/ssh-mcp-rs/releases/download/rolling/ssh-mcp-macos-aarch64) |

```bash
curl -L https://github.com/0FL01/ssh-mcp-rs/releases/download/rolling/ssh-mcp-linux-x86_64 -o ssh-mcp
chmod +x ssh-mcp && sudo mv ssh-mcp /usr/local/bin/
ssh-mcp --version
```

### Cargo (crates.io)

```bash
cargo install ssh-mcp-rs   # installs the `ssh-mcp` binary to ~/.cargo/bin
```

### Build from source

Requires the [Rust toolchain](https://rustup.rs/) plus `pkg-config` and OpenSSL headers (`libssl-dev` on Debian/Ubuntu).

```bash
git clone https://github.com/0FL01/ssh-mcp-rs.git && cd ssh-mcp-rs
cargo build --release
```

## Adding to MCP clients

### OpenCode

Add to `opencode.jsonc` (SSH key recommended; password auth uses the `exec-raw` transfer transport):

```jsonc
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "ssh-remote": {
      "type": "local",
      "command": [
        "/absolute/path/to/ssh-mcp",
        "--host=192.168.1.10",
        "--port=22",
        "--user=agent-nc",
        "--key=/path/to/private/key"
      ],
      "enabled": true
    }
  }
}
```

Use `--password=your-password` instead of `--key=...` for password authentication.

<details>
<summary><b>Claude Code</b> — .mcp.json or ~/.claude.json</summary>

Add to your project's `.mcp.json` (shared via git) or to `~/.claude.json` under the top-level `mcpServers` key:

```json
{
  "mcpServers": {
    "ssh-remote": {
      "type": "stdio",
      "command": "/absolute/path/to/ssh-mcp",
      "args": [
        "--host=192.168.1.10",
        "--port=22",
        "--user=agent-nc",
        "--key=/path/to/private/key"
      ]
    }
  }
}
```

</details>

<details>
<summary><b>Strict production</b> — verified host key</summary>

Set `--strict-host-key-checking=yes` and point at a pre-populated `known_hosts` file (works in any client config):

```jsonc
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "ssh-remote": {
      "type": "local",
      "command": [
        "/absolute/path/to/ssh-mcp",
        "--host=example.com",
        "--user=alice",
        "--key=/home/alice/.ssh/id_ed25519",
        "--strict-host-key-checking=yes",
        "--known-hosts=/home/alice/.ssh/known_hosts"
      ],
      "enabled": true
    }
  }
}
```

</details>

## Configuration

Every flag also has an `SSH_MCP_*` environment variable. Required: `--host`, `--user`, and one of `--password` / `--key`.

| Argument | Env | Description |
|----------|-----|-------------|
| `--host` | `SSH_MCP_HOST` | SSH host (required) |
| `--user` | `SSH_MCP_USER` | SSH username (required) |
| `--port` | `SSH_MCP_PORT` | SSH port (default: 22) |
| `--password` | `SSH_MCP_PASSWORD` | SSH password (alternative to key) |
| `--key` | `SSH_MCP_KEY` | Path to private key file |
| `--sudo-password` | `SSH_MCP_SUDO_PASSWORD` | Password for `sudo` commands |
| `--timeout` | `SSH_MCP_TIMEOUT` | Command timeout in ms (default: 300000) |
| `--max-output-tokens` | `SSH_MCP_MAX_OUTPUT_TOKENS` | Shell/read output token limit (default: 16000 ≈ 64KB; `none` to disable) |
| `--disable-sudo` | `SSH_MCP_DISABLE_SUDO` | Disable the `sudo_shell` tool |

Run `ssh-mcp --help` for the full list (logging, keepalive, reconnect, host-key options).

### SSH host key verification

`ssh-mcp` verifies the server host key before authentication to prevent silent man-in-the-middle replacement:

- `accept-new` (default): trust and record an unknown key on first connection; reject later changes.
- `yes`: require the key to already exist in `known_hosts`; reject unknown or changed keys.
- `no`: disable verification; only for disposable test environments.

## Long-running jobs

Start with `background=true` (or let a foreground command auto-detach on timeout). You immediately get `{job_id, pid, log_path}`, where `log_path` is a local log on the MCP server (default `/tmp/ssh-mcp/<job_id>.log`). Poll with `check_process`:

```json
{"job_id": "abc123", "tail_lines": 50}
```

`check_process` returns a strict state — `running`, `completed`, `failed`, or `state_lost` — plus the log tail. Sleep between checks (2–5s, then 10–30s for long jobs) rather than tight-polling; a job is done when the state is terminal and an `exit_code` is present.

## Safety

- **Stdio transport.** JSON-RPC over stdin/stdout — no exposed network ports.
- **Credentials in memory only.** Passwords and keys are never logged.
- **Logs to stderr.** Internal logging stays off the MCP protocol channel.
- **Path validation.** Rejects control characters, traversal (`..`), and shell-injection shapes.
- **Binary protection.** Text tools reject non-UTF-8 content to prevent corruption.
- **Atomic edits.** Staging with automatic cleanup and conflict detection; no silent overwrites.

## Documentation

Deeper references live in [`Docs/`](Docs/):

- [`ssh-remote-file-editing-reference.md`](Docs/ssh-remote-file-editing-reference.md) — the SSH file-editing workflow.
- [`diff-generation-reference.md`](Docs/diff-generation-reference.md) — diff generation for file operations.
- [`backup-manager-reference.md`](Docs/backup-manager-reference.md) — backup manager implementation.
- [`rmcp-sdk.md`](Docs/rmcp-sdk.md) / [`russh-library.md`](Docs/russh-library.md) — SDK and SSH library notes.

## License

MIT — see [LICENSE](LICENSE) for details.
