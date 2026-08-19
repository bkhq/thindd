//! Command-line surface.

use clap::{ArgAction, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;
use thindd_core::{ChecksumKind, DecompressMode, DetectMode, ZeroMode};

/// The better `dd` for embedded images.
///
/// Copies only the parts of an image that actually carry data: file-system
/// holes and all-zero regions are never written.
#[derive(Debug, Parser)]
#[command(name = "thindd", version, about, long_about = None, propagate_version = true)]
pub(crate) struct Cli {
    /// Increase log verbosity. Repeat for more (`-vv` enables trace).
    #[arg(short, long, action = ArgAction::Count, global = true)]
    pub(crate) verbose: u8,

    /// Silence everything except errors.
    #[arg(short, long, global = true, conflicts_with = "verbose")]
    pub(crate) quiet: bool,

    /// Emit logs as JSON instead of human-readable lines.
    #[arg(long, global = true)]
    pub(crate) log_json: bool,

    /// Do not render a progress bar even on a terminal.
    #[arg(long, global = true)]
    pub(crate) no_progress: bool,

    #[command(subcommand)]
    pub(crate) command: Command,
}

/// Subcommands.
#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Copy an image to a file or flash it to a block device.
    Copy(CopyArgs),
    /// Generate a bmap file for an image.
    Create(CreateArgs),
    /// Describe a bmap file, or what a bmap for an image would look like.
    Info(InfoArgs),
}

/// `thindd copy`
#[derive(Debug, Parser)]
#[expect(clippy::struct_excessive_bools, reason = "these are command-line flags, not state")]
pub(crate) struct CopyArgs {
    /// Image to copy. `-` reads from standard input.
    pub(crate) image: PathBuf,

    /// Destination file or block device.
    pub(crate) dest: PathBuf,

    /// Use this bmap file.
    ///
    /// Defaults to `<IMAGE>.bmap` when it exists. For a compressed image the
    /// uncompressed name is tried too: `core.wic.gz` finds `core.wic.bmap`.
    #[arg(long, value_name = "FILE", conflicts_with = "no_bmap")]
    pub(crate) bmap: Option<PathBuf>,

    /// Ignore any bmap file and discover what to copy by scanning the image.
    #[arg(long)]
    pub(crate) no_bmap: bool,

    /// What may be skipped.
    #[arg(long, value_enum, default_value_t = Detect::Both)]
    pub(crate) detect: Detect,

    /// What to do with the regions the image does not cover.
    ///
    /// `skip` leaves them as they are, which is what upstream `bmaptool` does
    /// and what keeps a flash fast. `zero` makes them read back as zero.
    #[arg(long, value_enum, default_value_t = Mode::Skip)]
    pub(crate) mode: Mode,

    /// Transparent decompression of the image.
    ///
    /// `auto` looks at the leading bytes, so a compressed image is recognised
    /// whatever it is called — including one arriving on standard input.
    #[arg(long, value_enum, default_value_t = Decompress::Auto)]
    pub(crate) decompress: Decompress,

    /// Do not verify the per-range checksums recorded in the bmap file.
    #[arg(long)]
    pub(crate) no_verify: bool,

    /// Do not flush the destination to stable storage before exiting.
    #[arg(long)]
    pub(crate) no_sync: bool,

    /// Start writing at this byte offset on the destination.
    ///
    /// `dd`'s `seek=`, except in bytes rather than blocks: `--seek 8K` puts the
    /// image at 8192. This is how a bootloader lands at the offset the boot ROM looks
    /// for it at, and how an image lands inside an existing partition.
    ///
    /// A non-zero offset makes the copy a partial update: a regular-file
    /// destination is extended if it is too short, but never truncated, so
    /// whatever follows the image is left alone.
    #[arg(long, value_name = "BYTES", value_parser = parse_size, default_value = "0")]
    pub(crate) seek: u64,

