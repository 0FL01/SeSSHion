# File Backup Manager - Implementation Reference

> **Use Case**: Safe file removal with automatic backups for MCP tools  > **Features**: Local and remote backups, UUID-based sessions, atomic moves, XML serialization

---

## Overview

This implementation provides safe file removal operations by automatically creating backups before deletion. Supports both local filesystem and remote SSH/SFTP targets.

### Key Features

| Feature | Description |
|---------|-------------|
| **Atomic Backup** | Uses `rename()` for instant move without copy |
| **UUID Sessions** | Unique backup identifiers for batch operations |
| **Dual Support** | Both local and remote (SSH/SFTP) backups |
| **XML Serialization** | Structured backup metadata for audit trails |
| **Path Preservation** | Original path stored for potential rollback |

---

## Dependencies

```toml
[dependencies]
uuid = { version = "1.0", features = ["v4"] }
serde = { version = "1.0", features = ["derive"] }
```

---

## Core Implementation

### 1. Backup Manager Structure

```rust
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;
use anyhow::{anyhow, Result};

/// Manages file backups for safe removal operations
/// 
/// # Example
/// ```
/// // Backup before removal
/// let backup_path = FileBackupManager::move_to_backup("/etc/nginx/nginx.conf")?;
/// println!("Backed up to: {}", backup_path);
/// 
/// // Later: restore if needed
/// std::fs::rename(&backup_path, "/etc/nginx/nginx.conf")?;
/// ```
pub struct FileBackupManager {
    /// Base directory for local backups
    local_backup_root: PathBuf,
    /// Session ID for grouping related backups
    session_id: String,
    /// Tracks original -> backup mappings
    backup_map: HashMap<String, String>,
}

impl FileBackupManager {
    /// Create new backup manager with unique session
    pub fn new(local_backup_root: PathBuf) -> Self {
        Self {
            local_backup_root,
            session_id: Uuid::new_v4().to_string(),
            backup_map: HashMap::new(),
        }
    }
    
    /// Create with explicit session ID (for restore operations)
    pub fn with_session(local_backup_root: PathBuf, session_id: String) -> Self {
        Self {
            local_backup_root,
            session_id,
            backup_map: HashMap::new(),
        }
    }
    
    /// Get backup directory path for current session
    pub fn session_backup_dir(&self) -> PathBuf {
        self.local_backup_root
            .join("backups")
            .join(&self.session_id)
    }
}
```

---

### 2. Local File Backup

```rust
impl FileBackupManager {
    /// Move local file or directory to backup location
    /// 
    /// # Arguments
    /// * `path` - Path to file or directory to backup
    ///
    /// # Returns
    /// Path to backup location
    ///
    /// # Errors
    /// * Path does not exist
    /// * Cannot create backup directory
    /// * Move operation fails
    pub fn backup_local(&mut self,
        path: &str,
    ) -> Result<String> {
        let path_obj = Path::new(path);
        
        // Verify path exists
        if !path_obj.exists() {
            return Err(anyhow!("Path does not exist: {}", path));
        }
        
        // Create backup directory
        let backup_dir = self.session_backup_dir();
        std::fs::create_dir_all(&backup_dir)
            .map_err(|e| anyhow!("Failed to create backup directory: {}", e))?;
        
        // Get item name (preserve original name)
        let item_name = path_obj
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow!("Invalid path name"))?;
        
        let backup_path = backup_dir.join(item_name);
        
        // Atomic move (instant, no copy for same filesystem)
        std::fs::rename(path_obj, &backup_path)
            .map_err(|e| anyhow!("Failed to move to backup: {}", e))?;
        
        let backup_path_str = backup_path.to_string_lossy().to_string();
        
        // Track mapping
        self.backup_map.insert(
            path.to_string(),
            backup_path_str.clone(),
        );
        
        Ok(backup_path_str)
    }
    
    /// Restore file from backup
    pub fn restore_local(&self,
        original_path: &str,
        backup_path: &str,
    ) -> Result<()> {
        let backup = Path::new(backup_path);
        
        if !backup.exists() {
            return Err(anyhow!("Backup not found: {}", backup_path));
        }
        
        // Ensure parent directory exists
        if let Some(parent) = Path::new(original_path).parent() {
            std::fs::create_dir_all(parent)?;
        }
        
        // Restore via atomic move
        std::fs::rename(backup, original_path)
            .map_err(|e| anyhow!("Failed to restore from backup: {}", e))?;
        
        Ok(())
    }
}
```

---

### 3. Remote File Backup (SSH/SFTP)

```rust
/// Remote connection abstraction (simplified)
pub trait RemoteConnection: Send + Sync {
    async fn rename(&self,
        from: &str,
        to: &str,
    ) -> Result<()>;
    
