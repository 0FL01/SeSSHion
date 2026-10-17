//! Bounded SSH identification exchange on the stream that russh will use.
//!
//! russh 0.61.2 fixes keepalive policy when connecting, before its public
//! callbacks expose the peer identification. Exchange just that identification
//! here, then replay it to russh without sending the client identification twice.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use russh::SshId;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};

use crate::error::{Result, SshMcpError};

// Match russh's line/preamble limits, including the line terminator.
const MAX_LINE_BYTES: usize = 255;
const MAX_PREAMBLE_LINES: usize = 20;
const READ_BYTES: usize = 256;

/// Compatibility selected from an authenticated target's SSH software token.
/// POSIX is the legacy backend, not a claim about the target's operating system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemotePlatform {
    Posix,
    Routeros,
}

impl RemotePlatform {
    pub(crate) fn validate_elevation(
        self,
        su_password: Option<&str>,
        sudo_password: Option<&str>,
    ) -> Result<()> {
        if self == Self::Routeros
            && [su_password, sudo_password]
                .into_iter()
                .flatten()
                .any(|password| !password.is_empty())
        {
            return Err(SshMcpError::config(
                "RouterOS does not support su_password or sudo_password",
            ));
        }
        Ok(())
    }
}

impl std::fmt::Display for RemotePlatform {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Posix => "posix",
            Self::Routeros => "routeros",
        })
    }
}

pub(super) struct IdentificationStream<S> {
    inner: S,
    replay: Vec<u8>,
    replay_offset: usize,
    client_id: Vec<u8>,
    client_id_offset: usize,
}

/// Send the configured ID once and retain every byte read, including any KEX
/// read-ahead. The caller's existing connection deadline bounds all I/O here.
pub(super) async fn identify<S>(
    mut stream: S,
    client_id: &SshId,
) -> io::Result<(IdentificationStream<S>, RemotePlatform)>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // Do not replace config.client_id: russh also needs it for the KEX hash.
    let client_id = match client_id {
        SshId::Standard(value) => format!("{value}\r\n").into_bytes(),
        SshId::Raw(value) => value.as_bytes().to_vec(),
    };
    stream.write_all(&client_id).await?;
    stream.flush().await?;

    let mut replay = Vec::new();
    let mut line_start = 0;
    let mut preamble_lines = 0;
    let mut buffer = [0; READ_BYTES];
    loop {
        // Retain at most russh's 256-byte look-ahead for the current line,
        // plus the bounded preamble already retained for replay.
        let available = READ_BYTES - (replay.len() - line_start);
        let count = stream.read(&mut buffer[..available]).await?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "SSH identification ended before a valid server ID",
            ));
        }
        replay.extend_from_slice(&buffer[..count]);

        loop {
            let pending = &replay[line_start..];
            // Like russh's parser, a terminator needs at least two bytes;
            // CRLF and nonempty LF-terminated lines are both accepted.
            let Some(newline) = pending
                .iter()
                .enumerate()
                .skip(1)
                .find_map(|(index, byte)| (*byte == b'\n').then_some(index))
            else {
                if pending.len() >= MAX_LINE_BYTES {
                    return Err(invalid_identification("line exceeds 255 bytes"));
                }
                break;
            };
            if newline + 1 > MAX_LINE_BYTES {
                return Err(invalid_identification("line exceeds 255 bytes"));
            }
            let line = &pending[..newline];
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if let Some(platform) = classify(line) {
                return Ok((
                    IdentificationStream {
                        inner: stream,
                        replay,
                        replay_offset: 0,
                        client_id,
                        client_id_offset: 0,
                    },
                    platform,
                ));
            }
            preamble_lines += 1;
            if preamble_lines > MAX_PREAMBLE_LINES {
                return Err(invalid_identification("more than 20 preamble lines"));
            }
            line_start += newline + 1;
        }
    }
}

fn invalid_identification(reason: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("Invalid SSH identification: {reason}"),
    )
}

fn classify(line: &[u8]) -> Option<RemotePlatform> {
    // Retain russh's protocol/UTF-8 leniency. Non-identification lines are
    // bounded preambles, never evidence for a POSIX fallback.
    let line = std::str::from_utf8(line).ok()?;
    let software = line
        .strip_prefix("SSH-2.0-")
        .or_else(|| line.strip_prefix("SSH-1.99-"))?;
    let token = software.split(' ').next().unwrap_or_default();
    Some(if token == "ROSSSH" {
        RemotePlatform::Routeros
    } else {
        RemotePlatform::Posix
    })
}

