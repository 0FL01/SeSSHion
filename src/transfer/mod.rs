//! File and directory transfer tool support.
//!
//! Transports:
//! - `sftp`: OpenSSH `sftp` client (batch mode)
//! - `scp`: OpenSSH `scp` client
//! - `exec-raw`: stdin/stdout streaming over the existing SSH session
//! - `auto`: fallback chain `sftp -> scp -> exec-raw`

mod exec_raw;
mod local_root;
mod openssh;
mod process;
mod rsync;
mod skeleton;
mod staging;
mod tar;
mod types;
mod walk;

pub use types::{
    CompactTransferResponse, ResolvedPaths, RsyncOptions, StagingLocal, StagingRemote,
    TransferCounts, TransferKind, TransferOperation, TransferParams, TransferResponse,
    TransferStaging, TransferTransport,
};

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::time::Instant;

use crate::error::{Result, SshMcpError};
use crate::ssh::SshConnectionManager;

fn io_to_transport_attempt(err: std::io::Error) -> TransportAttemptError {
    TransportAttemptError::Other(SshMcpError::Io(err))
}

struct StepCtx<'a> {
    conn: &'a SshConnectionManager,
    remote_home: &'a str,
    id: u64,
    kind: TransferKind,
    resolved: &'a ResolvedPaths,
    timeout: Duration,
    response: &'a mut TransferResponse,
}

struct OpenSshContext<'a> {
    conn: &'a SshConnectionManager,
    remote_home: &'a str,
    key_path: Option<&'a Path>,
    ssh: &'a TransferSshOptions,
    id: u64,
    timeout: Duration,
}

struct OpenSshOperation<'a> {
    transport: openssh::OpenSshTransport,
    kind: TransferKind,
    response: &'a mut TransferResponse,
}

/// Core transfer engine.
///
/// For now this selects the EXEC-RAW transport, but it is structured so that
/// SFTP/SCP can be added as additional implementations.
#[derive(Clone, Debug)]
pub struct TransferEngine {
    local_root: Arc<PathBuf>,
    counter: Arc<AtomicU64>,
}

#[derive(Clone, Debug)]
pub struct TransferRunContext {
    pub timeout: Duration,
    pub ssh: TransferSshOptions,
}

#[derive(Clone, Debug)]
pub struct TransferSshOptions {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub key_path: Option<PathBuf>,
}

impl TransferEngine {
    pub fn new(local_root: PathBuf) -> Self {
        Self {
            local_root: Arc::new(local_root),
            counter: Arc::new(AtomicU64::new(1)),
        }
    }

    pub fn local_root(&self) -> &Path {
        self.local_root.as_path()
    }

    fn next_id(&self) -> u64 {
        self.counter.fetch_add(1, Ordering::Relaxed)
    }

