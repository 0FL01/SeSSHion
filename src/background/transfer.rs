use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use crate::transfer::{CompactTransferResponse, TransferResponse, TransferTransport};
use crate::transfer::{TransferEvent, TransferProgressTarget};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransferJobPhase {
    Queued,
    Preparing,
    Transferring,
    Finalizing,
    Completed,
    Failed,
}

impl TransferJobPhase {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Preparing => "preparing",
            Self::Transferring => "transferring",
            Self::Finalizing => "finalizing",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug)]
pub(crate) struct TransferJobState {
    pub(crate) job_id: String,
    started_at: Instant,
    completed_at: Option<Instant>,
    phase: TransferJobPhase,
    transport: Option<TransferTransport>,
    progress_target: Option<TransferProgressTarget>,
    total_bytes: Option<u64>,
    result: Option<CompactTransferResponse>,
}

impl TransferJobState {
    fn new(job_id: String) -> Self {
        Self {
            job_id,
            started_at: Instant::now(),
            completed_at: None,
            phase: TransferJobPhase::Queued,
            transport: None,
            progress_target: None,
            total_bytes: None,
            result: None,
        }
    }

    pub(crate) fn apply_event(&mut self, event: TransferEvent) {
        if self.completed_at.is_some() {
            return;
        }
        match event {
            TransferEvent::Preparing => self.phase = TransferJobPhase::Preparing,
            TransferEvent::Transferring(transport) => {
                self.phase = TransferJobPhase::Transferring;
                self.transport = Some(transport);
                self.progress_target = None;
                self.total_bytes = None;
            }
            TransferEvent::FileStage {
                target,
                total_bytes,
            } => {
                self.phase = TransferJobPhase::Transferring;
                self.progress_target = Some(target);
                self.total_bytes = total_bytes;
            }
            TransferEvent::Finalizing => self.phase = TransferJobPhase::Finalizing,
        }
    }

    pub(crate) fn finish(&mut self, response: &TransferResponse) {
        self.phase = if response.ok {
            TransferJobPhase::Completed
        } else {
            TransferJobPhase::Failed
        };
        self.completed_at = Some(Instant::now());
        self.result = Some(response.to_compact());
        self.progress_target = None;
    }

    pub(crate) fn is_terminal(&self) -> bool {
        self.completed_at.is_some()
    }

    fn snapshot(&self) -> TransferJobSnapshot {
        TransferJobSnapshot {
            job_id: self.job_id.clone(),
            state: if self.is_terminal() {
                self.phase.as_str().to_string()
            } else {
                "running".to_string()
            },
            running: !self.is_terminal(),
            phase: self.phase,
            elapsed_ms: self.started_at.elapsed().as_millis() as u64,
            transport: self.transport,
            progress_target: self.progress_target.clone(),
            total_bytes: self.total_bytes,
            result: self.result.clone(),
        }
    }
}

pub(crate) type SharedTransferJob = Arc<Mutex<TransferJobState>>;

#[derive(Debug, Clone)]
pub(crate) struct TransferJobSnapshot {
    pub(crate) job_id: String,
    pub(crate) state: String,
    pub(crate) running: bool,
    pub(crate) phase: TransferJobPhase,
    pub(crate) elapsed_ms: u64,
    pub(crate) transport: Option<TransferTransport>,
    pub(crate) progress_target: Option<TransferProgressTarget>,
    pub(crate) total_bytes: Option<u64>,
    pub(crate) result: Option<CompactTransferResponse>,
}

#[derive(Debug)]
pub(crate) struct TransferJobRegistry {
    jobs: RwLock<HashMap<String, SharedTransferJob>>,
    completed_retention: Duration,
}

impl TransferJobRegistry {
    pub(crate) fn new(completed_retention: Duration) -> Self {
        Self {
            jobs: RwLock::new(HashMap::new()),
            completed_retention,
        }
    }

    pub(crate) fn register(&self, job_id: String) -> SharedTransferJob {
        self.prune_expired();
        let job = Arc::new(Mutex::new(TransferJobState::new(job_id.clone())));
        if let Ok(mut jobs) = self.jobs.write() {
            jobs.insert(job_id, Arc::clone(&job));
        }
        job
    }

    pub(crate) fn snapshot(&self, job_id: &str) -> Option<TransferJobSnapshot> {
        self.prune_expired();
        let job = self.jobs.read().ok()?.get(job_id).cloned()?;
        job.lock().ok().map(|state| state.snapshot())
    }

    pub(crate) fn contains(&self, job_id: &str) -> bool {
        self.prune_expired();
        self.jobs.read().is_ok_and(|jobs| jobs.contains_key(job_id))
    }

    pub(crate) fn prune_expired(&self) {
        let Ok(mut jobs) = self.jobs.write() else {
            return;
        };
        jobs.retain(|_, job| {
            let Ok(state) = job.lock() else {
                return true;
            };
            state
                .completed_at
                .is_none_or(|completed| completed.elapsed() <= self.completed_retention)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transfer::{TransferParams, TransferResponse};
    use std::path::Path;

    #[test]
    fn registered_job_is_immediately_visible_and_terminal_result_is_retained() {
        let registry = TransferJobRegistry::new(Duration::from_secs(60));
        let job = registry.register("transfer-1".to_string());
        let queued = registry.snapshot("transfer-1").expect("queued job");
        assert!(queued.running);
        assert_eq!(queued.phase, TransferJobPhase::Queued);

        let response = TransferResponse::error(
            TransferParams::default(),
            Path::new("/tmp"),
            "expected failure",
        );
        job.lock().expect("job lock").finish(&response);

        let failed = registry.snapshot("transfer-1").expect("failed job");
        assert!(!failed.running);
        assert_eq!(failed.state, "failed");
        assert_eq!(
            failed.result.and_then(|result| result.error),
            Some("expected failure".to_string())
        );
    }
}
