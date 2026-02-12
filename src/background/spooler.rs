use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use tokio::fs;
use tracing::warn;

use super::{BackgroundError, Result};

const DEFAULT_SPOOL_DIR: &str = "/tmp/ssh-mcp";

/// Local-only log spooler.
///
/// Phase 1 is responsible for local directory existence and deterministic
/// path generation. Later phases will stream remote stdout/stderr into these
/// files and use the registry to serve `check-process`.
#[derive(Debug, Clone)]
pub struct LocalLogSpooler {
    base_dir: PathBuf,
}

impl LocalLogSpooler {
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    pub fn new_default() -> Self {
        Self::new(PathBuf::from(DEFAULT_SPOOL_DIR))
    }

    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    pub async fn ensure_dir(&self) -> Result<()> {
        match fs::symlink_metadata(&self.base_dir).await {
            Ok(meta) => validate_spool_dir_meta(&meta)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                match fs::create_dir(&self.base_dir).await {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(e.into()),
                }

                // Re-validate after creation to close TOCTOU window.
                let meta = fs::symlink_metadata(&self.base_dir).await?;
                validate_spool_dir_meta(&meta)?;
            }
            Err(e) => return Err(e.into()),
        }

        #[cfg(unix)]
        {
            let meta = fs::symlink_metadata(&self.base_dir).await?;
            validate_spool_dir_meta(&meta)?;
            let perms = std::fs::Permissions::from_mode(0o700);
            fs::set_permissions(&self.base_dir, perms).await?;
        }

        Ok(())
    }

    pub fn log_path_for(&self, job_id: &str) -> Result<PathBuf> {
        validate_job_id(job_id)?;
        Ok(self.base_dir.join(format!("{job_id}.log")))
    }

    pub async fn cleanup_old_logs(&self, max_age: Duration) -> Result<usize> {
        self.ensure_dir().await?;

        let now = SystemTime::now();
        let mut removed = 0usize;

        let mut entries = match fs::read_dir(&self.base_dir).await {
            Ok(e) => e,
            Err(e) => return Err(e.into()),
        };

        loop {
            let entry = match entries.next_entry().await {
                Ok(Some(e)) => e,
                Ok(None) => break,
                Err(e) => {
                    warn!(error = ?e, "failed to read spool directory entry");
                    continue;
                }
            };
            let path = entry.path();

            let file_name = match entry.file_name().to_str() {
                Some(s) => s.to_owned(),
                None => continue,
            };

            let Some((job_id, ext)) = split_spool_file_name(&file_name) else {
                continue;
            };
            if validate_job_id(job_id).is_err() {
                continue;
            }
            if ext != "log" && ext != "exit" {
                continue;
            }

            let meta = match fs::symlink_metadata(&path).await {
                Ok(m) => m,
                Err(e) => {
                    warn!(path = ?path, error = ?e, "failed to stat spool file");
                    continue;
                }
            };

            let ft = meta.file_type();
            if ft.is_symlink() || !ft.is_file() {
                continue;
            }

            let modified = match meta.modified() {
                Ok(m) => m,
                Err(e) => {
                    warn!(path = ?path, error = ?e, "failed to read mtime");
                    continue;
                }
            };

            let age = match now.duration_since(modified) {
                Ok(d) => d,
                Err(e) => {
                    warn!(path = ?path, error = ?e, "invalid modified time");
                    continue;
                }
            };
            if age <= max_age {
                continue;
            }

            match fs::remove_file(&path).await {
                Ok(()) => removed += 1,
                Err(e) => {
                    warn!(path = ?path, error = ?e, "failed to remove old spool file");
                }
            }
        }

        Ok(removed)
    }
}

fn validate_spool_dir_meta(meta: &std::fs::Metadata) -> Result<()> {
    let ft = meta.file_type();
    if ft.is_symlink() {
        return Err(BackgroundError::InvalidState {
            message: "spool directory is a symlink",
        });
    }
    if !ft.is_dir() {
        return Err(BackgroundError::InvalidState {
            message: "spool path exists but is not a directory",
        });
    }
    Ok(())
}