    pub async fn run(
        &self,
        conn: &SshConnectionManager,
        params: TransferParams,
        ctx: TransferRunContext,
    ) -> TransferResponse {
        let key_path_opt = ctx.ssh.key_path.clone();

        let started_at = Instant::now();
        let id = self.next_id();

        let remote_home = match exec_raw::resolve_remote_home(conn, ctx.timeout).await {
            Ok(home) => home,
            Err(e) => {
                return TransferResponse::error(
                    params,
                    self.local_root(),
                    &format!("failed to resolve remote HOME: {e}"),
                );
            }
        };

        let mut response = TransferResponse::ok_stub(
            params,
            TransferTransport::ExecRaw,
            &remote_home,
            self.local_root(),
        );

        let kind = match resolve_kind(conn, self.local_root(), &response.params, ctx.timeout).await
        {
            Ok(kind) => kind,
            Err(e) => {
                response.set_error(&format!("failed to resolve transfer kind: {e}"));
                response.elapsed_ms = Some(started_at.elapsed().as_millis() as u64);
                return response;
            }
        };
        response.kind = Some(kind);

        let transports = match response.params.transport {
            TransferTransport::Auto => {
                vec![
                    TransferTransport::Rsync,   // Try rsync first (most efficient)
                    TransferTransport::Sftp,    // Fallback to sftp
                    TransferTransport::Scp,     // Fallback to scp
                    TransferTransport::ExecRaw, // Last resort
                ]
            }
            other => vec![other],
        };

        let mut unsupported_reasons: Vec<String> = Vec::new();
        let mut failed_reasons: Vec<String> = Vec::new();

        for transport in transports {
            response.transport_used = transport;
            let attempt = match transport {
                TransferTransport::ExecRaw => self
                    .run_exec_raw(conn, &remote_home, id, kind, ctx.timeout, &mut response)
                    .await
                    .map_err(TransportAttemptError::Other),
                TransferTransport::Sftp => {
                    self.run_openssh(
                        OpenSshContext {
                            conn,
                            remote_home: &remote_home,
                            key_path: key_path_opt.as_deref(),
                            ssh: &ctx.ssh,
                            id,
                            timeout: ctx.timeout,
                        },
                        OpenSshOperation {
                            transport: openssh::OpenSshTransport::Sftp,
                            kind,
                            response: &mut response,
                        },
                    )
                    .await
                }
                TransferTransport::Scp => {
                    self.run_openssh(
                        OpenSshContext {
                            conn,
                            remote_home: &remote_home,
                            key_path: key_path_opt.as_deref(),
                            ssh: &ctx.ssh,
                            id,
                            timeout: ctx.timeout,
                        },
                        OpenSshOperation {
                            transport: openssh::OpenSshTransport::Scp,
                            kind,
                            response: &mut response,
                        },
                    )
                    .await
                }
                TransferTransport::Auto => {
                    Err(TransportAttemptError::Other(SshMcpError::connection(
                        "internal error: transport=auto should have been expanded",
                    )))
                }
                TransferTransport::Rsync => {
                    self.run_rsync(
                        OpenSshContext {
                            conn,
                            remote_home: &remote_home,
                            key_path: key_path_opt.as_deref(),
                            ssh: &ctx.ssh,
                            id,
                            timeout: ctx.timeout,
                        },
                        kind,
                        &mut response,
                    )
                    .await
                }
            };

            match attempt {
                Ok(()) => {
                    response.ok = true;
                    break;
                }
                Err(TransportAttemptError::Unsupported { transport, reason }) => {
                    unsupported_reasons.push(format!("{transport:?}: {reason}"));
                    continue;
                }
                Err(e) => {
                    if response.params.transport == TransferTransport::Auto {
                        failed_reasons.push(format!("{transport:?}: {e}"));
                        continue;
                    }
                    response.set_error(&e.to_string());
                    break;
                }
            }
        }

        if !response.ok
            && response.error.is_none()
            && response.params.transport == TransferTransport::Auto
        {
            // If we have both unsupported and failed errors, include all of them
            let all_reasons: Vec<String> = unsupported_reasons
                .into_iter()
                .chain(failed_reasons)
                .collect();

            if all_reasons.is_empty() {
                response.set_error("all transfer transports failed");
            } else {
                response.set_error(&format!(
                    "all auto transports failed: {}",
                    all_reasons.join("; ")
                ));
            }
        }

        response.elapsed_ms = Some(started_at.elapsed().as_millis() as u64);
        response
    }

    async fn run_exec_raw(
        &self,
        conn: &SshConnectionManager,
        remote_home: &str,
        id: u64,
        kind: TransferKind,
        timeout: Duration,
        response: &mut TransferResponse,
    ) -> Result<()> {
        let resolved = self
            .resolve_and_validate_local_paths(&response.params, kind)
            .await?;
        response.resolved_paths = Some(resolved.clone());

        let mut ctx = StepCtx {
            conn,
            remote_home,
            id,
            kind,
            resolved: &resolved,
            timeout,
            response,
        };

        match ctx.response.params.operation {
            TransferOperation::Put => self.put(&mut ctx).await,
            TransferOperation::Get => self.get(&mut ctx).await,
        }
    }

