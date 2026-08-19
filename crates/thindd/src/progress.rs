//! Terminal progress reporting.

use indicatif::{ProgressBar, ProgressStyle};
use std::sync::atomic::{AtomicU64, Ordering};
use thindd_core::progress::Progress;

/// A [`Progress`] backed by an `indicatif` bar.
///
/// The bar tracks *image* bytes processed, not bytes written: that is the
/// quantity whose total is known up front, and it makes the ETA meaningful even
/// when 90% of the image is being skipped.
#[derive(Debug)]
pub(crate) struct BarProgress {
    bar: ProgressBar,
    written: AtomicU64,
}

impl BarProgress {
    /// Create a bar. Pass `enabled = false` for a hidden, zero-cost bar.
    #[must_use]
    pub(crate) fn new(enabled: bool) -> Self {
        let bar = if enabled { ProgressBar::no_length() } else { ProgressBar::hidden() };
        Self { bar, written: AtomicU64::new(0) }
    }
}

impl Progress for BarProgress {
    #[expect(
        clippy::literal_string_with_formatting_args,
        reason = "these braces are indicatif template placeholders, not Rust format args"
    )]
    fn set_total(&self, total_bytes: Option<u64>) {
        let template = if total_bytes.is_some() {
            "{spinner} [{elapsed_precise}] [{bar:32}] {bytes}/{total_bytes} \
             ({bytes_per_sec}, eta {eta}) {msg}"
        } else {
            "{spinner} [{elapsed_precise}] {bytes} ({bytes_per_sec}) {msg}"
        };
        if let Ok(style) = ProgressStyle::with_template(template) {
            self.bar.set_style(style.progress_chars("=> "));
        }
        match total_bytes {
            Some(total) => self.bar.set_length(total),
            None => self.bar.unset_length(),
        }
        self.bar.enable_steady_tick(std::time::Duration::from_millis(120));
    }

    fn advance(&self, processed: u64, written: u64) {
        self.bar.inc(processed);
        let total_written = self.written.fetch_add(written, Ordering::Relaxed) + written;
        if processed > written {
            self.bar
                .set_message(format!("written {}", thindd_core::bmap::human_size(total_written)));
        }
    }

    fn finish(&self) {
        self.bar.finish_and_clear();
    }
}
