#![forbid(unsafe_code)]

//! Block-map (`bmap`) creation and image copying, with first-class handling of
//! images that contain large all-zero regions.
//!
//! The crate is a from-scratch Rust implementation of the ideas behind the
//! Yocto Project's [`bmaptool`]. It keeps the on-disk bmap format (version 2.0)
//! byte-compatible with upstream, so bmap files can be exchanged in both
//! directions, and adds one significant capability on top:
//!
//! * upstream discovers "must copy" areas purely from **file-system holes**
//!   (`SEEK_HOLE` / `FIEMAP`). A dense image full of zero bytes — the usual
//!   shape of an image that has been downloaded, decompressed or `dd`-ed off a
//!   device — therefore maps 100% and gains nothing.
//! * this crate additionally detects **all-zero blocks by content**
//!   ([`DetectMode::Both`] is the default), so those images flash at the speed
//!   of their non-zero payload.
//!
//! # Layout
//!
//! | Module | Responsibility |
//! |---|---|
//! | [`bmap`] | the bmap XML format: parse, verify, render |
//! | [`checksum`] | SHA-1 / SHA-256 / SHA-512 dispatch |
//! | [`zero`] | branch-light all-zero detection |
//! | [`filemap`] | hole discovery via `SEEK_DATA` / `SEEK_HOLE` |
//! | [`create`] | building a bmap for an image |
//! | [`copy`] | the pipelined copy engine |
//! | [`decompress`] | transparent gzip decoding, detected by magic bytes |
//! | [`source`] | seekable or streaming image input |
//! | [`dest`] | regular-file / block-device output, hole punching |
//! | [`sysfs`] | temporary block-device I/O tuning |
//!
//! # Example
//!
//! ```no_run
//! use thindd_core::{create::{self, CreateOptions}, progress::NoProgress};
//! use std::path::Path;
//!
//! # fn main() -> Result<(), thindd_core::Error> {
//! let bmap = create::create(Path::new("core-image.wic"), &CreateOptions::default(), &NoProgress)?;
//! println!("{}", bmap.render());
//! # Ok(())
//! # }
//! ```
//!
//! [`bmaptool`]: https://github.com/yoctoproject/bmaptool

pub mod bmap;
pub mod checksum;
pub mod copy;
pub mod create;
pub mod decompress;
pub mod dest;
pub mod error;
pub mod filemap;
pub mod progress;
pub mod range;
pub mod source;
pub mod sysfs;
pub mod zero;

pub use crate::{
    bmap::{Bmap, human_size},
    checksum::ChecksumKind,
    decompress::{Compression, DecompressMode},
    dest::{Destination, ZeroMode},
    error::{Error, Result},
    filemap::DetectMode,
    range::{BlockRange, MappedRange},
    source::ImageSource,
};

/// Default block size used when the file system does not report a usable one.
pub const DEFAULT_BLOCK_SIZE: u64 = 4096;

/// Default size of a single read/write batch.
///
/// Large enough that per-syscall overhead disappears, small enough that the
/// reader/writer pipeline stays responsive on slow USB media.
pub const DEFAULT_BATCH_BYTES: usize = 8 * 1024 * 1024;

/// Default number of batches in flight between the reader and the writer.
pub const DEFAULT_QUEUE_DEPTH: usize = 4;
