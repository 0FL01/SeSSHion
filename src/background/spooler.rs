use std::env;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::warn;

use super::job::{JobState, PersistedJobState};
use super::{BackgroundError, Result};
#[cfg(unix)]
use crate::platform::O_NOFOLLOW_FLAG;

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
        Self::new(env::temp_dir().join("ssh-mcp"))
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

    pub fn state_path_for(&self, job_id: &str) -> Result<PathBuf> {
        validate_job_id(job_id)?;
        Ok(self.base_dir.join(format!("{job_id}.state")))
    }

    pub async fn persist_job_state(&self, job: &JobState) -> Result<()> {
        self.ensure_dir().await?;

        if job.log_path.parent() != Some(self.base_dir()) {
            return Err(BackgroundError::InvalidState {
                message: "job log path is outside spool directory",
            });
        }

        let path = self.state_path_for(&job.job_id)?;
        let payload =
            serde_json::to_vec(&job.to_persisted()).map_err(|_| BackgroundError::InvalidState {
                message: "failed to serialize persisted job state",
            })?;

        let mut file = open_spool_write_no_symlink(&path).await?;
        file.write_all(&payload).await?;
        file.sync_all().await?;
        Ok(())
    }

    pub async fn load_job_state(&self, job_id: &str) -> Result<Option<JobState>> {
        self.ensure_dir().await?;
        let path = self.state_path_for(job_id)?;

        let mut file = match open_spool_read_no_symlink(&path).await? {
            Some(file) => file,
            None => return Ok(None),
        };

        let mut payload = Vec::new();
        file.read_to_end(&mut payload).await?;

        let persisted: PersistedJobState =
            serde_json::from_slice(&payload).map_err(|_| BackgroundError::InvalidState {
                message: "failed to parse persisted job state",
            })?;
        let job = JobState::from_persisted(persisted)
            .map_err(|message| BackgroundError::InvalidState { message })?;

        if job.job_id != job_id {
            return Err(BackgroundError::InvalidState {
                message: "persisted job id does not match requested job id",
            });
        }
        if job.log_path.parent() != Some(self.base_dir()) {
            return Err(BackgroundError::InvalidState {
                message: "persisted log path is outside spool directory",
            });
        }

        Ok(Some(job))
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
            if ext != "log" && ext != "exit" && ext != "state" {
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

async fn open_spool_write_no_symlink(path: &Path) -> Result<tokio::fs::File> {
    match fs::symlink_metadata(path).await {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(BackgroundError::InvalidState {
                message: "spool metadata path is a symlink",
            });
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }

    let mut opts = tokio::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);

    #[cfg(unix)]
    {
        opts.custom_flags(O_NOFOLLOW_FLAG);
    }

    match opts.open(path).await {
        Ok(file) => Ok(file),
        Err(e) => {
            if let Ok(meta) = fs::symlink_metadata(path).await
                && meta.file_type().is_symlink()
            {
                return Err(BackgroundError::InvalidState {
                    message: "spool metadata path is a symlink",
                });
            }
            Err(e.into())
        }
    }
}

async fn open_spool_read_no_symlink(path: &Path) -> Result<Option<tokio::fs::File>> {
    match fs::symlink_metadata(path).await {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(BackgroundError::InvalidState {
                    message: "spool metadata path is a symlink",
                });
            }
            if !meta.is_file() {
                return Err(BackgroundError::InvalidState {
                    message: "spool metadata path is not a regular file",
                });
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    }

    let mut opts = tokio::fs::OpenOptions::new();
    opts.read(true);

    #[cfg(unix)]
    {
        opts.custom_flags(O_NOFOLLOW_FLAG);
    }

    match opts.open(path).await {
        Ok(file) => Ok(Some(file)),
        Err(e) => {
            if let Ok(meta) = fs::symlink_metadata(path).await
                && meta.file_type().is_symlink()
            {
                return Err(BackgroundError::InvalidState {
                    message: "spool metadata path is a symlink",
                });
            }
            Err(e.into())
        }
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
        let state = spooler.state_path_for("job_123").expect("state_path_for");
        assert_eq!(state, base.join("job_123.state"));
    }

    #[test]
    fn test_log_path_for_rejects_invalid_job_ids() {
        let spooler = LocalLogSpooler::new(PathBuf::from("/tmp/ssh-mcp-test"));
        for job_id in ["", "..", "/abs", "a/b", "a\\b", "job id", "job\n1"] {
            assert!(spooler.log_path_for(job_id).is_err(), "job_id={job_id}");
        }
    }

    #[tokio::test]
    async fn test_cleanup_old_logs_removes_log_exit_and_state_files_only() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let base = tmp.path().join("spool");
        let spooler = LocalLogSpooler::new(base.clone());
        spooler.ensure_dir().await.expect("ensure_dir");

        let log = base.join("job_1.log");
        let exit = base.join("job_1.exit");
        let state = base.join("job_1.state");
        let keep = base.join("job_1.tmp");
        tokio::fs::write(&log, "hello\n").await.expect("write log");
        tokio::fs::write(&exit, "0\n").await.expect("write exit");
        tokio::fs::write(&state, "{}\n").await.expect("write state");
        tokio::fs::write(&keep, "x\n").await.expect("write tmp");

        // Avoid flakiness from tight timing windows by waiting until the newest file
        // is safely older than the max_age threshold.
        wait_until_older_than(&keep, Duration::from_millis(25)).await;
        let removed = spooler
            .cleanup_old_logs(Duration::from_millis(1))
            .await
            .expect("cleanup_old_logs");

        assert!(removed >= 3, "expected to remove at least log+exit+state");
        assert!(!log.exists(), "log should be removed");
        assert!(!exit.exists(), "exit should be removed");
        assert!(!state.exists(), "state should be removed");
        assert!(keep.exists(), "non-log file should be kept");
    }

    #[tokio::test]
    async fn test_persist_and_load_job_state_round_trip() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let base = tmp.path().join("spool");
        let spooler = LocalLogSpooler::new(base.clone());
        spooler.ensure_dir().await.expect("ensure_dir");

        let mut job = JobState::new_running(super::super::job::NewRunningJob {
            job_id: "job_123".to_string(),
            pid: 4242,
            log_path: base.join("job_123.log"),
            command: "wget https://example.test/file".to_string(),
            connection_id: "test@localhost:22".to_string(),
        });
        job.mark_state_lost("stream_error");

        spooler
            .persist_job_state(&job)
            .await
            .expect("persist_job_state");

        let loaded = spooler
            .load_job_state("job_123")
            .await
            .expect("load_job_state")
            .expect("job should exist");

        assert_eq!(loaded.job_id, job.job_id);
        assert_eq!(loaded.pid, job.pid);
        assert_eq!(loaded.status, job.status);
        assert_eq!(loaded.exit_code, job.exit_code);
        assert_eq!(loaded.state_reason, job.state_reason);
        assert_eq!(loaded.command, job.command);
        assert_eq!(loaded.log_path, job.log_path);
    }
}
