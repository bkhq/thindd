//! Copy destination: a regular file, a block device, or `/dev/null`.
//!
//! Two things matter here beyond "write the bytes".
//!
//! * **Positional writes.** Every write goes through `pwrite`, so the writer
//!   never has to seek between the scattered ranges a bmap describes.
//! * **Cheap zeroing.** When the caller asks for unmapped areas to actually be
//!   zeroed, we ask the kernel to do it without moving any bytes — see
//!   [`fast_zero`]. Writing zero pages by hand is only the fallback.

use crate::error::{Error, Result};
#[cfg(target_os = "linux")]
use rustix::fs::{FallocateFlags, fallocate};
use std::{
    fs::{File, OpenOptions},
    os::unix::fs::{FileExt, FileTypeExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

/// Chunk size used when zeroing has to fall back to real writes.
const ZERO_WRITE_CHUNK: usize = 1024 * 1024;

/// What kind of thing we are writing to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DestKind {
    /// A regular file. Holes can be punched, and the final size can be set.
    RegularFile,
    /// A block device. Fixed capacity, `fallocate` may or may not work.
    BlockDevice,
    /// A character device such as `/dev/null`.
    CharDevice,
    /// Anything else (FIFO, socket, …).
    Other,
}

/// What to do with the parts of the image that are *not* mapped.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ZeroMode {
    /// Leave them untouched. This is what upstream `bmaptool` has always done:
    /// the bmap only promises that mapped blocks are written. On a fresh file
    /// the result reads as zero anyway (it becomes a sparse file); on a block
    /// device that already held data, the old bytes survive.
    #[default]
    Skip,
    /// Guarantee they read back as zero, using `fallocate` where the kernel
    /// supports it and explicit writes otherwise.
    Zero,
}

impl std::fmt::Display for ZeroMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Skip => "skip",
            Self::Zero => "zero",
        })
    }
}

/// An opened copy destination.
#[derive(Debug)]
pub struct Destination {
    file: File,
    path: PathBuf,
    kind: DestKind,
    capacity: Option<u64>,
    rdev: u64,
    /// Cleared the first time the kernel refuses [`fast_zero`], so a
    /// destination that cannot do it pays the failed syscall only once.
    fast_zero_available: std::sync::atomic::AtomicBool,
}

impl Destination {
    /// Open `path` for writing.
    ///
    /// Block devices are opened `O_EXCL`, which makes the kernel refuse the
    /// open while the device or any of its partitions is mounted or otherwise
    /// claimed. That single flag is the whole "don't overwrite your running
    /// root file system" guard, and it is far more reliable than parsing
    /// `/proc/mounts`. `force` drops it.
    pub fn open(path: &Path, force: bool) -> Result<Self> {
        let existing = std::fs::metadata(path).ok();
        let is_block = existing.as_ref().is_some_and(|m| m.file_type().is_block_device());

        let mut opts = OpenOptions::new();
        opts.write(true);
        if is_block {
            if !force {
                opts.custom_flags(exclusive_flag());
            }
        } else {
            opts.create(true);
        }

        let file = opts.open(path).map_err(|e| {
            if is_block && !force && e.kind() == std::io::ErrorKind::ResourceBusy {
                Error::DeviceBusy { path: path.to_path_buf(), source: e }
            } else {
                Error::io("open destination", path, e)
            }
        })?;

        let meta = file.metadata().map_err(|e| Error::io("stat destination", path, e))?;
        let ft = meta.file_type();
        let kind = if ft.is_file() {
            DestKind::RegularFile
        } else if ft.is_block_device() {
            DestKind::BlockDevice
        } else if ft.is_char_device() {
            DestKind::CharDevice
        } else {
            DestKind::Other
        };

        let capacity = if kind == DestKind::BlockDevice {
            rustix::fs::seek(&file, rustix::fs::SeekFrom::End(0)).ok().inspect(|_| {
                let _ = rustix::fs::seek(&file, rustix::fs::SeekFrom::Start(0));
            })
        } else {
            None
        };

        Ok(Self {
            file,
            path: path.to_path_buf(),
            kind,
            capacity,
            rdev: std::os::unix::fs::MetadataExt::rdev(&meta),
            fast_zero_available: std::sync::atomic::AtomicBool::new(cfg!(target_os = "linux")),
        })
    }

