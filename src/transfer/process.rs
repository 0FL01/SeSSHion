use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::error::{Result, SshMcpError};

pub(super) fn classify_spawn_error_with_reason(
    err: std::io::Error,
    transport: super::TransferTransport,
    reason: String,
) -> SshMcpError {
    if err.kind() == std::io::ErrorKind::NotFound {
        return SshMcpError::config(format!("transport {transport:?} unsupported: {reason}"));
    }
    SshMcpError::Io(err)
}

#[derive(Debug)]
pub(super) struct CapturedOutput {
    pub(super) status: std::process::ExitStatus,
    pub(super) stdout: Vec<u8>,
    pub(super) stderr: Vec<u8>,
}

pub(super) fn configure_child_command(command: &mut Command) {
    command.kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
}

pub(super) async fn terminate_child(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(raw_pid) = child.id().and_then(|id| i32::try_from(id).ok())
        && let Some(pid) = rustix::process::Pid::from_raw(raw_pid)
    {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }

    let _ = child.kill().await;
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
}

pub(super) async fn wait_child_with_timeout(
    mut child: tokio::process::Child,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<CapturedOutput> {
    let mut stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| SshMcpError::connection("missing stdout pipe"))?;
    let mut stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| SshMcpError::connection("missing stderr pipe"))?;

    let stdout_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        stdout_pipe.read_to_end(&mut buf).await?;
        Ok::<Vec<u8>, std::io::Error>(buf)
    });

    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        stderr_pipe.read_to_end(&mut buf).await?;
        Ok::<Vec<u8>, std::io::Error>(buf)
    });

    let sleep = tokio::time::sleep(timeout);
    tokio::pin!(sleep);

    let status = tokio::select! {
        res = child.wait() => {
            res?
        }
        _ = &mut sleep => {
            stdout_task.abort();
            stderr_task.abort();
            terminate_child(&mut child).await;
            let _ = tokio::join!(stdout_task, stderr_task);
            return Err(SshMcpError::Timeout(timeout.as_millis() as u64));
        }
        _ = cancellation.cancelled() => {
            stdout_task.abort();
            stderr_task.abort();
            terminate_child(&mut child).await;
            let _ = tokio::join!(stdout_task, stderr_task);
            return Err(SshMcpError::connection("transfer cancelled"));
        }
    };

    let stdout = match stdout_task.await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return Err(SshMcpError::Io(e)),
        Err(_) => {
            return Err(SshMcpError::connection("stdout task join failed"));
        }
    };

    let stderr = match stderr_task.await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return Err(SshMcpError::Io(e)),
        Err(_) => {
            return Err(SshMcpError::connection("stderr task join failed"));
        }
    };

    Ok(CapturedOutput {
        status,
        stdout,
        stderr,
    })
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::process::Stdio;

    use super::*;

    #[test]
    fn missing_client_is_config_error_for_selected_transport() {
        for transport in [
            super::super::TransferTransport::Sftp,
            super::super::TransferTransport::Scp,
            super::super::TransferTransport::Rsync,
        ] {
            let error = classify_spawn_error_with_reason(
                std::io::Error::new(std::io::ErrorKind::NotFound, "client unavailable"),
                transport,
                "missing selected client".to_string(),
            );
            let SshMcpError::Config(reason) = error else {
                panic!("missing client must report a configuration error");
            };
            assert!(reason.contains(&format!("{transport:?}")));
            assert!(reason.contains("missing selected client"));
        }
    }

    #[test]
    fn other_spawn_errors_preserve_io_error() {
        let error = classify_spawn_error_with_reason(
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "client not executable",
            ),
            super::super::TransferTransport::Sftp,
            "missing selected client".to_string(),
        );
        let SshMcpError::Io(error) = error else {
            panic!("non-missing client spawn errors must remain IO errors");
        };
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(error.to_string(), "client not executable");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_kills_and_reaps_process_group() {
        let mut command = Command::new("sh");
        command
            .args(["-c", "sleep 30"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_child_command(&mut command);

        let child = command.spawn().expect("spawn child process group");
        let raw_pid = i32::try_from(child.id().expect("child pid")).expect("pid fits i32");
        let cancellation = CancellationToken::new();
        let cancel_after_spawn = cancellation.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            cancel_after_spawn.cancel();
        });

        let error = wait_child_with_timeout(child, Duration::from_secs(30), &cancellation)
            .await
            .expect_err("cancelled child should fail");
        assert!(error.to_string().contains("transfer cancelled"));

        let pid = rustix::process::Pid::from_raw(raw_pid).expect("nonzero pid");
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                match rustix::process::kill_process_group(pid, rustix::process::Signal::KILL) {
                    Err(rustix::io::Errno::SRCH) => break,
                    Ok(()) => tokio::time::sleep(Duration::from_millis(10)).await,
                    Err(error) => panic!("failed to check process group: {error}"),
                }
            }
        })
        .await
        .expect("process group should be gone after cancellation");
    }
}
