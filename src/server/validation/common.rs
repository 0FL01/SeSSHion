//! Common validation helpers used by server tools.

use std::path::{Component, Path};

use crate::validate::validate_basic_path_str;

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