    /// Path this destination was opened from.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// What kind of destination this is.
    #[must_use]
    pub const fn kind(&self) -> DestKind {
        self.kind
    }

    /// Capacity in bytes, for destinations that have a fixed one.
    #[must_use]
    pub const fn capacity(&self) -> Option<u64> {
        self.capacity
    }

    /// Device number, used to locate the sysfs knobs of a block device.
    #[must_use]
    pub const fn rdev(&self) -> u64 {
        self.rdev
    }

    /// `true` when syncing this destination is meaningful.
    ///
    /// Character devices have no cache of ours to flush — `/dev/null` is the
    /// one people actually pass — and `fsync` on them fails outright on some
    /// platforms.
    #[must_use]
    pub const fn supports_sync(&self) -> bool {
        !matches!(self.kind, DestKind::CharDevice)
    }

    /// Fail unless the destination can hold `image_size` bytes written at
    /// `offset`.
    pub fn ensure_fits(&self, image_size: u64, offset: u64) -> Result<()> {
        let needed = image_size.saturating_add(offset);
        match self.capacity {
            Some(cap) if cap < needed => Err(Error::DestinationTooSmall {
                path: self.path.clone(),
                image_size,
                dest_offset: offset,
                dest_size: cap,
            }),
            _ => Ok(()),
        }
    }

    /// Write `buf` at absolute byte `offset`.
    pub fn write_all_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        self.file
            .write_all_at(buf, offset)
            .map_err(|e| Error::io("write destination", &self.path, e))
    }

    /// Make `len` bytes at `offset` read back as zero, as cheaply as the
    /// destination allows.
    pub fn zero_range(&self, offset: u64, len: u64) -> Result<()> {
        use std::sync::atomic::Ordering;

        if len == 0 {
            return Ok(());
        }

        if self.fast_zero_available.load(Ordering::Relaxed) {
            match fast_zero(&self.file, self.kind, offset, len) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    tracing::debug!(
                        path = %self.path.display(),
                        error = %e,
                        "in-kernel zeroing unavailable, falling back to explicit writes"
                    );
                    self.fast_zero_available.store(false, Ordering::Relaxed);
                }
            }
        }

        let zeros = vec![0u8; ZERO_WRITE_CHUNK];
        let mut written = 0u64;
        while written < len {
            let chunk =
                usize::try_from(len - written).unwrap_or(ZERO_WRITE_CHUNK).min(ZERO_WRITE_CHUNK);
            self.write_all_at(offset + written, &zeros[..chunk])?;
            written += chunk as u64;
        }
        Ok(())
    }

    /// Clear the **whole** destination before anything is copied onto it.
    ///
    /// This is the one operation that reaches past the end of the image. A bmap
    /// describes the image and says nothing about the space after it, so a
    /// device that previously held a larger or differently laid out image keeps
    /// its old GPT backup header and file-system superblocks out there —
    /// enough to make `blkid`, udev or a bootloader find a partition that no
    /// longer exists.
    ///
    /// Returns the number of bytes cleared. Costs nothing on a regular file
    /// (truncate) and next to nothing on a block device whose controller
    /// implements write-zeroes or discard-with-zeroes; on one that implements
    /// neither it falls back to writing the zeroes, which is slow and worth
    /// telling the user about — see [`Destination::used_fast_zero`].
    pub fn wipe(&self) -> Result<u64> {
        match self.kind {
            DestKind::RegularFile => {
                let len = self.file.metadata().map_or(0, |m| m.len());
                self.file
                    .set_len(0)
                    .map_err(|e| Error::io("truncate destination", &self.path, e))?;
                Ok(len)
            }
            DestKind::BlockDevice => {
                let capacity = self.capacity.unwrap_or(0);
                self.zero_range(0, capacity)?;
                Ok(capacity)
            }
            // Nothing to clear on /dev/null or a fifo.
            DestKind::CharDevice | DestKind::Other => Ok(0),
        }
    }

    /// `false` once the kernel has refused an in-kernel zeroing request and the
    /// slow explicit-write path took over.
    #[must_use]
    pub fn used_fast_zero(&self) -> bool {
        self.fast_zero_available.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Extend a regular-file destination to at least `size`, never shrinking it.
    ///
    /// Used when the copy targets an offset: writing an image into the middle
    /// of a file is a partial update, and truncating away whatever follows it
    /// would be a surprising way to answer that request.
    pub fn grow_to(&self, size: u64) -> Result<()> {
        if self.kind != DestKind::RegularFile {
            return Ok(());
        }
        let current = self.file.metadata().map_or(0, |m| m.len());
        if current >= size {
            return Ok(());
        }
        self.file.set_len(size).map_err(|e| Error::io("extend destination", &self.path, e))
    }

    /// Set the length of a regular-file destination. A no-op elsewhere.
    pub fn set_len(&self, size: u64) -> Result<()> {
        if self.kind != DestKind::RegularFile {
            return Ok(());
        }
        self.file.set_len(size).map_err(|e| Error::io("truncate destination", &self.path, e))
    }

    /// Flush the destination to stable storage.
    pub fn sync(&self) -> Result<()> {
        if !self.supports_sync() {
            return Ok(());
        }
        self.file.sync_data().map_err(|e| Error::io("fsync destination", &self.path, e))
    }
}

