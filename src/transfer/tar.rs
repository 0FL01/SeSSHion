use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use tokio::fs;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::{Result, SshMcpError};

const TAR_BLOCK: usize = 512;

#[derive(Debug, Clone, Default)]
pub struct TarCounts {
    pub bytes: u64,
    pub files: u64,
    pub directories: u64,
}

pub async fn write_dir_as_tar<W: AsyncWrite + Unpin>(root: &Path, mut out: W) -> Result<TarCounts> {
    let meta = fs::symlink_metadata(root).await?;
    if !meta.is_dir() {
        return Err(SshMcpError::invalid_params("local_path is not a directory"));
    }

    let mut counts = TarCounts::default();

    #[derive(Debug, Clone)]
    struct EntryInfo {
        path: PathBuf,
        name: String,
    }

    #[derive(Debug)]
    struct Frame {
        entries: Vec<EntryInfo>,
        prefix: String,
        idx: usize,
    }

    async fn read_entries(dir: &Path) -> Result<Vec<EntryInfo>> {
        let mut out = Vec::new();
        let mut rd = fs::read_dir(dir).await?;
        while let Some(entry) = rd.next_entry().await? {
            let name = entry.file_name().to_string_lossy().to_string();
            out.push(EntryInfo {
                path: entry.path(),
                name,
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    let mut stack = Vec::new();
    stack.push(Frame {
        entries: read_entries(root).await?,
        prefix: String::new(),
        idx: 0,
    });

    while let Some(frame) = stack.last_mut() {
        if frame.idx >= frame.entries.len() {
            stack.pop();
            continue;
        }

        let item = frame.entries[frame.idx].clone();
        frame.idx += 1;

        let archive_path = if frame.prefix.is_empty() {
            item.name.clone()
        } else {
            format!("{}/{}", frame.prefix, item.name)
        };

        let meta = fs::symlink_metadata(&item.path).await?;
        if meta.file_type().is_symlink() {
            return Err(SshMcpError::invalid_params(
                "symlinks are not supported by transfer tar in this iteration",
            ));
        }

        if meta.is_dir() {
            write_header(
                &mut out,
                &format!("{}/", archive_path),
                0,
                meta.permissions().perm_mode(),
                meta_modified_secs(&meta)?,
                b'5',
            )
            .await?;
            counts.directories += 1;

            stack.push(Frame {
                entries: read_entries(&item.path).await?,
                prefix: archive_path,
                idx: 0,
            });
            continue;
        }

        if meta.is_file() {
            let size = meta.len();
            write_header(
                &mut out,
                &archive_path,
                size,
                meta.permissions().perm_mode(),
                meta_modified_secs(&meta)?,
                b'0',
            )
            .await?;
            counts.files += 1;

            let mut f = fs::File::open(&item.path).await?;
            copy_exact_len(&mut f, &mut out, size, &mut counts.bytes).await?;
            write_padding(&mut out, size).await?;
            continue;
        }

        return Err(SshMcpError::invalid_params(
            "unsupported file type in directory transfer",
        ));
    }

    // End of archive: two zero blocks.
    out.write_all(&[0u8; TAR_BLOCK]).await?;
    out.write_all(&[0u8; TAR_BLOCK]).await?;
    out.flush().await?;
    Ok(counts)
}

#[cfg(unix)]
trait PermMode {
    fn perm_mode(&self) -> u32;
}

#[cfg(unix)]
impl PermMode for std::fs::Permissions {
    fn perm_mode(&self) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        self.mode()
    }
}

#[cfg(not(unix))]
trait PermMode {
    fn perm_mode(&self) -> u32;
}

#[cfg(not(unix))]
impl PermMode for std::fs::Permissions {
    fn perm_mode(&self) -> u32 {
        0o644
    }
}

fn meta_modified_secs(meta: &std::fs::Metadata) -> Result<u64> {
    match meta.modified() {
        Ok(t) => match t.duration_since(UNIX_EPOCH) {
            Ok(d) => Ok(d.as_secs()),
            Err(_) => Ok(0),
        },
        Err(_) => Ok(0),
    }
}

async fn write_header<W: AsyncWrite + Unpin>(
    out: &mut W,
    path: &str,
    size: u64,
    mode: u32,
    mtime: u64,
    typeflag: u8,
) -> Result<()> {
    let (name, prefix) = split_ustar_path(path).map_err(SshMcpError::invalid_params)?;
    let mut header = [0u8; TAR_BLOCK];

    write_bytes(&mut header[0..100], name.as_bytes());
    write_octal(&mut header[100..108], mode as u64, 7);
    write_octal(&mut header[108..116], 0, 7); // uid
    write_octal(&mut header[116..124], 0, 7); // gid
    write_octal(&mut header[124..136], size, 11);
    write_octal(&mut header[136..148], mtime, 11);

    // checksum field: initially spaces
    for b in &mut header[148..156] {
        *b = b' ';
    }

    header[156] = typeflag;

    // magic + version
    write_bytes(&mut header[257..263], b"ustar\0");
    write_bytes(&mut header[263..265], b"00");

    if let Some(prefix_str) = prefix {
        write_bytes(&mut header[345..500], prefix_str.as_bytes());
    }

    let checksum = header.iter().map(|b| *b as u32).sum::<u32>();
    write_octal_checksum(&mut header[148..156], checksum as u64);

    out.write_all(&header).await?;
    Ok(())
}

fn write_bytes(dst: &mut [u8], src: &[u8]) {
    let len = dst.len().min(src.len());
    dst[..len].copy_from_slice(&src[..len]);
}

fn write_octal(dst: &mut [u8], value: u64, width_digits: usize) {
    // dst includes final NUL.
    // width_digits: number of octal digits (excluding NUL).
    let mut buf = vec![b'0'; width_digits];
    let mut v = value;
    for i in (0..width_digits).rev() {
        buf[i] = b'0' + ((v & 0o7) as u8);
        v >>= 3;
        if v == 0 {
            break;
        }
    }

    let write_len = dst.len().min(width_digits);
    dst[..write_len].copy_from_slice(&buf[width_digits - write_len..]);
    if dst.len() > width_digits {
        dst[width_digits] = 0;
    }
}

fn write_octal_checksum(dst: &mut [u8], value: u64) {
    // Standard: 6 digits, NUL, space.
    let mut buf = [b'0'; 8];
    let mut v = value;
    for i in (0..6).rev() {
        buf[i] = b'0' + ((v & 0o7) as u8);
        v >>= 3;
    }
    buf[6] = 0;
    buf[7] = b' ';
    dst.copy_from_slice(&buf);
}

fn split_ustar_path(path: &str) -> std::result::Result<(String, Option<String>), String> {
    if path.len() <= 100 {
        return Ok((path.to_string(), None));
    }

    // Try to split into prefix (<=155) and name (<=100).
    if let Some(pos) = path.rfind('/') {
        let (pfx, name) = path.split_at(pos);
        let name = &name[1..];
        if name.len() <= 100 && pfx.len() <= 155 {
            return Ok((name.to_string(), Some(pfx.to_string())));
        }
    }

    Err("path too long for ustar header".to_string())
}

async fn copy_exact_len<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    input: &mut R,
    out: &mut W,
    mut remaining: u64,
    bytes_counter: &mut u64,
) -> Result<()> {
    let mut buf = vec![0u8; 32 * 1024];
    while remaining > 0 {
        let want = remaining.min(buf.len() as u64) as usize;
        let n = input.read(&mut buf[..want]).await?;
        if n == 0 {
            return Err(SshMcpError::connection(
                "unexpected EOF while reading local file",
            ));
        }
        out.write_all(&buf[..n]).await?;
        *bytes_counter += n as u64;
        remaining -= n as u64;
    }
    Ok(())
}

async fn write_padding<W: AsyncWrite + Unpin>(out: &mut W, size: u64) -> Result<()> {
    let pad = (TAR_BLOCK as u64 - (size % TAR_BLOCK as u64)) % TAR_BLOCK as u64;
    if pad == 0 {
        return Ok(());
    }
    let zeros = vec![0u8; pad as usize];
    out.write_all(&zeros).await?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct ExtractCounts {
    pub bytes: u64,
    pub files: u64,
    pub directories: u64,
}

pub async fn extract_tar_to_dir<R: AsyncRead + Unpin>(
    mut input: R,
    root: &Path,
) -> Result<ExtractCounts> {
    let mut counts = ExtractCounts {
        bytes: 0,
        files: 0,
        directories: 0,
    };

    loop {
        let mut header = [0u8; TAR_BLOCK];
        input
            .read_exact(&mut header)
            .await
            .map_err(|e| SshMcpError::connection(format!("failed to read tar header: {e}")))?;

        if header.iter().all(|b| *b == 0) {
            // read the second zero block and finish
            let mut second = [0u8; TAR_BLOCK];
            input
                .read_exact(&mut second)
                .await
                .map_err(|e| SshMcpError::connection(format!("failed to read tar trailer: {e}")))?;
            break;
        }

        let entry = TarEntryHeader::parse(&header)?;
        let rel = sanitize_tar_path(&entry.path).map_err(SshMcpError::invalid_params)?;
        let dest = root.join(rel);

        match entry.typeflag {
            b'5' => {
                if entry.size != 0 {
                    // Keep the stream aligned before reporting an error. Otherwise the
                    // underlying producer may block on a full pipe.
                    discard_stream_len(&mut input, entry.size).await?;
                    skip_padding(&mut input, entry.size).await?;
                    return Err(SshMcpError::invalid_params(
                        "tar directory entry must have size 0",
                    ));
                }
                fs::create_dir_all(&dest).await?;
                counts.directories += 1;
                // For completeness (size is expected to be 0).
                skip_padding(&mut input, entry.size).await?;
            }
            b'0' | 0 => {
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent).await?;
                }
                let mut f = fs::File::create(&dest).await?;
                copy_stream_len(&mut input, &mut f, entry.size, &mut counts.bytes).await?;
                f.flush().await?;
                counts.files += 1;
                skip_padding(&mut input, entry.size).await?;
            }
            _ => {
                // Unknown entry type: drain to keep stream aligned then report an error.
                discard_stream_len(&mut input, entry.size).await?;
                skip_padding(&mut input, entry.size).await?;
                return Err(SshMcpError::invalid_params(
                    "unsupported tar entry type in this iteration",
                ));
            }
        }
    }

    Ok(counts)
}

async fn copy_stream_len<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    input: &mut R,
    out: &mut W,
    mut remaining: u64,
    bytes_counter: &mut u64,
) -> Result<()> {
    let mut buf = vec![0u8; 32 * 1024];
    while remaining > 0 {
        let want = remaining.min(buf.len() as u64) as usize;
        let n = input
            .read(&mut buf[..want])
            .await
            .map_err(|e| SshMcpError::connection(format!("failed to read tar stream: {e}")))?;
        if n == 0 {
            return Err(SshMcpError::connection(
                "unexpected EOF while reading tar stream",
            ));
        }
        out.write_all(&buf[..n]).await?;
        *bytes_counter += n as u64;
        remaining -= n as u64;
    }
    Ok(())
}

