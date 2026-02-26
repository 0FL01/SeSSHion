# SSH MCP Server (Rust Implementation)

[![Rust](https://img.shields.io/badge/rust-stable-brightgreen.svg)](https://www.rust-lang.org/)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Protocol: MCP](https://img.shields.io/badge/Protocol-MCP-blue.svg)](https://modelcontextprotocol.io)

A high-performance Rust implementation of the SSH Model Context Protocol (MCP) server, optimized for DevOps workflows. This tool allows AI models to securely interact with remote Linux systems over SSH, providing tools for command execution and administrative tasks.

## ✨ Features

- **DevOps-Optimized**: Purpose-built for CI/CD, infrastructure automation, deployment pipelines, and system administration workflows.
- **Persistent Connections**: Maintains a single SSH session across multiple tool calls for maximum speed.
- **Auto-Reconnect**: Automatically restores the connection if it drops.
- **Interactive Elevation**: Supports `su` elevation with PTY shell for full root access.
- **Sudo Integration**: Provides a `sudo-exec` tool with password wrapping.
- **File Transfer**: Upload/download files and directories via SFTP, SCP, rsync (delta sync), or streaming (works with both key and password auth).
- **Command Sanitization**: Built-in safety checks for command inputs.
- **Output Control**: Configurable output length limits to prevent token overflow.
- **Cross-Platform**: Compiled binary runs on any system with SSH access.

## 🛠 Installation

### Pre-built Binaries (Recommended)

Download the latest rolling release for your platform from the [Releases page](https://github.com/0FL01/ssh-mcp-rs/releases/tag/rolling).

| Platform | Download Link |
|----------|---------------|
| **Linux x86_64** | [ssh-mcp-linux-x86_64](https://github.com/0FL01/ssh-mcp-rs/releases/download/rolling/ssh-mcp-linux-x86_64) |
| **Windows x86_64** | [ssh-mcp-windows-x86_64.exe](https://github.com/0FL01/ssh-mcp-rs/releases/download/rolling/ssh-mcp-windows-x86_64.exe) |
| **macOS ARM64** | [ssh-mcp-macos-aarch64](https://github.com/0FL01/ssh-mcp-rs/releases/download/rolling/ssh-mcp-macos-aarch64) |

**Quick install (Linux/macOS):**
```bash
# Download and install
curl -L https://github.com/0FL01/ssh-mcp-rs/releases/download/rolling/ssh-mcp-linux-x86_64 -o ssh-mcp
chmod +x ssh-mcp
sudo mv ssh-mcp /usr/local/bin/

# Verify installation
ssh-mcp --version
```

**Quick install (Windows PowerShell):**
```powershell
# Download
Invoke-WebRequest -Uri "https://github.com/0FL01/ssh-mcp-rs/releases/download/rolling/ssh-mcp-windows-x86_64.exe" -OutFile "ssh-mcp.exe"

# Add to PATH (choose a directory in your PATH or add current directory)
# Verify installation
.\ssh-mcp.exe --version
```

### Build from Source

#### Prerequisites

- [Rust toolchain](https://rustup.rs/) (cargo, rustc)
- `pkg-config` and OpenSSL headers (usually `libssl-dev` on Ubuntu/Debian)

#### Build

```bash
git clone https://github.com/0FL01/ssh-mcp-rs.git
cd ssh-mcp-rs
cargo build --release
```

## ⚙️ Configuration

The server is configured via CLI arguments or environment variables.

| Argument | Environment Variable | Description |
|----------|----------------------|-------------|
| `--host` | `SSH_MCP_HOST` | SSH host (required) |
| `--user` | `SSH_MCP_USER` | SSH username (required) |
| `--port` | `SSH_MCP_PORT` | SSH port (default: 22) |
| `--password` | `SSH_MCP_PASSWORD` | SSH password (alt to key) |
| `--key` | `SSH_MCP_KEY` | Path to private key file |
| `--su-password` | `SSH_MCP_SU_PASSWORD` | Password for `su` elevation |
| `--sudo-password` | `SSH_MCP_SUDO_PASSWORD` | Password for `sudo` pipes |
| `--timeout` | `SSH_MCP_TIMEOUT` | Command timeout in ms (default: 60000) |
| `--maxChars` | `SSH_MCP_MAX_CHARS` | Output limit (default: 1000, "none" to disable) |
| `--disable-sudo` | `SSH_MCP_DISABLE_SUDO` | Disable the `sudo-exec` tool |
| `--log-level` | `SSH_MCP_LOG_LEVEL` | Log level: trace, debug, info, warn, error (default: info) |
| `--log-file` | `SSH_MCP_LOG_FILE` | Log file path (base name; daily/hourly adds date suffix) |
| `--log-format` | `SSH_MCP_LOG_FORMAT` | Log file format: text, json (default: text) |
| `--log-rotation` | `SSH_MCP_LOG_ROTATION` | Log rotation: daily, hourly, never (default: daily) |

Note: with `--log-rotation=daily`, the actual file will be `/var/log/ssh-mcp/app.log.YYYY-MM-DD`.
Use `--log-rotation=never` to write exactly to `/var/log/ssh-mcp/app.log`.

## 🚀 Adding to MCP Clients

### OpenCode

Add this to your `opencode.jsonc`:

**With SSH key (recommended for best transfer performance):**
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
        "--key=/path/to/private/key",
        "--timeout=30000",
        "--maxChars=1000"
      ],
      "enabled": true
    }
  }
}
```

**With password (file transfer uses exec-raw transport):**
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
        "--password=your-password",
        "--timeout=30000",
        "--maxChars=1000"
      ],
      "enabled": true
    }
  }
}
```

### Claude Desktop

Add this to your `claude_desktop_config.json`:

**With SSH key (recommended for best transfer performance):**
```json
{
  "mcpServers": {
    "ssh-remote": {
      "command": "/absolute/path/to/ssh-mcp",
      "args": [
        "--host=192.168.1.10",
        "--port=22",
        "--user=agent-nc",
        "--key=/path/to/private/key",
        "--timeout=30000",
        "--maxChars=1000"
      ]
    }
  }
}
```

**With password (file transfer uses exec-raw transport):**
```json
{
  "mcpServers": {
    "ssh-remote": {
      "command": "/absolute/path/to/ssh-mcp",
      "args": [
        "--host=192.168.1.10",
        "--port=22",
        "--user=agent-nc",
        "--password=your-password",
        "--timeout=30000",
        "--maxChars=1000"
      ]
    }
  }
}
```

## 🛠 Tools

The server exposes the following MCP tools:

### `exec`
Execute a command as the connected user via POSIX-compatible `sh`.
- **Arguments**:
  - `command` (string, required): Command string evaluated by POSIX-compatible `sh`; use portable shell syntax.
  - `background` (boolean, default: false): If true, return immediately and continue streaming output to a local log on the MCP server. The job is tracked via `job_id` in an in-memory registry and the response includes `{job_id, pid, log_path}`. Recommended for long-running operations to avoid client timeouts.
  - `timeout_ms` (integer, optional): Override the default command timeout (ms) for foreground runs. If the foreground command exceeds this timeout, it auto-detaches to background and returns `{ok:false, timeout:true, background:true, job_id, pid, log_path}`. When `background=true`, `timeout_ms` is ignored and NOT validated.
  - `log_path` (string, optional): Custom local log path on the MCP server for background mode output. Defaults to `/tmp/ssh-mcp/<job_id>.log`. When `background=false`, `log_path` is ignored and NOT validated.

- **Background response fields**:
  - `log_path` (string): Local log path on the MCP server (e.g. `/tmp/ssh-mcp/<job_id>.log`).
  - `remote_log_path` (string, deprecated): Compatibility field for backward compatibility. Does NOT represent an actual remote log file in the current architecture. Output is streamed locally; use `log_path` for local log access.

### `sudo-exec`
Execute a command with root privileges using `sudo` via POSIX-compatible `sh`.
- **Arguments**:
  - `command` (string, required): Command string evaluated by POSIX-compatible `sh` under sudo; use portable shell syntax.
  - `background` (boolean, default: false): Same behavior as `exec` - return immediately and stream output to a local log.
  - `timeout_ms` (integer, optional): Override timeout (ms) for foreground runs. On timeout, `sudo-exec` foreground returns a timeout error (no auto-detach). Auto-detach-on-timeout applies to `exec` foreground only. When `background=true`, `timeout_ms` is ignored and NOT validated.
  - `log_path` (string, optional): Custom local log path on the MCP server for background mode output. When `background=false`, `log_path` is ignored and NOT validated.
- **Note**: This tool uses the `--sudo-password` provided at startup. For long-running sudo tasks, use `background=true` and monitor via `check-process`.

### `check-process`
Check if a background job is still running and read the tail of its local log (stored on the MCP server).

- **Arguments**:
  - `job_id` (string, required): Job ID returned by `exec`/`sudo-exec` when `background=true`, or by `exec` foreground timeout auto-detach.
  - `tail_lines` (integer, default: 50): Number of last lines to read from the local log.

### `transfer`
Transfer a file or directory over SSH.

- **Authentication**: Supports both SSH key and password authentication. When using password auth, the `exec-raw` transport is used automatically.
- **Local root**: `local_path` can be relative to `local_root` (the server's current working directory at startup) or an absolute path within `local_root`. Paths outside `local_root`, `..` components, and paths that normalize to `.` are rejected.
- **Remote path validation**: `remote_path` must be non-empty, must not contain control characters, must not have leading/trailing whitespace, must not contain NUL, and must not start with `-`.
- **Transport**:
  - `transport=auto`: attempts rsync (most efficient), then sftp, then scp, then falls back to exec-raw deterministically.
  - `transport=sftp` / `transport=scp`: use local OpenSSH client binaries (`sftp` / `scp`).
  - `transport=exec-raw`: uses streaming stdin/stdout over the existing SSH session (tar streaming for directories).
  - `transport=rsync`: uses local rsync binary with SSH transport for efficient delta-sync transfers (requires --key).
  - **Note**: `sftp`/`scp` transports require the server to be started with a private key path (`--key=/path/to/key`). When using password authentication, the `exec-raw` transport is used automatically (streaming over the existing SSH session).

**Directory transfer (tar)**

- Directory transfers use a streamed POSIX `ustar` archive.
- Each tar header is validated (ustar magic/version + checksum). Invalid archives are rejected.
- Entry path rules:
  - must be relative
  - must be non-empty and must not normalize to `.`
  - must not contain `..`
- Supported entry types: regular files and directories only. Symlinks, device nodes, hardlinks, FIFOs, etc. are rejected.
- Remote requirements: the remote host must provide `tar` (or `busybox tar`) in `PATH`.

**Overwrite semantics**

- `overwrite=false` (default - safer)
  - `put file`: requires sibling staging and installs the final file via a hard-link (`ln`) to avoid replacement. This requires hard-link support on the remote filesystem; if unavailable the tool fails with an error suggesting to use `overwrite=true`.
  - `get file`: installs the final file via a local hard-link (`fs::hard_link`). This requires hard-link support on the local filesystem; if unavailable the tool fails with an error suggesting to use `overwrite=true`.
  - `put dir`: fails if the destination exists with a clear error message; use `overwrite=true` to replace existing directories.
  - `get dir`: fails if the destination exists with a clear error message; use `overwrite=true` to replace existing directories.

- `overwrite=true` (explicit opt-in for replacement)
  - `put file`: stream to a staging file and `mv` into place.
  - `get file`: stream to a local staging file and `rename` into place (best-effort replacement on platforms where rename does not replace).
  - `put dir`: extract a streamed tar into a staging directory, then `mv` into place; if the destination existed it may be moved to a backup path during the swap.
  - `get dir`: extract a streamed tar into a local staging directory, then swap into place via rename; if the destination existed it is first renamed to a sibling backup path.

**Staging behavior (no /tmp)**

- Remote staging prefers a sibling path under the destination parent for better atomicity.
- If that location is not writable, `overwrite=true` operations fall back to `$HOME/.ssh-mcp/staging/<id>/...` and then move into place.
- For `overwrite=false` file transfers, fallback staging is not allowed because the finalize step requires a sibling hard-link install; the tool fails if sibling staging is not writable.

**Rsync Options**

When using `transport=rsync`, you can customize behavior via `rsync_options`:
- `checksum` (boolean, default: true): Use checksums instead of file times/sizes for file comparison
- `compress` (boolean, default: false): Compress data during transfer
- `delete` (boolean, default: false): Delete files on destination that don't exist on source
- `inplace` (boolean, default: true): Update files in-place instead of creating new files
- `partial` (boolean, default: true): Keep partially transferred files for resume
- `bwlimit` (integer, optional): Bandwidth limit in KB/s

### Monitoring Background Jobs
When using `background=true` or when a command auto-detaches on timeout:
- The response includes `{job_id, pid, log_path}` and a `hint` field with monitoring guidance.
  - `log_path` is local to the MCP server (default: `/tmp/ssh-mcp/<job_id>.log`).
  - `remote_log_path` may still be present for backward compatibility but is deprecated; it is compat-only and does not represent a remote log file.

### Log Path Restrictions

When providing a custom `log_path`:
- Must be an absolute path
- Must be directly under `/tmp/ssh-mcp/` (no subdirectories)
- Must have `.log` extension
- Cannot contain `.` or `..` components
- Must not have leading/trailing whitespace
- Must not start with `-`
- Must not contain control characters (including `\n` / `\r`)
- Example: `/tmp/ssh-mcp/my-job.log`

Example response:
```json
{
  "job_id": "abc123",
  "pid": 12345,
  "log_path": "/tmp/ssh-mcp/abc123.log",
  "remote_log_path": "/tmp/.ssh-mcp-job-abc123.log"
}
```
- **Recommended approach**: Sleep between checks instead of tight polling.
  - Start with 2-5s intervals, then use 10-30s for longer-running jobs.
- To check the status/output of a background job, use the `check-process` tool with the `job_id`:
  ```json
  {"job_id": "abc123", "tail_lines": 50}
  ```
  Or, if you want to run commands on the target host:
  ```bash
  ps -p <pid> -o pid,etime,cmd
  ```

### When to Use Background Mode

**Typical long-running tasks:**
- Database exports/imports (`mysqldump`, `pg_dump`, `pg_restore`)
- Large file transfers (`rsync`, `scp`)
- Build processes (`cargo build`, `make`, `npm install`)
- Container operations (`docker build`, `docker compose up`)
- System maintenance (`apt update`, `yum update`, log rotation)

**Agent workflow:**
1. Start command with `background=true` or let it auto-detach on timeout
2. Get `{job_id, pid, log_path}` immediately
3. Sleep 2-5s, then check status/output with `check-process` using `job_id`
4. For long jobs, increase interval to 10-30s
   5. Confirm completion when `check-process` reports `running=false` and an `exit_code`

When using `--log-rotation=daily`, log files are suffixed with the date: `<log_file>.YYYY-MM-DD` (in the same directory as `--log-file`).

## 📝 JSON Log Format

When `--log-file` is specified with `--log-format=json`, logs are written in structured JSON format:

```json
{"timestamp":"2024-01-24T10:15:23.456789Z","level":"INFO","message":"SSH MCP Server v1.4.0 starting...","target":"ssh_mcp"}
{"timestamp":"2024-01-24T10:15:23.458Z","level":"INFO","message":"Connecting to admin@prod-server:22","target":"ssh_mcp"}
{"timestamp":"2024-01-24T10:15:24.123Z","level":"ERROR","message":"Command timeout after 60000ms","target":"ssh_mcp::command"}
```

Use `jq` for pretty printing:
```bash
# Daily rotation writes to a date-suffixed filename
tail -f "/var/log/ssh-mcp/app.log.$(date +%Y-%m-%d)" | jq

# Or disable rotation for a stable filename
# tail -f /var/log/ssh-mcp/app.log | jq
```
   
## 🔒 Security

- **Stdio Transport**: Communicates using JSON-RPC over stdin/stdout, ensuring no exposed ports.
- **Credential Storage**: Passwords and keys are only kept in memory and never logged.
- **Logging**: All internal logs are sent to `stderr` to avoid interfering with the MCP protocol.

## 📄 License

This project is licensed under the MIT License - see the LICENSE file for details.
