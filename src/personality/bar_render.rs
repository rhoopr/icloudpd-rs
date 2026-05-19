//! Animated bar rendering: leading-edge shimmer, fill-color tiers, rule pulse.
//!
//! All functions take pure inputs (`fraction`, `elapsed_ms`) and return either
//! a `String` ready to write into an indicatif template or a `console::Style`
//! ready to wrap content. No I/O, no global state, fully unit-testable.

use std::time::Instant;

use console::Style;

/// Sub-cell heights ordered so index `i` represents `i/8` cell fill. Used to
/// pick the leading-edge cell character based on the fractional position.
/// Index 0 is empty (used as `EMPTY_CELL` instead), index 8 is full block.
pub const PARTIAL_HEIGHTS: &[char] = &[' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];

/// Empty-cell character used in the unfilled portion of the bar.
pub const EMPTY_CELL: char = '░';

/// Per-tick easing factor for visually smoothing the bar position. Lower =
/// more lag, smoother. 0.15 takes ~1s to converge 80% of the way to a new
/// target at 10Hz redraw, which feels like a proper slide without dragging.
const SMOOTHING_ALPHA: f64 = 0.15;

/// Track a smoothed bar position across redraws.
///
/// The underlying `state.fraction()` jumps in chunks (one file = several bar
/// cells); rendering it directly produces visible jitter. This struct lerps
/// the displayed fraction toward the true fraction so the bar slides
/// smoothly between progress events.
#[derive(Debug)]
pub struct BarSmoother {
    displayed: f64,
    last_tick_at: Option<Instant>,
}

impl Default for BarSmoother {
    fn default() -> Self {
        Self {
            displayed: 0.0,
            last_tick_at: None,
        }
    }
}

impl BarSmoother {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Advance the smoother toward `true_fraction` by one tick. Returns the
    /// displayed (smoothed) fraction in `[0.0, 1.0]`.
    ///
    /// Reaching the final position deserves to snap rather than asymptote
    /// forever, so we hard-set displayed once we're within a fraction of a
    /// cell of the target (or the target is exactly 1.0).
    pub fn tick(&mut self, true_fraction: f64) -> f64 {
        self.tick_at(true_fraction, Instant::now())
    }

    fn tick_at(&mut self, true_fraction: f64, now: Instant) -> f64 {
        let target = true_fraction.clamp(0.0, 1.0);
        // Effective alpha grows with elapsed wall-time so the easing is
        // smooth across variable redraw cadences (e.g. when steady_tick
        // races with a position-driven redraw).
        let alpha = match self.last_tick_at {
            Some(prev) => {
                // Saturating cast: dt_ms beyond u32 (~49 days) doesn't matter
                // here; we only need millisecond resolution against a 100ms
                // reference cadence. f64 mantissa handles a u32 exactly.
                let dt_ms_u = u32::try_from(
                    now.saturating_duration_since(prev)
                        .as_millis()
                        .min(u128::from(u32::MAX)),
                )
                .unwrap_or(u32::MAX);
                let dt_ms = f64::from(dt_ms_u);
                // Reference cadence: 100ms. Beyond 1s, ramp toward instant.
                let scale = (dt_ms / 100.0).clamp(0.5, 6.0);
                (SMOOTHING_ALPHA * scale).min(0.95)
            }
            None => 1.0, // first tick: snap to current target so we don't slide from 0
        };
        self.last_tick_at = Some(now);
        // Snap when target is at the cap or essentially-converged to avoid
        // a perpetual asymptote against the rounding boundary at 100%.
        if (target - self.displayed).abs() < 1e-4 || target >= 1.0 - f64::EPSILON {
            self.displayed = target;
        } else {
            self.displayed += (target - self.displayed) * alpha;
        }
        self.displayed
    }
}

/// Render the bar at `fraction` over `width` cells with sub-cell partial
/// fill at the leading edge. Pure function: no animation timer, no global
/// state. Visual motion comes from feeding it a smoothed fraction (see
/// `BarSmoother`) on each redraw, not from cycling chars in place.
///
/// Layout: `<full cells, fill-color>` + `<partial cell, fill-color>` +
/// `<empty cells, dim>`. The partial cell encodes the eighths-of-a-cell
/// remainder of `fraction * width`. At `fraction == 1.0` the entire bar is
/// full color and there's no partial cell.
#[must_use]
pub fn animated_bar_string(fraction: f64, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let clamped = fraction.clamp(0.0, 1.0);
    #[allow(
        clippy::cast_precision_loss,
        reason = "width fits in u32 in practice; precision loss is sub-cell"
    )]
    let cells_f = clamped * width as f64;
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "cells_f is non-negative and bounded by width"
    )]
    let full_cells = cells_f.floor() as usize;
    #[allow(
        clippy::cast_precision_loss,
        reason = "full_cells <= width fits comfortably in f64 mantissa"
    )]
    let full_cells_f = full_cells as f64;
    let fill_style = bar_fill_style(clamped);
    let dim_style = empty_cell_style();

    if full_cells >= width {
        return fill_style.apply_to("█".repeat(width)).to_string();
    }

    // Eighths-of-a-cell remainder picks the partial char. Index 0 is empty
    // (skip; render as EMPTY_CELL instead so the unfilled style applies),
    // index 8 wraps to a full block (which means we should have advanced a
    // cell — but float rounding can land us here, handle gracefully).
    let remainder = cells_f - full_cells_f;
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "remainder*8 is in [0, 8]"
    )]
    let partial_idx = (remainder * 8.0).round() as usize;

    let mut filled_part = String::with_capacity(full_cells * 4 + 4);
    for _ in 0..full_cells {
        filled_part.push('█');
    }

    let (partial_char_for_fill, has_visible_partial) = if partial_idx == 0 {
        (None, false)
    } else if partial_idx >= PARTIAL_HEIGHTS.len() {
        // Float rounded to a full cell; treat as a full cell at the leading
        // edge (we'll still have one fewer empty cell below).
        (Some('█'), true)
    } else {
        (PARTIAL_HEIGHTS.get(partial_idx).copied(), true)
    };
    if let Some(c) = partial_char_for_fill {
        filled_part.push(c);
    }

    let empty_cells = width - full_cells - usize::from(has_visible_partial);
    let empty_part: String = (0..empty_cells).map(|_| EMPTY_CELL).collect();

    format!(
        "{}{}",
        fill_style.apply_to(filled_part),
        dim_style.apply_to(empty_part),
    )
}

