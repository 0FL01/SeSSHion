# SSH Remote File Editing - Implementation Reference

> **Use Case**: Reference implementation for adding remote file editing capabilities to MCP servers  
> **Target**: Integration with SeSSHion SSH MCP server infrastructure

---

## Overview

This document describes the architecture and implementation of remote file editing capabilities via SSH/SFTP transport in the Stakpak Agent. It provides a complete reference for integrating similar functionality into your own MCP server implementation.

### Key Capabilities

| Feature | Description |
|---------|-------------|
| **Read** | View files, directories, with grep/glob filtering |
| **Write** | Create new files with auto-directory creation |
| **Edit** | String replacement with unified diff output |
| **Delete** | Safe removal with automatic backups |
| **Execute** | Run commands with timeout and progress tracking |
| **Background** | Async task execution on remote hosts |

---

## Architecture

### Component Diagram

```
┌─────────────────────────────────────────────────────────────────┐
│                     MCP Tool Layer                               │
│  ┌─────────┐ ┌────────────┐ ┌──────────┐ ┌─────────┐            │
│  │  view   │ │ str_replace│ │  create  │ │ remove  │  ...       │
│  └────┬────┘ └─────┬──────┘ └────┬─────┘ └────┬────┘            │
└───────┼────────────┼─────────────┼────────────┼─────────────────┘
        │            │             │            │
        └────────────┴──────┬──────┴────────────┘
                            ▼
┌─────────────────────────────────────────────────────────────────┐
│              RemoteConnectionManager (Connection Pool)           │
│  - Caches active SSH connections                                 │
│  - Reuses sessions across tool calls                             │
│  - Thread-safe (Arc<Mutex<>>)                                    │
└───────────────────────────┬─────────────────────────────────────┘
                            │
                            ▼
┌─────────────────────────────────────────────────────────────────┐
│                    RemoteConnection                              │
│  ┌─────────────────┐  ┌──────────────────┐                      │
│  │  SSH Session    │  │  SFTP Session    │                      │
│  │  (russh)        │  │  (russh-sftp)    │                      │
│  └─────────────────┘  └──────────────────┘                      │
│                                                                  │
│  Methods:                                                        │
│  - connect() - establish SSH + SFTP                              │
│  - read_file_to_string() - SFTP read                             │
│  - write_file() - SFTP write                                     │
│  - execute_command() - SSH exec                                  │
│  - exists(), is_directory(), canonicalize()                      │
└─────────────────────────────────────────────────────────────────┘
```

---

## Core Components

### 1. RemoteConnectionInfo

Configuration structure for SSH connections:

```rust
use std::sync::Arc;
use anyhow::{anyhow, Result};
use russh::client;
use russh_sftp::client::SftpSession;

pub struct RemoteConnectionInfo {
    /// Connection string: "user@host" or "user@host:port"
    pub connection_string: String,
    /// Optional password for authentication
    pub password: Option<String>,
    /// Optional path to private key file
    pub private_key_path: Option<String>,
}

impl RemoteConnectionInfo {
    /// Parse "user@host:port" or "user@host" format
    pub fn parse(connection_string: &str) -> Result<(String, String, Option<u16>)> {
        // Implementation parses connection string into user, host, port
        let parts: Vec<&str> = connection_string.split('@').collect();
        if parts.len() != 2 {
            return Err(anyhow!("Invalid connection string format"));
        }
        let user = parts[0].to_string();
        let host_port = parts[1];
        
        // Parse host and optional port
        if let Some(colon_idx) = host_port.rfind(':') {
            let host = host_port[..colon_idx].to_string();
            let port = host_port[colon_idx + 1..].parse::<u16>().ok();
            Ok((user, host, port))
        } else {
            Ok((user, host_port.to_string(), None))
        }
    }
}
```

### 2. RemoteConnection

The main SSH/SFTP connection handler:

```rust
pub struct RemoteConnection {
    sftp: SftpSession,
    connection_info: RemoteConnectionInfo,
    // Internal SSH session handle (stored for keepalive/reconnection)
    session: client::Handle<SSHClient>,
}

impl RemoteConnection {
    /// Establish new SSH connection and SFTP session
    pub async fn new(connection_info: RemoteConnectionInfo) -> Result<Self> {
        // 1. Create SSH client handler
        let config = client::Config {
            // Standard SSH config
            ..Default::default()
        };
        let config = Arc::new(config);
        
        // 2. Connect to SSH server
        let (user, host, port) = RemoteConnectionInfo::parse(&connection_info.connection_string)?;
        let port = port.unwrap_or(22);
        
        let mut session = client::connect(config, (host.as_str(), port), SSHClient {}).await?;
        
        // 3. Authenticate (password or key)
        if let Some(password) = &connection_info.password {
            // Password auth
            let result = session.authenticate_password(&user, password).await?;
            if !result.success() {
                return Err(anyhow!("Password authentication failed"));
            }
        } else {
            // Key auth - auto-discover or use provided path
            let key_path = if let Some(path) = &connection_info.private_key_path {
                path.clone()
            } else {
                Self::discover_ssh_key().await?
            };
            
            let keypair = russh::keys::load_secret_key(&key_path, None)?;
            let result = session.authenticate_publickey(
                &user,
                russh::keys::PrivateKeyWithHashAlg::new(
                    Arc::new(keypair),
                    session.algorithms().map(|a| a.key)
                ).unwrap()
            ).await?;
            
            if !result.success() {
                return Err(anyhow!("Public key authentication failed"));
            }
        }
        
        // 4. Open SFTP subsystem
        let channel = session.channel_open_session().await?;
        channel.request_subsystem(true, "sftp").await?;
        let sftp = SftpSession::new(channel.into_stream()).await?;
        
        Ok(Self {
            sftp,
            connection_info,
            session,
        })
    }
    
    /// Auto-discover SSH keys from ~/.ssh/
    async fn discover_ssh_key() -> Result<String> {
        let home_dir = dirs::home_dir()
            .ok_or_else(|| anyhow!("Home directory not found"))?;
        let ssh_dir = home_dir.join(".ssh");
        
        if !ssh_dir.is_dir() {
            return Err(anyhow!("SSH directory not found: {}", ssh_dir.display()));
        }
        
        // Try common key names in order of preference
        let key_names = ["id_ed25519", "id_rsa", "id_ecdsa", "id_dsa"];
        for key_name in &key_names {
            let private_key = ssh_dir.join(key_name);
            let public_key = ssh_dir.join(format!("{}.pub", key_name));
            
            if private_key.exists() && public_key.exists() {
                return Ok(private_key.to_string_lossy().to_string());
            }
        }
        
        Err(anyhow!("No SSH private key found in {}", ssh_dir.display()))
    }
    
    // === File Operations ===
    
    /// Read entire file to string
    pub async fn read_file_to_string(&self, path: &str) -> Result<String> {
        let mut file = self.sftp.open(path).await?;
        let mut content = String::new();
        tokio::io::AsyncReadExt::read_to_string(&mut file, &mut content).await?;
        Ok(content)
    }
    
    /// Write bytes to file (creates or overwrites)
    pub async fn write_file(&self, path: &str, content: &[u8]) -> Result<()> {
        let mut file = self.sftp.create(path).await?;
        tokio::io::AsyncWriteExt::write_all(&mut file, content).await?;
        file.shutdown().await?;
        Ok(())
    }
    
    /// Create file with content (ensures parent dirs exist)
    pub async fn create_file(&self, path: &str, content: &[u8]) -> Result<()> {
        // Create parent directories if needed
        if let Some(parent) = std::path::Path::new(path).parent() {
            let parent_str = parent.to_string_lossy();
            if !parent_str.is_empty() && parent_str != "/" {
                self.create_dir_all(&parent_str).await?;
            }
        }
        
        self.write_file(path, content).await
    }
    
    /// Create directory recursively
    pub async fn create_dir_all(&self, path: &str) -> Result<()> {
        // Try to create, ignore if exists
        match self.sftp.create_dir(path).await {
            Ok(_) => Ok(()),
            Err(e) => {
                // Check if already exists
                if self.exists(path).await {
                    Ok(())
                } else {
                    Err(anyhow!("Failed to create directory: {}", e))
                }
            }
        }
    }
    
    /// Check if path exists
    pub async fn exists(&self, path: &str) -> bool {
        self.sftp.metadata(path).await.is_ok()
    }
    
    /// Check if path is directory
    pub async fn is_directory(&self, path: &str) -> bool {
        match self.sftp.metadata(path).await {
            Ok(metadata) => metadata.is_dir(),
            Err(_) => false,
        }
    }
    
    /// Get canonical absolute path
    pub async fn canonicalize(&self, path: &str) -> Result<String> {
        self.sftp.canonicalize(path).await
            .map_err(|e| anyhow!("Failed to canonicalize path: {}", e))
    }
    
    /// List directory contents
    pub async fn read_dir(&self, path: &str) -> Result<Vec<DirEntry>> {
        let mut entries = Vec::new();
        let mut dir = self.sftp.read_dir(path).await?;
        
        while let Some(entry) = dir.next().await {
            let entry = entry?;
            entries.push(DirEntry {
                name: entry.file_name().to_string_lossy().to_string(),
                path: format!("{}/{}", path.trim_end_matches('/'), entry.file_name().to_string_lossy()),
                is_directory: entry.file_type().is_dir(),
            });
        }
        
        Ok(entries)
    }
    
    /// Execute command via SSH
    pub async fn execute_command(
        &self,
        command: &str,
        timeout_secs: Option<u64>,
    ) -> Result<CommandOutput> {
        let channel = self.session.channel_open_session().await?;
        channel.exec(true, command).await?;
        
        let timeout = timeout_secs.map(Duration::from_secs);
        let start = Instant::now();
        
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut exit_code = None;
        
        // Read output with optional timeout
        loop {
            if let Some(timeout) = timeout {
                if start.elapsed() > timeout {
                    channel.close().await?;
                    return Err(anyhow!("Command timed out after {:?}", timeout));
                }
            }
            
            // Use tokio::select! for timeout handling
            tokio::select! {
                msg = channel.wait() => {
                    match msg {
                        Some(russh::ChannelMsg::Data { data }) => {
                            stdout.extend_from_slice(&data);
                        }
                        Some(russh::ChannelMsg::ExtendedData { data, ext }) if ext == 1 => {
                            stderr.extend_from_slice(&data);
                        }
                        Some(russh::ChannelMsg::ExitStatus { exit_status }) => {
                            exit_code = Some(exit_status);
                            break;
                        }
                        None => break,
                        _ => {}
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(100)) => {
                    // Check timeout
                }
            }
        }
        
        channel.close().await?;
        
        Ok(CommandOutput {
            stdout: String::from_utf8_lossy(&stdout).to_string(),
            stderr: String::from_utf8_lossy(&stderr).to_string(),
            exit_code: exit_code.unwrap_or(-1),
        })
    }
}

/// SSH client handler (accepts all server keys)
pub struct SSHClient;

impl client::Handler for SSHClient {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::PublicKey,
    ) -> Result<bool, Self::Error> {
        // Accept all server keys (for simplified usage)
        // In production, implement host key verification
        Ok(true)
    }
}
```

