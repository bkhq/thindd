//! Transparent decompression of the image stream.
//!
//! Images are routinely shipped compressed, and decompressing to a scratch file
//! first defeats the point of a bmap: the scratch file is the very thing we were
//! trying not to write. So the compressed stream is inflated on the fly and fed
//! straight to the copy engine.
//!
//! Detection is by **magic bytes**, not by file name. `image.wic.gz` renamed to
//! `image.img` still works, and a gzip stream arriving on standard input is
//! recognised just the same.
//!
//! # What this costs
//!
//! A compressed stream cannot be rewound, so the whole thing is inflated even
//! for the parts of the image that turn out to be skippable. That is unavoidable
//! — those bytes have to be produced before we can tell they are zero. The win
//! is entirely on the write side, which on SD/eMMC/USB media is an order of
//! magnitude slower than inflating, so it still dominates.

#[cfg(not(feature = "gzip"))]
use crate::error::Error;
use crate::error::Result;
use std::io::Read;

/// Number of leading bytes needed to classify a stream.
pub const SNIFF_LEN: usize = 4;

/// A container format the image may arrive in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Compression {
    /// Raw image bytes.
    #[default]
    None,
    /// gzip (RFC 1952), including multi-member streams as produced by
    /// `pigz`, `cat a.gz b.gz`, and rsyncable gzip.
    Gzip,
}

impl Compression {
    /// Human-readable name, used in error messages.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "raw",
            Self::Gzip => "gzip",
        }
    }
}

impl std::fmt::Display for Compression {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// How eagerly to decompress.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DecompressMode {
    /// Sniff the magic bytes and decompress when they match. The default.
    #[default]
    Auto,
    /// Treat the input as raw image bytes whatever it looks like.
    Never,
    /// Force gzip, even without a recognisable header.
    Gzip,
}

impl std::fmt::Display for DecompressMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Auto => "auto",
            Self::Never => "none",
            Self::Gzip => "gzip",
        })
    }
}

/// Classify a stream from its first few bytes.
///
/// ```
/// # use thindd_core::decompress::{sniff, Compression};
/// assert_eq!(sniff(&[0x1f, 0x8b, 0x08, 0x00]), Compression::Gzip);
/// assert_eq!(sniff(b"MBR!"), Compression::None);
/// assert_eq!(sniff(&[]), Compression::None);
/// ```
#[must_use]
pub fn sniff(head: &[u8]) -> Compression {
    // RFC 1952 §2.3.1: ID1 = 0x1f, ID2 = 0x8b. CM = 8 (deflate) is the only
    // compression method ever assigned, so check it too and avoid mistaking a
    // raw image that happens to open with those two bytes for a gzip stream.
    if head.len() >= 3 && head[0] == 0x1f && head[1] == 0x8b && head[2] == 8 {
        Compression::Gzip
    } else {
        Compression::None
    }
}

/// Resolve `mode` against the stream's header bytes.
#[must_use]
pub fn resolve(mode: DecompressMode, head: &[u8]) -> Compression {
    match mode {
        DecompressMode::Never => Compression::None,
        DecompressMode::Gzip => Compression::Gzip,
        DecompressMode::Auto => sniff(head),
    }
}

/// Wrap `reader` in the decoder for `kind`.
///
/// Returns the reader unchanged for [`Compression::None`].
///
/// # Errors
///
/// [`Error::UnsupportedCompression`] when the build lacks the feature for
/// `kind` — the tool then says so instead of copying compressed bytes to a
/// device and leaving the user to work out why it will not boot.
pub fn decode(reader: Box<dyn Read + Send>, kind: Compression) -> Result<Box<dyn Read + Send>> {
    match kind {
        Compression::None => Ok(reader),
        #[cfg(feature = "gzip")]
        Compression::Gzip => Ok(Box::new(flate2::read::MultiGzDecoder::new(reader))),
        #[cfg(not(feature = "gzip"))]
        Compression::Gzip => Err(Error::UnsupportedCompression { format: "gzip" }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gzip_magic_is_recognised() {
        assert_eq!(sniff(&[0x1f, 0x8b, 0x08, 0x00]), Compression::Gzip);
    }

    #[test]
    fn a_truncated_or_foreign_header_is_raw() {
        assert_eq!(sniff(&[0x1f]), Compression::None);
        assert_eq!(sniff(&[0x1f, 0x8b]), Compression::None);
        // Right magic, unassigned compression method: not something we decode.
        assert_eq!(sniff(&[0x1f, 0x8b, 0x09, 0x00]), Compression::None);
        assert_eq!(sniff(&[0u8; 4]), Compression::None);
    }

    #[test]
    fn mode_overrides_the_sniff() {
        let gz = [0x1f, 0x8b, 0x08, 0x00];
        assert_eq!(resolve(DecompressMode::Never, &gz), Compression::None);
        assert_eq!(resolve(DecompressMode::Auto, &gz), Compression::Gzip);
        assert_eq!(resolve(DecompressMode::Gzip, b"raw!"), Compression::Gzip);
    }

    #[cfg(feature = "gzip")]
    #[test]
    fn round_trips_a_multi_member_stream() {
        use flate2::{Compression as Level, write::GzEncoder};
        use std::io::Write as _;

        let mut stream = Vec::new();
        for part in [b"hello ".as_slice(), b"world".as_slice()] {
            let mut enc = GzEncoder::new(Vec::new(), Level::fast());
            enc.write_all(part).unwrap();
            stream.extend(enc.finish().unwrap());
        }

        assert_eq!(sniff(&stream), Compression::Gzip);
        let mut out = String::new();
        decode(Box::new(std::io::Cursor::new(stream)), Compression::Gzip)
            .unwrap()
            .read_to_string(&mut out)
            .unwrap();
        assert_eq!(out, "hello world");
    }
}
