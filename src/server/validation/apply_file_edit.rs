//! Apply-file-edit validation and processing helpers.

const APPLY_FILE_EDIT_ERROR_MARKER: &str = "__SSH_MCP_APPLY_FILE_EDIT_ERR__";
const APPLY_FILE_EDIT_CONFLICT_MARKER: &str = "__SSH_MCP_APPLY_FILE_EDIT_CONFLICT__";
pub(crate) const APPLY_FILE_EDIT_HARD_MAX_BYTES: usize = 1024 * 1024;

/// Parses an apply-file-edit error marker from stderr.
pub(crate) fn parse_apply_file_edit_error_marker(stderr: &str) -> Option<&str> {
    parse_apply_file_edit_marker_value(stderr, APPLY_FILE_EDIT_ERROR_MARKER)
}

/// Parses a marker value from stderr.
pub(crate) fn parse_apply_file_edit_marker_value<'a>(
    stderr: &'a str,
    prefix: &str,
) -> Option<&'a str> {
    stderr.lines().find_map(|line| {
        line.trim()
            .strip_prefix(prefix)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    })
}

/// Checks if stderr contains a conflict marker.
pub(crate) fn has_apply_file_edit_conflict_marker(stderr: &str) -> bool {
    stderr
        .lines()
        .any(|line| line.trim() == APPLY_FILE_EDIT_CONFLICT_MARKER)
}

/// Builds an error message for apply-file-edit size exceeded.
pub(crate) fn apply_file_edit_too_large_error(max_bytes: usize) -> String {
    format!(
        "Error: new_content exceeds apply-file-edit size limit ({max_bytes} bytes). Use transfer for large files"
    )
}
