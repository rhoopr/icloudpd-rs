//! Throughput sparkline: a rolling-window block-char chart of recent progress.
//!
//! Sized for inline rendering inside the friendly progress bar. Holds a small
//! ring buffer of per-tick deltas and renders each as one of eight block-char
//! heights normalized to the buffer's local max. No I/O; thread-safe via
//! external `Arc<Mutex<...>>` wrapping at call sites.

use std::collections::VecDeque;
use std::time::Instant;

/// Eight block-char heights from `▁` (1/8 full) to `█` (8/8 full). All in the
/// "Block Elements" Unicode range, narrow east-asian width on every modern
/// terminal we've tested.
pub const HEIGHTS: &[char] = &['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

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
pub struct SparklineState {
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
    pub fn new(capacity: usize) -> Self {
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
    pub fn sample(&mut self, position: u64) {
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
    pub fn render(&self) -> String {
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
    pub fn rate_per_sec(&self) -> f64 {
        self.smoothed_rate
    }
}

#[cfg(test)]
mod tests {
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
