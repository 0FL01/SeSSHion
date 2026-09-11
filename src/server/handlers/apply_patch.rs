use rmcp::{ErrorData as McpError, model::*};
use serde_json::{Value, json};
use std::time::Duration;

use crate::patch::{FilePatch, PlannedPatch, parse_patch};
use crate::server::SshMcpServer;
use crate::server::handlers::file_edit_common::{
    FileCommitAction, FileCommitRequest, FileEditError, FileEditFaultInjection, FileEditPrivilege,
    FileExpectedState, RemoteTextFileState,
};
use crate::server::validation::file_edit::FILE_EDIT_HARD_MAX_BYTES;
use crate::tools::ApplyPatchParams;

impl SshMcpServer {
    pub(in crate::server) async fn execute_apply_patch(
        &self,
        params: ApplyPatchParams,
        fault_injection: FileEditFaultInjection,
        privilege: FileEditPrivilege,
    ) -> Result<CallToolResult, McpError> {
        let patches = match parse_patch(&params.patch) {
            Ok(patches) => patches,
            Err(error) => {
                return Ok(apply_patch_error(error.kind(), error.to_string()));
            }
        };
        let timeout = self.resolve_timeout(None);
        let mut files: Vec<Value> = patches
            .iter()
            .map(|patch| {
                json!({
                    "path": patch.path(),
                    "operation": patch.operation().as_str(),
                    "status": "not_attempted",
                })
            })
            .collect();
        let mut prepared = Vec::with_capacity(patches.len());

        // No target writes until every section has a valid snapshot and plan.
        for (index, patch) in patches.iter().enumerate() {
            match self.prepare_file_patch(patch, timeout, privilege).await {
                Ok(plan) => prepared.push(plan),
                Err(error) => return Ok(apply_patch_failure(files, index, "preflight", error)),
            }
        }

        for (index, (planned, expected_state)) in prepared.into_iter().enumerate() {
            if !planned.changed {
                files[index]["status"] = json!("unchanged");
                continue;
            }

            if let Err(error) = self
                .apply_file_edit_fault_injection(&planned.path, timeout, fault_injection, privilege)
                .await
            {
                return Ok(apply_patch_failure(files, index, "commit", error));
            }

            let action = match planned.new_content.as_deref() {
                Some(content) => FileCommitAction::Write(content),
                None => FileCommitAction::Delete,
            };
            if let Err(error) = self
                .commit_remote_text_file(FileCommitRequest {
                    remote_path: &planned.path,
                    action,
                    expected: expected_state,
                    timeout,
                    privilege,
                })
                .await
            {
                return Ok(apply_patch_failure(files, index, "commit", error));
            }
            files[index]["status"] = json!("applied");
        }

        let body = if files.len() == 1 {
            json!({
                "ok": true,
                "path": files[0]["path"],
                "operation": files[0]["operation"],
            })
        } else {
            json!({"ok": true, "files": files})
        };
        Ok(apply_patch_result(body, false))
    }

    async fn prepare_file_patch(
        &self,
        patch: &FilePatch,
        timeout: Duration,
        privilege: FileEditPrivilege,
    ) -> Result<(PlannedPatch, FileExpectedState), FileEditError> {
        let snapshot = self
            .load_remote_text_file_state(patch.path(), timeout, privilege)
            .await?;
        let (original, expected_state) = match &snapshot {
            RemoteTextFileState::Missing => (None, FileExpectedState::Missing),
            RemoteTextFileState::Existing { content, sha256 } => (
                Some(content.as_str()),
                FileExpectedState::Sha256(sha256.clone()),
            ),
        };

        let planned = patch.plan(original).map_err(|error| FileEditError {
            kind: error.kind(),
            message: error.to_string(),
        })?;
        if planned
            .new_content
            .as_ref()
            .is_some_and(|content| content.len() > FILE_EDIT_HARD_MAX_BYTES)
        {
            return Err(FileEditError {
                kind: "limit_exceeded",
                message: format!(
                    "result exceeds apply_patch size limit ({FILE_EDIT_HARD_MAX_BYTES} bytes)"
                ),
            });
        }

        Ok((planned, expected_state))
    }
}

fn apply_patch_failure(
    mut files: Vec<Value>,
    index: usize,
    phase: &str,
    error: FileEditError,
) -> CallToolResult {
    if files.len() == 1 {
        return apply_patch_error(error.kind, error.message);
    }
    // A missing acknowledgement is not evidence that the remote commit did not run.
    files[index]["status"] = json!(
        if phase == "commit" && error.kind == "remote_commit_failed" {
            "unknown"
        } else {
            "failed"
        }
    );
    apply_patch_result(
        json!({
            "ok": false,
            "error": error.kind,
            "message": error.message,
            "path": files[index]["path"],
            "phase": phase,
            "files": files,
        }),
        true,
    )
}

fn apply_patch_error(kind: &str, message: impl Into<String>) -> CallToolResult {
    apply_patch_result(
        json!({
            "ok": false,
            "error": kind,
            "message": message.into(),
        }),
        true,
    )
}

fn apply_patch_result(body: Value, is_error: bool) -> CallToolResult {
    let text = serde_json::to_string(&body).unwrap_or_else(|_| {
        "{\"ok\":false,\"error\":\"serialization_error\",\"message\":\"failed to serialize apply_patch response\"}".to_owned()
    });
    if is_error {
        CallToolResult::error(vec![ContentBlock::text(text)])
    } else {
        CallToolResult::success(vec![ContentBlock::text(text)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_patch_commit_loss_preserves_partial_outcomes_and_single_file_errors() {
        let error = || FileEditError {
            kind: "remote_commit_failed",
            message: "SSH connection lost".to_owned(),
        };
        let result = apply_patch_failure(
            vec![
                json!({"path": "/tmp/a", "operation": "add", "status": "applied"}),
                json!({"path": "/tmp/b", "operation": "update", "status": "not_attempted"}),
                json!({"path": "/tmp/c", "operation": "delete", "status": "not_attempted"}),
            ],
            1,
            "commit",
            error(),
        );
        assert_eq!(result.is_error, Some(true));
        let body: Value = serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap();
        assert_eq!(body["phase"], "commit");
        assert_eq!(body["path"], "/tmp/b");
        assert_eq!(body["files"][0]["status"], "applied");
        assert_eq!(body["files"][1]["status"], "unknown");
        assert_eq!(body["files"][2]["status"], "not_attempted");

        let single = apply_patch_failure(vec![json!({"path": "/tmp/b"})], 0, "commit", error());
        let body: Value = serde_json::from_str(&single.content[0].as_text().unwrap().text).unwrap();
        assert_eq!(
            body,
            json!({"ok": false, "error": "remote_commit_failed", "message": "SSH connection lost"})
        );
    }
}
