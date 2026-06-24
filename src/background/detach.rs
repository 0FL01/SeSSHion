use std::future::Future;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use tokio::sync::Mutex;
use tracing::debug;

use crate::background::marker::parse_background_markers;
use crate::background::wrapper::{
    build_background_wrapper_script_full, build_background_wrapper_script_portable,
    remote_job_log_path,
};
use crate::error::Result;
use crate::shell_escape::escape_for_shell;

fn is_safe_job_id(job_id: &str) -> bool {
    !job_id.is_empty()
        && job_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DetachMode {
    Unknown = 0,
    Full = 1,
    Portable = 2,
    DirectOnly = 3,
}

impl DetachMode {
    pub(crate) fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Full,
            2 => Self::Portable,
            3 => Self::DirectOnly,
            _ => Self::Unknown,
        }
    }

    pub(crate) fn as_u8(self) -> u8 {
        self as u8
    }
}

#[derive(Debug, Clone)]
pub(crate) struct DetachProbeOutput {
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) exit_code: Option<u32>,
}

#[derive(Debug, Clone)]
pub(crate) struct DetachProbeRequest {
    pub(crate) wrapper: String,
}

fn select_detach_mode(full_supported: bool, portable_supported: bool) -> DetachMode {
    if full_supported {
        DetachMode::Full
    } else if portable_supported {
        DetachMode::Portable
    } else {
        DetachMode::DirectOnly
    }
}

pub(crate) async fn determine_detach_mode<MakeJobId, Exec, Fut>(
    cache: &AtomicU8,
    cache_lock: &Mutex<()>,
    make_job_id: MakeJobId,
    exec: Exec,
) -> Result<DetachMode>
where
    MakeJobId: Fn() -> String,
    Exec: Fn(DetachProbeRequest, Duration) -> Fut,
    Fut: Future<Output = Result<DetachProbeOutput>>,
{
    let cached = DetachMode::from_u8(cache.load(Ordering::Acquire));
    if cached != DetachMode::Unknown {
        return Ok(cached);
    }

    let _guard = cache_lock.lock().await;
    let cached = DetachMode::from_u8(cache.load(Ordering::Acquire));
    if cached != DetachMode::Unknown {
        return Ok(cached);
    }

    let full_supported = match probe_detach_mode(DetachMode::Full, &make_job_id, &exec).await {
        Ok(true) => true,
        Ok(false) => false,
        Err(e) => {
            debug!(error = ?e, "full detach probe failed");
            return Err(e);
        }
    };

    let portable_supported = if full_supported {
        false
    } else {
        match probe_detach_mode(DetachMode::Portable, &make_job_id, &exec).await {
            Ok(true) => true,
            Ok(false) => false,
            Err(e) => {
                debug!(error = ?e, "portable detach probe failed");
                return Err(e);
            }
        }
    };

    let selected = select_detach_mode(full_supported, portable_supported);
    // Cache only positively verified detach capabilities.  DirectOnly is a
    // negative observation, not a capability for background jobs; caching it
    // would turn any transient probe failure into a permanent process-wide
    // denial until MCP restart.
    if selected != DetachMode::DirectOnly {
        cache.store(selected.as_u8(), Ordering::Release);
    }

    Ok(selected)
}

