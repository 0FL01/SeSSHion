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
pub(crate) use types::{TransferEvent, TransferEventSink, TransferProgressTarget};

use std::collections::HashSet;
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::error::{Result, SshMcpError};
use crate::ssh::{HostKeyCheckMode, SshConnectionManager};

fn io_to_transport_attempt(err: std::io::Error) -> TransportAttemptError {
    TransportAttemptError::Other(SshMcpError::Io(err))
}

struct StepCtx<'a> {
    conn: &'a SshConnectionManager,
    remote_home: &'a str,
    id: &'a str,
    kind: TransferKind,
    resolved: &'a ResolvedPaths,
    timeout: Duration,
    cancellation: &'a CancellationToken,
    progress: Option<&'a TransferEventSink>,
    response: &'a mut TransferResponse,
}

struct OpenSshContext<'a> {
    conn: &'a SshConnectionManager,
    remote_home: &'a str,
    key_path: Option<&'a Path>,
    ssh: &'a TransferSshOptions,
    id: &'a str,
    timeout: Duration,
    cancellation: &'a CancellationToken,
    progress: Option<&'a TransferEventSink>,
}

struct OpenSshOperation<'a> {
    transport: openssh::OpenSshTransport,
    kind: TransferKind,
    response: &'a mut TransferResponse,
}

struct ExecRawOperation<'a> {
    conn: &'a SshConnectionManager,
    remote_home: &'a str,
    id: &'a str,
    kind: TransferKind,
    timeout: Duration,
    cancellation: &'a CancellationToken,
    progress: Option<&'a TransferEventSink>,
    response: &'a mut TransferResponse,
}

/// Core transfer engine.
///
/// For now this selects the EXEC-RAW transport, but it is structured so that
/// SFTP/SCP can be added as additional implementations.
#[derive(Clone, Debug)]
pub struct TransferEngine {
    local_root: Arc<PathBuf>,
    active_destinations: Arc<Mutex<HashSet<String>>>,
}

#[derive(Clone, Debug)]
pub struct TransferRunContext {
    pub timeout: Duration,
    pub ssh: TransferSshOptions,
}

struct TransferExecutionContext {
    timeout: Duration,
    ssh: TransferSshOptions,
    cancellation: CancellationToken,
    progress: Option<TransferEventSink>,
}

#[derive(Clone, Debug)]
pub struct TransferSshOptions {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub key_path: Option<PathBuf>,
    pub host_key_checking: HostKeyCheckMode,
    pub known_hosts: Option<PathBuf>,
}

struct DestinationGuard {
    key: String,
    active: Arc<Mutex<HashSet<String>>>,
}

impl Drop for DestinationGuard {
    fn drop(&mut self) {
        if let Ok(mut active) = self.active.lock() {
            active.remove(&self.key);
        }
    }
}

