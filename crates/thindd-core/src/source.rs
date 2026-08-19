//! Image input: a seekable file, or a forward-only stream.
//!
//! bmap ranges are always ascending, and the zero scanner walks the image front
//! to back, so forward-only access is enough for every operation `copy`
//! performs. That is what lets an image be piped in on stdin.

use crate::{
    decompress::{self, Compression, DecompressMode},
    error::{Error, Result},
};
use std::{
    fmt,
    fs::File,
    io::Read,
    os::unix::fs::FileTypeExt,
    path::{Path, PathBuf},
};

/// Scratch buffer size used when skipping forward in a non-seekable stream.
const SKIP_CHUNK: usize = 1024 * 1024;

/// Where image bytes come from.
pub enum ImageSource {
    /// A regular file or block device, opened for reading.
    Seekable {
        /// The open file.
        file: File,
        /// Path it was opened from.
        path: PathBuf,
        /// Size in bytes.
        size: u64,
        /// Current read position.
        pos: u64,
    },
    /// A pipe, stdin, or a decompressor — anything that cannot seek.
    Stream {
        /// The reader.
        reader: Box<dyn Read + Send>,
        /// Display path (`-` for stdin).
        path: PathBuf,
        /// Current read position, in *decoded* bytes.
        pos: u64,
        /// The container the bytes were decoded from, for diagnostics.
        compression: Compression,
    },
}

impl fmt::Debug for ImageSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Seekable { path, size, pos, .. } => f
                .debug_struct("ImageSource::Seekable")
                .field("path", path)
                .field("size", size)
                .field("pos", pos)
                .finish(),
            Self::Stream { path, pos, .. } => {
                f.debug_struct("ImageSource::Stream").field("path", path).field("pos", pos).finish()
            }
        }
    }
}

