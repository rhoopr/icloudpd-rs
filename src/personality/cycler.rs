//! Driver that cycles a `VerbPool` onto a bar's `wide_msg` until cancelled.
//!
//! Used during the dead time between seeding the bar and the first download
//! result. Without it, the user sees a static "scanning..." string for as
//! long as the listing phase takes (10s+ on large libraries with thousands
//! of assets).
//!
//! In `Mode::Off` (and any non-friendly future mode), the cycler seeds the
//! same static "scanning..." label and skips spawning the task, so machine-
//! output and v0.13 byte-for-byte parity are preserved. Cancellation is
//! idempotent and cheap (single atomic store) so the consumer can safely
//! call it on every per-file iteration.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use indicatif::ProgressBar;

use crate::personality::verbs::VerbPool;
use crate::personality::Mode;

/// Default cadence between verb advances. Slow enough to be readable; fast
/// enough that a 30-second listing pass cycles through every verb in the
/// pool at least once.
const DEFAULT_INTERVAL: Duration = Duration::from_millis(600);

/// Drives a `pb`'s `wide_msg` line with a cycling verb pool until cancelled.
///
/// Drop the cycler (or call `cancel`) when the consumer takes over the bar's
/// message; the first per-file `set_message` should immediately precede
/// cancellation to avoid a verb/filename flicker.
///
/// The spawned task holds a clone of `stop`, so dropping the cycler signals
/// the task to exit at its next tick. We don't keep a `JoinHandle` because
/// dropping the handle wouldn't shorten the cooperative cancellation window
/// (tokio::spawn detaches on drop) and `abort` would leave the bar in an
/// indeterminate frame.
#[derive(Debug)]
pub struct PhaseCycler {
    stop: Arc<AtomicBool>,
}

impl PhaseCycler {
    /// Seed the bar's message and (in friendly mode) spawn the verb-cycling
    /// task. In off mode, seeds a static "<label> · scanning..." and returns
    /// a no-op cycler whose `cancel` and `Drop` are no-ops.
    #[must_use]
    pub fn spawn(pb: ProgressBar, pass_label: String, mode: Mode) -> Self {
        Self::spawn_with_interval(pb, pass_label, mode, DEFAULT_INTERVAL)
    }

    /// Seed-and-spawn, parameterised on cycle interval. Tests use this with
    /// a short interval to avoid sleeping for full production cadences.
    #[must_use]
    pub fn spawn_with_interval(
        pb: ProgressBar,
        pass_label: String,
        mode: Mode,
        interval: Duration,
    ) -> Self {
        if !mode.is_friendly() {
            pb.set_message(format!("{pass_label} \u{00b7} scanning..."));
            return Self {
                stop: Arc::new(AtomicBool::new(true)),
            };
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = Arc::clone(&stop);
        let mut pool = VerbPool::new();
        // Seed synchronously so the bar's first frame already shows a verb,
        // even if cancellation or runtime shutdown beats the first interval
        // tick.
        pb.set_message(format!("{pass_label} \u{00b7} {}", pool.next()));
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            // tokio::time::interval fires the first tick immediately.
            // Eat it so we don't redundantly set the message we just seeded.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                if stop_clone.load(Ordering::Acquire) {
                    break;
                }
                pb.set_message(format!("{pass_label} \u{00b7} {}", pool.next()));
            }
        });
        Self { stop }
    }

    /// Stop the cycling task on its next tick. Idempotent and cheap; the
    /// consumer can call this on every per-file completion without a guard.
    pub fn cancel(&self) {
        self.stop.store(true, Ordering::Release);
    }
}

impl Drop for PhaseCycler {
    fn drop(&mut self) {
        self.cancel();
        // We don't await the handle: indicatif's set_message lands in a
        // bounded buffer that the multi's draw thread drains, so a stale
        // tick after Drop returns is at worst one redundant frame that
        // gets overwritten by the next consumer set_message. The tokio
        // runtime cancels orphaned tasks on shutdown.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::personality::Mode;
    use indicatif::ProgressBar;

    #[tokio::test]
    async fn off_mode_seeds_scanning_string_and_does_not_cycle() {
        let pb = ProgressBar::hidden();
        let cycler = PhaseCycler::spawn(pb.clone(), "unfiled".to_string(), Mode::Off);
        assert_eq!(pb.message(), "unfiled \u{00b7} scanning...");
        // Off mode must not spawn a cycling task. Wait long enough that any
        // friendly-mode cycler would have ticked, then assert the message
        // is unchanged.
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            pb.message(),
            "unfiled \u{00b7} scanning...",
            "off mode must not advance the message",
        );
        // Cancel is a no-op when no task ran; AtomicBool starts true.
        cycler.cancel();
    }

