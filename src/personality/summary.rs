//! Friendly-mode summary card and post-card recap renderer.
//!
//! Replaces the plain `── Summary ──` block in friendly mode with a
//! multi-line card carrying the same data in a more legible shape, plus
//! up to three "what's new" lines below it. Off mode is a strict no-op:
//! the existing `log_sync_summary` tracing events still fire so journals
//! and `kei status` consumers see byte-identical output.
//!
//! The card and the recap are friendly-only by design (per the personality
//! plan's hard constraint that personality strings stay out of machine
//! outputs). The data they render comes entirely from existing
//! `SyncStats` fields plus `state::SyncSummary` library totals; no new
//! state-DB queries are introduced beyond the one this module reads.
//!
//! Render order downstream of `sync_loop::run_cycle`:
//!
//! 1. Per-phase ✓ narration (auth / list / downloaded / verified)
//! 2. `SummaryCard` block (this module)
//! 3. `recap_lines` block (this module; suppressed on no-op cycles)
//! 4. Existing `signoff_to_stderr` closing flourish

use std::time::Duration;

use crate::download::recap::RunRecap;
use crate::personality::format::format_bytes;
use crate::personality::Mode;

/// Inputs for the summary card. All fields are already on `SyncStats` /
/// `state::SyncSummary`; the wrapper exists so the renderer doesn't
/// reach into either type directly (keeps the personality module a leaf).
#[derive(Debug, Clone)]
pub struct SummaryCard {
    pub photos_new: u64,
    pub videos_new: u64,
    pub skipped_total: u64,
    pub skipped_already_present: u64,
    pub failed: u64,
    pub elapsed: Duration,
    pub bytes_downloaded: u64,
    pub library_total_assets: u64,
    pub library_total_bytes: u64,
}

impl SummaryCard {
    /// Print the card to stderr in friendly mode. Off mode is a no-op.
    pub fn print_to_stderr(&self, mode: Mode) {
        if !mode.is_friendly() {
            return;
        }
        for line in self.render_lines() {
            crate::personality::active_bar::println_above_bars(&line);
        }
    }

    /// Render the card as a vector of lines. Pulled out for testing so
    /// the assertions don't have to interact with indicatif.
    pub fn render_lines(&self) -> Vec<String> {
        let mut lines = Vec::with_capacity(8);
        lines.push("─── kei ────────────────────────────────────".to_string());
        lines.push(format!(
            "   New      {}",
            format_new(self.photos_new, self.videos_new)
        ));
        if self.skipped_total > 0 {
            let reason = if self.skipped_already_present == self.skipped_total {
                "already present".to_string()
            } else if self.skipped_already_present > 0 {
                format!(
                    "{} already present, {} other",
                    self.skipped_already_present,
                    self.skipped_total - self.skipped_already_present
                )
            } else {
                "filtered".to_string()
            };
            lines.push(format!(
                "   Skipped  {} ({})",
                format_count(self.skipped_total),
                reason
            ));
        } else {
            lines.push("   Skipped  0".to_string());
        }
        lines.push(format!("   Failed   {}", format_count(self.failed)));
        lines.push(format!("   Time     {}", format_duration(self.elapsed)));
        lines.push(format!(
            "   Speed    {}",
            format_speed(self.bytes_downloaded, self.elapsed)
        ));
        lines.push(format!(
            "   Library  {} items · {}",
            format_count(self.library_total_assets),
            format_bytes(self.library_total_bytes)
        ));
        lines.push("────────────────────────────────────────────".to_string());
        lines
    }
}

/// Render the post-card recap lines. Returns an empty vector when the
/// recap is empty (no successful downloads this cycle), so the caller
/// can pass the result straight to a `for line in ...` loop without
/// guarding.
#[must_use]
pub fn recap_lines(recap: &RunRecap) -> Vec<String> {
    if recap.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(3);
    if let Some(asset) = &recap.biggest {
        out.push(format!(
            "   Biggest grab  {} ({})",
            asset.filename,
            format_bytes(asset.bytes)
        ));
    }
    if let Some(asset) = &recap.oldest {
        out.push(format!(
            "   Oldest find   {} ({})",
            asset.filename,
            asset.created_local.format("%Y-%m-%d")
        ));
    }
    if let Some((label, asset)) = recap.top_album() {
        out.push(format!(
            "   Newest album  {} (latest capture {})",
            label,
            asset.created_local.format("%Y-%m-%d")
        ));
    }
    out
}

/// Print the recap to stderr in friendly mode. Off mode is a no-op.
pub fn print_recap_to_stderr(mode: Mode, recap: &RunRecap) {
    if !mode.is_friendly() {
        return;
    }
    for line in recap_lines(recap) {
        crate::personality::active_bar::println_above_bars(&line);
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

fn format_new(photos: u64, videos: u64) -> String {
    match (photos, videos) {
        (0, 0) => "0 new".to_string(),
        (p, 0) => format!("{} photo{}", format_count(p), if p == 1 { "" } else { "s" }),
        (0, v) => format!("{} video{}", format_count(v), if v == 1 { "" } else { "s" }),
        (p, v) => format!(
            "{} photo{} · {} video{}",
            format_count(p),
            if p == 1 { "" } else { "s" },
            format_count(v),
            if v == 1 { "" } else { "s" }
        ),
    }
}

fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        let m = secs / 60;
        let s = secs % 60;
        format!("{m}m {s:02}s")
    } else {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        let s = secs % 60;
        format!("{h}h {m:02}m {s:02}s")
    }
}

