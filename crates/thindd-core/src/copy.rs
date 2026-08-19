//! The copy engine.
//!
//! # Shape
//!
//! ```text
//!  reader thread                       writer thread (the caller's)
//!  ─────────────                       ────────────────────────────
//!  take buffer from pool  ──┐
//!  read one batch           │  bounded
//!  classify zero / data     │  channel   ──▶  pwrite the data spans
//!  hash for verification    │                 fallocate the zero spans
//!  send batch             ──┘                 return buffer to the pool
//! ```
//!
//! Reading and writing overlap, which matters because the two sides usually
//! differ by an order of magnitude in speed: the image lives on an `NVMe` disk or
//! in the page cache, the destination is an SD card. Buffers are recycled
//! through a second channel so a multi-gigabyte copy allocates a fixed,
//! bounded amount of memory.
//!
//! # What gets skipped
//!
//! Three independent things can keep a block from being written:
//!
//! 1. it is outside every range of the bmap file, if one was supplied;
//! 2. it sits in a file-system hole of the image ([`DetectMode::uses_holes`]);
//! 3. it is present but entirely zero ([`DetectMode::uses_zeros`]).
//!
//! Point 3 is what makes this fast on ordinary, non-sparse images, and it costs
//! nothing extra when a bmap is in play: those blocks have to be read anyway to
//! verify the range checksum, so eliding the *write* is free.

use crate::{
    DEFAULT_BATCH_BYTES, DEFAULT_BLOCK_SIZE, DEFAULT_QUEUE_DEPTH,
    bmap::Bmap,
    checksum::ChecksumKind,
    dest::{DestKind, Destination, ZeroMode},
    error::{Error, Result},
    filemap::{self, DetectMode},
    progress::Progress,
    range::{BlockRange, MappedRange},
    source::ImageSource,
    zero::{self, Span},
};
use std::{
    sync::mpsc::{Receiver, SyncSender, sync_channel},
    thread,
    time::{Duration, Instant},
};

/// Knobs for [`copy`].
#[derive(Clone, Copy, Debug)]
pub struct CopyOptions {
    /// Which parts of the image may be skipped.
    pub detect: DetectMode,
    /// What to do with the parts that are skipped.
    pub zero_mode: ZeroMode,
    /// Verify per-range checksums from the bmap file while copying.
    pub verify: bool,
    /// `fsync` the destination when the copy finishes.
    pub sync: bool,
    /// Block size to assume when no bmap file supplies one.
    pub block_size: Option<u64>,
    /// Bytes per read/write batch.
    pub batch_bytes: usize,
    /// Batches in flight between reader and writer.
    pub queue_depth: usize,
    /// Sync the destination every this many written bytes. Keeps the final
    /// `close()` from blocking for minutes on slow media, and keeps the
    /// progress bar honest. `None` disables intermediate syncs.
    pub sync_watermark: Option<u64>,
    /// Clear the whole destination before copying — see [`Destination::wipe`].
    /// This is the only setting that reaches past the end of the image.
    pub wipe: bool,
    /// Byte offset on the destination at which the image starts.
    ///
    /// `dd`'s `seek=`, in bytes. Everything the copy writes, zeroes or checks
    /// for capacity is shifted by this much; the image itself is unchanged, so
    /// a bmap made for it stays valid.
    pub dest_offset: u64,
}

impl Default for CopyOptions {
    fn default() -> Self {
        Self {
            detect: DetectMode::default(),
            zero_mode: ZeroMode::default(),
            verify: true,
            sync: true,
            block_size: None,
            batch_bytes: DEFAULT_BATCH_BYTES,
            queue_depth: DEFAULT_QUEUE_DEPTH,
            sync_watermark: Some(16 * 1024 * 1024),
            wipe: false,
            dest_offset: 0,
        }
    }
}

