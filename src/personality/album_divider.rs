//! Per-album phase divider printed above the active progress bar during
//! multi-album syncs.
//!
//! When `--friendly` is on and more than one album/smart-folder runs, each
//! completed pass prints a ✓ line in scrollback with the album name,
//! download count, and elapsed time. The currently-active album is shown
//! by the bar's own `wide_msg` line — the divider only prints done lines.
//!
//! Off mode is a strict no-op: no lines printed, no allocations.
//!
//! Rendering output (friendly, three-album sync post-completion):
//! ```text
//!   ✓ Family             1,204 / 1,204  2m 04s
//!   ✓ Travel                          50 / 50    38s
//!   ✓ Pets                             30 / 30    12s
//! ```
//!
//! Done lines are printed atomically via `active_bar::with_suspended`
//! so tracing WARN/ERROR events can't interleave between lines.

use std::time::Duration;

use crate::personality::active_bar;
use crate::personality::Mode;

/// Renders a short human-readable elapsed duration for the per-album done
/// line. Fixed-width columns (`NNs` or `Nm NNs` or `Nh NNm`).
fn format_album_elapsed(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs:>2}s")
    } else {
        let minutes = secs / 60;
        let remainder = secs % 60;
        if minutes < 60 {
            format!("{minutes:>2}m {remainder:02}s")
        } else {
            let hours = minutes / 60;
            let min = minutes % 60;
            format!("{hours:>2}h {min:02}m")
        }
    }
}

/// Tracks completed album passes and prints ✓ lines in scrollback.
///
/// Constructed with the ordered pass labels for column-alignment sizing.
/// Each `mark_done()` call prints one done line above the active bar.
/// `finish()` prints a blank separator after all passes.
///
/// Off mode / single-pass: all methods are no-ops.
#[derive(Debug)]
pub struct AlbumDivider {
    mode: Mode,
    /// Max label width (char count) for column alignment.
    label_width: usize,
}

impl AlbumDivider {
    /// Build a divider for the ordered `(label, asset_count)` pairs.
    /// Returns a no-op divider for single-pass or off-mode plans.
    #[must_use]
    pub fn new(mode: Mode, pass_labels_and_counts: &[(&str, u64)]) -> Self {
        let label_width = if mode.is_friendly() && pass_labels_and_counts.len() > 1 {
            pass_labels_and_counts
                .iter()
                .map(|(label, _)| label.chars().count())
                .max()
                .unwrap_or(0)
        } else {
            0
        };
        Self { mode, label_width }
    }

    /// Print a done line for the just-completed album pass.
    pub fn mark_done(&self, label: &str, count: u64, total: u64, elapsed: Duration) {
        if !self.mode.is_friendly() || self.label_width == 0 {
            return;
        }
        let elapsed_str = format_album_elapsed(elapsed);
        let line = format!(
            "  \u{2713} {label:<width$} {count:>4} / {total:<4}  {elapsed_str}",
            width = self.label_width,
        );
        active_bar::with_suspended(|| {
            let mut stderr = std::io::stderr();
            let _ = std::io::Write::write(&mut stderr, line.as_bytes());
            let _ = std::io::Write::write(&mut stderr, b"\n");
        });
    }

    /// Blank separator after all passes, before the summary card.
    pub fn finish(&self) {
        if !self.mode.is_friendly() || self.label_width == 0 {
            return;
        }
        active_bar::with_suspended(|| {
            let _ = std::io::Write::write(&mut std::io::stderr(), b"\n");
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_mode_is_noop() {
        let d = AlbumDivider::new(Mode::Off, &[("Family", 100), ("Travel", 50)]);
        assert_eq!(d.label_width, 0);
    }

    #[test]
    fn single_pass_is_noop() {
        let d = AlbumDivider::new(Mode::Friendly, &[("Family", 100)]);
        assert_eq!(d.label_width, 0);
    }

    #[test]
    fn empty_passes_is_noop() {
        let d = AlbumDivider::new(Mode::Friendly, &[]);
        assert_eq!(d.label_width, 0);
    }

    #[test]
    fn label_width_is_max_chars() {
        let d = AlbumDivider::new(
            Mode::Friendly,
            &[("a", 1), ("very long label", 2), ("mid", 3)],
        );
        assert_eq!(d.label_width, 15);
    }

    #[test]
    fn format_album_elapsed_under_minute() {
        assert_eq!(format_album_elapsed(Duration::from_secs(0)), " 0s");
        assert_eq!(format_album_elapsed(Duration::from_secs(5)), " 5s");
        assert_eq!(format_album_elapsed(Duration::from_secs(59)), "59s");
    }

    #[test]
    fn format_album_elapsed_minutes_and_seconds() {
        assert_eq!(format_album_elapsed(Duration::from_secs(60)), " 1m 00s");
        assert_eq!(format_album_elapsed(Duration::from_secs(125)), " 2m 05s");
        assert_eq!(format_album_elapsed(Duration::from_secs(3599)), "59m 59s");
    }

    #[test]
    fn format_album_elapsed_hours_and_minutes() {
        assert_eq!(format_album_elapsed(Duration::from_secs(3600)), " 1h 00m");
        assert_eq!(format_album_elapsed(Duration::from_secs(7320)), " 2h 02m");
    }
}
