//! Building a block map for an image.
//!
//! The work is one forward pass over the image:
//!
//! * ranges the file system reports as holes are never read at all;
//! * everything else is read once, split into all-zero and non-zero runs at
//!   block granularity, and the non-zero runs are hashed as they go.
//!
//! With [`DetectMode::Holes`] and no checksums requested, nothing is read —
//! the map falls straight out of `SEEK_HOLE`, exactly like upstream
//! `bmaptool create`.

use crate::{
    DEFAULT_BATCH_BYTES, DEFAULT_BLOCK_SIZE,
    bmap::{BMAP_FORMAT_VERSION, Bmap},
    checksum::{ChecksumKind, Hasher},
    decompress::DecompressMode,
    error::{Error, Result},
    filemap::{self, DetectMode},
    progress::Progress,
    range::{BlockRange, MappedRange},
    source::ImageSource,
    zero::{self, Span},
};
use std::{os::unix::fs::MetadataExt, path::Path};

/// Largest block size we will accept from `stat`, as a sanity clamp.
const MAX_BLOCK_SIZE: u64 = 1024 * 1024;

/// Knobs for [`create`].
#[derive(Clone, Copy, Debug)]
pub struct CreateOptions {
    /// Override the block size. Defaults to the file system's preferred size.
    pub block_size: Option<u64>,
    /// Digest for per-range checksums. `None` writes ranges without them,
    /// which makes creation roughly twice as fast on a warm cache.
    pub checksum: Option<ChecksumKind>,
    /// Which parts of the image count as skippable.
    pub detect: DetectMode,
    /// Bytes per read.
    pub batch_bytes: usize,
    /// Whether a compressed image should be decoded first. The map then
    /// describes the *decompressed* image, which is what gets flashed.
    pub decompress: DecompressMode,
}

impl Default for CreateOptions {
    fn default() -> Self {
        Self {
            block_size: None,
            checksum: Some(ChecksumKind::Sha256),
            detect: DetectMode::default(),
            batch_bytes: DEFAULT_BATCH_BYTES,
            decompress: DecompressMode::default(),
        }
    }
}

/// Build a block map for the image at `path`.
///
/// A compressed image is decoded on the fly when [`CreateOptions::decompress`]
/// allows it, and the resulting map describes the decompressed image — that is
/// the thing that ends up on the device.
///
/// # Errors
///
/// Fails when the image cannot be read, or when it is empty — a bmap for a
/// zero-length image would describe nothing.
pub fn create(path: &Path, opts: &CreateOptions, progress: &dyn Progress) -> Result<Bmap> {
    create_from(ImageSource::open_auto(path, opts.decompress)?, opts, progress)
}

/// Build a block map from an already-opened source.
///
/// Seekable sources get hole detection; streams (stdin, a decompressor) are
/// scanned for zero blocks only, because there is nothing to ask about holes.
pub fn create_from(
    src: ImageSource,
    opts: &CreateOptions,
    progress: &dyn Progress,
) -> Result<Bmap> {
    match src.size() {
        Some(image_size) => create_seekable(src, image_size, opts, progress),
        None => create_streaming(src, opts, progress),
    }
}

fn create_seekable(
    mut src: ImageSource,
    image_size: u64,
    opts: &CreateOptions,
    progress: &dyn Progress,
) -> Result<Bmap> {
    let path = src.path().to_path_buf();
    if image_size == 0 {
        return Err(Error::invalid("image", format!("'{}' is empty", path.display())));
    }

    let block_size = resolve_block_size(&src, opts)?;
    let blocks_cnt = image_size.div_ceil(block_size);

    let file =
        src.as_file().ok_or_else(|| Error::NotSeekable { op: "create", path: path.clone() })?;
    let candidates =
        filemap::candidate_ranges(file, &path, opts.detect, image_size, block_size, blocks_cnt)?;

    let scan_needed = opts.detect.uses_zeros() || opts.checksum.is_some();
    let ranges = if scan_needed {
        progress.set_total(Some(candidate_bytes(&candidates, block_size, image_size)));
        scan(&mut src, &candidates, opts, block_size, image_size, progress)?
    } else {
        progress.set_total(Some(0));
        candidates.into_iter().map(MappedRange::bare).collect()
    };
    progress.finish();

    Ok(assemble(image_size, block_size, blocks_cnt, ranges, opts))
}