impl TransferEngine {
    pub fn new(local_root: PathBuf) -> Self {
        Self {
            local_root: Arc::new(local_root),
            active_destinations: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    pub fn local_root(&self) -> &Path {
        self.local_root.as_path()
    }

    fn next_attempt_token(&self) -> Result<String> {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).map_err(|error| {
            SshMcpError::connection(format!("failed to create staging token: {error}"))
        })?;
        let mut token = String::with_capacity(32);
        for byte in bytes {
            write!(&mut token, "{byte:02x}")
                .map_err(|_| SshMcpError::connection("failed to format staging token"))?;
        }
        Ok(token)
    }

    fn reserve_destination(
        &self,
        params: &TransferParams,
        kind: TransferKind,
    ) -> Result<DestinationGuard> {
        let key = match params.operation {
            TransferOperation::Put => {
                exec_raw::validate_remote_user_path(&params.remote_path, "remote_path")?;
                format!("put:{}", normalize_remote_path(&params.remote_path))
            }
            TransferOperation::Get => {
                let resolved = local_root::resolve_paths(self.local_root(), params, kind)
                    .map_err(SshMcpError::invalid_params)?;
                format!("get:{}", resolved.local_path.display())
            }
        };

        let mut active = self
            .active_destinations
            .lock()
            .map_err(|_| SshMcpError::connection("destination guard poisoned"))?;
        if !active.insert(key.clone()) {
            return Err(SshMcpError::invalid_params(format!(
                "destination busy: {}",
                match params.operation {
                    TransferOperation::Put => &params.remote_path,
                    TransferOperation::Get => &params.local_path,
                }
            )));
        }
        drop(active);

        Ok(DestinationGuard {
            key,
            active: Arc::clone(&self.active_destinations),
        })
    }

    pub async fn run(
        &self,
        conn: &SshConnectionManager,
        params: TransferParams,
        ctx: TransferRunContext,
    ) -> TransferResponse {
        self.run_controlled(conn, params, ctx, CancellationToken::new(), None)
            .await
    }

    pub(crate) async fn run_controlled(
        &self,
        conn: &SshConnectionManager,
        params: TransferParams,
        ctx: TransferRunContext,
        external_cancellation: CancellationToken,
        progress: Option<TransferEventSink>,
    ) -> TransferResponse {
        const TEARDOWN_GRACE: Duration = Duration::from_secs(5);

        let started_at = Instant::now();
        let transfer_timeout = ctx.timeout;
        let response_params = params.clone();
        let work_cancellation = CancellationToken::new();
        let execution_ctx = TransferExecutionContext {
            timeout: ctx.timeout,
            ssh: ctx.ssh,
            cancellation: work_cancellation.clone(),
            progress,
        };

        let work = self.run_inner(conn, params, execution_ctx, started_at);
        tokio::pin!(work);

        enum StopReason {
            Cancelled,
            TimedOut,
        }

        let stop_reason = tokio::select! {
            response = &mut work => return response,
            _ = external_cancellation.cancelled() => StopReason::Cancelled,
            _ = tokio::time::sleep_until(started_at + transfer_timeout) => StopReason::TimedOut,
        };

        work_cancellation.cancel();
        let _ = tokio::time::timeout(TEARDOWN_GRACE, &mut work).await;

        let message = match stop_reason {
            StopReason::Cancelled => "transfer cancelled".to_string(),
            StopReason::TimedOut => {
                format!("transfer timeout after {}ms", transfer_timeout.as_millis())
            }
        };
        let mut response = TransferResponse::error(response_params, self.local_root(), &message);
        response.elapsed_ms = Some(started_at.elapsed().as_millis() as u64);
        response
    }

    async fn run_inner(
        &self,
        conn: &SshConnectionManager,
        params: TransferParams,
        ctx: TransferExecutionContext,
        started_at: Instant,
    ) -> TransferResponse {
        let key_path_opt = ctx.ssh.key_path.clone();

        if let Some(progress) = &ctx.progress {
            progress.emit(types::TransferEvent::Preparing);
        }

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

        let kind = match resolve_kind(
            conn,
            self.local_root(),
            &response.params,
            ctx.timeout,
            &ctx.cancellation,
        )
        .await
        {
            Ok(kind) => kind,
            Err(e) => {
                response.set_error(&format!("failed to resolve transfer kind: {e}"));
                response.elapsed_ms = Some(started_at.elapsed().as_millis() as u64);
                return response;
            }
        };
        response.kind = Some(kind);

        let _destination_guard = match self.reserve_destination(&response.params, kind) {
            Ok(guard) => guard,
            Err(error) => {
                response.set_error(&error.to_string());
                response.elapsed_ms = Some(started_at.elapsed().as_millis() as u64);
                return response;
            }
        };

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

        let mut attempted_transports: Vec<TransferTransport> = Vec::new();
        let mut unsupported_reasons: Vec<String> = Vec::new();

        for transport in transports {
            let id = match self.next_attempt_token() {
                Ok(id) => id,
                Err(error) => {
                    response.set_error(&error.to_string());
                    break;
                }
            };
            attempted_transports.push(transport);
            response.transport_used = transport;
            if let Some(progress) = &ctx.progress {
                progress.emit(types::TransferEvent::Transferring(transport));
            }
            let attempt = match transport {
                TransferTransport::ExecRaw => self
                    .run_exec_raw(ExecRawOperation {
                        conn,
                        remote_home: &remote_home,
                        id: &id,
                        kind,
                        timeout: ctx.timeout,
                        cancellation: &ctx.cancellation,
                        progress: ctx.progress.as_ref(),
                        response: &mut response,
                    })
                    .await
                    .map_err(TransportAttemptError::Other),
                TransferTransport::Sftp => {
                    self.run_openssh(
                        OpenSshContext {
                            conn,
                            remote_home: &remote_home,
                            key_path: key_path_opt.as_deref(),
                            ssh: &ctx.ssh,
                            id: &id,
                            timeout: ctx.timeout,
                            cancellation: &ctx.cancellation,
                            progress: ctx.progress.as_ref(),
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
                            id: &id,
                            timeout: ctx.timeout,
                            cancellation: &ctx.cancellation,
                            progress: ctx.progress.as_ref(),
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
                            id: &id,
                            timeout: ctx.timeout,
                            cancellation: &ctx.cancellation,
                            progress: ctx.progress.as_ref(),
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
                Err(TransportAttemptError::FallbackSafe { transport, reason }) => {
                    unsupported_reasons.push(format!("{transport:?}: {reason}"));
                    continue;
                }
                Err(e) => {
                    response.set_error(&e.to_string());
                    break;
                }
            }
        }

        // Only populate fallback_chain when the original transport was Auto
        if response.params.transport == TransferTransport::Auto {
            response.fallback_chain = attempted_transports;
        }

        if !response.ok
            && response.error.is_none()
            && response.params.transport == TransferTransport::Auto
        {
            let all_reasons = unsupported_reasons;

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

    async fn run_exec_raw(&self, operation: ExecRawOperation<'_>) -> Result<()> {
        let ExecRawOperation {
            conn,
            remote_home,
            id,
            kind,
            timeout,
            cancellation,
            progress,
            response,
        } = operation;
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
            cancellation,
            progress,
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
            cancellation: ctx.cancellation,
            progress: ctx.progress,
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
                    "directory transfer behavior depends on overwrite: if overwrite=true, it uses an exclusively created sibling staging directory and rollback-protected rename; if overwrite=false, it creates the destination directory and writes directly into it (no atomic swap); on upload error it attempts to remove the stage directory (best-effort; for overwrite=false this is the created destination directory, and partial contents may remain)"
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
            cancellation: ctx.cancellation,
            progress: ctx.progress,
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
                    "directory transfer writes into an exclusively created sibling staging directory under local_root, then installs it with rollback-protected rename; local_path must not normalize to '.'"
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
                return Err(TransportAttemptError::FallbackSafe {
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
                    cancellation: ctx.cancellation,
                    progress: ctx.progress,
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
            host_key_checking: ctx.ssh.host_key_checking,
            known_hosts: ctx.ssh.known_hosts.clone(),
        };

        let overwrite = response.params.overwrite;

        let openssh_args = openssh::OpenSshTransferArgs {
            transport: op.transport,
            conn: ctx.conn,
            remote_home: ctx.remote_home,
            local_root: self.local_root(),
            id: ctx.id.to_string(),
            timeout: ctx.timeout,
            cancellation: ctx.cancellation.clone(),
            progress: ctx.progress.cloned(),
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
                TransferOperation::Put => "directory transfer behavior depends on overwrite: if overwrite=true, it uses an exclusively created sibling staging directory and rollback-protected rename; if overwrite=false, it creates the destination directory and writes directly into it (no atomic swap)".to_string(),
                TransferOperation::Get => "directory transfer writes into an exclusively created sibling staging directory under local_root, then installs it with rollback-protected rename; local_path must not normalize to '.'".to_string(),
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
        if ctx.key_path.is_none() {
            return Err(TransportAttemptError::FallbackSafe {
                transport: TransferTransport::Rsync,
                reason: "SSH key required for rsync transport".to_string(),
            });
        }

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
                    cancellation: ctx.cancellation,
                    progress: ctx.progress,
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
            host_key_checking: ctx.ssh.host_key_checking,
            known_hosts: ctx.ssh.known_hosts.clone(),
        };

        let overwrite = response.params.overwrite;
        let rsync_options = response.params.rsync_options.clone();

        let rsync_args = rsync::RsyncTransferArgs {
            conn: ctx.conn,
            remote_home: ctx.remote_home,
            local_root: self.local_root(),
            id: ctx.id.to_string(),
            timeout: ctx.timeout,
            cancellation: ctx.cancellation.clone(),
            progress: ctx.progress.cloned(),
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
                TransferOperation::Put => "directory transfer behavior depends on overwrite: if overwrite=true, it uses an exclusively created sibling staging directory and rollback-protected rename; if overwrite=false, it creates the destination directory and writes directly into it (no atomic swap)".to_string(),
                TransferOperation::Get => "directory transfer writes into an exclusively created sibling staging directory under local_root, then installs it with rollback-protected rename; local_path must not normalize to '.'".to_string(),
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

fn normalize_remote_path(path: &str) -> String {
    let absolute = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." if parts.last().is_some_and(|last| *last != "..") => {
                parts.pop();
            }
            ".." if !absolute => parts.push(part),
            ".." => {}
            _ => parts.push(part),
        }
    }

    let joined = parts.join("/");
    if absolute {
        format!("/{joined}")
    } else if joined.is_empty() {
        ".".to_string()
    } else {
        joined
    }
}

#[derive(Debug)]
enum TransportAttemptError {
    FallbackSafe {
        transport: TransferTransport,
        reason: String,
    },
    Other(SshMcpError),
}

impl std::fmt::Display for TransportAttemptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FallbackSafe { transport, reason } => {
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
    cancellation: &CancellationToken,
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
                        id: "",
                        timeout,
                        cancellation,
                        progress: None,
                    },
                    remote_path: &params.remote_path,
                })
                .await
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destination_guard_rejects_normalized_alias_until_release() {
        let engine = TransferEngine::new(PathBuf::from("/tmp/local-root"));
        let first = TransferParams {
            remote_path: "/tmp/a/../target".to_string(),
            ..TransferParams::default()
        };
        let second = TransferParams {
            remote_path: "/tmp/target".to_string(),
            ..TransferParams::default()
        };

        let guard = engine
            .reserve_destination(&first, TransferKind::File)
            .expect("first destination reservation");
        let error = engine
            .reserve_destination(&second, TransferKind::File)
            .err()
            .expect("normalized alias must be busy");
        assert!(error.to_string().contains("destination busy"));

        drop(guard);
        assert!(
            engine
                .reserve_destination(&second, TransferKind::File)
                .is_ok()
        );
    }

    #[test]
    fn attempt_tokens_are_128_bit_hex() {
        let engine = TransferEngine::new(PathBuf::from("/tmp/local-root"));
        let first = engine.next_attempt_token().expect("first token");
        let second = engine.next_attempt_token().expect("second token");

        assert_eq!(first.len(), 32);
        assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }
}
