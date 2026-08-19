//! The bmap file format: parse, verify and render.
//!
//! The format is an XML document listing the blocks of an image that hold
//! useful data. This module implements version 2.0 (what upstream `bmaptool`
//! writes today) for both reading and writing, and additionally reads the
//! legacy 1.x layouts.
//!
//! Two details make the format slightly unusual and are worth calling out:
//!
//! * the file carries a digest **of itself**, computed with the digest field
//!   overwritten by ASCII `'0'` characters. [`Bmap::render`] and
//!   [`Bmap::parse`] both implement that convention, so files round-trip
//!   between this crate and upstream unchanged.
//! * ranges are inclusive and expressed in blocks, spelled `"12"` for a single
//!   block and `"12-40"` for a run.

use crate::{
    checksum::ChecksumKind,
    error::{Error, Result},
    range::{BlockRange, MappedRange},
};
use std::path::{Path, PathBuf};

/// The bmap format version this crate writes.
pub const BMAP_FORMAT_VERSION: (u32, u32) = (2, 0);

/// Highest major version this crate can read.
const MAX_SUPPORTED_MAJOR: u32 = 2;

/// A parsed (or freshly built) block map.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bmap {
    /// Format version as `(major, minor)`.
    pub version: (u32, u32),
    /// Size of the image the map describes, in bytes.
    pub image_size: u64,
    /// Size of one block, in bytes.
    pub block_size: u64,
    /// Total number of blocks in the image.
    pub blocks_cnt: u64,
    /// Number of blocks that have to be copied.
    pub mapped_blocks_cnt: u64,
    /// Digest algorithm used by this file, if any (1.0-1.2 files carry none).
    pub checksum_kind: Option<ChecksumKind>,
    /// The mapped ranges, in ascending block order.
    pub ranges: Vec<MappedRange>,
}

impl Bmap {
    /// Blocks-to-copy expressed in bytes, the way upstream reports it
    /// (`mapped blocks × block size`, so the trailing partial block counts in
    /// full).
    #[must_use]
    pub const fn mapped_size(&self) -> u64 {
        self.mapped_blocks_cnt * self.block_size
    }

    /// Exact number of image bytes covered by the mapped ranges, with the final
    /// partial block clamped against [`Self::image_size`].
    #[must_use]
    pub fn mapped_bytes(&self) -> u64 {
        self.ranges
            .iter()
            .map(|r| {
                let start = r.range.start_byte(self.block_size).min(self.image_size);
                let end = r.range.end_byte(self.block_size).min(self.image_size);
                end - start
            })
            .sum()
    }

