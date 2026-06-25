//! Cryptographic read-ticket module.
//!
//! A `TicketSigner` issues short-lived HMAC-SHA256 tickets that prove a file
//! path was read at a known point in time.  The server creates one signer at
//! startup (ephemeral key); the ticket is opaque to the client.
//!
//! # Ticket format
//!
//! ```text
//! rt1.{expiry_epoch_secs}.{hmac_hex}
//! rt2.{expiry_epoch_secs}.{content_sha256}.{hmac_hex}
//! ```
//!
//! # HMAC input
//!
//! ```text
//! "rt1\0{path}\0{expiry_epoch_secs}"
//! "rt2\0{path}\0{content_sha256}\0{expiry_epoch_secs}"
//! ```
//!
//! `rt2` is the current format. It binds both path and content SHA-256, so a
//! write can safely derive an implicit optimistic-lock baseline from the read.
//! `rt1` remains accepted for backward compatibility within a single process.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Default ticket lifetime: 10 minutes.
pub const DEFAULT_TICKET_TTL_SECS: u64 = 600;

// ── Error type ───────────────────────────────────────────────────────────────

/// Errors returned when verifying a read-ticket.
#[derive(Debug, PartialEq, Eq)]
pub enum TicketError {
    /// The ticket string is syntactically invalid.
    Malformed,
    /// The ticket's expiry timestamp is in the past.
    Expired,
    /// The HMAC did not match (wrong path, tampered data, or wrong key).
    InvalidSignature,
}

impl fmt::Display for TicketError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TicketError::Malformed => f.write_str("read ticket is malformed"),
            TicketError::Expired => f.write_str("read ticket has expired"),
            TicketError::InvalidSignature => f.write_str("read ticket has an invalid signature"),
        }
    }
}

impl std::error::Error for TicketError {}

/// Verified read-ticket claims.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TicketClaims {
    content_sha256: Option<String>,
}

impl TicketClaims {
    /// Returns the content SHA-256 bound into the ticket, when present.
    pub fn content_sha256(&self) -> Option<&str> {
        self.content_sha256.as_deref()
    }
}

// ── Signer ───────────────────────────────────────────────────────────────────

/// Signs and verifies read-tickets using an ephemeral 256-bit HMAC key.
///
/// Create one instance per process lifetime via [`TicketSigner::new`].
pub struct TicketSigner {
    key: [u8; 32],
}

impl TicketSigner {
    /// Creates a new signer with a cryptographically random key.
    ///
    /// # Panics
    ///
    /// Panics if the OS CSPRNG is unavailable — an unrecoverable situation.
    pub fn new() -> Self {
        let mut key = [0u8; 32];
        getrandom::fill(&mut key).unwrap_or_else(|e| panic!("OS CSPRNG unavailable: {e}"));
        Self { key }
    }

    /// Issues a ticket for `path` + `content_sha256` that expires after `ttl_secs` seconds.
    ///
    /// Returns the opaque ticket string `"rt2.{exp}.{content_sha256}.{hmac_hex}"`.
    pub fn issue(&self, path: &str, content_sha256: &str, ttl_secs: u64) -> String {
        let exp = now_epoch_secs().saturating_add(ttl_secs);
        let mac_hex = compute_hmac_hex_v2(&self.key, path, content_sha256, exp);
        format!("rt2.{exp}.{content_sha256}.{mac_hex}")
    }

    /// Verifies that `ticket` was issued for `expected_path` and has not expired.
    ///
    /// Returns parsed claims on success, or a [`TicketError`] describing the failure.
    pub fn verify(&self, ticket: &str, expected_path: &str) -> Result<TicketClaims, TicketError> {
        let parts: Vec<&str> = ticket.split('.').collect();
        match parts.as_slice() {
            ["rt1", exp_raw, signature_hex] => {
                let exp = parse_unexpired_expiry(exp_raw)?;
                verify_hmac_hex(
                    &self.key,
                    hmac_message_v1(expected_path, exp),
                    signature_hex,
                )?;
                Ok(TicketClaims {
                    content_sha256: None,
                })
            }
            ["rt2", exp_raw, content_sha256, signature_hex] => {
                let exp = parse_unexpired_expiry(exp_raw)?;
                if content_sha256.len() != 64 || !hex::is_valid_hex(content_sha256) {
                    return Err(TicketError::Malformed);
                }
                verify_hmac_hex(
                    &self.key,
                    hmac_message_v2(expected_path, content_sha256, exp),
                    signature_hex,
                )?;
                Ok(TicketClaims {
                    content_sha256: Some((*content_sha256).to_string()),
                })
            }
            _ => Err(TicketError::Malformed),
        }
    }
}

