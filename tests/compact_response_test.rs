//! E2E tests for TransferResponse compact and JSON serialization
//!
//! These tests verify that TransferResponse correctly produces compact and verbose
//! JSON representations, including the required local_path and remote_path fields.

use ssh_mcp::transfer::{
    CompactTransferResponse, ResolvedPaths, StagingLocal, StagingRemote, TransferCounts,
    TransferKind, TransferOperation, TransferParams, TransferResponse, TransferStaging,
    TransferTransport,
};
use std::path::PathBuf;

/// Creates a sample TransferParams for testing
fn sample_params() -> TransferParams {
    TransferParams {
        operation: TransferOperation::Put,
        local_path: "config.yml".to_string(),
        remote_path: "/etc/app/config.yml".to_string(),
        transport: TransferTransport::ExecRaw,
        kind: Some(TransferKind::File),
        overwrite: true,
        timeout_ms: Some(30000),
        verbose: false,
    }
}

/// Creates a sample TransferCounts for testing
fn sample_counts() -> TransferCounts {
    TransferCounts {
        bytes: 1024,
        files: 1,
        directories: 0,
    }
}

/// Creates a successful TransferResponse for testing
fn sample_success_response() -> TransferResponse {
    TransferResponse {
        ok: true,
        error: None,
        params: sample_params(),
        kind: Some(TransferKind::File),
        transport_used: TransferTransport::ExecRaw,
        remote_home: Some("/home/user".to_string()),
        local_root: "/tmp/test".to_string(),
        resolved_paths: Some(ResolvedPaths {
            local_path: PathBuf::from("/tmp/test/config.yml"),
        }),
        staging: Some(TransferStaging {
            local: Some(StagingLocal {
                staging_path: "/tmp/test/.config.yml.staging".to_string(),
                backup_path: None,
                final_path: "/tmp/test/config.yml".to_string(),
            }),
            remote: Some(StagingRemote {
                staging_path: "/home/user/.staging/config.yml".to_string(),
                backup_path: None,
                final_path: "/etc/app/config.yml".to_string(),
                staging_base_home: "/home/user/.staging".to_string(),
            }),
        }),
        counts: Some(sample_counts()),
        elapsed_ms: Some(150),
        semantics: Some("test semantics".to_string()),
    }
}

/// Creates an error TransferResponse for testing
fn sample_error_response() -> TransferResponse {
    TransferResponse {
        ok: false,
        error: Some("Permission denied".to_string()),
        params: sample_params(),
        kind: None,
        transport_used: TransferTransport::ExecRaw,
        remote_home: None,
        local_root: "/tmp/test".to_string(),
        resolved_paths: None,
        staging: None,
        counts: None,
        elapsed_ms: Some(50),
        semantics: None,
    }
}

#[test]
fn test_compact_response_includes_paths() {
    // Create a sample response
    let response = sample_success_response();

    // Get compact representation
    let compact = response.to_compact();

    // Verify paths are included
    assert_eq!(compact.local_path, "config.yml");
    assert_eq!(compact.remote_path, "/etc/app/config.yml");
}

#[test]
fn test_compact_response_success_fields() {
    let response = sample_success_response();
    let compact = response.to_compact();

    // Verify basic fields
    assert!(compact.ok);
    assert_eq!(compact.error, None);
    assert_eq!(compact.kind, Some(TransferKind::File));
    assert_eq!(compact.local_path, "config.yml");
    assert_eq!(compact.remote_path, "/etc/app/config.yml");

    // Verify counts and timing
    assert!(compact.counts.is_some());
    let counts = compact.counts.unwrap();
    assert_eq!(counts.bytes, 1024);
    assert_eq!(counts.files, 1);
    assert_eq!(counts.directories, 0);
    assert_eq!(compact.elapsed_ms, Some(150));
}

#[test]
fn test_compact_response_error_fields() {
    let response = sample_error_response();
    let compact = response.to_compact();

    // Verify error state
    assert!(!compact.ok);
    assert_eq!(compact.error, Some("Permission denied".to_string()));
    assert_eq!(compact.kind, None);

    // Paths should still be present even on error
    assert_eq!(compact.local_path, "config.yml");
    assert_eq!(compact.remote_path, "/etc/app/config.yml");

    // Counts should be None on error
    assert_eq!(compact.counts, None);
    assert_eq!(compact.elapsed_ms, Some(50));
}

#[test]
fn test_to_json_false_includes_paths() {
    let response = sample_success_response();

    // Get non-verbose JSON
    let json_str = response
        .to_json(false)
        .expect("JSON serialization should succeed");

    // Parse the JSON
    let json: serde_json::Value = serde_json::from_str(&json_str).expect("Should parse as JSON");

    // Verify paths are present
    assert_eq!(json["local_path"].as_str(), Some("config.yml"));
    assert_eq!(json["remote_path"].as_str(), Some("/etc/app/config.yml"));

    // Verify basic fields
    assert_eq!(json["ok"].as_bool(), Some(true));
    assert_eq!(json["kind"].as_str(), Some("file"));
}

