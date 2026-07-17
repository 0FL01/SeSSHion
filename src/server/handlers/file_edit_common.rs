use sha2::{Digest, Sha256};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tracing::error;

#[cfg(unix)]
use crate::platform::O_NOFOLLOW_FLAG;
use crate::server::SshMcpServer;
use crate::server::make_job_id;
use crate::server::validation::file_edit::*;
use crate::server::validation::read_file::sanitize_read_file_stderr_snippet;
use crate::shell_escape::escape_for_shell;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(in crate::server) enum FileEditFaultInjection {
    None,
    PartialMutateBeforeWrite,
}

#[derive(Debug)]
pub(in crate::server) enum RemoteTextFileState {
    Missing,
    Existing { content: String, sha256: String },
}

#[derive(Debug)]
pub(in crate::server) enum FileExpectedState {
    Missing,
    Sha256(String),
}

pub(in crate::server) enum FileCommitAction<'a> {
    Write(&'a str),
    Delete,
}

pub(in crate::server) struct FileCommitRequest<'a> {
    pub remote_path: &'a str,
    pub action: FileCommitAction<'a>,
    pub expected: FileExpectedState,
    pub timeout: Duration,
}

#[derive(Debug)]
pub(in crate::server) struct FileEditError {
    pub kind: &'static str,
    pub message: String,
}