fn validate_job_id(job_id: &str) -> Result<()> {
    if job_id.is_empty() || job_id.len() > 128 {
        return Err(BackgroundError::InvalidJobId {
            job_id: job_id.to_owned(),
        });
    }
    if job_id.as_bytes().contains(&0) {
        return Err(BackgroundError::InvalidJobId {
            job_id: job_id.to_owned(),
        });
    }

    // Require a single, normal path component (reject absolute paths, separators, '.', '..').
    let p = Path::new(job_id);
    let mut components = p.components();
    let Some(Component::Normal(_)) = components.next() else {
        return Err(BackgroundError::InvalidJobId {
            job_id: job_id.to_owned(),
        });
    };
    if components.next().is_some() {
        return Err(BackgroundError::InvalidJobId {
            job_id: job_id.to_owned(),
        });
    }

    // Tighten further: allow only ASCII alnum plus '-' and '_' to keep file naming predictable.
    if !job_id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(BackgroundError::InvalidJobId {
            job_id: job_id.to_owned(),
        });
    }

    Ok(())
}

fn split_spool_file_name(name: &str) -> Option<(&str, &str)> {
    let (stem, ext) = name.rsplit_once('.')?;
    if stem.is_empty() || ext.is_empty() {
        return None;
    }
    Some((stem, ext))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Instant, SystemTime};

    async fn wait_until_older_than(path: &Path, min_age: Duration) {
        let start = Instant::now();
        loop {
            let meta = tokio::fs::metadata(path).await.expect("metadata");
            let modified = meta.modified().expect("modified time");
            let age = SystemTime::now()
                .duration_since(modified)
                .unwrap_or_else(|_| Duration::from_secs(0));

            if age >= min_age {
                return;
            }

            assert!(
                start.elapsed() < Duration::from_secs(2),
                "file did not become old enough: {path:?}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn test_ensure_dir_and_log_path_for() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let base = tmp.path().join("spool");
        let spooler = LocalLogSpooler::new(base.clone());

        spooler.ensure_dir().await.expect("ensure_dir");
        let meta = std::fs::metadata(&base).expect("spool dir metadata");
        assert!(meta.is_dir());

        let log = spooler.log_path_for("job_123").expect("log_path_for");
        assert_eq!(log, base.join("job_123.log"));
    }

    #[test]
    fn test_log_path_for_rejects_invalid_job_ids() {
        let spooler = LocalLogSpooler::new(PathBuf::from("/tmp/ssh-mcp-test"));
        for job_id in ["", "..", "/abs", "a/b", "a\\b", "job id", "job\n1"] {
            assert!(spooler.log_path_for(job_id).is_err(), "job_id={job_id}");
        }
    }

    #[tokio::test]
    async fn test_cleanup_old_logs_removes_log_and_exit_files_only() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let base = tmp.path().join("spool");
        let spooler = LocalLogSpooler::new(base.clone());
        spooler.ensure_dir().await.expect("ensure_dir");

        let log = base.join("job_1.log");
        let exit = base.join("job_1.exit");
        let keep = base.join("job_1.tmp");
        tokio::fs::write(&log, "hello\n").await.expect("write log");
        tokio::fs::write(&exit, "0\n").await.expect("write exit");
        tokio::fs::write(&keep, "x\n").await.expect("write tmp");

        // Avoid flakiness from tight timing windows by waiting until the newest file
        // is safely older than the max_age threshold.
        wait_until_older_than(&keep, Duration::from_millis(25)).await;
        let removed = spooler
            .cleanup_old_logs(Duration::from_millis(1))
            .await
            .expect("cleanup_old_logs");

        assert!(removed >= 2, "expected to remove at least log+exit");
        assert!(!log.exists(), "log should be removed");
        assert!(!exit.exists(), "exit should be removed");
        assert!(keep.exists(), "non-log file should be kept");
    }
}
