//! Validation helpers for server operations.
//!
//! This module contains pure validation functions extracted from server.rs
//! to improve code organization and testability.

pub(crate) mod apply_file_edit;
pub(crate) mod common;
pub(crate) mod read_file;

// Re-export common items for convenience
pub(crate) use apply_file_edit::*;
pub(crate) use common::*;
pub(crate) use read_file::*;

// Re-export constants for test access
pub(crate) use apply_file_edit::APPLY_FILE_EDIT_HARD_MAX_BYTES;
pub(crate) use read_file::SHA256_HEX_LEN;
#[cfg(test)]
pub(crate) use read_file::{
    READ_FILE_BYTES_PER_TOKEN, READ_FILE_DEFAULT_PREVIEW_LINES, READ_FILE_HARD_MAX_BYTES,
    READ_FILE_MAX_LINE_WINDOW,
};
