use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BackgroundMarkers {
    pub(crate) job_id: String,
    pub(crate) pid: u32,
    pub(crate) remote_log_path: String,
}

pub(crate) async fn read_background_markers_from_channel(
    channel: &mut russh::Channel<russh::client::Msg>,
    expected_job_id: &str,
    expected_log_path: &str,
    timeout_duration: Duration,
) -> std::result::Result<(BackgroundMarkers, Vec<u8>), String> {
    let mut stdout_buf: Vec<u8> = Vec::with_capacity(256);
    let mut marker_stdout = String::new();
    let mut parsed_lines = 0usize;
    let mut line_start = 0usize;

    let fut = async {
        while parsed_lines < 3 {
            let Some(msg) = channel.wait().await else {
                return Err("channel ended before background markers".to_string());
            };

            match msg {
                russh::ChannelMsg::Data { data } => {
                    stdout_buf.extend_from_slice(data.as_ref());

                    while parsed_lines < 3 {
                        let Some(rel_nl) =
                            stdout_buf[line_start..].iter().position(|b| *b == b'\n')
                        else {
                            break;
                        };

                        let nl = line_start.saturating_add(rel_nl);
                        let line_bytes = &stdout_buf[line_start..nl];
                        let line = std::str::from_utf8(line_bytes)
                            .map_err(|e| format!("invalid UTF-8 in marker stream: {e}"))?;
                        marker_stdout.push_str(line);
                        marker_stdout.push('\n');

                        parsed_lines = parsed_lines.saturating_add(1);
                        line_start = nl.saturating_add(1);
                    }
                }
                russh::ChannelMsg::ExtendedData { data, .. } => {
                    let snippet = String::from_utf8_lossy(data.as_ref());
                    let snippet: String = snippet.chars().take(256).collect();
                    return Err(format!(
                        "unexpected stderr while reading background markers: {snippet}"
                    ));
                }
                russh::ChannelMsg::ExitStatus { exit_status } => {
                    return Err(format!(
                        "channel exited before background markers (exit_status={exit_status})"
                    ));
                }
                russh::ChannelMsg::Close | russh::ChannelMsg::Eof => {
                    // Keep reading: ExitStatus may still arrive.
                }
                _ => {}
            }
        }

        let markers = parse_background_markers(&marker_stdout, expected_job_id, expected_log_path)
            .map_err(|e| format!("failed to parse background markers: {e}"))?;

        let remaining = if line_start < stdout_buf.len() {
            stdout_buf.split_off(line_start)
        } else {
            Vec::new()
        };
        Ok((markers, remaining))
    };

    match tokio::time::timeout(timeout_duration, fut).await {
        Ok(r) => r,
        Err(_) => Err(format!(
            "timed out waiting for background markers after {}ms",
            timeout_duration.as_millis()
        )),
    }
}

