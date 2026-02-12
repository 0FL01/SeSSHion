pub(crate) fn validate_basic_path_str(
    path: &str,
    field: &'static str,
) -> std::result::Result<(), String> {
    if path.is_empty() {
        return Err(format!("{field} cannot be empty"));
    }

    if path.trim().is_empty() {
        return Err(format!("{field} must not be whitespace-only"));
    }

    if path != path.trim() {
        return Err(format!(
            "{field} must not have leading or trailing whitespace"
        ));
    }

    if path.contains('\0') {
        return Err(format!("{field} must not contain NUL"));
    }

    if path.chars().any(|c| c.is_control()) {
        return Err(format!("{field} must not contain control characters"));
    }

    // Without consistent `--` support across all utilities, a leading '-' may be interpreted as an
    // option. Reject early for user-provided paths.
    if path.starts_with('-') {
        return Err(format!("{field} must not start with '-'"));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_basic_path_str;

    #[test]
    fn validate_basic_path_str_ok() {
        for s in ["a", "/tmp/x", "relative/path"] {
            assert!(validate_basic_path_str(s, "path").is_ok());
        }
    }

    #[test]
    fn validate_basic_path_str_err_cases() {
        let cases = [
            ("", "cannot be empty"),
            ("   ", "whitespace-only"),
            (" a", "leading or trailing whitespace"),
            ("a ", "leading or trailing whitespace"),
            ("a\0b", "NUL"),
            ("a\nb", "control characters"),
            ("a\u{0007}b", "control characters"),
            ("-dash", "must not start"),
        ];

        for (s, needle) in cases {
            let err = validate_basic_path_str(s, "path").expect_err("expected validation error");
            assert!(err.contains("path"), "unexpected error: {err}");
            assert!(err.contains(needle), "unexpected error: {err}");
        }
    }
}
