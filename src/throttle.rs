//! Adaptive throttle for watch-mode sync intervals.
//!
//! When Apple rate-limits (HTTP 429 / 503) or service-unavailables (503),
//! the throttle scales up the watch interval. On clean cycles pressure
//! decays back to baseline. A [`ThrottleController`] lives for the lifetime
//! of the sync run and is consulted before every idle sleep.

/// What changed after a cycle completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThrottleDelta {
    /// Throttle engaged this cycle (pressure rose from zero).
    Engaged,
    /// Throttle disengaged this cycle (pressure dropped to zero).
    Disengaged,
    /// No crossing — either already engaged/staying engaged, or calm/staying calm.
    None,
}

/// Manages adaptive backoff of the watch interval between sync cycles.
/// Scales up on rate-limit pressure, decays back to baseline.
#[derive(Debug)]
pub(crate) struct ThrottleController {
    baseline_interval_secs: u64,
    current_interval_secs: u64,
    pressure: f64,
    decay_per_cycle: f64,
    max_multiplier: f64,
}

impl ThrottleController {
    /// Create a new throttle with the user's configured baseline interval.
    ///
    /// `baseline_interval_secs` of `0` means one-shot mode; the throttle
    /// is a no-op (`current_interval_secs` stays `0`).
    pub(crate) fn new(baseline_interval_secs: u64) -> Self {
        Self {
            baseline_interval_secs,
            current_interval_secs: baseline_interval_secs,
            pressure: 0.0,
            decay_per_cycle: 0.1,
            max_multiplier: 3.0,
        }
    }