async fn discard_stream_len<R: AsyncRead + Unpin>(input: &mut R, mut remaining: u64) -> Result<()> {
    let mut buf = vec![0u8; 32 * 1024];
    while remaining > 0 {
        let want = remaining.min(buf.len() as u64) as usize;
        let n = input
            .read(&mut buf[..want])
            .await
            .map_err(|e| SshMcpError::connection(format!("failed to read tar stream: {e}")))?;
        if n == 0 {
            return Err(SshMcpError::connection(
                "unexpected EOF while reading tar stream",
            ));
        }
        remaining -= n as u64;
    }
    Ok(())
}

async fn skip_padding<R: AsyncRead + Unpin>(input: &mut R, size: u64) -> Result<()> {
    let pad = (TAR_BLOCK as u64 - (size % TAR_BLOCK as u64)) % TAR_BLOCK as u64;
    if pad == 0 {
        return Ok(());
    }
    let mut buf = vec![0u8; pad as usize];
    input
        .read_exact(&mut buf)
        .await
        .map_err(|e| SshMcpError::connection(format!("failed to read tar padding: {e}")))?;
    Ok(())
}

#[derive(Debug, Clone)]
struct TarEntryHeader {
    path: String,
    size: u64,
    typeflag: u8,
}

impl TarEntryHeader {
    fn parse(block: &[u8; TAR_BLOCK]) -> Result<Self> {
        validate_ustar_header(block)?;

        let name = parse_string(&block[0..100]);
        let prefix = parse_string(&block[345..500]);
        let path = if !prefix.is_empty() {
            format!("{}/{}", prefix, name)
        } else {
            name
        };

        let size = parse_octal(&block[124..136]).map_err(SshMcpError::invalid_params)?;
        let typeflag = block[156];

        Ok(Self {
            path,
            size,
            typeflag,
        })
    }
}

