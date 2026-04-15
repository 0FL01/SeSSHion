use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::sync::Mutex;

/// Status of a background job as tracked locally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    Running,
    Completed,
    Failed,
}

/// Shared, async-mutable job state handle.
pub type SharedJobState = Arc<Mutex<JobState>>;

/// Local representation of a background job running on a remote host.
#[derive(Debug, Clone)]
pub struct JobState {
    /// Unique job id returned to callers.
    pub job_id: String,

    /// Remote process id.
    pub pid: u32,

    /// Local log path (spooled on the MCP server host).
    pub log_path: PathBuf,

    /// Exit code captured when the remote process completes.
    pub exit_code: Option<i32>,

    /// Current status.
    pub status: JobStatus,

    /// When job execution started.
    pub start_time: SystemTime,

    /// When job execution completed (if terminal).
    pub completed_at: Option<SystemTime>,

    /// Original command string.
    pub command: String,

    /// Opaque connection identifier (resolved by an external manager).
    pub connection_id: String,
}

#[derive(Debug, Clone)]
pub struct NewRunningJob {
    pub job_id: String,
    pub pid: u32,
    pub log_path: PathBuf,
    pub command: String,
    pub connection_id: String,
}

fn format_ps_elapsed(elapsed: Duration) -> String {
    let total_secs = elapsed.as_secs();
    let days = total_secs / 86_400;
    let hours = (total_secs % 86_400) / 3_600;
    let minutes = (total_secs % 3_600) / 60;
    let seconds = total_secs % 60;

    if days > 0 {
        format!("{days}-{hours:02}:{minutes:02}:{seconds:02}")
    } else if hours > 0 {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

impl JobState {
    pub fn new_running(opts: NewRunningJob) -> Self {
        Self {
            job_id: opts.job_id,
            pid: opts.pid,
            log_path: opts.log_path,
            exit_code: None,
            status: JobStatus::Running,
            start_time: SystemTime::now(),
            completed_at: None,
            command: opts.command,
            connection_id: opts.connection_id,
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self.status, JobStatus::Completed | JobStatus::Failed)
    }

    pub fn elapsed_time(&self) -> String {
        let end_time = self.completed_at.unwrap_or_else(SystemTime::now);
        let elapsed = end_time.duration_since(self.start_time).unwrap_or_default();
        format_ps_elapsed(elapsed)
    }

    pub fn mark_exit(&mut self, exit_code: i32) {
        self.exit_code = Some(exit_code);
        self.completed_at = Some(SystemTime::now());
        self.status = if exit_code == 0 {
            JobStatus::Completed
        } else {
            JobStatus::Failed
        };
    }

    pub fn mark_failed(&mut self) {
        self.exit_code = None;
        self.completed_at = Some(SystemTime::now());
        self.status = JobStatus::Failed;
    }
}
