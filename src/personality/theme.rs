//! Visual theme: spinner glyphs, bar templates, semantic color roles.
//!
//! All glyph sets and templates are static, so the theme can be tested
//! without a live terminal. Color is applied at render time via
//! `console::Style`, which honours `NO_COLOR` and TTY detection on its own.

use crate::personality::Mode;

/// Spinner glyph set per phase. Indicatif consumes a `&[&str]` of frames.
#[allow(
    dead_code,
    reason = "spinner sets land in delight-B/C with auth/2FA/watch wires"
)]
pub mod spinners {
    /// Auth / session check (Dots, ~80ms).
    pub const AUTH: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

    /// Listing iCloud (Dots2, ~80ms).
    pub const LISTING: &[&str] = &["⠋", "⠙", "⠚", "⠞", "⠖", "⠦", "⠴", "⠲", "⠳", "⠓"];

    /// Verify / hash pass (Dots3, ~120ms).
    pub const VERIFY: &[&str] = &["⡀", "⡄", "⡆", "⡇", "⠇", "⠃", "⠁", "⠀"];

    /// EXIF / sidecar write (block oscillator, ~100ms).
    pub const EXIF: &[&str] = &[
        "▏", "▎", "▍", "▌", "▋", "▊", "▉", "█", "▉", "▊", "▋", "▌", "▍", "▎", "▏", " ",
    ];

    /// 2FA wait (slow breathing dots, 4-frame ~400ms).
    pub const TWOFA: &[&str] = &["⠁", "⠂", "⠄", "⠂"];

    /// Watch idle (static dot; included as a single-frame "set" for API
    /// consistency).
    pub const WATCH_IDLE: &[&str] = &["·"];

    /// ASCII fallback for terminals that fail Unicode width detection.
    pub const ASCII: &[&str] = &["|", "/", "-", "\\"];
}

/// Width tier for adaptive bar templates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WidthTier {
    /// Wide: full template with elapsed, bar, counts, ETA, message.
    Wide,
    /// Medium: drop elapsed, narrower bar.
    Medium,
    /// Narrow: bar + counts only.
    Narrow,
}

