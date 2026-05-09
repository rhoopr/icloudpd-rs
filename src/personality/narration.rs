//! Friendly narration lines printed above the active progress bar.
//!
//! Tracing in friendly mode is filtered to WARN+, so successful sync events
//! never reach the user via logging. This module fills that gap with
//! curated lines printed to stderr (the same stream the bar uses) wrapped
//! in `MultiProgress::suspend` so an in-flight redraw doesn't collide.
//!
//! Off mode is a strict no-op: nothing is written, no allocation, no lock
//! contention.

#[cfg(test)]
use std::io::Write;

use crate::personality::Mode;

/// Write a single narration line to `w`, appending a newline. In off mode
/// this is a no-op and `w` is not touched. Test-only surface that mirrors
/// the off-vs-friendly gate used by `line_to_stderr`; production callers
/// go through `line_to_stderr` (which routes through indicatif's
/// `MultiProgress::println` to coexist with an active bar).
#[cfg(test)]
fn line<W: Write>(w: &mut W, mode: Mode, text: &str) -> std::io::Result<()> {
    if !mode.is_friendly() {
        return Ok(());
    }
    writeln!(w, "{text}")
}

/// Print a narration line above any active progress bar.
///
/// Routes through `MultiProgress::println` rather than `suspend(...) +
/// eprintln!`: println is a single atomic draw that combines the new line
/// with the bars, so indicatif's cursor tracking stays consistent. The
/// suspend path has a window between clear and redraw where caller writes
/// don't update indicatif's line count - on a bar whose last row reaches
/// the terminal's right edge, the cursor wraps to a new line and the
/// next redraw lands off-by-one, leaving a stale row above the bar.
///
/// Cheap when no bar is registered (indicatif's println short-circuits).
pub fn line_to_stderr(mode: Mode, text: &str) {
    if !mode.is_friendly() {
        return;
    }
    crate::personality::active_bar::println_above_bars(text);
}

/// Pre-cycle greeting. One line, fired once per process before the first
/// sync cycle runs.
pub fn greet_to_stderr(mode: Mode, watch_mode: bool) {
    let text = if watch_mode {
        "Hi! Watching iCloud for new photos."
    } else {
        "Hi! Checking iCloud for new photos."
    };
    line_to_stderr(mode, text);
}

/// Post-auth narration: confirms the account that just authenticated.
pub fn auth_ok_to_stderr(mode: Mode, username: &str) {
    line_to_stderr(mode, &format!("Signed in as {username}."));
}

/// Post-library-resolve narration: how many libraries kei is going to walk.
pub fn libraries_resolved_to_stderr(mode: Mode, library_count: usize) {
    let text = match library_count {
        0 => "No libraries available; nothing to sync.".to_string(),
        1 => "Found 1 library to sync.".to_string(),
        n => format!("Found {n} libraries to sync."),
    };
    line_to_stderr(mode, &text);
}

/// First-Ctrl+C acknowledgement. Friendly mode filters `tracing::info` to
/// WARN+, so the shutdown handler's existing log lines are silent here -
/// without this narration the user sees no response and assumes the app
/// has hung. The off path keeps the original tracing lines for journals.
pub fn stop_signal_to_stderr(mode: Mode) {
    line_to_stderr(
        mode,
        "Stopping. Finishing in-flight downloads. Press Ctrl+C again to exit immediately.",
    );
}

/// Pre-sleep heartbeat in watch mode: tells the user when the next cycle
/// will start so an idle-looking process is legible. Local clock; the
/// existing `tracing::info!(interval_secs)` event at the same call site
/// keeps the journal-friendly form for off-mode users.
pub fn sleeping_until_to_stderr(mode: Mode, wake_at: chrono::DateTime<chrono::Local>) {
    line_to_stderr(mode, &render_sleeping_until(wake_at));
}

