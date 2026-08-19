//! Everything this binary prints.
//!
//! `clippy::print_stdout` / `print_stderr` are denied workspace-wide; a CLI has
//! to print, so the exemption is confined to this one module and every write to
//! a standard stream goes through it.

#![allow(clippy::print_stdout, clippy::print_stderr, reason = "this is the CLI output boundary")]

use std::io::Write;
use thindd_core::{Bmap, bmap::human_size, copy::CopyStats};

/// Prefix every human-readable line carries.
const TAG: &str = "thindd:";

/// Print an informational line to stderr, so stdout stays usable for data.
pub(crate) fn note(msg: &str) {
    eprintln!("{TAG} {msg}");
}

/// Print a warning.
pub(crate) fn warn(msg: &str) {
    eprintln!("{TAG} WARNING: {msg}");
}

/// Print a fatal error.
pub(crate) fn error(err: &anyhow::Error) {
    eprintln!("{TAG} ERROR: {err}");
    for cause in err.chain().skip(1) {
        eprintln!("{TAG}   caused by: {cause}");
    }
}

/// Write arbitrary text to stdout (the `create -o -` path).
pub(crate) fn stdout_write(text: &str) -> std::io::Result<()> {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    lock.write_all(text.as_bytes())?;
    lock.flush()
}

/// Describe a block map.
pub(crate) fn describe_bmap(bmap: &Bmap, list_ranges: bool) {
    println!("format version:     {}.{}", bmap.version.0, bmap.version.1);
    println!("image size:         {} ({} bytes)", human_size(bmap.image_size), bmap.image_size);
    println!("block size:         {} bytes", bmap.block_size);
    println!("blocks:             {}", bmap.blocks_cnt);
    println!(
        "mapped blocks:      {} ({}, {:.1}%)",
        bmap.mapped_blocks_cnt,
        human_size(bmap.mapped_size()),
        bmap.mapped_percent()
    );
    println!(
        "skipped:            {} ({:.1}%)",
        human_size(bmap.image_size.saturating_sub(bmap.mapped_bytes())),
        100.0 - bmap.mapped_percent()
    );
    println!("ranges:             {}", bmap.ranges.len());
    println!(
        "checksums:          {}",
        bmap.checksum_kind.map_or_else(|| "none".to_owned(), |k| k.to_string())
    );

    if list_ranges {
        println!();
        for r in &bmap.ranges {
            match &r.checksum {
                Some(c) => println!("  {:<24} {c}", r.range.to_string()),
                None => println!("  {}", r.range),
            }
        }
    }
}

/// Report what a finished copy did.
pub(crate) fn report_copy(stats: &CopyStats) {
    note(&format!(
        "wrote {} of {} ({:.1}% skipped) in {} — {}/s effective",
        human_size(stats.bytes_written),
        human_size(stats.image_size),
        stats.elided_percent(),
        thindd_core::bmap::human_time(stats.elapsed),
        human_size(throughput_bytes(stats)),
    ));
    if stats.bytes_wiped > 0 {
        note(&format!("wiped {} before copying", human_size(stats.bytes_wiped)));
    }
    if stats.bytes_zeroed > 0 {
        note(&format!("zeroed {} on the destination", human_size(stats.bytes_zeroed)));
    }
}

/// Throughput rounded to whole bytes per second, for display.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "display only; the value is non-negative and far below u64::MAX"
)]
fn throughput_bytes(stats: &CopyStats) -> u64 {
    stats.throughput() as u64
}