    async fn put(&self, ctx: &mut StepCtx<'_>) -> Result<()> {
        let raw_ctx = exec_raw::ExecRawCtx {
            conn: ctx.conn,
            id: ctx.id,
            timeout: ctx.timeout,
        };

        match ctx.kind {
            TransferKind::File => {
                let (staging, counts) = exec_raw::put_file_exec_raw(exec_raw::PutFileExecRawArgs {
                    ctx: raw_ctx,
                    remote_home: ctx.remote_home,
                    local_src: &ctx.resolved.local_path,
                    remote_dst: &ctx.response.params.remote_path,
                    overwrite: ctx.response.params.overwrite,
                })
                .await?;
                ctx.response.staging = Some(staging);
                ctx.response.counts = Some(counts);
                Ok(())
            }
            TransferKind::Directory => {
                let (staging, counts) = exec_raw::put_dir_exec_raw(exec_raw::PutDirExecRawArgs {
                    ctx: raw_ctx,
                    remote_home: ctx.remote_home,
                    local_src_dir: &ctx.resolved.local_path,
                    remote_dst_dir: &ctx.response.params.remote_path,
                    overwrite: ctx.response.params.overwrite,
                })
                .await?;
                ctx.response.staging = Some(staging);
                ctx.response.counts = Some(counts);
                ctx.response.semantics = Some(
                    "directory transfer behavior depends on overwrite: if overwrite=true, it stages into a temp dir (sibling under destination parent when possible, else $HOME/.ssh-mcp/staging) and then swaps into place via rename, optionally moving an existing destination to a backup path removed on success (backup may remain if swap fails); if overwrite=false, it creates the destination directory and writes directly into it (no atomic swap); on upload error it attempts to remove the stage directory (best-effort; for overwrite=false this is the created destination directory, and partial contents may remain)"
                        .to_string(),
                );
                Ok(())
            }
        }
    }

    async fn get(&self, ctx: &mut StepCtx<'_>) -> Result<()> {
        let raw_ctx = exec_raw::ExecRawCtx {
            conn: ctx.conn,
            id: ctx.id,
            timeout: ctx.timeout,
        };

        // If the client explicitly provided a kind, validate the remote path kind
        // before starting any streaming transfer.
        if ctx.response.params.kind.is_some() {
            let remote_kind = exec_raw::probe_remote_kind(exec_raw::ProbeRemoteKindArgs {
                ctx: raw_ctx,
                remote_path: &ctx.response.params.remote_path,
            })
            .await?;

            if remote_kind != ctx.kind {
                let msg = match ctx.kind {
                    TransferKind::File => "remote_path is not a file",
                    TransferKind::Directory => "remote_path is not a directory",
                };
                return Err(SshMcpError::invalid_params(msg));
            }
        }

        match ctx.kind {
            TransferKind::File => {
                let (staging, counts) = exec_raw::get_file_exec_raw(exec_raw::GetFileExecRawArgs {
                    ctx: raw_ctx,
                    remote_src: &ctx.response.params.remote_path,
                    local_dst: &ctx.resolved.local_path,
                    local_root: self.local_root(),
                    overwrite: ctx.response.params.overwrite,
                })
                .await?;
                ctx.response.staging = Some(staging);
                ctx.response.counts = Some(counts);
                Ok(())
            }
            TransferKind::Directory => {
                let (staging, counts) = exec_raw::get_dir_exec_raw(exec_raw::GetDirExecRawArgs {
                    ctx: raw_ctx,
                    remote_src_dir: &ctx.response.params.remote_path,
                    local_dst_dir: &ctx.resolved.local_path,
                    local_root: self.local_root(),
                    overwrite: ctx.response.params.overwrite,
                })
                .await?;
                ctx.response.staging = Some(staging);
                ctx.response.counts = Some(counts);
                ctx.response.semantics = Some(
                    "directory transfer writes into a sibling staging dir under local_root, then swaps into place via rename; local_path must not normalize to '.'; if the destination existed, it is first renamed to a sibling backup path and removed after the swap (backup may remain if swap fails)"
                        .to_string(),
                );
                Ok(())
            }
        }
    }