/// Final line on a graceful Ctrl+C exit, after in-flight downloads drain.
/// Off mode is silent; the existing `tracing::info!` "Shutdown..." lines
/// already serve journal consumers.
pub fn farewell_to_stderr(mode: Mode) {
    line_to_stderr(mode, FAREWELL_LINE);
}

/// Friendly framing for a CloudKit 421 Misdirected Request. Printed just
/// before the HTTP/2 connection pool is reset so the user sees a brief
/// human explanation alongside the existing diagnostic `tracing::warn!`.
/// Off mode is silent.
pub fn wobble_to_stderr(mode: Mode) {
    line_to_stderr(mode, WOBBLE_LINE);
}

/// Pre-sleep narration before a retry. The existing `tracing::warn!` in
/// `retry_with_backoff` carries the structured fields (attempt, delay,
/// error) for journals; this is the human-shaped reminder that the pause
/// is intentional. Off mode is silent.
pub fn retry_pause_to_stderr(mode: Mode, delay: std::time::Duration) {
    line_to_stderr(mode, &render_retry_pause(delay));
}

/// Confirms a 421 retry, retry-with-backoff sequence, or other
/// recoverable hiccup actually recovered, so the prior framing line
/// (wobble / retry pause) doesn't sit in scrollback as the last thing
/// the user saw before downloads resumed. Off mode is silent.
pub fn back_on_track_to_stderr(mode: Mode) {
    line_to_stderr(mode, BACK_ON_TRACK_LINE);
}

/// Final-attempt narration when retries are exhausted. Tells the user the
/// failure is recorded and the next sync will pick the asset back up,
/// rather than letting the surfaced error stand alone. Only fires after
/// at least one `retry_pause_to_stderr`; one-shot failures are silent.
pub fn giving_up_to_stderr(mode: Mode) {
    line_to_stderr(mode, GIVING_UP_LINE);
}

const FAREWELL_LINE: &str = "Done. See you next time.";
const WOBBLE_LINE: &str = "iCloud connection wobbled. Resetting...";
const BACK_ON_TRACK_LINE: &str = "Back on track.";
const GIVING_UP_LINE: &str = "That one is being stubborn. Skipping for now, will retry next sync.";

fn render_sleeping_until(wake_at: chrono::DateTime<chrono::Local>) -> String {
    format!(
        "Sleeping until {}. Press Ctrl+C to stop.",
        wake_at.format("%H:%M"),
    )
}

fn render_retry_pause(delay: std::time::Duration) -> String {
    // Sub-second delays still surface as "1s" so the user has a concrete
    // number rather than "0s" (which reads as "no pause" and undermines
    // the friendly framing).
    let secs = delay.as_secs().max(1);
    format!("iCloud hiccup. Retrying in {secs}s...")
}

/// Post-cycle sign-off summarising what the cycle did. Friendly-only;
/// callers in off mode keep relying on `log_sync_summary` for journals.
pub fn signoff_to_stderr(mode: Mode, summary: &CycleSummary) {
    if !mode.is_friendly() {
        return;
    }
    line_to_stderr(mode, &summary.render());
}

/// Cycle summary ready for human rendering. Held off the `download::SyncStats`
/// surface so narration stays a leaf module - sync_loop maps stats into this
/// before calling `signoff_to_stderr`.
#[derive(Debug, Clone)]
pub struct CycleSummary {
    pub downloaded: u64,
    pub failed: u64,
    pub elapsed: std::time::Duration,
    pub watch_mode: bool,
}

impl CycleSummary {
    fn render(&self) -> String {
        let elapsed = format_elapsed(self.elapsed);
        let body = match (self.downloaded, self.failed) {
            (0, 0) => format!("Done. Nothing new in {elapsed}."),
            (n, 0) => format!(
                "Done. {n} new file{s} in {elapsed}.",
                s = if n == 1 { "" } else { "s" },
            ),
            (0, f) => format!(
                "Finished with {f} failure{s} in {elapsed}.",
                s = if f == 1 { "" } else { "s" },
            ),
            (n, f) => format!(
                "Done with {n} new file{ns} and {f} failure{fs} in {elapsed}.",
                ns = if n == 1 { "" } else { "s" },
                fs = if f == 1 { "" } else { "s" },
            ),
        };
        if self.watch_mode {
            format!("{body} Will check again on the next cycle.")
        } else {
            body
        }
    }
}