    async fn create_dir_all(&self,
        path: &str,
    ) -> Result<()>;
    
    async fn exists(&self,
        path: &str,
    ) -> bool;
}

impl FileBackupManager {
    /// Move remote file or directory to backup location
    /// 
    /// # Arguments
    /// * `conn` - Remote connection (SSH/SFTP)
    /// * `path` - Remote path to backup
    /// * `remote_backup_root` - Base backup directory on remote host
    pub async fn backup_remote(
        &mut self,
        conn: &Arc<dyn RemoteConnection>,
        path: &str,
        remote_backup_root: &str,
    ) -> Result<String> {
        // Verify path exists
        if !conn.exists(path).await {
            return Err(anyhow!("Remote path does not exist: {}", path));
        }
        
        // Create backup directory on remote
        let backup_dir = format!(
            "{}/backups/{}",
            remote_backup_root,
            self.session_id
        );
        
        conn.create_dir_all(&backup_dir).await
            .map_err(|e| anyhow!("Failed to create remote backup dir: {}", e))?;
        
        // Get item name
        let item_name = Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");
        
        let backup_path = format!("{}/{}", backup_dir, item_name);
        
        // Atomic move on remote
        conn.rename(path, &backup_path).await
            .map_err(|e| anyhow!("Failed to move remote to backup: {}", e))?;
        
        // Track mapping
        self.backup_map.insert(
            path.to_string(),
            backup_path.clone(),
        );
        
        Ok(backup_path)
    }
    
    /// Restore remote file from backup
    pub async fn restore_remote(
        &self,
        conn: &Arc<dyn RemoteConnection>,
        original_path: &str,
        backup_path: &str,
    ) -> Result<()> {
        if !conn.exists(backup_path).await {
            return Err(anyhow!("Remote backup not found: {}", backup_path));
        }
        
        // Ensure parent exists
        if let Some(parent) = Path::new(original_path).parent() {
            let parent_str = parent.to_string_lossy();
            if !parent_str.is_empty() && parent_str != "/" {
                let _ = conn.create_dir_all(&parent_str).await;
            }
        }
        
        // Restore
        conn.rename(backup_path, original_path).await
            .map_err(|e| anyhow!("Failed to restore remote backup: {}", e))?;
        
        Ok(())
    }
}
```

---

### 4. Backup Serialization (XML)

```rust
impl FileBackupManager {
    /// Serialize backup mappings to XML
    /// 
    /// Format:
    /// ```xml
    /// <file_backups>
    ///   <file
    ///       original_path="/etc/nginx/nginx.conf"
    ///       backup_path="/backups/session-uuid/nginx.conf"
    ///       location="remote"
    ///   />
    /// </file_backups>
    /// ```
    pub fn to_xml(&self,
        location: &str,  // "local" or "remote"
    ) -> String {
        let mut xml = String::from("\n<file_backups>");
        
        for (original, backup) in &self.backup_map {
            xml.push_str(&format!(
                r#"\n  <file
    original_path="{}"
    backup_path="{}"
    location="{}"
  />"#,
                Self::escape_xml(original),
                Self::escape_xml(backup),
                location
            ));
        }
        
        xml.push_str("\n</file_backups>");
        xml
    }
    
    /// Escape XML special characters
    fn escape_xml(text: &str) -> String {
        text
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;")
    }
    
    /// Parse XML back to backup map (for restore)
    pub fn from_xml(xml: &str) -> Result<HashMap<String, String>> {
        // Simple XML parsing - in production use proper XML library
        let mut map = HashMap::new();
        
        // Extract file elements
        for line in xml.lines() {
            let line = line.trim();
            if line.starts_with("<file ") {
                let original = Self::extract_attr(line, "original_path");
                let backup = Self::extract_attr(line, "backup_path");
                if let (Some(orig), Some(back)) = (original, backup) {
                    map.insert(orig, back);
                }
            }
        }
        
        Ok(map)
    }
    