### 3. RemoteConnectionManager

Connection pool for reusing SSH sessions:

```rust
use std::collections::HashMap;
use std::sync::Mutex;

pub struct RemoteConnectionManager {
    connections: Mutex<HashMap<String, Arc<RemoteConnection>>>,
}

impl RemoteConnectionManager {
    pub fn new() -> Self {
        Self {
            connections: Mutex::new(HashMap::new()),
        }
    }
    
    /// Get or create connection for the given connection info
    pub async fn get_connection(
        &self,
        info: &RemoteConnectionInfo,
    ) -> Result<Arc<RemoteConnection>> {
        let key = format!(
            "{}@{}:{}",
            info.user,
            info.host,
            info.port.unwrap_or(22)
        );
        
        // Try to get existing connection
        {
            let connections = self.connections.lock().unwrap();
            if let Some(conn) = connections.get(&key) {
                // Verify connection is still alive
                if conn.sftp.metadata("/").await.is_ok() {
                    return Ok(conn.clone());
                }
            }
        }
        
        // Create new connection
        let connection = Arc::new(RemoteConnection::new(info.clone()).await?);
        
        {
            let mut connections = self.connections.lock().unwrap();
            connections.insert(key, connection.clone());
        }
        
        Ok(connection)
    }
    
    /// Remove a connection from the pool
    pub fn remove_connection(&self, connection_string: &str) {
        let mut connections = self.connections.lock().unwrap();
        connections.remove(connection_string);
    }
    
    /// List active connections
    pub fn list_connections(&self) -> Vec<String> {
        let connections = self.connections.lock().unwrap();
        connections.keys().cloned().collect()
    }
}
```

