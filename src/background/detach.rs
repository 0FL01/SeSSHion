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
) -> DetachMode
where
    MakeJobId: Fn() -> String,
    Exec: Fn(DetachProbeRequest, Duration) -> Fut,
    Fut: Future<Output = Result<DetachProbeOutput>>,
{
    let cached = DetachMode::from_u8(cache.load(Ordering::Acquire));
    if cached != DetachMode::Unknown {
        return cached;
    }

    let _guard = cache_lock.lock().await;
    let cached = DetachMode::from_u8(cache.load(Ordering::Acquire));
    if cached != DetachMode::Unknown {
        return cached;
    }

    let full_supported = match probe_detach_mode(DetachMode::Full, &make_job_id, &exec).await {
        Ok(true) => true,
        Ok(false) => false,
        Err(e) => {
            debug!(error = ?e, "full detach probe failed");
            false
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
                false
            }
        }
    };

    let selected = select_detach_mode(full_supported, portable_supported);
    // Cache a non-Unknown decision to avoid repeated probes (and /tmp litter).
    cache.store(selected.as_u8(), Ordering::Release);
    selected
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
}
