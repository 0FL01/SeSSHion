use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, Content};
use std::time::Duration;
use tracing::debug;

use crate::server::SshMcpServer;
use crate::server::handlers::file_edit_common::{
    FileEditFaultInjection, FileWriteTransactionRequest, build_file_edit_conflict_result,
    build_unified_diff, local_text_sha256_hex,
};
use crate::server::validation::common::extract_text_from_call_tool_result;
use crate::server::validation::common::validate_read_file_path;
use crate::server::validation::file_edit::replace_in_file_too_large_error;
use crate::server::validation::read_file::normalize_optional_sha256_hex;
use crate::tools::{ReadFileMode, ReadFileParams, ReplaceInFileParams};

struct PlannedReplacement {
    updated_content: String,
    match_count: usize,
    selected_match_indices: Vec<usize>,
}

struct ResolvedScope {
    start: usize,
    end: usize,
}

fn resolve_scope(
    current_content: &str,
    scope_text: Option<&str>,
) -> std::result::Result<ResolvedScope, String> {
    match scope_text {
        None => Ok(ResolvedScope {
            start: 0,
            end: current_content.len(),
        }),
        Some(scope_text) => {
            let scope_positions: Vec<usize> = current_content
                .match_indices(scope_text)
                .map(|(offset, _)| offset)
                .collect();

            match scope_positions.len() {
                0 => Err("Error: scope_text was not found in remote file".to_string()),
                1 => {
                    let start = scope_positions[0];
                    Ok(ResolvedScope {
                        start,
                        end: start + scope_text.len(),
                    })
                }
                count => Err(format!(
                    "Error: scope_text matched {count} times; provide a more specific scope_text"
                )),
            }
        }
    }
}

fn plan_exact_text_replacement(
    current_content: &str,
    old_text: &str,
    new_text: &str,
    replace_all: bool,
    match_index: Option<usize>,
) -> std::result::Result<PlannedReplacement, String> {
    let match_positions: Vec<usize> = current_content
        .match_indices(old_text)
        .map(|(offset, _)| offset)
        .collect();
    let match_count = match_positions.len();

    if match_count == 0 {
        return Err("Error: old_text was not found in remote file".to_string());
    }

    if replace_all {
        return Ok(PlannedReplacement {
            updated_content: current_content.replace(old_text, new_text),
            match_count,
            selected_match_indices: (1..=match_count).collect(),
        });
    }

    if let Some(index) = match_index {
        if index > match_count {
            return Err(format!(
                "Error: match_index {index} is out of range; old_text matched {match_count} times"
            ));
        }

        let selected_offset = match_positions[index - 1];
        let selected_end = selected_offset + old_text.len();
        let mut updated_content = String::with_capacity(
            current_content.len().saturating_sub(old_text.len()) + new_text.len(),
        );
        updated_content.push_str(&current_content[..selected_offset]);
        updated_content.push_str(new_text);
        updated_content.push_str(&current_content[selected_end..]);

        return Ok(PlannedReplacement {
            updated_content,
            match_count,
            selected_match_indices: vec![index],
        });
    }

    if match_count != 1 {
        return Err(format!(
            "Error: old_text matched {match_count} times; pass match_index to select one match or set replace_all=true to replace all matches"
        ));
    }

    Ok(PlannedReplacement {
        updated_content: current_content.replacen(old_text, new_text, 1),
        match_count,
        selected_match_indices: vec![1],
    })
}