/// What a copy actually did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CopyStats {
    /// Size of the image, discovered from the bmap, from `stat`, or from where
    /// the stream ended.
    pub image_size: u64,
    /// Block size used.
    pub block_size: u64,
    /// Image bytes read and inspected.
    pub bytes_read: u64,
    /// Bytes actually written to the destination.
    pub bytes_written: u64,
    /// Bytes that were skipped because they were zero or unmapped.
    pub bytes_elided: u64,
    /// Bytes explicitly zeroed on the destination (only with [`ZeroMode::Zero`]).
    pub bytes_zeroed: u64,
    /// Bytes cleared by an up-front wipe (only with [`CopyOptions::wipe`]).
    pub bytes_wiped: u64,
    /// Mapped blocks read, for the "does this bmap belong to this image" check.
    pub blocks_read: u64,
    /// Wall-clock duration of the copy.
    pub elapsed: Duration,
}

impl CopyStats {
    /// Fraction of the image that never had to be written, in percent.
    #[must_use]
    #[expect(clippy::cast_precision_loss, reason = "display-only percentage")]
    pub fn elided_percent(&self) -> f64 {
        if self.image_size == 0 {
            return 0.0;
        }
        (self.image_size - self.bytes_written) as f64 * 100.0 / self.image_size as f64
    }

    /// Effective throughput in bytes per second, measured against image size.
    #[must_use]
    #[expect(clippy::cast_precision_loss, reason = "display-only rate")]
    pub fn throughput(&self) -> f64 {
        let secs = self.elapsed.as_secs_f64();
        if secs <= 0.0 { 0.0 } else { self.image_size as f64 / secs }
    }
}

/// One unit of work handed from the reader to the writer.
#[derive(Debug)]
struct Op {
    /// Absolute image offset the payload starts at.
    offset: u64,
    payload: Payload,
}

#[derive(Debug)]
enum Payload {
    /// A batch that was read, already classified into zero and data spans.
    /// `len` is how much of `buf` is live; the rest is stale pool capacity.
    Batch { buf: Vec<u8>, len: usize, spans: Vec<Span> },
    /// A region that was never read and is known to be unmapped.
    Unmapped { len: u64 },
}

/// Copy `src` onto `dest`.
///
/// `bmap` is optional: without it the engine discovers what to copy itself,
/// which is exactly as fast for zero-heavy images and only costs the read of
/// the image (which it has to do regardless).
///
/// # Errors
///
/// Returns [`Error::RangeChecksum`] when `opts.verify` is set and the image
/// does not match the bmap, [`Error::DestinationTooSmall`] when the image does
/// not fit, and [`Error::Io`] for anything the kernel refuses.
pub fn copy(
    src: ImageSource,
    dest: &Destination,
    bmap: Option<&Bmap>,
    opts: &CopyOptions,
    progress: &dyn Progress,
) -> Result<CopyStats> {
    let started = Instant::now();
    let block_size = resolve_block_size(bmap, opts)?;
    let image_size = bmap.map(|b| b.image_size).or_else(|| src.size());

    if let Some(size) = image_size {
        if opts.dest_offset.checked_add(size).is_none() {
            return Err(Error::invalid(
                "seek offset",
                format!("{} plus the image size overflows a 64-bit offset", opts.dest_offset),
            ));
        }
        dest.ensure_fits(size, opts.dest_offset)?;
    }
    // Wipe before sizing: on a regular file the wipe truncates to zero, and the
    // sizing below then re-establishes the final length as a single hole.
    let bytes_wiped = if opts.wipe { dest.wipe()? } else { 0 };
    if let Some(size) = image_size {
        size_destination(dest, opts.dest_offset, size)?;
    }

    let plan = build_plan(&src, bmap, opts, block_size, image_size)?;
    progress.set_total(plan.total_bytes(block_size, image_size));

    let cfg = ReaderCfg { block_size, image_size, detect: opts.detect, verify: opts.verify };
    let checksum_kind = bmap.and_then(|b| b.checksum_kind).unwrap_or_default();

    // Whatever happens, the progress reporter is told the run is over before
    // the error travels any further.
    let outcome = run_pipeline(src, dest, plan, opts, progress, &cfg, checksum_kind);
    progress.finish();
    let outcome = outcome?;

    let image_size = image_size.unwrap_or(outcome.read.bytes_read);

    if let Some(bmap) = bmap
        && outcome.read.blocks_read != bmap.mapped_blocks_cnt
    {
        return Err(Error::MappedBlockMismatch {
            path: outcome.read.path,
            read: outcome.read.blocks_read,
            expected: bmap.mapped_blocks_cnt,
        });
    }

    // Regular files are sized again: skipped tail blocks leave the file short.
    size_destination(dest, opts.dest_offset, image_size)?;
    if opts.sync {
        dest.sync()?;
    }

    Ok(CopyStats {
        image_size,
        block_size,
        bytes_read: outcome.read.bytes_read,
        bytes_written: outcome.written.written,
        bytes_elided: outcome.written.elided,
        bytes_zeroed: outcome.written.zeroed,
        bytes_wiped,
        blocks_read: outcome.read.blocks_read,
        elapsed: started.elapsed(),
    })
}

