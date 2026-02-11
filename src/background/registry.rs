use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::sync::RwLock;

use super::job::SharedJobState;

/// Thread-safe job registry.
///
/// Stores running jobs and keeps completed jobs for a retention window.
#[derive(Debug, Clone)]
pub struct JobRegistry {
    jobs: Arc<RwLock<HashMap<String, SharedJobState>>>,
    completed_retention: Duration,
}

impl JobRegistry {
    pub fn new(completed_retention: Duration) -> Self {
        Self {
            jobs: Arc::new(RwLock::new(HashMap::new())),
            completed_retention,
        }
    }

    pub fn completed_retention(&self) -> Duration {
        self.completed_retention
    }

    pub async fn insert(&self, job_id: String, job: SharedJobState) {
        let mut guard = self.jobs.write().await;
        guard.insert(job_id, job);
    }

    pub async fn get(&self, job_id: &str) -> Option<SharedJobState> {
        let guard = self.jobs.read().await;
        guard.get(job_id).cloned()
    }

    pub async fn remove(&self, job_id: &str) -> Option<SharedJobState> {
        let mut guard = self.jobs.write().await;
        guard.remove(job_id)
    }

    /// Remove completed jobs that exceeded the retention window.
    ///
    /// Returns the number of pruned jobs.
    pub async fn prune_expired(&self) -> usize {
        let now = SystemTime::now();

        let snapshot: Vec<(String, SharedJobState)> = {
            let guard = self.jobs.read().await;
            guard
                .iter()
                .map(|(id, job)| (id.clone(), Arc::clone(job)))
                .collect()
        };

        let mut expired = Vec::new();
        for (job_id, job) in snapshot {
            let job_for_list = Arc::clone(&job);
            let job_guard = job.lock().await;
            if !job_guard.is_terminal() {
                continue;
            };

            let Some(completed_at) = job_guard.completed_at else {
                continue;
            };

            let Ok(age) = now.duration_since(completed_at) else {
                continue;
            };
            if age > self.completed_retention {
                expired.push((job_id, job_for_list, completed_at));
            }
        }

        if expired.is_empty() {
            return 0;
        }

        let mut guard = self.jobs.write().await;
        let mut removed = 0;
        for (job_id, job, completed_at) in expired {
            let Some(current) = guard.get(&job_id).cloned() else {
                continue;
            };

            // Avoid deleting a new job if a job_id is reused.
            if !Arc::ptr_eq(&current, &job) {
                continue;
            }

            // Re-validate terminal + age under the write lock before removing.
            let Ok(job_guard) = current.try_lock() else {
                continue;
            };
            if !job_guard.is_terminal() {
                continue;
            }
            if job_guard.completed_at != Some(completed_at) {
                continue;
            }
            let Ok(age) = now.duration_since(completed_at) else {
                continue;
            };
            if age <= self.completed_retention {
                continue;
            }

            if guard.remove(&job_id).is_some() {
                removed += 1;
            }
        }
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::background::job::{JobState, NewRunningJob};
    use tokio::sync::Mutex;

    fn make_running(job_id: &str) -> SharedJobState {
        Arc::new(Mutex::new(JobState::new_running(NewRunningJob {
            job_id: job_id.to_string(),
            pid: 123,
            log_path: std::path::PathBuf::from("/tmp/ssh-mcp/test.log"),
            command: "echo test".to_string(),
            connection_id: "test@localhost:22".to_string(),
        })))
    }

    #[tokio::test]
    async fn test_insert_get_remove() {
        let reg = JobRegistry::new(Duration::from_secs(60));

        let job = make_running("job_1");
        reg.insert("job_1".to_string(), Arc::clone(&job)).await;

        let got = reg.get("job_1").await.expect("job should exist");
        assert!(Arc::ptr_eq(&got, &job));

        let removed = reg.remove("job_1").await.expect("job should be removed");
        assert!(Arc::ptr_eq(&removed, &job));
        assert!(reg.get("job_1").await.is_none());
    }

    #[tokio::test]
    async fn test_prune_expired_removes_terminal_jobs_past_retention() {
        let reg = JobRegistry::new(Duration::from_millis(10));

        let running = make_running("job_running");
        reg.insert("job_running".to_string(), Arc::clone(&running))
            .await;

        let completed = make_running("job_completed");
        {
            let mut guard = completed.lock().await;
            guard.mark_exit(0);
            guard.completed_at = guard
                .completed_at
                .and_then(|t| t.checked_sub(Duration::from_secs(1)));
        }
        reg.insert("job_completed".to_string(), Arc::clone(&completed))
            .await;

        let removed = reg.prune_expired().await;
        assert_eq!(removed, 1);
        assert!(reg.get("job_completed").await.is_none());
        assert!(reg.get("job_running").await.is_some());
    }
}
