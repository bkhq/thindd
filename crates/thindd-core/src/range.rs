//! Inclusive block ranges — the unit the bmap format is expressed in.

use std::fmt;

/// An inclusive range of image blocks, `[first, last]`.
///
/// Both ends are block numbers, not byte offsets. A single-block range has
/// `first == last`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct BlockRange {
    /// First block of the range.
    pub first: u64,
    /// Last block of the range, inclusive.
    pub last: u64,
}

impl BlockRange {
    /// Create a range, returning `None` when `last < first`.
    #[must_use]
    pub const fn new(first: u64, last: u64) -> Option<Self> {
        if last < first { None } else { Some(Self { first, last }) }
    }

    /// Number of blocks covered by this range.
    #[must_use]
    pub const fn count(self) -> u64 {
        self.last - self.first + 1
    }

    /// Byte offset of the first block.
    #[must_use]
    pub const fn start_byte(self, block_size: u64) -> u64 {
        self.first * block_size
    }

    /// Byte offset one past the last block (may exceed the image size for the
    /// final, partially filled block — callers clamp against the image size).
    #[must_use]
    pub const fn end_byte(self, block_size: u64) -> u64 {
        (self.last + 1) * block_size
    }

    /// `true` when `other` starts exactly where `self` ends.
    #[must_use]
    pub const fn is_adjacent_to(self, other: Self) -> bool {
        self.last + 1 == other.first
    }
}

impl fmt::Debug for BlockRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}..={}", self.first, self.last)
    }
}

impl fmt::Display for BlockRange {
    /// Renders the range the way the bmap format spells it: `"7"` for a single
    /// block, `"7-19"` for a run.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.first == self.last {
            write!(f, "{}", self.first)
        } else {
            write!(f, "{}-{}", self.first, self.last)
        }
    }
}

/// A block range together with the optional digest of its contents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MappedRange {
    /// The blocks themselves.
    pub range: BlockRange,
    /// Lower-case hex digest of the range's bytes, when the bmap carries one.
    pub checksum: Option<String>,
}

impl MappedRange {
    /// Create a range with no checksum attached.
    #[must_use]
    pub const fn bare(range: BlockRange) -> Self {
        Self { range, checksum: None }
    }
}

/// Merge overlapping and adjacent byte ranges into a minimal sorted set.
///
/// Input ranges are half-open `[start, end)` byte offsets and must already be
/// sorted by `start`.
pub(crate) fn coalesce_byte_ranges(ranges: &mut Vec<(u64, u64)>) {
    if ranges.len() < 2 {
        return;
    }
    let mut write = 0usize;
    for read in 1..ranges.len() {
        let (cur_start, cur_end) = ranges[read];
        let (_, prev_end) = ranges[write];
        if cur_start <= prev_end {
            ranges[write].1 = prev_end.max(cur_end);
        } else {
            write += 1;
            ranges[write] = (cur_start, cur_end);
        }
    }
    ranges.truncate(write + 1);
}

/// Convert half-open byte ranges into inclusive block ranges, rounding
/// outwards so that no byte of a range is ever dropped.
pub(crate) fn byte_ranges_to_blocks(ranges: &[(u64, u64)], block_size: u64) -> Vec<BlockRange> {
    let mut out: Vec<BlockRange> = Vec::with_capacity(ranges.len());
    for &(start, end) in ranges {
        if end <= start {
            continue;
        }
        let first = start / block_size;
        let last = (end - 1) / block_size;
        match out.last_mut() {
            Some(prev) if first <= prev.last + 1 => prev.last = prev.last.max(last),
            _ => out.push(BlockRange { first, last }),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_matches_bmap_spelling() {
        assert_eq!(BlockRange { first: 7, last: 7 }.to_string(), "7");
        assert_eq!(BlockRange { first: 7, last: 19 }.to_string(), "7-19");
    }

    #[test]
    fn new_rejects_inverted_ranges() {
        assert!(BlockRange::new(5, 4).is_none());
        assert_eq!(BlockRange::new(4, 4).unwrap().count(), 1);
    }

    #[test]
    fn coalesce_merges_touching_and_overlapping() {
        let mut r = vec![(0, 10), (10, 20), (25, 30), (28, 40)];
        coalesce_byte_ranges(&mut r);
        assert_eq!(r, vec![(0, 20), (25, 40)]);
    }

    #[test]
    fn byte_ranges_round_outwards() {
        // 4096-byte blocks: [100, 5000) touches blocks 0 and 1.
        let blocks = byte_ranges_to_blocks(&[(100, 5000)], 4096);
        assert_eq!(blocks, vec![BlockRange { first: 0, last: 1 }]);
    }

    #[test]
    fn byte_ranges_merge_adjacent_blocks() {
        let blocks = byte_ranges_to_blocks(&[(0, 4096), (4096, 8192), (16384, 20480)], 4096);
        assert_eq!(
            blocks,
            vec![BlockRange { first: 0, last: 1 }, BlockRange { first: 4, last: 4 }]
        );
    }
}