impl FileEditError {
    fn remote(kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    fn conflict() -> Self {
        Self {
            kind: "conflict",
            message: "file changed while patch was being applied; retry".to_owned(),
        }
    }

    fn lock_busy() -> Self {
        Self {
            kind: "lock_busy",
            message: "path is temporarily locked by another apply_patch call".to_owned(),
        }
    }
}

pub(in crate::server) fn local_text_sha256_hex(content: &str) -> String {
    let hash = Sha256::digest(content.as_bytes());
    hash.iter().fold(String::with_capacity(64), |mut acc, b| {
        use std::fmt::Write as _;
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

impl SshMcpServer {
    pub(in crate::server) async fn load_remote_text_file_state(
        &self,
        remote_path: &str,
        timeout: Duration,
    ) -> Result<RemoteTextFileState, FileEditError> {
        if let Err(e) = self.connection.ensure_connected().await {
            error!(error = ?e, "Failed to ensure SSH connection");
            return Err(FileEditError::remote("connection_failed", e.to_string()));
        }

        let capture_path = self
            .spooler
            .base_dir()
            .join(format!("apply-patch-read-{}.tmp", make_job_id()));
        let mut capture_opts = tokio::fs::OpenOptions::new();
        capture_opts.write(true).create_new(true);
        #[cfg(unix)]
        capture_opts.custom_flags(O_NOFOLLOW_FLAG);

        let mut capture_file = capture_opts.open(&capture_path).await.map_err(|e| {
            FileEditError::remote(
                "local_io",
                format!("failed to create local snapshot file: {e}"),
            )
        })?;

        let escaped_path = escape_for_shell(remote_path);
        let read_cmd = format!(
            r#"sh -c 'set -eu; p=$1; max=$2; if [ ! -e "$p" ]; then printf "%s\n" "{FILE_EDIT_ERROR_MARKER}not_found" >&2; exit 1; fi; if [ ! -f "$p" ]; then printf "%s\n" "{FILE_EDIT_ERROR_MARKER}not_regular_file" >&2; exit 1; fi; size=$(stat -c %s "$p" 2>/dev/null || stat -f %z "$p" 2>/dev/null || printf 0); if [ "$size" -gt "$max" ]; then printf "%s\n" "{FILE_EDIT_ERROR_MARKER}too_large" >&2; exit 1; fi; head -c "$((max + 1))" < "$p"' sh '{escaped_path}' '{FILE_EDIT_HARD_MAX_BYTES}'"#,
        );

        let mut empty_stdin = tokio::io::empty();
        let exec_result = self
            .connection
            .exec_raw_streaming(
                &read_cmd,
                Some(&mut empty_stdin),
                Some(&mut capture_file),
                timeout,
            )
            .await;

        if let Err(e) = capture_file.flush().await {
            let _ = tokio::fs::remove_file(&capture_path).await;
            return Err(FileEditError::remote(
                "local_io",
                format!("failed to flush local snapshot file: {e}"),
            ));
        }
        drop(capture_file);

        let out = match exec_result {
            Ok(out) => out,
            Err(e) => {
                let _ = tokio::fs::remove_file(&capture_path).await;
                return Err(FileEditError::remote(
                    "remote_read_failed",
                    format!("failed to read remote file: {e}"),
                ));
            }
        };

        if let Some(marker) = parse_file_edit_error_marker(&out.stderr) {
            let _ = tokio::fs::remove_file(&capture_path).await;
            return match marker {
                "not_found" => Ok(RemoteTextFileState::Missing),
                "not_regular_file" => Err(FileEditError::remote(
                    "not_regular_file",
                    "remote path is not a regular file",
                )),
                "too_large" => Err(FileEditError::remote(
                    "limit_exceeded",
                    format!(
                        "remote file exceeds apply_patch size limit ({FILE_EDIT_HARD_MAX_BYTES} bytes)"
                    ),
                )),
                _ => Err(FileEditError::remote(
                    "remote_read_failed",
                    "failed to read remote file",
                )),
            };
        }

        if out.exit_code != Some(0) {
            let _ = tokio::fs::remove_file(&capture_path).await;
            return Err(FileEditError::remote(
                "remote_read_failed",
                remote_failure_message("read remote file", out.exit_code, &out.stderr),
            ));
        }

        let bytes = match tokio::fs::read(&capture_path).await {
            Ok(bytes) => bytes,
            Err(e) => {
                let _ = tokio::fs::remove_file(&capture_path).await;
                return Err(FileEditError::remote(
                    "local_io",
                    format!("failed to load local snapshot file: {e}"),
                ));
            }
        };
        let _ = tokio::fs::remove_file(&capture_path).await;

        if bytes.len() > FILE_EDIT_HARD_MAX_BYTES {
            return Err(FileEditError::remote(
                "limit_exceeded",
                format!(
                    "remote file exceeds apply_patch size limit ({FILE_EDIT_HARD_MAX_BYTES} bytes)"
                ),
            ));
        }
        let content = String::from_utf8(bytes).map_err(|e| {
            FileEditError::remote(
                "invalid_utf8",
                format!("remote file is not valid UTF-8 text ({})", e.utf8_error()),
            )
        })?;
        let sha256 = local_text_sha256_hex(&content);
        Ok(RemoteTextFileState::Existing { content, sha256 })
    }

    pub(in crate::server) async fn apply_file_edit_fault_injection(
        &self,
        remote_path: &str,
        timeout: Duration,
        fault_injection: FileEditFaultInjection,
    ) -> Result<(), FileEditError> {
        let injected_cmd = match fault_injection {
            FileEditFaultInjection::PartialMutateBeforeWrite => {
                let escaped = escape_for_shell(remote_path);
                Some(format!(
                    "sh -c 'set -eu; [ -f \"$1\" ]; printf \"__ssh_mcp_race_injected__\\n\" > \"$1\"' sh '{escaped}'"
                ))
            }
            _ => None,
        };

        let Some(injected_cmd) = injected_cmd else {
            return Ok(());
        };
        let out = self
            .connection
            .exec_command(&injected_cmd, timeout)
            .await
            .map_err(|e| {
                FileEditError::remote(
                    "fault_injection_failed",
                    format!("failed to run edit fault injection: {e}"),
                )
            })?;
        if out.exit_code == Some(0) {
            Ok(())
        } else {
            Err(FileEditError::remote(
                "fault_injection_failed",
                remote_failure_message("run edit fault injection", out.exit_code, &out.stderr),
            ))
        }
    }

    pub(in crate::server) async fn commit_remote_text_file(
        &self,
        request: FileCommitRequest<'_>,
    ) -> Result<(), FileEditError> {
        let FileCommitRequest {
            remote_path,
            action,
            expected,
            timeout,
        } = request;

        let new_content = match action {
            FileCommitAction::Write(content) => {
                if content.len() > FILE_EDIT_HARD_MAX_BYTES {
                    return Err(FileEditError::remote(
                        "limit_exceeded",
                        format!(
                            "result exceeds apply_patch size limit ({FILE_EDIT_HARD_MAX_BYTES} bytes)"
                        ),
                    ));
                }
                Some(content)
            }
            FileCommitAction::Delete => None,
        };

        if let Err(e) = self.connection.ensure_connected().await {
            error!(error = ?e, "Failed to ensure SSH connection");
            return Err(FileEditError::remote("connection_failed", e.to_string()));
        }

        let expected_sha256 = match expected {
            FileExpectedState::Missing => FILE_EDIT_MISSING_SHA256.to_owned(),
            FileExpectedState::Sha256(value) => value,
        };
        let operation = if new_content.is_some() {
            "write"
        } else {
            "delete"
        };
        let new_sha256 = new_content.map(local_text_sha256_hex);
        let remote_lock_dir = format!("{remote_path}.ssh-mcp-lock");
        let remote_stage_path = format!("{remote_path}.ssh-mcp-stage-{}", make_job_id());
        let apply_cmd = format!(
            r#"sh -c 'set -eu; dst=$1; expected=$2; operation=$3; expected_new=$4; lock_dir=$5; stage=$6; missing_sha=$7; stale_after_secs=$8; \
              sha256_file() {{ file=$1; if command -v sha256sum >/dev/null 2>&1; then set -- $(sha256sum -- "$file"); printf "%s\n" "$1"; return 0; fi; if command -v shasum >/dev/null 2>&1; then set -- $(shasum -a 256 -- "$file"); printf "%s\n" "$1"; return 0; fi; return 1; }}; \
              reclaim_stale_lock() {{ now_epoch=$1; lock_started_path=$lock_dir/started_at; lock_operation_path=$lock_dir/operation; if [ ! -f "$lock_started_path" ]; then return 1; fi; if ! IFS= read -r lock_started_at < "$lock_started_path"; then return 1; fi; case "$lock_started_at" in ""|*[!0-9]*) return 1 ;; esac; if [ "$lock_started_at" -gt "$now_epoch" ]; then return 1; fi; lock_age=$((now_epoch - lock_started_at)); if [ "$lock_age" -lt "$stale_after_secs" ]; then return 1; fi; rm -f -- "$lock_started_path" "$lock_operation_path" 2>/dev/null || true; rmdir -- "$lock_dir" 2>/dev/null; }}; \
              lock_started_path=$lock_dir/started_at; lock_operation_path=$lock_dir/operation; cleanup() {{ rm -f -- "$stage" "$lock_started_path" "$lock_operation_path" 2>/dev/null || true; rmdir -- "$lock_dir" 2>/dev/null || true; }}; trap cleanup EXIT INT TERM; \
              parent=${{dst%/*}}; if [ -z "$parent" ]; then parent=/; fi; if [ ! -d "$parent" ]; then printf "%s\n" "{FILE_EDIT_ERROR_MARKER}parent_not_found" >&2; exit 1; fi; \
              if ! sha256_file /dev/null >/dev/null 2>&1; then printf "%s\n" "{FILE_EDIT_ERROR_MARKER}sha256_unavailable" >&2; exit 1; fi; \
              if [ "$operation" = "write" ]; then if ! : > "$stage" 2>/dev/null; then printf "%s\n" "{FILE_EDIT_ERROR_MARKER}staging_unwritable" >&2; exit 1; fi; if ! cat > "$stage"; then printf "%s\n" "{FILE_EDIT_ERROR_MARKER}stage_write_failed" >&2; exit 1; fi; if ! stage_hash=$(sha256_file "$stage"); then printf "%s\n" "{FILE_EDIT_ERROR_MARKER}sha256_unavailable" >&2; exit 1; fi; if [ "$stage_hash" != "$expected_new" ]; then printf "%s\n" "{FILE_EDIT_ERROR_MARKER}stage_hash_mismatch" >&2; exit 1; fi; fi; \
              lock_spins=0; while ! mkdir -- "$lock_dir" 2>/dev/null; do if [ -d "$lock_dir" ]; then if now_epoch=$(date +%s 2>/dev/null); then if reclaim_stale_lock "$now_epoch"; then continue; fi; fi; lock_spins=$((lock_spins + 1)); if [ "$lock_spins" -ge {FILE_EDIT_LOCK_MAX_SPINS} ]; then printf "%s\n" "{FILE_EDIT_ERROR_MARKER}lock_busy" >&2; exit 1; fi; sleep 1; continue; fi; printf "%s\n" "{FILE_EDIT_ERROR_MARKER}lock_acquire_failed" >&2; exit 1; done; \
              if now_epoch=$(date +%s 2>/dev/null); then printf "%s\n" "$now_epoch" > "$lock_started_path" 2>/dev/null || true; fi; printf "%s\n" "apply_patch" > "$lock_operation_path" 2>/dev/null || true; \
              if [ -e "$dst" ]; then if [ ! -f "$dst" ]; then printf "%s\n" "{FILE_EDIT_ERROR_MARKER}not_regular_file" >&2; exit 1; fi; if ! current_hash=$(sha256_file "$dst"); then printf "%s\n" "{FILE_EDIT_ERROR_MARKER}sha256_unavailable" >&2; exit 1; fi; else current_hash=$missing_sha; fi; \
              if [ "$current_hash" != "$expected" ]; then printf "%s\n" "{FILE_EDIT_CONFLICT_MARKER}" >&2; exit 3; fi; \
              if [ "$operation" = "delete" ]; then if ! rm -- "$dst"; then printf "%s\n" "{FILE_EDIT_ERROR_MARKER}finalize_failed" >&2; exit 1; fi; \
              elif [ "$expected" = "$missing_sha" ]; then if ! ln -- "$stage" "$dst" 2>/dev/null; then if [ -e "$dst" ]; then printf "%s\n" "{FILE_EDIT_CONFLICT_MARKER}" >&2; exit 3; fi; printf "%s\n" "{FILE_EDIT_ERROR_MARKER}finalize_failed" >&2; exit 1; fi; rm -f -- "$stage"; \
              else if ! mv -- "$stage" "$dst"; then printf "%s\n" "{FILE_EDIT_ERROR_MARKER}finalize_failed" >&2; exit 1; fi; fi; \
              trap - EXIT INT TERM; cleanup' sh '{}' '{}' '{}' '{}' '{}' '{}' '{}' '{}'"#,
            escape_for_shell(remote_path),
            escape_for_shell(&expected_sha256),
            escape_for_shell(operation),
            escape_for_shell(new_sha256.as_deref().unwrap_or("-")),
            escape_for_shell(&remote_lock_dir),
            escape_for_shell(&remote_stage_path),
            escape_for_shell(FILE_EDIT_MISSING_SHA256),
            escape_for_shell(&FILE_EDIT_LOCK_STALE_AFTER_SECS.to_string()),
        );

        let local_tmp_path = if let Some(content) = new_content {
            Some(self.write_local_stage(content).await?)
        } else {
            None
        };
        let mut sink = tokio::io::sink();
        let out = if let Some(path) = local_tmp_path.as_ref() {
            let mut input = tokio::fs::File::open(path).await.map_err(|e| {
                FileEditError::remote(
                    "local_io",
                    format!("failed to open local staging file: {e}"),
                )
            })?;
            self.connection
                .exec_raw_streaming(&apply_cmd, Some(&mut input), Some(&mut sink), timeout)
                .await
        } else {
            let mut empty = tokio::io::empty();
            self.connection
                .exec_raw_streaming(&apply_cmd, Some(&mut empty), Some(&mut sink), timeout)
                .await
        };
        if let Some(path) = local_tmp_path {
            let _ = tokio::fs::remove_file(path).await;
        }
        let out = out.map_err(|e| {
            FileEditError::remote("remote_commit_failed", format!("apply_patch failed: {e}"))
        })?;

        if has_file_edit_conflict_marker(&out.stderr) {
            return Err(FileEditError::conflict());
        }

        if let Some(marker) = parse_file_edit_error_marker(&out.stderr) {
            return Err(match marker {
                "lock_busy" => FileEditError::lock_busy(),
                "parent_not_found" => FileEditError::remote(
                    "parent_not_found",
                    "remote parent directory does not exist",
                ),
                "not_regular_file" => {
                    FileEditError::remote("not_regular_file", "remote path is not a regular file")
                }
                "sha256_unavailable" => FileEditError::remote(
                    "sha256_unavailable",
                    "remote host does not provide SHA-256 utilities",
                ),
                "stage_hash_mismatch" => FileEditError::remote(
                    "stage_hash_mismatch",
                    "uploaded staging file SHA-256 did not match planned content",
                ),
                "lock_acquire_failed" => {
                    FileEditError::remote("lock_failed", "failed to acquire remote edit lock")
                }
                "staging_unwritable" | "stage_write_failed" => {
                    FileEditError::remote("stage_failed", "failed to write remote staging file")
                }
                "finalize_failed" => {
                    FileEditError::remote("finalize_failed", "failed to finalize remote edit")
                }
                _ => FileEditError::remote("remote_commit_failed", "apply_patch failed remotely"),
            });
        }
        if out.exit_code != Some(0) {
            return Err(FileEditError::remote(
                "remote_commit_failed",
                remote_failure_message("apply patch", out.exit_code, &out.stderr),
            ));
        }

        Ok(())
    }

    async fn write_local_stage(&self, content: &str) -> Result<std::path::PathBuf, FileEditError> {
        let local_tmp_path = self
            .spooler
            .base_dir()
            .join(format!("apply-patch-write-{}.tmp", make_job_id()));

        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.custom_flags(O_NOFOLLOW_FLAG);
        let mut file = options.open(&local_tmp_path).await.map_err(|e| {
            FileEditError::remote(
                "local_io",
                format!("failed to create local staging file: {e}"),
            )
        })?;
        if let Err(e) = file.write_all(content.as_bytes()).await {
            let _ = tokio::fs::remove_file(&local_tmp_path).await;
            return Err(FileEditError::remote(
                "local_io",
                format!("failed to write local staging file: {e}"),
            ));
        }
        if let Err(e) = file.flush().await {
            let _ = tokio::fs::remove_file(&local_tmp_path).await;
            return Err(FileEditError::remote(
                "local_io",
                format!("failed to flush local staging file: {e}"),
            ));
        }
        Ok(local_tmp_path)
    }
}

fn remote_failure_message(operation: &str, exit_code: Option<u32>, stderr: &str) -> String {
    let mut message = match exit_code {
        Some(code) => format!("failed to {operation}: remote command exited with code {code}"),
        None => format!("failed to {operation}: remote command did not report an exit status"),
    };
    if let Some(snippet) = sanitize_read_file_stderr_snippet(stderr) {
        message.push_str(&format!("; stderr={snippet}"));
    }
    message
}