---

## MCP Tool Implementations

### Path Parsing Utilities

```rust
/// Enum for local vs remote paths
pub enum PathLocation {
    Local(String),
    Remote {
        connection: RemoteConnectionInfo,
        path: String,
    },
}

impl PathLocation {
    /// Parse path string into location
    /// 
    /// Supported formats:
    /// - Local: `/path/to/file`, `relative/path`, `./file`
    /// - Remote SCP: `user@host:/path`
    /// - Remote with port: `user@host#port:/path`
    /// - SSH URL: `ssh://user@host:port/path`
    pub fn parse(path: &str) -> Result<Self> {
        // SSH URL format: ssh://user@host:port/path
        if path.starts_with("ssh://") {
            let without_prefix = &path[6..]; // Remove "ssh://"
            let (auth, remote_path) = without_prefix.split_once('/')
                .ok_or_else(|| anyhow!("Invalid SSH URL format"))?;
            
            let (user_host, port) = if let Some(colon_idx) = auth.rfind(':') {
                let port_str = &auth[colon_idx + 1..];
                let port = port_str.parse::<u16>()
                    .map_err(|_| anyhow!("Invalid port in SSH URL"))?;
                (auth[..colon_idx].to_string(), Some(port))
            } else {
                (auth.to_string(), None)
            };
            
            return Ok(PathLocation::Remote {
                connection: RemoteConnectionInfo {
                    connection_string: user_host,
                    password: None,
                    private_key_path: None,
                },
                path: format!("/{})", remote_path),
            });
        }
        
        // SCP format: user@host:/path or user@host#port:/path
        if path.contains('@') && (path.contains(":/") || path.contains("#")) {
            let (connection_str, remote_path) = if let Some(idx) = path.find(":/") {
                (path[..idx].to_string(), path[idx + 1..].to_string())
            } else if let Some(idx) = path.find("#") {
                // Handle port notation: user@host#port:/path
                let colon_idx = path.find(":/")
                    .ok_or_else(|| anyhow!("Invalid remote path format"))?;
                (path[..colon_idx].to_string(), path[colon_idx + 1..].to_string())
            } else {
                return Err(anyhow!("Invalid remote path format"));
            };
            
            return Ok(PathLocation::Remote {
                connection: RemoteConnectionInfo {
                    connection_string: connection_str,
                    password: None,
                    private_key_path: None,
                },
                path: remote_path,
            });
        }
        
        // Local path
        Ok(PathLocation::Local(path.to_string()))
    }
    
    /// Check if string is a remote path
    pub fn is_remote_path(path: &str) -> bool {
        path.starts_with("ssh://") || 
        (path.contains('@') && (path.contains(":/") || path.contains("#")))
    }
}
```

### 1. View Tool

```rust
/// View file or directory contents
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ViewParams {
    /// Path to view. For remote: user@host:/path or user@host#port:/path
    pub path: String,
    /// Optional line range to view (e.g., "10-20")
    pub view_range: Option<String>,
    /// Optional grep pattern to filter
    pub grep: Option<String>,
    /// Optional glob pattern for directory filtering
    pub glob: Option<String>,
    /// Show as tree structure
    pub tree: Option<bool>,
    /// Max depth for tree view
    pub depth: Option<usize>,
    /// Password for remote connection
    pub password: Option<String>,
    /// Path to private key for remote connection
    pub private_key_path: Option<String>,
}

pub async fn view(&self, params: ViewParams) -> Result<ToolResult> {
    let path = params.path;
    
    if PathLocation::is_remote_path(&path) {
        // Remote path handling
        let (conn, remote_path) = self.get_remote_connection(&path, params.password, params.private_key_path).await?;
        
        // Check if file or directory
        if conn.is_directory(&remote_path).await {
            // Directory listing
            let entries = conn.read_dir(&remote_path).await?;
            let content = format_directory_listing(entries);
            Ok(ToolResult::success(content))
        } else {
            // File content
            let content = conn.read_file_to_string(&remote_path).await?;
            
            // Apply view range if specified
            let content = if let Some(range) = params.view_range {
                apply_line_range(&content, &range)?
            } else {
                content
            };
            
            // Apply grep filter if specified
            let content = if let Some(pattern) = params.grep {
                grep_content(&content, &pattern)?
            } else {
                content
            };
            
            Ok(ToolResult::success(format_file_with_line_numbers(&content)))
        }
    } else {
        // Local path handling (standard file operations)
        // ...
    }
}

