//! Frame codec: `u32 BE length | flags | body`, optional zstd body, server preamble scanning.

use std::borrow::Cow;
use std::io;

use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::proto::MAGIC;

pub const MAX_FRAME: usize = 32 * 1024 * 1024;
/// Bytes a remote shell may print before the server's magic before we give up.
pub const MAX_NOISE: usize = 64 * 1024;
pub const COMPRESS_THRESHOLD: usize = 1024;
const FLAG_ZSTD: u8 = 0b0000_0001;
const ZSTD_LEVEL: i32 = 3;

#[derive(Debug)]
pub enum FrameError {
    Io(io::Error),
    TooLarge(usize),
    Empty,
    BadFlags(u8),
    Decompress(String),
    Truncated,
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::Io(err) => write!(f, "I/O error: {err}"),
            FrameError::TooLarge(n) => write!(f, "frame of {n} bytes exceeds {MAX_FRAME}"),
            FrameError::Empty => write!(f, "empty frame"),
            FrameError::BadFlags(flags) => write!(f, "unknown frame flags {flags:#04x}"),
            FrameError::Decompress(err) => write!(f, "cannot decompress frame: {err}"),
            FrameError::Truncated => write!(f, "stream ended inside a frame"),
        }
    }
}

impl std::error::Error for FrameError {}

#[derive(Debug)]
pub struct Frame {
    pub body: Vec<u8>,
    /// Bytes this frame occupied on the wire, including the header.
    pub wire_len: usize,
}

pub fn encode_frame(body: &[u8], allow_compress: bool) -> Vec<u8> {
    let (flags, payload): (u8, Cow<'_, [u8]>) = if allow_compress && body.len() > COMPRESS_THRESHOLD
    {
        match zstd::bulk::compress(body, ZSTD_LEVEL) {
            Ok(packed) if packed.len() < body.len() => (FLAG_ZSTD, Cow::Owned(packed)),
            _ => (0, Cow::Borrowed(body)),
        }
    } else {
        (0, Cow::Borrowed(body))
    };
    let n = payload.len() + 1;
    let mut out = Vec::with_capacity(4 + n);
    out.extend_from_slice(&(n as u32).to_be_bytes());
    out.push(flags);
    out.extend_from_slice(&payload);
    out
}

pub fn encode_msg<T: Serialize>(msg: &T, allow_compress: bool) -> Vec<u8> {
    let body = serde_json::to_vec(msg).expect("wire messages always serialize");
    encode_frame(&body, allow_compress)
}

/// Read one frame. `Ok(None)` means the stream closed cleanly between frames.
pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Option<Frame>, FrameError> {
    let mut len_buf = [0u8; 4];
    let first = reader
        .read(&mut len_buf[..1])
        .await
        .map_err(FrameError::Io)?;
    if first == 0 {
        return Ok(None);
    }
    read_exact(reader, &mut len_buf[1..]).await?;
    let n = u32::from_be_bytes(len_buf) as usize;
    if n == 0 {
        return Err(FrameError::Empty);
    }
    if n > MAX_FRAME {
        return Err(FrameError::TooLarge(n));
    }
    let mut buf = vec![0u8; n];
    read_exact(reader, &mut buf).await?;
    let flags = buf[0];
    if flags & !FLAG_ZSTD != 0 {
        return Err(FrameError::BadFlags(flags));
    }
    let body = if flags & FLAG_ZSTD != 0 {
        zstd::bulk::decompress(&buf[1..], MAX_FRAME)
            .map_err(|err| FrameError::Decompress(err.to_string()))?
    } else {
        buf.remove(0);
        buf
    };
    Ok(Some(Frame {
        body,
        wire_len: 4 + n,
    }))
}

async fn read_exact<R: AsyncRead + Unpin>(
    reader: &mut R,
    buf: &mut [u8],
) -> Result<(), FrameError> {
    reader.read_exact(buf).await.map(|_| ()).map_err(|err| {
        if err.kind() == io::ErrorKind::UnexpectedEof {
            FrameError::Truncated
        } else {
            FrameError::Io(err)
        }
    })
}

#[derive(Debug)]
pub enum PreambleError {
    /// The stream closed before the magic; carries whatever was printed.
    Eof {
        noise: Vec<u8>,
    },
    TooMuchNoise {
        noise: Vec<u8>,
    },
    Io(io::Error),
}

impl PreambleError {
    pub fn noise(&self) -> &[u8] {
        match self {
            PreambleError::Eof { noise } | PreambleError::TooMuchNoise { noise } => noise,
            PreambleError::Io(_) => &[],
        }
    }
}

impl std::fmt::Display for PreambleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PreambleError::Eof { noise } if noise.is_empty() => {
                write!(f, "connection closed before the server started")
            }
            PreambleError::Eof { noise } => write!(
                f,
                "connection closed before the server started; it printed: {}",
                preview(noise)
            ),
            PreambleError::TooMuchNoise { noise } => write!(
                f,
                "no sessift server found in {} bytes of output: {}",
                noise.len(),
                preview(noise)
            ),
            PreambleError::Io(err) => write!(f, "I/O error: {err}"),
        }
    }
}

