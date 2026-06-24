//! SSH client handler implementation
//!
//! Implements the `russh::client::Handler` trait to handle SSH connection events.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use russh::keys::HashAlg;
use tracing::{info, warn};

use super::config::HostKeyCheckMode;

/// Outcome of a host key check, recorded for recovery decisions in the
/// connection layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyCheckOutcome {
    /// Key matched an existing known_hosts entry.
    Accepted,
    /// New key was learned and accepted (accept-new mode only).
    AcceptedNew,
    /// Key differs from the known_hosts entry (rotation or MITM).
    KeyChanged,
    /// Unknown key was rejected (strict mode only).
    UnknownRejected,
}

/// SSH client handler for russh
///
/// This handler is used by russh to process SSH events such as server key
/// verification.
#[derive(Debug, Clone)]
pub struct SshHandler {
    host: String,
    port: u16,
    host_key_checking: HostKeyCheckMode,
    known_hosts: Option<PathBuf>,
    /// Shared state to record the key check outcome for connection-layer
    /// recovery decisions.  Set by the connection manager before each
    /// connect attempt; `None` when not needed (tests, default handler).
    key_check_outcome: Option<Arc<Mutex<Option<KeyCheckOutcome>>>>,
}

impl SshHandler {
    /// Create a new SSH handler
    pub fn new(
        host: impl Into<String>,
        port: u16,
        host_key_checking: HostKeyCheckMode,
        known_hosts: Option<PathBuf>,
    ) -> Self {
        Self {
            host: host.into(),
            port,
            host_key_checking,
            known_hosts,
            key_check_outcome: None,
        }
    }

    fn check_known_hosts(
        &self,
        server_public_key: &russh::keys::PublicKey,
    ) -> std::result::Result<bool, russh::keys::Error> {
        if let Some(path) = &self.known_hosts {
            russh::keys::known_hosts::check_known_hosts_path(
                &self.host,
                self.port,
                server_public_key,
                path,
            )
        } else {
            russh::keys::known_hosts::check_known_hosts(&self.host, self.port, server_public_key)
        }
    }

    fn learn_known_hosts(
        &self,
        server_public_key: &russh::keys::PublicKey,
    ) -> std::result::Result<(), russh::keys::Error> {
        if let Some(path) = &self.known_hosts {
            russh::keys::known_hosts::learn_known_hosts_path(
                &self.host,
                self.port,
                server_public_key,
                path,
            )
        } else {
            russh::keys::known_hosts::learn_known_hosts(&self.host, self.port, server_public_key)
        }
    }