/// Run the reader and writer against each other until one of them stops.
fn run_pipeline(
    src: ImageSource,
    dest: &Destination,
    plan: Plan,
    opts: &CopyOptions,
    progress: &dyn Progress,
    cfg: &ReaderCfg,
    checksum_kind: ChecksumKind,
) -> Result<Outcome> {
    let batch_bytes = opts.batch_bytes.max(usize::try_from(cfg.block_size).unwrap_or(usize::MAX));
    let queue_depth = opts.queue_depth.max(1);

    thread::scope(|scope| {
        let (tx, rx) = sync_channel::<Op>(queue_depth);
        let (pool_tx, pool_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        for _ in 0..queue_depth {
            // The receiver is alive for the whole scope, so this cannot fail.
            drop(pool_tx.send(vec![0u8; batch_bytes]));
        }

        let handle = scope.spawn(move || read_image(src, plan, cfg, checksum_kind, &tx, &pool_rx));

        let write_result = write_stream(&rx, dest, opts, progress, &pool_tx);
        // Dropping both ends unblocks a reader that is waiting on a full
        // channel or on an empty buffer pool, so the join below cannot hang.
        drop(rx);
        drop(pool_tx);

        let read_result = handle.join().map_err(|_| Error::ReaderLost)?;

        // A writer failure usually shows up on both sides, because the reader
        // notices the closed channel. Report the reader's error only when the
        // writer is happy, so the root cause wins.
        let written = write_result?;
        let read = read_result?;
        Ok(Outcome { read, written })
    })
}

struct Outcome {
    read: ReadOutcome,
    written: WriteOutcome,
}

/// Give a regular-file destination its final length.
///
/// At offset zero the file *is* the image, so it is truncated to match. At any
/// other offset the copy is a partial update of something larger, and shrinking
/// it would throw away bytes the caller never asked about.
fn size_destination(dest: &Destination, dest_offset: u64, image_size: u64) -> Result<()> {
    let end = dest_offset.saturating_add(image_size);
    if dest_offset == 0 { dest.set_len(end) } else { dest.grow_to(end) }
}

/// What the copy engine intends to read.
#[derive(Debug)]
enum Plan {
    /// A known set of mapped ranges, in ascending order.
    Ranges(Vec<MappedRange>),
    /// Read from the current position until end of image.
    UntilEof,
}

impl Plan {
    fn total_bytes(&self, block_size: u64, image_size: Option<u64>) -> Option<u64> {
        match self {
            Self::Ranges(ranges) => Some(
                ranges
                    .iter()
                    .map(|r| {
                        let end = image_size.map_or_else(
                            || r.range.end_byte(block_size),
                            |s| r.range.end_byte(block_size).min(s),
                        );
                        end.saturating_sub(r.range.start_byte(block_size))
                    })
                    .sum(),
            ),
            Self::UntilEof => image_size,
        }
    }
}

fn resolve_block_size(bmap: Option<&Bmap>, opts: &CopyOptions) -> Result<u64> {
    let block_size =
        bmap.map_or_else(|| opts.block_size.unwrap_or(DEFAULT_BLOCK_SIZE), |b| b.block_size);
    if block_size == 0 || !block_size.is_power_of_two() {
        return Err(Error::invalid("block size", format!("{block_size} is not a power of two")));
    }
    Ok(block_size)
}

fn build_plan(
    src: &ImageSource,
    bmap: Option<&Bmap>,
    opts: &CopyOptions,
    block_size: u64,
    image_size: Option<u64>,
) -> Result<Plan> {
    if let Some(bmap) = bmap {
        return Ok(Plan::Ranges(bmap.ranges.clone()));
    }
    let (Some(file), Some(size)) = (src.as_file(), image_size) else {
        // A stream: we cannot ask about holes, so read everything and let the
        // zero scanner do the work.
        return Ok(Plan::UntilEof);
    };
    if !opts.detect.uses_holes() {
        return Ok(Plan::UntilEof);
    }
    let blocks_cnt = size.div_ceil(block_size);
    let ranges =
        filemap::candidate_ranges(file, src.path(), opts.detect, size, block_size, blocks_cnt)?;
    Ok(Plan::Ranges(ranges.into_iter().map(MappedRange::bare).collect()))
}

struct ReaderCfg {
    block_size: u64,
    image_size: Option<u64>,
    detect: DetectMode,
    verify: bool,
}

struct ReadOutcome {
    path: std::path::PathBuf,
    bytes_read: u64,
    blocks_read: u64,
}

/// Reader thread: walk the plan, classify, hand batches to the writer.
fn read_image(
    mut src: ImageSource,
    plan: Plan,
    cfg: &ReaderCfg,
    checksum_kind: ChecksumKind,
    tx: &SyncSender<Op>,
    pool: &Receiver<Vec<u8>>,
) -> Result<ReadOutcome> {
    let path = src.path().to_path_buf();
    let mut bytes_read = 0u64;
    let mut blocks_read = 0u64;
    let mut spans: Vec<Span> = Vec::new();
    let block_size_usize = usize::try_from(cfg.block_size).unwrap_or(usize::MAX);

    match plan {
        Plan::UntilEof => {
            let mut offset = 0u64;
            while let Ok(mut buf) = pool.recv() {
                let filled = src.read_full(&mut buf)?;
                if filled == 0 {
                    break;
                }
                classify(&buf[..filled], block_size_usize, cfg.detect, &mut spans);
                bytes_read += filled as u64;
                blocks_read += (filled as u64).div_ceil(cfg.block_size);
                let payload = Payload::Batch { buf, len: filled, spans: spans.clone() };
                if tx.send(Op { offset, payload }).is_err() {
                    break;
                }
                offset += filled as u64;
            }
        }
        Plan::Ranges(ranges) => {
            let mut next_unmapped_block = 0u64;
            for entry in &ranges {
                report_gap(tx, cfg, next_unmapped_block, entry.range.first);
                next_unmapped_block = entry.range.last + 1;

                let start = entry.range.start_byte(cfg.block_size);
                let end = cfg.image_size.map_or_else(
                    || entry.range.end_byte(cfg.block_size),
                    |s| entry.range.end_byte(cfg.block_size).min(s),
                );
                if end <= start {
                    continue;
                }
                src.skip_to(start)?;

                let mut hasher =
                    (cfg.verify && entry.checksum.is_some()).then(|| checksum_kind.hasher());
                let mut offset = start;

                while offset < end {
                    let Ok(mut buf) = pool.recv() else {
                        return Ok(ReadOutcome { path, bytes_read, blocks_read });
                    };
                    let want = usize::try_from(end - offset).unwrap_or(buf.len()).min(buf.len());
                    let filled = src.read_full(&mut buf[..want])?;
                    if filled == 0 {
                        return Err(Error::ShortImage { path, read: offset, expected: end });
                    }
                    if let Some(h) = hasher.as_mut() {
                        h.update(&buf[..filled]);
                    }
                    classify(&buf[..filled], block_size_usize, cfg.detect, &mut spans);
                    bytes_read += filled as u64;
                    let payload = Payload::Batch { buf, len: filled, spans: spans.clone() };
                    if tx.send(Op { offset, payload }).is_err() {
                        return Ok(ReadOutcome { path, bytes_read, blocks_read });
                    }
                    offset += filled as u64;
                }

                blocks_read += entry.range.count();
                src.drop_cache_before(offset);

                if let (Some(h), Some(expected)) = (hasher, entry.checksum.as_deref()) {
                    let actual = h.finish();
                    if actual != expected {
                        return Err(Error::RangeChecksum {
                            first: entry.range.first,
                            last: entry.range.last,
                            expected: expected.to_owned(),
                            actual,
                        });
                    }
                }
            }
            if let Some(size) = cfg.image_size {
                report_gap(tx, cfg, next_unmapped_block, size.div_ceil(cfg.block_size));
            }
        }
    }

    Ok(ReadOutcome { path, bytes_read, blocks_read })
}

/// Tell the writer about `[first_block, end_block)` — blocks that are not in
/// the plan and were therefore never read.
fn report_gap(tx: &SyncSender<Op>, cfg: &ReaderCfg, first_block: u64, end_block: u64) {
    if end_block <= first_block {
        return;
    }
    let start = first_block * cfg.block_size;
    let end =
        cfg.image_size.map_or(end_block * cfg.block_size, |s| (end_block * cfg.block_size).min(s));
    if end <= start {
        return;
    }
    // A send failure means the writer has already stopped; it will surface its
    // own error, so this is not the place to report one.
    drop(tx.send(Op { offset: start, payload: Payload::Unmapped { len: end - start } }));
}

/// Classify a batch into zero and data spans, or mark it wholly as data when
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

/// Byte tallies produced by the writer.
#[derive(Default)]
struct WriteOutcome {
    written: u64,
    elided: u64,
    zeroed: u64,
}

/// Writer: drain the channel until the reader closes it.
fn write_stream(
    rx: &Receiver<Op>,
    dest: &Destination,
    opts: &CopyOptions,
    progress: &dyn Progress,
    pool: &std::sync::mpsc::Sender<Vec<u8>>,
) -> Result<WriteOutcome> {
    let mut out = WriteOutcome::default();
    let mut since_sync = 0u64;

    while let Ok(op) = rx.recv() {
        match op.payload {
            Payload::Batch { buf, len, spans } => {
                let mut written_here = 0u64;
                for span in &spans {
                    let at = opts.dest_offset + op.offset + span.offset as u64;
                    let span_len = span.len as u64;
                    if span.zero {
                        match opts.zero_mode {
                            ZeroMode::Skip => out.elided += span_len,
                            ZeroMode::Zero => {
                                dest.zero_range(at, span_len)?;
                                out.zeroed += span_len;
                            }
                        }
                    } else {
                        dest.write_all_at(at, &buf[span.offset..span.offset + span.len])?;
                        written_here += span_len;
                    }
                }
                out.written += written_here;
                since_sync += written_here;
                // Recycle the buffer before reporting progress so the reader
                // gets it back as early as possible.
                drop(pool.send(buf));
                progress.advance(len as u64, written_here);
            }
            Payload::Unmapped { len } => match opts.zero_mode {
                ZeroMode::Skip => out.elided += len,
                ZeroMode::Zero => {
                    dest.zero_range(opts.dest_offset + op.offset, len)?;
                    out.zeroed += len;
                }
            },
        }

        if let Some(watermark) = opts.sync_watermark
            && since_sync >= watermark
            && dest.kind() == DestKind::BlockDevice
        {
            dest.sync()?;
            since_sync = 0;
        }
    }

    Ok(out)
}

/// Build a bmap-shaped plan covering the whole image, for callers that want to
/// copy without any skipping at all.
#[must_use]
pub fn full_image_ranges(blocks_cnt: u64) -> Vec<MappedRange> {
    BlockRange::new(0, blocks_cnt.saturating_sub(1))
        .filter(|_| blocks_cnt > 0)
        .map(MappedRange::bare)
        .into_iter()
        .collect()
}