impl std::error::Error for PreambleError {}

/// Skip everything up to and including the server magic. Use a buffered reader:
/// this reads one byte at a time.
pub async fn read_preamble<R: AsyncRead + Unpin>(reader: &mut R) -> Result<(), PreambleError> {
    let mut seen: Vec<u8> = Vec::with_capacity(MAGIC.len());
    loop {
        let mut byte = [0u8; 1];
        let n = reader.read(&mut byte).await.map_err(PreambleError::Io)?;
        if n == 0 {
            return Err(PreambleError::Eof { noise: seen });
        }
        seen.push(byte[0]);
        if seen.ends_with(MAGIC) {
            return Ok(());
        }
        if seen.len() > MAX_NOISE + MAGIC.len() {
            return Err(PreambleError::TooMuchNoise { noise: seen });
        }
    }
}

/// Human-readable preview of unexpected output, trimmed to a few hundred characters.
pub fn preview(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim();
    let (head, cut) = crate::text::truncate_chars(text, 300);
    if cut { format!("{head}…") } else { head }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn round_trip_plain_and_compressed() {
        let small = br#"{"t":"cancel","id":1}"#.to_vec();
        let large = "部署".repeat(5000).into_bytes();
        let mut wire = Vec::new();
        wire.extend(encode_frame(&small, true));
        let packed = encode_frame(&large, true);
        assert!(
            packed.len() < large.len() / 4,
            "repetitive text should compress"
        );
        wire.extend(&packed);
        wire.extend(encode_frame(&large, false));

        let mut reader = wire.as_slice();
        let a = read_frame(&mut reader).await.unwrap().unwrap();
        assert_eq!(a.body, small);
        let b = read_frame(&mut reader).await.unwrap().unwrap();
        assert_eq!(b.body, large);
        assert_eq!(b.wire_len, packed.len());
        let c = read_frame(&mut reader).await.unwrap().unwrap();
        assert_eq!(c.body, large);
        assert!(read_frame(&mut reader).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn rejects_bad_frames() {
        let mut too_large = ((MAX_FRAME + 1) as u32).to_be_bytes().to_vec();
        too_large.push(0);
        assert!(matches!(
            read_frame(&mut too_large.as_slice()).await,
            Err(FrameError::TooLarge(_))
        ));

        let mut frame = encode_frame(b"{}", false);
        frame[4] = 0x80;
        assert!(matches!(
            read_frame(&mut frame.as_slice()).await,
            Err(FrameError::BadFlags(0x80))
        ));

        let frame = encode_frame(b"{\"a\":1}", false);
        assert!(matches!(
            read_frame(&mut &frame[..frame.len() - 2]).await,
            Err(FrameError::Truncated)
        ));
        assert!(matches!(
            read_frame(&mut &frame[..2]).await,
            Err(FrameError::Truncated)
        ));
    }

    #[tokio::test]
    async fn preamble_skips_shell_noise() {
        let (mut tx, rx) = tokio::io::duplex(1024);
        tx.write_all(b"Welcome to fish, the friendly interactive shell\n")
            .await
            .unwrap();
        tx.write_all(MAGIC).await.unwrap();
        tx.write_all(&encode_frame(b"{}", false)).await.unwrap();
        drop(tx);
        let mut rx = tokio::io::BufReader::new(rx);
        read_preamble(&mut rx).await.unwrap();
        assert_eq!(read_frame(&mut rx).await.unwrap().unwrap().body, b"{}");
    }

    #[tokio::test]
    async fn preamble_reports_what_was_printed() {
        let output = b"fish: Unknown command: sessift\n".to_vec();
        let err = read_preamble(&mut output.as_slice()).await.unwrap_err();
        assert!(matches!(err, PreambleError::Eof { .. }));
        assert!(err.to_string().contains("Unknown command"));

        let noise = vec![b'x'; MAX_NOISE + 100];
        let err = read_preamble(&mut noise.as_slice()).await.unwrap_err();
        assert!(matches!(err, PreambleError::TooMuchNoise { .. }));
    }
}
