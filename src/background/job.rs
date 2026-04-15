use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

/// Status of a background job as tracked locally.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Running,
    Completed,
    Failed,
    StateLost,
}

impl JobStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::StateLost => "state_lost",
        }
    }
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

    /// Why the state became untrustworthy, if applicable.
    pub state_reason: Option<String>,

    /// When job execution started.
    pub start_time: SystemTime,

    /// When job execution completed (if terminal).
    pub completed_at: Option<SystemTime>,

    /// Original command string.
    pub command: String,

    /// Opaque connection identifier (resolved by an external manager).
    pub connection_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PersistedJobState {
    pub version: u8,
    pub job_id: String,
    pub pid: u32,
    pub log_path: String,
    pub exit_code: Option<i32>,
    pub status: JobStatus,
    pub state_reason: Option<String>,
    pub start_time_unix_ms: u64,
    pub completed_at_unix_ms: Option<u64>,
    pub command: String,
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
            state_reason: None,
            start_time: SystemTime::now(),
            completed_at: None,
            command: opts.command,
            connection_id: opts.connection_id,
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status,
            JobStatus::Completed | JobStatus::Failed | JobStatus::StateLost
        )
    }

    pub fn elapsed_time(&self) -> String {
        let end_time = self.completed_at.unwrap_or_else(SystemTime::now);
        let elapsed = end_time.duration_since(self.start_time).unwrap_or_default();
        format_ps_elapsed(elapsed)
    }

    pub fn mark_exit(&mut self, exit_code: i32) {
        self.exit_code = Some(exit_code);
        self.completed_at.get_or_insert_with(SystemTime::now);
        self.status = if exit_code == 0 {
            JobStatus::Completed
        } else {
            JobStatus::Failed
        };
        self.state_reason = None;
    }

    pub fn mark_state_lost(&mut self, reason: impl Into<String>) {
        self.exit_code = None;
        self.completed_at.get_or_insert_with(SystemTime::now);
        self.status = JobStatus::StateLost;
        self.state_reason = Some(reason.into());
    }

    pub(crate) fn to_persisted(&self) -> PersistedJobState {
        PersistedJobState {
            version: 1,
            job_id: self.job_id.clone(),
            pid: self.pid,
            log_path: self.log_path.to_string_lossy().to_string(),
            exit_code: self.exit_code,
            status: self.status,
            state_reason: self.state_reason.clone(),
            start_time_unix_ms: system_time_to_unix_ms(self.start_time),
            completed_at_unix_ms: self.completed_at.map(system_time_to_unix_ms),
            command: self.command.clone(),
            connection_id: self.connection_id.clone(),
        }
    }

    pub(crate) fn from_persisted(
        persisted: PersistedJobState,
    ) -> std::result::Result<Self, &'static str> {
        if persisted.version != 1 {
            return Err("unsupported persisted job state version");
        }

        Ok(Self {
            job_id: persisted.job_id,
            pid: persisted.pid,
            log_path: PathBuf::from(persisted.log_path),
            exit_code: persisted.exit_code,
            status: persisted.status,
            state_reason: persisted.state_reason,
            start_time: unix_ms_to_system_time(persisted.start_time_unix_ms),
            completed_at: persisted.completed_at_unix_ms.map(unix_ms_to_system_time),
            command: persisted.command,
            connection_id: persisted.connection_id,
        })
    }
}

fn system_time_to_unix_ms(value: SystemTime) -> u64 {
    value
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

fn unix_ms_to_system_time(value: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(value)
}
