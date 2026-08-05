# SeSSHion — SSH MCP Server

[![Rust](https://img.shields.io/badge/rust-stable-brightgreen.svg)](https://www.rust-lang.org/)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Protocol: MCP](https://img.shields.io/badge/Protocol-MCP-blue.svg)](https://modelcontextprotocol.io)
[![crates.io](https://img.shields.io/crates/v/ssh-mcp-rs.svg)](https://crates.io/crates/ssh-mcp-rs)

**SeSSHion** (formerly **ssh-mcp / ssh-mcp-rs**) is a capability-bound SSH MCP server for autonomous DevOps agents: composable primitives, deterministic long-running jobs, bounded context, and atomic remote edits.

Written in Rust, SeSSHion gives an AI agent secure, narrowly-scoped control of a remote Linux host over a single persistent SSH session through the [Model Context Protocol](https://modelcontextprotocol.io). It exposes four base tools plus two explicitly privileged sudo variants and is built to keep agent context small and operations deterministic.

## Why

- **Capability-bound surface.** Four base tools and two optional sudo variants, no open-ended remote API. The agent can only run commands, patch one file, transfer files, and inspect background jobs.
- **Deterministic long-running jobs.** `background=true` returns a `job_id` immediately for commands and transfers; poll it with `check_process` instead of depending on the client RPC deadline.
- **Bounded context.** Foreground shell output is capped by `--max-output-tokens` by default. Use bounded commands when inspecting large remote files.
- **Atomic remote edits.** `apply_patch` edits as the SSH user; the separately gated `sudo_apply_patch` preserves the same conflict detection and atomic commit under sudo.

## Tools

| Tool | Purpose |
|------|---------|
| `shell` | Run a command via POSIX `sh` as the connected user; `background=true` for long tasks. |
| `sudo_shell` | Same, under `sudo` (uses `--sudo-password`); can be disabled with `--disable-sudo`. |
| `check_process` | Poll a command or transfer background job by `job_id`. |
| `apply_patch` | Create, update, or delete one remote UTF-8 file with an exact patch (atomic, conflict-checked). |
| `sudo_apply_patch` | Same exact patch flow under `sudo`; can be disabled with `--disable-sudo`. |
| `transfer` | Move files/directories (`put`/`get`); `background=true` returns immediately. `auto` falls back through `rsync` → `sftp` → `scp` → `exec-raw` only when a transport is unavailable before writing. |

Full parameter schemas are served to the client at runtime; deeper references live in [`Docs/`](#documentation).

Inspect remote text with bounded shell commands such as `head -n 800 -- /path`, `tail -n 200 -- /path`, or `sed -n '801,1600p' -- /path`. Use `transfer` with `operation=get` to retrieve files instead of printing large content into the MCP response.

## Installation

### Pre-built binaries (recommended)

Download the latest rolling release from the [Releases page](https://github.com/0FL01/SeSSHion/releases/tag/rolling):

| Platform | Download |
|----------|----------|
| Linux x86_64 | [ssh-mcp-linux-x86_64](https://github.com/0FL01/SeSSHion/releases/download/rolling/ssh-mcp-linux-x86_64) |
| Windows x86_64 | [ssh-mcp-windows-x86_64.exe](https://github.com/0FL01/SeSSHion/releases/download/rolling/ssh-mcp-windows-x86_64.exe) |
| macOS ARM64 | [ssh-mcp-macos-aarch64](https://github.com/0FL01/SeSSHion/releases/download/rolling/ssh-mcp-macos-aarch64) |

```bash
curl -L https://github.com/0FL01/SeSSHion/releases/download/rolling/ssh-mcp-linux-x86_64 -o ssh-mcp
chmod +x ssh-mcp && sudo mv ssh-mcp /usr/local/bin/
ssh-mcp --version
```

### Cargo (crates.io)

```bash
cargo install ssh-mcp-rs   # installs the `ssh-mcp` binary to ~/.cargo/bin
```

SeSSHion retains the established `ssh-mcp-rs` crate and `ssh-mcp` executable names for installation and configuration compatibility.

### Build from source

Requires the [Rust toolchain](https://rustup.rs/) plus `pkg-config` and OpenSSL headers (`libssl-dev` on Debian/Ubuntu).

```bash
git clone https://github.com/0FL01/SeSSHion.git && cd SeSSHion
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
        "--key=~/.ssh/id_ed25519"
      ],
      "enabled": true
    }
  }
}
```

Use `--password=your-password` instead of `--key=...` for password authentication.
For `--key`, a leading `~/` is resolved through the local `HOME`; other tilde forms are left unchanged.

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
| `--key` | `SSH_MCP_KEY` | Path to private key file (leading `~/` uses local `HOME`) |
| `--spool-dir` | `SSH_MCP_SPOOL_DIR` | Absolute local directory for background job logs and state |
| `--sudo-password` | `SSH_MCP_SUDO_PASSWORD` | Password for `sudo` commands |
| `--timeout` | `SSH_MCP_TIMEOUT` | Command timeout in ms (default: 300000) |
| `--max-output-tokens` | `SSH_MCP_MAX_OUTPUT_TOKENS` | Shell output token limit (default: 16000 ≈ 64KB; `none` to disable) |
| `--disable-sudo` | `SSH_MCP_DISABLE_SUDO` | Disable the `sudo_shell` and `sudo_apply_patch` tools |

Run `ssh-mcp --help` for the full list (logging, keepalive, reconnect, host-key options).

The explicit spool path must be absolute. Without it, Unix uses `$XDG_RUNTIME_DIR/ssh-mcp` when `XDG_RUNTIME_DIR` is absolute, then falls back to `${TMPDIR:-/tmp}/ssh-mcp-$EUID`. Windows uses `%TEMP%\ssh-mcp`. On Unix, the spool directory must be owned by the server user and is kept at mode `0700`.

### SSH host key verification

`ssh-mcp` verifies the server host key before authentication to prevent silent man-in-the-middle replacement:

- `accept-new` (default): trust and record an unknown key on first connection; reject later changes.
- `yes`: require the key to already exist in `known_hosts`; reject unknown or changed keys.
- `no`: disable verification; only for disposable test environments.

## Long-running jobs

Start potentially long commands or transfers with `background=true`. MCP does not expose the client's deadline to the server, so the client may stop waiting earlier even when the server-side timeout is longer. Background mode returns a `job_id` before SSH connection or transfer preflight. Poll it with `check_process`:

```json
{"job_id": "abc123", "tail_lines": 50}
```

For a scheduled one-shot observation, set a local wait in seconds:

```json
{"job_id": "abc123", "wait_for": 600, "tail_lines": 10}
```

Command jobs return PID, command, log path/tail, and exit state. Transfer jobs return `job_type="transfer"`, coarse phase, elapsed time, current transport, reliable file staging bytes when available, and the compact transfer result at completion. Transfer jobs are in-memory only. Their `timeout_ms` is one whole-operation deadline including connection, preflight, and safe fallback; cleanup gets a short separate grace period.

`check_process` first validates and snapshots the job. Errors and terminal states return immediately. A running job waits locally for the full `wait_for` interval without polling, then returns one fresh snapshot; completion during the interval does not wake the call early.

Cancelling `check_process` stops only its passive local wait. Cancelling the request that created a background job after handoff does not stop that job. Foreground transfer cancellation stops its owned writer and prevents commit; call `check_process` again for authoritative background state.

On SIGINT or SIGTERM, the server cancels the MCP service and performs its bounded request drain before closing SSH. Shutdown prevents new SSH connections and reconnects, but closing the session may terminate channel-bound remote commands; no explicit remote kill or survival guarantee is made.

For command jobs, the returned state is one of `running`, `completed`, `failed`, or `state_lost`, plus the log tail. `completed` and `failed` include an `exit_code`; `state_lost` means the server no longer has a trustworthy terminal outcome.

Transfer destinations have one active in-process writer. Staging names are collision-resistant and exclusively created; overwrite commits use sibling staging, and directory replacement rolls back the old destination if installation fails. Directory `overwrite=false` intentionally remains non-atomic.

Background tracking requires the MCP server and its SSH session to remain alive; transfer jobs do not survive server restart and do not automatically resume.

## Safety

- **Stdio transport.** JSON-RPC over stdin/stdout — no exposed network ports.
- **Credentials in memory only.** Passwords and keys are never logged.
- **Logs to stderr.** Internal logging stays off the MCP protocol channel.
- **Path validation.** Rejects control characters, traversal (`..`), and shell-injection shapes.
- **Binary protection.** Patch tools reject non-UTF-8 content to prevent corruption.
- **Atomic edits.** Staging with automatic cleanup and conflict detection; no silent overwrites.
- **Explicit elevation.** `apply_patch` never retries under sudo; privileged edits require an explicit `sudo_apply_patch` call.

## Documentation

Deeper references live in [`Docs/`](Docs/):

- [`ssh-remote-file-editing-reference.md`](Docs/ssh-remote-file-editing-reference.md) — the SSH file-editing workflow.
- [`diff-generation-reference.md`](Docs/diff-generation-reference.md) — diff generation for file operations.
- [`backup-manager-reference.md`](Docs/backup-manager-reference.md) — backup manager implementation.
- [`rmcp-sdk.md`](Docs/rmcp-sdk.md) / [`russh-library.md`](Docs/russh-library.md) — SDK and SSH library notes.

## License

MIT — see [LICENSE](LICENSE) for details.
