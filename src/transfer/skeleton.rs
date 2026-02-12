use std::future::Future;
use std::path::Path;
use std::time::Duration;

use crate::error::SshMcpError;
use crate::ssh::{SshConnectionManager, escape_for_shell};

use super::TransportAttemptError;
use super::exec_raw;
use super::staging;
use super::types::{StagingLocal, StagingRemote, TransferCounts, TransferStaging};

pub(in crate::transfer) struct PutFileWithRemoteStagingArgs<'a> {
    pub(in crate::transfer) conn: &'a SshConnectionManager,
    pub(in crate::transfer) remote_home: &'a str,
    pub(in crate::transfer) remote_path: String,
    pub(in crate::transfer) overwrite: bool,
    pub(in crate::transfer) id: u64,
    pub(in crate::transfer) timeout: Duration,
    pub(in crate::transfer) local_path: &'a Path,
}

pub(in crate::transfer) struct GetFileWithLocalStagingArgs<'a> {
    pub(in crate::transfer) local_root: &'a Path,
    pub(in crate::transfer) local_path: &'a Path,
    pub(in crate::transfer) remote_path: &'a str,
    pub(in crate::transfer) overwrite: bool,
    pub(in crate::transfer) id: u64,
}

pub(in crate::transfer) struct PutDirWithRemoteStagingArgs<'a> {
    pub(in crate::transfer) conn: &'a SshConnectionManager,
    pub(in crate::transfer) remote_home: &'a str,
    pub(in crate::transfer) remote_path: String,
    pub(in crate::transfer) overwrite: bool,
    pub(in crate::transfer) id: u64,
    pub(in crate::transfer) timeout: Duration,
    pub(in crate::transfer) counts: TransferCounts,
}

pub(in crate::transfer) struct GetDirWithLocalStagingArgs<'a> {
    pub(in crate::transfer) conn: &'a SshConnectionManager,
    pub(in crate::transfer) local_root: &'a Path,
    pub(in crate::transfer) local_path: &'a Path,
    pub(in crate::transfer) remote_path: &'a str,
    pub(in crate::transfer) overwrite: bool,
    pub(in crate::transfer) id: u64,
    pub(in crate::transfer) timeout: Duration,
}

pub(in crate::transfer) async fn put_file_with_remote_staging<F, Fut>(
    args: PutFileWithRemoteStagingArgs<'_>,
    upload_into_stage: F,
) -> std::result::Result<(TransferStaging, TransferCounts), TransportAttemptError>
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = std::result::Result<(), TransportAttemptError>>,
{
    let meta = tokio::fs::symlink_metadata(args.local_path)
        .await
        .map_err(SshMcpError::Io)
        .map_err(TransportAttemptError::Other)?;
    if !meta.is_file() {
        return Err(TransportAttemptError::Other(SshMcpError::invalid_params(
            "local_path is not a file",
        )));
    }
    let size = meta.len();

    let stage = staging::remote_prepare_put_file_stage(
        args.conn,
        args.remote_home,
        &args.remote_path,
        args.overwrite,
        args.id,
        args.timeout,
    )
    .await
    .map_err(TransportAttemptError::Other)?;

    let stage_path = stage.stage_path.clone();

    if let Err(e) = upload_into_stage(stage_path.clone()).await {
        best_effort_remote_rm_file(args.conn, args.timeout, &stage_path).await;
        return Err(e);
    }

    if let Err(e) = staging::remote_finalize_put_file(
        args.conn,
        &args.remote_path,
        &stage_path,
        args.overwrite,
        args.timeout,
    )
    .await
    .map_err(TransportAttemptError::Other)
    {
        best_effort_remote_rm_file(args.conn, args.timeout, &stage_path).await;
        return Err(e);
    }

    Ok((
        TransferStaging {
            local: None,
            remote: Some(StagingRemote {
                staging_path: stage.stage_path,
                backup_path: None,
                final_path: args.remote_path,
                staging_base_home: stage.stage_base,
            }),
        },
        TransferCounts {
            bytes: size,
            files: 1,
            directories: 0,
        },
    ))
}

pub(in crate::transfer) async fn get_file_with_local_staging<F, Fut>(
    args: GetFileWithLocalStagingArgs<'_>,
    download_into_tmp: F,
) -> std::result::Result<(TransferStaging, TransferCounts), TransportAttemptError>
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = std::result::Result<(), TransportAttemptError>>,
{
    exec_raw::validate_remote_user_file_path(args.remote_path, "remote_path")
        .map_err(TransportAttemptError::Other)?;

    let (tmp, f) =
        exec_raw::create_unique_local_staging_file(args.local_root, args.local_path, args.id)
            .await
            .map_err(TransportAttemptError::Other)?;
    drop(f);

    let tmp_str = tmp.display().to_string();

    if let Err(e) = download_into_tmp(tmp_str.clone()).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e);
    }

    let meta = tokio::fs::metadata(&tmp)
        .await
        .map_err(SshMcpError::Io)
        .map_err(TransportAttemptError::Other)?;
    let bytes = meta.len();

    if args.overwrite {
        exec_raw::atomic_replace_file(&tmp, args.local_path)
            .await
            .map_err(TransportAttemptError::Other)?;
    } else {
        exec_raw::atomic_install_file_overwrite_false(&tmp, args.local_path)
            .await
            .map_err(TransportAttemptError::Other)?;
    }

    Ok((
        TransferStaging {
            local: Some(StagingLocal {
                staging_path: tmp_str,
                backup_path: None,
                final_path: args.local_path.display().to_string(),
            }),
            remote: None,
        },
        TransferCounts {
            bytes,
            files: 1,
            directories: 0,
        },
    ))
}