    /// Fraction of the image that has to be copied, in percent.
    #[must_use]
    #[expect(
        clippy::cast_precision_loss,
        reason = "display-only percentage; f64 covers block counts far beyond any real image"
    )]
    pub fn mapped_percent(&self) -> f64 {
        if self.blocks_cnt == 0 {
            return 0.0;
        }
        self.mapped_blocks_cnt as f64 * 100.0 / self.blocks_cnt as f64
    }

    /// Parse a bmap file and verify the digest it carries of itself.
    ///
    /// `path` is only used to build good error messages.
    pub fn parse(text: &str, path: &Path) -> Result<Self> {
        let bad = |reason: String| Error::BmapParse { path: path.to_path_buf(), reason };

        let doc = roxmltree::Document::parse(text)
            .map_err(|e| bad(format!("not well-formed XML: {e}")))?;
        let root = doc.root_element();
        if root.tag_name().name() != "bmap" {
            return Err(bad(format!(
                "root element is <{}>, expected <bmap>",
                root.tag_name().name()
            )));
        }

        let version_str = root
            .attribute("version")
            .ok_or_else(|| bad("<bmap> has no 'version' attribute".to_owned()))?
            .trim();
        let version = parse_version(version_str)
            .ok_or_else(|| bad(format!("malformed version '{version_str}'")))?;
        if version.0 > MAX_SUPPORTED_MAJOR {
            return Err(Error::UnsupportedBmapVersion { version: version_str.to_owned() });
        }

        let text_of = |name: &str| -> Result<String> {
            root.children()
                .find(|n| n.is_element() && n.tag_name().name() == name)
                .and_then(|n| n.text())
                .map(|t| t.trim().to_owned())
                .ok_or_else(|| bad(format!("missing <{name}> element")))
        };
        let u64_of = |name: &str| -> Result<u64> {
            let raw = text_of(name)?;
            raw.parse::<u64>().map_err(|e| bad(format!("<{name}> is not a number ('{raw}'): {e}")))
        };

        let image_size = u64_of("ImageSize")?;
        let block_size = u64_of("BlockSize")?;
        let blocks_cnt = u64_of("BlocksCount")?;
        let mapped_blocks_cnt = u64_of("MappedBlocksCount")?;

        if block_size == 0 {
            return Err(bad("<BlockSize> is zero".to_owned()));
        }
        let expected_blocks = image_size.div_ceil(block_size);
        if blocks_cnt != expected_blocks {
            return Err(bad(format!(
                "inconsistent bmap: image size {image_size} over block size {block_size} \
                 needs {expected_blocks} blocks, but <BlocksCount> says {blocks_cnt}"
            )));
        }

        // Version 1.3 spelled everything "sha1"; 1.4 (a misnumbered 2.0) and
        // later use a named algorithm. Anything older carries no digests.
        let (checksum_kind, range_attr, file_element) =
            if version.0 > 1 || (version.0 == 1 && version.1 >= 4) {
                (
                    Some(ChecksumKind::from_name(&text_of("ChecksumType")?)?),
                    "chksum",
                    Some("BmapFileChecksum"),
                )
            } else if version == (1, 3) {
                (Some(ChecksumKind::Sha1), "sha1", Some("BmapFileSHA1"))
            } else {
                (None, "chksum", None)
            };

        if let (Some(kind), Some(element)) = (checksum_kind, file_element) {
            let expected = text_of(element)?;
            verify_self_digest(text, &expected, kind, path)?;
        }

        let block_map = root
            .children()
            .find(|n| n.is_element() && n.tag_name().name() == "BlockMap")
            .ok_or_else(|| bad("missing <BlockMap> element".to_owned()))?;

        let mut ranges = Vec::new();
        for node in block_map.children().filter(roxmltree::Node::is_element) {
            if node.tag_name().name() != "Range" {
                continue;
            }
            let raw = node.text().unwrap_or_default().trim();
            let range = parse_range(raw).ok_or_else(|| bad(format!("bad block range '{raw}'")))?;
            if range.last >= blocks_cnt {
                return Err(bad(format!(
                    "range '{raw}' runs past the end of the image ({blocks_cnt} blocks)"
                )));
            }
            ranges.push(MappedRange {
                range,
                checksum: node.attribute(range_attr).map(|c| c.trim().to_ascii_lowercase()),
            });
        }

        let counted: u64 = ranges.iter().map(|r| r.range.count()).sum();
        if counted != mapped_blocks_cnt {
            return Err(bad(format!(
                "<MappedBlocksCount> says {mapped_blocks_cnt} but the ranges cover {counted} blocks"
            )));
        }

        Ok(Self {
            version,
            image_size,
            block_size,
            blocks_cnt,
            mapped_blocks_cnt,
            checksum_kind,
            ranges,
        })
    }

    /// Read and parse a bmap file from disk.
    pub fn from_file(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).map_err(|e| Error::io("read bmap file", path, e))?;
        Self::parse(&text, path)
    }

    /// Render the map as a bmap 2.0 XML document, digest included.
    ///
    /// The output is accepted by upstream `bmaptool copy` unchanged.
    #[must_use]
    pub fn render(&self) -> String {
        let kind = self.checksum_kind.unwrap_or_default();
        let placeholder = "0".repeat(kind.hex_len());
        let body = self.render_with_digest(&placeholder, kind);
        let digest = kind.digest(body.as_bytes());
        // The placeholder is the only run of `hex_len` ASCII zeroes we emit, so
        // re-rendering with the real digest is equivalent to substituting it in
        // place — and is immune to the digest happening to contain the
        // placeholder as a substring.
        self.render_with_digest(&digest, kind)
    }

    fn render_with_digest(&self, digest: &str, kind: ChecksumKind) -> String {
        let ranges: String = self
            .ranges
            .iter()
            .map(|r| {
                r.checksum.as_ref().map_or_else(
                    || format!("        <Range> {} </Range>\n", r.range),
                    |c| format!("        <Range chksum=\"{c}\"> {} </Range>\n", r.range),
                )
            })
            .collect();

        format!(
            "<?xml version=\"1.0\" ?>\n\
             <!-- This file is the block map (bmap) of an image file: the list of blocks\n\
             \x20    that actually carry data and therefore have to be written to the target\n\
             \x20    device. Every other block is either a file-system hole or a run of zero\n\
             \x20    bytes, and copying it would be wasted I/O.\n\
             \n\
             \x20    The format is bmap 2.0, the same one the Yocto Project's bmaptool uses,\n\
             \x20    so both tools can read this file. The 'version' attribute is spelled\n\
             \x20    'major.minor'; the major number changes on incompatible edits. -->\n\
             \n\
             <bmap version=\"{major}.{minor}\">\n\
             \x20   <!-- Image size in bytes: {image_human} -->\n\
             \x20   <ImageSize> {image_size} </ImageSize>\n\
             \n\
             \x20   <!-- Size of a block in bytes -->\n\
             \x20   <BlockSize> {block_size} </BlockSize>\n\
             \n\
             \x20   <!-- Count of blocks in the image file -->\n\
             \x20   <BlocksCount> {blocks_cnt} </BlocksCount>\n\
             \n\
             \x20   <!-- Count of mapped blocks: {mapped_human} or {mapped_percent:.1}% -->\n\
             \x20   <MappedBlocksCount> {mapped_cnt} </MappedBlocksCount>\n\
             \n\
             \x20   <!-- Type of checksum used in this file -->\n\
             \x20   <ChecksumType> {kind} </ChecksumType>\n\
             \n\
             \x20   <!-- Checksum of this bmap file. It is computed over the file with this\n\
             \x20        field overwritten by ASCII '0' characters. -->\n\
             \x20   <BmapFileChecksum> {digest} </BmapFileChecksum>\n\
             \n\
             \x20   <!-- The block map itself. Each element is either a single block or an\n\
             \x20        inclusive range of blocks. The 'chksum' attribute, when present, is\n\
             \x20        the checksum of that range's contents. -->\n\
             \x20   <BlockMap>\n\
             {ranges}\
             \x20   </BlockMap>\n\
             </bmap>\n",
            major = BMAP_FORMAT_VERSION.0,
            minor = BMAP_FORMAT_VERSION.1,
            image_human = human_size(self.image_size),
            image_size = self.image_size,
            block_size = self.block_size,
            blocks_cnt = self.blocks_cnt,
            mapped_human = human_size(self.mapped_size()),
            mapped_percent = self.mapped_percent(),
            mapped_cnt = self.mapped_blocks_cnt,
        )
    }

    /// Write the rendered map to `path`.
    pub fn write_to(&self, path: &Path) -> Result<()> {
        std::fs::write(path, self.render()).map_err(|e| Error::io("write bmap file", path, e))
    }
}