impl Default for TicketSigner {
    fn default() -> Self {
        Self::new()
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Returns seconds since UNIX epoch; saturates to 0 on clock anomalies.
fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn parse_unexpired_expiry(exp_raw: &str) -> Result<u64, TicketError> {
    let exp: u64 = exp_raw.parse().map_err(|_| TicketError::Malformed)?;
    if exp <= now_epoch_secs() {
        return Err(TicketError::Expired);
    }
    Ok(exp)
}

/// Builds the HMAC message `"rt1\0{path}\0{exp}"`.
fn hmac_message_v1(path: &str, exp: u64) -> String {
    format!("rt1\0{path}\0{exp}")
}

/// Builds the HMAC message `"rt2\0{path}\0{content_sha256}\0{exp}"`.
fn hmac_message_v2(path: &str, content_sha256: &str, exp: u64) -> String {
    format!("rt2\0{path}\0{content_sha256}\0{exp}")
}

/// Computes `HMAC-SHA256(key, message)` and returns the lowercase hex digest.
fn compute_hmac_hex(message: &str, key: &[u8; 32]) -> String {
    // SAFETY: HMAC-SHA256 accepts any key length; a 32-byte key is always valid.
    // The `Err` branch is structurally unreachable.
    let mut mac = HmacSha256::new_from_slice(key)
        .unwrap_or_else(|_| unreachable!("HMAC-SHA256 accepts any key length"));
    mac.update(message.as_bytes());
    let result = mac.finalize();
    let bytes = result.into_bytes();
    bytes.iter().fold(String::with_capacity(64), |mut acc, b| {
        use std::fmt::Write as _;
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

fn compute_hmac_hex_v2(key: &[u8; 32], path: &str, content_sha256: &str, exp: u64) -> String {
    compute_hmac_hex(&hmac_message_v2(path, content_sha256, exp), key)
}

fn verify_hmac_hex(
    key: &[u8; 32],
    message: String,
    signature_hex: &str,
) -> Result<(), TicketError> {
    if signature_hex.len() != 64 {
        return Err(TicketError::Malformed);
    }
    let expected_bytes =
        hex::decode_hmac_input(signature_hex).ok_or(TicketError::InvalidSignature)?;
    let mut mac = HmacSha256::new_from_slice(key).map_err(|_| TicketError::InvalidSignature)?;
    mac.update(message.as_bytes());
    mac.verify_slice(&expected_bytes)
        .map_err(|_| TicketError::InvalidSignature)
}

/// Inline hex decoder — avoids pulling in a `hex` crate just for this.
mod hex {
    /// Decodes a 64-char lowercase or uppercase hex string into a fixed 32-byte array.
    /// Returns `None` on any invalid input (wrong length is already checked by caller).
    pub fn decode_hmac_input(s: &str) -> Option<[u8; 32]> {
        debug_assert_eq!(s.len(), 64, "caller must pre-check length");
        let mut out = [0u8; 32];
        for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
            let hi = nibble(chunk[0])?;
            let lo = nibble(chunk[1])?;
            out[i] = (hi << 4) | lo;
        }
        Some(out)
    }

    fn nibble(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }

    pub fn is_valid_hex(s: &str) -> bool {
        s.as_bytes().iter().all(|b| nibble(*b).is_some())
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Allow expect/unwrap in tests — failures should panic visibly.

    #[test]
    fn test_issue_verify_roundtrip() {
        let signer = TicketSigner::new();
        let path = "/etc/ssh/sshd_config";
        let sha = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let ticket = signer.issue(path, sha, DEFAULT_TICKET_TTL_SECS);
        assert!(
            ticket.starts_with("rt2."),
            "ticket must start with version prefix"
        );
        assert_eq!(
            ticket.split('.').count(),
            4,
            "ticket must have 4 dot-separated parts"
        );
        let claims = signer
            .verify(&ticket, path)
            .expect("roundtrip must succeed");
        assert_eq!(claims.content_sha256(), Some(sha));
    }

    #[test]
    fn test_wrong_path_fails() {
        let signer = TicketSigner::new();
        let ticket = signer.issue(
            "/etc/passwd",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            DEFAULT_TICKET_TTL_SECS,
        );
        let err = signer.verify(&ticket, "/etc/shadow").unwrap_err();
        assert_eq!(err, TicketError::InvalidSignature);
    }

    #[test]
    fn test_expired_ticket() {
        let signer = TicketSigner::new();
        // TTL=0 produces exp == now, which is immediately ≤ now on next check.
        let ticket = signer.issue(
            "/tmp/file",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            0,
        );
        // Give the clock at least a moment to tick past exp.
        std::thread::sleep(std::time::Duration::from_millis(10));
        let err = signer.verify(&ticket, "/tmp/file").unwrap_err();
        assert_eq!(err, TicketError::Expired);
    }

    #[test]
    fn test_malformed_ticket() {
        let signer = TicketSigner::new();
        let future_exp = now_epoch_secs().saturating_add(DEFAULT_TICKET_TTL_SECS);
        let bad_tickets = [
            String::new(),
            "rt1".to_string(),
            "rt1.abc".to_string(),
            "rt2.12345.aabb".to_string(),
            format!("rt2.{future_exp}.short.bad"),
            "notrt1.12345.aabb".to_string(),
        ];
        for bad in &bad_tickets {
            let err = signer.verify(bad, "/any/path").unwrap_err();
            assert!(
                matches!(err, TicketError::Malformed | TicketError::InvalidSignature),
                "expected Malformed or InvalidSignature for {bad:?}, got {err:?}"
            );
        }
    }

    #[test]
    fn test_tampered_signature() {
        let signer = TicketSigner::new();
        let path = "/home/user/.ssh/authorized_keys";
        let ticket = signer.issue(
            path,
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            DEFAULT_TICKET_TTL_SECS,
        );

        // Flip the last hex character of the HMAC.
        let mut parts: Vec<&str> = ticket.split('.').collect();
        let sig = parts[3];
        let mut sig_bytes = sig.as_bytes().to_vec();
        let last = sig_bytes.len() - 1;
        sig_bytes[last] = if sig_bytes[last] == b'0' { b'1' } else { b'0' };
        let bad_sig = String::from_utf8(sig_bytes).expect("ascii only");
        parts[3] = Box::leak(bad_sig.into_boxed_str());
        let tampered = parts.join(".");

        let err = signer.verify(&tampered, path).unwrap_err();
        assert_eq!(err, TicketError::InvalidSignature);
    }

    #[test]
    fn test_legacy_rt1_verify_still_works_without_hash_claim() {
        let signer = TicketSigner::new();
        let path = "/tmp/legacy.txt";
        let exp = now_epoch_secs().saturating_add(DEFAULT_TICKET_TTL_SECS);
        let mac_hex = compute_hmac_hex(&hmac_message_v1(path, exp), &signer.key);
        let ticket = format!("rt1.{exp}.{mac_hex}");

        let claims = signer
            .verify(&ticket, path)
            .expect("legacy rt1 must verify");
        assert_eq!(claims.content_sha256(), None);
    }

    #[test]
    fn test_display_messages() {
        assert_eq!(
            TicketError::Malformed.to_string(),
            "read ticket is malformed"
        );
        assert_eq!(TicketError::Expired.to_string(), "read ticket has expired");
        assert_eq!(
            TicketError::InvalidSignature.to_string(),
            "read ticket has an invalid signature"
        );
    }

    #[test]
    fn test_default_ttl_constant() {
        assert_eq!(DEFAULT_TICKET_TTL_SECS, 600);
    }
}
