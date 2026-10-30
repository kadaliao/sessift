//! Read transcript lines back by location. Plain files are read with `pread`; zstd-compressed
//! ones (Codex compresses old rollouts) are decompressed as a stream up to the last wanted
//! line, since a single zstd frame cannot be entered in the middle.

use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::fs::FileExt;
use std::path::Path;

use anyhow::{Context, Result};

/// A line wider than this is not read back (it would only be truncated for display anyway).
pub const MAX_LINE_BYTES: u32 = 64 * 1024 * 1024;

pub fn is_compressed(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "zst")
}

/// Open a transcript for sequential reading, decompressing when needed.
pub fn open_stream(path: &Path) -> Result<Box<dyn BufRead + Send>> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    if is_compressed(path) {
        let decoder = zstd::stream::read::Decoder::new(file)
            .with_context(|| format!("decompress {}", path.display()))?;
        Ok(Box::new(BufReader::with_capacity(1 << 20, decoder)))
    } else {
        Ok(Box::new(BufReader::with_capacity(1 << 20, file)))
    }
}

/// The bytes at each `(pos, len)` span, in the order given. A span that cannot be read
/// (file shrank, truncated stream) yields `None`.
pub fn read_spans(path: &Path, spans: &[(u64, u32)]) -> Result<Vec<Option<Vec<u8>>>> {
    if spans.is_empty() {
        return Ok(Vec::new());
    }
    if !is_compressed(path) {
        let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
        return Ok(spans
            .iter()
            .map(|&(pos, len)| {
                if len > MAX_LINE_BYTES {
                    return None;
                }
                let mut buf = vec![0u8; len as usize];
                file.read_exact_at(&mut buf, pos).ok().map(|_| buf)
            })
            .collect());
    }
    // One pass over the stream in position order.
    let mut order: Vec<usize> = (0..spans.len()).collect();
    order.sort_by_key(|&i| spans[i].0);
    let mut out: Vec<Option<Vec<u8>>> = vec![None; spans.len()];
    let mut stream = open_stream(path)?;
    let mut at: u64 = 0;
    for i in order {
        let (pos, len) = spans[i];
        if len > MAX_LINE_BYTES {
            continue;
        }
        if pos < at {
            // Overlapping or repeated span: reuse an earlier read when identical.
            if let Some(j) =
                (0..spans.len()).find(|&j| j != i && spans[j] == spans[i] && out[j].is_some())
            {
                out[i] = out[j].clone();
            }
            continue;
        }
        let skip = pos - at;
        let skipped = std::io::copy(&mut (&mut stream).take(skip), &mut std::io::sink())?;
        at += skipped;
        if skipped < skip {
            break;
        }
        let mut buf = vec![0u8; len as usize];
        match stream.read_exact(&mut buf) {
            Ok(()) => {
                at += len as u64;
                out[i] = Some(buf);
            }
            Err(_) => break,
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_spans_from_plain_and_compressed_files() {
        let dir = tempfile::tempdir().unwrap();
        let text = b"{\"a\":1}\n{\"b\":\"\xe5\x9b\x9e\xe6\xbb\x9a\"}\n{\"c\":3}\n";
        let plain = dir.path().join("x.jsonl");
        std::fs::write(&plain, text).unwrap();
        let packed = dir.path().join("x.jsonl.zst");
        std::fs::write(&packed, zstd::encode_all(&text[..], 3).unwrap()).unwrap();
        let spans = [(23u64, 7u32), (0, 7), (8, 14), (100, 3)];
        for path in [&plain, &packed] {
            let got = read_spans(path, &spans).unwrap();
            assert_eq!(got[0].as_deref(), Some(&b"{\"c\":3}"[..]));
            assert_eq!(got[1].as_deref(), Some(&b"{\"a\":1}"[..]));
            assert_eq!(
                std::str::from_utf8(got[2].as_deref().unwrap()).unwrap(),
                "{\"b\":\"回滚\"}"
            );
            assert_eq!(got[3], None);
        }
    }
}