    fn extract_attr(line: &str, attr: &str) -> Option<String> {
        let pattern = format!(r#"{}=""#, attr);
        line.find(&pattern).and_then(|start| {
            let value_start = start + pattern.len();
            line[value_start..].find('"').map(|end| {
                line[value_start..value_start + end].to_string()
            })
        })
    }
}
```

---

## Batch Operations

### Transaction-style Backup

```rust
/// Transaction for batch file operations with rollback support
pub struct BackupTransaction {
    manager: FileBackupManager,
    committed: bool,
}

impl BackupTransaction {
    /// Start new transaction
    pub fn new(backup_root: PathBuf) -> Self {
        Self {
            manager: FileBackupManager::new(backup_root),
            committed: false,
        }
    }
    
    /// Backup multiple paths atomically
    pub fn backup_batch(&mut self,
        paths: &[&str],
    ) -> Result<Vec<String>> {
        let mut backup_paths = Vec::new();
        
        for path in paths {
            match self.manager.backup_local(path) {
                Ok(backup) => backup_paths.push(backup),
                Err(e) => {
                    // Rollback on failure
                    self.rollback()?;
                    return Err(anyhow!(
                        "Backup failed for '{}': {}. Rolled back {} backups.",
                        path, e, backup_paths.len()
                    ));
                }
            }
        }
        
        Ok(backup_paths)
    }
    
    /// Commit transaction (prevent auto-rollback)
    pub fn commit(mut self) {
        self.committed = true;
    }
    
    /// Rollback all backups in this transaction
    pub fn rollback(&self) -> Result<()> {
        for (original, backup) in &self.manager.backup_map {
            if let Err(e) = self.manager.restore_local(original, backup) {
                eprintln!("Warning: Failed to restore {}: {}", original, e);
            }
        }
        Ok(())
    }
}

impl Drop for BackupTransaction {
    fn drop(&mut self) {
        if !self.committed && !self.manager.backup_map.is_empty() {
            println!("Auto-rolling back {} backups...", self.manager.backup_map.len());
            let _ = self.rollback();
        }
    }
}
```

---

## MCP Tool Integration

### Remove Tool with Backup

```rust
use mcp_sdk::types::{Tool, CallToolResult, Content};
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RemoveParams {
    /// Path to remove
    pub path: String,
    /// Recursive removal for directories
    #[serde(default)]
    pub recursive: bool,
    /// Skip backup (dangerous!)
    #[serde(default)]
    pub no_backup: bool,
}

pub fn remove_tool() -> Tool {
    Tool {
        name: "remove".to_string(),
        description: Some(
            "Remove file or directory with automatic backup. \
             Backups can be restored using the restore tool.".to_string()
        ),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "recursive": { "type": "boolean", "default": false },
                "no_backup": { "type": "boolean", "default": false }
            },
            "required": ["path"]
        }),
    }
}

pub async fn handle_remove(
    params: RemoveParams,
    backup_root: PathBuf,
) -> CallToolResult {
    let path = Path::new(&params.path);
    
    // Verify path exists
    if !path.exists() {
        return CallToolResult::error(vec![
            Content::text(format!("Path does not exist: {}", params.path))
        ]);
    }
    
    // Check if directory
    let is_dir = path.is_dir();
    if is_dir && !params.recursive {
        return CallToolResult::error(vec![
            Content::text(
                "Path is a directory. Use recursive=true to remove.".to_string()
            )
        ]);
    }
    
    // Create backup unless skipped
    let backup_info = if !params.no_backup {
        let mut manager = FileBackupManager::new(backup_root);
        match manager.backup_local(&params.path) {
            Ok(backup_path) => {
                Some((backup_path, manager.to_xml("local")))
            }
            Err(e) => {
                return CallToolResult::error(vec![
                    Content::text(format!("Failed to create backup: {}", e))
                ]);
            }
        }
    } else {
        None
    };
    
    // Perform removal
    let result = if is_dir {
        std::fs::remove_dir_all(&params.path)
    } else {
        std::fs::remove_file(&params.path)
    };
    
    if let Err(e) = result {
        return CallToolResult::error(vec![
            Content::text(format!("Failed to remove: {}", e))
        ]);
    }
    
    // Build success response
    let mut response = format!("Successfully removed: {}", params.path);
    
    if let Some((backup_path, xml)) = backup_info {
        response.push_str(&format!(
            "\n\nBackup created at: {}\n\nBackup metadata:\n```xml\n{}\n```",
            backup_path,
            xml
        ));
    }
    
    CallToolResult::success(vec![Content::text(response)])
}
```

### Restore Tool

```rust
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RestoreParams {
    /// Original path where file should be restored
    pub original_path: String,
    /// Backup path (from previous remove operation)
    pub backup_path: String,
}

