use rmcp::{ErrorData as McpError, model::*};
use serde_json::{Value, json};

use crate::patch::FilePatch;
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
        let patch = match FilePatch::parse(&params.patch) {
            Ok(patch) => patch,
            Err(error) => {
                return Ok(apply_patch_error(error.kind(), error.to_string()));
            }
        };
        let path = patch.path().to_owned();
        let timeout = self.resolve_timeout(None);

        let snapshot = match self
            .load_remote_text_file_state(&path, timeout, privilege)
            .await
        {
            Ok(snapshot) => snapshot,
            Err(error) => return Ok(file_edit_error_result(error)),
        };
        let (original, expected_state) = match &snapshot {
            RemoteTextFileState::Missing => (None, FileExpectedState::Missing),
            RemoteTextFileState::Existing { content, sha256 } => (
                Some(content.as_str()),
                FileExpectedState::Sha256(sha256.clone()),
            ),
        };

        let planned = match patch.plan(original) {
            Ok(planned) => planned,
            Err(error) => return Ok(apply_patch_error(error.kind(), error.to_string())),
        };
        if planned
            .new_content
            .as_ref()
            .is_some_and(|content| content.len() > FILE_EDIT_HARD_MAX_BYTES)
        {
            return Ok(apply_patch_error(
                "limit_exceeded",
                format!("result exceeds apply_patch size limit ({FILE_EDIT_HARD_MAX_BYTES} bytes)"),
            ));
        }

        if !planned.changed {
            return Ok(apply_patch_success(json!({
                "ok": true,
                "path": planned.path,
                "operation": planned.operation.as_str(),
            })));
        }

        if let Err(error) = self
            .apply_file_edit_fault_injection(&path, timeout, fault_injection, privilege)
            .await
        {
            return Ok(file_edit_error_result(error));
        }

        let action = match planned.new_content.as_deref() {
            Some(content) => FileCommitAction::Write(content),
            None => FileCommitAction::Delete,
        };
        if let Err(error) = self
            .commit_remote_text_file(FileCommitRequest {
                remote_path: &path,
                action,
                expected: expected_state,
                timeout,
                privilege,
            })
            .await
        {
            return Ok(file_edit_error_result(error));
        }

        Ok(apply_patch_success(json!({
            "ok": true,
            "path": planned.path,
            "operation": planned.operation.as_str(),
        })))
    }
}

fn file_edit_error_result(error: FileEditError) -> CallToolResult {
    apply_patch_error(error.kind, error.message)
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

fn apply_patch_success(body: Value) -> CallToolResult {
    apply_patch_result(body, false)
}

fn apply_patch_result(body: Value, is_error: bool) -> CallToolResult {
    let text = serde_json::to_string(&body).unwrap_or_else(|_| {
        "{\"ok\":false,\"error\":\"serialization_error\",\"message\":\"failed to serialize apply_patch response\"}".to_owned()
    });
    if is_error {
        CallToolResult::error(vec![Content::text(text)])
    } else {
        CallToolResult::success(vec![Content::text(text)])
    }
}
