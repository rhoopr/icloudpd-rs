//! Human ETA wording. Numeric ETAs are precise but boring; the same number in
//! prose ("about 1h 20m left, feel free to step away") tells the user how to
//! plan around it.
//!
//! Pure functions where possible. The "calculating" -> "still calculating"
//! transition is stateful, so it lives in `EtaPhrasing` which the caller polls.

use std::time::{Duration, Instant};

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
pub fn format_known_eta(secs: u64) -> String {
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
pub struct EtaPhrasing {
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
    pub fn new() -> Self {
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
    pub fn known(&mut self, secs: u64) -> String {
        self.first_unknown = None;
        format_known_eta(secs)
    }

    /// Phrase an unknown ETA, escalating to "still calculating" if it has
    /// persisted past `still_threshold`.
    pub fn unknown(&mut self) -> &'static str {
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
mod tests {
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