impl<S: AsyncRead + Unpin> AsyncRead for IdentificationStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.replay_offset < this.replay.len() {
            let end = this
                .replay
                .len()
                .min(this.replay_offset + buffer.remaining());
            buffer.put_slice(&this.replay[this.replay_offset..end]);
            this.replay_offset = end;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_read(context, buffer)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for IdentificationStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.client_id_offset < this.client_id.len() {
            let count = buffer
                .len()
                .min(this.client_id.len() - this.client_id_offset);
            if buffer[..count]
                != this.client_id[this.client_id_offset..this.client_id_offset + count]
            {
                return Poll::Ready(Err(invalid_identification(
                    "russh client identification did not match the exchanged ID",
                )));
            }
            this.client_id_offset += count;
            return Poll::Ready(Ok(count));
        }
        Pin::new(&mut this.inner).poll_write(context, buffer)
    }

    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(context)
    }

    fn poll_shutdown(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(context)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::io::DuplexStream;
    use tokio::time::timeout;

    use super::*;

    const LOCAL: &[u8] = b"SSH-2.0-identification-test\r\n";

    async fn identify_bytes(
        bytes: Vec<u8>,
        fragmented: bool,
    ) -> io::Result<(IdentificationStream<DuplexStream>, RemotePlatform)> {
        let (client, mut peer) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            let mut id = vec![0; LOCAL.len()];
            peer.read_exact(&mut id).await.unwrap();
            assert_eq!(id, LOCAL);
            if fragmented {
                for byte in bytes {
                    if peer.write_all(&[byte]).await.is_err() {
                        return;
                    }
                    tokio::task::yield_now().await;
                }
            } else {
                let _ = peer.write_all(&bytes).await;
            }
            let _ = peer.shutdown().await;
        });
        identify(
            client,
            &SshId::Standard("SSH-2.0-identification-test".into()),
        )
        .await
    }

    #[test]
    fn software_token_not_comments_or_substrings_selects_routeros() {
        for (line, expected) in [
            ("SSH-2.0-ROSSSH", Some(RemotePlatform::Routeros)),
            ("SSH-1.99-ROSSSH", Some(RemotePlatform::Routeros)),
            ("SSH-2.0-ROSSSH comments", Some(RemotePlatform::Routeros)),
            ("SSH-2.0-OpenSSH_10 ROSSSH", Some(RemotePlatform::Posix)),
            ("SSH-2.0-other ROSSSH more", Some(RemotePlatform::Posix)),
            ("SSH-2.0-ROSSSH_7", Some(RemotePlatform::Posix)),
            ("SSH-2.0-XROSSSH", Some(RemotePlatform::Posix)),
            ("SSH-2.0-rosssh", Some(RemotePlatform::Posix)),
            ("SSH-2.0-ROSSSH\tcomment", Some(RemotePlatform::Posix)),
            ("SSH-2.0-", Some(RemotePlatform::Posix)),
            ("SSH-3.0-ROSSSH", None),
            ("prelude ROSSSH", None),
        ] {
            assert_eq!(classify(line.as_bytes()), expected, "{line:?}");
        }
        assert_eq!(classify(b"SSH-2.0-ROSSSH\xff"), None);
    }

    #[tokio::test]
    async fn fragmented_preambles_lf_crlf_and_kex_read_ahead_replay_exactly() {
        for (id, platform) in [
            ("SSH-2.0-ROSSSH\r\n", RemotePlatform::Routeros),
            ("SSH-1.99-ROSSSH\n", RemotePlatform::Routeros),
            ("SSH-2.0-OpenSSH_10\n", RemotePlatform::Posix),
        ] {
            for fragmented in [false, true] {
                let mut bytes = format!("notice\nsecond notice\r\n{id}").into_bytes();
                bytes.extend_from_slice(b"\0\0\0\x10\xffbinary KEX\r\n");
                let (mut stream, actual) = identify_bytes(bytes.clone(), fragmented).await.unwrap();
                assert_eq!(actual, platform);
                let mut replay = Vec::new();
                stream.read_to_end(&mut replay).await.unwrap();
                assert_eq!(replay, bytes);
            }
        }
    }

    #[tokio::test]
    async fn preamble_and_line_limits_match_russh() {
        for fragmented in [false, true] {
            let bytes = format!("{}SSH-2.0-ROSSSH\r\n", "notice\r\n".repeat(20));
            let (stream, platform) = identify_bytes(bytes.into_bytes(), fragmented)
                .await
                .unwrap();
            assert_eq!(platform, RemotePlatform::Routeros);
            assert!(stream.replay.len() <= 20 * MAX_LINE_BYTES + READ_BYTES);

            let maximum_id = format!("SSH-2.0-{}\r\n", "a".repeat(245));
            assert_eq!(maximum_id.len(), MAX_LINE_BYTES);
            let (_, platform) = identify_bytes(maximum_id.into_bytes(), fragmented)
                .await
                .unwrap();
            assert_eq!(platform, RemotePlatform::Posix);

            let maximum_preamble = format!("{}\nSSH-2.0-ROSSSH\n", "x".repeat(254));
            assert!(
                identify_bytes(maximum_preamble.into_bytes(), fragmented)
                    .await
                    .is_ok()
            );

            for bytes in [
                format!("{}SSH-2.0-ROSSSH\r\n", "notice\r\n".repeat(21)).into_bytes(),
                format!("SSH-2.0-{}\r\n", "a".repeat(246)).into_bytes(),
                vec![b'x'; MAX_LINE_BYTES],
                format!("{}\nSSH-2.0-ROSSSH\n", "x".repeat(255)).into_bytes(),
            ] {
                let error = identify_bytes(bytes, fragmented).await.err().unwrap();
                assert_eq!(error.kind(), io::ErrorKind::InvalidData);
                assert!(error.to_string().contains("SSH identification"));
            }
        }

        let bytes = format!(
            "{}SSH-2.0-{}\r\n{}",
            format!("{}\r\n", "p".repeat(253)).repeat(20),
            "a".repeat(245),
            "K".repeat(256),
        )
        .into_bytes();
        let (mut stream, _) = identify_bytes(bytes.clone(), false).await.unwrap();
        assert!(stream.replay.len() <= 20 * MAX_LINE_BYTES + READ_BYTES);
        let mut replay = Vec::new();
        stream.read_to_end(&mut replay).await.unwrap();
        assert_eq!(replay, bytes);
    }

    #[tokio::test]
    async fn absent_invalid_or_incomplete_ids_never_fall_back() {
        for bytes in [
            Vec::new(),
            b"SSH-2.0-ROSSSH".to_vec(),
            b"SSH-3.0-ROSSSH\r\n".to_vec(),
            b"not an SSH identification\r\n".to_vec(),
            b"SSH-2.0-\xff\r\n".to_vec(),
        ] {
            let error = identify_bytes(bytes, false).await.err().unwrap();
            assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        }
    }

    #[tokio::test]
    async fn client_id_is_sent_once_even_when_peer_waits_and_russh_writes_fragments() {
        for (id, wire) in [
            (
                SshId::Standard("SSH-2.0-custom-standard".into()),
                b"SSH-2.0-custom-standard\r\n".as_slice(),
            ),
            (
                SshId::Raw("SSH-2.0-custom-raw\n".into()),
                b"SSH-2.0-custom-raw\n".as_slice(),
            ),
        ] {
            let (client, mut peer) = tokio::io::duplex(1024);
            let peer_task = tokio::spawn(async move {
                let mut received_id = vec![0; wire.len()];
                peer.read_exact(&mut received_id).await.unwrap();
                assert_eq!(received_id, wire);
                // Deliberately withhold our ID until the configured client ID arrives.
                peer.write_all(b"SSH-2.0-ROSSSH\r\nKEX from peer")
                    .await
                    .unwrap();
                peer.shutdown().await.unwrap();
                let mut later = Vec::new();
                peer.read_to_end(&mut later).await.unwrap();
                later
            });
            let (mut stream, platform) = timeout(Duration::from_secs(1), identify(client, &id))
                .await
                .expect("a peer waiting for the client ID must not deadlock")
                .unwrap();
            assert_eq!(platform, RemotePlatform::Routeros);
            assert!(stream.replay.ends_with(b"KEX from peer"));
            stream.write_all(&wire[..3]).await.unwrap();
            stream.write_all(&wire[3..wire.len() - 2]).await.unwrap();
            // A write can contain both the tail of the duplicate and actual KEX.
            let mut tail = wire[wire.len() - 2..].to_vec();
            tail.extend_from_slice(b"KEX from client");
            stream.write_all(&tail).await.unwrap();
            stream.flush().await.unwrap();
            stream.shutdown().await.unwrap();
            let mut replay = Vec::new();
            stream.read_to_end(&mut replay).await.unwrap();
            assert_eq!(replay, b"SSH-2.0-ROSSSH\r\nKEX from peer");
            assert_eq!(peer_task.await.unwrap(), b"KEX from client");
        }
    }

    #[tokio::test]
    async fn mismatched_duplicate_is_a_typed_error_not_forwarded() {
        let (mut stream, _) = identify_bytes(b"SSH-2.0-ROSSSH\n".to_vec(), false)
            .await
            .unwrap();
        stream.write_all(&LOCAL[..5]).await.unwrap();
        let error = stream.write_all(b"different").await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("did not match"));
    }

    #[tokio::test]
    async fn caller_deadline_bounds_a_stalled_identification() {
        let (client, mut peer) = tokio::io::duplex(1024);
        let peer_task = tokio::spawn(async move {
            let mut id = vec![0; LOCAL.len()];
            peer.read_exact(&mut id).await.unwrap();
            peer.write_all(b"SSH-2.0-").await.unwrap();
            let mut remaining = Vec::new();
            peer.read_to_end(&mut remaining).await.unwrap();
            assert!(remaining.is_empty());
        });
        assert!(
            timeout(
                Duration::from_millis(30),
                identify(
                    client,
                    &SshId::Standard("SSH-2.0-identification-test".into())
                ),
            )
            .await
            .is_err()
        );
        peer_task.await.unwrap();
    }

    #[tokio::test]
    async fn configured_raw_client_id_is_preserved_for_the_real_kex_hash() {
        struct Client;
        impl russh::client::Handler for Client {
            type Error = russh::Error;

            async fn check_server_key(
                &mut self,
                _: &russh::keys::PublicKey,
            ) -> std::result::Result<bool, Self::Error> {
                Ok(true)
            }
        }
        struct Peer;
        impl russh::server::Handler for Peer {
            type Error = russh::Error;

            async fn auth_password(
                &mut self,
                _: &str,
                _: &str,
            ) -> std::result::Result<russh::server::Auth, Self::Error> {
                Ok(russh::server::Auth::Accept)
            }
        }

        let key = russh::keys::PrivateKey::new(
            russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&[44; 32]).into(),
            "identification-test",
        )
        .unwrap();
        let server_config = Arc::new(russh::server::Config {
            server_id: SshId::Standard("SSH-2.0-ROSSSH".into()),
            keys: vec![key],
            ..Default::default()
        });
        let (client, server) = tokio::io::duplex(64 * 1024);
        let peer_task = tokio::spawn(async move {
            let running = russh::server::run_stream(server_config, server, Peer)
                .await
                .unwrap();
            let _ = running.await;
        });
        let config = Arc::new(russh::client::Config {
            client_id: SshId::Raw("SSH-2.0-custom-kex-id\n".into()),
            ..Default::default()
        });
        let (stream, platform) = identify(client, &config.client_id).await.unwrap();
        assert_eq!(platform, RemotePlatform::Routeros);
        let mut handle = russh::client::connect_stream(config, stream, Client)
            .await
            .unwrap();
        assert!(
            handle
                .authenticate_password("test", "test")
                .await
                .unwrap()
                .success()
        );
        handle
            .disconnect(russh::Disconnect::ByApplication, "", "")
            .await
            .unwrap();
        let _ = handle.await;
        peer_task.await.unwrap();
    }

    #[test]
    fn routeros_elevation_validation_uses_exact_empty_semantics() {
        for (su, sudo) in [(None, None), (Some(""), None), (None, Some(""))] {
            RemotePlatform::Routeros
                .validate_elevation(su, sudo)
                .unwrap();
        }
        for (su, sudo) in [(Some("su"), None), (None, Some("sudo")), (Some(" "), None)] {
            assert!(matches!(
                RemotePlatform::Routeros.validate_elevation(su, sudo),
                Err(SshMcpError::Config(_))
            ));
            RemotePlatform::Posix.validate_elevation(su, sudo).unwrap();
        }
    }
}
