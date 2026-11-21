//! Multi-frame Zstandard (`zstd`) reader built on pure-Rust `ruzstd`.
//!
//! OCI `tar+zstd` and `zstd:chunked` layers can contain multiple concatenated
//! zstd frames as well as RFC 8878 skippable frames (used for chunk manifests
//! and footers). `ruzstd::decoding::StreamingDecoder` decodes only a single
//! frame, so this wrapper loops cleanly across concatenated and skippable
//! frames until the underlying stream reaches EOF.

use std::io::{self, BufRead, Read};

use ruzstd::decoding::errors::{FrameDecoderError, ReadFrameHeaderError};
use ruzstd::decoding::{BlockDecodingStrategy, FrameDecoder};

/// True if `magic` begins with a Zstandard frame magic (`0xFD2FB528`, LE) or
/// a Zstandard skippable frame magic (`0x184D2A50..=0x184D2A5F`, LE).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn is_zstd_magic(magic: &[u8]) -> bool {
    if magic.len() < 4 {
        return false;
    }
    magic[..4] == [0x28, 0xb5, 0x2f, 0xfd]
        || ((0x50..=0x5f).contains(&magic[0]) && magic[1..4] == [0x2a, 0x4d, 0x18])
}

/// Streaming Zstandard reader that handles concatenated and skippable frames.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct ZstdReader<R: BufRead> {
    source: R,
    decoder: FrameDecoder,
    in_frame: bool,
    eof: bool,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl<R: BufRead> ZstdReader<R> {
    pub fn new(source: R) -> Self {
        Self { source, decoder: FrameDecoder::new(), in_frame: false, eof: false }
    }

    fn advance_frame(&mut self) -> io::Result<bool> {
        loop {
            if self.source.fill_buf()?.is_empty() {
                self.eof = true;
                self.in_frame = false;
                return Ok(false);
            }
            match self.decoder.reset(&mut self.source) {
                Ok(()) => {
                    self.in_frame = true;
                    return Ok(true);
                }
                Err(FrameDecoderError::ReadFrameHeaderError(ReadFrameHeaderError::SkipFrame { length, .. })) => {
                    let skipped = io::copy(&mut (&mut self.source).take(u64::from(length)), &mut io::sink())?;
                    if skipped < u64::from(length) {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "truncated zstd skippable frame",
                        ));
                    }
                }
                Err(err) => return Err(io::Error::other(format!("zstd header: {err}"))),
            }
        }
    }
}

impl<R: BufRead> Read for ZstdReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.eof {
            return Ok(0);
        }
        loop {
            if !self.in_frame && !self.advance_frame()? {
                return Ok(0);
            }
            while self.decoder.can_collect() == 0 && !self.decoder.is_finished() {
                self.decoder
                    .decode_blocks(&mut self.source, BlockDecodingStrategy::UptoBlocks(1))
                    .map_err(|err| io::Error::other(format!("zstd decode: {err}")))?;
            }
            if self.decoder.can_collect() > 0 {
                return self.decoder.read(buf);
            }
            // Current frame finished with nothing left to collect: check for another frame.
            self.in_frame = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufReader, Cursor, Read};

    use super::*;

    /// Build a minimal valid RFC 8878 zstd frame carrying `payload` in a single raw block.
    fn zstd_raw_frame(payload: &[u8]) -> Vec<u8> {
        assert!(payload.len() < (1 << 21));
        let mut out = Vec::with_capacity(9 + payload.len());
        // Magic: 0xFD2FB528 LE
        out.extend_from_slice(&[0x28, 0xb5, 0x2f, 0xfd]);
        // Frame Header Descriptor: Single_Segment_Flag=1 (0x20), FCS_Field_Size=0 (1 byte if <=255, else 4 bytes with flag=2)
        if payload.len() <= 255 {
            out.push(0x20);
            out.push(payload.len() as u8);
        } else {
            out.push(0x80 | 0x20); // FCS_Flag = 2 (4 bytes) + Single_Segment_Flag
            out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        }
        // Block Header (3 bytes LE): Last_Block=1 (bit 0), Block_Type=0 (Raw, bits 1..2), Block_Size (bits 3..23)
        let bh: u32 = 1 | ((payload.len() as u32) << 3);
        out.extend_from_slice(&bh.to_le_bytes()[..3]);
        out.extend_from_slice(payload);
        out
    }

    /// Build a valid RFC 8878 skippable frame (magic 0x184D2A50..=0x184D2A5F).
    fn zstd_skippable_frame(variant: u8, metadata: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + metadata.len());
        out.extend_from_slice(&[0x50 | (variant & 0x0f), 0x2a, 0x4d, 0x18]);
        out.extend_from_slice(&(metadata.len() as u32).to_le_bytes());
        out.extend_from_slice(metadata);
        out
    }

    #[test]
    fn detects_standard_and_skippable_magic() {
        assert!(is_zstd_magic(&[0x28, 0xb5, 0x2f, 0xfd]));
        assert!(is_zstd_magic(&[0x50, 0x2a, 0x4d, 0x18]));
        assert!(is_zstd_magic(&[0x5f, 0x2a, 0x4d, 0x18]));
        assert!(!is_zstd_magic(&[0x1f, 0x8b, 0x08, 0x00])); // gzip
        assert!(!is_zstd_magic(&[0x28, 0xb5, 0x2f])); // too short
    }

    #[test]
    fn decodes_single_multi_and_skippable_frames() {
        // Simulates zstd:chunked (skippable header + multiple data frames + skippable footer).
        let mut stream = Vec::new();
        stream.extend_from_slice(&zstd_skippable_frame(0, b"chunk-header"));
        stream.extend_from_slice(&zstd_raw_frame(b"hello "));
        stream.extend_from_slice(&zstd_raw_frame(b"zstd "));
        stream.extend_from_slice(&zstd_skippable_frame(5, b"{\"toc\":\"footer\"}"));
        stream.extend_from_slice(&zstd_raw_frame(b"layers!"));

        let mut reader = ZstdReader::new(BufReader::new(Cursor::new(stream)));
        let mut decoded = Vec::new();
        reader.read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded, b"hello zstd layers!");
    }

    #[test]
    fn decodes_compressed_rle_frame_and_small_reads() {
        // Hand-built RLE block (Block_Type=1): repeats 'A' 1000 times.
        let mut frame = vec![0x28, 0xb5, 0x2f, 0xfd, 0x80 | 0x20];
        frame.extend_from_slice(&1000u32.to_le_bytes());
        let bh: u32 = 1 | (1 << 1) | (1000u32 << 3); // last=1, type=RLE(1), size=1000
        frame.extend_from_slice(&bh.to_le_bytes()[..3]);
        frame.push(b'A');

        // Read with a tiny 13-byte buffer to exercise partial reads across calls.
        let mut reader = ZstdReader::new(BufReader::new(Cursor::new(frame)));
        let mut out = Vec::new();
        let mut chunk = [0u8; 13];
        loop {
            let n = reader.read(&mut chunk).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&chunk[..n]);
        }
        assert_eq!(out.len(), 1000);
        assert!(out.iter().all(|&b| b == b'A'));
    }

    #[test]
    fn rejects_truncated_skippable_frame() {
        let mut bad = vec![0x50, 0x2a, 0x4d, 0x18];
        bad.extend_from_slice(&100u32.to_le_bytes());
        bad.extend_from_slice(b"short");
        let mut reader = ZstdReader::new(BufReader::new(Cursor::new(bad)));
        let mut out = Vec::new();
        assert!(reader.read_to_end(&mut out).is_err());
    }
}
