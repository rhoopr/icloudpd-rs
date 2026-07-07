//! Friendly-mode final summary renderer.
//!
//! Prints a short cycle outcome after the progress card clears. Off mode is
//! a strict no-op: the existing `log_sync_summary` tracing events still fire
//! so journals and `kei status` consumers see byte-identical output.
//!
//! The summary is friendly-only by design (per the personality plan's hard
//! constraint that personality strings stay out of machine outputs). The data
//! comes entirely from existing `SyncStats` fields plus `state::SyncSummary`
//! library totals; no new state-DB queries are introduced beyond the one this
//! module reads.

use std::time::Duration;

use crate::personality::Mode;

/// Format byte counts for friendly-mode renderers. Integer at >= 10 of a
/// unit, one decimal below 10, so 412 GB and 8.4 GB read distinctly
/// without 412.0 GB looking awkward. 1024-based units displayed as `GB`
/// to match the UX mock.
#[allow(
    clippy::cast_precision_loss,
    reason = "display-only byte formatting; precision loss at exabyte scale is fine"
)]
pub(crate) fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1_024;
    const MB: u64 = KB * 1_024;
    const GB: u64 = MB * 1_024;
    const TB: u64 = GB * 1_024;
    if bytes >= 10 * TB {
        format!("{} TB", bytes / TB)
    } else if bytes >= TB {
        format!("{:.1} TB", bytes as f64 / TB as f64)
    } else if bytes >= 10 * GB {
        format!("{} GB", bytes / GB)
    } else if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{} MB", bytes / MB)
    } else if bytes >= KB {
        format!("{} KB", bytes / KB)
    } else {
        format!("{bytes} B")
    }
}

/// Inputs for the final friendly summary. All fields are already on
/// `SyncStats` / `state::SyncSummary`; the wrapper exists so the renderer
/// doesn't reach into either type directly (keeps the personality module a
/// leaf).
#[derive(Debug, Clone)]
pub(crate) struct FinalSummary {
    pub(crate) downloaded: u64,
    pub(crate) skipped_total: u64,
    pub(crate) failed: u64,
    pub(crate) elapsed: Duration,
    pub(crate) library_totals: Option<LibraryTotals>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct LibraryTotals {
    pub(crate) files: u64,
    pub(crate) bytes: u64,
}

impl FinalSummary {
    /// Print the summary to stderr in friendly mode. Off mode is a no-op.
    pub fn print_to_stderr(&self, mode: Mode) {
        if !mode.is_friendly() {
            return;
        }
        for line in self.render_lines() {
            crate::personality::active_bar::println_above_bars(&line);
        }
    }

    /// Render the summary as a vector of lines. Pulled out for testing so
    /// the assertions don't have to interact with indicatif.
    pub fn render_lines(&self) -> Vec<String> {
        let mut lines = Vec::with_capacity(2);
        lines.push(format!(
            "Downloaded {} {}, skipped {}, failed {} in {}.",
            format_count(self.downloaded),
            plural(self.downloaded, "file", "files"),
            format_count(self.skipped_total),
            format_count(self.failed),
            format_duration(self.elapsed),
        ));
        if let Some(totals) = self.library_totals {
            lines.push(format!(
                "Library now has {} {}, {}.",
                format_count(totals.files),
                plural(totals.files, "file", "files"),
                format_bytes(totals.bytes),
            ));
        }
        lines
    }
}

fn format_count(n: u64) -> String {
    // Insert thousands separators so `53,209` is legible. Builds the
    // string from the right because `n.to_string()` is the only stable
    // path; chunking by 3 matches the standard locale-free format.
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        let m = secs / 60;
        let s = secs % 60;
        if s == 0 {
            format!("{m}m")
        } else {
            format!("{m}m{s:02}s")
        }
    } else {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        let s = secs % 60;
        if s == 0 {
            format!("{h}h{m:02}m")
        } else {
            format!("{h}h{m:02}m{s:02}s")
        }
    }
}

fn plural<'a>(n: u64, singular: &'a str, plural: &'a str) -> &'a str {
    if n == 1 { singular } else { plural }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_summary() -> FinalSummary {
        FinalSummary {
            downloaded: 1_291,
            skipped_total: 12,
            failed: 0,
            elapsed: Duration::from_secs(4 * 60 + 12),
            library_totals: Some(LibraryTotals {
                files: 53_209,
                bytes: 412_u64 * 1024 * 1024 * 1024,
            }),
        }
    }

    #[test]
    fn format_bytes_uses_integer_at_or_above_ten_units() {
        assert_eq!(format_bytes(412_u64 * 1024 * 1024 * 1024), "412 GB");
        assert_eq!(
            format_bytes(8_u64 * 1024 * 1024 * 1024 + 400 * 1024 * 1024),
            "8.4 GB"
        );
        assert_eq!(format_bytes(500 * 1024 * 1024), "500 MB");
        assert_eq!(format_bytes(1024), "1 KB");
        assert_eq!(format_bytes(0), "0 B");
    }

    #[test]
    fn render_lines_is_short_final_summary() {
        let lines = sample_summary().render_lines();
        assert_eq!(
            lines,
            vec![
                "Downloaded 1,291 files, skipped 12, failed 0 in 4m12s.",
                "Library now has 53,209 files, 412 GB.",
            ],
        );
    }

    #[test]
    fn render_lines_handles_singular_file_and_missing_library_totals() {
        let mut summary = sample_summary();
        summary.downloaded = 1;
        summary.skipped_total = 0;
        summary.failed = 1;
        summary.elapsed = Duration::from_secs(1);
        summary.library_totals = None;
        assert_eq!(
            summary.render_lines(),
            vec!["Downloaded 1 file, skipped 0, failed 1 in 1s."],
        );
    }
}
