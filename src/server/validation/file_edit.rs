//! File editing validation and processing helpers.

pub(crate) const FILE_EDIT_ERROR_MARKER: &str = "__SSH_MCP_FILE_EDIT_ERR__";
pub(crate) const FILE_EDIT_CONFLICT_MARKER: &str = "__SSH_MCP_FILE_EDIT_CONFLICT__";
pub(crate) const FILE_EDIT_PREVIOUS_SHA_MARKER: &str = "__SSH_MCP_FILE_EDIT_PREVIOUS_SHA__";
pub(crate) const FILE_EDIT_NEW_SHA_MARKER: &str = "__SSH_MCP_FILE_EDIT_NEW_SHA__";
pub(crate) const FILE_EDIT_ACTUAL_SHA_MARKER: &str = "__SSH_MCP_FILE_EDIT_ACTUAL_SHA__";
pub(crate) const FILE_EDIT_BASELINE_SHA_MARKER: &str = "__SSH_MCP_FILE_EDIT_BASELINE_SHA__";
pub(crate) const FILE_EDIT_MISSING_SHA256: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";
pub(crate) const FILE_EDIT_HARD_MAX_BYTES: usize = 1024 * 1024;
pub(crate) const FILE_EDIT_LOCK_MAX_SPINS: u32 = 20;
pub(crate) const FILE_EDIT_LOCK_STALE_AFTER_SECS: u64 = 120;
pub(crate) const FILE_EDIT_LOCK_RETRY_AFTER_MS: u64 = 2000;

/// Parses a file-edit error marker from stderr.
pub(crate) fn parse_file_edit_error_marker(stderr: &str) -> Option<&str> {
    parse_file_edit_marker_value(stderr, FILE_EDIT_ERROR_MARKER)
}

/// Parses a marker value from stderr.
pub(crate) fn parse_file_edit_marker_value<'a>(stderr: &'a str, prefix: &str) -> Option<&'a str> {
    stderr.lines().find_map(|line| {
        line.trim()
            .strip_prefix(prefix)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    })
}

/// Checks if stderr contains a conflict marker.
pub(crate) fn has_file_edit_conflict_marker(stderr: &str) -> bool {
    stderr
        .lines()
        .any(|line| line.trim() == FILE_EDIT_CONFLICT_MARKER)
}

/// Builds an error message for a write-file size limit failure.
pub(crate) fn write_file_too_large_error(max_bytes: usize) -> String {
    format!(
        "Error: new_content exceeds write-file size limit ({max_bytes} bytes). Use transfer for large files"
    )
}

/// Builds an error message for a replace-in-file size limit failure.
pub(crate) fn replace_in_file_too_large_error(max_bytes: usize) -> String {
    format!(
        "Error: replacement result exceeds replace-in-file size limit ({max_bytes} bytes). Use transfer for large files"
    )
}