    /// Record the end of a sync cycle and update throttle state.
    ///
    /// `rate_limit_count` — number of HTTP 429/503 observations this cycle.
    /// `success` — whether the cycle completed without session expiry or
    ///   partial failure (reserved for future policy refinement; currently
    ///   only `rate_limit_count` drives pressure changes).
    ///
    /// Returns [`ThrottleDelta::Engaged`] when pressure crosses from 0 to >0,
    /// [`ThrottleDelta::Disengaged`] when it returns to 0, and
    /// [`ThrottleDelta::None`] otherwise.
    #[allow(
        clippy::cast_precision_loss,
        reason = "rate_limit_count is small (per-cycle observations) and 0.25 scaling is deliberate"
    )]
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "interval rounds to nearest whole second; negative and fractional results are impossible here"
    )]
    pub(crate) fn cycle_complete(
        &mut self,
        rate_limit_count: usize,
        _success: bool,
    ) -> ThrottleDelta {
        // One-shot mode: throttle is a no-op.
        if self.baseline_interval_secs == 0 {
            self.pressure = 0.0;
            self.current_interval_secs = 0;
            return ThrottleDelta::None;
        }

        let was_engaged = self.is_engaged();

        if rate_limit_count > 0 {
            self.pressure += 0.25 * rate_limit_count as f64;
            if self.pressure > 1.0 {
                self.pressure = 1.0;
            }
        } else {
            self.pressure *= 1.0 - self.decay_per_cycle;
            if self.pressure < 0.01 {
                self.pressure = 0.0;
            }
        }

        self.current_interval_secs = if self.baseline_interval_secs == 0 {
            0
        } else {
            let multiplier = 1.0 + self.pressure * (self.max_multiplier - 1.0);
            let scaled = self.baseline_interval_secs as f64 * multiplier;
            scaled.round() as u64
        };

        let is_engaged = self.is_engaged();
        if !was_engaged && is_engaged {
            ThrottleDelta::Engaged
        } else if was_engaged && !is_engaged {
            ThrottleDelta::Disengaged
        } else {
            ThrottleDelta::None
        }
    }

    /// Current watch interval in seconds, which may be scaled above baseline.
    pub(crate) fn current_interval_secs(&self) -> u64 {
        self.current_interval_secs
    }

    /// Current throttle pressure, 0.0 (calm) .. 1.0 (max).
    pub(crate) fn pressure(&self) -> f64 {
        self.pressure
    }

    /// Whether the throttle is currently engaged (pressure > 0).
    pub(crate) fn is_engaged(&self) -> bool {
        self.pressure > 0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compare two f64 values within a tolerance appropriate for the
    /// throttle's arithmetic (multiplication, 0.25 steps, 0.9 decay).
    fn assert_pressure_eq(actual: f64, expected: f64) {
        let abs_diff = (actual - expected).abs();
        assert!(
            abs_diff < 1e-9,
            "pressure mismatch: actual={actual}, expected={expected}, diff={abs_diff}"
        );
    }

    #[test]
    fn new_has_zero_pressure_and_baseline_interval() {
        let t = ThrottleController::new(300);
        assert_pressure_eq(t.pressure(), 0.0);
        assert_eq!(t.current_interval_secs(), 300);
        assert!(!t.is_engaged());
    }

    #[test]
    fn single_429_rises_to_quarter_pressure() {
        let mut t = ThrottleController::new(300);
        let delta = t.cycle_complete(1, true);
        assert_eq!(delta, ThrottleDelta::Engaged);
        assert_pressure_eq(t.pressure(), 0.25);
        // interval = 300 * (1 + 0.25 * 2) = 300 * 1.5 = 450
        assert_eq!(t.current_interval_secs(), 450);
    }

    #[test]
    fn four_429s_caps_pressure_at_one() {
        let mut t = ThrottleController::new(300);
        let delta = t.cycle_complete(4, true);
        assert_eq!(delta, ThrottleDelta::Engaged);
        assert_pressure_eq(t.pressure(), 1.0);
        // interval = 300 * (1 + 1.0 * 2) = 900 (3x baseline)
        assert_eq!(t.current_interval_secs(), 900);
    }

    #[test]
    fn clean_cycle_decays_by_ten_percent() {
        let mut t = ThrottleController::new(300);
        t.cycle_complete(4, true); // pressure = 1.0
        assert_pressure_eq(t.pressure(), 1.0);

        let delta = t.cycle_complete(0, true);
        assert_eq!(delta, ThrottleDelta::None);
        assert_pressure_eq(t.pressure(), 0.9);
        // interval = 300 * (1 + 0.9 * 2) = 300 * 2.8 = 840
        assert_eq!(t.current_interval_secs(), 840);
    }

    #[test]
    fn pressure_below_point_zero_one_snaps_to_zero() {
        let mut t = ThrottleController::new(300);
        t.cycle_complete(1, true); // pressure = 0.25
                                   // Decay until below 0.01 (needs ~31 cycles with 0.1 decay)
        for _ in 0..40 {
            t.cycle_complete(0, true);
        }
        assert_pressure_eq(t.pressure(), 0.0);
        assert_eq!(t.current_interval_secs(), 300);
        assert!(!t.is_engaged());
    }

    #[test]
    fn disengage_fires_when_pressure_returns_to_zero() {
        let mut t = ThrottleController::new(300);
        t.cycle_complete(1, true); // engage
        assert!(t.is_engaged());

        // Decay back to zero (~31 cycles with 0.1 decay from 0.25)
        let mut disengaged = false;
        for _ in 0..40 {
            if t.cycle_complete(0, true) == ThrottleDelta::Disengaged {
                disengaged = true;
                break;
            }
        }
        assert!(
            disengaged,
            "expected Disengaged delta before 40 clean cycles"
        );
        assert!(!t.is_engaged());
    }

    #[test]
    fn baseline_zero_is_no_op() {
        let mut t = ThrottleController::new(0);
        assert_eq!(t.cycle_complete(10, true), ThrottleDelta::None);
        assert_pressure_eq(t.pressure(), 0.0);
        assert_eq!(t.current_interval_secs(), 0);
    }

    #[test]
    fn engaged_only_on_first_crossing() {
        let mut t = ThrottleController::new(300);
        assert_eq!(t.cycle_complete(1, true), ThrottleDelta::Engaged);
        assert_eq!(t.cycle_complete(1, true), ThrottleDelta::None);
        assert_eq!(t.cycle_complete(1, true), ThrottleDelta::None);
    }

    #[test]
    fn partial_decay_then_repressure() {
        let mut t = ThrottleController::new(300);
        t.cycle_complete(4, true); // pressure = 1.0, interval = 900
        t.cycle_complete(0, true); // pressure = 0.9, interval = 840
        t.cycle_complete(0, true); // pressure = 0.81, interval = 786

        // Re-pressure from 0.81
        let delta = t.cycle_complete(1, true);
        assert_eq!(delta, ThrottleDelta::None); // already engaged
                                                // 0.81 + 0.25 = 1.06 -> clamped to 1.0
        assert_pressure_eq(t.pressure(), 1.0);
    }

    #[test]
    fn interval_rounding() {
        let mut t = ThrottleController::new(100);
        t.cycle_complete(1, true); // pressure 0.25, interval = 100 * 1.5 = 150
        assert_eq!(t.current_interval_secs(), 150);

        // Decay to 0.225 -> interval = 100 * 1.45 = 145
        t.cycle_complete(0, true);
        assert_eq!(t.current_interval_secs(), 145);
    }
}
