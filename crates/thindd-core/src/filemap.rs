//! Discovering which parts of an image actually need copying.
//!
//! Two independent signals are combined:
//!
//! * **file-system holes** — `lseek(SEEK_DATA)` / `lseek(SEEK_HOLE)` tell us
//!   which byte ranges are backed by real extents. This costs a handful of
//!   syscalls and reads nothing. It is what upstream `bmaptool` relies on.
//! * **zero content** — blocks that are backed by extents but consist entirely
//!   of zero bytes. Finding these requires reading the image, but reading is
//!   cheap compared with writing to eMMC/SD/USB, and it is the only signal that
//!   works on a non-sparse image.
//!
//! `FIEMAP` is deliberately not used. It needs a raw `ioctl` (and therefore
//! `unsafe`), it is Linux-only, it is known to report speculative preallocation
//! as mapped, and `SEEK_HOLE` gives the same answer on every file system that
//! matters. Where `SEEK_HOLE` is unimplemented the kernel reports "all data",
//! which is the safe direction to be wrong in — the zero scan then does the
//! real work.

use crate::{
    error::{Error, Result},
    range::{self, BlockRange},
};
use rustix::{
    fs::{SeekFrom, seek},
    io::Errno,
};
use std::{fs::File, path::Path};

/// How the mapped (must-copy) areas of an image are discovered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DetectMode {
    /// Only skip file-system holes. This is upstream `bmaptool` behaviour.
    Holes,
    /// Only skip all-zero blocks, ignoring hole information.
    Zeros,
    /// Skip holes *and* all-zero blocks. The default, and the reason this tool
    /// exists.
    #[default]
    Both,
    /// Copy everything; produce a fully mapped image.
    None,
}

impl DetectMode {
    /// Whether hole information should be queried.
    #[must_use]
    pub const fn uses_holes(self) -> bool {
        matches!(self, Self::Holes | Self::Both)
    }

    /// Whether image content should be scanned for zero blocks.
    #[must_use]
    pub const fn uses_zeros(self) -> bool {
        matches!(self, Self::Zeros | Self::Both)
    }
}

impl std::fmt::Display for DetectMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Holes => "holes",
            Self::Zeros => "zeros",
            Self::Both => "both",
            Self::None => "none",
        })
    }
}

/// Byte ranges of `file` that are backed by data, as half-open `[start, end)`
/// pairs clamped to `image_size`.
///
/// Returns `Ok(None)` when the kernel or file system does not implement
/// `SEEK_DATA`/`SEEK_HOLE`; the caller should then treat the whole image as
/// mapped and fall back to the zero scan.
///
/// The file offset is restored to the start of the file before returning.
pub fn data_byte_ranges(
    file: &File,
    image_size: u64,
    path: &Path,
) -> Result<Option<Vec<(u64, u64)>>> {
    if image_size == 0 {
        return Ok(Some(Vec::new()));
    }

    let mut out: Vec<(u64, u64)> = Vec::new();
    let mut cursor = 0u64;

    while cursor < image_size {
        let start = match seek(file, SeekFrom::Data(cursor)) {
            Ok(offset) => offset,
            // No further data: everything from here on is a hole.
            Err(Errno::NXIO) => break,
            Err(e) if is_unsupported(e) => {
                tracing::debug!(path = %path.display(), "SEEK_DATA unsupported, scanning content only");
                return Ok(None);
            }
            Err(e) => return Err(Error::io("lseek(SEEK_DATA)", path, e.into())),
        };
        if start >= image_size {
            break;
        }

        let end = match seek(file, SeekFrom::Hole(start)) {
            Ok(offset) => offset.min(image_size),
            Err(Errno::NXIO) => image_size,
            Err(e) if is_unsupported(e) => return Ok(None),
            Err(e) => return Err(Error::io("lseek(SEEK_HOLE)", path, e.into())),
        };

        // A file system that answers both calls with the same offset would spin
        // this loop forever; treat that as "no hole information available".
        if end <= start {
            tracing::debug!(
                path = %path.display(),
                start, end, "inconsistent SEEK_HOLE answer, scanning content only"
            );
            return Ok(None);
        }

        out.push((start, end));
        cursor = end;
    }

    seek(file, SeekFrom::Start(0)).map_err(|e| Error::io("lseek", path, e.into()))?;
    range::coalesce_byte_ranges(&mut out);
    Ok(Some(out))
}

/// Candidate block ranges to feed the scanner, derived from hole information
/// when `detect` asks for it and the file system provides it.
///
/// Falls back to a single range covering the whole image.
pub fn candidate_ranges(
    file: &File,
    path: &Path,
    detect: DetectMode,
    image_size: u64,
    block_size: u64,
    blocks_cnt: u64,
) -> Result<Vec<BlockRange>> {
    let whole = || match BlockRange::new(0, blocks_cnt.saturating_sub(1)) {
        Some(r) if blocks_cnt > 0 => vec![r],
        _ => Vec::new(),
    };

    if !detect.uses_holes() {
        return Ok(whole());
    }

    Ok(data_byte_ranges(file, image_size, path)?
        .map_or_else(whole, |byte_ranges| range::byte_ranges_to_blocks(&byte_ranges, block_size)))
}

/// `true` when the errno means "this file system has no hole information".
const fn is_unsupported(e: Errno) -> bool {
    matches!(e, Errno::INVAL | Errno::NOSYS | Errno::OPNOTSUPP)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, Write};

    #[test]
    fn dense_file_is_one_data_range() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(&vec![1u8; 64 * 1024]).unwrap();
        f.flush().unwrap();
        let ranges = data_byte_ranges(f.as_file(), 64 * 1024, f.path()).unwrap();
        assert_eq!(ranges, Some(vec![(0, 64 * 1024)]));
    }

    #[test]
    fn empty_file_has_no_data_ranges() {
        let f = tempfile::NamedTempFile::new().unwrap();
        assert_eq!(data_byte_ranges(f.as_file(), 0, f.path()).unwrap(), Some(Vec::new()));
    }

    #[test]
    fn sparse_file_reports_only_its_extents() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        // 1 MiB hole, then 64 KiB of data, then a 1 MiB hole.
        f.as_file().set_len(0).unwrap();
        f.seek(std::io::SeekFrom::Start(1024 * 1024)).unwrap();
        f.write_all(&vec![1u8; 64 * 1024]).unwrap();
        f.as_file().set_len(3 * 1024 * 1024).unwrap();
        f.flush().unwrap();

        let Some(ranges) = data_byte_ranges(f.as_file(), 3 * 1024 * 1024, f.path()).unwrap() else {
            // File system without hole support: nothing to assert.
            return;
        };
        // Some file systems round extents outward; assert containment, not
        // exact equality.
        assert!(!ranges.is_empty());
        let covered: u64 = ranges.iter().map(|(a, b)| b - a).sum();
        assert!(covered >= 64 * 1024, "extents {ranges:?} do not cover the written data");
        assert!(covered < 3 * 1024 * 1024, "sparse file reported as fully mapped: {ranges:?}");
        assert!(ranges.iter().any(|&(a, b)| a <= 1024 * 1024 && b >= 1024 * 1024 + 64 * 1024));
    }

    #[test]
    fn detect_mode_flags() {
        assert!(DetectMode::Both.uses_holes() && DetectMode::Both.uses_zeros());
        assert!(DetectMode::Holes.uses_holes() && !DetectMode::Holes.uses_zeros());
        assert!(!DetectMode::Zeros.uses_holes() && DetectMode::Zeros.uses_zeros());
        assert!(!DetectMode::None.uses_holes() && !DetectMode::None.uses_zeros());
    }
}