pub fn restore_tool() -> Tool {
    Tool {
        name: "restore".to_string(),
        description: Some(
            "Restore file from backup created by remove tool".to_string()
        ),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "original_path": { "type": "string" },
                "backup_path": { "type": "string" }
            },
            "required": ["original_path", "backup_path"]
        }),
    }
}

pub fn handle_restore(params: RestoreParams) -> CallToolResult {
    let manager = FileBackupManager::new(PathBuf::from("/tmp")); // root not used for restore
    
    match manager.restore_local(&params.original_path,
        &params.backup_path,
    ) {
        Ok(_) => CallToolResult::success(vec![
            Content::text(format!(
                "Restored {} from {}",
                params.original_path,
                params.backup_path
            ))
        ]),
        Err(e) => CallToolResult::error(vec![
            Content::text(format!("Failed to restore: {}", e))
        ]),
    }
}
```

---

## Testing

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    
    #[test]
    fn test_local_backup_and_restore() {
        let temp_dir = TempDir::new().unwrap();
        let backup_root = temp_dir.path().join("backups");
        
        // Create test file
        let test_file = temp_dir.path().join("test.txt");
        std::fs::write(&test_file, "original content").unwrap();
        
        // Backup
        let mut manager = FileBackupManager::new(backup_root.clone());
        let backup_path = manager.backup_local(
            test_file.to_str().unwrap()
        ).unwrap();
        
        // Verify file moved to backup
        assert!(!test_file.exists());
        assert!(Path::new(&backup_path).exists());
        
        // Restore
        manager.restore_local(
            test_file.to_str().unwrap(),
            &backup_path,
        ).unwrap();
        
        // Verify restored
        assert!(test_file.exists());
        assert!(!Path::new(&backup_path).exists());
        
        let content = std::fs::read_to_string(&test_file).unwrap();
        assert_eq!(content, "original content");
    }
    
    #[test]
    fn test_xml_serialization() {
        let temp_dir = TempDir::new().unwrap();
        let mut manager = FileBackupManager::new(temp_dir.path().to_path_buf());
        
        // Create mock backup
        manager.backup_map.insert(
            "/etc/config.conf".to_string(),
            "/backups/uuid/config.conf".to_string(),
        );
        
        let xml = manager.to_xml("local");
        
        assert!(xml.contains("<file_backups>"));
        assert!(xml.contains("original_path="));
        assert!(xml.contains("/etc/config.conf"));
        assert!(xml.contains("location=\"local\""));
        
        // Parse back
        let map = FileBackupManager::from_xml(&xml).unwrap();
        assert_eq!(map.len(), 1);
        assert_eq!(
            map.get("/etc/config.conf"),
            Some(&"/backups/uuid/config.conf".to_string())
        );
    }
    
    #[test]
    fn test_transaction_rollback() {
        let temp_dir = TempDir::new().unwrap();
        let backup_root = temp_dir.path().join("backups");
        
        // Create files
        let file1 = temp_dir.path().join("file1.txt");
        let file2 = temp_dir.path().join("file2.txt");
        std::fs::write(&file1, "content1").unwrap();
        std::fs::write(&file2, "content2").unwrap();
        
        // Start transaction and backup first file
        {
            let mut tx = BackupTransaction::new(backup_root.clone());
            tx.backup_batch(&[file1.to_str().unwrap()]).unwrap();
            
            // Transaction dropped without commit - should auto-rollback
        }
        
        // Verify file1 restored
        assert!(file1.exists());
    }
    
    #[test]
    fn test_xml_escaping() {
        let input = "Special <chars> \"quoted\" & 'apostrophe'";
        let escaped = FileBackupManager::escape_xml(input);
        
        assert_eq!(
            escaped,
            "Special &lt;chars&gt; &quot;quoted&quot; &amp; &apos;apostrophe&apos;"
        );
    }
}
```

---

## Directory Structure

```
~/.mcp/backups/
└── {session-uuid}/
    ├── file1.conf          # backed up file
    ├── file2.conf
    └── directory/          # backed up directory (moved atomically)
        └── contents...
```

---

## Integration Checklist

1. **Dependencies**: Add `uuid` and create backup root directory
2. **Storage**: Decide on backup location (`~/.mcp/backups/` or `/var/backups/`)
3. **Retention**: Implement cleanup for old backups (cron/scheduled task)
4. **Permissions**: Ensure backup directory has proper permissions (700)
5. **Monitoring**: Track backup sizes to prevent disk fill

---

## External Libraries

- [uuid](https://github.com/uuid-rs/uuid) - UUID generation

---

*Adapt this reference to your MCP server architecture.*