pub(in crate::transfer) async fn put_dir_with_remote_staging<F, Fut>(
    args: PutDirWithRemoteStagingArgs<'_>,
    upload_into_stage: F,
) -> std::result::Result<(TransferStaging, TransferCounts), TransportAttemptError>
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = std::result::Result<(), TransportAttemptError>>,
{
    let PutDirWithRemoteStagingArgs {
        conn,
        remote_home,
        remote_path,
        overwrite,
        id,
        timeout,
        counts,
    } = args;

    let stage = staging::remote_prepare_put_dir_stage(
        conn,
        remote_home,
        &remote_path,
        overwrite,
        id,
        timeout,
    )
    .await
    .map_err(TransportAttemptError::Other)?;

    let stage_path = stage.stage_path.clone();
    let cleanup_on_finalize_error = !stage.stage_is_destination;

    if let Err(e) = upload_into_stage(stage_path.clone()).await {
        best_effort_remote_rm_dir(conn, timeout, &stage_path).await;
        return Err(e);
    }

    let backup_path = if overwrite {
        match staging::remote_finalize_put_dir_overwrite_true(
            conn,
            remote_home,
            &remote_path,
            &stage_path,
            id,
            timeout,
        )
        .await
        .map_err(TransportAttemptError::Other)
        {
            Ok(backup) => backup,
            Err(e) => {
                if cleanup_on_finalize_error {
                    best_effort_remote_rm_dir(conn, timeout, &stage_path).await;
                }
                return Err(e);
            }
        }
    } else {
        None
    };

    Ok((
        TransferStaging {
            local: None,
            remote: Some(StagingRemote {
                staging_path: stage.stage_path,
                backup_path,
                final_path: remote_path,
                staging_base_home: stage.stage_base,
            }),
        },
        counts,
    ))
}

pub(in crate::transfer) async fn get_dir_with_local_staging<F, Fut>(
    args: GetDirWithLocalStagingArgs<'_>,
    download_into_dir: F,
) -> std::result::Result<(TransferStaging, TransferCounts), TransportAttemptError>
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = std::result::Result<(), TransportAttemptError>>,
{
    exec_raw::validate_remote_user_path(args.remote_path, "remote_path")
        .map_err(TransportAttemptError::Other)?;

    staging::remote_validate_dir_contents(args.conn, args.remote_path, args.timeout)
        .await
        .map_err(TransportAttemptError::Other)?;

    let (extract_target, local_backup) = if args.overwrite {
        let stage =
            exec_raw::create_unique_local_staging_dir(args.local_root, args.local_path, args.id)
                .await
                .map_err(TransportAttemptError::Other)?;
        let backup = exec_raw::local_backup_dir_sibling(args.local_path, args.id);
        (stage, Some(backup))
    } else {
        match tokio::fs::create_dir(args.local_path).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(TransportAttemptError::Other(SshMcpError::invalid_params(
                    "local destination exists and overwrite=false. Use overwrite=true to replace it.",
                )));
            }
            Err(e) => return Err(TransportAttemptError::Other(SshMcpError::Io(e))),
        }
        (args.local_path.to_path_buf(), None)
    };

    let extract_target_str = extract_target.display().to_string();

    if let Err(e) = download_into_dir(extract_target_str).await {
        let _ = tokio::fs::remove_dir_all(&extract_target).await;
        return Err(e);
    }

    let counts = match super::walk::count_dir_no_symlinks(&extract_target).await {
        Ok(counts) => counts,
        Err(e) => {
            let _ = tokio::fs::remove_dir_all(&extract_target).await;
            return Err(TransportAttemptError::Other(e));
        }
    };

    let (staging_path, backup_path) = if args.overwrite {
        let backup = match local_backup.as_ref() {
            Some(backup) => backup,
            None => {
                let _ = tokio::fs::remove_dir_all(&extract_target).await;
                return Err(TransportAttemptError::Other(SshMcpError::connection(
                    "missing local backup path",
                )));
            }
        };

        if let Err(e) = exec_raw::atomic_replace_dir(&extract_target, args.local_path, backup).await
        {
            let _ = tokio::fs::remove_dir_all(&extract_target).await;
            return Err(TransportAttemptError::Other(e));
        }

        (
            extract_target.display().to_string(),
            Some(backup.display().to_string()),
        )
    } else {
        (args.local_path.display().to_string(), None)
    };

    Ok((
        TransferStaging {
            local: Some(StagingLocal {
                staging_path,
                backup_path,
                final_path: args.local_path.display().to_string(),
            }),
            remote: None,
        },
        counts,
    ))
}

async fn best_effort_remote_rm_file(conn: &SshConnectionManager, timeout: Duration, path: &str) {
    if exec_raw::validate_remote_user_file_path(path, "remote_stage").is_err() {
        return;
    }

    let escaped = escape_for_shell(path);
    let cmd = format!(r#"sh -c 'rm -f -- "$1" 2>/dev/null || true' sh '{escaped}'"#);
    let _ = conn.exec_command(&cmd, timeout).await;
}

async fn best_effort_remote_rm_dir(conn: &SshConnectionManager, timeout: Duration, path: &str) {
    if exec_raw::validate_remote_user_path(path, "remote_stage").is_err() {
        return;
    }

    let escaped = escape_for_shell(path);
    let cmd = format!(r#"sh -c 'rm -rf -- "$1" 2>/dev/null || true' sh '{escaped}'"#);
    let _ = conn.exec_command(&cmd, timeout).await;
}
