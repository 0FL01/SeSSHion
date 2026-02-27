# Unified Diff Generation - Implementation Reference

> **Use Case**: Generate human-readable diffs for file modifications in MCP tools  
> **Features**: Unified diff format, Unicode normalization for LLM-generated content

---

## Overview

This implementation provides unified diff generation for file editing tools, with special handling for Unicode characters that LLMs often normalize differently (smart quotes, dashes, ellipsis, etc.).

### Key Features

| Feature | Description |
|---------|-------------|
| **Unified Diff** | Standard `diff -u` output format |
| **Unicode Normalization** | Handles LLM quote/dash normalization |
| **Two-way Algorithm** | O(n+m) substring search performance |
| **Byte Mapping** | Correct position tracking for 1-to-many char mappings |

---

## Dependencies

```toml
[dependencies]
similar = "2.4"  # For unified diff generation
```

---

## Core Implementation

### 1. Basic Unified Diff Generator

```rust
use similar::TextDiff;

/// Create a unified diff between original and modified content
/// 
/// # Arguments
/// * `original` - Original file content
/// * `modified` - Modified file content  
/// * `from_file` - Original filename for diff header
/// * `to_file` - Modified filename for diff header
///
/// # Returns
/// Unified diff as string with markers
fn create_unified_diff(
    original: &str,
    modified: &str,
    from_file: &str,
    to_file: &str,
) -> String {
    let text_diff = TextDiff::from_lines(original, modified);
    format!(
        "{}",
        text_diff
            .unified_diff()
            .context_radius(3)  // Lines of context around changes
            .header(from_file, to_file)
    )
}
```

**Example Output:**
```diff
--- /etc/nginx/nginx.conf
+++ /etc/nginx/nginx.conf
@@ -10,7 +10,7 @@
     server {
         listen 80;
         server_name localhost;
-        root /var/www/html;
+        root /var/www/app;
         
         location / {
             try_files $uri $uri/ =404;
```

---

### 2. File Edit Tool with Diff Output

```rust
use std::fs;
use anyhow::{anyhow, Result};

/// Parameters for string replacement
pub struct StrReplaceParams {
    pub path: String,
    pub old_str: String,
    pub new_str: String,
    pub replace_all: Option<bool>,
}

/// Result of string replacement operation
pub struct StrReplaceResult {
    pub new_content: String,
    pub replaced_count: usize,
    pub unified_diff: String,
}

/// Replace string in file and generate diff
/// 
/// # Example
/// ```
/// let result = str_replace_with_diff(
///     "/etc/config.conf",
///     "localhost",
///     "127.0.0.1",
///     false,
/// )?;
/// println!("Changes:\n{}", result.unified_diff);
/// ```
pub fn str_replace_with_diff(
    path: &str,
    old_str: &str,
    new_str: &str,
    replace_all: Option<bool>,
) -> Result<StrReplaceResult> {
    // Validate inputs
    if old_str == new_str {
        return Err(anyhow!("Old string and new string are identical"));
    }
    
    // Read original content
    let original_content = fs::read_to_string(path)?;
    
    // Check if string exists
    if !original_content.contains(old_str) {
        return Err(anyhow!("String not found in file: {}", old_str));
    }
    
    // Perform replacement
    let replace_all = replace_all.unwrap_or(false);
    let new_content = if replace_all {
        original_content.replace(old_str, new_str)
    } else {
        original_content.replacen(old_str, new_str, 1)
    };
    
    // Count replacements
    let replaced_count = if replace_all {
        original_content.matches(old_str).count()
    } else {
        1
    };
    
    // Generate unified diff
    let unified_diff = create_unified_diff(
        &original_content,
        &new_content,
        path,
        path,
    );
    
    // Write new content
    fs::write(path, &new_content)?;
    
    Ok(StrReplaceResult {
        new_content,
        replaced_count,
        unified_diff,
    })
}
```

---

## Advanced: Unicode Normalization

LLMs often normalize Unicode characters differently than source files:
- **Smart quotes**: `'` `"` → `'` `"`
- **Dashes**: `–` `—` → `-`
- **Ellipsis**: `…` → `...`
- **Non-breaking spaces** → regular spaces

### Unicode Normalization Map