pub(crate) async fn probe_detach_mode<MakeJobId, Exec, Fut>(
    mode: DetachMode,
    make_job_id: &MakeJobId,
    exec: &Exec,
) -> Result<bool>
where
    MakeJobId: Fn() -> String,
    Exec: Fn(DetachProbeRequest, Duration) -> Fut,
    Fut: Future<Output = Result<DetachProbeOutput>>,
{
    if matches!(mode, DetachMode::Unknown | DetachMode::DirectOnly) {
        return Ok(false);
    }

    let job_id = make_job_id();
    if !is_safe_job_id(&job_id) {
        debug!(
            job_id = ?job_id,
            "autodetect probe job_id rejected (unsafe characters)"
        );
        return Ok(false);
    }
    let marker = format!("__SSH_MCP_AUTODETECT_OK={job_id}");
    let probe_command = format!("printf '%s\\n' '{}'", escape_for_shell(&marker));

    let remote_log_path = remote_job_log_path(&job_id);
    let wrapper = if mode == DetachMode::Full {
        build_background_wrapper_script_full(&job_id, &probe_command, &remote_log_path)
    } else {
        build_background_wrapper_script_portable(&job_id, &probe_command, &remote_log_path)
    };

    let start_output = exec(DetachProbeRequest { wrapper }, Duration::from_secs(5)).await?;

    let markers = match parse_background_markers(&start_output.stdout, &job_id, &remote_log_path) {
        Ok(m) => m,
        Err(parse_err) => {
            debug!(
                mode = ?mode,
                error = ?parse_err,
                exit_code = ?start_output.exit_code,
                stderr_len = start_output.stderr.len(),
                "detach probe markers parse failed"
            );
            return Ok(false);
        }
    };

    Ok(start_output.exit_code == Some(0)
        && start_output.stderr.is_empty()
        && markers.remote_log_path == remote_log_path
        && start_output.stdout.contains(&marker))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn test_select_detach_mode_ladder() {
        assert_eq!(select_detach_mode(true, true), DetachMode::Full);
        assert_eq!(select_detach_mode(true, false), DetachMode::Full);
        assert_eq!(select_detach_mode(false, true), DetachMode::Portable);
        assert_eq!(select_detach_mode(false, false), DetachMode::DirectOnly);
    }

    #[test]
    fn test_detach_mode_u8_roundtrip() {
        assert_eq!(DetachMode::from_u8(0), DetachMode::Unknown);
        assert_eq!(DetachMode::from_u8(1), DetachMode::Full);
        assert_eq!(DetachMode::from_u8(2), DetachMode::Portable);
        assert_eq!(DetachMode::from_u8(3), DetachMode::DirectOnly);
        assert_eq!(DetachMode::from_u8(9), DetachMode::Unknown);
        assert_eq!(DetachMode::Full.as_u8(), 1);
    }

    fn successful_probe_output(job_id: &str) -> DetachProbeOutput {
        let remote_log_path = remote_job_log_path(job_id);
        DetachProbeOutput {
            stdout: format!(
                "__SSH_MCP_JOB_ID={job_id}\n\
                 __SSH_MCP_PID=123\n\
                 __SSH_MCP_LOG={remote_log_path}\n\
                 __SSH_MCP_AUTODETECT_OK={job_id}\n"
            ),
            stderr: String::new(),
            exit_code: Some(0),
        }
    }

    fn failed_wrapper_probe_output() -> DetachProbeOutput {
        DetachProbeOutput {
            stdout: String::new(),
            stderr: "sh: not found".to_string(),
            exit_code: Some(127),
        }
    }

    #[tokio::test]
    async fn transient_probe_error_does_not_cache_directonly_and_reprobes_after_recovery() {
        let cache = AtomicU8::new(DetachMode::Unknown.as_u8());
        let cache_lock = Mutex::new(());

        let exec_call_count = Arc::new(AtomicU8::new(0));
        let exec_call_count_clone = exec_call_count.clone();

        let result_after_failure = determine_detach_mode(
            &cache,
            &cache_lock,
            || "test-probe-fail".to_string(),
            move |_req, _timeout| {
                let count = exec_call_count_clone.clone();
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    Err(crate::error::SshMcpError::Connection(
                        "transient SSH error: connection refused".to_string(),
                    ))
                }
            },
        )
        .await;

        assert!(
            result_after_failure.is_err(),
            "transient probe errors must be returned, not converted to DirectOnly"
        );
        assert_eq!(
            exec_call_count.load(Ordering::SeqCst),
            1,
            "Full probe failure should stop detection as inconclusive"
        );
        assert_eq!(
            DetachMode::from_u8(cache.load(Ordering::SeqCst)),
            DetachMode::Unknown,
            "transient probe errors must leave the cache Unknown"
        );

        let exec_call_count_after = Arc::new(AtomicU8::new(0));
        let exec_call_count_after_clone = exec_call_count_after.clone();

        let mode_after_recovery = determine_detach_mode(
            &cache,
            &cache_lock,
            || "test-probe-recover".to_string(),
            move |_req, _timeout| {
                let count = exec_call_count_after_clone.clone();
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    Ok(successful_probe_output("test-probe-recover"))
                }
            },
        )
        .await;

        assert_eq!(
            mode_after_recovery.expect("recovered probe should succeed"),
            DetachMode::Full
        );
        assert_eq!(
            exec_call_count_after.load(Ordering::SeqCst),
            1,
            "cache must be re-probed after an inconclusive failure"
        );
        assert_eq!(
            DetachMode::from_u8(cache.load(Ordering::SeqCst)),
            DetachMode::Full,
            "positive probe result should still be cached"
        );
    }

    #[tokio::test]
    async fn negative_probe_result_does_not_cache_directonly_and_reprobes_after_recovery() {
        let cache = AtomicU8::new(DetachMode::Unknown.as_u8());
        let cache_lock = Mutex::new(());

        let exec_call_count = Arc::new(AtomicU8::new(0));
        let exec_call_count_clone = exec_call_count.clone();

        let mode_after_negative_probe = determine_detach_mode(
            &cache,
            &cache_lock,
            || "test-probe-no-sh".to_string(),
            move |_req, _timeout| {
                let count = exec_call_count_clone.clone();
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    Ok(failed_wrapper_probe_output())
                }
            },
        )
        .await;

        assert_eq!(
            mode_after_negative_probe.expect("completed negative probes should return a mode"),
            DetachMode::DirectOnly
        );
        assert_eq!(
            exec_call_count.load(Ordering::SeqCst),
            2,
            "Full and Portable probes should both be attempted before DirectOnly"
        );
        assert_eq!(
            DetachMode::from_u8(cache.load(Ordering::SeqCst)),
            DetachMode::Unknown,
            "DirectOnly is a negative observation and must not be cached forever"
        );

        let exec_call_count_after = Arc::new(AtomicU8::new(0));
        let exec_call_count_after_clone = exec_call_count_after.clone();

        let mode_after_recovery = determine_detach_mode(
            &cache,
            &cache_lock,
            || "test-probe-recover".to_string(),
            move |_req, _timeout| {
                let count = exec_call_count_after_clone.clone();
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    Ok(successful_probe_output("test-probe-recover"))
                }
            },
        )
        .await;

        assert_eq!(
            mode_after_recovery.expect("recovered probe should succeed"),
            DetachMode::Full
        );
        assert_eq!(
            exec_call_count_after.load(Ordering::SeqCst),
            1,
            "negative DirectOnly result must not short-circuit recovery re-probe"
        );
        assert_eq!(
            DetachMode::from_u8(cache.load(Ordering::SeqCst)),
            DetachMode::Full,
            "positive recovery probe should be cached"
        );
    }
}
