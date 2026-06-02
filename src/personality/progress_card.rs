//! Friendly progress-card rendering helpers.
//!
//! This module owns the card template, animated bar rendering, throughput
//! sparkline, bandwidth label, spinner glyphs, and smart ETA wording.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use console::Style;

use crate::personality::Mode;

/// Friendly bar spinner glyphs. Each visible glyph is repeated four times so
/// the 10Hz redraw cadence advances one visible phase roughly every 400ms.
pub(crate) const FRIENDLY_TICK_CHARS: &str = "◐◐◐◐◓◓◓◓◑◑◑◑◒◒◒◒ ";

/// Width tier for adaptive bar templates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WidthTier {
    /// Wide: full template with elapsed, bar, counts, ETA, message.
    Wide,
    /// Medium: drop elapsed, narrower bar.
    Medium,
    /// Narrow: bar + counts only.
    Narrow,
}

impl WidthTier {
    /// Choose tier from terminal column count.
    #[must_use]
    fn from_cols(cols: u16) -> Self {
        match cols {
            c if c < 60 => Self::Narrow,
            c if c < 80 => Self::Medium,
            _ => Self::Wide,
        }
    }
}

/// Indicatif `progress_chars` argument. Block-char gradient when friendly is on
/// and the terminal can render Unicode; ASCII fallback otherwise.
///
/// The 9-char string drives indicatif's sub-cell partial-fill rendering: index
/// 0 is the fully-filled cell, indices 1-7 are progressively less-filled,
/// index 8 is the empty cell.
#[must_use]
pub(crate) fn progress_chars(mode: Mode) -> &'static str {
    match mode {
        Mode::Friendly => "█▉▊▋▌▍▎▏ ",
        Mode::Off => "=> ",
    }
}

/// Indicatif template for the cumulative download bar.
///
/// Off mode reproduces v0.13's exact template byte-for-byte so anyone parsing
/// the bar (e.g. from a recorded asciinema) sees no difference.
///
/// Friendly mode wraps a six-row "card" around the work (one blank row of
/// breathing room above a five-row box) whose top and bottom rules are
/// sized to the terminal so the box stays a true rectangle:
/// ```text
///
/// ╭── kei · downloading from iCloud ───────
/// │  IMG_4521.HEIC
/// │  ████████████████░░░░░░ 62%
/// │   4.2/s  ▁▂▃▅▇█▇▅  18/30  ·  about 1 minute
/// ╰────────────────────────────────────────
/// ```
///
/// `cols` is the terminal width; the rules are `cols-1` wide so they don't
/// wrap on the rightmost column. `total` is the bar's total count, used to
/// zero-pad `{pos}` to `{len}`'s digit count so the counter doesn't shift
/// when crossing a power of ten.
///
/// Custom keys `{rate_sparkline}` and `{smart_eta}` are registered in
/// `progress::single` via `ProgressStyle::with_key`.
/// Indicatif template + the static rule strings the friendly card draws on
/// its top and bottom lines.
///
/// `template` is the indicatif template string (passed to
/// `ProgressStyle::with_template`). `top_rule` and `bottom_rule` are the
/// rendered rule strings to feed into the `{top_rule}` and `{bottom_rule}`
/// custom keys via `ProgressStyle::with_key` so the closure can color-cycle
/// them on each redraw.
///
/// In off mode and narrow tier, both rules are empty (the template doesn't
/// reference them so the closures never fire — but providing an empty rule
/// keeps the call site simpler than branching).
#[derive(Debug, Clone)]
pub(crate) struct CardTemplate {
    pub(crate) template: String,
    pub(crate) top_rule: String,
    pub(crate) bottom_rule: String,
    pub(crate) bar_width: usize,
    pub(crate) sparkline_width: usize,
}

const SOURCE_LABEL: &str = "iCloud";

#[must_use]
pub(crate) fn template(mode: Mode, cols: u16, total: u64) -> CardTemplate {
    let tier = WidthTier::from_cols(cols);
    template_for_tier(mode, tier, cols, total)
}

fn template_for_tier(mode: Mode, tier: WidthTier, cols: u16, total: u64) -> CardTemplate {
    match (mode, tier) {
        (Mode::Off, _) => CardTemplate {
            template: "[{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta}) {msg}"
                .to_string(),
            top_rule: String::new(),
            bottom_rule: String::new(),
            bar_width: 0,
            sparkline_width: 0,
        },
        (Mode::Friendly, WidthTier::Wide) => friendly_card(cols, total, true),
        (Mode::Friendly, WidthTier::Medium) => friendly_card(cols, total, false),
        (Mode::Friendly, WidthTier::Narrow) => CardTemplate {
            template: "{bar:16.cyan/blue} {pos}/{len}".to_string(),
            top_rule: String::new(),
            bottom_rule: String::new(),
            bar_width: friendly_bar_width(cols) as usize,
            sparkline_width: friendly_sparkline_width(cols) as usize,
        },
    }
}

