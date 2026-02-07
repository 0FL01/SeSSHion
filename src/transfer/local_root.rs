use std::path::{Component, Path, PathBuf};

use tokio::fs;

use crate::error::SshMcpError;

use super::types::{ResolvedPaths, TransferKind, TransferOperation, TransferParams};

/// Join `user_path` to `local_root` while preventing traversal.
///
/// Rules:
/// - `user_path` must be relative
/// - no `..` components
/// - no root/prefix
pub fn safe_join_local_root(local_root: &Path, user_path: &str) -> Result<PathBuf, String> {
    let trimmed = user_path.trim();
    if trimmed.is_empty() {
        return Err("local_path cannot be empty".to_string());
    }

    let path = Path::new(trimmed);

    let mut saw_normal_component = false;
    for component in path.components() {
        match component {
            Component::Normal(_) => {
                saw_normal_component = true;
            }
            Component::CurDir => {}
            Component::ParentDir => {
                return Err("local_path must not contain '..'".to_string());
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err("local_path must be a relative path".to_string());
            }
        }
    }

    // Prevent get operations targeting the local root itself ("." normalizes to local_root).
    if !saw_normal_component {
        return Err("local_path must not normalize to '.'".to_string());
    }

    Ok(local_root.join(path))
}

/// Resolve and validate local paths according to the local_root policy.
pub fn resolve_paths(
    local_root: &Path,
    params: &TransferParams,
    _kind: TransferKind,
) -> Result<ResolvedPaths, String> {
    match params.operation {
        TransferOperation::Get => {
            let local_path = safe_join_local_root(local_root, &params.local_path)?;
            Ok(ResolvedPaths { local_path })
        }
        TransferOperation::Put => {
            let local_path = safe_join_local_root(local_root, &params.local_path)?;
            Ok(ResolvedPaths { local_path })
        }
    }
}

/// Best-effort symlink escape prevention for `put` sources.
///
/// This rejects any existing path component under `local_root` that is a symlink.
pub async fn validate_put_source_no_symlinks(
    local_root: &Path,
    absolute_source: &Path,
) -> Result<(), String> {
    let rel = absolute_source
        .strip_prefix(local_root)
        .map_err(|_| "local_path must be within local_root".to_string())?;

    let mut cursor = local_root.to_path_buf();
    for component in rel.components() {
        match component {
            Component::Normal(seg) => {
                cursor.push(seg);
                if let Ok(meta) = fs::symlink_metadata(&cursor).await
                    && meta.file_type().is_symlink()
                {
                    return Err("local_path traverses a symlink component".to_string());
                }
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err("local_path must be a relative path within local_root".to_string());
            }
        }
    }

    Ok(())
}

/// Best-effort symlink escape prevention for `get` destinations.
///
/// This rejects any existing path component under `local_root` that is a symlink.
pub async fn validate_get_target_no_symlinks(
    local_root: &Path,
    absolute_target: &Path,
) -> Result<(), String> {
    let rel = absolute_target
        .strip_prefix(local_root)
        .map_err(|_| "local_path must be within local_root".to_string())?;

    let mut cursor = local_root.to_path_buf();
    for component in rel.components() {
        match component {
            Component::Normal(seg) => {
                cursor.push(seg);
                if let Ok(meta) = fs::symlink_metadata(&cursor).await
                    && meta.file_type().is_symlink()
                {
                    return Err("local_path traverses a symlink component".to_string());
                }
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err("local_path must be a relative path within local_root".to_string());
            }
        }
    }

    Ok(())
}

/// Create all missing parent directories for `absolute_target` under `local_root`,
/// rejecting symlink components (best-effort).
///
/// This is intended for `get` destinations where we want to avoid following
/// attacker-controlled symlinks during directory creation.
pub async fn ensure_parent_dirs_no_symlinks(
    local_root: &Path,
    absolute_target: &Path,
) -> crate::error::Result<()> {
    let rel = absolute_target
        .strip_prefix(local_root)
        .map_err(|_| SshMcpError::invalid_params("local_path must be within local_root"))?;

    let mut cursor = local_root.to_path_buf();
    let mut comps = rel.components().peekable();

    while let Some(component) = comps.next() {
        // Skip the final component (file name / destination dir name).
        if comps.peek().is_none() {
            break;
        }

        match component {
            Component::Normal(seg) => {
                cursor.push(seg);

                match fs::symlink_metadata(&cursor).await {
                    Ok(meta) => {
                        if meta.file_type().is_symlink() {
                            return Err(SshMcpError::invalid_params(
                                "local_path traverses a symlink component",
                            ));
                        }
                        if !meta.is_dir() {
                            return Err(SshMcpError::invalid_params(
                                "local_path parent component is not a directory",
                            ));
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        match fs::create_dir(&cursor).await {
                            Ok(()) => {}
                            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                            Err(e) => return Err(SshMcpError::Io(e)),
                        }

                        let meta = fs::symlink_metadata(&cursor).await?;
                        if meta.file_type().is_symlink() {
                            return Err(SshMcpError::invalid_params(
                                "local_path traverses a symlink component",
                            ));
                        }
                        if !meta.is_dir() {
                            return Err(SshMcpError::invalid_params(
                                "local_path parent component is not a directory",
                            ));
                        }
                    }
                    Err(e) => return Err(SshMcpError::Io(e)),
                }
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(SshMcpError::invalid_params(
                    "local_path must be a relative path within local_root",
                ));
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_join_rejects_absolute() {
        let root = Path::new("/srv");
        assert!(safe_join_local_root(root, "/etc/passwd").is_err());
    }

    #[test]
    fn safe_join_rejects_parent_dir() {
        let root = Path::new("/srv");
        assert!(safe_join_local_root(root, "../x").is_err());
        assert!(safe_join_local_root(root, "a/../../x").is_err());
    }

    #[test]
    fn safe_join_allows_normal() {
        let root = Path::new("/srv");
        let joined = safe_join_local_root(root, "a/b/c.txt").unwrap();
        assert_eq!(joined, PathBuf::from("/srv/a/b/c.txt"));
    }

    #[test]
    fn safe_join_rejects_dot() {
        let root = Path::new("/srv");
        assert!(safe_join_local_root(root, ".").is_err());
        assert!(safe_join_local_root(root, "./").is_err());
        assert!(safe_join_local_root(root, "   ").is_err());
    }
}