    fn host_port(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    fn fingerprint(server_public_key: &russh::keys::PublicKey) -> String {
        server_public_key.fingerprint(HashAlg::Sha256).to_string()
    }

    /// Record the key check outcome into shared state if attached.
    fn record_outcome(&self, outcome: KeyCheckOutcome) {
        if let Some(ref state) = self.key_check_outcome {
            if let Ok(mut guard) = state.lock() {
                *guard = Some(outcome);
            }
        }
    }

    /// Attach shared state for recording the key check outcome.
    ///
    /// The connection manager calls this before each connect attempt so
    /// that `do_connect` can inspect the outcome after a failure and
    /// decide whether to retry (e.g. remove a stale entry on key change).
    pub fn with_key_check_outcome(
        mut self,
        outcome: Arc<Mutex<Option<KeyCheckOutcome>>>,
    ) -> Self {
        self.key_check_outcome = Some(outcome);
        self
    }
}

impl Default for SshHandler {
    fn default() -> Self {
        Self::new("localhost", 22, HostKeyCheckMode::No, None)
    }
}

impl russh::client::Handler for SshHandler {
    type Error = anyhow::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::PublicKey,
    ) -> Result<bool, Self::Error> {
        let fingerprint = Self::fingerprint(server_public_key);

        match self.host_key_checking {
            HostKeyCheckMode::No => {
                warn!(
                    host = %self.host,
                    port = self.port,
                    fingerprint = %fingerprint,
                    "SSH host key verification disabled"
                );
                self.record_outcome(KeyCheckOutcome::Accepted);
                Ok(true)
            }
            HostKeyCheckMode::Yes => match self.check_known_hosts(server_public_key) {
                Ok(true) => {
                    self.record_outcome(KeyCheckOutcome::Accepted);
                    Ok(true)
                }
                Ok(false) => {
                    self.record_outcome(KeyCheckOutcome::UnknownRejected);
                    Err(anyhow::anyhow!(
                        "SSH host key verification failed for {}: unknown host key ({fingerprint}); add it to known_hosts or use --strict-host-key-checking=accept-new",
                        self.host_port()
                    ))
                }
                Err(e) => {
                    self.record_outcome(KeyCheckOutcome::KeyChanged);
                    Err(anyhow::anyhow!(
                        "SSH host key verification failed for {}: {e} ({fingerprint})",
                        self.host_port()
                    ))
                }
            },
            HostKeyCheckMode::AcceptNew => match self.check_known_hosts(server_public_key) {
                Ok(true) => {
                    self.record_outcome(KeyCheckOutcome::Accepted);
                    Ok(true)
                }
                Ok(false) => {
                    self.learn_known_hosts(server_public_key).with_context(|| {
                        format!(
                            "failed to record SSH host key for {} ({fingerprint})",
                            self.host_port()
                        )
                    })?;
                    info!(
                        host = %self.host,
                        port = self.port,
                        fingerprint = %fingerprint,
                        "Recorded new SSH host key"
                    );
                    self.record_outcome(KeyCheckOutcome::AcceptedNew);
                    Ok(true)
                }
                Err(e) => {
                    self.record_outcome(KeyCheckOutcome::KeyChanged);
                    Err(anyhow::anyhow!(
                        "SSH host key verification failed for {}: {e} ({fingerprint})",
                        self.host_port()
                    ))
                }
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Known-hosts entry removal
// ---------------------------------------------------------------------------

/// Remove known_hosts entries matching `host:port`.
///
/// Reads the file line by line, drops entries whose host-pattern field
/// matches, and writes the result back atomically (temp file + rename).
/// Comments, blank lines, and hashed (`|1|…`) entries are preserved.
///
/// Returns `Ok(())` when the file does not exist (nothing to remove).
pub fn remove_known_hosts_entry(
    host: &str,
    port: u16,
    known_hosts: &Path,
) -> io::Result<()> {
    let content = match std::fs::read_to_string(known_hosts) {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };

    let kept: Vec<&str> = content
        .lines()
        .filter(|line| !line_matches_host(line, host, port))
        .collect();

    let mut new_content = kept.join("\n");
    if content.ends_with('\n') {
        new_content.push('\n');
    }

    // Atomic write: temp file alongside, then rename.
    let mut temp_path = known_hosts.as_os_str().to_owned();
    temp_path.push(".tmp");
    let temp_path = PathBuf::from(temp_path);

    std::fs::write(&temp_path, &new_content)?;
    std::fs::rename(&temp_path, known_hosts)?;

    Ok(())
}

/// Resolve the default known_hosts path (`$HOME/.ssh/known_hosts`).
///
/// Returns `None` when `HOME` is not set.
pub fn default_known_hosts_path() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(|home| PathBuf::from(home).join(".ssh").join("known_hosts"))
}

/// Check whether a single known_hosts line matches `host:port`.
fn line_matches_host(line: &str, host: &str, port: u16) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return false;
    }
    let first_field = match trimmed.split_whitespace().next() {
        Some(f) => f,
        None => return false,
    };
    for entry in first_field.split(',') {
        let entry = entry.trim();
        if entry.starts_with("|1|") {
            continue; // hashed entry — cannot match by host
        }
        if port == 22 {
            if entry == host {
                return true;
            }
        } else {
            let expected = format!("[{}]:{}", host, port);
            if entry == expected {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh::client::Handler as _;

    const KEY_ONE: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ";
    const KEY_TWO: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILIG2T/B0l0gaqj3puu510tu9N1OkQ4znY3LYuEm5zCF";

    fn public_key(s: &str) -> russh::keys::PublicKey {
        russh::keys::PublicKey::from_openssh(s).expect("test public key should parse")
    }

    #[test]
    fn test_handler_creation() {
        let handler = SshHandler::new("localhost", 22, HostKeyCheckMode::No, None);
        assert!(format!("{:?}", handler).contains("SshHandler"));
    }

    #[test]
    fn test_handler_default() {
        let _handler: SshHandler = Default::default();
    }

    #[tokio::test]
    async fn test_strict_rejects_unknown_host_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let known_hosts = dir.path().join("known_hosts");
        let key = public_key(KEY_ONE);
        let mut handler =
            SshHandler::new("example.com", 22, HostKeyCheckMode::Yes, Some(known_hosts));

        let err = handler
            .check_server_key(&key)
            .await
            .expect_err("strict mode should reject unknown host key");

        assert!(err.to_string().contains("unknown host key"));
    }

    #[tokio::test]
    async fn test_accept_new_records_host_key_then_strict_accepts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let known_hosts = dir.path().join("known_hosts");
        let key = public_key(KEY_ONE);

        let mut accept_new = SshHandler::new(
            "example.com",
            22,
            HostKeyCheckMode::AcceptNew,
            Some(known_hosts.clone()),
        );
        assert!(
            accept_new
                .check_server_key(&key)
                .await
                .expect("accept-new should record unknown host key")
        );

        let contents = std::fs::read_to_string(&known_hosts).expect("known_hosts should exist");
        assert!(contents.contains("example.com ssh-ed25519"));

        let mut strict =
            SshHandler::new("example.com", 22, HostKeyCheckMode::Yes, Some(known_hosts));
        assert!(
            strict
                .check_server_key(&key)
                .await
                .expect("strict mode should accept recorded host key")
        );
    }

    #[tokio::test]
    async fn test_accept_new_rejects_changed_host_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let known_hosts = dir.path().join("known_hosts");
        let old_key = public_key(KEY_ONE);
        let new_key = public_key(KEY_TWO);

        russh::keys::known_hosts::learn_known_hosts_path("example.com", 22, &old_key, &known_hosts)
            .expect("should write known_hosts");

        let mut handler = SshHandler::new(
            "example.com",
            22,
            HostKeyCheckMode::AcceptNew,
            Some(known_hosts),
        );
        let err = handler
            .check_server_key(&new_key)
            .await
            .expect_err("accept-new should reject changed host key");

        assert!(err.to_string().contains("server key changed"));
    }

    #[test]
    fn test_line_matches_host() {
        // Non-standard port — bracketed form
        assert!(line_matches_host(
            "[127.0.0.1]:2222 ssh-ed25519 AAAA...",
            "127.0.0.1",
            2222
        ));
        assert!(!line_matches_host(
            "[127.0.0.1]:2223 ssh-ed25519 AAAA...",
            "127.0.0.1",
            2222
        ));

        // Standard port 22 — plain hostname
        assert!(line_matches_host(
            "example.com ssh-ed25519 AAAA...",
            "example.com",
            22
        ));
        assert!(!line_matches_host(
            "other.com ssh-ed25519 AAAA...",
            "example.com",
            22
        ));

        // Comma-separated multi-host entry
        assert!(line_matches_host(
            "host1,[127.0.0.1]:2222,host3 ssh-ed25519 AAAA...",
            "127.0.0.1",
            2222
        ));
        assert!(line_matches_host(
            "host1,[127.0.0.1]:2222,host3 ssh-ed25519 AAAA...",
            "host3",
            22
        ));

        // Comments and blank lines preserved (not matched)
        assert!(!line_matches_host("# comment line", "host", 22));
        assert!(!line_matches_host("", "host", 22));
        assert!(!line_matches_host("   ", "host", 22));

        // Hashed entries cannot be matched by host
        assert!(!line_matches_host(
            "|1|c3No|base64 ssh-ed25519 AAAA...",
            "host",
            22
        ));
    }

    #[test]
    fn test_remove_known_hosts_entry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let known_hosts = dir.path().join("known_hosts");
        let old_key = public_key(KEY_ONE);

        // Learn an entry for example.com:2222
        russh::keys::known_hosts::learn_known_hosts_path("example.com", 2222, &old_key, &known_hosts)
            .expect("should write known_hosts");

        let before = std::fs::read_to_string(&known_hosts).expect("read");
        assert!(before.contains("example.com"));

        // Remove the entry
        remove_known_hosts_entry("example.com", 2222, &known_hosts)
            .expect("should remove entry");

        let after = std::fs::read_to_string(&known_hosts).expect("read");
        assert!(!after.contains("example.com"), "entry should be gone");

        // Removing from a non-existent file is a no-op
        let missing = dir.path().join("nope");
        remove_known_hosts_entry("example.com", 2222, &missing)
            .expect("non-existent file should be Ok");
    }