/// Inner-bar character width for a friendly card at `cols` terminal columns.
///
/// Rationale: line 2 is `│  {bar:N} 100%` — overhead is 3 (left margin) + 1
/// (space before percent) + 4 (the rendered "100%" plus a trailing breathing
/// pixel) = 8. We subtract a bit more (12 total) to leave the right edge
/// slightly inset from the box rule, which reads as more intentional than
/// the bar butting up against `╮`.
///
/// Clamped to `[24, 120]`: 24 keeps the bar legible at 60-col terminals,
/// 120 stops the bar from looking like a runway on ultrawide monitors where
/// the proportion to the rest of the card matters more than absolute width.
#[must_use]
fn friendly_bar_width(cols: u16) -> u16 {
    cols.saturating_sub(12).clamp(24, 120)
}

/// Sparkline cell count for a friendly card at `cols` terminal columns.
///
/// Sized to roughly one third of the terminal so the chart reads as a real
/// shape rather than a thumbnail. Line 3 also has to fit `rate + counts +
/// "·" + smart_eta`, so we cap below the bar's max — the chart shouldn't
/// fight the bar for visual weight.
///
/// Clamped to `[16, 48]`: 16 keeps a useful number of samples visible at
/// narrow card widths, 48 stops the chart from dominating row 3 when paired
/// with a long ETA wording on ultrawide terminals.
#[must_use]
fn friendly_sparkline_width(cols: u16) -> u16 {
    (cols / 3).clamp(16, 48)
}

