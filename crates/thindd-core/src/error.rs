//! Error types for this crate.

use std::{io, path::PathBuf};

/// Result alias used throughout the crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Everything that can go wrong while creating a bmap or copying an image.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A syscall against a named path failed.
    #[error("{op} failed on '{}'", path.display())]
    Io {
        /// Short description of the operation that failed, e.g. `"open"`.
        op: &'static str,
        /// Path the operation was performed on.
        path: PathBuf,
        /// Underlying OS error.
        #[source]
        source: io::Error,
    },

    /// The bmap file is not well-formed XML, or a required element is missing.
    #[error("cannot parse bmap file '{}': {reason}", path.display())]
    BmapParse {
        /// Path of the offending bmap file.
        path: PathBuf,
        /// Human-readable explanation.
        reason: String,
    },

    /// The checksum embedded in the bmap file does not match its contents.
    #[error(
        "checksum mismatch for bmap file '{}': calculated '{actual}', expected '{expected}'",
        path.display()
    )]
    BmapChecksum {
        /// Path of the offending bmap file.
        path: PathBuf,
        /// Checksum recorded inside the file.
        expected: String,
        /// Checksum computed over the file contents.
        actual: String,
    },

    /// The bmap file uses a format version this implementation cannot read.
    #[error("bmap format version {version} is not supported (this build supports up to 2.x)")]
    UnsupportedBmapVersion {
        /// The version string found in the file.
        version: String,
    },

    /// The bmap file names a digest algorithm that is not compiled in.
    #[error("unsupported checksum algorithm '{name}' (supported: sha1, sha256, sha512)")]
    UnsupportedChecksum {
        /// Algorithm name as it appeared in the file.
        name: String,
    },

    /// A block range read from the image does not hash to the value recorded in
    /// the bmap file — the image and the bmap do not belong together, or the
    /// image is corrupt.
    #[error(
        "checksum mismatch for blocks {first}-{last}: calculated '{actual}', expected '{expected}'"
    )]
    RangeChecksum {
        /// First block of the range.
        first: u64,
        /// Last block of the range, inclusive.
        last: u64,
        /// Checksum recorded in the bmap file.
        expected: String,
        /// Checksum computed over the image data.
        actual: String,
    },

    /// The destination cannot hold the image at the requested offset.
    #[error("{}", too_small_message(path, *image_size, *dest_offset, *dest_size))]
    DestinationTooSmall {
        /// Destination path.
        path: PathBuf,
        /// Size of the image in bytes.
        image_size: u64,
        /// Byte offset the image was to be written at.
        dest_offset: u64,
        /// Capacity of the destination in bytes.
        dest_size: u64,
    },

    /// The block device is in use by the kernel (mounted, held by device
    /// mapper, …). Opening it exclusively failed.
    #[error(
        "block device '{}' is busy — it (or one of its partitions) is most likely mounted; \
         unmount it, or pass --force to write anyway",
        path.display()
    )]
    DeviceBusy {
        /// Device path.
        path: PathBuf,
        /// Underlying OS error from the exclusive `open`.
        #[source]
        source: io::Error,
    },

    /// The image ended before the bmap said it would.
    #[error(
        "image '{}' ended after {read} bytes, but the bmap describes {expected} bytes",
        path.display()
    )]
    ShortImage {
        /// Image path.
        path: PathBuf,
        /// Bytes actually read.
        read: u64,
        /// Bytes the bmap expects.
        expected: u64,
    },

    /// The image does not contain as many mapped blocks as the bmap claims.
    #[error(
        "read {read} mapped blocks from '{}' but the bmap describes {expected} — \
         the bmap file does not belong to this image",
        path.display()
    )]
    MappedBlockMismatch {
        /// Image path.
        path: PathBuf,
        /// Blocks actually read.
        read: u64,
        /// Blocks the bmap describes.
        expected: u64,
    },

    /// A non-seekable input was used for an operation that needs random access.
    #[error("{op} requires a seekable input, but '{}' is a stream", path.display())]
    NotSeekable {
        /// Operation that needs seeking.
        op: &'static str,
        /// Input path (or `-` for stdin).
        path: PathBuf,
    },

    /// A caller-supplied parameter is out of range.
    #[error("invalid {what}: {reason}")]
    InvalidArgument {
        /// Which parameter is wrong.
        what: &'static str,
        /// Why it is wrong.
        reason: String,
    },

    /// A wipe was requested for something whose size is unknown.
    #[error(
        "cannot clear '{}': it reports no size, so there is nothing to clear — \
         a wipe needs a regular file or a device",
        path.display()
    )]
    CannotWipe {
        /// Destination path.
        path: PathBuf,
    },

    /// The image is compressed with a format this build cannot decode.
    #[error(
        "the image is {format}-compressed, but this build has no {format} support \
         (rebuild with the '{format}' feature, or decompress it first)"
    )]
    UnsupportedCompression {
        /// Name of the container format, e.g. `"gzip"`.
        format: &'static str,
    },

    /// The reader thread panicked. Only reachable on a bug in this crate.
    #[error("the image reader thread terminated unexpectedly")]
    ReaderLost,
}

/// Phrase the too-small message so the offset only appears when there is one,
/// and so the number that does not fit is named rather than left to arithmetic.
fn too_small_message(path: &std::path::Path, image: u64, offset: u64, dest: u64) -> String {
    use crate::bmap::human_size;
    if offset == 0 {
        format!(
            "image is {} ({image} bytes) but destination '{}' only holds {} ({dest} bytes)",
            human_size(image),
            path.display(),
            human_size(dest),
        )
    } else {
        format!(
            "image is {} written at offset {}, needing {} ({} bytes), but destination '{}' \
             only holds {} ({dest} bytes)",
            human_size(image),
            human_size(offset),
            human_size(image.saturating_add(offset)),
            image.saturating_add(offset),
            path.display(),
            human_size(dest),
        )
    }
}

impl Error {
    /// Build an [`Error::Io`] from an operation name, a path and an OS error.
    pub fn io(op: &'static str, path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io { op, path: path.into(), source }
    }

    /// Build an [`Error::InvalidArgument`].
    pub fn invalid(what: &'static str, reason: impl Into<String>) -> Self {
        Self::InvalidArgument { what, reason: reason.into() }
    }
}
