use std::time::Duration;

use tokio::io::AsyncReadExt;

use crate::error::SshMcpError;

use super::TransportAttemptError;

pub(super) fn classify_spawn_error_with_reason(
    err: std::io::Error,
    transport: super::TransferTransport,
    reason: String,
) -> TransportAttemptError {
    if err.kind() == std::io::ErrorKind::NotFound {
        return TransportAttemptError::Unsupported { transport, reason };
    }
    TransportAttemptError::Other(SshMcpError::Io(err))
}

#[derive(Debug)]
pub(super) struct CapturedOutput {
    pub(super) status: std::process::ExitStatus,
    pub(super) stdout: Vec<u8>,
    pub(super) stderr: Vec<u8>,
}

pub(super) async fn wait_child_with_timeout(
    mut child: tokio::process::Child,
    timeout: Duration,
) -> std::result::Result<CapturedOutput, TransportAttemptError> {
    let mut stdout_pipe = child.stdout.take().ok_or_else(|| {
        TransportAttemptError::Other(SshMcpError::connection("missing stdout pipe"))
    })?;
    let mut stderr_pipe = child.stderr.take().ok_or_else(|| {
        TransportAttemptError::Other(SshMcpError::connection("missing stderr pipe"))
    })?;

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
            res.map_err(super::io_to_transport_attempt)?
        }
        _ = &mut sleep => {
            stdout_task.abort();
            stderr_task.abort();
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(TransportAttemptError::Other(SshMcpError::Timeout(
                timeout.as_millis() as u64,
            )));
        }
    };

    let stdout = match stdout_task.await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return Err(TransportAttemptError::Other(SshMcpError::Io(e))),
        Err(_) => {
            return Err(TransportAttemptError::Other(SshMcpError::connection(
                "stdout task join failed",
            )));
        }
    };

    let stderr = match stderr_task.await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return Err(TransportAttemptError::Other(SshMcpError::Io(e))),
        Err(_) => {
            return Err(TransportAttemptError::Other(SshMcpError::connection(
                "stderr task join failed",
            )));
        }
    };

    Ok(CapturedOutput {
        status,
        stdout,
        stderr,
    })
}
