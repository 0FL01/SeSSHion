//! Logging initialization and configuration
//!
//! This module provides functions to initialize logging based on CLI arguments.
//! It supports multiple log levels, file logging with rotation, and different
//! output formats (text or JSON).

use std::fs::{self, OpenOptions};
use std::path::Path;

use tracing::info;
use tracing_appender::{non_blocking, non_blocking::WorkerGuard, rolling};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Registry};

use crate::config::Args;
use crate::error::{Result, SshMcpError};

/// Initialize logging based on configuration arguments.
///
/// This function sets up tracing subscribers with:
/// - Environment-based log level filtering (falls back to `args.log_level`)
/// - Stderr output in text format (stdout is reserved for MCP protocol)
/// - Optional file logging with rotation support
///
/// # Returns
///
/// - `Ok(Some(WorkerGuard))` - File logging enabled, guard must be kept alive
/// - `Ok(None)` - No file logging configured
/// - `Err(SshMcpError)` - Configuration or IO error
///
/// # Note
///
/// The `WorkerGuard` must be stored for the entire program lifetime when file
/// logging is enabled, or the background worker will be dropped and logs may be lost.
pub fn init_logging(args: &Args) -> Result<Option<WorkerGuard>> {
    // Create env filter (respect RUST_LOG, fall back to args.log_level)
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&args.log_level));

    // Set up file layer if log_file is specified
    let (file_writer, guard) = if let Some(log_file) = &args.log_file {
        let (writer, worker_guard) = setup_file_logging(log_file, &args.log_rotation)?;
        (Some(writer), Some(worker_guard))
    } else {
        (None, None)
    };

    // Build subscriber with stderr (text) and optionally file (text or JSON)
    let registry = Registry::default().with(env_filter);

    // Always add stderr layer (text format only - stdout reserved for MCP protocol)
    let stderr_layer = fmt::layer().with_target(false).with_writer(std::io::stderr);

    match (file_writer, args.log_format.as_str()) {
        (Some(writer), "json") => {
            // File with JSON format
            let file_layer = fmt::layer().json().with_target(false).with_writer(writer);
            registry.with(stderr_layer).with(file_layer).init();
        }
        (Some(writer), _) => {
            // File with text format
            let file_layer = fmt::layer().with_target(false).with_writer(writer);
            registry.with(stderr_layer).with(file_layer).init();
        }
        (None, _) => {
            // No file logging
            registry.with(stderr_layer).init();
        }
    }

    // Emit startup log message explaining configuration and file naming
    if let Some(log_file) = &args.log_file {
        let rotation_note = match args.log_rotation.as_str() {
            "daily" => format!(" (actual file: {}.YYYY-MM-DD)", log_file.display()),
            "hourly" => format!(" (actual file: {}.YYYY-MM-DD-HH)", log_file.display()),
            _ => String::new(),
        };
        info!(
            "Logging initialized: file={}, format={}, rotation={}{}",
            log_file.display(),
            args.log_format,
            args.log_rotation,
            rotation_note
        );
    } else {
        info!("Logging initialized: stderr only, level={}", args.log_level);
    }

    Ok(guard)
}

/// Set up file logging with rotation configuration.
///
/// Returns a tuple of (non_blocking_writer, worker_guard) where:
/// - `non_blocking_writer`: The non-blocking writer for the log file
/// - `worker_guard`: Must be kept alive for the duration of the program
fn setup_file_logging(
    log_file: &Path,
    rotation: &str,
) -> Result<(non_blocking::NonBlocking, WorkerGuard)> {
    // Determine directory and file name for rolling logs
    let log_dir = log_file.parent().unwrap_or(Path::new(".")).to_path_buf();

    // Ensure parent directory exists for file logging
    if !log_dir.as_os_str().is_empty() && !log_dir.exists() {
        fs::create_dir_all(&log_dir).map_err(|e| {
            SshMcpError::Config(format!(
                "Failed to create log directory {}: {}",
                log_dir.display(),
                e
            ))
        })?;
    }

    let log_name = log_file
        .file_name()
        .ok_or_else(|| SshMcpError::Config("Log file path has no file name".to_string()))?
        .to_str()
        .ok_or_else(|| SshMcpError::Config("Log file name is not valid UTF-8".to_string()))?;

    // Create the appropriate appender and guard based on rotation strategy
    match rotation {
        "hourly" => {
            let appender = rolling::hourly(&log_dir, log_name);
            Ok(non_blocking(appender))
        }
        "never" => {
            // Use OpenOptions with append(true) to avoid truncation and ensure file creation
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(log_file)
                .map_err(|e| {
                    SshMcpError::Config(format!(
                        "Failed to open log file {}: {}",
                        log_file.display(),
                        e
                    ))
                })?;
            Ok(non_blocking(file))
        }
        _ => {
            // daily (default)
            let appender = rolling::daily(&log_dir, log_name);
            Ok(non_blocking(appender))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_setup_file_logging_creates_dirs() {
        let temp_dir = TempDir::new().unwrap();
        let log_file = temp_dir.path().join("a").join("b").join("test.log");

        let result = setup_file_logging(&log_file, "never");
        assert!(result.is_ok());

        assert!(log_file.parent().unwrap().exists());
        assert!(log_file.exists());
    }

    #[test]
    fn test_setup_file_logging_append_mode() {
        let temp_dir = TempDir::new().unwrap();
        let log_file = temp_dir.path().join("test.log");

        // Create file with some content
        std::fs::write(&log_file, "initial content\n").unwrap();

        {
            let (_writer, _guard) = setup_file_logging(&log_file, "never").unwrap();
            // Writer is non-blocking, but we just want to check if it truncated
        }

        let contents = std::fs::read_to_string(&log_file).unwrap();
        assert_eq!(contents, "initial content\n");
    }
}