/// Get remote connection helper
async fn get_remote_connection(
    &self,
    path: &str,
    password: Option<String>,
    private_key_path: Option<String>,
) -> Result<(Arc<RemoteConnection>, String)> {
    let location = PathLocation::parse(path)?;
    
    match location {
        PathLocation::Remote { mut connection, path: remote_path } => {
            // Apply credentials
            connection.password = password;
            connection.private_key_path = private_key_path;
            
            let conn = self.connection_manager.get_connection(&connection).await?;
            Ok((conn, remote_path))
        }
        PathLocation::Local(_) => Err(anyhow!("Expected remote path")),
    }
}
```

### 2. String Replace Tool (Edit)

```rust
/// Replace text in file
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct StrReplaceParams {
    /// Path to file. For remote: user@host:/path
    pub path: String,
    /// Old string to replace
    pub old_str: String,
    /// New string to insert
    pub new_str: String,
    /// Replace all occurrences
    pub replace_all: Option<bool>,
    /// Password for remote connection
    pub password: Option<String>,
    /// Path to private key
    pub private_key_path: Option<String>,
}

pub async fn str_replace(&self, params: StrReplaceParams) -> Result<ToolResult> {
    let path = params.path;
    let old_str = params.old_str;
    let new_str = params.new_str;
    let replace_all = params.replace_all.unwrap_or(false);
    
    if PathLocation::is_remote_path(&path) {
        let (conn, remote_path) = self.get_remote_connection(&path, params.password, params.private_key_path).await?;
        
        // Read file content
        let content = conn.read_file_to_string(&remote_path).await?;
        
        // Perform replacement
        let new_content = if replace_all {
            content.replace(&old_str, &new_str)
        } else {
            content.replacen(&old_str, &new_str, 1)
        };
        
        // Verify replacement happened
        if new_content == content {
            return Err(anyhow!("Old string not found in file"));
        }
        
        // Write back
        conn.write_file(&remote_path, new_content.as_bytes()).await?;
        
        // Generate diff for output
        let diff = create_unified_diff(&remote_path, &content, &new_content);
        
        Ok(ToolResult::success(format!(
            "Successfully replaced text. Diff:\n```diff\n{}\n```",
            diff
        )))
    } else {
        // Local file handling
        // ...
    }
}

/// Create unified diff for display
fn create_unified_diff(path: &str, old_content: &str, new_content: &str) -> String {
    use similar::{ChangeTag, TextDiff};
    
    let diff = TextDiff::from_lines(old_content, new_content);
    let mut output = String::new();
    
    for change in diff.iter_all_changes() {
        let sign = match change.tag() {
            ChangeTag::Delete => "-",
            ChangeTag::Insert => "+",
            ChangeTag::Equal => " ",
        };
        output.push_str(&format!("{}{}", sign, change));
    }
    
    output
}
```

### 3. Create Tool

```rust
/// Create new file
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CreateParams {
    /// Path for new file. For remote: user@host:/path
    pub path: String,
    /// File content
    pub file_text: String,
    /// Password for remote connection
    pub password: Option<String>,
    /// Path to private key
    pub private_key_path: Option<String>,
}

pub async fn create(&self, params: CreateParams) -> Result<ToolResult> {
    let path = params.path;
    
    if PathLocation::is_remote_path(&path) {
        let (conn, remote_path) = self.get_remote_connection(&path, params.password, params.private_key_path).await?;
        
        // Check if file already exists
        if conn.exists(&remote_path).await {
            return Err(anyhow!("File already exists: {}", path));
        }
        
        // Create file with auto-directory creation
        conn.create_file(&remote_path, params.file_text.as_bytes()).await?;
        
        Ok(ToolResult::success(format!(
            "Created file: {} ({} bytes)",
            path,
            params.file_text.len()
        )))
    } else {
        // Local file creation
        // ...
    }
}
```

### 4. Remove Tool (with Backup)

```rust
/// Remove file or directory
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RemoveParams {
    /// Path to remove. For remote: user@host:/path
    pub path: String,
    /// Recursive removal for directories
    pub recursive: Option<bool>,
    /// Password for remote connection
    pub password: Option<String>,
    /// Path to private key
    pub private_key_path: Option<String>,
}