    async fn run_openssh(
        &self,
        ctx: OpenSshContext<'_>,
        op: OpenSshOperation<'_>,
    ) -> std::result::Result<(), TransportAttemptError> {
        let key_path = match ctx.key_path {
            Some(p) => p,
            None => {
                return Err(TransportAttemptError::Unsupported {
                    transport: match op.transport {
                        openssh::OpenSshTransport::Sftp => TransferTransport::Sftp,
                        openssh::OpenSshTransport::Scp => TransferTransport::Scp,
                    },
                    reason: "SSH key required for OpenSSH transports (sftp/scp)".to_string(),
                });
            }
        };

        let kind = op.kind;
        let response = op.response;

        let resolved = self
            .resolve_and_validate_local_paths(&response.params, kind)
            .await
            .map_err(TransportAttemptError::Other)?;
        response.resolved_paths = Some(resolved.clone());

        // If the client explicitly provided a kind for get, validate the remote path kind
        // before invoking OpenSSH tooling.
        let (operation, remote_path, kind_override) = {
            let params = &response.params;
            (params.operation, params.remote_path.clone(), params.kind)
        };

        if matches!(operation, TransferOperation::Get) && kind_override.is_some() {
            let remote_kind = exec_raw::probe_remote_kind(exec_raw::ProbeRemoteKindArgs {
                ctx: exec_raw::ExecRawCtx {
                    conn: ctx.conn,
                    id: ctx.id,
                    timeout: ctx.timeout,
                },
                remote_path: &remote_path,
            })
            .await
            .map_err(TransportAttemptError::Other)?;

            if remote_kind != kind {
                let msg = match kind {
                    TransferKind::File => "remote_path is not a file",
                    TransferKind::Directory => "remote_path is not a directory",
                };
                return Err(TransportAttemptError::Other(SshMcpError::invalid_params(
                    msg,
                )));
            }
        }

        let endpoint = openssh::OpenSshEndpoint {
            host: ctx.ssh.host.clone(),
            port: ctx.ssh.port,
            user: ctx.ssh.user.clone(),
            key_path: key_path.to_path_buf(),
        };

        let overwrite = response.params.overwrite;

        let openssh_args = openssh::OpenSshTransferArgs {
            transport: op.transport,
            conn: ctx.conn,
            remote_home: ctx.remote_home,
            local_root: self.local_root(),
            id: ctx.id,
            timeout: ctx.timeout,
            operation,
            kind,
            local_path: resolved.local_path,
            remote_path,
            overwrite,
        };

        let (staging, counts) = openssh::run_transfer(endpoint, openssh_args).await?;
        response.staging = Some(staging);
        response.counts = Some(counts);
        if kind == TransferKind::Directory {
            response.semantics = Some(match operation {
                TransferOperation::Put => "directory transfer behavior depends on overwrite: if overwrite=true, it stages into a temp dir (sibling under destination parent when possible, else $HOME/.ssh-mcp/staging) and then swaps into place via rename, optionally moving an existing destination to a backup path removed on success (backup may remain if swap fails); if overwrite=false, it creates the destination directory and writes directly into it (no atomic swap); on upload error it attempts to remove the stage directory (best-effort; for overwrite=false this is the created destination directory, and partial contents may remain)".to_string(),
                TransferOperation::Get => "directory transfer writes into a sibling staging dir under local_root, then swaps into place via rename; local_path must not normalize to '.'; if the destination existed, it is first renamed to a sibling backup path and removed after the swap (backup may remain if swap fails)".to_string(),
            });
        }
        Ok(())
    }

