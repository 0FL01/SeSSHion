use crate::validate::validate_basic_path_str;

const BEGIN_PATCH: &str = "*** Begin Patch";
const END_PATCH: &str = "*** End Patch";
const ADD_FILE: &str = "*** Add File: ";
const UPDATE_FILE: &str = "*** Update File: ";
const DELETE_FILE: &str = "*** Delete File: ";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PatchOperationKind {
    Add,
    Update,
    Delete,
}

impl PatchOperationKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Update => "update",
            Self::Delete => "delete",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FilePatch {
    path: String,
    operation: PatchOperation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PatchOperation {
    Add { content: String },
    Update { hunks: Vec<PatchHunk> },
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PatchHunk {
    anchor: Option<String>,
    old_lines: Vec<String>,
    new_lines: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlannedPatch {
    pub(crate) path: String,
    pub(crate) operation: PatchOperationKind,
    pub(crate) new_content: Option<String>,
    pub(crate) changed: bool,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum PatchError {
    #[error("invalid patch envelope: {0}")]
    InvalidEnvelope(String),
    #[error("invalid patch section: {0}")]
    InvalidSection(String),
    #[error("patch must contain exactly one file section")]
    MultipleFiles,
    #[error("unsupported patch operation: {0}")]
    UnsupportedOperation(String),
    #[error("invalid patch path: {0}")]
    InvalidPath(String),
    #[error("invalid update hunk: {0}")]
    InvalidHunk(String),
    #[error("file already exists: {0}")]
    AlreadyExists(String),
    #[error("file does not exist: {0}")]
    NotFound(String),
    #[error("update hunk {hunk} context was not found")]
    ContextNotFound { hunk: usize },
    #[error("update hunk {hunk} context is ambiguous")]
    AmbiguousContext { hunk: usize },
}

impl PatchError {
    pub(crate) const fn kind(&self) -> &'static str {
        match self {
            Self::InvalidEnvelope(_) | Self::InvalidSection(_) | Self::InvalidHunk(_) => {
                "invalid_patch"
            }
            Self::MultipleFiles => "multiple_files",
            Self::UnsupportedOperation(_) => "unsupported_operation",
            Self::InvalidPath(_) => "invalid_path",
            Self::AlreadyExists(_) => "already_exists",
            Self::NotFound(_) => "not_found",
            Self::ContextNotFound { .. } => "context_not_found",
            Self::AmbiguousContext { .. } => "ambiguous_context",
        }
    }
}

impl FilePatch {
    pub(crate) fn parse(input: &str) -> Result<Self, PatchError> {
        let input = input.strip_suffix('\n').unwrap_or(input);
        let lines: Vec<&str> = input.split('\n').collect();

        if lines.first() != Some(&BEGIN_PATCH) {
            return Err(PatchError::InvalidEnvelope(format!(
                "first line must be {BEGIN_PATCH:?}"
            )));
        }
        if lines.last() != Some(&END_PATCH) {
            return Err(PatchError::InvalidEnvelope(format!(
                "last line must be {END_PATCH:?}"
            )));
        }
        if lines.len() < 3 {
            return Err(PatchError::InvalidEnvelope(
                "a file section is required".to_owned(),
            ));
        }

        let section = &lines[1..lines.len() - 1];
        let header = section[0];
        let body = &section[1..];

        let (path, operation) = if let Some(path) = header.strip_prefix(ADD_FILE) {
            let content = parse_add(body)?;
            (path, PatchOperation::Add { content })
        } else if let Some(path) = header.strip_prefix(UPDATE_FILE) {
            let hunks = parse_update(body)?;
            (path, PatchOperation::Update { hunks })
        } else if let Some(path) = header.strip_prefix(DELETE_FILE) {
            if let Some(extra) = body.first() {
                return Err(section_tail_error(extra));
            }
            (path, PatchOperation::Delete)
        } else if header.starts_with("*** Move to:") {
            return Err(PatchError::UnsupportedOperation("Move File".to_owned()));
        } else {
            return Err(PatchError::InvalidSection(header.to_owned()));
        };

        validate_path(path)?;

        Ok(Self {
            path: path.to_owned(),
            operation,
        })
    }

    pub(crate) fn path(&self) -> &str {
        &self.path
    }

    pub(crate) fn operation(&self) -> PatchOperationKind {
        match self.operation {
            PatchOperation::Add { .. } => PatchOperationKind::Add,
            PatchOperation::Update { .. } => PatchOperationKind::Update,
            PatchOperation::Delete => PatchOperationKind::Delete,
        }
    }

    pub(crate) fn plan(&self, current: Option<&str>) -> Result<PlannedPatch, PatchError> {
        let (new_content, changed) = match (&self.operation, current) {
            (PatchOperation::Add { .. }, Some(_)) => {
                return Err(PatchError::AlreadyExists(self.path.clone()));
            }
            (PatchOperation::Add { content }, None) => (Some(content.clone()), true),
            (PatchOperation::Update { .. }, None) | (PatchOperation::Delete, None) => {
                return Err(PatchError::NotFound(self.path.clone()));
            }
            (PatchOperation::Update { hunks }, Some(content)) => {
                let updated = apply_hunks(content, hunks)?;
                let changed = updated != content;
                (Some(updated), changed)
            }
            (PatchOperation::Delete, Some(_)) => (None, true),
        };

        Ok(PlannedPatch {
            path: self.path.clone(),
            operation: self.operation(),
            new_content,
            changed,
        })
    }
}

fn parse_add(lines: &[&str]) -> Result<String, PatchError> {
    let mut content_lines = Vec::with_capacity(lines.len());
    for line in lines {
        let Some(content) = line.strip_prefix('+') else {
            return Err(section_tail_error(line));
        };
        content_lines.push(content);
    }

    if content_lines.is_empty() {
        return Ok(String::new());
    }

    let mut content = content_lines.join("\n");
    content.push('\n');
    Ok(content)
}

fn parse_update(lines: &[&str]) -> Result<Vec<PatchHunk>, PatchError> {
    if lines.is_empty() {
        return Err(PatchError::InvalidHunk(
            "at least one hunk is required".to_owned(),
        ));
    }

    let mut hunks = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let marker = lines[index];
        if marker.starts_with("*** ") {
            return Err(section_tail_error(marker));
        }
        let anchor = if marker == "@@" {
            None
        } else if let Some(anchor) = marker.strip_prefix("@@ ") {
            if anchor.is_empty() {
                return Err(PatchError::InvalidHunk(
                    "hunk anchor cannot be empty".to_owned(),
                ));
            }
            Some(anchor.to_owned())
        } else {
            return Err(PatchError::InvalidHunk(format!(
                "expected @@ marker, got {marker:?}"
            )));
        };
        index += 1;

        let mut old_lines = Vec::new();
        let mut new_lines = Vec::new();
        let mut has_change = false;
        while index < lines.len() && !lines[index].starts_with("@@") {
            let line = lines[index];
            if line.starts_with("*** ") {
                return Err(section_tail_error(line));
            }
            let Some(prefix) = line.chars().next() else {
                return Err(PatchError::InvalidHunk(
                    "every hunk line needs a space, +, or - prefix".to_owned(),
                ));
            };
            let text = &line[prefix.len_utf8()..];
            match prefix {
                ' ' => {
                    old_lines.push(text.to_owned());
                    new_lines.push(text.to_owned());
                }
                '-' => {
                    old_lines.push(text.to_owned());
                    has_change = true;
                }
                '+' => {
                    new_lines.push(text.to_owned());
                    has_change = true;
                }
                _ => {
                    return Err(PatchError::InvalidHunk(format!(
                        "invalid hunk line prefix {prefix:?}"
                    )));
                }
            }
            index += 1;
        }

        if old_lines.is_empty() {
            return Err(PatchError::InvalidHunk(
                "a hunk must include context or a removed line".to_owned(),
            ));
        }
        if !has_change {
            return Err(PatchError::InvalidHunk(
                "a hunk must contain a + or - line".to_owned(),
            ));
        }
        hunks.push(PatchHunk {
            anchor,
            old_lines,
            new_lines,
        });
    }

    Ok(hunks)
}

fn apply_hunks(content: &str, hunks: &[PatchHunk]) -> Result<String, PatchError> {
    let had_final_newline = content.ends_with('\n');
    let body = content.strip_suffix('\n').unwrap_or(content);
    let mut lines: Vec<String> = if content.is_empty() {
        Vec::new()
    } else {
        body.split('\n').map(str::to_owned).collect()
    };

    for (index, hunk) in hunks.iter().enumerate() {
        let old_len = hunk.old_lines.len();
        let mut candidates = Vec::new();
        if old_len <= lines.len() {
            for start in 0..=lines.len() - old_len {
                if lines[start..start + old_len] != hunk.old_lines {
                    continue;
                }
                if let Some(anchor) = &hunk.anchor
                    && !lines[..start + old_len]
                        .iter()
                        .any(|line| line.contains(anchor))
                {
                    continue;
                }
                candidates.push(start);
            }
        }

        let start = match candidates.as_slice() {
            [] => return Err(PatchError::ContextNotFound { hunk: index + 1 }),
            [start] => *start,
            _ => return Err(PatchError::AmbiguousContext { hunk: index + 1 }),
        };
        lines.splice(start..start + old_len, hunk.new_lines.iter().cloned());
    }

    let mut result = lines.join("\n");
    if had_final_newline && !lines.is_empty() {
        result.push('\n');
    }
    Ok(result)
}

fn validate_path(path: &str) -> Result<(), PatchError> {
    validate_basic_path_str(path, "patch path").map_err(PatchError::InvalidPath)?;
    if !path.starts_with('/') {
        return Err(PatchError::InvalidPath(
            "patch path must be absolute".to_owned(),
        ));
    }
    if path.ends_with('/') {
        return Err(PatchError::InvalidPath(
            "patch path must identify a file".to_owned(),
        ));
    }
    Ok(())
}

fn section_tail_error(line: &str) -> PatchError {
    if line.starts_with(ADD_FILE) || line.starts_with(UPDATE_FILE) || line.starts_with(DELETE_FILE)
    {
        PatchError::MultipleFiles
    } else if line.starts_with("*** Move to:") {
        PatchError::UnsupportedOperation("Move File".to_owned())
    } else {
        PatchError::InvalidSection(line.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_plans_add_and_delete() {
        let add = FilePatch::parse(
            "*** Begin Patch\n*** Add File: /tmp/new.txt\n+first\n+second\n*** End Patch",
        )
        .unwrap();
        let add_plan = add.plan(None).unwrap();
        assert_eq!(add_plan.operation, PatchOperationKind::Add);
        assert_eq!(add_plan.new_content.as_deref(), Some("first\nsecond\n"));
        assert_eq!(
            add.plan(Some("occupied")).unwrap_err().kind(),
            "already_exists"
        );

        let delete =
            FilePatch::parse("*** Begin Patch\n*** Delete File: /tmp/old.txt\n*** End Patch")
                .unwrap();
        let delete_plan = delete.plan(Some("old\n")).unwrap();
        assert_eq!(delete_plan.operation, PatchOperationKind::Delete);
        assert_eq!(delete_plan.new_content, None);
    }

    #[test]
    fn applies_multiple_update_hunks_exactly() {
        let patch = FilePatch::parse(
            "*** Begin Patch\n*** Update File: /tmp/app.conf\n@@ server\n server\n-port=80\n+port=8080\n@@\n-enabled=false\n+enabled=true\n*** End Patch",
        )
        .unwrap();
        let plan = patch
            .plan(Some("server\nport=80\nenabled=false\n"))
            .unwrap();
        assert_eq!(plan.operation, PatchOperationKind::Update);
        assert_eq!(
            plan.new_content.as_deref(),
            Some("server\nport=8080\nenabled=true\n")
        );
    }

    #[test]
    fn rejects_invalid_or_multiple_sections() {
        let relative =
            FilePatch::parse("*** Begin Patch\n*** Add File: relative.txt\n+x\n*** End Patch")
                .unwrap_err();
        assert_eq!(relative.kind(), "invalid_path");

        let multiple = FilePatch::parse(
            "*** Begin Patch\n*** Add File: /tmp/a\n+x\n*** Delete File: /tmp/b\n*** End Patch",
        )
        .unwrap_err();
        assert_eq!(multiple, PatchError::MultipleFiles);

        let move_file =
            FilePatch::parse("*** Begin Patch\n*** Move to: /tmp/b\n*** End Patch").unwrap_err();
        assert_eq!(move_file.kind(), "unsupported_operation");
    }

    #[test]
    fn rejects_missing_and_ambiguous_update_context() {
        let patch = FilePatch::parse(
            "*** Begin Patch\n*** Update File: /tmp/a\n@@\n-value=old\n+value=new\n*** End Patch",
        )
        .unwrap();
        assert_eq!(
            patch.plan(Some("other=value\n")).unwrap_err(),
            PatchError::ContextNotFound { hunk: 1 }
        );
        assert_eq!(
            patch.plan(Some("value=old\nvalue=old\n")).unwrap_err(),
            PatchError::AmbiguousContext { hunk: 1 }
        );
    }
}