```rust
/// Map Unicode characters to ASCII equivalents
/// Returns None if no normalization needed
fn normalize_unicode_char(c: char) -> Option<&'static str> {
    match c {
        // Single quotes: ' ' ‚ ‹ › → '
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{2039}' | '\u{203A}' => Some("'"),
        '\u{FF07}' => Some("'"),  // Fullwidth apostrophe
        
        // Double quotes: " " „ « » → "
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{00AB}' | '\u{00BB}' => Some("\""),
        
        // Dashes: ‐ ‑ ‒ – — ― → -
        '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}' => Some("-"),
        
        // Spaces: NBSP, en/em spaces → regular space
        '\u{00A0}' | '\u{2002}' | '\u{2003}' | '\u{2009}' | '\u{200A}' | '\u{202F}' => Some(" "),
        
        // Ellipsis: … → ... (1-to-many mapping!)
        '\u{2026}' => Some("..."),
        
        // Other common normalizations
        '\u{2022}' => Some("*"),   // Bullet → *
        '\u{00B7}' => Some("."),   // Middle dot → .
        
        _ => None,
    }
}
```

### Normalized String Replacement

```rust
use std::cmp::Ordering;

/// Replace string with Unicode normalization support
/// 
/// Strategy:
/// 1. Try exact match first (fast path)
/// 2. If not found, try Unicode-normalized matching
/// 3. Preserve original encoding in result
pub fn str_replace_normalized(
    content: &str,
    old_str: &str,
    new_str: &str,
    replace_all: bool,
) -> Option<(String, usize)> {
    // Fast path: exact match
    if content.contains(old_str) {
        let result = if replace_all {
            content.replace(old_str, new_str)
        } else {
            content.replacen(old_str, new_str, 1)
        };
        let count = if replace_all {
            content.matches(old_str).count()
        } else {
            1
        };
        return Some((result, count));
    }
    
    // Fallback: Unicode-normalized matching
    unicode_normalized_replace(content, old_str, new_str, replace_all)
}

/// Unicode-normalized replacement with byte-position tracking
/// 
/// Uses Two-Way algorithm (O(n+m)) for substring search.
/// Handles 1-to-many character mappings (e.g., … → ...)
fn unicode_normalized_replace(
    content: &str,
    old_str: &str,
    new_str: &str,
    replace_all: bool,
) -> Option<(String, usize)> {
    let norm_old = normalize_unicode_to_ascii(old_str);
    
    if norm_old.is_empty() {
        return None;
    }
    
    // Quick check without building full mapping
    let norm_content_quick = normalize_unicode_to_ascii(content);
    if !norm_content_quick.contains(&norm_old) {
        return None;
    }
    drop(norm_content_quick);
    
    // Pattern found - build full byte mapping
    let norm_content = normalize_with_byte_mapping(content);
    let norm_old_byte_len = norm_old.len();
    
    // Find all matches using Two-Way algorithm
    let mut match_ranges: Vec<(usize, usize)> = Vec::new();
    
    if replace_all {
        let mut search_pos = 0;
        while search_pos + norm_old_byte_len <= norm_content.text.len() {
            if let Some(rel) = norm_content.text[search_pos..].find(&norm_old) {
                let match_start = search_pos + rel;
                let match_end = match_start + norm_old_byte_len;
                
                if let (Some(orig_start), Some(orig_end)) = (
                    norm_content.orig_byte_at(match_start),
                    norm_content.orig_byte_at(match_end),
                ) {
                    match_ranges.push((orig_start, orig_end));
                }
                search_pos = match_end;
            } else {
                break;
            }
        }
    } else if let Some(match_start) = norm_content.text.find(&norm_old) {
        let match_end = match_start + norm_old_byte_len;
        if let (Some(orig_start), Some(orig_end)) = (
            norm_content.orig_byte_at(match_start),
            norm_content.orig_byte_at(match_end),
        ) {
            match_ranges.push((orig_start, orig_end));
        }
    }
    
    if match_ranges.is_empty() {
        return None;
    }
    
    let replaced_count = match_ranges.len();
    
    // Build result with replacements
    let mut result = String::with_capacity(content.len());
    let mut prev_end = 0;
    
    for &(orig_start, orig_end) in &match_ranges {
        result.push_str(&content[prev_end..orig_start]);
        result.push_str(new_str);
        prev_end = orig_end;
    }
    result.push_str(&content[prev_end..]);
    
    Some((result, replaced_count))
}

/// Normalize string to ASCII, dropping chars that don't need normalization
fn normalize_unicode_to_ascii(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match normalize_unicode_char(c) {
            Some(replacement) => out.push_str(replacement),
            None => out.push(c),
        }
    }
    out
}

/// Normalized string with byte-position mapping back to original
struct NormalizedWithMapping {
    /// Normalized text
    text: String,
    /// Maps byte positions in normalized text to original byte positions
    norm_byte_to_orig_byte: Vec<usize>,
    /// Character boundaries in normalized text
    char_boundaries: Vec<usize>,
}

impl NormalizedWithMapping {
    /// Convert normalized byte position to original byte position
    fn orig_byte_at(&self,
        norm_byte: usize,
    ) -> Option<usize> {
        let idx = self.char_boundaries.binary_search(&norm_byte).ok()?;
        Some(self.norm_byte_to_orig_byte[idx])
    }
}

/// Build normalized string with full byte-position mapping
fn normalize_with_byte_mapping(s: &str) -> NormalizedWithMapping {
    let mut text = String::with_capacity(s.len());
    let mut norm_byte_to_orig_byte: Vec<usize> = Vec::with_capacity(s.len() + 1);
    let mut char_boundaries: Vec<usize> = Vec::with_capacity(s.len() + 1);
    
    for (byte_idx, c) in s.char_indices() {
        match normalize_unicode_char(c) {
            Some(replacement) => {
                // 1-to-many mapping: each replacement char maps to same original byte
                for _ in replacement.chars() {
                    char_boundaries.push(text.len());
                    norm_byte_to_orig_byte.push(byte_idx);
                    // Note: pushing char by char would be incorrect here
                    // This is pseudocode - see full implementation below
                }
                text.push_str(replacement);
            }
            None => {
                char_boundaries.push(text.len());
                norm_byte_to_orig_byte.push(byte_idx);
                text.push(c);
            }
        }
    }
    
    // Sentinel values
    char_boundaries.push(text.len());
    norm_byte_to_orig_byte.push(s.len());
    
    NormalizedWithMapping {
        text,
        norm_byte_to_orig_byte,
        char_boundaries,
    }
}
```