/// Color tier for the bar's filled portion based on progress fraction.
///
/// Three tiers chosen for readable contrast on dark and light terminals:
/// bright cyan (early progress), cyan (mid), green (near done). The shift
/// gives a glance-able "how far along am I" cue beyond the fill itself,
/// landing on green as a "finished" signal as the bar fills.
#[must_use]
pub fn bar_fill_style(fraction: f64) -> Style {
    if fraction < 0.30 {
        Style::new().cyan().bold()
    } else if fraction < 0.70 {
        Style::new().cyan()
    } else {
        Style::new().green()
    }
}

/// Format a bandwidth rate (bytes/sec) for inline display in the bar's
/// rate column. Fixed total width of 10 chars so the cells to its right
/// (sparkline, counts, ETA) don't shift as units cross thresholds. Picks
/// the largest unit that keeps the integer part in two digits where
/// possible, falling back to a third digit at higher rates.
#[must_use]
pub fn format_bandwidth(bytes_per_sec: f64) -> String {
    let (val, unit) = if bytes_per_sec < 1024.0 {
        (bytes_per_sec, "B/s ")
    } else if bytes_per_sec < 1024.0 * 1024.0 {
        (bytes_per_sec / 1024.0, "KB/s")
    } else if bytes_per_sec < 1024.0 * 1024.0 * 1024.0 {
        (bytes_per_sec / (1024.0 * 1024.0), "MB/s")
    } else {
        (bytes_per_sec / (1024.0 * 1024.0 * 1024.0), "GB/s")
    };
    // Single-digit values use one decimal; double-digit values use one
    // decimal; triple-digit values drop the decimal so the column stays
    // 4-wide. Pad to 10 chars total ("12.4 MB/s ") so the rate column has
    // a consistent footprint as the unit changes.
    let body = if val >= 100.0 {
        format!("{val:>4.0} {unit}")
    } else {
        format!("{val:>4.1} {unit}")
    };
    format!("{body:<10}")
}