    /// Clear the whole destination before copying.
    ///
    /// Destroys everything on the device, including any partition that lives
    /// past the end of the image — use `--mode zero` instead if you need to
    /// keep one. In exchange it is the only option that removes a stale GPT
    /// backup header or an old file-system superblock left behind by a
    /// previous, differently sized image.
    ///
    /// Near-free where the controller implements write-zeroes or discard; where
    /// it does not, it writes zeroes over the whole device, which scales with
    /// the card rather than with the image.
    #[arg(long)]
    pub(crate) wipe: bool,

    /// Write to a block device even if the kernel says it is in use.
    ///
    /// Without this, the device is opened `O_EXCL`, so a mounted disk (your
    /// running system, for instance) simply cannot be overwritten.
    #[arg(long)]
    pub(crate) force: bool,

    /// Block size to assume when there is no bmap file.
    #[arg(long, value_name = "BYTES", value_parser = parse_size)]
    pub(crate) block_size: Option<u64>,

    /// Size of each read and each write, `dd`'s `bs=`.
    ///
    /// Larger is usually faster up to a point; on slow removable media the
    /// gains flatten out around 4-8 MiB. Not to be confused with
    /// `--block-size`, which is the granularity zeroes are detected at.
    #[arg(long, value_name = "BYTES", value_parser = parse_size, default_value = "8M")]
    pub(crate) bs: u64,

    /// Number of batches in flight between the reader and the writer.
    #[arg(long, value_name = "N", default_value_t = thindd_core::DEFAULT_QUEUE_DEPTH)]
    pub(crate) queue_depth: usize,

    /// Flush the destination every this many written bytes. `0` disables it.
    ///
    /// The flush is a real one — `fsync` on Linux, `F_FULLFSYNC` on macOS, which
    /// pushes the data past the drive's own cache. Lower it if you want the
    /// card to be safe to pull sooner after the progress bar stops; raise it,
    /// or set `0`, if the repeated flushing is costing you throughput.
    #[arg(long, value_name = "BYTES", value_parser = parse_size, default_value = "16M")]
    pub(crate) sync_every: u64,
}

/// `thindd create`
#[derive(Debug, Parser)]
pub(crate) struct CreateArgs {
    /// Image to map.
    pub(crate) image: PathBuf,

    /// Where to write the bmap file. `-` writes to standard output.
    /// Defaults to `<IMAGE>.bmap`.
    #[arg(short, long, value_name = "FILE")]
    pub(crate) output: Option<PathBuf>,

    /// What counts as skippable.
    #[arg(long, value_enum, default_value_t = Detect::Both)]
    pub(crate) detect: Detect,

    /// Digest for per-range checksums.
    #[arg(long, value_enum, default_value_t = Checksum::Sha256)]
    pub(crate) checksum: Checksum,

    /// Transparent decompression. The map then describes the *decompressed*
    /// image, which is what gets written to the device.
    #[arg(long, value_enum, default_value_t = Decompress::Auto)]
    pub(crate) decompress: Decompress,

    /// Block size. Defaults to the file system's preferred size.
    #[arg(long, value_name = "BYTES", value_parser = parse_size)]
    pub(crate) block_size: Option<u64>,

    /// Size of each read, `dd`'s `bs=`.
    #[arg(long, value_name = "BYTES", value_parser = parse_size, default_value = "8M")]
    pub(crate) bs: u64,
}

/// `thindd info`
#[derive(Debug, Parser)]
pub(crate) struct InfoArgs {
    /// A bmap file to describe.
    pub(crate) bmap: PathBuf,

    /// Also list every mapped range.
    #[arg(long)]
    pub(crate) ranges: bool,
}

/// CLI spelling of [`DetectMode`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum Detect {
    /// Skip file-system holes only — what upstream `bmaptool` does.
    Holes,
    /// Skip all-zero blocks only.
    Zeros,
    /// Skip both. The default.
    Both,
    /// Skip nothing; copy the image in full.
    None,
}

impl From<Detect> for DetectMode {
    fn from(value: Detect) -> Self {
        match value {
            Detect::Holes => Self::Holes,
            Detect::Zeros => Self::Zeros,
            Detect::Both => Self::Both,
            Detect::None => Self::None,
        }
    }
}