impl ImageSource {
    /// Open `path` for reading, treating its bytes as a raw image.
    ///
    /// Equivalent to [`ImageSource::open_auto`] with [`DecompressMode::Never`].
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_auto(path, DecompressMode::Never)
    }

    /// Open `path` for reading, transparently decompressing when `mode` says so
    /// and the file's magic bytes agree.
    ///
    /// A raw image is opened seekable, so holes can be discovered and mapped
    /// ranges jumped to. A compressed one necessarily becomes a forward-only
    /// stream. Block devices are sized with `lseek`, since `stat` reports zero
    /// for them.
    ///
    /// The kernel is told to expect sequential access, which keeps a
    /// multi-gigabyte flash from evicting the rest of the system's page cache.
    pub fn open_auto(path: &Path, mode: DecompressMode) -> Result<Self> {
        let file = File::open(path).map_err(|e| Error::io("open image", path, e))?;
        let meta = file.metadata().map_err(|e| Error::io("stat image", path, e))?;

        advise_sequential(&file);

        let compression = match mode {
            DecompressMode::Never => Compression::None,
            other => {
                let mut head = [0u8; decompress::SNIFF_LEN];
                let read = read_head(&file, &mut head, path)?;
                rustix::fs::seek(&file, rustix::fs::SeekFrom::Start(0))
                    .map_err(|e| Error::io("rewind image", path, e.into()))?;
                decompress::resolve(other, &head[..read])
            }
        };

        if compression != Compression::None {
            tracing::debug!(path = %path.display(), %compression, "decoding image stream");
            let reader = decompress::decode(Box::new(file), compression)?;
            return Ok(Self::Stream { reader, path: path.to_path_buf(), pos: 0, compression });
        }

        let size = if meta.file_type().is_block_device() {
            let end = rustix::fs::seek(&file, rustix::fs::SeekFrom::End(0))
                .map_err(|e| Error::io("size block device", path, e.into()))?;
            rustix::fs::seek(&file, rustix::fs::SeekFrom::Start(0))
                .map_err(|e| Error::io("rewind block device", path, e.into()))?;
            end
        } else {
            meta.len()
        };

        Ok(Self::Seekable { file, path: path.to_path_buf(), size, pos: 0 })
    }

    /// Wrap an arbitrary reader (stdin, a socket) and read it as a raw image.
    #[must_use]
    pub fn from_reader(reader: Box<dyn Read + Send>, path: impl Into<PathBuf>) -> Self {
        Self::Stream { reader, path: path.into(), pos: 0, compression: Compression::None }
    }

    /// Wrap an arbitrary reader, decompressing it when `mode` says so and the
    /// leading bytes agree.
    ///
    /// Sniffing consumes the first few bytes; they are pushed back in front of
    /// the reader, so the decoder — or the raw path — still sees a complete
    /// stream.
    pub fn from_reader_auto(
        mut reader: Box<dyn Read + Send>,
        path: impl Into<PathBuf>,
        mode: DecompressMode,
    ) -> Result<Self> {
        let path = path.into();
        if mode == DecompressMode::Never {
            return Ok(Self::from_reader(reader, path));
        }

        let mut head = [0u8; decompress::SNIFF_LEN];
        let mut filled = 0usize;
        while filled < head.len() {
            match reader.read(&mut head[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(Error::io("read image", &path, e)),
            }
        }

        let compression = decompress::resolve(mode, &head[..filled]);
        let rewound: Box<dyn Read + Send> =
            Box::new(std::io::Cursor::new(head[..filled].to_vec()).chain(reader));
        if compression != Compression::None {
            tracing::debug!(path = %path.display(), %compression, "decoding image stream");
        }
        let reader = decompress::decode(rewound, compression)?;
        Ok(Self::Stream { reader, path, pos: 0, compression })
    }

    /// The container the image bytes were decoded from.
    #[must_use]
    pub const fn compression(&self) -> Compression {
        match self {
            Self::Seekable { .. } => Compression::None,
            Self::Stream { compression, .. } => *compression,
        }
    }

    /// Path the image came from, for diagnostics.
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::Seekable { path, .. } | Self::Stream { path, .. } => path,
        }
    }

    /// Image size, when it is known up front.
    #[must_use]
    pub const fn size(&self) -> Option<u64> {
        match self {
            Self::Seekable { size, .. } => Some(*size),
            Self::Stream { .. } => None,
        }
    }

    /// Current read position.
    #[must_use]
    pub const fn position(&self) -> u64 {
        match self {
            Self::Seekable { pos, .. } | Self::Stream { pos, .. } => *pos,
        }
    }

    /// Borrow the underlying file, if this source is seekable.
    #[must_use]
    pub const fn as_file(&self) -> Option<&File> {
        match self {
            Self::Seekable { file, .. } => Some(file),
            Self::Stream { .. } => None,
        }
    }

    /// Advance to absolute byte `offset`.
    ///
    /// Seekable sources jump; streams read and discard. Seeking backwards is an
    /// error — nothing in this crate needs it.
    pub fn skip_to(&mut self, offset: u64) -> Result<()> {
        if offset < self.position() {
            return Err(Error::NotSeekable { op: "rewind", path: self.path().to_path_buf() });
        }
        match self {
            Self::Seekable { file, path, pos, .. } => {
                if offset != *pos {
                    rustix::fs::seek(&*file, rustix::fs::SeekFrom::Start(offset))
                        .map_err(|e| Error::io("lseek image", &*path, e.into()))?;
                    *pos = offset;
                }
                Ok(())
            }
            Self::Stream { reader, path, pos, .. } => {
                let mut scratch = vec![0u8; SKIP_CHUNK];
                while *pos < offset {
                    let want = usize::try_from(offset - *pos).unwrap_or(SKIP_CHUNK).min(SKIP_CHUNK);
                    let n = reader
                        .read(&mut scratch[..want])
                        .map_err(|e| Error::io("read image", &*path, e))?;
                    if n == 0 {
                        return Err(Error::ShortImage {
                            path: path.clone(),
                            read: *pos,
                            expected: offset,
                        });
                    }
                    *pos += n as u64;
                }
                Ok(())
            }
        }
    }

    /// Fill `buf` completely, returning fewer bytes only at end of image.
    pub fn read_full(&mut self, buf: &mut [u8]) -> Result<usize> {
        let (reader, path, pos): (&mut dyn Read, &Path, &mut u64) = match self {
            Self::Seekable { file, path, pos, .. } => (file, path, pos),
            Self::Stream { reader, path, pos, .. } => (reader, path, pos),
        };

        let mut filled = 0usize;
        while filled < buf.len() {
            match reader.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(Error::io("read image", path, e)),
            }
        }
        *pos += filled as u64;
        Ok(filled)
    }

    /// Tell the kernel the pages just read will not be needed again.
    ///
    /// Called by the copy engine after each batch so that flashing a large
    /// image does not push everything else out of the page cache.
    #[cfg_attr(
        not(target_os = "linux"),
        allow(
            clippy::missing_const_for_fn,
            reason = "const only where the advice compiles away to nothing; the public                       signature must not differ between platforms"
        )
    )]
    pub fn drop_cache_before(&self, offset: u64) {
        if let Self::Seekable { file, .. } = self {
            advise_dropped(file, offset);
        }
    }
}