impl SshMcpServer {
    pub(in crate::server) async fn execute_replace_in_file(
        &self,
        params: ReplaceInFileParams,
        fault_injection: FileEditFaultInjection,
    ) -> std::result::Result<CallToolResult, McpError> {
        debug!(remote_path = ?params.remote_path, "replace-in-file tool called");

        let ReplaceInFileParams {
            remote_path,
            old_text,
            new_text,
            scope_text,
            replace_all,
            match_index,
            dry_run,
            expected_sha256,
            timeout_ms,
        } = params;

        validate_read_file_path(&remote_path).map_err(|msg| McpError::invalid_params(msg, None))?;

        let user_expected_sha256 =
            normalize_optional_sha256_hex(expected_sha256.as_deref(), "expected_sha256")
                .map_err(|msg| McpError::invalid_params(msg, None))?;

        let timeout = match timeout_ms {
            Some(0) => {
                return Err(McpError::invalid_params(
                    "timeout_ms must be a positive integer",
                    None,
                ));
            }
            Some(ms) => Duration::from_millis(ms),
            None => self.timeout,
        };

        let replace_all = replace_all.unwrap_or(false);
        let dry_run = dry_run.unwrap_or(false);
        if old_text.is_empty() {
            return Err(McpError::invalid_params(
                "old_text must not be empty in replace-in-file",
                None,
            ));
        }
        if scope_text.as_deref() == Some("") {
            return Err(McpError::invalid_params(
                "scope_text must not be empty in replace-in-file",
                None,
            ));
        }
        if replace_all && match_index.is_some() {
            return Err(McpError::invalid_params(
                "match_index cannot be combined with replace_all=true in replace-in-file",
                None,
            ));
        }
        if match_index == Some(0) {
            return Err(McpError::invalid_params(
                "match_index must be a positive 1-based integer in replace-in-file",
                None,
            ));
        }

        let read_result = self
            .execute_read_file(ReadFileParams {
                remote_path: remote_path.clone(),
                mode: ReadFileMode::Full,
                lines: None,
                timeout_ms,
            })
            .await?;

        if read_result.is_error.unwrap_or(false) {
            return Ok(read_result);
        }

        let read_text = extract_text_from_call_tool_result(&read_result);
        let read_value: serde_json::Value =
            serde_json::from_str(read_text.trim()).map_err(|e| {
                McpError::internal_error(
                    format!(
                        "failed to parse read-file response while preparing replace-in-file: {e}"
                    ),
                    None,
                )
            })?;

        let current_content = read_value
            .get("content")
            .and_then(|value| value.as_str())
            .ok_or_else(|| {
                McpError::internal_error(
                    "read-file response missing content while preparing replace-in-file"
                        .to_string(),
                    None,
                )
            })?;

        let current_sha256 = read_value
            .get("sha256")
            .and_then(|value| value.as_str())
            .ok_or_else(|| {
                McpError::internal_error(
                    "read-file response missing sha256 while preparing replace-in-file".to_string(),
                    None,
                )
            })?;

        let resolved_scope = match resolve_scope(current_content, scope_text.as_deref()) {
            Ok(scope) => scope,
            Err(message) => return Ok(CallToolResult::error(vec![Content::text(message)])),
        };
        let scoped_content = &current_content[resolved_scope.start..resolved_scope.end];

        let planned = match plan_exact_text_replacement(
            scoped_content,
            old_text.as_str(),
            new_text.as_str(),
            replace_all,
            match_index,
        ) {
            Ok(plan) => plan,
            Err(message) => {
                let message = if scope_text.is_some()
                    && message == "Error: old_text was not found in remote file"
                {
                    "Error: old_text was not found within scope_text".to_string()
                } else {
                    message
                };
                return Ok(CallToolResult::error(vec![Content::text(message)]));
            }
        };
        let PlannedReplacement {
            updated_content: scoped_updated_content,
            match_count,
            selected_match_indices,
        } = planned;

        let updated_content =
            if resolved_scope.start == 0 && resolved_scope.end == current_content.len() {
                scoped_updated_content
            } else {
                let mut combined = String::with_capacity(
                    current_content.len() - (resolved_scope.end - resolved_scope.start)
                        + scoped_updated_content.len(),
                );
                combined.push_str(&current_content[..resolved_scope.start]);
                combined.push_str(&scoped_updated_content);
                combined.push_str(&current_content[resolved_scope.end..]);
                combined
            };

        if let Some(expected) = user_expected_sha256.as_deref()
            && expected != current_sha256
        {
            return Ok(build_file_edit_conflict_result(
                &remote_path,
                expected,
                current_sha256,
            ));
        }

        if dry_run {
            let preview = serde_json::json!({
                "path": remote_path,
                "dry_run": true,
                "changed": current_content != updated_content,
                "previous_sha256": current_sha256,
                "predicted_new_sha256": local_text_sha256_hex(&updated_content),
                "bytes_written": updated_content.len(),
                "match_count": match_count,
                "selected_match_indices": selected_match_indices,
                "diff": build_unified_diff(&remote_path, current_content, &updated_content),
            });
            return Ok(CallToolResult::success(vec![Content::text(
                preview.to_string(),
            )]));
        }

        let partial_baseline_sha256 = if user_expected_sha256.is_none() {
            let baseline = match self
                .compute_partial_baseline_sha256(current_content, timeout)
                .await
            {
                Ok(value) => value,
                Err(result) => return Ok(result),
            };
            Some(baseline)
        } else {
            None
        };

        if let Err(result) = self
            .apply_partial_fault_injection(remote_path.as_str(), timeout, fault_injection)
            .await
        {
            return Ok(result);
        }

        self.execute_file_write_transaction(FileWriteTransactionRequest {
            remote_path: remote_path.as_str(),
            new_content: updated_content.as_str(),
            expected_sha256: user_expected_sha256.or(partial_baseline_sha256),
            timeout,
            fault_injection,
            too_large_error: replace_in_file_too_large_error(
                crate::server::validation::file_edit::FILE_EDIT_HARD_MAX_BYTES,
            ),
            operation_name: "replace-in-file",
        })
        .await
    }
}