/// Recompute the bmap file's own digest and compare it against the value the
/// file records for itself.
///
/// The digest is computed over the file with its own digest field replaced by
/// ASCII `'0'` characters, which is the convention upstream `bmaptool`
/// established and the only way a file can carry a checksum of itself.
fn verify_self_digest(text: &str, recorded: &str, kind: ChecksumKind, path: &Path) -> Result<()> {
    let recorded = recorded.trim();
    let expected = recorded.to_ascii_lowercase();
    if expected.len() != kind.hex_len() {
        return Err(Error::BmapParse {
            path: path.to_path_buf(),
            reason: format!(
                "self-checksum is {} characters long, expected {} for {kind}",
                expected.len(),
                kind.hex_len()
            ),
        });
    }

    // Blank the digest where it literally appears in the file. Upstream does
    // the same first-occurrence search, so files agree byte for byte.
    let Some(pos) = text.find(recorded) else {
        return Err(Error::BmapParse {
            path: path.to_path_buf(),
            reason: "the recorded self-checksum does not occur verbatim in the file".to_owned(),
        });
    };

    let mut blanked = Vec::with_capacity(text.len());
    blanked.extend_from_slice(&text.as_bytes()[..pos]);
    blanked.extend(std::iter::repeat_n(b'0', recorded.len()));
    blanked.extend_from_slice(&text.as_bytes()[pos + recorded.len()..]);

    let actual = kind.digest(&blanked);
    if actual == expected {
        Ok(())
    } else {
        Err(Error::BmapChecksum { path: path.to_path_buf(), expected, actual })
    }
}