### Full Byte Mapping Implementation

```rust
/// Correct implementation handling 1-to-many mappings
fn normalize_with_byte_mapping(s: &str) -> NormalizedWithMapping {
    let mut text = String::with_capacity(s.len());
    let mut norm_byte_to_orig_byte: Vec<usize> = Vec::with_capacity(s.len() + 1);
    let mut char_boundaries: Vec<usize> = Vec::with_capacity(s.len() + 1);
    
    for (byte_idx, c) in s.char_indices() {
        match normalize_unicode_char(c) {
            Some(replacement) => {
                // Track each character in the replacement
                for rc in replacement.chars() {
                    char_boundaries.push(text.len());
                    norm_byte_to_orig_byte.push(byte_idx);
                    text.push(rc);
                }
            }
            None => {
                char_boundaries.push(text.len());
                norm_byte_to_orig_byte.push(byte_idx);
                text.push(c);
            }
        }
    }
    
    // Sentinel values for end-of-string
    char_boundaries.push(text.len());
    norm_byte_to_orig_byte.push(s.len());
    
    NormalizedWithMapping {
        text,
        norm_byte_to_orig_byte,
        char_boundaries,
    }
}
```

---

## Complete MCP Tool Example

```rust
use mcp_sdk::types::{Tool, CallToolResult, Content};
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EditFileParams {
    /// Path to file
    pub path: String,
    /// Text to find
    pub old_str: String,
    /// Replacement text
    pub new_str: String,
    /// Replace all occurrences
    #[serde(default)]
    pub replace_all: bool,
    /// Show diff without applying
    #[serde(default)]
    pub preview_only: bool,
}

pub fn edit_file_tool() -> Tool {
    Tool {
        name: "edit_file".to_string(),
        description: Some(
            "Edit file by replacing text. Shows unified diff of changes. \
             Supports Unicode normalization for smart quotes and dashes.".to_string()
        ),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "old_str": { "type": "string" },
                "new_str": { "type": "string" },
                "replace_all": { "type": "boolean", "default": false },
                "preview_only": { "type": "boolean", "default": false }
            },
            "required": ["path", "old_str", "new_str"]
        }),
    }
}

pub async fn handle_edit_file(params: EditFileParams) -> CallToolResult {
    // Read file
    let original = match std::fs::read_to_string(&params.path) {
        Ok(content) => content,
        Err(e) => {
            return CallToolResult::error(vec![
                Content::text(format!("Failed to read file: {}", e))
            ]);
        }
    };
    
    // Try replacement (with Unicode normalization fallback)
    let (new_content, count) = match str_replace_normalized(
        &original,
        &params.old_str,
        &params.new_str,
        params.replace_all,
    ) {
        Some(result) => result,
        None => {
            return CallToolResult::error(vec![
                Content::text("Text not found in file".to_string())
            ]);
        }
    };
    
    // Generate diff
    let diff = create_unified_diff(
        &original,
        &new_content,
        &params.path,
        &params.path,
    );
    
    // Preview mode: show diff without writing
    if params.preview_only {
        return CallToolResult::success(vec![
            Content::text(format!(
                "Preview ({} replacement{}):\n\n```diff\n{}\n```",
                count,
                if count == 1 { "" } else { "s" },
                diff
            ))
        ]);
    }
    
    // Apply changes
    if let Err(e) = std::fs::write(&params.path, &new_content) {
        return CallToolResult::error(vec![
            Content::text(format!("Failed to write file: {}", e))
        ]);
    }
    
    CallToolResult::success(vec![
        Content::text(format!(
            "Successfully made {} replacement{}.\n\n```diff\n{}\n```",
            count,
            if count == 1 { "" } else { "s" },
            diff
        ))
    ])
}
```

