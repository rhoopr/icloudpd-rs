//! Download progress bar construction and friendly progress-card wiring.

use std::io::IsTerminal;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use indicatif::{ProgressBar, ProgressState, ProgressStyle};

use crate::personality::{active_bar, progress_card, Mode};

/// Progress bar plus shared byte counter for multi-pass download loops.
#[derive(Debug)]
pub(crate) struct DownloadProgress {
    pub(crate) bar: ProgressBar,
    pub(crate) bytes: Arc<AtomicU64>,
}

/// Create the shared progress bar used by per-pass download loops.
///
/// Returns the bar plus an `Arc<AtomicU64>` byte counter that the caller
/// threads through to each pass's stream call. The same counter drives the
/// friendly bar's bandwidth sparkline / rate display, and the download loop
/// bumps it on every successful task completion.
pub(crate) fn for_passes(
    no_progress_bar: bool,
    only_print_filenames: bool,
    total: u64,
    mode: Mode,
) -> DownloadProgress {
    let bytes = Arc::new(AtomicU64::new(0));
    let bar = single(
        no_progress_bar,
        only_print_filenames,
        total,
        mode,
        Some(Arc::clone(&bytes)),
    );
    DownloadProgress { bar, bytes }
}

/// Create a progress bar with a consistent template.
///
/// Returns `ProgressBar::hidden()` when the user passed `--no-progress-bar`,
/// `--only-print-filenames`, or stdout is not a TTY (e.g. piped output, cron
/// jobs) - this prevents output corruption and honours the user's preference.
///
/// In friendly mode the template uses block-char gradients and adapts to
/// terminal width; in off mode it reproduces the v0.13 template byte-for-byte
/// so machine consumers (asciinema replays, log scrapers) see no diff.
pub(crate) fn single(
    no_progress_bar: bool,
    only_print_filenames: bool,
    total: u64,
    mode: Mode,
    bytes_counter: Option<Arc<AtomicU64>>,
) -> ProgressBar {
    if no_progress_bar || only_print_filenames || !std::io::stdout().is_terminal() {
        return ProgressBar::hidden();
    }
    // Register with the singleton MultiProgress so tracing events landing
    // mid-redraw (via the BarSuspendingStderr in lib.rs) don't desync the
    // bar's cursor positioning. Visual output is unchanged from a standalone
    // ProgressBar; the registration is purely about coordination.
    let pb = active_bar::register(ProgressBar::new(total));
    let cols = console::Term::stdout().size_checked().map(|(_, c)| c);
    // Default to 80 cols when detection fails (e.g. piped stdout, but we
    // already gated those paths above to ProgressBar::hidden so this is
    // conservative). Cap at 200 so the rule line doesn't grow unbounded.
    let cols_for_template = cols.unwrap_or(80).min(200);
    let bar_template = progress_card::template(mode, cols_for_template, total);
    let chars = progress_card::progress_chars(mode);
    if let Ok(mut style) = ProgressStyle::with_template(&bar_template.template) {
        style = style.progress_chars(chars);
        // Friendly mode registers custom template keys for the animated bar,
        // pulsing rules, sparkline, and smart ETA. Off mode skips them since
        // its template doesn't reference any of these names.
        if mode.is_friendly() {
            let bar_width = bar_template.bar_width;
            let sparkline = Arc::new(Mutex::new(progress_card::SparklineState::new(
                bar_template.sparkline_width,
            )));

            // Animated bar: a `BarSmoother` lerps the displayed fraction
            // toward the true fraction across redraws so the bar slides
            // smoothly between file completions instead of jumping several
            // cells per file. The leading-edge cell encodes the smoothed
            // fractional position via PARTIAL_HEIGHTS - no in-place cycling
            // that would compete with the bar's actual motion.
            let smoother = Arc::new(Mutex::new(progress_card::BarSmoother::new()));
            let smoother_for_key = Arc::clone(&smoother);
            style = style.with_key(
                "bar_animated",
                move |state: &ProgressState, w: &mut dyn std::fmt::Write| {
                    let true_frac = f64::from(state.fraction());
                    let displayed = {
                        let mut sm = smoother_for_key
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        sm.tick(true_frac)
                    };
                    let _ = write!(w, "{}", progress_card::render_bar(displayed, bar_width),);
                },
            );

            // Rules track the bar's fill-color tier (green / cyan / bright
            // cyan) so the box and bar shift together as progress advances.
            // No time-based pulse: the color change comes from progress
            // crossing a tier threshold, not from a redraw timer.
            let top_rule_text = bar_template.top_rule.clone();
            style = style.with_key(
                "top_rule",
                move |state: &ProgressState, w: &mut dyn std::fmt::Write| {
                    let frac = f64::from(state.fraction());
                    let s = progress_card::fill_style(frac);
                    let _ = write!(w, "{}", s.apply_to(&top_rule_text));
                },
            );
            let bottom_rule_text = bar_template.bottom_rule.clone();
            style = style.with_key(
                "bottom_rule",
                move |state: &ProgressState, w: &mut dyn std::fmt::Write| {
                    let frac = f64::from(state.fraction());
                    let s = progress_card::fill_style(frac);
                    let _ = write!(w, "{}", s.apply_to(&bottom_rule_text));
                },
            );

            let sparkline_for_key = Arc::clone(&sparkline);
            // The sparkline samples bytes when a counter is wired up
            // (production / per-pass branch); otherwise it falls back to the
            // bar's file-count position so off-mode-tests-using-friendly
            // surfaces still get something sensible.
            let bytes_for_key = bytes_counter.clone();
            style = style.with_key(
                "rate_sparkline",
                move |state: &ProgressState, w: &mut dyn std::fmt::Write| {
                    let mut sl = sparkline_for_key
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let sample = match &bytes_for_key {
                        Some(b) => b.load(Ordering::Relaxed),
                        None => state.pos(),
                    };
                    sl.sample(sample);
                    let rate = sl.rate_per_sec();
                    let chart = sl.render();
                    if bytes_for_key.is_some() {
                        // Bytes/sec -> human-readable bandwidth (B/s, KB/s,
                        // MB/s, GB/s). Fixed-width via format_bandwidth so the
                        // sparkline / counts / ETA to its right stay aligned.
                        if rate > 0.0 {
                            let bandwidth = progress_card::format_bandwidth(rate);
                            let _ = write!(w, "{bandwidth} {chart}");
                        } else {
                            let _ = write!(w, "{:<10} {chart}", "  --   B/s");
                        }
                    } else {
                        // Fallback: file rate display. Right-align to fixed
                        // 5-char width.
                        if rate > 0.0 {
                            let _ = write!(w, "{rate:>5.1}/s {chart}");
                        } else {
                            let _ = write!(w, "{:>5}/s {chart}", "--.-");
                        }
                    }
                },
            );
            // Per-bar EtaPhrasing carries the "calculating..." -> "still
            // calculating..." escalation across redraws. Shared state via
            // Arc<Mutex<>> because indicatif::with_key requires Send+Sync;
            // contention is nil (single-bar, single draw thread, ~10Hz).
            let phrasing = Arc::new(Mutex::new(progress_card::EtaPhrasing::new()));
            let phrasing_for_key = Arc::clone(&phrasing);
            style = style.with_key(
                "smart_eta",
                move |state: &ProgressState, w: &mut dyn std::fmt::Write| {
                    let secs = state.eta().as_secs();
                    let mut p = phrasing_for_key
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if secs == 0 && state.pos() < state.len().unwrap_or(u64::MAX) {
                        let _ = write!(w, "{}", p.unknown());
                    } else {
                        let _ = write!(w, "{}", p.known(secs));
                    }
                },
            );

            // Spinner glyph next to the percent - independent motion signal
            // even if work pauses. Moon-phase rotation (`◐◓◑◒`) sits on the
            // baseline like the digits beside it; braille spinners cluster
            // dots in the upper-half of their cell and read as floating high.
            //
            // Each glyph is repeated 4 times so the spinner advances one
            // visible phase per ~400ms of redraw activity (1.6s per full
            // rotation at the 10Hz steady-tick cadence) - slow enough to read
            // as "loading" rather than "frantic". The trailing space is
            // indicatif's "finished" frame.
            style = style.tick_chars(progress_card::FRIENDLY_TICK_CHARS);
        }
        pb.set_style(style);
    }
    // Steady tick so the bar redraws on its own clock and doesn't drift
    // off-screen when stderr logs scroll past or work pauses on a network
    // round-trip. 100ms is well under the perception threshold and also
    // under indicatif's 20Hz redraw cap, so we don't burn CPU on draws.
    pb.enable_steady_tick(Duration::from_millis(100));
    pb
}

#[cfg(test)]
mod tests {
    use std::io::IsTerminal;

    use super::*;

    #[test]
    fn test_single_hidden_when_disabled() {
        let pb = single(true, false, 100, Mode::Off, None);
        assert!(pb.is_hidden());
    }

    #[test]
    fn test_single_hidden_when_only_print_filenames() {
        let pb = single(false, true, 100, Mode::Off, None);
        assert!(pb.is_hidden());
    }

    #[test]
    fn test_single_with_total() {
        // When not disabled, the bar should have the correct length.
        // In CI/test environments stdout may not be a TTY, so the bar
        // may be hidden - we test both branches.
        let pb = single(false, false, 42, Mode::Off, None);
        if std::io::stdout().is_terminal() {
            assert!(!pb.is_hidden());
            assert_eq!(pb.length(), Some(42));
        } else {
            // Non-TTY: bar is hidden regardless of the flag.
            assert!(pb.is_hidden());
        }
    }

    #[test]
    fn test_for_passes_returns_bar_and_zeroed_counter() {
        let progress = for_passes(true, false, 100, Mode::Off);

        assert!(progress.bar.is_hidden());
        assert_eq!(progress.bytes.load(Ordering::Relaxed), 0);
    }
}
