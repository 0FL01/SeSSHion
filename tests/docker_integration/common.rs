//! Common test utilities for Docker integration tests

pub use ssh_mcp::transfer::{TransferKind, TransferOperation, TransferParams, TransferTransport};
pub use ssh_mcp::{Config, SshMcpServer};
pub use std::sync::Once;
pub use testcontainers::runners::AsyncRunner;
pub use testcontainers::{GenericImage, ImageExt};

/// Static storage for build result using std::sync::Mutex for thread safety
pub use std::sync::Mutex;

pub static IMAGE_BUILD_ONCE: Once = Once::new();
pub static IMAGE_BUILD_RESULT: Mutex<Option<Result<(), String>>> = Mutex::new(None);

/// Build the custom Debian SSH Docker images if not already present.
/// Checks and builds:
/// - ssh-mcp-debian-sshd:latest
/// - ssh-mcp-debian-sshd-norsync:latest
/// - ssh-mcp-debian-sshd-fish:latest
pub fn ensure_debian_sshd_image() -> Result<(), String> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let mut errors = Vec::new();

    // Helper closure to check and build a single image
    let check_and_build = |image_name: &str, dockerfile_path: &str| {
        // Check if the image already exists
        let output = std::process::Command::new("docker")
            .args(["images", "--format", "{{.Repository}}:{{.Tag}}", image_name])
            .output()
            .map_err(|e| format!("Failed to check if Docker image {image_name} exists: {e}"))?;

        let existing = String::from_utf8_lossy(&output.stdout);
        if existing.trim() == image_name {
            return Ok(());
        }

        // Build the image from the Dockerfile
        let output = std::process::Command::new("docker")
            .args([
                "build",
                "-t",
                image_name,
                "-f",
                dockerfile_path,
                manifest_dir,
            ])
            .output()
            .map_err(|e| format!("Failed to build Docker image {image_name}: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("Docker build failed for {image_name}: {stderr}"));
        }

        Ok(())
    };

    // Check and build first image
    let dockerfile_path = format!("{}/tests/fixtures/debian-sshd/Dockerfile", manifest_dir);
    if let Err(e) = check_and_build("ssh-mcp-debian-sshd:latest", &dockerfile_path) {
        errors.push(e);
    }

    // Check and build second image (norsync variant)
    let norsync_dockerfile_path = format!(
        "{}/tests/fixtures/debian-sshd-norsync/Dockerfile",
        manifest_dir
    );
    if let Err(e) = check_and_build(
        "ssh-mcp-debian-sshd-norsync:latest",
        &norsync_dockerfile_path,
    ) {
        errors.push(e);
    }

    // Check and build third image (fish login shell variant)
    let fish_dockerfile_path = format!(
        "{}/tests/fixtures/debian-sshd-fish/Dockerfile",
        manifest_dir
    );
    if let Err(e) = check_and_build("ssh-mcp-debian-sshd-fish:latest", &fish_dockerfile_path) {
        errors.push(e);
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// Initialize the test environment - builds the Docker image once
pub fn init_test_env() -> Result<(), String> {
    IMAGE_BUILD_ONCE.call_once(|| {
        let result = ensure_debian_sshd_image();
        let mut guard = IMAGE_BUILD_RESULT
            .lock()
            .expect("IMAGE_BUILD_RESULT poisoned");
        *guard = Some(result);
    });

    let guard = IMAGE_BUILD_RESULT
        .lock()
        .expect("IMAGE_BUILD_RESULT poisoned");
    guard.as_ref().expect("IMAGE_BUILD_RESULT not set").clone()
}

/// Helper to extract text content from a CallToolResult
pub fn extract_text_from_result(result: &rmcp::model::CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| {
            // Content is Annotated<RawContent>, and RawContent has a text() method
            // Access to raw field to get the underlying RawContent
            c.raw
                .as_text()
                .map(|text_content| text_content.text.clone())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Test SSH key constants
pub const TEST_PRIVATE_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\n\
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW\n\
QyNTUxOQAAACCZ7b1U1KOd6jVsDPOFQZFVot4BaNM+2hTy6RiD/Ttc+QAAAJD4/zqo+P86\n\
qAAAAAtzc2gtZWQyNTUxOQAAACCZ7b1U1KOd6jVsDPOFQZFVot4BaNM+2hTy6RiD/Ttc+Q\n\
AAAEDCxgrF63olxn5oZkm+x+wntKjbSB9nWO+mazmilqLU5pntvVTUo53qNWwM84VBkVWi\n\
3gFo0z7aFPLpGIP9O1z5AAAADHNzaC1tY3AtdGVzdAE=\n\
-----END OPENSSH PRIVATE KEY-----\n";

pub const TEST_PUBLIC_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJntvVTUo53qNWwM84VBkVWi3gFo0z7aFPLpGIP9O1z5 ssh-mcp-test";

/// Setup SSH key for authentication
pub fn setup_test_key() -> (tempfile::TempDir, std::path::PathBuf) {
    let key_dir = tempfile::TempDir::new().expect("tempdir");
    let key_path = key_dir.path().join("id_ed25519");
    std::fs::write(&key_path, TEST_PRIVATE_KEY).expect("write private key");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(&key_path, perms).expect("chmod key");
    }
    (key_dir, key_path)
}

/// Check if sftp is available locally
pub fn check_sftp() -> bool {
    match std::process::Command::new("sftp").arg("-V").output() {
        Ok(out) if out.status.success() => true,
        Ok(out) => {
            tracing::warn!(
                "local 'sftp' exists but '-V' failed (status={}); treating as unavailable",
                out.status
            );
            false
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            tracing::warn!("failed to spawn local 'sftp -V': {e}; treating as unavailable");
            false
        }
    }
}

/// Check if scp is available locally
pub fn check_scp() -> bool {
    match std::process::Command::new("scp").arg("-V").output() {
        Ok(out) if out.status.success() => true,
        Ok(out) => {
            tracing::warn!(
                "local 'scp' exists but '-V' failed (status={}); treating as unavailable",
                out.status
            );
            false
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            tracing::warn!("failed to spawn local 'scp -V': {e}; treating as unavailable");
            false
        }
    }
}

/// Check if rsync is available locally
pub fn check_rsync() -> bool {
    match std::process::Command::new("rsync")
        .arg("--version")
        .output()
    {
        Ok(out) if out.status.success() => true,
        Ok(out) => {
            tracing::warn!(
                "local 'rsync' exists but '--version' failed (status={}); treating as unavailable",
                out.status
            );
            false
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            tracing::warn!("failed to spawn local 'rsync --version': {e}; treating as unavailable");
            false
        }
    }
}

/// Check if an OpenSSH client binary is available locally.
/// Note: some non-OpenSSH implementations may return non-zero for `-V`.
pub fn check_openssh_client(bin: &str) -> bool {
    match std::process::Command::new(bin).arg("-V").output() {
        Ok(out) if out.status.success() => true,
        Ok(out) => {
            tracing::warn!(
                "local '{bin}' exists but '{bin} -V' failed (status={}); treating as unavailable",
                out.status
            );
            false
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            tracing::warn!("failed to spawn local '{bin} -V': {e}; treating as unavailable");
            false
        }
    }
}
