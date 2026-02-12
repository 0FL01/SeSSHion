use std::path::{Path, PathBuf};

use crate::error::{Result, SshMcpError};

use super::TransferCounts;

pub(super) async fn count_dir_no_symlinks(root: &Path) -> Result<TransferCounts> {
    let meta = tokio::fs::symlink_metadata(root).await?;
    if meta.file_type().is_symlink() {
        return Err(SshMcpError::invalid_params(
            "symlinks are not supported by directory transfer",
        ));
    }
    if !meta.is_dir() {
        return Err(SshMcpError::invalid_params("local_path is not a directory"));
    }

    let mut counts = TransferCounts::zero();

    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut rd = tokio::fs::read_dir(&dir).await?;
        while let Some(ent) = rd.next_entry().await? {
            let p = ent.path();
            let m = tokio::fs::symlink_metadata(&p).await?;
            if m.file_type().is_symlink() {
                return Err(SshMcpError::invalid_params(
                    "symlinks are not supported by directory transfer",
                ));
            }
            if m.is_dir() {
                counts.directories += 1;
                stack.push(p);
            } else if m.is_file() {
                counts.files += 1;
                counts.bytes += m.len();
            } else {
                return Err(SshMcpError::invalid_params(
                    "unsupported file type in directory transfer",
                ));
            }
        }
    }

    Ok(counts)
}