fn parse_version(s: &str) -> Option<(u32, u32)> {
    let (major, minor) = s.split_once('.')?;
    Some((major.trim().parse().ok()?, minor.trim().parse().ok()?))
}

fn parse_range(s: &str) -> Option<BlockRange> {
    if let Some((a, b)) = s.split_once('-') {
        BlockRange::new(a.trim().parse().ok()?, b.trim().parse().ok()?)
    } else {
        let only = s.trim().parse().ok()?;
        BlockRange::new(only, only)
    }
}

/// Format a byte count the way bmap files and upstream `bmaptool` spell it.
///
/// ```
/// # use thindd_core::human_size;
/// assert_eq!(human_size(1), "1 byte");
/// assert_eq!(human_size(500), "500 bytes");
/// assert_eq!(human_size(4 * 1024 * 1024), "4.0 MiB");
/// ```
#[must_use]
#[expect(clippy::cast_precision_loss, reason = "human-readable output, one decimal digit")]
pub fn human_size(size: u64) -> String {
    if size == 1 {
        return "1 byte".to_owned();
    }
    if size < 512 {
        return format!("{size} bytes");
    }
    let mut value = size as f64;
    for unit in ["KiB", "MiB", "GiB", "TiB", "PiB"] {
        value /= 1024.0;
        if value < 1024.0 {
            return format!("{value:.1} {unit}");
        }
    }
    value /= 1024.0;
    format!("{value:.1} EiB")
}

/// Format a duration the way upstream `bmaptool` reports elapsed time.
///
/// ```
/// # use thindd_core::bmap::human_time;
/// # use std::time::Duration;
/// assert_eq!(human_time(Duration::from_secs(3671)), "1h 1m 11.0s");
/// assert_eq!(human_time(Duration::from_millis(1500)), "1.5s");
/// ```
#[must_use]
pub fn human_time(d: std::time::Duration) -> String {
    let total = d.as_secs_f64();
    let hours = (total / 3600.0).floor();
    let minutes = ((total - hours * 3600.0) / 60.0).floor();
    let seconds = total - hours * 3600.0 - minutes * 60.0;
    if hours > 0.0 {
        format!("{hours:.0}h {minutes:.0}m {seconds:.1}s")
    } else if minutes > 0.0 {
        format!("{minutes:.0}m {seconds:.1}s")
    } else {
        format!("{seconds:.1}s")
    }
}

/// Path a bmap file is looked for at, given an image path: `image.wic` →
/// `image.wic.bmap`.
#[must_use]
pub fn default_bmap_path(image: &Path) -> PathBuf {
    let mut p = image.as_os_str().to_os_string();
    p.push(".bmap");
    PathBuf::from(p)
}

/// Extensions that mark a compressed image. A map generated for `image.wic`
/// stays valid after the image is compressed, so `image.wic.gz` should find
/// `image.wic.bmap` as well as `image.wic.gz.bmap`.
const COMPRESSED_EXTENSIONS: &[&str] = &["gz", "gzip"];

