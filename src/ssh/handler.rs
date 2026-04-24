//! SSH client handler implementation
//!
//! Implements the `russh::client::Handler` trait to handle SSH connection events.

use std::path::PathBuf;

use anyhow::Context;
use russh::keys::HashAlg;
use tracing::{info, warn};

use super::config::HostKeyCheckMode;

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
                Ok(true)
            }
            HostKeyCheckMode::Yes => match self.check_known_hosts(server_public_key) {
                Ok(true) => Ok(true),
                Ok(false) => Err(anyhow::anyhow!(
                    "SSH host key verification failed for {}: unknown host key ({fingerprint}); add it to known_hosts or use --strict-host-key-checking=accept-new",
                    self.host_port()
                )),
                Err(e) => Err(anyhow::anyhow!(
                    "SSH host key verification failed for {}: {e} ({fingerprint})",
                    self.host_port()
                )),
            },
            HostKeyCheckMode::AcceptNew => match self.check_known_hosts(server_public_key) {
                Ok(true) => Ok(true),
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
                    Ok(true)
                }
                Err(e) => Err(anyhow::anyhow!(
                    "SSH host key verification failed for {}: {e} ({fingerprint})",
                    self.host_port()
                )),
            },
        }
    }
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
}