impl WidthTier {
    /// Choose tier from terminal column count. Falls back to wide on unknown.
    #[must_use]
    pub fn from_cols(cols: Option<u16>) -> Self {
        match cols {
            Some(c) if c < 60 => Self::Narrow,
            Some(c) if c < 80 => Self::Medium,
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
pub fn progress_chars(mode: Mode) -> &'static str {
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
/// Friendly mode wraps a five-line "card" around the work whose top and
/// bottom rules are sized to the terminal so the box stays a true rectangle:
/// ```text
/// ╭── downloading ─────────────────────────
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
/// `pipeline::create_progress_bar` via `ProgressStyle::with_key`.
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
pub struct BarTemplate {
    pub template: String,
    pub top_rule: String,
    pub bottom_rule: String,
}

#[must_use]
pub fn download_bar_template(mode: Mode, tier: WidthTier, cols: u16, total: u64) -> BarTemplate {
    match (mode, tier) {
        (Mode::Off, _) => BarTemplate {
            template: "[{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta}) {msg}"
                .to_string(),
            top_rule: String::new(),
            bottom_rule: String::new(),
        },
        (Mode::Friendly, WidthTier::Wide) => {
            friendly_card(cols, total, friendly_bar_width(cols), true)
        }
        (Mode::Friendly, WidthTier::Medium) => {
            friendly_card(cols, total, friendly_bar_width(cols), false)
        }
        (Mode::Friendly, WidthTier::Narrow) => BarTemplate {
            template: "{bar:16.cyan/blue} {pos}/{len}".to_string(),
            top_rule: String::new(),
            bottom_rule: String::new(),
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
pub fn friendly_bar_width(cols: u16) -> u16 {
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
pub fn friendly_sparkline_width(cols: u16) -> u16 {
    (cols / 3).clamp(16, 48)
}

/// Build a friendly five-line card sized to `cols`.
///
/// Returns the indicatif template (referencing custom keys `{top_rule}`,
/// `{bottom_rule}`, `{bar_animated}`, `{spinner}`) plus the rendered rule
/// strings the closures will pulse-color on each redraw.
fn friendly_card(cols: u16, total: u64, bar_width: u16, with_smart_eta: bool) -> BarTemplate {
    // Rule width: cols - 1 so a final newline / cursor reset doesn't bump the
    // bar onto a phantom line on terminals that auto-wrap at exactly cols.
    let rule_total = cols.saturating_sub(1).max(20) as usize;
    let header = " downloading ";
    // Top rule: ╭── downloading ───...─╮
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
    // bar_width passes through to the animated-bar custom key (which reads
    // it from a closure), but indicatif's measure-text-width still reads the
    // bar's slot in the template, so we encode the width in the placeholder
    // name itself: `{bar_animated:N}` where N is the visual width. The
    // closure parses the suffix back out via state metadata.
    let pos_width = total.checked_ilog10().map_or(1, |n| n + 1) as usize;

    // Bar width carries via the closure capture in pipeline.rs; the template
    // just references the custom key by name. `{bar_animated}` and other
    // custom keys (`{top_rule}`, `{bottom_rule}`, `{rate_sparkline}`,
    // `{smart_eta}`) are registered there. `{spinner}` is an indicatif
    // built-in that animates against the bar's tick chars.
    //
    // We embed bar_width into the placeholder name so a future caller could
    // look it up if needed; the closure ignores it but it documents intent.
    let template = if with_smart_eta {
        format!(
            "{{top_rule}}\n│  {{wide_msg}}\n│  {{bar_animated}} {{percent:>3}}% {{spinner}}\n│  {{rate_sparkline}}  {{pos:>{pos_width}}}/{{len}}  ·  {{smart_eta}}\n{{bottom_rule}}"
        )
    } else {
        format!(
            "{{top_rule}}\n│  {{wide_msg}}\n│  {{bar_animated}} {{percent:>3}}% {{spinner}}\n│  {{rate_sparkline}}  {{pos:>{pos_width}}}/{{len}}\n{{bottom_rule}}"
        )
    };
    let _ = bar_width; // captured via pipeline.rs closure; no template slot.
    BarTemplate {
        template,
        top_rule: top,
        bottom_rule: bottom,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_tier_from_cols() {
        assert_eq!(WidthTier::from_cols(Some(40)), WidthTier::Narrow);
        assert_eq!(WidthTier::from_cols(Some(59)), WidthTier::Narrow);
        assert_eq!(WidthTier::from_cols(Some(60)), WidthTier::Medium);
        assert_eq!(WidthTier::from_cols(Some(79)), WidthTier::Medium);
        assert_eq!(WidthTier::from_cols(Some(80)), WidthTier::Wide);
        assert_eq!(WidthTier::from_cols(Some(200)), WidthTier::Wide);
        assert_eq!(WidthTier::from_cols(None), WidthTier::Wide);
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
    fn off_template_matches_v013_exactly() {
        let v013 = "[{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta}) {msg}";
        let off = download_bar_template(Mode::Off, WidthTier::Wide, 80, 100);
        assert_eq!(off.template, v013);
        assert!(off.top_rule.is_empty());
        assert!(off.bottom_rule.is_empty());
        // Off mode ignores tier so machine-output stability is unconditional.
        let off_narrow = download_bar_template(Mode::Off, WidthTier::Narrow, 80, 100);
        assert_eq!(off_narrow.template, v013);
    }

    #[test]
    fn friendly_narrow_drops_elapsed_and_eta() {
        let narrow = download_bar_template(Mode::Friendly, WidthTier::Narrow, 50, 30);
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
        let wide = download_bar_template(Mode::Friendly, WidthTier::Wide, 80, 30);
        // Top and bottom rules are now stored separately on BarTemplate so
        // pipeline.rs can wrap them in pulse-color closures. Width must still
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
            wide.top_rule.contains("downloading"),
            "top rule should embed phase header: {:?}",
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
        let bt = download_bar_template(Mode::Friendly, WidthTier::Wide, 80, 999);
        assert!(
            bt.template.contains("{pos:>3}/{len}"),
            "pos should be padded to 3 digits for total=999, got: {}",
            bt.template,
        );
        // total=10000 -> 5 digits.
        let bt = download_bar_template(Mode::Friendly, WidthTier::Wide, 80, 10_000);
        assert!(
            bt.template.contains("{pos:>5}/{len}"),
            "pos should be padded to 5 digits for total=10000, got: {}",
            bt.template,
        );
    }

    #[test]
    fn friendly_wide_is_five_line_card_with_animated_keys() {
        let wide = download_bar_template(Mode::Friendly, WidthTier::Wide, 80, 100);
        let template = &wide.template;
        // Top rule key, three content lines, bottom rule key. Four `\n`s.
        assert_eq!(
            template.matches('\n').count(),
            4,
            "wide template should be five lines, got: {template:?}",
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
        // Top rule and bottom rule live on the BarTemplate struct, not in the
        // template; the closures pulse them per redraw.
        assert!(wide.top_rule.contains("downloading"));
        assert!(wide.top_rule.starts_with('╭') && wide.top_rule.ends_with('╮'));
        assert!(wide.bottom_rule.starts_with('╰') && wide.bottom_rule.ends_with('╯'));
    }

    #[test]
    fn friendly_medium_is_five_line_card_without_smart_eta() {
        let medium = download_bar_template(Mode::Friendly, WidthTier::Medium, 70, 100);
        let template = &medium.template;
        assert_eq!(
            template.matches('\n').count(),
            4,
            "medium template should be five lines, got: {template:?}",
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

    #[test]
    fn spinner_sets_are_non_empty() {
        for set in [
            spinners::AUTH,
            spinners::LISTING,
            spinners::VERIFY,
            spinners::EXIF,
            spinners::TWOFA,
            spinners::WATCH_IDLE,
            spinners::ASCII,
        ] {
            assert!(!set.is_empty(), "spinner set must have at least one frame");
            for frame in set {
                assert!(!frame.is_empty(), "spinner frame must be non-empty");
            }
        }
    }
}