/// Map a forward-only stream. Everything has to be read, and holes are not a
/// concept a stream has, so this is the zero scan and nothing else.
fn create_streaming(
    mut src: ImageSource,
    opts: &CreateOptions,
    progress: &dyn Progress,
) -> Result<Bmap> {
    let path = src.path().to_path_buf();
    let block_size = resolve_block_size(&src, opts)?;
    let block_size_usize = usize::try_from(block_size).unwrap_or(usize::MAX);
    let batch = batch_len(opts.batch_bytes, block_size_usize);

    progress.set_total(None);

    let mut buf = vec![0u8; batch];
    let mut spans: Vec<Span> = Vec::new();
    let mut builder = RangeBuilder::new(block_size, opts.checksum);
    let mut image_size = 0u64;

    loop {
        let filled = src.read_full(&mut buf)?;
        if filled == 0 {
            break;
        }
        let live = &buf[..filled];
        classify(live, block_size_usize, opts.detect, &mut spans);
        consume_spans(&mut builder, &spans, live, image_size, block_size);
        image_size += filled as u64;
        progress.advance(filled as u64, 0);
    }
    progress.finish();

    if image_size == 0 {
        return Err(Error::invalid("image", format!("'{}' is empty", path.display())));
    }

    let blocks_cnt = image_size.div_ceil(block_size);
    Ok(assemble(image_size, block_size, blocks_cnt, builder.finish(), opts))
}

fn assemble(
    image_size: u64,
    block_size: u64,
    blocks_cnt: u64,
    ranges: Vec<MappedRange>,
    opts: &CreateOptions,
) -> Bmap {
    let mapped_blocks_cnt = ranges.iter().map(|r| r.range.count()).sum();
    Bmap {
        version: BMAP_FORMAT_VERSION,
        image_size,
        block_size,
        blocks_cnt,
        mapped_blocks_cnt,
        checksum_kind: Some(opts.checksum.unwrap_or_default()),
        ranges,
    }
}

/// Round a requested batch size down to a whole number of blocks, so a block is
/// never split across two reads.
fn batch_len(requested: usize, block_size: usize) -> usize {
    requested.max(block_size) / block_size * block_size
}

/// Split a batch into zero and non-zero runs, or mark it wholly as data when
/// zero detection is off.
fn classify(buf: &[u8], block_size: usize, detect: DetectMode, out: &mut Vec<Span>) {
    if detect.uses_zeros() {
        zero::classify_blocks(buf, block_size, out);
    } else {
        out.clear();
        if !buf.is_empty() {
            out.push(Span { offset: 0, len: buf.len(), zero: false });
        }
    }
}

/// Feed one batch's spans into the range builder. `base` is the image offset
/// the batch starts at, and is always block-aligned.
fn consume_spans(
    builder: &mut RangeBuilder,
    spans: &[Span],
    buf: &[u8],
    base: u64,
    block_size: u64,
) {
    for span in spans {
        if span.zero {
            builder.close();
        } else {
            let first_block = (base + span.offset as u64) / block_size;
            builder.extend(first_block, &buf[span.offset..span.offset + span.len]);
        }
    }
}

fn resolve_block_size(src: &ImageSource, opts: &CreateOptions) -> Result<u64> {
    if let Some(bs) = opts.block_size {
        if bs == 0 || !bs.is_power_of_two() {
            return Err(Error::invalid("block size", format!("{bs} is not a power of two")));
        }
        return Ok(bs);
    }
    let preferred = src
        .as_file()
        .and_then(|f| f.metadata().ok())
        .map(|m| m.blksize())
        .filter(|bs| *bs >= 512 && *bs <= MAX_BLOCK_SIZE && bs.is_power_of_two())
        .unwrap_or(DEFAULT_BLOCK_SIZE);
    Ok(preferred)
}