/// CLI spelling of [`ZeroMode`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum Mode {
    /// Leave the regions the image does not cover untouched. The default.
    Skip,
    /// Make them read back as zero, within the image's extent.
    Zero,
}

impl From<Mode> for ZeroMode {
    fn from(value: Mode) -> Self {
        match value {
            Mode::Skip => Self::Skip,
            Mode::Zero => Self::Zero,
        }
    }
}

/// CLI spelling of [`DecompressMode`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum Decompress {
    /// Detect the container from the image's magic bytes. The default.
    Auto,
    /// Never decompress; treat the bytes as a raw image.
    None,
    /// Force gzip decoding even without a recognisable header.
    Gzip,
}

impl From<Decompress> for DecompressMode {
    fn from(value: Decompress) -> Self {
        match value {
            Decompress::Auto => Self::Auto,
            Decompress::None => Self::Never,
            Decompress::Gzip => Self::Gzip,
        }
    }
}

/// CLI spelling of [`ChecksumKind`], plus "no checksums at all".
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum Checksum {
    /// SHA-1. Only for interoperating with very old bmap files.
    Sha1,
    /// SHA-256. The default.
    Sha256,
    /// SHA-512.
    Sha512,
    /// Omit per-range checksums. Roughly halves creation time.
    None,
}

impl From<Checksum> for Option<ChecksumKind> {
    fn from(value: Checksum) -> Self {
        match value {
            Checksum::Sha1 => Some(ChecksumKind::Sha1),
            Checksum::Sha256 => Some(ChecksumKind::Sha256),
            Checksum::Sha512 => Some(ChecksumKind::Sha512),
            Checksum::None => None,
        }
    }
}

/// Parse a byte count, with an optional binary suffix: `4096`, `8K`, `16M`,
/// `1G`. `KiB`/`MiB`/`GiB` spellings are accepted too.
fn parse_size(raw: &str) -> Result<u64, String> {
    let s = raw.trim();
    let digits_end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    if digits_end == 0 {
        return Err(format!("'{raw}' does not start with a number"));
    }
    let (number, suffix) = s.split_at(digits_end);
    let value: u64 = number.parse().map_err(|e| format!("'{number}' is not a number: {e}"))?;

    let multiplier: u64 = match suffix.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1024 * 1024 * 1024,
        other => return Err(format!("unknown size suffix '{other}'")),
    };

    value.checked_mul(multiplier).ok_or_else(|| format!("'{raw}' overflows a 64-bit byte count"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory as _;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn sizes_parse_with_and_without_suffixes() {
        assert_eq!(parse_size("4096"), Ok(4096));
        assert_eq!(parse_size("8K"), Ok(8192));
        assert_eq!(parse_size("16MiB"), Ok(16 * 1024 * 1024));
        assert_eq!(parse_size("2g"), Ok(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_size("0"), Ok(0));
    }

    #[test]
    fn bad_sizes_are_rejected() {
        assert!(parse_size("M").is_err());
        assert!(parse_size("12x").is_err());
        assert!(parse_size("99999999999999999999G").is_err());
    }

    #[test]
    fn copy_defaults_match_the_documented_behaviour() {
        let cli = Cli::parse_from(["thindd", "copy", "a.img", "/dev/null"]);
        let Command::Copy(args) = cli.command else { panic!("expected copy") };
        assert_eq!(args.detect, Detect::Both);
        assert_eq!(args.mode, Mode::Skip);
        assert_eq!(args.seek, 0);
        assert_eq!(args.decompress, Decompress::Auto);
        assert!(!args.no_verify);
        assert_eq!(args.bs, 8 * 1024 * 1024);
    }

    #[test]
    fn seek_accepts_byte_suffixes() {
        let cli = Cli::parse_from(["thindd", "copy", "--seek", "8K", "a.img", "/dev/null"]);
        let Command::Copy(args) = cli.command else { panic!("expected copy") };
        assert_eq!(args.seek, 8192);
    }

    #[test]
    fn bmap_and_no_bmap_conflict() {
        assert!(
            Cli::try_parse_from(["thindd", "copy", "--no-bmap", "--bmap", "x", "a", "b"]).is_err()
        );
    }
}
