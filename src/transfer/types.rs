use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TransferOperation {
    Put,
    Get,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TransferTransport {
    Auto,
    ExecRaw,
    Sftp,
    Scp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TransferKind {
    File,
    Directory,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferParams {
    pub operation: TransferOperation,

    /// Local path.
    ///
    /// - For `put`: local source path (file or directory; must be within local_root, relative-only)
    /// - For `get`: local destination path (must be within local_root, relative-only)
    pub local_path: String,

    /// Remote path.
    ///
    /// - For `put`: remote destination path
    /// - For `get`: remote source path
    pub remote_path: String,

    #[serde(default = "default_transport")]
    pub transport: TransferTransport,

    /// Optional explicit kind. If omitted, the server auto-detects.
    pub kind: Option<TransferKind>,

    /// Whether overwriting an existing destination is allowed.
    ///
    /// Note: For file transfers, `overwrite=false` relies on creating a hard-link to install the
    /// final file without replacement. This requires hard-link support on the destination
    /// filesystem:
    /// - `put` (local -> remote): requires hard-link support on the remote filesystem
    /// - `get` (remote -> local): requires hard-link support on the local filesystem
    #[serde(default = "default_overwrite")]
    pub overwrite: bool,

    /// Optional timeout override for this transfer.
    pub timeout_ms: Option<u64>,

    /// When true, return full diagnostic response including staging details.
    /// When false or omitted, return compact response with only essential fields.
    #[serde(default)]
    pub verbose: bool,
}

fn default_transport() -> TransferTransport {
    TransferTransport::Auto
}

fn default_overwrite() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedPaths {
    pub local_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferCounts {
    pub bytes: u64,
    pub files: u64,
    pub directories: u64,
}

impl TransferCounts {
    pub fn zero() -> Self {
        Self {
            bytes: 0,
            files: 0,
            directories: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferStaging {
    pub local: Option<StagingLocal>,
    pub remote: Option<StagingRemote>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StagingLocal {
    pub staging_path: String,
    pub backup_path: Option<String>,
    pub final_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StagingRemote {
    pub staging_path: String,
    pub backup_path: Option<String>,
    pub final_path: String,
    pub staging_base_home: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferResponse {
    pub ok: bool,
    pub error: Option<String>,

    pub params: TransferParams,
    pub kind: Option<TransferKind>,

    pub transport_used: TransferTransport,
    pub remote_home: Option<String>,
    pub local_root: String,

    pub resolved_paths: Option<ResolvedPaths>,
    pub staging: Option<TransferStaging>,
    pub counts: Option<TransferCounts>,

    pub elapsed_ms: Option<u64>,
    pub semantics: Option<String>,
}

impl TransferResponse {
    pub fn ok_stub(
        params: TransferParams,
        transport_used: TransferTransport,
        remote_home: &str,
        local_root: &Path,
    ) -> Self {
        Self {
            ok: false,
            error: None,
            params,
            kind: None,
            transport_used,
            remote_home: Some(remote_home.to_string()),
            local_root: local_root.display().to_string(),
            resolved_paths: None,
            staging: None,
            counts: None,
            elapsed_ms: None,
            semantics: None,
        }
    }

    pub fn error(params: TransferParams, local_root: &Path, msg: &str) -> Self {
        Self {
            ok: false,
            error: Some(msg.to_string()),
            transport_used: params.transport,
            remote_home: None,
            local_root: local_root.display().to_string(),
            params,
            kind: None,
            resolved_paths: None,
            staging: None,
            counts: None,
            elapsed_ms: None,
            semantics: None,
        }
    }

    pub fn set_error(&mut self, msg: &str) {
        self.ok = false;
        self.error = Some(msg.to_string());
    }
}

/// Compact transfer response for non-verbose mode.
/// Contains only essential fields that agents need.
#[derive(Debug, Clone, Serialize)]
pub struct CompactTransferResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub kind: Option<TransferKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub counts: Option<TransferCounts>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
}

impl TransferResponse {
    /// Convert to compact representation for non-verbose responses.
    pub fn to_compact(&self) -> CompactTransferResponse {
        CompactTransferResponse {
            ok: self.ok,
            error: self.error.clone(),
            kind: self.kind,
            counts: self.counts.clone(),
            elapsed_ms: self.elapsed_ms,
        }
    }

    /// Serialize to JSON, using compact format unless verbose is true.
    pub fn to_json(&self, verbose: bool) -> Result<String, serde_json::Error> {
        if verbose {
            serde_json::to_string(self)
        } else {
            serde_json::to_string(&self.to_compact())
        }
    }
}