fn candidate_bytes(candidates: &[BlockRange], block_size: u64, image_size: u64) -> u64 {
    candidates
        .iter()
        .map(|r| {
            let end = r.end_byte(block_size).min(image_size);
            end.saturating_sub(r.start_byte(block_size))
        })
        .sum()
}

/// Read the candidate ranges and split them into mapped ranges.
fn scan(
    src: &mut ImageSource,
    candidates: &[BlockRange],
    opts: &CreateOptions,
    block_size: u64,
    image_size: u64,
    progress: &dyn Progress,
) -> Result<Vec<MappedRange>> {
    let block_size_usize = usize::try_from(block_size).unwrap_or(usize::MAX);
    let mut buf = vec![0u8; batch_len(opts.batch_bytes, block_size_usize)];
    let mut spans: Vec<Span> = Vec::new();
    let mut builder = RangeBuilder::new(block_size, opts.checksum);

    for candidate in candidates {
        let start = candidate.start_byte(block_size);
        let end = candidate.end_byte(block_size).min(image_size);
        if end <= start {
            continue;
        }
        src.skip_to(start)?;

        let mut offset = start;
        while offset < end {
            let want = usize::try_from(end - offset).unwrap_or(buf.len()).min(buf.len());
            let filled = src.read_full(&mut buf[..want])?;
            if filled == 0 {
                break; // the image shrank underneath us; stop where it ends
            }
            let live = &buf[..filled];
            classify(live, block_size_usize, opts.detect, &mut spans);
            consume_spans(&mut builder, &spans, live, offset, block_size);

            offset += filled as u64;
            progress.advance(filled as u64, 0);
            src.drop_cache_before(offset);
        }

        // A hole always ends a range, even when the next candidate resumes on
        // the very next block.
        builder.close();
    }

    Ok(builder.finish())
}

/// Accumulates contiguous non-zero blocks into [`MappedRange`]s, hashing as it
/// goes so the image is read exactly once.
struct RangeBuilder {
    block_size: u64,
    checksum: Option<ChecksumKind>,
    open: Option<OpenRange>,
    ranges: Vec<MappedRange>,
}

struct OpenRange {
    first: u64,
    last: u64,
    hasher: Option<Hasher>,
}

impl RangeBuilder {
    const fn new(block_size: u64, checksum: Option<ChecksumKind>) -> Self {
        Self { block_size, checksum, open: None, ranges: Vec::new() }
    }

    /// Add `data`, which starts at block `first_block` and is contiguous.
    fn extend(&mut self, first_block: u64, data: &[u8]) {
        let blocks = (data.len() as u64).div_ceil(self.block_size).max(1);
        let last_block = first_block + blocks - 1;

        match self.open.as_mut() {
            Some(open) if open.last + 1 == first_block => {
                open.last = last_block;
                if let Some(h) = open.hasher.as_mut() {
                    h.update(data);
                }
            }
            _ => {
                self.close();
                let mut hasher = self.checksum.map(ChecksumKind::hasher);
                if let Some(h) = hasher.as_mut() {
                    h.update(data);
                }
                self.open = Some(OpenRange { first: first_block, last: last_block, hasher });
            }
        }
    }

    /// End the range currently being built, if any.
    fn close(&mut self) {
        if let Some(open) = self.open.take() {
            self.ranges.push(MappedRange {
                range: BlockRange { first: open.first, last: open.last },
                checksum: open.hasher.map(Hasher::finish),
            });
        }
    }