fn validate_ustar_header(block: &[u8; TAR_BLOCK]) -> Result<()> {
    let magic = &block[257..263];
    let version = &block[263..265];
    if magic != b"ustar\0" || version != b"00" {
        return Err(SshMcpError::invalid_params(
            "tar header is not a valid ustar header",
        ));
    }

    let stored = parse_octal(&block[148..156]).map_err(SshMcpError::invalid_params)?;
    let computed = compute_ustar_checksum(block);
    if stored != computed {
        return Err(SshMcpError::invalid_params(format!(
            "invalid tar header checksum (expected {computed}, got {stored})",
        )));
    }

    Ok(())
}

fn compute_ustar_checksum(block: &[u8; TAR_BLOCK]) -> u64 {
    // Checksum is the sum of all bytes in the header, treating the checksum field as spaces.
    let mut sum: u64 = 0;
    for (idx, b) in block.iter().enumerate() {
        if (148..156).contains(&idx) {
            sum += u64::from(b' ');
        } else {
            sum += u64::from(*b);
        }
    }
    sum
}

fn parse_string(field: &[u8]) -> String {
    let end = field.iter().position(|b| *b == 0).unwrap_or(field.len());
    let s = &field[..end];
    let s = s
        .iter()
        .take_while(|b| **b != 0)
        .copied()
        .collect::<Vec<u8>>();
    // Do not trim: leading/trailing spaces are valid tar path bytes.
    // Only NUL termination is handled via the slice end above.
    String::from_utf8_lossy(&s).to_string()
}