pub async fn remove(&self, params: RemoveParams) -> Result<ToolResult> {
    let path = params.path;
    let recursive = params.recursive.unwrap_or(false);
    
    if PathLocation::is_remote_path(&path) {
        let (conn, remote_path) = self.get_remote_connection(&path, params.password, params.private_key_path).await?;
        
        // Check existence
        if !conn.exists(&remote_path).await {
            return Err(anyhow!("Path does not exist: {}", path));
        }
        
        // Check if directory
        let is_dir = conn.is_directory(&remote_path).await;
        if is_dir && !recursive {
            return Err(anyhow!("Path is a directory. Use recursive=true to remove."));
        }
        
        // Create backup before removal
        let backup_path = self.create_remote_backup(&conn, &remote_path).await?;
        
        // Perform removal
        if is_dir {
            conn.remove_dir_recursive(&remote_path).await?;
        } else {
            conn.remove_file(&remote_path).await?;
        }
        
        Ok(ToolResult::success(format!(
            "Removed: {}\nBackup created at: {}",
            path, backup_path
        )))
    } else {
        // Local removal with backup
        // ...
    }
}

async fn create_remote_backup(
    &self,
    conn: &Arc<RemoteConnection>,
    path: &str,
) -> Result<String> {
    use chrono::Utc;
    use uuid::Uuid;
    
    let timestamp = Utc::now().format("%Y%m%d_%H%M%S");
    let uuid = Uuid::new_v4().to_string()[..8].to_string();
    let filename = std::path::Path::new(path)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    
    let backup_dir = format!("~/.stakpak/backups/{}", uuid);
    let backup_path = format!("{}/{}_{}", backup_dir, timestamp, filename);
    
    // Create backup directory
    conn.create_dir_all(&backup_dir).await?;
    
    // Move original to backup
    conn.rename(path, &backup_path).await?;
    
    Ok(backup_path)
}
```

---

## Integration with MCP Server

### Tool Registration

```rust
use mcp_sdk::server::{Server, ServerBuilder};
use mcp_sdk::types::{Tool, TextContent};

pub struct RemoteFileTools {
    connection_manager: Arc<RemoteConnectionManager>,
}

impl RemoteFileTools {
    pub fn new() -> Self {
        Self {
            connection_manager: Arc::new(RemoteConnectionManager::new()),
        }
    }
    
    pub fn register_tools(&self, builder: ServerBuilder) -> ServerBuilder {
        let cm = self.connection_manager.clone();
        
        builder
            .register_tool("view", view_schema(), move |params| {
                let cm = cm.clone();
                async move { view_tool(cm, params).await }
            })
            .register_tool("str_replace", str_replace_schema(), move |params| {
                let cm = cm.clone();
                async move { str_replace_tool(cm, params).await }
            })
            .register_tool("create", create_schema(), move |params| {
                let cm = cm.clone();
                async move { create_tool(cm, params).await }
            })
            .register_tool("remove", remove_schema(), move |params| {
                let cm = cm.clone();
                async move { remove_tool(cm, params).await }
            })
    }
}

fn view_schema() -> Tool {
    Tool {
        name: "view".to_string(),
        description: Some(
            "View file or directory contents. Supports remote paths: user@host:/path".to_string()
        ),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to view. For remote: user@host:/path or user@host#port:/path"
                },
                "view_range": {
                    "type": "string",
                    "description": "Optional line range (e.g., '10-20')"
                },
                "grep": {
                    "type": "string", 
                    "description": "Optional grep pattern"
                },
                "password": {
                    "type": "string",
                    "description": "Password for remote connection"
                },
                "private_key_path": {
                    "type": "string",
                    "description": "Path to SSH private key"
                }
            },
            "required": ["path"]
        }),
    }
}
```

---

## Dependencies

Add to your `Cargo.toml`:

```toml
[dependencies]
# SSH/SFTP
russh = "0.53.0"
russh-sftp = "2.1.1"

# For SSH key discovery
dirs = "5.0"

# For diff generation (optional)
similar = "2.4"

# For backup timestamps
chrono = "0.4"

# For backup UUIDs
uuid = { version = "1.0", features = ["v4"] }

# Async runtime
tokio = { version = "1.0", features = ["full"] }

# Error handling
anyhow = "1.0"
thiserror = "1.0"

# Serialization
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
schemars = "0.8"