pub(crate) fn parse_background_markers(
    stdout: &str,
    expected_job_id: &str,
    expected_log_path: &str,
) -> std::result::Result<BackgroundMarkers, String> {
    let mut job_id: Option<String> = None;
    let mut pid: Option<u32> = None;
    let mut log_path: Option<String> = None;

    for raw_line in stdout.lines() {
        let line = raw_line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("__SSH_MCP_JOB_ID=") {
            if job_id.is_some() {
                return Err("Duplicate __SSH_MCP_JOB_ID marker".to_string());
            }
            job_id = Some(rest.to_string());
            continue;
        }
        if let Some(rest) = line.strip_prefix("__SSH_MCP_PID=") {
            if pid.is_some() {
                return Err("Duplicate __SSH_MCP_PID marker".to_string());
            }
            let parsed_pid: u32 = rest
                .parse()
                .map_err(|e| format!("Invalid pid marker value '{rest}': {e}"))?;
            pid = Some(parsed_pid);
            continue;
        }
        if let Some(rest) = line.strip_prefix("__SSH_MCP_LOG=") {
            if log_path.is_some() {
                return Err("Duplicate __SSH_MCP_LOG marker".to_string());
            }
            log_path = Some(rest.to_string());
            continue;
        }
    }

    let job_id = job_id.ok_or_else(|| "Missing __SSH_MCP_JOB_ID marker".to_string())?;
    let pid = pid.ok_or_else(|| "Missing __SSH_MCP_PID marker".to_string())?;
    let log_path = log_path.ok_or_else(|| "Missing __SSH_MCP_LOG marker".to_string())?;

    if pid == 0 {
        return Err("Invalid pid marker value '0'".to_string());
    }

    if job_id != expected_job_id {
        return Err(format!(
            "Unexpected job id marker value '{job_id}', expected '{expected_job_id}'"
        ));
    }

    if log_path != expected_log_path {
        return Err(format!(
            "Unexpected log path marker value '{log_path}', expected '{expected_log_path}'"
        ));
    }

    Ok(BackgroundMarkers {
        job_id,
        pid,
        remote_log_path: log_path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::background::wrapper::remote_job_log_path;

    #[test]
    fn test_parse_background_markers() {
        let remote_log = remote_job_log_path("abc-123");
        let stdout =
            format!("__SSH_MCP_JOB_ID=abc-123\n__SSH_MCP_PID=456\n__SSH_MCP_LOG={remote_log}\n");
        let markers = parse_background_markers(&stdout, "abc-123", &remote_log).unwrap();
        assert_eq!(
            markers,
            BackgroundMarkers {
                job_id: "abc-123".to_string(),
                pid: 456,
                remote_log_path: remote_log,
            }
        );
    }

    #[test]
    fn test_parse_background_markers_missing_marker() {
        let remote_log = remote_job_log_path("abc-123");
        let stdout = "__SSH_MCP_JOB_ID=abc-123\n__SSH_MCP_PID=456\n";
        let err = parse_background_markers(stdout, "abc-123", &remote_log).unwrap_err();
        assert_eq!(err, "Missing __SSH_MCP_LOG marker");
    }

    #[test]
    fn test_parse_background_markers_duplicate_marker() {
        let remote_log = remote_job_log_path("abc-123");
        let stdout = format!(
            "__SSH_MCP_JOB_ID=abc-123\n__SSH_MCP_JOB_ID=abc-123\n__SSH_MCP_PID=456\n__SSH_MCP_LOG={remote_log}\n"
        );
        let err = parse_background_markers(&stdout, "abc-123", &remote_log).unwrap_err();
        assert_eq!(err, "Duplicate __SSH_MCP_JOB_ID marker");
    }

    #[test]
    fn test_parse_background_markers_pid_zero() {
        let remote_log = remote_job_log_path("abc-123");
        let stdout =
            format!("__SSH_MCP_JOB_ID=abc-123\n__SSH_MCP_PID=0\n__SSH_MCP_LOG={remote_log}\n");
        let err = parse_background_markers(&stdout, "abc-123", &remote_log).unwrap_err();
        assert_eq!(err, "Invalid pid marker value '0'");
    }

    #[test]
    fn test_parse_background_markers_wrong_job_id() {
        let remote_log = remote_job_log_path("abc-123");
        let stdout =
            format!("__SSH_MCP_JOB_ID=zzz\n__SSH_MCP_PID=456\n__SSH_MCP_LOG={remote_log}\n");
        let err = parse_background_markers(&stdout, "abc-123", &remote_log).unwrap_err();
        assert_eq!(
            err,
            "Unexpected job id marker value 'zzz', expected 'abc-123'"
        );
    }

    #[test]
    fn test_parse_background_markers_wrong_log_path() {
        let remote_log = remote_job_log_path("abc-123");
        let other_log = remote_job_log_path("other");
        let stdout =
            format!("__SSH_MCP_JOB_ID=abc-123\n__SSH_MCP_PID=456\n__SSH_MCP_LOG={other_log}\n");
        let err = parse_background_markers(&stdout, "abc-123", &remote_log).unwrap_err();
        assert_eq!(
            err,
            format!("Unexpected log path marker value '{other_log}', expected '{remote_log}'")
        );
    }

    #[test]
    fn test_parse_background_markers_crlf() {
        let remote_log = remote_job_log_path("abc-123");
        let stdout = format!(
            "__SSH_MCP_JOB_ID=abc-123\r\n__SSH_MCP_PID=456\r\n__SSH_MCP_LOG={remote_log}\r\n"
        );
        let markers = parse_background_markers(&stdout, "abc-123", &remote_log).unwrap();
        assert_eq!(markers.pid, 456);
        assert_eq!(markers.remote_log_path, remote_log);
    }

    #[test]
    fn test_parse_background_markers_extra_unrelated_lines() {
        let remote_log = remote_job_log_path("abc-123");
        let stdout = format!(
            "noise before\n__SSH_MCP_JOB_ID=abc-123\nignored=1\n__SSH_MCP_PID=456\nmore noise\n__SSH_MCP_LOG={remote_log}\nnoise after\n"
        );
        let markers = parse_background_markers(&stdout, "abc-123", &remote_log).unwrap();
        assert_eq!(markers.job_id, "abc-123");
        assert_eq!(markers.pid, 456);
        assert_eq!(markers.remote_log_path, remote_log);
    }
}