/// Ask the kernel to make `len` bytes at `offset` read back as zero without
/// sending a single zero byte through the write path.
///
/// Linux spells this `fallocate`: `PUNCH_HOLE` on a regular file costs no I/O
/// and no disk space at all, and `ZERO_RANGE` on a block device becomes the
/// device's own write-zeroes or discard-with-zeroes command, executed inside
/// the controller.
///
/// No other platform this builds for offers an equivalent that `rustix` can
/// reach without `unsafe` — macOS has `F_PUNCHHOLE`, but only through a raw
/// `fcntl` with a packed struct. There the caller falls back to writing zeroes,
/// which is correct, just slower.
#[cfg(target_os = "linux")]
fn fast_zero(file: &File, kind: DestKind, offset: u64, len: u64) -> std::io::Result<()> {
    let flags = if kind == DestKind::RegularFile {
        FallocateFlags::PUNCH_HOLE | FallocateFlags::KEEP_SIZE
    } else {
        FallocateFlags::ZERO_RANGE
    };
    fallocate(file, flags, offset, len).map_err(Into::into)
}

/// See the Linux version above: nothing portable to call here.
#[cfg(not(target_os = "linux"))]
fn fast_zero(_file: &File, _kind: DestKind, _offset: u64, _len: u64) -> std::io::Result<()> {
    Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
}

/// `O_EXCL` as an `OpenOptionsExt::custom_flags` value.
const fn exclusive_flag() -> i32 {
    #[expect(
        clippy::cast_possible_wrap,
        reason = "O_EXCL is a single low bit; the cast is exact on every Unix"
    )]
    {
        rustix::fs::OFlags::EXCL.bits() as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn regular_file_writes_and_truncates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.img");
        let dest = Destination::open(&path, false).unwrap();
        assert_eq!(dest.kind(), DestKind::RegularFile);

        dest.write_all_at(4096, b"hello").unwrap();
        dest.set_len(8192).unwrap();
        dest.sync().unwrap();

        let mut buf = Vec::new();
        File::open(&path).unwrap().read_to_end(&mut buf).unwrap();
        assert_eq!(buf.len(), 8192);
        assert_eq!(&buf[4096..4101], b"hello");
        assert!(buf[..4096].iter().all(|&b| b == 0));
    }

    #[test]
    fn zero_range_clears_previously_written_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.img");
        let dest = Destination::open(&path, false).unwrap();

        dest.write_all_at(0, &vec![0xabu8; 16384]).unwrap();
        dest.zero_range(4096, 8192).unwrap();
        dest.set_len(16384).unwrap();

        let mut buf = Vec::new();
        File::open(&path).unwrap().read_to_end(&mut buf).unwrap();
        assert!(buf[..4096].iter().all(|&b| b == 0xab));
        assert!(buf[4096..12288].iter().all(|&b| b == 0), "punched range not zero");
        assert!(buf[12288..].iter().all(|&b| b == 0xab));
    }

    #[test]
    fn ensure_fits_ignores_regular_files() {
        let dir = tempfile::tempdir().unwrap();
        let dest = Destination::open(&dir.path().join("out.img"), false).unwrap();
        assert!(dest.ensure_fits(u64::MAX, 0).is_ok());
    }

    #[test]
    fn dev_null_is_not_synced() {
        let Ok(dest) = Destination::open(Path::new("/dev/null"), false) else {
            return;
        };
        assert_eq!(dest.kind(), DestKind::CharDevice);
        assert!(!dest.supports_sync());
        assert!(dest.sync().is_ok());
    }
}
