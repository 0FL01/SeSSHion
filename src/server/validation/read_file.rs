//! Read-file validation and processing helpers.

use crate::tools::ReadFileMode;

pub(crate) const READ_FILE_BYTES_PER_TOKEN: usize = 4;
pub(crate) const READ_FILE_HARD_MAX_BYTES: usize = 1024 * 1024;
pub(crate) const READ_FILE_DEFAULT_PREVIEW_LINES: usize = 800;
pub(crate) const READ_FILE_MAX_LINE_WINDOW: usize = 10_000;
pub(crate) const READ_FILE_STDERR_SNIPPET_LIMIT_CHARS: usize = 256;
pub(crate) const SHA256_HEX_LEN: usize = 64;

/// stderr marker prefix carrying the real remote file size in bytes.
/// Emitted by the server-side read-file command so the local handler can
/// report `approx_tokens_total_estimate` without transferring the whole file.
pub(crate) const READ_FILE_SIZE_MARKER: &str = "__SSH_MCP_READ_FILE_SIZE__";

/// stderr marker indicating the remote file has more lines than the windowed
/// read returned (truncation).  Emitted by head/preview/tail producers.
pub(crate) const READ_FILE_TRUNC_MARKER: &str = "__SSH_MCP_READ_FILE_TRUNC__";

/// Normalizes a SHA-256 hex string to lowercase and validates length.
pub(crate) fn normalize_sha256_hex(
    value: &str,
    field: &str,
) -> std::result::Result<String, String> {
    let normalized = value.trim().to_ascii_lowercase();
    if normalized.len() != SHA256_HEX_LEN {
        return Err(format!(
            "{field} must be a {SHA256_HEX_LEN}-character lowercase hex string"
        ));
    }
    if !normalized.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(format!(
            "{field} must be a {SHA256_HEX_LEN}-character lowercase hex string"
        ));
    }

    Ok(normalized)
}

/// Trims optional SHA-256 input and treats empty strings as absent.
pub(crate) fn normalize_optional_sha256_hex(
    value: Option<&str>,
    field: &str,
) -> std::result::Result<Option<String>, String> {
    match value.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => normalize_sha256_hex(value, field).map(Some),
        None => Ok(None),
    }
}

/// Resolves the max bytes limit from max_output_tokens.
pub(crate) fn resolve_read_file_max_bytes(max_output_tokens: Option<usize>) -> usize {
    max_output_tokens
        .and_then(|tokens| tokens.checked_mul(READ_FILE_BYTES_PER_TOKEN))
        .filter(|bytes| *bytes > 0)
        .unwrap_or(READ_FILE_HARD_MAX_BYTES)
        .min(READ_FILE_HARD_MAX_BYTES)
}

/// Estimates tokens from byte length.
pub(crate) fn estimate_tokens_from_bytes(byte_len: usize) -> usize {
    byte_len.saturating_add(READ_FILE_BYTES_PER_TOKEN.saturating_sub(1)) / READ_FILE_BYTES_PER_TOKEN
}

/// Resolves the line limit based on mode and user input.
pub(crate) fn resolve_read_file_line_limit(
    mode: ReadFileMode,
    lines: Option<usize>,
) -> std::result::Result<Option<usize>, String> {
    match mode {
        ReadFileMode::Full => Ok(None),
        ReadFileMode::Preview | ReadFileMode::Head | ReadFileMode::Tail => {
            let value = lines.unwrap_or(READ_FILE_DEFAULT_PREVIEW_LINES);
            if value == 0 {
                return Err("lines must be a positive integer".to_string());
            }
            if value > READ_FILE_MAX_LINE_WINDOW {
                return Err(format!("lines must be <= {READ_FILE_MAX_LINE_WINDOW}"));
            }
            Ok(Some(value))
        }
    }
}

/// Counts lines in content.
pub(crate) fn read_file_line_count(content: &str) -> usize {
    if content.is_empty() {
        return 0;
    }

    let newline_count = content.bytes().filter(|byte| *byte == b'\n').count();
    if content.ends_with('\n') {
        newline_count
    } else {
        newline_count.saturating_add(1)
    }
}

/// Parses the remote file size (in bytes) from the SIZE marker in stderr.
///
/// Returns `None` when the marker is absent, which should only happen if the
/// remote command failed before emitting it.  Callers fall back to the
/// captured-byte count in that case.
pub(crate) fn parse_read_file_size_marker(stderr: &str) -> Option<usize> {
    stderr.lines().find_map(|line| {
        line.trim()
            .strip_prefix(READ_FILE_SIZE_MARKER)
            .and_then(|rest| rest.trim().parse::<usize>().ok())
    })
}

/// Returns `true` when the TRUNC marker is present in stderr, meaning the
/// remote file has more lines than the windowed read returned.
pub(crate) fn read_file_stderr_indicates_truncated(stderr: &str) -> bool {
    stderr
        .lines()
        .any(|line| line.trim().starts_with(READ_FILE_TRUNC_MARKER))
}

/// Builds a hint message for read-file responses.
pub(crate) fn build_read_file_hint(
    mode: ReadFileMode,
    line_limit: usize,
    truncated: bool,
) -> Option<String> {
    match mode {
        ReadFileMode::Preview => Some(format!(
            "Preview mode returns up to {line_limit} lines. Re-run with mode=\"full\" to read the entire file, or mode=\"tail\" to inspect the file end"
        )),
        ReadFileMode::Head | ReadFileMode::Tail if truncated => Some(format!(
            "Output truncated to {line_limit} lines in {} mode. Re-run with mode=\"full\" to read the entire file",
            mode.as_str()
        )),
        _ => None,
    }
}

/// Builds an error message for file size exceeded.
pub(crate) fn read_file_too_large_error(max_bytes: usize) -> String {
    format!(
        "Error: remote file exceeds read-file size limit ({max_bytes} bytes). Use transfer for large files"
    )
}

/// Sanitizes stderr output for error messages.
pub(crate) fn sanitize_read_file_stderr_snippet(stderr: &str) -> Option<String> {
    let mut snippet = String::new();
    let mut truncated = false;
    let mut prev_space = false;
    let mut char_count = 0usize;

    for ch in stderr.trim().chars() {
        if char_count >= READ_FILE_STDERR_SNIPPET_LIMIT_CHARS {
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

/// Builds a remote failure message for read-file operations.
pub(crate) fn build_read_file_remote_failure(exit_code: Option<u32>, stderr: &str) -> String {
    let mut message = match exit_code {
        Some(code) => format!("Error reading file: remote command failed with exit_code={code}"),
        None => "Error reading file: remote command did not provide an exit status".to_string(),
    };

    if let Some(snippet) = sanitize_read_file_stderr_snippet(stderr) {
        message.push_str(&format!("; stderr={snippet}"));
    }

    message
}