    #[tokio::test]
    async fn friendly_mode_seeds_first_verb_synchronously() {
        let pb = ProgressBar::hidden();
        let cycler = PhaseCycler::spawn_with_interval(
            pb.clone(),
            "unfiled".to_string(),
            Mode::Friendly,
            Duration::from_millis(50),
        );
        // The seed happens inside spawn before the task runs, so the first
        // frame is visible synchronously after spawn returns.
        let msg = pb.message().to_string();
        assert!(
            msg.starts_with("unfiled \u{00b7} "),
            "first message should carry the pass_label prefix, got {msg:?}",
        );
        assert_ne!(
            msg, "unfiled \u{00b7} scanning...",
            "friendly mode must never use the static scanning seed",
        );
        assert_eq!(msg, "unfiled \u{00b7} looking around");
        cycler.cancel();
    }

    #[tokio::test]
    async fn friendly_mode_advances_verb_after_interval() {
        let pb = ProgressBar::hidden();
        let cycler = PhaseCycler::spawn_with_interval(
            pb.clone(),
            "trip 2024".to_string(),
            Mode::Friendly,
            Duration::from_millis(20),
        );
        let initial = pb.message().to_string();
        // Sample throughout the sleep window. The cycle can return to the
        // initial verb if the sample lands on a full-pool boundary, so we
        // assert "at least one sample differed" rather than "the final
        // sample differs". 20ms cadence * 8 samples * 25ms sleep each =
        // 200ms total, plenty of room for multiple advances.
        let mut saw_change = false;
        for _ in 0..8 {
            tokio::time::sleep(Duration::from_millis(25)).await;
            let now = pb.message().to_string();
            assert!(
                now.starts_with("trip 2024 \u{00b7} "),
                "cycled messages must keep the pass_label prefix, got {now:?}",
            );
            if now != initial {
                saw_change = true;
            }
        }
        assert!(
            saw_change,
            "verb should have advanced past initial at least once during 200ms",
        );
        cycler.cancel();
    }

    #[tokio::test]
    async fn cancel_stops_further_cycling() {
        let pb = ProgressBar::hidden();
        let cycler = PhaseCycler::spawn_with_interval(
            pb.clone(),
            "unfiled".to_string(),
            Mode::Friendly,
            Duration::from_millis(10),
        );
        tokio::time::sleep(Duration::from_millis(60)).await;
        cycler.cancel();
        // Wait long enough for the cancel to be observed plus a few ticks
        // worth of slack for the spawned task to exit.
        tokio::time::sleep(Duration::from_millis(60)).await;
        let frozen = pb.message().to_string();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            frozen,
            pb.message().to_string(),
            "message should be frozen after cancel",
        );
    }

    #[tokio::test]
    async fn cancel_is_idempotent_under_hot_call() {
        let pb = ProgressBar::hidden();
        let cycler = PhaseCycler::spawn_with_interval(
            pb.clone(),
            "unfiled".to_string(),
            Mode::Friendly,
            Duration::from_millis(50),
        );
        // Mirrors the per-file consumer path: cancel called every iteration
        // once the bar's message is taken over by a real filename.
        for _ in 0..1000 {
            cycler.cancel();
        }
    }

    #[tokio::test]
    async fn drop_cancels_without_explicit_call() {
        let pb = ProgressBar::hidden();
        {
            let _cycler = PhaseCycler::spawn_with_interval(
                pb.clone(),
                "unfiled".to_string(),
                Mode::Friendly,
                Duration::from_millis(10),
            );
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        // After drop, the bar's message must stop changing.
        tokio::time::sleep(Duration::from_millis(60)).await;
        let frozen = pb.message().to_string();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(frozen, pb.message().to_string());
    }
}
