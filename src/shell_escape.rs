//! Shell escaping utilities shared across subsystems.
//!
//! This module is intentionally dependency-neutral to avoid introducing
//! architectural cycles between `background` and `ssh`.

/// Escapes a string for safe use inside a single-quoted shell string.
///
/// This replaces each `'` with the standard POSIX pattern `'"'"'`.
pub(crate) fn escape_for_shell(s: &str) -> String {
    s.replace('\'', "'\"'\"'")
}