/// Every path worth looking for a sibling bmap file at, most specific first.
///
/// ```
/// # use thindd_core::bmap::bmap_candidates;
/// # use std::path::{Path, PathBuf};
/// assert_eq!(
///     bmap_candidates(Path::new("core.wic.gz")),
///     vec![PathBuf::from("core.wic.gz.bmap"), PathBuf::from("core.wic.bmap")],
/// );
/// assert_eq!(bmap_candidates(Path::new("core.wic")), vec![PathBuf::from("core.wic.bmap")]);
/// ```
#[must_use]
pub fn bmap_candidates(image: &Path) -> Vec<PathBuf> {
    let mut out = vec![default_bmap_path(image)];
    let compressed = image
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| COMPRESSED_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()));
    if compressed {
        let stem = image.with_extension("");
        if stem.file_name().is_some() {
            out.push(default_bmap_path(&stem));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Bmap {
        Bmap {
            version: BMAP_FORMAT_VERSION,
            image_size: 4096 * 10 - 100,
            block_size: 4096,
            blocks_cnt: 10,
            mapped_blocks_cnt: 4,
            checksum_kind: Some(ChecksumKind::Sha256),
            ranges: vec![
                MappedRange {
                    range: BlockRange { first: 0, last: 1 },
                    checksum: Some(ChecksumKind::Sha256.digest(b"a")),
                },
                MappedRange {
                    range: BlockRange { first: 7, last: 8 },
                    checksum: Some(ChecksumKind::Sha256.digest(b"b")),
                },
            ],
        }
    }

    #[test]
    fn render_parse_round_trip() {
        let original = sample();
        let text = original.render();
        let parsed = Bmap::parse(&text, Path::new("test.bmap")).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn rendered_file_verifies_its_own_checksum() {
        let text = sample().render();
        // Parsing performs the verification; a tampered file must be rejected.
        assert!(Bmap::parse(&text, Path::new("t.bmap")).is_ok());
        let tampered = text.replace("<BlockSize> 4096 ", "<BlockSize> 8192 ");
        assert!(matches!(
            Bmap::parse(&tampered, Path::new("t.bmap")),
            Err(Error::BmapChecksum { .. } | Error::BmapParse { .. })
        ));
    }

    #[test]
    fn single_block_ranges_render_without_a_dash() {
        let mut b = sample();
        b.ranges = vec![MappedRange::bare(BlockRange { first: 3, last: 3 })];
        b.mapped_blocks_cnt = 1;
        assert!(b.render().contains("<Range> 3 </Range>"));
    }

    #[test]
    fn mapped_bytes_clamps_the_trailing_partial_block() {
        let b = sample();
        // Blocks 0-1 are whole (8192 bytes); blocks 7-8 are whole too, since
        // the image ends inside block 9.
        assert_eq!(b.mapped_bytes(), 4 * 4096);
        assert_eq!(b.mapped_size(), 4 * 4096);
    }

    #[test]
    fn mapped_bytes_clamps_a_mapped_final_block() {
        let mut b = sample();
        b.ranges = vec![MappedRange::bare(BlockRange { first: 9, last: 9 })];
        b.mapped_blocks_cnt = 1;
        assert_eq!(b.mapped_bytes(), 4096 - 100);
        assert_eq!(b.mapped_size(), 4096);
    }

    #[test]
    fn inconsistent_block_count_is_rejected() {
        let text = sample().render().replace("<BlocksCount> 10 ", "<BlocksCount> 11 ");
        assert!(matches!(Bmap::parse(&text, Path::new("t.bmap")), Err(Error::BmapParse { .. })));
    }

    #[test]
    fn future_major_version_is_rejected() {
        let text = sample().render().replace("<bmap version=\"2.0\">", "<bmap version=\"3.0\">");
        assert!(matches!(
            Bmap::parse(&text, Path::new("t.bmap")),
            Err(Error::UnsupportedBmapVersion { .. })
        ));
    }

    #[test]
    fn default_bmap_path_appends_the_suffix() {
        assert_eq!(default_bmap_path(Path::new("/x/core.wic")), PathBuf::from("/x/core.wic.bmap"));
    }

    #[test]
    fn a_compressed_image_also_looks_for_the_uncompressed_map() {
        assert_eq!(
            bmap_candidates(Path::new("/x/core.wic.gz")),
            vec![PathBuf::from("/x/core.wic.gz.bmap"), PathBuf::from("/x/core.wic.bmap"),]
        );
        // Case-insensitive, and a bare name without a directory still works.
        assert_eq!(bmap_candidates(Path::new("core.wic.GZ")).len(), 2);
        // Nothing to strip.
        assert_eq!(bmap_candidates(Path::new("/x/core.wic")).len(), 1);
        // A lone ".gz" has no stem to fall back to.
        assert_eq!(bmap_candidates(Path::new(".gz")).len(), 1);
    }

    #[test]
    fn human_size_matches_upstream_spelling() {
        assert_eq!(human_size(0), "0 bytes");
        assert_eq!(human_size(1), "1 byte");
        assert_eq!(human_size(511), "511 bytes");
        assert_eq!(human_size(512), "0.5 KiB");
        assert_eq!(human_size(1024), "1.0 KiB");
        assert_eq!(human_size(1024 * 1024 * 1024), "1.0 GiB");
    }
}
