//! Tests for logging module

use clap::Parser;
use tempfile::TempDir;

use ssh_mcp::config::Args;

/// Test that logging configuration is parsed correctly without file.
///
/// Note: We don't call init_logging here because tracing allows only one global
/// subscriber. The actual init_logging call is tested in integration tests.
#[test]
fn test_logging_config_defaults() {
    // Test that args can be parsed with default logging settings
    let args = Args::try_parse_from(["ssh-mcp", "--host=test", "--user=test"]).unwrap();

    // Verify default logging config values
    assert_eq!(args.log_level, "info");
    assert!(args.log_file.is_none());
    assert_eq!(args.log_format, "text");
}

/// Test that logging configuration is parsed correctly with file options.
#[test]
fn test_logging_config_with_file() {
    let temp_dir = TempDir::new().unwrap();
    let log_file = temp_dir.path().join("test.log");

    let args = Args::try_parse_from([
        "ssh-mcp",
        "--host=test",
        "--user=test",
        "--log-file",
        log_file.to_str().unwrap(),
        "--log-format=json",
        "--log-rotation=daily",
    ])
    .unwrap();

    // Verify logging config values
    assert_eq!(args.log_level, "info");
    assert!(args.log_file.is_some());
    assert_eq!(args.log_file.as_deref(), Some(log_file.as_path()));
    assert_eq!(args.log_format, "json");
    assert_eq!(args.log_rotation, "daily");
}

/// Test logging with file output in a single test (to avoid global subscriber conflict).
///
/// This test initializes logging once and verifies file creation and JSON format.
#[tokio::test]
async fn test_logging_file_integration() {
    let temp_dir = TempDir::new().unwrap();
    let log_file = temp_dir.path().join("test.log");

    let args = Args::try_parse_from([
        "ssh-mcp",
        "--host=test",
        "--user=test",
        "--log-file",
        log_file.to_str().unwrap(),
        "--log-format=json",
        "--log-rotation=never",
    ])
    .unwrap();

    let _guard = ssh_mcp::logging::init_logging(&args).unwrap();

    // Log a test message
    tracing::info!("Test log message");

    // Flush (drop guard to ensure log is written)
    drop(_guard);

    // Verify log file was created
    assert!(log_file.exists(), "Log file not found at {:?}", log_file);

    // Verify log file contains JSON
    let contents = std::fs::read_to_string(&log_file).unwrap();
    assert!(contents.contains("Test log message"));
    assert!(contents.contains("\"level\":"));
}
