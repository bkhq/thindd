//! All-zero detection.
//!
//! This is the hot loop of the whole tool: every byte of the image passes
//! through it. The implementation deliberately compares slices against a static
//! zero page instead of iterating bytes — slice equality on `[u8]` lowers to
//! `memcmp`, which the C library implements with vector instructions and which
//! bails out on the first differing byte. In practice this runs at memory
//! bandwidth, an order of magnitude faster than a hand-written byte loop, and
//! it needs no `unsafe`.

/// Size of the static comparison window.
const ZERO_CHUNK: usize = 4096;

/// A page of zeroes to compare against.
static ZEROS: [u8; ZERO_CHUNK] = [0; ZERO_CHUNK];

/// Returns `true` when every byte of `buf` is zero.
///
/// An empty slice counts as zero.
///
/// ```
/// # use thindd_core::zero::is_zero;
/// assert!(is_zero(&[0u8; 8192]));
/// assert!(!is_zero(&[0, 0, 1, 0]));
/// assert!(is_zero(&[]));
/// ```
#[must_use]
pub fn is_zero(buf: &[u8]) -> bool {
    let mut rest = buf;
    while rest.len() >= ZERO_CHUNK {
        let (head, tail) = rest.split_at(ZERO_CHUNK);
        if head != &ZEROS[..] {
            return false;
        }
        rest = tail;
    }
    rest == &ZEROS[..rest.len()]
}

/// A half-open byte span inside a scan buffer, plus what it contains.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    /// Offset of the span relative to the start of the buffer.
    pub offset: usize,
    /// Length of the span in bytes.
    pub len: usize,
    /// `true` when every byte in the span is zero.
    pub zero: bool,
}

/// Split `buf` into maximal runs of blocks that are either entirely zero or
/// not, appending the result to `out`.
///
/// Classification happens at `block_size` granularity because that is the
/// granularity the bmap format can express. A trailing partial block is
/// classified on the bytes it actually has.
///
/// `out` is cleared first, and its allocation is reused across calls — the
/// copy engine calls this once per 8 MiB batch.
///
/// ```
/// # use thindd_core::zero::{classify_blocks, Span};
/// let mut buf = vec![0u8; 4096 * 3];
/// buf[4096] = 0xff; // block 1 carries data
/// let mut spans = Vec::new();
/// classify_blocks(&buf, 4096, &mut spans);
/// assert_eq!(spans, vec![
///     Span { offset: 0,    len: 4096, zero: true },
///     Span { offset: 4096, len: 4096, zero: false },
///     Span { offset: 8192, len: 4096, zero: true },
/// ]);
/// ```
pub fn classify_blocks(buf: &[u8], block_size: usize, out: &mut Vec<Span>) {
    out.clear();
    if buf.is_empty() {
        return;
    }

    // Fast path: whole batches are very often uniformly zero (a hole-heavy
    // image) or uniformly non-zero (a packed rootfs). One memcmp settles it.
    if is_zero(buf) {
        out.push(Span { offset: 0, len: buf.len(), zero: true });
        return;
    }

    let mut offset = 0usize;
    let mut run_start = 0usize;
    let mut run_zero: Option<bool> = None;

    while offset < buf.len() {
        let len = block_size.min(buf.len() - offset);
        let zero = is_zero(&buf[offset..offset + len]);
        match run_zero {
            Some(prev) if prev == zero => {}
            Some(prev) => {
                out.push(Span { offset: run_start, len: offset - run_start, zero: prev });
                run_start = offset;
            }
            None => run_start = offset,
        }
        run_zero = Some(zero);
        offset += len;
    }

    if let Some(zero) = run_zero {
        out.push(Span { offset: run_start, len: buf.len() - run_start, zero });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_zero_buffer_is_one_span() {
        let mut spans = Vec::new();
        classify_blocks(&vec![0u8; 4096 * 4], 4096, &mut spans);
        assert_eq!(spans, vec![Span { offset: 0, len: 16384, zero: true }]);
    }

    #[test]
    fn all_data_buffer_is_one_span() {
        let mut spans = Vec::new();
        classify_blocks(&vec![7u8; 4096 * 4], 4096, &mut spans);
        assert_eq!(spans, vec![Span { offset: 0, len: 16384, zero: false }]);
    }

    #[test]
    fn trailing_partial_block_is_classified() {
        let mut buf = vec![0u8; 4096 + 10];
        buf[4100] = 1;
        let mut spans = Vec::new();
        classify_blocks(&buf, 4096, &mut spans);
        assert_eq!(
            spans,
            vec![
                Span { offset: 0, len: 4096, zero: true },
                Span { offset: 4096, len: 10, zero: false },
            ]
        );
    }

    #[test]
    fn a_single_nonzero_byte_marks_its_block() {
        for pos in [0usize, 1, 2047, 4095] {
            let mut buf = vec![0u8; 4096];
            buf[pos] = 1;
            assert!(!is_zero(&buf), "byte at {pos} not detected");
        }
    }

    #[test]
    fn spans_cover_the_whole_buffer() {
        let mut buf = vec![0u8; 4096 * 5 + 7];
        buf[4096 * 2] = 9;
        buf[4096 * 5] = 9;
        let mut spans = Vec::new();
        classify_blocks(&buf, 4096, &mut spans);
        let mut next = 0;
        for s in &spans {
            assert_eq!(s.offset, next);
            next += s.len;
        }
        assert_eq!(next, buf.len());
    }
}
