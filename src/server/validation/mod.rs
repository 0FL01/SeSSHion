//! Validation helpers for server operations.
//!
//! This module contains pure validation functions extracted from server.rs
//! to improve code organization and testability.

pub(crate) mod common;
pub(crate) mod file_edit;

// Re-export common items for convenience
pub(crate) use common::*;