#[test]
fn test_to_json_false_excludes_verbose_fields() {
    let response = sample_success_response();
    let json_str = response
        .to_json(false)
        .expect("JSON serialization should succeed");
    let json: serde_json::Value = serde_json::from_str(&json_str).expect("Should parse as JSON");

    // Verify verbose fields are NOT present in non-verbose mode
    assert!(
        json.get("transport_used").is_none(),
        "transport_used should not be in compact JSON"
    );
    assert!(
        json.get("staging").is_none(),
        "staging should not be in compact JSON"
    );
    assert!(
        json.get("params").is_none(),
        "params should not be in compact JSON"
    );
    assert!(
        json.get("remote_home").is_none(),
        "remote_home should not be in compact JSON"
    );
    assert!(
        json.get("local_root").is_none(),
        "local_root should not be in compact JSON"
    );
    assert!(
        json.get("resolved_paths").is_none(),
        "resolved_paths should not be in compact JSON"
    );
    assert!(
        json.get("semantics").is_none(),
        "semantics should not be in compact JSON"
    );
}

#[test]
fn test_to_json_false_excludes_error_on_success() {
    let response = sample_success_response();
    let json_str = response
        .to_json(false)
        .expect("JSON serialization should succeed");
    let json: serde_json::Value = serde_json::from_str(&json_str).expect("Should parse as JSON");

    // Error field should be skipped when None (skip_serializing_if)
    assert!(
        json.get("error").is_none(),
        "error should be skipped when None"
    );
}

#[test]
fn test_to_json_false_includes_error_on_failure() {
    let response = sample_error_response();
    let json_str = response
        .to_json(false)
        .expect("JSON serialization should succeed");
    let json: serde_json::Value = serde_json::from_str(&json_str).expect("Should parse as JSON");

    // Error field should be present when set
    assert_eq!(json["error"].as_str(), Some("Permission denied"));
    assert_eq!(json["ok"].as_bool(), Some(false));
}

#[test]
fn test_to_json_true_includes_all_fields() {
    let response = sample_success_response();
    let json_str = response
        .to_json(true)
        .expect("JSON serialization should succeed");
    let json: serde_json::Value = serde_json::from_str(&json_str).expect("Should parse as JSON");

    // Verify all fields are present in verbose mode
    assert_eq!(json["ok"].as_bool(), Some(true));
    assert_eq!(json["kind"].as_str(), Some("file"));
    assert_eq!(
        json["transport_used"].as_str(),
        Some("exec-raw"),
        "transport_used should be present in verbose JSON"
    );
    assert_eq!(
        json["remote_home"].as_str(),
        Some("/home/user"),
        "remote_home should be present in verbose JSON"
    );
    assert_eq!(
        json["local_root"].as_str(),
        Some("/tmp/test"),
        "local_root should be present in verbose JSON"
    );
    assert_eq!(
        json["semantics"].as_str(),
        Some("test semantics"),
        "semantics should be present in verbose JSON"
    );

    // Verify params object is present
    assert!(
        json.get("params").is_some(),
        "params should be present in verbose JSON"
    );
    let params = json["params"]
        .as_object()
        .expect("params should be an object");
    assert_eq!(params["local_path"].as_str(), Some("config.yml"));
    assert_eq!(params["remote_path"].as_str(), Some("/etc/app/config.yml"));
    assert_eq!(params["operation"].as_str(), Some("put"));
    assert_eq!(params["transport"].as_str(), Some("exec-raw"));

    // Verify resolved_paths is present
    assert!(
        json.get("resolved_paths").is_some(),
        "resolved_paths should be present in verbose JSON"
    );

    // Verify staging is present
    assert!(
        json.get("staging").is_some(),
        "staging should be present in verbose JSON"
    );
    let staging = json["staging"]
        .as_object()
        .expect("staging should be an object");
    assert!(
        staging.get("local").is_some(),
        "staging.local should be present"
    );
    assert!(
        staging.get("remote").is_some(),
        "staging.remote should be present"
    );

    // Verify counts are present
    assert!(
        json.get("counts").is_some(),
        "counts should be present in verbose JSON"
    );
    let counts = json["counts"]
        .as_object()
        .expect("counts should be an object");
    assert_eq!(counts["bytes"].as_u64(), Some(1024));
    assert_eq!(counts["files"].as_u64(), Some(1));

    // Verify elapsed_ms is present
    assert_eq!(json["elapsed_ms"].as_u64(), Some(150));
}