/// Build a friendly six-row card (1 blank + 5 content) sized to `cols`.
///
/// Returns the indicatif template (referencing custom keys `{top_rule}`,
/// `{bottom_rule}`, `{bar_animated}`, `{spinner}`) plus the rendered rule
/// strings the closures will pulse-color on each redraw.
fn friendly_card(cols: u16, total: u64, with_smart_eta: bool) -> CardTemplate {
    // Rule width: cols - 1 so a final newline / cursor reset doesn't bump the
    // bar onto a phantom line on terminals that auto-wrap at exactly cols.
    let rule_total = cols.saturating_sub(1).max(20) as usize;
    let header = format!(" kei \u{00b7} downloading from {SOURCE_LABEL} ");
    // Top rule: ╭── kei · downloading from <source> ───...─╮
    // Layout pieces: ╭ + 2 dashes + header + N dashes + ╮.
    let top_dashes_after_header = rule_total
        .saturating_sub(4) // ╭ + 2 leading dashes + ╮
        .saturating_sub(header.chars().count());
    let top = format!("╭──{header}{}╮", "─".repeat(top_dashes_after_header),);

    // Bottom rule: ╰────...────╯ (matches top width via the same rule_total).
    let bottom = format!("╰{}╯", "─".repeat(rule_total.saturating_sub(2)));

    // Pos width tracks len's digit count. `1/30` aligns with `30/30` as
    // ` 1/30` and `30/30` so the counter doesn't shift when crossing a
    // power of ten.
    //
    let pos_width = total.checked_ilog10().map_or(1, |n| n + 1) as usize;

    // `{bar_animated}` and other custom keys (`{top_rule}`, `{bottom_rule}`,
    // `{rate_sparkline}`, `{smart_eta}`) are registered in progress.rs.
    // `{spinner}` is an indicatif built-in that animates against the bar's
    // tick chars.
    // Leading `\n` on the template gives the card one blank line of
    // breathing room above the top rule, separating it from prior
    // scrollback (greeting, narration, the previous cycle's summary)
    // without the user having to read a wall of stacked content. The
    // empty line is part of the bar's tracked draw region so it scrolls
    // with the bar instead of accumulating.
    let template = if with_smart_eta {
        format!(
            "\n{{top_rule}}\n│  {{wide_msg}}\n│  {{bar_animated}} {{percent:>3}}% {{spinner}}\n│  {{rate_sparkline}}  {{pos:>{pos_width}}}/{{len}}  ·  {{smart_eta}}\n{{bottom_rule}}"
        )
    } else {
        format!(
            "\n{{top_rule}}\n│  {{wide_msg}}\n│  {{bar_animated}} {{percent:>3}}% {{spinner}}\n│  {{rate_sparkline}}  {{pos:>{pos_width}}}/{{len}}\n{{bottom_rule}}"
        )
    };
    CardTemplate {
        template,
        top_rule: top,
        bottom_rule: bottom,
        bar_width: friendly_bar_width(cols) as usize,
        sparkline_width: friendly_sparkline_width(cols) as usize,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_tier_from_cols() {
        assert_eq!(WidthTier::from_cols(40), WidthTier::Narrow);
        assert_eq!(WidthTier::from_cols(59), WidthTier::Narrow);
        assert_eq!(WidthTier::from_cols(60), WidthTier::Medium);
        assert_eq!(WidthTier::from_cols(79), WidthTier::Medium);
        assert_eq!(WidthTier::from_cols(80), WidthTier::Wide);
        assert_eq!(WidthTier::from_cols(200), WidthTier::Wide);
    }

    #[test]
    fn off_mode_progress_chars_match_v013() {
        assert_eq!(progress_chars(Mode::Off), "=> ");
    }

    #[test]
    fn friendly_progress_chars_use_block_gradient() {
        let chars = progress_chars(Mode::Friendly);
        assert!(chars.contains('█'));
        assert!(chars.contains('▏'));
        assert!(chars.ends_with(' '));
    }

    #[test]
    fn friendly_tick_chars_include_finished_frame() {
        assert!(FRIENDLY_TICK_CHARS.contains('◐'));
        assert!(FRIENDLY_TICK_CHARS.ends_with(' '));
    }

    #[test]
    fn off_template_matches_v013_exactly() {
        let v013 = "[{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta}) {msg}";
        let off = template_for_tier(Mode::Off, WidthTier::Wide, 80, 100);
        assert_eq!(off.template, v013);
        assert!(off.top_rule.is_empty());
        assert!(off.bottom_rule.is_empty());
        // Off mode ignores tier so machine-output stability is unconditional.
        let off_narrow = template_for_tier(Mode::Off, WidthTier::Narrow, 80, 100);
        assert_eq!(off_narrow.template, v013);
    }

    #[test]
    fn friendly_narrow_drops_elapsed_and_eta() {
        let narrow = template_for_tier(Mode::Friendly, WidthTier::Narrow, 50, 30);
        assert!(!narrow.template.contains("elapsed"));
        assert!(!narrow.template.contains("eta"));
        assert!(narrow.template.contains("{bar:"));
        assert!(narrow.template.contains("{pos}/{len}"));
        // Narrow tier drops the box rules.
        assert!(narrow.top_rule.is_empty());
        assert!(narrow.bottom_rule.is_empty());
    }

    #[test]
    fn friendly_wide_card_top_and_bottom_rules_match_width() {
        let wide = template_for_tier(Mode::Friendly, WidthTier::Wide, 80, 30);
        // Top and bottom rules are now stored separately on CardTemplate so
        // progress.rs can wrap them in pulse-color closures. Width must still
        // match between the two so the box reads as a true rectangle.
        let top_chars = wide.top_rule.chars().count();
        let bottom_chars = wide.bottom_rule.chars().count();
        assert_eq!(
            top_chars, bottom_chars,
            "top ({top_chars}) and bottom ({bottom_chars}) rules must match width",
        );
        assert!(
            wide.top_rule.ends_with('╮'),
            "top should end with ╮: {:?}",
            wide.top_rule,
        );
        assert!(
            wide.bottom_rule.ends_with('╯'),
            "bottom should end with ╯: {:?}",
            wide.bottom_rule,
        );
        assert!(
            wide.top_rule.starts_with('╭'),
            "top should start with ╭: {:?}",
            wide.top_rule,
        );
        assert!(
            wide.bottom_rule.starts_with('╰'),
            "bottom should start with ╰: {:?}",
            wide.bottom_rule,
        );
        assert!(
            wide.top_rule.contains("downloading from iCloud"),
            "top rule should embed source-aware header: {:?}",
            wide.top_rule,
        );
        assert!(
            wide.top_rule.contains("kei"),
            "top rule should brand the box with kei: {:?}",
            wide.top_rule,
        );
    }

    #[test]
    fn friendly_bar_width_grows_with_cols_within_clamp() {
        assert_eq!(friendly_bar_width(60), 48, "cols=60 -> bar=48");
        assert_eq!(friendly_bar_width(80), 68, "cols=80 -> bar=68");
        assert_eq!(friendly_bar_width(100), 88, "cols=100 -> bar=88");
        assert_eq!(friendly_bar_width(120), 108, "cols=120 -> bar=108");
        assert_eq!(
            friendly_bar_width(132),
            120,
            "cols=132 -> bar=120 (clamped)"
        );
        assert_eq!(
            friendly_bar_width(200),
            120,
            "cols=200 -> bar=120 (clamped)"
        );
        // Lower clamp: very narrow inputs (rare; we already gate to >=60 for
        // the card path) shouldn't return a bar narrower than 24.
        assert_eq!(friendly_bar_width(30), 24, "cols=30 -> bar=24 (clamped)");
        assert_eq!(friendly_bar_width(0), 24, "cols=0 -> bar=24 (clamped)");
    }

    #[test]
    fn friendly_sparkline_width_grows_with_cols_within_clamp() {
        assert_eq!(friendly_sparkline_width(60), 20, "cols=60 -> chart=20");
        assert_eq!(friendly_sparkline_width(80), 26, "cols=80 -> chart=26");
        assert_eq!(friendly_sparkline_width(100), 33, "cols=100 -> chart=33");
        assert_eq!(friendly_sparkline_width(120), 40, "cols=120 -> chart=40");
        assert_eq!(
            friendly_sparkline_width(144),
            48,
            "cols=144 -> chart=48 (clamped)"
        );
        assert_eq!(
            friendly_sparkline_width(177),
            48,
            "cols=177 -> chart=48 (clamped)"
        );
        assert_eq!(
            friendly_sparkline_width(200),
            48,
            "cols=200 -> chart=48 (clamped)"
        );
        // Lower clamp on narrow / unknown inputs.
        assert_eq!(
            friendly_sparkline_width(30),
            16,
            "cols=30 -> chart=16 (clamped)"
        );
        assert_eq!(
            friendly_sparkline_width(0),
            16,
            "cols=0 -> chart=16 (clamped)"
        );
    }

    #[test]
    fn friendly_wide_card_pos_pads_to_len_digit_count() {
        // total=999 -> 3 digits -> pos formatted as `{pos:>3}`.
        let bt = template_for_tier(Mode::Friendly, WidthTier::Wide, 80, 999);
        assert!(
            bt.template.contains("{pos:>3}/{len}"),
            "pos should be padded to 3 digits for total=999, got: {}",
            bt.template,
        );
        // total=10000 -> 5 digits.
        let bt = template_for_tier(Mode::Friendly, WidthTier::Wide, 80, 10_000);
        assert!(
            bt.template.contains("{pos:>5}/{len}"),
            "pos should be padded to 5 digits for total=10000, got: {}",
            bt.template,
        );
    }

    #[test]
    fn friendly_wide_is_six_line_card_with_animated_keys() {
        let wide = template_for_tier(Mode::Friendly, WidthTier::Wide, 80, 100);
        let template = &wide.template;
        // Leading blank line + top rule + three content rows + bottom
        // rule = six rows, joined by five `\n`s. The blank gives the
        // card breathing room from prior scrollback.
        assert_eq!(
            template.matches('\n').count(),
            5,
            "wide template should be six rows (1 blank + 5 content), got: {template:?}",
        );
        assert!(
            template.starts_with('\n'),
            "wide template must start with a blank row for breathing room: {template:?}",
        );
        // Vertical bar prefix on content lines.
        assert!(template.contains('\u{2502}'), "missing vertical bar │");
        // Custom-key references (rules + animated bar + indicatif spinner).
        assert!(template.contains("{top_rule}"));
        assert!(template.contains("{bottom_rule}"));
        assert!(template.contains("{bar_animated}"));
        assert!(template.contains("{spinner}"));
        // Other content fields.
        assert!(template.contains("{wide_msg}"));
        assert!(template.contains("{percent:>3}"));
        assert!(template.contains("{rate_sparkline}"));
        assert!(template.contains("/{len}"));
        assert!(template.contains("{smart_eta}"));
        // Top rule and bottom rule live on the CardTemplate struct, not in the
        // template; the closures pulse them per redraw.
        assert!(wide.top_rule.contains("downloading from iCloud"));
        assert!(wide.top_rule.starts_with('╭') && wide.top_rule.ends_with('╮'));
        assert!(wide.bottom_rule.starts_with('╰') && wide.bottom_rule.ends_with('╯'));
    }

    #[test]
    fn friendly_medium_is_six_line_card_without_smart_eta() {
        let medium = template_for_tier(Mode::Friendly, WidthTier::Medium, 70, 100);
        let template = &medium.template;
        // Same 1-blank + 5-content layout as wide; only the third row's
        // smart-ETA suffix is dropped to keep the line short.
        assert_eq!(
            template.matches('\n').count(),
            5,
            "medium template should be six rows (1 blank + 5 content), got: {template:?}",
        );
        assert!(
            template.starts_with('\n'),
            "medium template must start with a blank row for breathing room: {template:?}",
        );
        assert!(template.contains("{top_rule}"));
        assert!(template.contains("{bottom_rule}"));
        assert!(template.contains("{bar_animated}"));
        assert!(template.contains("{spinner}"));
        assert!(template.contains("{wide_msg}"));
        assert!(template.contains("{rate_sparkline}"));
        // Smart ETA dropped on medium width to keep the third line short.
        assert!(!template.contains("{smart_eta}"));
        assert!(medium.top_rule.starts_with('╭') && medium.top_rule.ends_with('╮'));
        assert!(medium.bottom_rule.starts_with('╰') && medium.bottom_rule.ends_with('╯'));
    }
}

/// Sub-cell heights ordered so index `i` represents `i/8` cell fill. Used to
/// pick the leading-edge cell character based on the fractional position.
/// Index 0 is empty (used as `EMPTY_CELL` instead), index 8 is full block.
const PARTIAL_HEIGHTS: &[char] = &[' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];

/// Empty-cell character used in the unfilled portion of the bar.
const EMPTY_CELL: char = '░';

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
pub(crate) struct BarSmoother {
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
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Advance the smoother toward `true_fraction` by one tick. Returns the
    /// displayed (smoothed) fraction in `[0.0, 1.0]`.
    ///
    /// Reaching the final position deserves to snap rather than asymptote
    /// forever, so we hard-set displayed once we're within a fraction of a
    /// cell of the target (or the target is exactly 1.0).
    pub(crate) fn tick(&mut self, true_fraction: f64) -> f64 {
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
pub(crate) fn render_bar(fraction: f64, width: usize) -> String {
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
    let fill_style = fill_style(clamped);
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
pub(crate) fn fill_style(fraction: f64) -> Style {
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
pub(crate) fn format_bandwidth(bytes_per_sec: f64) -> String {
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
fn empty_cell_style() -> Style {
    // 244 is medium grey in the 256-color palette; falls back gracefully on
    // 16-color terminals to plain "dim".
    Style::new().color256(244)
}

#[cfg(test)]
mod render_tests {
    use super::*;

    #[test]
    fn animated_bar_at_zero_fraction_shows_only_empty_cells() {
        let s = render_bar(0.0, 10);
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
        let s = render_bar(1.0, 10);
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
        let s = render_bar(0.5, 10);
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
        let s = render_bar(0.55, 10);
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
        let a = render_bar(0.42, 20);
        let b = render_bar(0.42, 20);
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
    fn fill_style_color_tiers() {
        // `console::Style::apply_to` only emits ANSI when colors are enabled
        // for the writer (TTY detection). cargo test runs without a TTY, so
        // we force-enable styling per call to verify the actual SGR codes.
        // SGR 32 = green, SGR 36 = cyan, SGR 1 = bold.
        let early = format!("{}", fill_style(0.10).force_styling(true).apply_to("x"));
        assert!(
            early.contains(";36m") || early.contains("\x1b[36m"),
            "early tier should be cyan-based: {early:?}"
        );
        assert!(
            early.contains("\x1b[1") || early.contains(";1m"),
            "early tier should be bold: {early:?}"
        );
        let mid = format!("{}", fill_style(0.50).force_styling(true).apply_to("x"));
        assert!(
            mid.contains("\x1b[36m") || mid.contains(";36m"),
            "mid tier should be cyan: {mid:?}"
        );
        let late = format!("{}", fill_style(0.85).force_styling(true).apply_to("x"));
        assert!(
            late.contains("\x1b[32m") || late.contains(";32m"),
            "late tier should be green: {late:?}"
        );
    }

    #[test]
    fn animated_bar_zero_width_returns_empty() {
        let s = render_bar(0.5, 0);
        assert_eq!(s, "");
    }

    #[test]
    fn animated_bar_clamps_fraction_above_one() {
        // Defensive: don't panic on bogus input; render as if fully filled.
        let s = render_bar(1.5, 10);
        let stripped = strip_ansi(&s);
        assert!(
            stripped.chars().all(|c| c == '█'),
            "over-100% clamps to full: {stripped:?}",
        );
    }

    #[test]
    fn animated_bar_clamps_negative_fraction() {
        // Defensive: treat negative as zero.
        let s = render_bar(-0.3, 10);
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

/// Eight block-char heights from `▁` (1/8 full) to `█` (8/8 full). All in the
/// "Block Elements" Unicode range, narrow east-asian width on every modern
/// terminal we've tested.
const HEIGHTS: &[char] = &['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// EMA smoothing factor. Lower = more smoothing, more lag. 0.25 keeps recent
/// activity visible while damping the on/off pattern from a file-counter
/// progress bar where most 100ms ticks have a raw delta of 0.
const EMA_ALPHA: f64 = 0.25;

/// Floor below which a smoothed rate renders as a space rather than the
/// thinnest block. Without this, decay-tail ticks render as `▁` for many
/// frames after work stops, giving a dragging look.
const RATE_RENDER_FLOOR: f64 = 0.05;

/// Rolling-window smoothed-rate tracker. Caller invokes `sample(position)`
/// each tick (typically 100ms) with the current absolute progress count; the
/// tracker computes a per-tick rate, smooths it via EMA so a file-counter
/// bar's binary deltas turn into a continuous wave, and stores the smoothed
/// rate for chart rendering.
#[derive(Debug, Clone)]
pub(crate) struct SparklineState {
    /// Smoothed rate samples (units / second). Each cell of the rendered
    /// chart corresponds to one entry; capacity sets the chart width.
    samples: VecDeque<f64>,
    capacity: usize,
    last_position: u64,
    last_sampled_at: Option<Instant>,
    /// Last EMA value. Carried forward when no new sample lands in a tick so
    /// the chart shows a slow decay rather than a hard drop to zero.
    smoothed_rate: f64,
}

impl SparklineState {
    /// Capacity is the number of cells in the rendered chart; values around
    /// 12-16 read well at 80-column terminals while still showing motion.
    #[must_use]
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            samples: VecDeque::with_capacity(capacity.max(1)),
            capacity: capacity.max(1),
            last_position: 0,
            last_sampled_at: None,
            smoothed_rate: 0.0,
        }
    }

    /// Record a new absolute-position sample. The first call seeds the prior
    /// position; subsequent calls compute a per-second rate, blend it into
    /// the EMA, and push the smoothed value into the chart buffer.
    pub(crate) fn sample(&mut self, position: u64) {
        self.sample_at(position, Instant::now());
    }

    /// Test seam for `sample`: takes the instant explicitly so tests can
    /// drive the buffer with deterministic timing.
    fn sample_at(&mut self, position: u64, now: Instant) {
        let elapsed_secs = self
            .last_sampled_at
            .map(|t| now.saturating_duration_since(t).as_secs_f64())
            .unwrap_or(0.0);
        self.last_sampled_at = Some(now);

        if elapsed_secs <= 0.0 {
            // First call seeds last_position; no rate to compute yet.
            self.last_position = position;
            return;
        }

        let delta = position.saturating_sub(self.last_position);
        self.last_position = position;

        // Raw per-second rate for this tick. Most ticks will be 0 with a
        // file-counter bar; that's expected and is exactly what EMA exists
        // to absorb.
        #[allow(
            clippy::cast_precision_loss,
            reason = "delta is at most a few thousand per tick; loss is sub-rate-unit"
        )]
        let raw_rate = (delta as f64) / elapsed_secs;
        self.smoothed_rate = EMA_ALPHA * raw_rate + (1.0 - EMA_ALPHA) * self.smoothed_rate;

        if self.samples.len() == self.capacity {
            self.samples.pop_front();
        }
        self.samples.push_back(self.smoothed_rate);
    }

    /// Render the smoothed-rate buffer as block chars normalised to the
    /// buffer's local maximum. Cells below `RATE_RENDER_FLOOR` render as a
    /// space so the chart tail fades cleanly when work stops.
    ///
    /// Always emits exactly `capacity` characters so the chart's visual width
    /// stays constant across the bar's lifetime. While the buffer fills, the
    /// left side is padded with spaces; the right side carries the newest
    /// data, so live activity always lives in the same column.
    #[must_use]
    pub(crate) fn render(&self) -> String {
        let pad = self.capacity.saturating_sub(self.samples.len());
        let mut out = String::with_capacity(self.capacity);
        for _ in 0..pad {
            out.push(' ');
        }
        if self.samples.is_empty() {
            return out;
        }
        let max = self
            .samples
            .iter()
            .copied()
            .fold(0.0_f64, f64::max)
            .max(RATE_RENDER_FLOOR);
        for &rate in &self.samples {
            if rate < RATE_RENDER_FLOOR {
                out.push(' ');
            } else {
                #[allow(
                    clippy::cast_precision_loss,
                    reason = "HEIGHTS.len() <= 8; precision loss is sub-cell"
                )]
                let scaled = (rate / max) * (HEIGHTS.len() as f64 - 1.0);
                #[allow(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "scaled is non-negative and bounded above by HEIGHTS.len() - 1"
                )]
                let idx = scaled.round() as usize;
                out.push(
                    HEIGHTS
                        .get(idx.min(HEIGHTS.len() - 1))
                        .copied()
                        .unwrap_or(' '),
                );
            }
        }
        out
    }

    /// Latest smoothed per-second rate. Drives the "X.X/s" label paired with
    /// the chart so the number and the chart agree on what's happening.
    #[must_use]
    pub(crate) fn rate_per_sec(&self) -> f64 {
        self.smoothed_rate
    }
}

#[cfg(test)]
mod sparkline_tests {
    use super::*;
    use std::time::Duration;

    fn ticks() -> impl Iterator<Item = Instant> {
        let start = Instant::now();
        (0..).map(move |i| start + Duration::from_millis(100 * i))
    }

    #[test]
    fn new_buffer_is_empty() {
        let s = SparklineState::new(12);
        assert!(s.samples.is_empty());
        assert_eq!(s.render(), "            "); // 12 spaces
    }

    #[test]
    fn first_sample_seeds_position_no_phantom_delta() {
        let mut s = SparklineState::new(12);
        let mut t = ticks();
        s.sample_at(100, t.next().unwrap());
        // First sample never produces a delta; buffer stays empty.
        assert!(s.samples.is_empty());
    }

    #[test]
    fn second_sample_records_delta() {
        let mut s = SparklineState::new(12);
        let mut t = ticks();
        s.sample_at(100, t.next().unwrap());
        s.sample_at(105, t.next().unwrap());
        assert_eq!(s.samples.len(), 1);
    }

    #[test]
    fn buffer_caps_at_capacity() {
        let mut s = SparklineState::new(4);
        let mut t = ticks();
        s.sample_at(0, t.next().unwrap());
        for i in 1..=10u64 {
            s.sample_at(i * 10, t.next().unwrap());
        }
        assert_eq!(s.samples.len(), 4);
    }

    #[test]
    fn render_uses_block_chars_with_at_least_one_char_per_cell() {
        let mut s = SparklineState::new(4);
        let mut t = ticks();
        s.sample_at(0, t.next().unwrap());
        s.sample_at(1, t.next().unwrap());
        s.sample_at(3, t.next().unwrap());
        s.sample_at(7, t.next().unwrap());
        s.sample_at(15, t.next().unwrap());
        let rendered = s.render();
        // 4 graphical cells; at full buffer no padding so chars().count() == 4.
        assert_eq!(rendered.chars().count(), 4);
        // Sample max should render as full block.
        assert!(rendered.contains('█'));
    }

    #[test]
    fn render_width_stays_constant_during_fill() {
        // Chart should be exactly `capacity` chars wide at every step from
        // empty buffer through fully populated. The user-facing complaint
        // was "the sparkline grows on first load" which would happen if
        // we rendered fewer chars while the buffer was filling.
        let mut s = SparklineState::new(8);
        assert_eq!(
            s.render().chars().count(),
            8,
            "empty buffer should still be 8 cells"
        );

        let mut t = ticks();
        // First sample is the seed (no push).
        s.sample_at(0, t.next().unwrap());
        assert_eq!(s.render().chars().count(), 8, "after seed, still 8 cells");

        for i in 1..=12u64 {
            s.sample_at(i, t.next().unwrap());
            assert_eq!(
                s.render().chars().count(),
                8,
                "after {i} pushes, render must be exactly 8 cells",
            );
        }
    }

    #[test]
    fn render_pads_left_so_newest_sample_is_on_the_right() {
        // With one real sample in an 8-cell chart, the right side carries
        // the data and the left is whitespace.
        let mut s = SparklineState::new(8);
        let mut t = ticks();
        s.sample_at(0, t.next().unwrap()); // seed
        s.sample_at(100, t.next().unwrap()); // first non-zero sample
        let rendered = s.render();
        let chars: Vec<char> = rendered.chars().collect();
        assert_eq!(chars.len(), 8);
        // First 7 chars are spaces (pad); last char is the populated cell.
        for (i, &c) in chars.iter().take(7).enumerate() {
            assert_eq!(c, ' ', "cell {i} should be space (padding)");
        }
        assert_ne!(chars[7], ' ', "rightmost cell should carry the new sample");
    }

    #[test]
    fn rate_converges_to_steady_state_under_constant_load() {
        // 100 units/sec sustained should converge the EMA to ~100 after a
        // dozen ticks. Convergence rate depends on EMA_ALPHA; with 0.25 we
        // expect 1 - (1-0.25)^12 = ~97% convergence at 12 ticks.
        let mut s = SparklineState::new(16);
        let start = Instant::now();
        s.sample_at(0, start);
        for i in 1..=20u64 {
            s.sample_at(i * 10, start + Duration::from_millis(100 * i));
        }
        let rate = s.rate_per_sec();
        assert!(
            (95.0..=105.0).contains(&rate),
            "EMA should be near 100 after 20 ticks of constant 100/s load, got {rate}",
        );
    }

    #[test]
    fn rate_decays_smoothly_when_work_stops() {
        // After a burst, no-progress ticks should decay the rate gradually,
        // not drop to zero. Each tick decays by (1-alpha) = 0.75.
        let mut s = SparklineState::new(16);
        let start = Instant::now();
        s.sample_at(0, start);
        s.sample_at(100, start + Duration::from_millis(100));
        let after_burst = s.rate_per_sec();
        assert!(
            after_burst > 100.0,
            "first non-zero rate should be at least the raw rate"
        );

        // Five no-op ticks. With alpha=0.25 the rate should decay to about
        // (0.75^5) ~= 0.237 of the burst, definitely above zero.
        for i in 2..=6u64 {
            s.sample_at(100, start + Duration::from_millis(100 * i));
        }
        let after_decay = s.rate_per_sec();
        assert!(
            after_decay > 0.0 && after_decay < after_burst,
            "rate should decay smoothly, got {after_decay} from {after_burst}",
        );
    }

    #[test]
    fn rate_is_zero_before_first_delta() {
        let s = SparklineState::new(8);
        assert!(
            s.rate_per_sec().abs() < f64::EPSILON,
            "expected exactly 0.0, got {}",
            s.rate_per_sec(),
        );
    }

    #[test]
    fn smoothed_chart_is_continuous_under_binary_input() {
        // Drive the buffer with a 1-every-3-ticks pattern (typical for a
        // file-counter bar where files take ~300ms each). Without smoothing
        // the chart would be `█  █  █  █` (binary). With EMA, it should be
        // a continuous wave with no `' '` in the populated cells.
        let mut s = SparklineState::new(12);
        let start = Instant::now();
        s.sample_at(0, start);
        let mut pos = 0u64;
        for i in 1..=20u64 {
            if i % 3 == 0 {
                pos += 1;
            }
            s.sample_at(pos, start + Duration::from_millis(100 * i));
        }
        let rendered = s.render();
        // After enough ticks, EMA stays above the floor for every cell once
        // it's populated. Not asserting exact heights (depends on alpha) but
        // asserting the chart does NOT degenerate to space-and-block alternation.
        let space_count = rendered.chars().filter(|c| *c == ' ').count();
        assert!(
            space_count <= 2,
            "chart should be a continuous wave under steady binary input, got {space_count} spaces in {rendered:?}",
        );
    }

    #[test]
    fn rewind_does_not_panic() {
        // Defensive: if a caller sends a smaller position than before, we
        // record a 0 raw rate rather than panicking on subtraction underflow.
        let mut s = SparklineState::new(4);
        let mut t = ticks();
        s.sample_at(100, t.next().unwrap());
        s.sample_at(50, t.next().unwrap());
        assert_eq!(s.samples.len(), 1);
    }

    #[test]
    fn capacity_one_still_works() {
        let mut s = SparklineState::new(1);
        let mut t = ticks();
        s.sample_at(0, t.next().unwrap());
        s.sample_at(5, t.next().unwrap());
        s.sample_at(10, t.next().unwrap());
        s.sample_at(15, t.next().unwrap());
        assert_eq!(s.samples.len(), 1);
        assert_eq!(s.render().chars().count(), 1);
    }

    #[test]
    fn zero_capacity_clamps_to_one() {
        let s = SparklineState::new(0);
        // Implementation detail: 0 capacity clamps to 1 so rendering never
        // produces an empty string that would collapse the bar layout.
        assert_eq!(s.capacity, 1);
    }

    #[test]
    fn flat_progress_renders_as_spaces() {
        // No movement at all means no blocks; render returns spaces.
        let mut s = SparklineState::new(4);
        let mut t = ticks();
        for _ in 0..5 {
            s.sample_at(50, t.next().unwrap());
        }
        let r = s.render();
        assert!(
            r.chars().all(|c| c == ' '),
            "expected all spaces, got {r:?}"
        );
    }
}

/// Format a known ETA in seconds as a friendly phrase.
///
/// Tiers:
/// - `>=3600s` (1h+): "about 1h 20m left, feel free to step away"
/// - `>=300s` (5m+): "around 12 minutes"
/// - `>=60s` (1-5m): "about N minutes"
/// - `<60s`: "almost there"
///
/// Off-mode callers should use indicatif's raw `{eta}` placeholder instead.
#[must_use]
fn format_known_eta(secs: u64) -> String {
    if secs >= 3600 {
        let hours = secs / 3600;
        let minutes = (secs % 3600) / 60;
        if minutes == 0 {
            format!("about {hours}h left, feel free to step away")
        } else {
            format!("about {hours}h {minutes}m left, feel free to step away")
        }
    } else if secs >= 300 {
        let minutes = secs.div_ceil(60);
        format!("around {minutes} minutes")
    } else if secs >= 60 {
        let minutes = secs.div_ceil(60);
        format!(
            "about {minutes} minute{}",
            if minutes == 1 { "" } else { "s" }
        )
    } else {
        "almost there".to_string()
    }
}

/// Tracks the "calculating" -> "still calculating" transition so the wording
/// only escalates when the unknown ETA persists past a threshold.
///
/// Lives inside the bar's `smart_eta` closure (one instance per bar),
/// wrapped in `Arc<Mutex<>>` because indicatif's `with_key` callbacks must
/// be `Send + Sync`. Contention is nil: the draw thread is the only writer
/// and runs at ~10Hz.
#[derive(Debug, Clone)]
pub(crate) struct EtaPhrasing {
    first_unknown: Option<Instant>,
    still_threshold: Duration,
}

impl Default for EtaPhrasing {
    fn default() -> Self {
        Self {
            first_unknown: None,
            still_threshold: Duration::from_secs(5),
        }
    }
}

impl EtaPhrasing {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Test helper: build with a configurable "still calculating" threshold so
    /// tests don't need to sleep for the real 5s.
    #[cfg(test)]
    fn with_threshold(threshold: Duration) -> Self {
        Self {
            first_unknown: None,
            still_threshold: threshold,
        }
    }

    /// Phrase a known ETA. Resets the "still calculating" timer.
    pub(crate) fn known(&mut self, secs: u64) -> String {
        self.first_unknown = None;
        format_known_eta(secs)
    }

    /// Phrase an unknown ETA, escalating to "still calculating" if it has
    /// persisted past `still_threshold`.
    pub(crate) fn unknown(&mut self) -> &'static str {
        let now = Instant::now();
        let first = *self.first_unknown.get_or_insert(now);
        if now.duration_since(first) >= self.still_threshold {
            "still calculating..."
        } else {
            "calculating..."
        }
    }
}

#[cfg(test)]
mod pace_tests {
    use super::*;
    use std::thread::sleep;

    #[test]
    fn under_one_minute_is_almost_there() {
        assert_eq!(format_known_eta(0), "almost there");
        assert_eq!(format_known_eta(30), "almost there");
        assert_eq!(format_known_eta(59), "almost there");
    }

    #[test]
    fn one_to_four_minutes_uses_about_n_minutes() {
        assert_eq!(format_known_eta(60), "about 1 minute");
        assert_eq!(format_known_eta(120), "about 2 minutes");
        assert_eq!(format_known_eta(240), "about 4 minutes");
    }

    #[test]
    fn five_to_sixty_minutes_uses_around_n_minutes() {
        assert_eq!(format_known_eta(300), "around 5 minutes");
        assert_eq!(format_known_eta(723), "around 13 minutes");
        assert_eq!(format_known_eta(3540), "around 59 minutes");
    }

    #[test]
    fn over_an_hour_says_step_away() {
        assert_eq!(
            format_known_eta(3600),
            "about 1h left, feel free to step away"
        );
        assert_eq!(
            format_known_eta(4523),
            "about 1h 15m left, feel free to step away"
        );
        assert_eq!(
            format_known_eta(7200),
            "about 2h left, feel free to step away"
        );
        assert_eq!(
            format_known_eta(7320),
            "about 2h 2m left, feel free to step away"
        );
    }

    #[test]
    fn unknown_starts_as_calculating() {
        let mut phrasing = EtaPhrasing::new();
        assert_eq!(phrasing.unknown(), "calculating...");
    }

    #[test]
    fn unknown_escalates_to_still_calculating_after_threshold() {
        let mut phrasing = EtaPhrasing::with_threshold(Duration::from_millis(50));
        assert_eq!(phrasing.unknown(), "calculating...");
        sleep(Duration::from_millis(70));
        assert_eq!(phrasing.unknown(), "still calculating...");
    }

    #[test]
    fn known_call_resets_unknown_timer() {
        let mut phrasing = EtaPhrasing::with_threshold(Duration::from_millis(50));
        assert_eq!(phrasing.unknown(), "calculating...");
        sleep(Duration::from_millis(70));
        // Known call should reset the timer.
        let _ = phrasing.known(100);
        assert_eq!(phrasing.unknown(), "calculating...");
    }
}