    async fn run_rsync(
        &self,
        ctx: OpenSshContext<'_>,
        kind: TransferKind,
        response: &mut TransferResponse,
    ) -> std::result::Result<(), TransportAttemptError> {
        let resolved = self
            .resolve_and_validate_local_paths(&response.params, kind)
            .await
            .map_err(TransportAttemptError::Other)?;
        response.resolved_paths = Some(resolved.clone());

        // If the client explicitly provided a kind for get, validate the remote path kind
        // before invoking rsync.
        let (operation, remote_path, kind_override) = {
            let params = &response.params;
            (params.operation, params.remote_path.clone(), params.kind)
        };

        if matches!(operation, TransferOperation::Get) && kind_override.is_some() {
            let remote_kind = exec_raw::probe_remote_kind(exec_raw::ProbeRemoteKindArgs {
                ctx: exec_raw::ExecRawCtx {
                    conn: ctx.conn,
                    id: ctx.id,
                    timeout: ctx.timeout,
                },
                remote_path: &remote_path,
            })
            .await
            .map_err(TransportAttemptError::Other)?;

            if remote_kind != kind {
                let msg = match kind {
                    TransferKind::File => "remote_path is not a file",
                    TransferKind::Directory => "remote_path is not a directory",
                };
                return Err(TransportAttemptError::Other(SshMcpError::invalid_params(
                    msg,
                )));
            }
        }

        let endpoint = rsync::RsyncEndpoint {
            host: ctx.ssh.host.clone(),
            port: ctx.ssh.port,
            user: ctx.ssh.user.clone(),
            key_path: ctx.key_path.map(|p| p.to_path_buf()),
        };

        let overwrite = response.params.overwrite;
        let rsync_options = response.params.rsync_options.clone();

        let rsync_args = rsync::RsyncTransferArgs {
            conn: ctx.conn,
            remote_home: ctx.remote_home,
            local_root: self.local_root(),
            id: ctx.id,
            timeout: ctx.timeout,
            operation,
            kind,
            local_path: &resolved.local_path,
            remote_path: &remote_path,
            overwrite,
            rsync_options,
        };

        let (staging, counts) = rsync::run_transfer(endpoint, rsync_args).await?;
        response.staging = Some(staging);
        response.counts = Some(counts);
        if kind == TransferKind::Directory {
            response.semantics = Some(match operation {
                TransferOperation::Put => "directory transfer behavior depends on overwrite: if overwrite=true, it stages into a temp dir (sibling under destination parent when possible, else $HOME/.ssh-mcp/staging) and then swaps into place via rename, optionally moving an existing destination to a backup path removed on success (backup may remain if swap fails); if overwrite=false, it creates the destination directory and writes directly into it (no atomic swap); on upload error it attempts to remove the stage directory (best-effort; for overwrite=false this is the created destination directory, and partial contents may remain)".to_string(),
                TransferOperation::Get => "directory transfer writes into a sibling staging dir under local_root, then swaps into place via rename; local_path must not normalize to '.'; if the destination existed, it is first renamed to a sibling backup path and removed after the swap (backup may remain if swap fails)".to_string(),
            });
        }
        Ok(())
    }

    async fn resolve_and_validate_local_paths(
        &self,
        params: &TransferParams,
        kind: TransferKind,
    ) -> Result<ResolvedPaths> {
        let resolved = local_root::resolve_paths(self.local_root(), params, kind)
            .map_err(SshMcpError::invalid_params)?;

        if matches!(params.operation, TransferOperation::Get) {
            local_root::validate_get_target_no_symlinks(self.local_root(), &resolved.local_path)
                .await
                .map_err(SshMcpError::invalid_params)?;

            // Create missing parent directories without following symlinks (best-effort).
            local_root::ensure_parent_dirs_no_symlinks(self.local_root(), &resolved.local_path)
                .await?;
        } else {
            // Best-effort: reject symlink components for put sources to prevent escaping local_root.
            local_root::validate_put_source_no_symlinks(self.local_root(), &resolved.local_path)
                .await
                .map_err(SshMcpError::invalid_params)?;
        }

        Ok(resolved)
    }
}

#[derive(Debug)]
enum TransportAttemptError {
    Unsupported {
        transport: TransferTransport,
        reason: String,
    },
    Other(SshMcpError),
}

impl std::fmt::Display for TransportAttemptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported { transport, reason } => {
                write!(f, "transport {transport:?} unsupported: {reason}")
            }
            Self::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for TransportAttemptError {}

async fn resolve_kind(
    conn: &SshConnectionManager,
    local_root: &Path,
    params: &TransferParams,
    timeout: Duration,
) -> Result<TransferKind> {
    match params.kind {
        Some(kind) => Ok(kind),
        None => match params.operation {
            TransferOperation::Put => {
                let local_src = local_root::safe_join_local_root(local_root, &params.local_path)
                    .map_err(SshMcpError::invalid_params)?;
                let meta = tokio::fs::symlink_metadata(&local_src).await?;
                if meta.is_dir() {
                    Ok(TransferKind::Directory)
                } else {
                    Ok(TransferKind::File)
                }
            }
            TransferOperation::Get => {
                exec_raw::probe_remote_kind(exec_raw::ProbeRemoteKindArgs {
                    ctx: exec_raw::ExecRawCtx {
                        conn,
                        id: 0,
                        timeout,
                    },
                    remote_path: &params.remote_path,
                })
                .await
            }
        },
    }
}