/// Style for the empty (unfilled) cells. Dim grey so the contrast against
/// the filled portion makes the wave-front pop.
#[must_use]
pub fn empty_cell_style() -> Style {
    // 244 is medium grey in the 256-color palette; falls back gracefully on
    // 16-color terminals to plain "dim".
    Style::new().color256(244)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn animated_bar_at_zero_fraction_shows_only_empty_cells() {
        let s = animated_bar_string(0.0, 10);
        assert!(
            !s.contains('█'),
            "zero fraction should have no full cells: {s:?}"
        );
        assert!(
            s.contains(EMPTY_CELL),
            "zero fraction should show empty cells"
        );
    }

    #[test]
    fn animated_bar_at_full_fraction_shows_only_full_cells() {
        let s = animated_bar_string(1.0, 10);
        assert!(s.contains('█'));
        assert!(!s.contains(EMPTY_CELL));
        // No partial-block characters at fully-filled.
        for c in PARTIAL_HEIGHTS.iter().filter(|&&c| c != '█' && c != ' ') {
            assert!(
                !s.contains(*c),
                "full bar should not contain partial char {c}",
            );
        }
    }

    #[test]
    fn animated_bar_at_half_fraction_has_clean_split() {
        // 0.5 of 10 cells = exactly 5 full cells, no partial. Leading edge
        // sits cleanly on the 5/10 boundary.
        let s = animated_bar_string(0.5, 10);
        let chars: Vec<char> = strip_ansi(&s).chars().collect();
        assert_eq!(chars.len(), 10, "bar width should always be 10 cells");
        assert!(
            chars.iter().take(5).all(|&c| c == '█'),
            "first 5 should be full"
        );
        assert!(
            chars.iter().skip(5).all(|&c| c == EMPTY_CELL),
            "last 5 should be empty"
        );
    }

    #[test]
    fn animated_bar_partial_cell_encodes_fractional_position() {
        // 0.55 of 10 cells = 5.5 cells: 5 full + ▌ (4/8) leading edge + 4 empty.
        let s = animated_bar_string(0.55, 10);
        let chars: Vec<char> = strip_ansi(&s).chars().collect();
        assert_eq!(chars.len(), 10);
        assert!(chars.iter().take(5).all(|&c| c == '█'));
        // Cell 5 is the partial; round(0.5 * 8) = 4 -> ▌ (index 4 in PARTIAL_HEIGHTS).
        assert_eq!(
            chars[5], '▌',
            "cell 5 should be ▌ for fractional 0.5: got {:?}",
            chars[5]
        );
        assert!(chars.iter().skip(6).all(|&c| c == EMPTY_CELL));
    }

    #[test]
    fn animated_bar_no_two_in_place_renders_compete() {
        // Same fraction must render the same string regardless of how many
        // times we call it. (Replaces the old test that verified the
        // leading-edge cycled with elapsed time — that animation is gone.)
        let a = animated_bar_string(0.42, 20);
        let b = animated_bar_string(0.42, 20);
        assert_eq!(a, b, "same fraction must render identically");
    }

    #[test]
    fn smoother_first_tick_snaps_to_target() {
        let mut sm = BarSmoother::new();
        let now = Instant::now();
        let f = sm.tick_at(0.5, now);
        assert!(
            (f - 0.5).abs() < 1e-9,
            "first tick should snap to target, got {f}",
        );
    }

    #[test]
    fn smoother_lerps_toward_target_over_subsequent_ticks() {
        let mut sm = BarSmoother::new();
        let start = Instant::now();
        sm.tick_at(0.0, start);
        // Target jumps to 0.5; subsequent ticks at 100ms should lerp toward it.
        let f1 = sm.tick_at(0.5, start + std::time::Duration::from_millis(100));
        assert!(
            f1 > 0.0 && f1 < 0.5,
            "first lerp tick should be partial: {f1}"
        );
        let f2 = sm.tick_at(0.5, start + std::time::Duration::from_millis(200));
        assert!(f2 > f1 && f2 < 0.5, "second lerp should advance: {f2}");
    }

    #[test]
    fn smoother_snaps_to_one_at_completion() {
        let mut sm = BarSmoother::new();
        let start = Instant::now();
        sm.tick_at(0.0, start);
        let final_f = sm.tick_at(1.0, start + std::time::Duration::from_millis(100));
        assert!(
            (final_f - 1.0).abs() < f64::EPSILON,
            "100% should snap, not asymptote: got {final_f}",
        );
    }

    #[test]
    fn format_bandwidth_picks_appropriate_unit() {
        assert!(format_bandwidth(0.0).contains("B/s"));
        assert!(format_bandwidth(512.0).contains("B/s"));
        assert!(format_bandwidth(2048.0).contains("KB/s"));
        assert!(format_bandwidth(1_500_000.0).contains("MB/s"));
        assert!(format_bandwidth(2_500_000_000.0).contains("GB/s"));
    }

    #[test]
    fn format_bandwidth_pads_to_fixed_width() {
        // The rate column claims 10 chars regardless of magnitude so the
        // sparkline / counts / ETA to its right stay aligned.
        for &rate in &[
            0.5_f64,
            12.4 * 1024.0,
            1.2 * 1024.0 * 1024.0,
            150.0 * 1024.0 * 1024.0,
            2.0 * 1024.0 * 1024.0 * 1024.0,
        ] {
            let s = format_bandwidth(rate);
            assert_eq!(
                s.chars().count(),
                10,
                "rate {rate} -> {s:?} should be 10 chars"
            );
        }
    }

    #[test]
    fn smoother_is_idempotent_at_steady_target() {
        // Once converged, repeated ticks at the same target stay put.
        let mut sm = BarSmoother::new();
        let start = Instant::now();
        sm.tick_at(0.3, start);
        let mut f = 0.0;
        for i in 1..50 {
            f = sm.tick_at(0.3, start + std::time::Duration::from_millis(100 * i));
        }
        assert!(
            (f - 0.3).abs() < 1e-3,
            "displayed should converge to target, got {f}",
        );
    }

    #[test]
    fn bar_fill_style_color_tiers() {
        // `console::Style::apply_to` only emits ANSI when colors are enabled
        // for the writer (TTY detection). cargo test runs without a TTY, so
        // we force-enable styling per call to verify the actual SGR codes.
        // SGR 32 = green, SGR 36 = cyan, SGR 1 = bold.
        let early = format!("{}", bar_fill_style(0.10).force_styling(true).apply_to("x"));
        assert!(
            early.contains(";36m") || early.contains("\x1b[36m"),
            "early tier should be cyan-based: {early:?}"
        );
        assert!(
            early.contains("\x1b[1") || early.contains(";1m"),
            "early tier should be bold: {early:?}"
        );
        let mid = format!("{}", bar_fill_style(0.50).force_styling(true).apply_to("x"));
        assert!(
            mid.contains("\x1b[36m") || mid.contains(";36m"),
            "mid tier should be cyan: {mid:?}"
        );
        let late = format!("{}", bar_fill_style(0.85).force_styling(true).apply_to("x"));
        assert!(
            late.contains("\x1b[32m") || late.contains(";32m"),
            "late tier should be green: {late:?}"
        );
    }

    #[test]
    fn animated_bar_zero_width_returns_empty() {
        let s = animated_bar_string(0.5, 0);
        assert_eq!(s, "");
    }

    #[test]
    fn animated_bar_clamps_fraction_above_one() {
        // Defensive: don't panic on bogus input; render as if fully filled.
        let s = animated_bar_string(1.5, 10);
        let stripped = strip_ansi(&s);
        assert!(
            stripped.chars().all(|c| c == '█'),
            "over-100% clamps to full: {stripped:?}",
        );
    }

    #[test]
    fn animated_bar_clamps_negative_fraction() {
        // Defensive: treat negative as zero.
        let s = animated_bar_string(-0.3, 10);
        let stripped = strip_ansi(&s);
        assert!(
            !stripped.contains('█'),
            "negative clamps to empty: {stripped:?}",
        );
    }

    /// Strip ANSI escape sequences from a string. Lightweight, tests-only.
    fn strip_ansi(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' && chars.peek() == Some(&'[') {
                // Skip CSI sequence: ESC [ ... letter
                chars.next();
                while let Some(&nc) = chars.peek() {
                    chars.next();
                    if nc.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }
}