/// Tell the kernel we will read this file front to back.
///
/// Advisory in every sense: it only ever costs performance, and only Linux has
/// `posix_fadvise` reachable through `rustix`. macOS would need `F_RDAHEAD`
/// through a raw `fcntl`; the read pattern is sequential enough that its
/// heuristics get there on their own.
#[cfg(target_os = "linux")]
fn advise_sequential(file: &File) {
    let _ = rustix::fs::fadvise(file, 0, None, rustix::fs::Advice::Sequential);
}

/// See the Linux version above.
#[cfg(not(target_os = "linux"))]
const fn advise_sequential(_file: &File) {}

/// Tell the kernel the first `offset` bytes will not be read again, so a
/// multi-gigabyte flash does not evict everything else from the page cache.
#[cfg(target_os = "linux")]
fn advise_dropped(file: &File, offset: u64) {
    if let Some(len) = std::num::NonZeroU64::new(offset) {
        let _ = rustix::fs::fadvise(file, 0, Some(len), rustix::fs::Advice::DontNeed);
    }
}

/// See the Linux version above.
#[cfg(not(target_os = "linux"))]
const fn advise_dropped(_file: &File, _offset: u64) {}

/// Read up to `buf.len()` bytes from the head of `file`.
fn read_head(mut file: &File, buf: &mut [u8], path: &Path) -> Result<usize> {
    let mut filled = 0usize;
    while filled < buf.len() {
        match file.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(Error::io("read image header", path, e)),
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn seekable_source_reports_size_and_reads() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(b"0123456789").unwrap();
        f.flush().unwrap();

        let mut src = ImageSource::open(f.path()).unwrap();
        assert_eq!(src.size(), Some(10));
        src.skip_to(4).unwrap();
        let mut buf = [0u8; 3];
        assert_eq!(src.read_full(&mut buf).unwrap(), 3);
        assert_eq!(&buf, b"456");
        assert_eq!(src.position(), 7);
    }

    #[test]
    fn stream_source_skips_by_reading() {
        let data = (0u8..=255).collect::<Vec<_>>();
        let mut src = ImageSource::from_reader(Box::new(std::io::Cursor::new(data)), "-");
        assert_eq!(src.size(), None);
        src.skip_to(200).unwrap();
        let mut buf = [0u8; 4];
        assert_eq!(src.read_full(&mut buf).unwrap(), 4);
        assert_eq!(buf, [200, 201, 202, 203]);
    }

    #[test]
    fn read_full_returns_short_at_eof() {
        let mut src = ImageSource::from_reader(Box::new(std::io::Cursor::new(vec![7u8; 5])), "-");
        let mut buf = [0u8; 16];
        assert_eq!(src.read_full(&mut buf).unwrap(), 5);
        assert_eq!(src.read_full(&mut buf).unwrap(), 0);
    }

    #[test]
    fn rewinding_is_rejected() {
        let mut src = ImageSource::from_reader(Box::new(std::io::Cursor::new(vec![0u8; 32])), "-");
        src.skip_to(16).unwrap();
        assert!(matches!(src.skip_to(8), Err(Error::NotSeekable { .. })));
    }
}
