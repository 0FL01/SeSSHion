use std::path::{Path, PathBuf};
use std::sync::Arc;

use russh::ChannelMsg;
use russh::client;
use tokio::io::AsyncWriteExt;
use tracing::warn;

use crate::background::{JobRegistry, LocalLogSpooler, Result};
#[cfg(unix)]
use crate::platform::O_NOFOLLOW_FLAG;

const STATE_REASON_LIMIT_CHARS: usize = 160;

enum JobTerminalUpdate {
    Exit(i32),
    StateLost(String),
}

fn truncate_state_reason(input: &str) -> String {
    input.chars().take(STATE_REASON_LIMIT_CHARS).collect()
}

fn clamp_exit_status(exit_status: u32) -> i32 {
    if exit_status > 255 {
        255
    } else {
        exit_status as i32
    }
}

async fn open_append_no_symlink(path: &Path) -> Result<tokio::fs::File> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "log path is a symlink (refusing to follow it)",
            )
            .into());
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }

    let mut opts = tokio::fs::OpenOptions::new();
    opts.create(true).append(true);

    #[cfg(unix)]
    {
        opts.custom_flags(O_NOFOLLOW_FLAG);
    }

    match opts.open(path).await {
        Ok(f) => Ok(f),
        Err(e) => {
            if let Ok(meta) = tokio::fs::symlink_metadata(path).await
                && meta.file_type().is_symlink()
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "log path is a symlink (refusing to follow it)",
                )
                .into());
            }
            Err(e.into())
        }
    }
}

pub struct OutputStreamer {
    job_id: String,
    local_log_path: PathBuf,
    registry: Arc<JobRegistry>,
    spooler: Arc<LocalLogSpooler>,
}

impl OutputStreamer {
    pub fn new(
        job_id: String,
        local_log_path: PathBuf,
        registry: Arc<JobRegistry>,
        spooler: Arc<LocalLogSpooler>,
    ) -> Self {
        Self {
            job_id,
            local_log_path,
            registry,
            spooler,
        }
    }

    pub async fn stream_channel(
        self,
        mut channel: russh::Channel<client::Msg>,
        initial_stdout: Vec<u8>,
    ) -> Result<Option<i32>> {
        let res = self
            .stream_channel_inner(&mut channel, initial_stdout)
            .await;

        match res {
            Ok(Some(exit_code)) => {
                self.update_job_terminal(JobTerminalUpdate::Exit(exit_code))
                    .await;
                Ok(Some(exit_code))
            }
            Ok(None) => {
                self.update_job_terminal(JobTerminalUpdate::StateLost(
                    "background_channel_closed_without_exit_status".to_string(),
                ))
                .await;
                Ok(None)
            }
            Err(e) => {
                self.update_job_terminal(JobTerminalUpdate::StateLost(format!(
                    "background_stream_error: {}",
                    truncate_state_reason(&e.to_string())
                )))
                .await;
                Err(e)
            }
        }
    }

    async fn stream_channel_inner(
        &self,
        channel: &mut russh::Channel<client::Msg>,
        initial_stdout: Vec<u8>,
    ) -> Result<Option<i32>> {
        let file = open_append_no_symlink(&self.local_log_path).await?;
        let mut file = tokio::io::BufWriter::new(file);

        if !initial_stdout.is_empty() {
            file.write_all(&initial_stdout).await?;
            file.flush().await?;
        }

        let mut exit_code: Option<i32> = None;
        let mut saw_close_or_eof = false;

        loop {
            let next = if exit_code.is_some() && !saw_close_or_eof {
                match tokio::time::timeout(std::time::Duration::from_millis(200), channel.wait())
                    .await
                {
                    Ok(v) => v,
                    Err(_) => break,
                }
            } else {
                channel.wait().await
            };

            let Some(msg) = next else {
                break;
            };

            match msg {
                ChannelMsg::Data { data } => {
                    file.write_all(data.as_ref()).await?;
                    file.flush().await?;
                }
                ChannelMsg::ExtendedData { data, .. } => {
                    // Phase 2 behavior used `2>&1` remote redirection; preserve a combined stream.
                    file.write_all(data.as_ref()).await?;
                    file.flush().await?;
                }
                ChannelMsg::ExitStatus { exit_status } => {
                    exit_code = Some(clamp_exit_status(exit_status));
                }
                ChannelMsg::ExitSignal { signal_name, .. } => {
                    // Map signal to a conventional shell exit code (128 + signal).
                    let code = match signal_name {
                        russh::Sig::HUP => 129,
                        russh::Sig::INT => 130,
                        russh::Sig::QUIT => 131,
                        russh::Sig::ILL => 132,
                        russh::Sig::ABRT => 134,
                        russh::Sig::FPE => 136,
                        russh::Sig::KILL => 137,
                        russh::Sig::USR1 => 138,
                        russh::Sig::SEGV => 139,
                        russh::Sig::PIPE => 141,
                        russh::Sig::ALRM => 142,
                        russh::Sig::TERM => 143,
                        russh::Sig::Custom(_) => 128,
                    };
                    exit_code = Some(code);
                }
                ChannelMsg::Close | ChannelMsg::Eof => {
                    saw_close_or_eof = true;
                    if exit_code.is_some() {
                        break;
                    }
                }
                _ => {}
            }
        }

        file.flush().await?;
        let inner = file.into_inner();
        inner.sync_all().await?;

        Ok(exit_code)
    }

    async fn update_job_terminal(&self, update: JobTerminalUpdate) {
        let Some(job) = self.registry.get(&self.job_id).await else {
            return;
        };

        let mut guard = job.lock().await;
        match update {
            JobTerminalUpdate::Exit(code) => guard.mark_exit(code),
            JobTerminalUpdate::StateLost(reason) => guard.mark_state_lost(reason),
        }
        let persisted = guard.clone();
        drop(guard);

        if let Err(e) = self.spooler.persist_job_state(&persisted).await {
            warn!(job_id = ?self.job_id, error = ?e, "failed to persist terminal job state");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clamp_exit_status_saturates() {
        assert_eq!(clamp_exit_status(0), 0);
        assert_eq!(clamp_exit_status(255), 255);
        assert_eq!(clamp_exit_status(256), 255);
        assert_eq!(clamp_exit_status(u32::MAX), 255);
    }

    #[tokio::test]
    async fn test_open_append_no_symlink_writes() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let path = tmp.path().join("out.log");

        let mut file = open_append_no_symlink(&path)
            .await
            .expect("open_append_no_symlink");
        file.write_all(b"hello\n").await.expect("write");
        file.sync_all().await.expect("sync");

        let content = tokio::fs::read_to_string(&path).await.expect("read");
        assert!(content.contains("hello"));
    }

    #[cfg(unix)]
    #[test]
    fn test_open_append_no_symlink_rejects_symlink() {
        use std::os::unix::fs::symlink;

        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async {
            let tmp = tempfile::TempDir::new().expect("tempdir");
            let target = tmp.path().join("target.log");
            tokio::fs::write(&target, "x\n")
                .await
                .expect("write target");

            let link = tmp.path().join("link.log");
            symlink(&target, &link).expect("symlink");

            let err = open_append_no_symlink(&link)
                .await
                .expect_err("symlink should be rejected");
            let msg = err.to_string();
            assert!(msg.contains("symlink"), "unexpected error: {msg}");
        });
    }
}