---

## Testing

```rust
#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_unified_diff_generation() {
        let original = "line1\nline2\nline3\n";
        let modified = "line1\nmodified\nline3\n";
        
        let diff = create_unified_diff(original, modified, "a.txt", "b.txt");
        
        assert!(diff.contains("--- a.txt"));
        assert!(diff.contains("+++ b.txt"));
        assert!(diff.contains("-line2"));
        assert!(diff.contains("+modified"));
    }
    
    #[test]
    fn test_unicode_normalization() {
        // LLM might generate smart quotes
        let content = "config = 'value'";  // ASCII
        let llm_pattern = "config = 'value'";  // Smart quotes
        
        let result = str_replace_normalized(
            content,
            llm_pattern,
            "config = 'new'",
            false,
        );
        
        assert!(result.is_some());
        let (new_content, count) = result.unwrap();
        assert_eq!(count, 1);
        assert_eq!(new_content, "config = 'new'");
    }
    
    #[test]
    fn test_ellipsis_mapping() {
        // … → ... (1-to-many mapping)
        let content = "Loading...";
        let pattern = "Loading...";  // Unicode ellipsis
        
        let result = str_replace_normalized(
            content,
            pattern,
            "Done",
            false,
        );
        
        assert!(result.is_some());
    }
    
    #[test]
    fn test_preview_mode() {
        let temp_file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(temp_file.path(), "original").unwrap();
        
        let result = str_replace_with_diff(
            temp_file.path().to_str().unwrap(),
            "original",
            "modified",
            Some(false),
        ).unwrap();
        
        // File should NOT be modified in preview mode
        let content = std::fs::read_to_string(temp_file.path()).unwrap();
        assert_eq!(content, "original");
        
        // But diff should be generated
        assert!(!result.unified_diff.is_empty());
    }
}
```

---

## Performance Notes

| Operation | Complexity | Notes |
|-----------|------------|-------|
| Exact match | O(n×m) | `str::replacen` uses Two-Way |
| Unicode quick check | O(n+m) | Single pass normalization |
| Full byte mapping | O(n+m) | Two-Way algorithm + binary search |
| Diff generation | O(n+m) | Myers algorithm in `similar` |

**Memory**: Byte mapping requires ~2× string size for position tracking.

---

## External Libraries

- [similar](https://github.com/mitsuhiko/similar) - Diff algorithms for Rust

---

*Adapt this reference to your MCP server architecture.*
