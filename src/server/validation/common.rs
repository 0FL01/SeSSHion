//! Common validation helpers used by server tools.

use std::path::{Component, Path};

use crate::validate::validate_basic_path_str;

const READ_FILE_ERROR_MARKER: &str = "__SSH_MCP_READ_FILE_ERR__";

/// Validates a background job log path.
///
/// Current semantics: log_path is a LOCAL path on the MCP server.
/// Keep it in a single, fixed spool directory to avoid arbitrary local writes.
pub(crate) fn validate_background_log_path(
    base_dir: &Path,
    log_path: &str,
) -> std::result::Result<(), String> {
    validate_basic_path_str(log_path, "log_path")?;

    let path = Path::new(log_path);
    if !path.is_absolute() {
        return Err("log_path must be an absolute path".to_string());
    }
    if path
        .components()
        .any(|c| matches!(c, Component::CurDir | Component::ParentDir))
    {
        return Err("log_path must not contain '.' or '..' path components".to_string());
    }

    if path.parent() != Some(base_dir) {
        return Err(format!(
            "log_path must be directly under {}",
            base_dir.display()
        ));
    }
    if path.extension().and_then(|s| s.to_str()) != Some("log") {
        return Err("log_path must have a .log extension".to_string());
    }

    Ok(())
}

/// Validates a remote file path for read operations.
pub(crate) fn validate_read_file_path(remote_path: &str) -> std::result::Result<(), String> {
    validate_basic_path_str(remote_path, "remote_path")?;

    if !remote_path.starts_with('/') {
        return Err("remote_path must be an absolute path".to_string());
    }

    if remote_path.ends_with('/') {
        return Err("remote_path must not end with '/'".to_string());
    }

    Ok(())
}

/// Parses a read-file error marker from stderr.
pub(crate) fn parse_read_file_error_marker(stderr: &str) -> Option<&str> {
    stderr.lines().find_map(|line| {
        line.trim()
            .strip_prefix(READ_FILE_ERROR_MARKER)
            .map(str::trim)
    })
}
