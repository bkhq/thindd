//! Progress reporting hook.
//!
//! The core stays free of any terminal handling: it just calls into this trait,
//! and the CLI supplies an implementation backed by a progress bar.

use std::fmt::Debug;

/// Receives progress notifications from a running copy or bmap creation.
///
/// Implementations are called from the writer thread and must be cheap;
/// anything expensive belongs behind a rate limiter inside the implementation.
pub trait Progress: Send + Sync + Debug {
    /// Announce the total amount of image bytes that will be processed.
    /// Called once, before any [`Progress::advance`] call. `None` means the
    /// total is not known up front (a streamed image).
    fn set_total(&self, total_bytes: Option<u64>);

    /// Report that `processed` more image bytes have been dealt with, of which
    /// `written` were actually sent to the destination.
    fn advance(&self, processed: u64, written: u64);

    /// The operation has ended. Called exactly once, including on failure.
    fn finish(&self);
}

/// A [`Progress`] implementation that discards everything.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoProgress;

impl Progress for NoProgress {
    fn set_total(&self, _total_bytes: Option<u64>) {}
    fn advance(&self, _processed: u64, _written: u64) {}
    fn finish(&self) {}
}
