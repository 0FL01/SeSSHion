use std::path::PathBuf;
use std::sync::Arc;

use rmcp::ErrorData as McpError;
use rmcp::model::CallToolResult;
use tracing::{debug, error};

use crate::background::OutputStreamer;
use crate::background::detach::DetachMode;
use crate::background::marker::read_background_markers_from_channel;
use crate::background::response::{background_json_err, background_json_ok};
use crate::background::wrapper::remote_job_log_path;
use crate::ssh::sanitize::wrap_in_posix_shell;
use crate::ssh::{sanitize_command, wrap_sudo_command};

use super::SshMcpServer;

pub(super) enum BackgroundPrivilege<'a> {
    Normal,
    Sudo { password: Option<&'a str> },
}

impl SshMcpServer {
    pub(super) async fn execute_background_impl(
        &self,
        command: &str,
        log_path: Option<&str>,
        privilege: BackgroundPrivilege<'_>,
    ) -> std::result::Result<CallToolResult, McpError> {
        let job_id = super::make_job_id();
        let remote_log_path = remote_job_log_path(&job_id);

        let (final_log_path_buf, final_log_path) = match log_path {
            Some(p) => (PathBuf::from(p), p.to_string()),
            None => match self.default_local_log_path(&job_id) {
                Ok(v) => v,
                Err(e) => {
                    return Ok(background_json_err(
                        &job_id,
                        "",
                        Some(&remote_log_path),
                        &e,
                        "",
                    ));
                }
            },
        };

        if let Err(e) = self.ensure_local_log_file(&final_log_path_buf).await {
            return Ok(background_json_err(
                &job_id,
                &final_log_path,
                Some(&remote_log_path),
                &e.to_string(),
                "",
            ));
        }

        let sanitized = match sanitize_command(command, self.max_chars) {
            Ok(cmd) => cmd,
            Err(e) => {
                return Ok(background_json_err(
                    &job_id,
                    &final_log_path,
                    Some(&remote_log_path),
                    &e.to_string(),
                    "",
                ));
            }
        };

        // Ensure connection is established
        if let Err(e) = self.connection.ensure_connected().await {
            return Ok(background_json_err(
                &job_id,
                &final_log_path,
                Some(&remote_log_path),
                &format!("SSH connection error: {}", e),
                "",
            ));
        }

        let (command_for_exec, command_for_registry, attempt_su_elevation, log_msg) =
            match privilege {
                BackgroundPrivilege::Normal => (
                    sanitized.clone(),
                    sanitized.clone(),
                    true,
                    "streaming failed for background job",
                ),
                BackgroundPrivilege::Sudo { password } => {
                    let wrapped_command = wrap_sudo_command(&sanitized, password);
                    debug!(
                        "Wrapped sudo command (password hidden): sudo -n sh -c '...' or printf '...' | sudo ..."
                    );
                    (
                        wrapped_command,
                        format!("sudo {sanitized}"),
                        false,
                        "streaming failed for background sudo job",
                    )
                }
            };

        // If su elevation is configured and available, ensure we're elevated (best-effort)
        if attempt_su_elevation
            && self.connection.get_su_password().is_some()
            && let Err(e) = self.connection.ensure_elevated().await
        {
            debug!(error = ?e, "Elevation failed, will run as normal user");
        }

        let detach_mode = self.determine_detach_mode().await;
        if detach_mode == DetachMode::DirectOnly {
            return Ok(background_json_err(
                &job_id,
                &final_log_path,
                Some(&remote_log_path),
                "Background detach is not supported on this target; run with background=false.",
                "",
            ));
        }

        let wrapper = super::build_background_wrapper_script(
            detach_mode,
            &job_id,
            &command_for_exec,
            &remote_log_path,
        );

        let permit = match self.connection.acquire_command_slot_raw().await {
            Ok(p) => p,
            Err(e) => {
                return Ok(background_json_err(
                    &job_id,
                    &final_log_path,
                    Some(&remote_log_path),
                    &format!("Failed to acquire command slot: {e}"),
                    "",
                ));
            }
        };

        let mut channel = match self.connection.open_channel().await {
            Ok(ch) => ch,
            Err(e) => {
                return Ok(background_json_err(
                    &job_id,
                    &final_log_path,
                    Some(&remote_log_path),
                    &e.to_string(),
                    "",
                ));
            }
        };

        let wrapped_wrapper = wrap_in_posix_shell(&wrapper, false);
        if let Err(e) = channel.exec(true, wrapped_wrapper.as_str()).await {
            return Ok(background_json_err(
                &job_id,
                &final_log_path,
                Some(&remote_log_path),
                &format!("Failed to exec background wrapper: {e}"),
                "",
            ));
        }

        let (markers, initial_stdout) = match read_background_markers_from_channel(
            &mut channel,
            &job_id,
            &remote_log_path,
            super::BACKGROUND_START_TIMEOUT,
        )
        .await
        {
            Ok(v) => v,
            Err(e) => {
                return Ok(background_json_err(
                    &job_id,
                    &final_log_path,
                    Some(&remote_log_path),
                    &e,
                    "",
                ));
            }
        };

        self.register_running_job(
            &job_id,
            markers.pid,
            final_log_path_buf.clone(),
            &command_for_registry,
        )
        .await;

        let streamer = OutputStreamer::new(
            job_id.clone(),
            final_log_path_buf.clone(),
            Arc::clone(&self.job_registry),
        );

        let job_id_for_log = job_id.clone();

        tokio::spawn(async move {
            let _permit = permit;
            if let Err(e) = streamer.stream_channel(channel, initial_stdout).await {
                error!(job_id = ?job_id_for_log, error = ?e, "{log_msg}");
            }
        });

        Ok(background_json_ok(
            &job_id,
            markers.pid,
            &final_log_path,
            &markers.remote_log_path,
        ))
    }
}