#[allow(
    clippy::cast_precision_loss,
    reason = "display-only speed arithmetic; precision loss at exabyte/sec is fine"
)]
fn format_speed(bytes: u64, elapsed: Duration) -> String {
    let secs = elapsed.as_secs_f64();
    if secs <= 0.0 || bytes == 0 {
        return "0 B/s".to_string();
    }
    let per_sec = bytes as f64 / secs;
    if per_sec >= 1_073_741_824.0 {
        format!("{:.1} GB/s", per_sec / 1_073_741_824.0)
    } else if per_sec >= 1_048_576.0 {
        format!("{:.1} MB/s", per_sec / 1_048_576.0)
    } else if per_sec >= 1024.0 {
        format!("{:.1} KB/s", per_sec / 1024.0)
    } else {
        format!("{per_sec:.0} B/s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download::recap::{RecapAsset, RunRecap};
    use chrono::TimeZone;

    fn sample_card() -> SummaryCard {
        SummaryCard {
            photos_new: 1_204,
            videos_new: 87,
            skipped_total: 12,
            skipped_already_present: 12,
            failed: 0,
            elapsed: Duration::from_secs(4 * 60 + 12),
            bytes_downloaded: (24 * 1024 * 1024 / 10) * (4 * 60 + 12) + (3 * 1024 * 1024),
            library_total_assets: 53_209,
            library_total_bytes: 412_u64 * 1024 * 1024 * 1024,
        }
    }

    #[test]
    fn render_lines_includes_top_and_bottom_borders() {
        let lines = sample_card().render_lines();
        assert!(lines.first().unwrap().starts_with("─── kei "));
        assert!(lines.last().unwrap().starts_with("───────"));
    }

    #[test]
    fn render_lines_contains_photos_dot_videos_split() {
        let lines = sample_card().render_lines();
        let body = lines.join("\n");
        assert!(
            body.contains("1,204 photos · 87 videos"),
            "expected photos · videos split with thousands separator; got:\n{body}"
        );
    }

    #[test]
    fn render_lines_handles_photos_only() {
        let mut card = sample_card();
        card.videos_new = 0;
        let body = card.render_lines().join("\n");
        assert!(body.contains("New      1,204 photos"));
        assert!(!body.contains("videos"));
    }

    #[test]
    fn render_lines_handles_singular_photo() {
        let mut card = sample_card();
        card.photos_new = 1;
        card.videos_new = 0;
        let body = card.render_lines().join("\n");
        assert!(body.contains("1 photo"));
        assert!(!body.contains("photos"));
    }

    #[test]
    fn render_lines_skipped_breakdown_when_some_other() {
        let mut card = sample_card();
        card.skipped_total = 20;
        card.skipped_already_present = 12;
        let body = card.render_lines().join("\n");
        assert!(body.contains("12 already present, 8 other"));
    }

    #[test]
    fn render_lines_speed_zero_for_zero_elapsed() {
        let mut card = sample_card();
        card.elapsed = Duration::ZERO;
        let body = card.render_lines().join("\n");
        assert!(body.contains("Speed    0 B/s"));
    }

    #[test]
    fn render_lines_library_uses_thousands_separator() {
        let body = sample_card().render_lines().join("\n");
        assert!(body.contains("Library  53,209 items · 412 GB"));
    }

    #[test]
    fn recap_lines_empty_when_recap_empty() {
        let recap = RunRecap::default();
        assert!(recap_lines(&recap).is_empty());
    }

    fn rec_asset(name: &str, bytes: u64, year: i32) -> RecapAsset {
        RecapAsset {
            filename: name.to_string(),
            bytes,
            created_local: chrono::Local
                .with_ymd_and_hms(year, 6, 15, 12, 0, 0)
                .unwrap(),
        }
    }

    #[test]
    fn recap_lines_render_three_highlights() {
        let mut recap = RunRecap::default();
        recap.observe("Family", rec_asset("a.jpg", 100, 2010));
        recap.observe(
            "Travel",
            rec_asset("italy_2024.MOV", 2_400 * 1024 * 1024, 2024),
        );
        recap.observe("Family", rec_asset("ancient.jpg", 50, 1998));
        let lines = recap_lines(&recap);
        assert_eq!(lines.len(), 3, "biggest + oldest + newest_album expected");
        let body = lines.join("\n");
        assert!(body.contains("Biggest grab  italy_2024.MOV"));
        assert!(body.contains("Oldest find   ancient.jpg (1998-06-15)"));
        assert!(body.contains("Newest album  Travel (latest capture 2024-06-15)"));
    }
}