fn parse_octal(field: &[u8]) -> std::result::Result<u64, String> {
    let s = String::from_utf8_lossy(field)
        .trim_matches(['\0', ' '])
        .to_string();
    if s.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(&s, 8).map_err(|e| format!("invalid octal field '{s}': {e}"))
}

fn sanitize_tar_path(path: &str) -> std::result::Result<PathBuf, String> {
    // Do not trim: tar entry paths may contain leading/trailing spaces.
    let mut p = path;
    while let Some(rest) = p.strip_prefix("./") {
        p = rest;
    }
    if p.is_empty() {
        return Err("tar entry path is empty".to_string());
    }
    let candidate = Path::new(p);
    let mut out = PathBuf::new();
    for comp in candidate.components() {
        match comp {
            Component::Normal(seg) => out.push(seg),
            Component::CurDir => {}
            Component::ParentDir => return Err("tar entry contains '..'".to_string()),
            Component::RootDir | Component::Prefix(_) => {
                return Err("tar entry path must be relative".to_string());
            }
        }
    }

    if out.as_os_str().is_empty() {
        return Err("tar entry path must not normalize to '.'".to_string());
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_ustar_path_short() {
        let (name, prefix) = split_ustar_path("a/b.txt").unwrap();
        assert_eq!(name, "a/b.txt");
        assert!(prefix.is_none());
    }

    #[test]
    fn sanitize_tar_path_rejects_parent() {
        assert!(sanitize_tar_path("../x").is_err());
        assert!(sanitize_tar_path("a/../../x").is_err());
    }

    #[test]
    fn sanitize_tar_path_rejects_empty_and_dot() {
        assert!(sanitize_tar_path("").is_err());
        assert!(sanitize_tar_path(".").is_err());
        assert!(sanitize_tar_path("./").is_err());
        assert!(sanitize_tar_path("././").is_err());
    }

    #[test]
    fn sanitize_tar_path_allows_spaces() {
        let p = sanitize_tar_path("   ").unwrap();
        assert_eq!(p, PathBuf::from("   "));
        let p2 = sanitize_tar_path(" a /b ").unwrap();
        assert_eq!(p2, PathBuf::from(" a /b "));
    }

    #[test]
    fn parse_string_does_not_trim_spaces() {
        let field = [b'a', b' ', b'b', 0, 0, 0];
        assert_eq!(parse_string(&field), "a b");
    }
}