/// Format an elapsed duration as a friendly phrase. Short syncs round to
/// seconds; longer ones surface minutes and hours.
fn format_elapsed(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        if secs <= 1 {
            "a second".to_string()
        } else {
            format!("{secs} seconds")
        }
    } else if secs < 3600 {
        let minutes = secs / 60;
        format!(
            "{minutes} minute{s}",
            s = if minutes == 1 { "" } else { "s" },
        )
    } else {
        let hours = secs / 3600;
        let minutes = (secs % 3600) / 60;
        if minutes == 0 {
            format!("{hours} hour{s}", s = if hours == 1 { "" } else { "s" })
        } else {
            format!("{hours}h {minutes}m",)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::time::Duration;

    fn capture(mode: Mode, f: impl FnOnce(&mut Vec<u8>, Mode)) -> String {
        let mut buf = Vec::new();
        f(&mut buf, mode);
        String::from_utf8(buf).expect("narration writes valid utf-8")
    }

    #[test]
    fn off_mode_writes_nothing() {
        let out = capture(Mode::Off, |w, m| {
            line(w, m, "anything").expect("never errors in off");
        });
        assert!(out.is_empty(), "off mode must not write to the buffer");
    }

    #[test]
    fn friendly_mode_writes_line_with_newline() {
        let out = capture(Mode::Friendly, |w, m| {
            line(w, m, "hello there").expect("write should succeed");
        });
        assert_eq!(out, "hello there\n");
    }

    #[test]
    fn friendly_mode_multiple_lines_concatenate() {
        let out = capture(Mode::Friendly, |w, m| {
            line(w, m, "first").unwrap();
            line(w, m, "second").unwrap();
        });
        assert_eq!(out, "first\nsecond\n");
    }

    #[test]
    fn stop_signal_off_mode_writes_nothing() {
        let out = capture(Mode::Off, |w, m| {
            // Mirror line() with the same body stop_signal_to_stderr writes.
            line(
                w,
                m,
                "Stopping. Finishing in-flight downloads. Press Ctrl+C again to exit immediately.",
            )
            .unwrap();
        });
        assert!(out.is_empty());
    }

    #[test]
    fn stop_signal_friendly_writes_full_line() {
        let out = capture(Mode::Friendly, |w, m| {
            line(
                w,
                m,
                "Stopping. Finishing in-flight downloads. Press Ctrl+C again to exit immediately.",
            )
            .unwrap();
        });
        assert_eq!(
            out,
            "Stopping. Finishing in-flight downloads. Press Ctrl+C again to exit immediately.\n",
        );
    }

    #[test]
    fn wobble_back_on_track_and_giving_up_lines_are_stable_text() {
        // Pin user-visible strings; reword behind a deliberate diff.
        assert_eq!(WOBBLE_LINE, "iCloud connection wobbled. Resetting...");
        assert_eq!(BACK_ON_TRACK_LINE, "Back on track.");
        assert_eq!(
            GIVING_UP_LINE,
            "That one is being stubborn. Skipping for now, will retry next sync.",
        );
    }

    #[test]
    fn render_retry_pause_with_seconds() {
        assert_eq!(
            render_retry_pause(Duration::from_secs(4)),
            "iCloud hiccup. Retrying in 4s...",
        );
        assert_eq!(
            render_retry_pause(Duration::from_secs(60)),
            "iCloud hiccup. Retrying in 60s...",
        );
    }

    #[test]
    fn render_retry_pause_subsecond_floors_to_one_second() {
        // Duration::from_millis(500).as_secs() == 0; render must promote to "1s"
        // so the user gets a concrete number rather than "0s".
        assert_eq!(
            render_retry_pause(Duration::from_millis(500)),
            "iCloud hiccup. Retrying in 1s...",
        );
        assert_eq!(
            render_retry_pause(Duration::from_secs(0)),
            "iCloud hiccup. Retrying in 1s...",
        );
    }

    #[test]
    fn cycle_summary_no_changes_says_nothing_new() {
        let s = CycleSummary {
            downloaded: 0,
            failed: 0,
            elapsed: Duration::from_secs(2),
            watch_mode: false,
        };
        assert_eq!(s.render(), "Done. Nothing new in 2 seconds.");
    }

    #[test]
    fn cycle_summary_pluralises_files() {
        let one = CycleSummary {
            downloaded: 1,
            failed: 0,
            elapsed: Duration::from_secs(5),
            watch_mode: false,
        };
        assert_eq!(one.render(), "Done. 1 new file in 5 seconds.");
        let many = CycleSummary {
            downloaded: 42,
            failed: 0,
            elapsed: Duration::from_secs(120),
            watch_mode: false,
        };
        assert_eq!(many.render(), "Done. 42 new files in 2 minutes.");
    }

    #[test]
    fn cycle_summary_failures_only() {
        let s = CycleSummary {
            downloaded: 0,
            failed: 3,
            elapsed: Duration::from_secs(30),
            watch_mode: false,
        };
        assert_eq!(s.render(), "Finished with 3 failures in 30 seconds.");
    }

    #[test]
    fn cycle_summary_mixed_downloaded_and_failures() {
        let s = CycleSummary {
            downloaded: 12,
            failed: 1,
            elapsed: Duration::from_secs(180),
            watch_mode: false,
        };
        assert_eq!(
            s.render(),
            "Done with 12 new files and 1 failure in 3 minutes.",
        );
    }

    #[test]
    fn cycle_summary_watch_mode_appends_next_cycle_line() {
        let s = CycleSummary {
            downloaded: 0,
            failed: 0,
            elapsed: Duration::from_secs(5),
            watch_mode: true,
        };
        assert_eq!(
            s.render(),
            "Done. Nothing new in 5 seconds. Will check again on the next cycle.",
        );
    }

    #[test]
    fn format_elapsed_under_one_second_says_a_second() {
        assert_eq!(format_elapsed(Duration::from_millis(0)), "a second");
        assert_eq!(format_elapsed(Duration::from_millis(900)), "a second");
    }

    #[test]
    fn farewell_line_is_stable_text() {
        // Pin the user-visible string so accidental rewording goes through review.
        assert_eq!(FAREWELL_LINE, "Done. See you next time.");
    }

    #[test]
    fn render_sleeping_until_uses_24h_local_clock() {
        let wake_at = chrono::Local
            .with_ymd_and_hms(2026, 5, 9, 14, 32, 0)
            .single()
            .expect("unambiguous local time");
        assert_eq!(
            render_sleeping_until(wake_at),
            "Sleeping until 14:32. Press Ctrl+C to stop.",
        );
    }

    #[test]
    fn render_sleeping_until_zero_pads_single_digits() {
        let wake_at = chrono::Local
            .with_ymd_and_hms(2026, 5, 9, 4, 5, 0)
            .single()
            .expect("unambiguous local time");
        assert_eq!(
            render_sleeping_until(wake_at),
            "Sleeping until 04:05. Press Ctrl+C to stop.",
        );
    }

    #[test]
    fn format_elapsed_seconds_minutes_hours() {
        assert_eq!(format_elapsed(Duration::from_secs(2)), "2 seconds");
        assert_eq!(format_elapsed(Duration::from_secs(60)), "1 minute");
        assert_eq!(format_elapsed(Duration::from_secs(120)), "2 minutes");
        assert_eq!(format_elapsed(Duration::from_secs(3600)), "1 hour");
        assert_eq!(format_elapsed(Duration::from_secs(7200)), "2 hours");
        assert_eq!(format_elapsed(Duration::from_secs(7320)), "2h 2m");
    }
}