    #[tokio::test]
    async fn test_key_change_recovery_flow() {
        let dir = tempfile::tempdir().expect("tempdir");
        let known_hosts = dir.path().join("known_hosts");
        let old_key = public_key(KEY_ONE);
        let new_key = public_key(KEY_TWO);

        // 1 — learn old key
        russh::keys::known_hosts::learn_known_hosts_path(
            "example.com",
            2222,
            &old_key,
            &known_hosts,
        )
        .expect("should write known_hosts");

        // 2 — new key is rejected (KeyChanged)
        let outcome = Arc::new(Mutex::new(None));
        let mut handler = SshHandler::new(
            "example.com",
            2222,
            HostKeyCheckMode::AcceptNew,
            Some(known_hosts.clone()),
        )
        .with_key_check_outcome(outcome.clone());

        let err = handler
            .check_server_key(&new_key)
            .await
            .expect_err("changed key should be rejected");
        assert!(err.to_string().contains("server key changed"));

        {
            let guard = outcome.lock().unwrap();
            assert_eq!(*guard, Some(KeyCheckOutcome::KeyChanged));
        }

        // 3 — remove stale entry
        remove_known_hosts_entry("example.com", 2222, &known_hosts)
            .expect("should remove entry");

        // 4 — new key is now accepted as a new host
        let outcome2 = Arc::new(Mutex::new(None));
        let mut handler2 = SshHandler::new(
            "example.com",
            2222,
            HostKeyCheckMode::AcceptNew,
            Some(known_hosts.clone()),
        )
        .with_key_check_outcome(outcome2.clone());

        assert!(
            handler2
                .check_server_key(&new_key)
                .await
                .expect("new key should be accepted after entry removal")
        );

        {
            let guard = outcome2.lock().unwrap();
            assert_eq!(*guard, Some(KeyCheckOutcome::AcceptedNew));
        }

        // Verify the new key is now in known_hosts
        let contents = std::fs::read_to_string(&known_hosts).expect("read");
        assert!(contents.contains("example.com"));
    }
}