#[test]
fn test_to_json_true_error_response() {
    let response = sample_error_response();
    let json_str = response
        .to_json(true)
        .expect("JSON serialization should succeed");
    let json: serde_json::Value = serde_json::from_str(&json_str).expect("Should parse as JSON");

    // Verify error fields in verbose mode
    assert_eq!(json["ok"].as_bool(), Some(false));
    assert_eq!(json["error"].as_str(), Some("Permission denied"));

    // params should still be present
    assert!(
        json.get("params").is_some(),
        "params should be present even in error"
    );

    // These fields should be null or absent in error case
    assert!(
        json.get("staging").is_none() || json["staging"].is_null(),
        "staging should be absent/null on error"
    );
    assert!(
        json.get("counts").is_none() || json["counts"].is_null(),
        "counts should be absent/null on error"
    );
}

#[test]
fn test_compact_response_counts_skipped_when_none() {
    // Create response with no counts
    let mut response = sample_error_response();
    response.counts = None;
    response.elapsed_ms = None;

    let compact = response.to_compact();
    let json_str = serde_json::to_string(&compact).expect("Compact serialization should succeed");
    let json: serde_json::Value = serde_json::from_str(&json_str).expect("Should parse as JSON");

    // Counts and elapsed_ms should be skipped when None (skip_serializing_if)
    assert!(
        json.get("counts").is_none(),
        "counts should be skipped when None"
    );
    assert!(
        json.get("elapsed_ms").is_none(),
        "elapsed_ms should be skipped when None"
    );
}

#[test]
fn test_compact_response_directory_kind() {
    let mut response = sample_success_response();
    response.kind = Some(TransferKind::Directory);
    response.params.local_path = "mydir".to_string();
    response.params.remote_path = "/opt/app/mydir".to_string();

    let compact = response.to_compact();

    assert_eq!(compact.kind, Some(TransferKind::Directory));
    assert_eq!(compact.local_path, "mydir");
    assert_eq!(compact.remote_path, "/opt/app/mydir");

    // Verify JSON serialization
    let json_str = response
        .to_json(false)
        .expect("JSON serialization should succeed");
    let json: serde_json::Value = serde_json::from_str(&json_str).expect("Should parse as JSON");

    assert_eq!(json["kind"].as_str(), Some("directory"));
    assert_eq!(json["local_path"].as_str(), Some("mydir"));
    assert_eq!(json["remote_path"].as_str(), Some("/opt/app/mydir"));
}

#[test]
fn test_to_json_invalid_utf8_paths() {
    // Test with paths that might contain special characters
    let mut params = sample_params();
    params.local_path = "file with spaces.txt".to_string();
    params.remote_path = "/path/with spaces/and-unicode-文件.txt".to_string();

    let response = TransferResponse {
        ok: true,
        error: None,
        params,
        kind: Some(TransferKind::File),
        transport_used: TransferTransport::ExecRaw,
        remote_home: None,
        local_root: "/tmp".to_string(),
        resolved_paths: None,
        staging: None,
        counts: None,
        elapsed_ms: None,
        semantics: None,
    };

    // Test compact JSON
    let compact = response.to_compact();
    assert_eq!(compact.local_path, "file with spaces.txt");
    assert_eq!(
        compact.remote_path,
        "/path/with spaces/and-unicode-文件.txt"
    );

    // Test JSON serialization handles special characters
    let json_str = response
        .to_json(false)
        .expect("JSON serialization should succeed");
    let json: serde_json::Value = serde_json::from_str(&json_str).expect("Should parse as JSON");

    assert_eq!(json["local_path"].as_str(), Some("file with spaces.txt"));
    assert_eq!(
        json["remote_path"].as_str(),
        Some("/path/with spaces/and-unicode-文件.txt")
    );
}

#[test]
fn test_compact_serialization_roundtrip() {
    let response = sample_success_response();
    let compact = response.to_compact();

    // Serialize compact response
    let json_str = serde_json::to_string(&compact).expect("Compact serialization should succeed");

    // Deserialize back
    let deserialized: CompactTransferResponse =
        serde_json::from_str(&json_str).expect("Should deserialize");

    // Verify all fields match
    assert_eq!(deserialized.ok, compact.ok);
    assert_eq!(deserialized.error, compact.error);
    assert_eq!(deserialized.kind, compact.kind);
    assert_eq!(deserialized.local_path, compact.local_path);
    assert_eq!(deserialized.remote_path, compact.remote_path);
    assert_eq!(deserialized.elapsed_ms, compact.elapsed_ms);

    // Compare counts
    assert!(deserialized.counts.is_some());
    let orig_counts = compact.counts.unwrap();
    let deser_counts = deserialized.counts.unwrap();
    assert_eq!(deser_counts.bytes, orig_counts.bytes);
    assert_eq!(deser_counts.files, orig_counts.files);
    assert_eq!(deser_counts.directories, orig_counts.directories);
}