    fn finish(mut self) -> Vec<MappedRange> {
        self.close();
        self.ranges
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::NoProgress;
    use std::io::Write;

    fn write_image(bytes: &[u8]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn dense_zero_image_maps_almost_nothing() {
        // 1 MiB of zeroes with a 4 KiB blob of data in the middle. The file is
        // written densely, so hole detection alone would map all of it.
        let mut data = vec![0u8; 1024 * 1024];
        data[512 * 1024..512 * 1024 + 4096].fill(0xa5);
        let f = write_image(&data);

        let opts = CreateOptions { block_size: Some(4096), ..CreateOptions::default() };
        let bmap = create(f.path(), &opts, &NoProgress).unwrap();

        assert_eq!(bmap.block_size, 4096);
        assert_eq!(bmap.image_size, 1024 * 1024);
        assert_eq!(bmap.blocks_cnt, 256);
        assert_eq!(bmap.mapped_blocks_cnt, 1);
        assert_eq!(bmap.ranges.len(), 1);
        assert_eq!(bmap.ranges[0].range, BlockRange { first: 128, last: 128 });
        assert_eq!(
            bmap.ranges[0].checksum.as_deref(),
            Some(ChecksumKind::Sha256.digest(&[0xa5u8; 4096]).as_str())
        );
    }

    #[test]
    fn detect_none_maps_everything() {
        let f = write_image(&vec![0u8; 40960]);
        let opts = CreateOptions {
            block_size: Some(4096),
            detect: DetectMode::None,
            checksum: None,
            ..CreateOptions::default()
        };
        let bmap = create(f.path(), &opts, &NoProgress).unwrap();
        assert_eq!(bmap.mapped_blocks_cnt, 10);
        assert_eq!(bmap.ranges.len(), 1);
        assert!(bmap.ranges[0].checksum.is_none());
    }

    #[test]
    fn adjacent_data_blocks_collapse_into_one_range() {
        let mut data = vec![0u8; 4096 * 6];
        data[4096..4096 * 4].fill(1);
        let f = write_image(&data);
        let opts = CreateOptions { block_size: Some(4096), ..CreateOptions::default() };
        let bmap = create(f.path(), &opts, &NoProgress).unwrap();
        assert_eq!(bmap.ranges.len(), 1);
        assert_eq!(bmap.ranges[0].range, BlockRange { first: 1, last: 3 });
        assert_eq!(bmap.mapped_blocks_cnt, 3);
    }

    #[test]
    fn ranges_survive_a_batch_boundary() {
        // Data spanning two read batches must yield a single range with one
        // checksum covering all of it.
        let mut data = vec![0u8; 4096 * 8];
        data[4096..4096 * 7].fill(3);
        let f = write_image(&data);
        let opts = CreateOptions {
            block_size: Some(4096),
            batch_bytes: 4096 * 2,
            ..CreateOptions::default()
        };
        let bmap = create(f.path(), &opts, &NoProgress).unwrap();
        assert_eq!(bmap.ranges.len(), 1);
        assert_eq!(bmap.ranges[0].range, BlockRange { first: 1, last: 6 });
        assert_eq!(
            bmap.ranges[0].checksum.as_deref(),
            Some(ChecksumKind::Sha256.digest(&vec![3u8; 4096 * 6]).as_str())
        );
    }

    #[test]
    fn trailing_partial_block_is_mapped() {
        let mut data = vec![0u8; 4096 + 17];
        data[4096 + 5] = 9;
        let f = write_image(&data);
        let opts = CreateOptions { block_size: Some(4096), ..CreateOptions::default() };
        let bmap = create(f.path(), &opts, &NoProgress).unwrap();
        assert_eq!(bmap.blocks_cnt, 2);
        assert_eq!(
            bmap.ranges,
            vec![MappedRange {
                range: BlockRange { first: 1, last: 1 },
                checksum: Some(ChecksumKind::Sha256.digest(&data[4096..])),
            }]
        );
    }

    #[test]
    fn empty_image_is_rejected() {
        let f = write_image(b"");
        assert!(matches!(
            create(f.path(), &CreateOptions::default(), &NoProgress),
            Err(Error::InvalidArgument { .. })
        ));
    }

    #[test]
    fn all_zero_image_maps_nothing() {
        let f = write_image(&vec![0u8; 4096 * 4]);
        let opts = CreateOptions { block_size: Some(4096), ..CreateOptions::default() };
        let bmap = create(f.path(), &opts, &NoProgress).unwrap();
        assert_eq!(bmap.mapped_blocks_cnt, 0);
        assert!(bmap.ranges.is_empty());
        // It must still round-trip through the XML form.
        let text = bmap.render();
        assert_eq!(Bmap::parse(&text, f.path()).unwrap(), bmap);
    }
}
