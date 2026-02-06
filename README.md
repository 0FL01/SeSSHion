# SSH MCP Server (Rust Implementation)

[![Rust](https://img.shields.io/badge/rust-stable-brightgreen.svg)](https://www.rust-lang.org/)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Protocol: MCP](https://img.shields.io/badge/Protocol-MCP-blue.svg)](https://modelcontextprotocol.io)

A high-performance Rust implementation of the SSH Model Context Protocol (MCP) server. This tool allows AI models to securely interact with remote Linux systems over SSH, providing tools for command execution and administrative tasks.

> This project is a comprehensive Rust port of the [TypeScript SSH MCP Server](https://github.com/tufantunc/ssh-mcp). It aims for complete feature parity with the original implementation while introducing unique, advanced features in the future.

## ✨ Features

- **Persistent Connections**: Maintains a single SSH session across multiple tool calls for maximum speed.
- **Auto-Reconnect**: Automatically restores the connection if it drops.
- **Interactive Elevation**: Supports `su` elevation with PTY shell for full root access.
- **Sudo Integration**: Provides a `sudo-exec` tool with password wrapping.
- **Command Sanitization**: Built-in safety checks for command inputs.
- **Output Control**: Configurable output length limits to prevent token overflow.
- **Cross-Platform**: Compiled binary runs on any system with SSH access.

## 📊 Performance

| Metric | TypeScript (Node.js) | Rust (Native) | Difference |
|--------|----------------------|---------------|------------|
| **RAM (RSS)** | ~82.5 MB | ~5.4 MB | **~15x more efficient** |
| **CPU Time (Start)** | 0.55s | 0.01s | **Near-zero overhead** |
| **Response Speed** | Instant | Instant | Limited by SSH latency |
| **Context Usage** | ~800 tokens | ~800 tokens | Fixed server schema overhead |

### 🛠️ Test Methodology

To evaluate performance, the following scenarios were executed:

1.  **Warmup**: A simple `echo` command to verify connectivity.
2.  **Listing**: The command `find /usr -maxdepth 2 | head -n 50` to generate a data stream.

**Monitoring Details:**

-   **Tools**: A monitoring script polled `/proc/[PID]/stat` every 100ms.
-   **Memory**: The TypeScript process demonstrated stable memory usage around **82 MB** (typical for Node.js runtime), whereas the Rust process consumed only **~5.4 MB**, highlighting a massive reduction in base overhead.
-   **CPU**: Both implementations showed minimal load during command execution (<10ms CPU time for the sequence), indicating high I/O efficiency. However, Rust demonstrated significantly lower initialization time (accumulated CPU time).

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

The binary will be available at `./target/release/ssh-mcp`.

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

## 💡 Usage Examples

### Basic Usage (stderr logging)
```bash
./ssh-mcp --host=192.168.1.10 --user=admin --password=secret
```

### Debug Logging
```bash
./ssh-mcp --host=192.168.1.10 --user=admin --log-level=debug
```

### File Logging (JSON format)
```bash
./ssh-mcp \
  --host=prod-server \
  --user=admin \
  --log-file=/var/log/ssh-mcp/app.log \
  --log-format=json \
  --log-rotation=daily
```

Note: with `--log-rotation=daily`, the actual file will be `/var/log/ssh-mcp/app.log.YYYY-MM-DD`.
Use `--log-rotation=never` to write exactly to `/var/log/ssh-mcp/app.log`.

### Environment Variables
```bash
export SSH_MCP_HOST=prod-server
export SSH_MCP_USER=admin
export SSH_MCP_LOG_LEVEL=warn
export SSH_MCP_LOG_FILE=/var/log/ssh-mcp/app.log
export SSH_MCP_LOG_FORMAT=json

./ssh-mcp --password=secret
```

### With SSH Key
```bash
./ssh-mcp \
  --host=192.168.1.10 \
  --user=agent \
  --key=/home/agent/.ssh/id_rsa \
  --log-level=debug
```

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

## 🚀 Adding to MCP Clients

### Claude Desktop

Add this to your `claude_desktop_config.json`:

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
        "--maxChars=1000",
        "--log-file=logs/ssh-mcp.log",
        "--log-format=json",
        "--log-rotation=daily"
      ]
    }
  }
}
```

With `--log-rotation=daily`, the file will be created as `logs/ssh-mcp.log.YYYY-MM-DD`.

## 🛠 Tools

The server exposes the following MCP tools:

### `exec`
Execute a shell command as the connected user.
- **Arguments**:
  - `command` (string, required): The shell command to execute.
  - `background` (boolean, default: false): If true, start the command detached via nohup and return immediately with a JSON response containing `{job_id, pid, log_path}`. Recommended for long-running operations to avoid client timeouts.
  - `timeout_ms` (integer, optional): Override the default command timeout (ms). If the foreground command exceeds this timeout, it auto-detaches to background and returns `{ok:false, timeout:true, background:true, job_id, pid, log_path}`.
  - `log_path` (string, optional): Custom remote log path for background mode. Defaults to `/tmp/ssh-mcp/<job_id>.log`.

### `sudo-exec`
Execute a command with root privileges using `sudo`.
- **Arguments**:
  - `command` (string, required): The shell command to execute with sudo.
  - `background` (boolean, default: false): Same behavior as `exec` - detach long-running commands.
  - `timeout_ms` (integer, optional): Override timeout (ms). Same auto-detach behavior on timeout.
  - `log_path` (string, optional): Custom log path for background mode.
- **Note**: This tool uses the `--sudo-password` provided at startup.

### Monitoring Background Jobs
When using `background=true` or when a command auto-detaches on timeout:
- The response includes `{job_id, pid, log_path}` and a `hint` field with monitoring guidance.
- **Recommended approach**: Sleep between checks instead of tight polling.
  - Start with 2-5s intervals, then use 10-30s for longer-running jobs.
- Check progress with:
  ```bash
  ps -p <pid> -o pid,etime,cmd
  tail -n 50 -- '<log_path>'
  ```

## 🔒 Security

- **Stdio Transport**: Communicates using JSON-RPC over stdin/stdout, ensuring no exposed ports.
- **Credential Storage**: Passwords and keys are only kept in memory and never logged.
- **Logging**: All internal logs are sent to `stderr` to avoid interfering with the MCP protocol.

## 📄 License

This project is licensed under the MIT License - see the LICENSE file for details.