# MCP SDK (adjust to your implementation)
mcp-sdk = "0.1"
```

---

## Security Considerations

1. **Host Key Verification**: Current implementation accepts all server keys. For production:
   ```rust
   // Store known hosts in ~/.ssh/known_hosts
   async fn check_server_key(&mut self, key: &PublicKey) -> Result<bool> {
       verify_known_host(&self.host, key)
   }
   ```

2. **Credential Handling**: Never log passwords or private key contents

3. **Path Traversal**: Validate paths to prevent `../../../etc/passwd` attacks
   ```rust
   fn sanitize_path(path: &str) -> Result<String> {
       let path = std::path::Path::new(path);
       if path.components().any(|c| matches!(c, Component::ParentDir)) {
           return Err(anyhow!("Path traversal detected"));
       }
       Ok(path.to_string_lossy().to_string())
   }
   ```

4. **Backup Retention**: Implement cleanup for old backups

5. **Command Injection**: Use SSH exec channel which avoids shell interpretation

---

## Testing

```rust
#[cfg(test)]
mod tests {
    use super::*;
    
    #[tokio::test]
    async fn test_remote_file_operations() {
        let info = RemoteConnectionInfo {
            connection_string: "test@localhost:2222".to_string(),
            password: Some("test".to_string()),
            private_key_path: None,
        };
        
        let conn = RemoteConnection::new(info).await.unwrap();
        
        // Test write and read
        conn.write_file("/tmp/test.txt", b"Hello, World!").await.unwrap();
        let content = conn.read_file_to_string("/tmp/test.txt").await.unwrap();
        assert_eq!(content, "Hello, World!");
        
        // Cleanup
        conn.remove_file("/tmp/test.txt").await.unwrap();
    }
    
    #[test]
    fn test_path_parsing() {
        // SSH URL
        let loc = PathLocation::parse("ssh://user@host:2222/path/to/file").unwrap();
        match loc {
            PathLocation::Remote { connection, path } => {
                assert_eq!(connection.connection_string, "user@host:2222");
                assert_eq!(path, "/path/to/file");
            }
            _ => panic!("Expected remote path"),
        }
        
        // SCP format
        let loc = PathLocation::parse("user@host:/path/to/file").unwrap();
        match loc {
            PathLocation::Remote { connection, path } => {
                assert_eq!(connection.connection_string, "user@host");
                assert_eq!(path, "/path/to/file");
            }
            _ => panic!("Expected remote path"),
        }
        
        // Local path
        let loc = PathLocation::parse("/local/path").unwrap();
        match loc {
            PathLocation::Local(path) => assert_eq!(path, "/local/path"),
            _ => panic!("Expected local path"),
        }
    }
}
```

---

## Integration Checklist

To integrate remote file editing into your MCP server:

1. **Add dependencies** to `Cargo.toml`:
   ```toml
   russh = "0.53.0"
   russh-sftp = "2.1.1"
   dirs = "5.0"
   similar = "2.4"      # for diff generation
   chrono = "0.4"       # for backup timestamps
   uuid = "1.0"         # for backup identifiers
   ```

2. **Implement connection management**:
   - `RemoteConnection` struct wrapping SSH + SFTP sessions
   - `RemoteConnectionManager` for connection pooling
   - `RemoteConnectionInfo` for connection parameters

3. **Implement file operations**:
   - `read_file_to_string()` - Read file content
   - `write_file()` - Write file content
   - `create_file()` - Create file with parent directories
   - `remove_file()` / `remove_dir()` - Remove with backup
   - `exists()` / `is_directory()` - Path checks

4. **Implement path parsing**:
   - `PathLocation` enum for local vs remote paths
   - Support formats: `user@host:/path`, `user@host#port:/path`, `ssh://host/path`

5. **Register MCP tools**:
   - `view` - Read files/directories
   - `str_replace` - Edit file content
   - `create` - Create new files
   - `remove` - Delete files with backup

6. **Add authentication handling**:
   - Password auth
   - Private key auth (with auto-discovery from `~/.ssh/`)
   - Credentials passed via tool parameters

---

## External Libraries

- [russh](https://github.com/warp-tech/russh) - Rust SSH client library
- [russh-sftp](https://github.com/AspectUnk/russh-sftp) - SFTP client for russh

---

*Adapt this reference to your specific MCP server architecture.*
