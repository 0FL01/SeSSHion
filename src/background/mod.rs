//! Local background job state and log spooling.
//!
//! The core types are:
//! - [`JobState`]: per-job state (pid, local log path, exit code, status)
//! - [`JobRegistry`]: thread-safe registry for active + recently completed jobs
//! - [`LocalLogSpooler`]: configured or per-user local spool directory management

pub mod job;
pub(crate) mod marker;
pub mod registry;
pub(crate) mod response;
pub mod spooler;
pub mod stream;
pub(crate) mod transfer;
pub(crate) mod wrapper;

pub use job::{JobState, JobStatus, SharedJobState};
pub use registry::JobRegistry;
pub use spooler::LocalLogSpooler;
pub use stream::OutputStreamer;

use thiserror::Error;

/// Background subsystem errors.
#[derive(Debug, Error)]
pub enum BackgroundError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("invalid job id: {job_id}")]
    InvalidJobId { job_id: String },

    #[error("job not found: {job_id}")]
    JobNotFound { job_id: String },

    #[error("invalid job state: {message}")]
    InvalidState { message: &'static str },

    #[error("time error: {0}")]
    Time(#[from] std::time::SystemTimeError),
}

pub type Result<T> = std::result::Result<T, BackgroundError>;
