//! File editing validation and processing helpers.

pub(crate) const FILE_EDIT_ERROR_MARKER: &str = "__SSH_MCP_FILE_EDIT_ERR__";
pub(crate) const FILE_EDIT_CONFLICT_MARKER: &str = "__SSH_MCP_FILE_EDIT_CONFLICT__";
pub(crate) const FILE_EDIT_MISSING_SHA256: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";
pub(crate) const FILE_EDIT_HARD_MAX_BYTES: usize = 1024 * 1024;
pub(crate) const FILE_EDIT_LOCK_MAX_SPINS: u32 = 20;
pub(crate) const FILE_EDIT_LOCK_STALE_AFTER_SECS: u64 = 120;
const FILE_EDIT_STDERR_SNIPPET_LIMIT_CHARS: usize = 256;

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

pub(crate) fn sanitize_file_edit_stderr_snippet(stderr: &str) -> Option<String> {
    let mut snippet = String::new();
    let mut truncated = false;
    let mut prev_space = false;
    let mut char_count = 0usize;

    for ch in stderr.trim().chars() {
        if char_count >= FILE_EDIT_STDERR_SNIPPET_LIMIT_CHARS {
            truncated = true;
            break;
        }

        let normalized = if ch.is_control() { ' ' } else { ch };
        if normalized.is_whitespace() {
            if !prev_space {
                snippet.push(' ');
                prev_space = true;
                char_count = char_count.saturating_add(1);
            }
        } else {
            snippet.push(normalized);
            prev_space = false;
            char_count = char_count.saturating_add(1);
        }
    }

    let mut normalized = snippet.trim().to_string();
    if normalized.is_empty() {
        return None;
    }
    if truncated {
        normalized.push_str("...");
    }

    Some(normalized)
}

#[cfg(test)]
mod tests {
    use super::sanitize_file_edit_stderr_snippet;

    #[test]
    fn sanitize_file_edit_stderr_snippet_normalizes_whitespace_and_controls() {
        let stderr = "line1\nline2\t\u{0007}bad\rline3";
        let snippet = sanitize_file_edit_stderr_snippet(stderr)
            .expect("snippet should be present for non-empty stderr");
        assert_eq!(snippet, "line1 line2 bad line3");
    }
}
