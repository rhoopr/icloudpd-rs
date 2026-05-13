//! Notification support for unattended operation.
//!
//! Provides a severity-tagged event taxonomy and a multi-backend notifier.
//! Backends include script execution, desktop notifications (stub, PR 2),
//! and webhooks (stub, PR 3). Script execution is the only operational
//! backend in this PR and preserves full backward compatibility.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::config::WebhookConfig;

// ── Severity ────────────────────────────────────────────────────────

/// Severity level for notification routing.
///
/// Backends can filter by severity so that e.g. desktop only shows Warn+,
/// while a script always gets everything. Ordered: `Silent < Info < Warn < Critical`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Severity {
    /// Not surfaced to any user-facing backend.
    Silent,
    /// Informational — sync started, completed, resumed.
    Info,
    /// Warning — partial failure, rate limiting, drift, auth expiring.
    Warn,
    /// Critical — session expired, disk full, all downloads failed.
    Critical,
}

// ── FailureMode ─────────────────────────────────────────────────────

/// Failure classification for [`Event::SyncFailed`].
///
/// [`SessionExpired`] and [`AllFailed`] are not constructed in PR 1;
/// they are wired in PR 2 (desktop notifications) and PR 3 (webhooks).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureMode {
    /// Some downloads failed, but the cycle otherwise completed.
    Partial(usize),
    /// Authentication session expired and re-authentication failed.
    /// Constructed in PRs 2 & 3.
    #[allow(dead_code, reason = "constructed in PRs 2 & 3")]
    SessionExpired,
    /// All downloads failed for reasons other than session expiry.
    /// Constructed in PRs 2 & 3.
    #[allow(dead_code, reason = "constructed in PRs 2 & 3")]
    AllFailed,
}

// ── Event ───────────────────────────────────────────────────────────

/// Events that trigger notification backends.
///
/// Each variant carries a [`Severity`] tag so backends can route by
/// importance. The `Copy` derive is intentionally omitted because
/// [`SyncFailed`](Event::SyncFailed) wraps a [`FailureMode`] — clone
/// is still cheap (a `usize` discriminant + one word of data).
///
/// New variants (`DiskLow` through `Resumed`) are stub-only in PR 1;
/// events are fired in PR 2 (desktop) and PR 3 (webhooks).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Event {
    // ── Existing (re-tagged) ────────────────────────────────────
    /// 2FA code is needed (session expired in headless mode).
    TwoFaRequired,
    /// A sync cycle is about to run (fires after skip-check, before run_cycle).
    SyncStarted,
    /// Sync cycle completed successfully.
    SyncComplete,
    /// Sync cycle had failures. Severity depends on [`FailureMode`]:
    /// `Partial` → Warn; `SessionExpired` / `AllFailed` → Critical.
    SyncFailed(FailureMode),
    /// Session expired and re-authentication failed.
    SessionExpired,

    // ── New (v0.15) ────────────────────────────────────────────
    /// Low disk space (< 100 MiB free on download volume).
    #[allow(dead_code, reason = "event fired in PRs 2 & 3")]
    DiskLow,
    /// Disk space recovered above low threshold.
    #[allow(dead_code, reason = "event fired in PRs 2 & 3")]
    DiskRecovered,
    /// Session age > 80 % of typical lifetime (pre-warning before expiry).
    #[allow(dead_code, reason = "event fired in PRs 2 & 3")]
    AuthExpiring,
    /// 429 / 503 received this sync cycle.
    #[allow(dead_code, reason = "event fired in PRs 2 & 3")]
    RateLimited,
    /// Adaptive throttle engaged (backoff active).
    #[allow(dead_code, reason = "event fired in PRs 2 & 3")]
    ThrottleEngaged,
    /// Adaptive throttle disengaged (backoff decayed to baseline).
    #[allow(dead_code, reason = "event fired in PRs 2 & 3")]
    ThrottleDisengaged,
    /// Asset count / size mismatch vs iCloud enumeration response.
    #[allow(dead_code, reason = "event fired in PRs 2 & 3")]
    DriftDetected,
    /// Sync paused (user or disk-pressure).
    #[allow(dead_code, reason = "event fired in PRs 2 & 3")]
    Paused,
    /// Sync resumed after pause.
    #[allow(dead_code, reason = "event fired in PRs 2 & 3")]
    Resumed,
}

impl Event {
    /// Human-readable event name for script env var `KEI_EVENT`.
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::TwoFaRequired => "2fa_required",
            Self::SyncStarted => "sync_started",
            Self::SyncComplete => "sync_complete",
            Self::SyncFailed(_) => "sync_failed",
            Self::SessionExpired => "session_expired",
            Self::DiskLow => "disk_low",
            Self::DiskRecovered => "disk_recovered",
            Self::AuthExpiring => "auth_expiring",
            Self::RateLimited => "rate_limited",
            Self::ThrottleEngaged => "throttle_engaged",
            Self::ThrottleDisengaged => "throttle_disengaged",
            Self::DriftDetected => "drift_detected",
            Self::Paused => "paused",
            Self::Resumed => "resumed",
        }
    }

    /// Severity tag for backend routing.
    pub(crate) fn severity(&self) -> Severity {
        match self {
            Self::TwoFaRequired => Severity::Critical,
            Self::SyncStarted => Severity::Info,
            Self::SyncComplete => Severity::Info,
            Self::SyncFailed(mode) => match mode {
                FailureMode::Partial(_) => Severity::Warn,
                FailureMode::SessionExpired | FailureMode::AllFailed => Severity::Critical,
            },
            Self::SessionExpired => Severity::Critical,
            Self::DiskLow => Severity::Critical,
            Self::DiskRecovered => Severity::Info,
            Self::AuthExpiring => Severity::Warn,
            Self::RateLimited => Severity::Warn,
            Self::ThrottleEngaged => Severity::Info,
            Self::ThrottleDisengaged => Severity::Info,
            Self::DriftDetected => Severity::Warn,
            Self::Paused => Severity::Info,
            Self::Resumed => Severity::Info,
        }
    }
}

// ── SyncNotificationData ────────────────────────────────────────────

/// Sync statistics passed to notification scripts as environment variables.
#[derive(Debug, Clone, Default)]
pub(crate) struct SyncNotificationData {
    pub assets_seen: u64,
    pub downloaded: usize,
    pub failed: usize,
    pub skipped: usize,
    pub bytes_downloaded: u64,
    pub disk_bytes_written: u64,
    pub elapsed_secs: f64,
    pub interrupted: bool,
    pub exif_failures: usize,
    pub state_write_failures: usize,
    pub enumeration_errors: usize,
    // Skip breakdown
    pub skipped_by_state: usize,
    pub skipped_on_disk: usize,
    pub skipped_by_media_type: usize,
    pub skipped_by_date_range: usize,
    pub skipped_by_live_photo: usize,
    pub skipped_by_filename: usize,
    pub skipped_by_excluded_album: usize,
    pub skipped_live_photo_variant: usize,
    pub skipped_duplicates: usize,
    pub skipped_retry_exhausted: usize,
    pub skipped_retry_only: usize,
}

impl From<&crate::download::SyncStats> for SyncNotificationData {
    fn from(s: &crate::download::SyncStats) -> Self {
        Self {
            assets_seen: s.assets_seen,
            downloaded: s.downloaded,
            failed: s.failed,
            skipped: s.skipped.total(),
            bytes_downloaded: s.bytes_downloaded,
            disk_bytes_written: s.disk_bytes_written,
            elapsed_secs: s.elapsed_secs,
            interrupted: s.interrupted,
            exif_failures: s.exif_failures,
            state_write_failures: s.state_write_failures,
            enumeration_errors: s.enumeration_errors,
            skipped_by_state: s.skipped.by_state,
            skipped_on_disk: s.skipped.on_disk,
            skipped_by_media_type: s.skipped.by_media_type,
            skipped_by_date_range: s.skipped.by_date_range,
            skipped_by_live_photo: s.skipped.by_live_photo,
            skipped_by_filename: s.skipped.by_filename,
            skipped_by_excluded_album: s.skipped.by_excluded_album,
            skipped_live_photo_variant: s.skipped.ampm_variant,
            skipped_duplicates: s.skipped.duplicates,
            skipped_retry_exhausted: s.skipped.retry_exhausted,
            skipped_retry_only: s.skipped.retry_only,
        }
    }
}

// ── Stub backends (PRs 2 & 3) ──────────────────────────────────────

/// Stub desktop notification backend. Full implementation in PR 2.
///
/// In PR 1 this stores whether the user opted in but performs no actual
/// notification delivery.
/// Desktop notification backend. Stub — delivery wired in PR 2.
///
/// In PR 1 this stores whether the user opted in and serves as a presence
/// marker in the multi-backend dispatch loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DesktopBackend {
    pub enabled: bool,
}

impl DesktopBackend {
    pub(crate) const fn new(enabled: bool) -> Self {
        Self { enabled }
    }
}

/// Error type for webhook delivery failures.
#[allow(dead_code, reason = "constructed in PR 3")]
#[derive(Debug, thiserror::Error)]
pub(crate) enum WebhookError {
    #[error("HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error("Timeout after {0}s")]
    Timeout(u64),
    #[error("Serialization: {0}")]
    Serialization(String),
}

/// Trait for webhook notification backends. Implementations in PR 3.
#[allow(dead_code, reason = "implemented in PR 3")]
#[async_trait::async_trait]
pub(crate) trait WebhookBackend: Send + Sync + std::fmt::Debug {
    /// Human-readable backend name for logging.
    fn name(&self) -> &'static str;
    /// Minimum severity that triggers this backend.
    fn min_severity(&self) -> Severity;
    /// Deliver the event. Called in a spawned task; errors are logged, not
    /// propagated.
    async fn send(&self, event: Event, message: &str, username: &str) -> Result<(), WebhookError>;
}

// ── Notifier ────────────────────────────────────────────────────────

/// Notification dispatcher. Holds an optional script path and multi-backend
/// slots for desktop and webhook notifications.
#[derive(Debug, Clone)]
pub(crate) struct Notifier {
    script: Option<PathBuf>,
    /// Global minimum severity threshold. Backends filter events below this
    /// level (script backend is exempt — always fires for backward compat).
    min_severity: Severity,
    /// Desktop notification backend. `Some` when the user opted in via
    /// `--desktop-notifications` or `[notifications].desktop`.
    desktop: Option<DesktopBackend>,
    /// Per-webhook endpoint configs, pre-resolved with per-backend
    /// `min_severity` overrides.
    webhooks: Vec<WebhookConfig>,
    /// Bounds how many notification scripts can run concurrently. A
    /// misbehaving or long-running script can't queue an unbounded
    /// number of spawned tasks behind itself under load.
    concurrency: Arc<tokio::sync::Semaphore>,
}

/// Timeout for notification scripts.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(30);

/// Cap on concurrent notification-script invocations. Events fire at
/// sync-cycle boundaries (start/complete/failure/token-required), so
/// 8 is plenty of headroom in watch mode while still bounding leaks.
const NOTIFIER_MAX_INFLIGHT: usize = 8;

impl Notifier {
    /// Create a new notifier from resolved notification config.
    ///
    /// `script` — path to a user-provided notification script (Unix only;
    /// silently dropped on Windows).
    ///
    /// `notifications` — resolved desktop, severity, and webhook config.
    pub fn new(
        script: Option<PathBuf>,
        notifications: &crate::config::NotificationsConfig,
    ) -> Self {
        // kei invokes scripts via `/bin/sh`, which isn't available on Windows.
        // Rather than let spawn fail silently on every event, drop the script
        // and warn once at construction time.
        if script.is_some() && cfg!(windows) {
            tracing::warn!(
                "--notification-script is unix-only (kei invokes scripts via /bin/sh). \
                 Ignoring the configured script on Windows."
            );
            return Self {
                script: None,
                min_severity: notifications.min_severity,
                desktop: None,
                webhooks: vec![],
                concurrency: Arc::new(tokio::sync::Semaphore::new(NOTIFIER_MAX_INFLIGHT)),
            };
        }
        Self {
            script,
            min_severity: notifications.min_severity,
            desktop: if notifications.desktop {
                Some(DesktopBackend::new(true))
            } else {
                None
            },
            webhooks: notifications.webhooks.clone(),
            concurrency: Arc::new(tokio::sync::Semaphore::new(NOTIFIER_MAX_INFLIGHT)),
        }
    }

    /// Whether `event` severity meets or exceeds a backend's threshold.
    ///
    /// `backend_min` — the backend's minimum severity (script = `Silent`,
    /// desktop = global `min_severity`, webhook = per-backend override).
    fn should_dispatch(backend_min: Severity, event: &Event) -> bool {
        event.severity() >= backend_min
    }

    /// Fire all configured notification backends with the given event.
    ///
    /// Fire-and-forget: script execution spawns in a background task so it
    /// never blocks sync. Desktop and webhook dispatch is stubbed — only
    /// script execution is active in this PR.
    pub fn notify(
        &self,
        event: Event,
        message: &str,
        username: &str,
        data: Option<&SyncNotificationData>,
    ) {
        // ── Script backend (always fires) ───────────────────────
        if Self::should_dispatch(Severity::Silent, &event) {
            self.dispatch_script(&event, message, username, data);
        }
        // ── Desktop backend (stub) ──────────────────────────────
        if self.desktop.is_some() && Self::should_dispatch(self.min_severity, &event) {
            tracing::trace!(
                event = event.as_str(),
                "would have sent desktop notification"
            );
        }
        // ── Webhook backends (stub) ─────────────────────────────
        for wh in &self.webhooks {
            if Self::should_dispatch(wh.min_severity, &event) {
                tracing::trace!(
                    event = event.as_str(),
                    backend = %wh.name,
                    "would have sent webhook notification"
                );
            }
        }
    }

    /// Spawn the script backend for `event`. Extracted from `notify()` so
    /// the dispatch loop stays readable.
    fn dispatch_script(
        &self,
        event: &Event,
        message: &str,
        username: &str,
        data: Option<&SyncNotificationData>,
    ) {
        let Some(script) = self.script.clone() else {
            return;
        };

        if !script.exists() {
            tracing::warn!(
                path = %script.display(),
                "Notification script does not exist"
            );
            return;
        }

        let event_str = event.as_str();
        let message = message.to_owned();
        let username = username.to_owned();
        let data = data.cloned();

        tracing::debug!(event = event_str, "Firing notification script");

        // Drop on saturation rather than queue: spawning a task that then
        // parks on `acquire_owned().await` is a softer version of the
        // unbounded-spawn behavior the semaphore exists to prevent. With
        // `try_acquire_owned` we also keep the saturation path observable
        // via the `notifier saturated` warning.
        let permit = match Arc::clone(&self.concurrency).try_acquire_owned() {
            Ok(permit) => permit,
            Err(tokio::sync::TryAcquireError::NoPermits) => {
                tracing::warn!(
                    event = event_str,
                    in_flight = NOTIFIER_MAX_INFLIGHT,
                    "Notifier saturated, dropping event"
                );
                return;
            }
            Err(tokio::sync::TryAcquireError::Closed) => {
                // Only reachable if the underlying semaphore is closed,
                // which kei never does. Treat as a process-exit no-op.
                return;
            }
        };
        tokio::spawn(async move {
            let _permit = permit;
            match run_script(&script, event_str, &message, &username, data.as_ref()).await {
                Ok(status) if status.success() => {
                    tracing::debug!(event = event_str, "Notification script completed");
                }
                Ok(status) => {
                    tracing::warn!(
                        event = event_str,
                        code = status.code(),
                        "Notification script exited with non-zero status"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        event = event_str,
                        error = %e,
                        "Notification script failed"
                    );
                }
            }
        });
    }
}

// ── Script runner ───────────────────────────────────────────────────

async fn run_script(
    script: &Path,
    event: &str,
    message: &str,
    username: &str,
    data: Option<&SyncNotificationData>,
) -> anyhow::Result<std::process::ExitStatus> {
    // Execute via /bin/sh to avoid ETXTBSY ("Text file busy") races when
    // the script file was recently written or replaced (e.g. config reload,
    // `kei setup`, parallel tests). Scripts with shebangs work fine via sh.
    let mut cmd = tokio::process::Command::new("/bin/sh");
    cmd.arg(script)
        .env("KEI_EVENT", event)
        .env("KEI_MESSAGE", message)
        .env("KEI_ICLOUD_USERNAME", username)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit());

    if let Some(d) = data {
        cmd.env("KEI_ASSETS_SEEN", d.assets_seen.to_string())
            .env("KEI_DOWNLOADED", d.downloaded.to_string())
            .env("KEI_FAILED", d.failed.to_string())
            .env("KEI_SKIPPED", d.skipped.to_string())
            .env("KEI_INTERRUPTED", d.interrupted.to_string())
            .env("KEI_BYTES_DOWNLOADED", d.bytes_downloaded.to_string())
            .env("KEI_DISK_BYTES", d.disk_bytes_written.to_string())
            .env("KEI_ELAPSED_SECS", format!("{:.1}", d.elapsed_secs))
            .env("KEI_EXIF_FAILURES", d.exif_failures.to_string())
            .env(
                "KEI_STATE_WRITE_FAILURES",
                d.state_write_failures.to_string(),
            )
            .env("KEI_ENUMERATION_ERRORS", d.enumeration_errors.to_string())
            .env("KEI_SKIPPED_BY_STATE", d.skipped_by_state.to_string())
            .env("KEI_SKIPPED_ON_DISK", d.skipped_on_disk.to_string())
            .env(
                "KEI_SKIPPED_BY_MEDIA_TYPE",
                d.skipped_by_media_type.to_string(),
            )
            .env(
                "KEI_SKIPPED_BY_DATE_RANGE",
                d.skipped_by_date_range.to_string(),
            )
            .env(
                "KEI_SKIPPED_BY_LIVE_PHOTO",
                d.skipped_by_live_photo.to_string(),
            )
            .env("KEI_SKIPPED_BY_FILENAME", d.skipped_by_filename.to_string())
            .env(
                "KEI_SKIPPED_BY_EXCLUDED_ALBUM",
                d.skipped_by_excluded_album.to_string(),
            )
            .env(
                "KEI_SKIPPED_LIVE_PHOTO_VARIANT",
                d.skipped_live_photo_variant.to_string(),
            )
            .env("KEI_SKIPPED_DUPLICATES", d.skipped_duplicates.to_string())
            .env(
                "KEI_SKIPPED_RETRY_EXHAUSTED",
                d.skipped_retry_exhausted.to_string(),
            )
            .env("KEI_SKIPPED_RETRY_ONLY", d.skipped_retry_only.to_string());
    }

    let mut child = cmd.spawn()?;

    if let Ok(result) = tokio::time::timeout(SCRIPT_TIMEOUT, child.wait()).await {
        Ok(result?)
    } else {
        tracing::warn!("Notification script timed out, killing");
        let _ = child.kill().await;
        anyhow::bail!(
            "notification script timed out after {}s",
            SCRIPT_TIMEOUT.as_secs()
        )
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Severity ─────────────────────────────────────────────────

    #[test]
    fn severity_ordering() {
        assert!(Severity::Silent < Severity::Info);
        assert!(Severity::Info < Severity::Warn);
        assert!(Severity::Warn < Severity::Critical);
    }

    // ── Event::as_str ────────────────────────────────────────────

    #[test]
    fn event_as_str() {
        assert_eq!(Event::TwoFaRequired.as_str(), "2fa_required");
        assert_eq!(Event::SyncStarted.as_str(), "sync_started");
        assert_eq!(Event::SyncComplete.as_str(), "sync_complete");
        assert_eq!(
            Event::SyncFailed(FailureMode::Partial(3)).as_str(),
            "sync_failed"
        );
        assert_eq!(
            Event::SyncFailed(FailureMode::SessionExpired).as_str(),
            "sync_failed"
        );
        assert_eq!(
            Event::SyncFailed(FailureMode::AllFailed).as_str(),
            "sync_failed"
        );
        assert_eq!(Event::SessionExpired.as_str(), "session_expired");
        assert_eq!(Event::DiskLow.as_str(), "disk_low");
        assert_eq!(Event::DiskRecovered.as_str(), "disk_recovered");
        assert_eq!(Event::AuthExpiring.as_str(), "auth_expiring");
        assert_eq!(Event::RateLimited.as_str(), "rate_limited");
        assert_eq!(Event::ThrottleEngaged.as_str(), "throttle_engaged");
        assert_eq!(Event::ThrottleDisengaged.as_str(), "throttle_disengaged");
        assert_eq!(Event::DriftDetected.as_str(), "drift_detected");
        assert_eq!(Event::Paused.as_str(), "paused");
        assert_eq!(Event::Resumed.as_str(), "resumed");
    }

    // ── Event::severity ──────────────────────────────────────────

    #[test]
    fn event_severity_existing_variants() {
        assert_eq!(Event::TwoFaRequired.severity(), Severity::Critical);
        assert_eq!(Event::SyncStarted.severity(), Severity::Info);
        assert_eq!(Event::SyncComplete.severity(), Severity::Info);
        assert_eq!(
            Event::SyncFailed(FailureMode::Partial(5)).severity(),
            Severity::Warn
        );
        assert_eq!(
            Event::SyncFailed(FailureMode::SessionExpired).severity(),
            Severity::Critical
        );
        assert_eq!(
            Event::SyncFailed(FailureMode::AllFailed).severity(),
            Severity::Critical
        );
        assert_eq!(Event::SessionExpired.severity(), Severity::Critical);
    }

    #[test]
    fn event_severity_new_variants() {
        assert_eq!(Event::DiskLow.severity(), Severity::Critical);
        assert_eq!(Event::DiskRecovered.severity(), Severity::Info);
        assert_eq!(Event::AuthExpiring.severity(), Severity::Warn);
        assert_eq!(Event::RateLimited.severity(), Severity::Warn);
        assert_eq!(Event::ThrottleEngaged.severity(), Severity::Info);
        assert_eq!(Event::ThrottleDisengaged.severity(), Severity::Info);
        assert_eq!(Event::DriftDetected.severity(), Severity::Warn);
        assert_eq!(Event::Paused.severity(), Severity::Info);
        assert_eq!(Event::Resumed.severity(), Severity::Info);
    }

    #[test]
    fn sync_failed_severity_differs_by_failure_mode() {
        // Partial failure → Warn
        assert_eq!(
            Event::SyncFailed(FailureMode::Partial(1)).severity(),
            Severity::Warn
        );
        assert_eq!(
            Event::SyncFailed(FailureMode::Partial(100)).severity(),
            Severity::Warn
        );
        // Session expiry → Critical
        assert_eq!(
            Event::SyncFailed(FailureMode::SessionExpired).severity(),
            Severity::Critical
        );
        // All failed → Critical
        assert_eq!(
            Event::SyncFailed(FailureMode::AllFailed).severity(),
            Severity::Critical
        );
    }

    // ── DesktopBackend ───────────────────────────────────────────

    #[test]
    fn desktop_backend_new_enabled() {
        let b = DesktopBackend::new(true);
        assert!(b.enabled);
    }

    #[test]
    fn desktop_backend_new_disabled() {
        let b = DesktopBackend::new(false);
        assert!(!b.enabled);
    }

    // ── Notifier construction ────────────────────────────────────

    /// Build a `NotificationsConfig` for tests. `desktop` controls whether
    /// the desktop backend is enabled.
    fn notif_config(desktop: bool) -> crate::config::NotificationsConfig {
        crate::config::NotificationsConfig {
            desktop,
            min_severity: Severity::Warn,
            webhooks: vec![],
        }
    }

    #[cfg(windows)]
    #[test]
    fn notifier_drops_script_on_windows() {
        let notifier = Notifier::new(
            Some(PathBuf::from("C:/does/not/matter.sh")),
            &notif_config(false),
        );
        assert!(notifier.script.is_none());
        assert!(notifier.desktop.is_none());
    }

    #[test]
    fn notifier_none_is_noop() {
        let notifier = Notifier::new(None, &notif_config(false));
        assert!(notifier.script.is_none());
        assert!(notifier.desktop.is_none());
    }

    #[test]
    fn notifier_with_desktop_enabled() {
        let notifier = Notifier::new(None, &notif_config(true));
        assert!(notifier.desktop.is_some());
        assert!(notifier.desktop.unwrap().enabled);
    }

    #[test]
    fn notify_with_nonexistent_script() {
        let notifier = Notifier::new(
            Some(PathBuf::from("/tmp/claude/nonexistent_notify.sh")),
            &notif_config(false),
        );
        // Should not panic, just log a warning (script existence checked synchronously)
        notifier.notify(
            Event::SyncComplete,
            "test message",
            "user@example.com",
            None,
        );
    }

    // ── Severity filtering / multi-backend dispatch ────────────

    #[test]
    fn notifier_script_always_fires_regardless_of_severity() {
        // Script backend should always dispatch even for Silent events.
        assert!(Notifier::should_dispatch(
            Severity::Silent,
            &Event::SyncStarted
        ));
        assert!(Notifier::should_dispatch(
            Severity::Silent,
            &Event::SyncComplete
        ));
    }

    #[test]
    fn should_dispatch_respects_threshold() {
        // Warn threshold: Info events don't pass, Warn+ do.
        assert!(!Notifier::should_dispatch(
            Severity::Warn,
            &Event::SyncStarted
        ));
        assert!(Notifier::should_dispatch(
            Severity::Warn,
            &Event::RateLimited
        ));
        assert!(Notifier::should_dispatch(Severity::Warn, &Event::DiskLow));
        // Critical threshold: only Critical passes.
        assert!(!Notifier::should_dispatch(
            Severity::Critical,
            &Event::SyncStarted
        ));
        assert!(!Notifier::should_dispatch(
            Severity::Critical,
            &Event::RateLimited
        ));
        assert!(Notifier::should_dispatch(
            Severity::Critical,
            &Event::TwoFaRequired
        ));
    }

    #[test]
    fn notifier_desktop_respects_global_min_severity() {
        let cfg = crate::config::NotificationsConfig {
            desktop: true,
            min_severity: Severity::Critical,
            webhooks: vec![],
        };
        let notifier = Notifier::new(None, &cfg);
        assert!(notifier.desktop.is_some());
        // Info event below Critical threshold — desktop should not receive it.
        assert!(!Notifier::should_dispatch(
            notifier.min_severity,
            &Event::SyncStarted
        ));
        // Critical event meets threshold.
        assert!(Notifier::should_dispatch(
            notifier.min_severity,
            &Event::TwoFaRequired
        ));
    }

    #[test]
    fn notifier_webhook_per_backend_severity_override() {
        let cfg = crate::config::NotificationsConfig {
            desktop: false,
            min_severity: Severity::Warn,
            webhooks: vec![
                WebhookConfig {
                    name: "ntfy".into(),
                    url: "https://ntfy.example.com/kei".into(),
                    min_severity: Severity::Critical,
                },
                WebhookConfig {
                    name: "discord".into(),
                    url: "https://discord.example.com/webhook".into(),
                    min_severity: Severity::Info,
                },
            ],
        };
        let notifier = Notifier::new(None, &cfg);
        assert_eq!(notifier.webhooks.len(), 2);
        // ntfy: only Critical
        assert!(!Notifier::should_dispatch(
            Severity::Critical,
            &Event::SyncStarted
        ));
        assert!(Notifier::should_dispatch(
            Severity::Critical,
            &Event::TwoFaRequired
        ));
        // discord: Info+ (everything)
        assert!(Notifier::should_dispatch(
            Severity::Info,
            &Event::SyncStarted
        ));
    }

    #[test]
    #[tracing_test::traced_test]
    fn desktop_stub_trace_emits_for_above_threshold_event() {
        let cfg = crate::config::NotificationsConfig {
            desktop: true,
            min_severity: Severity::Warn,
            webhooks: vec![],
        };
        let notifier = Notifier::new(None, &cfg);
        notifier.notify(Event::DiskLow, "disk low", "user@example.com", None);
        assert!(
            logs_contain("would have sent desktop notification"),
            "expected desktop trace for Critical event above Warn threshold"
        );
    }

    #[test]
    #[tracing_test::traced_test]
    fn webhook_stub_trace_emits_for_above_threshold_event() {
        let cfg = crate::config::NotificationsConfig {
            desktop: false,
            min_severity: Severity::Warn,
            webhooks: vec![
                WebhookConfig {
                    name: "ntfy".into(),
                    url: "https://ntfy.example.com/kei".into(),
                    min_severity: Severity::Critical,
                },
                WebhookConfig {
                    name: "discord".into(),
                    url: "https://discord.example.com/webhook".into(),
                    min_severity: Severity::Info,
                },
            ],
        };
        let notifier = Notifier::new(None, &cfg);
        notifier.notify(Event::SyncStarted, "sync started", "user@example.com", None);
        // ntfy threshold is Critical — SyncStarted (Info) should not fire.
        assert!(
            !logs_contain("backend=ntfy"),
            "ntfy should not trace for Info < Critical"
        );
        // discord threshold is Info — SyncStarted should fire.
        assert!(
            logs_contain("backend=discord"),
            "discord should trace for Info >= Info"
        );
    }

    #[test]
    fn notifier_construction_stores_config() {
        let cfg = crate::config::NotificationsConfig {
            desktop: true,
            min_severity: Severity::Info,
            webhooks: vec![WebhookConfig {
                name: "test".into(),
                url: "https://example.com".into(),
                min_severity: Severity::Warn,
            }],
        };
        let notifier = Notifier::new(None, &cfg);
        assert_eq!(notifier.min_severity, Severity::Info);
        assert!(notifier.desktop.is_some());
        assert!(notifier.desktop.unwrap().enabled);
        assert_eq!(notifier.webhooks.len(), 1);
        assert_eq!(notifier.webhooks[0].name, "test");
    }

    // ── Script runner helpers ────────────────────────────────────

    /// Write a shell script to a temp dir. No executable permission needed
    /// since `run_script` invokes scripts via `/bin/sh`.
    #[cfg(unix)]
    fn write_test_script(dir: &std::path::Path, name: &str, body: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    // ── run_script tests ─────────────────────────────────────────

    #[cfg(unix)]
    #[tokio::test]
    async fn run_script_success() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_test_script(dir.path(), "success.sh", b"#!/bin/sh\nexit 0\n");

        let status = run_script(&script, "test_event", "msg", "user", None)
            .await
            .unwrap();
        assert!(status.success());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_script_nonzero_exit() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_test_script(dir.path(), "fail.sh", b"#!/bin/sh\nexit 1\n");

        let status = run_script(&script, "test_event", "msg", "user", None)
            .await
            .unwrap();
        assert!(!status.success());
    }

    // ── notify() tests ───────────────────────────────────────────

    #[cfg(unix)]
    #[tokio::test]
    async fn notify_runs_script_with_env_vars() {
        let dir = tempfile::tempdir().unwrap();
        let output_path = dir.path().join("test_notify_output.txt");
        let body = format!(
            "#!/bin/sh\necho \"$KEI_EVENT|$KEI_MESSAGE|$KEI_ICLOUD_USERNAME\" > {}\n",
            output_path.display()
        );
        let script_path = write_test_script(dir.path(), "test_notify.sh", body.as_bytes());

        let notifier = Notifier::new(Some(script_path.clone()), &notif_config(false));
        notifier.notify(
            Event::TwoFaRequired,
            "Need 2FA code",
            "test@example.com",
            None,
        );

        // Wait for the spawned background task to complete (poll instead of fixed sleep)
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if output_path.exists() {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "notification script did not produce output within timeout"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let output = std::fs::read_to_string(&output_path).unwrap();
        assert_eq!(output.trim(), "2fa_required|Need 2FA code|test@example.com");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn notify_with_sync_data_sets_extended_env_vars() {
        let dir = tempfile::tempdir().unwrap();
        let output_path = dir.path().join("test_data_output.txt");
        let body = format!(
            "#!/bin/sh\necho \"$KEI_DOWNLOADED|$KEI_FAILED|$KEI_SKIPPED|$KEI_BYTES_DOWNLOADED|$KEI_SKIPPED_BY_STATE\" > {}\n",
            output_path.display()
        );
        let script_path = write_test_script(dir.path(), "test_data.sh", body.as_bytes());

        let data = SyncNotificationData {
            downloaded: 42,
            failed: 3,
            skipped: 100,
            bytes_downloaded: 1_500_000,
            skipped_by_state: 80,
            ..SyncNotificationData::default()
        };

        let notifier = Notifier::new(Some(script_path), &notif_config(false));
        notifier.notify(Event::SyncComplete, "test", "user@example.com", Some(&data));

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if output_path.exists() {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "notification script did not produce output"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let output = std::fs::read_to_string(&output_path).unwrap();
        assert_eq!(output.trim(), "42|3|100|1500000|80");
    }

    // ── Semaphore / saturation tests ─────────────────────────────

    /// Test scaffold: a barrier-blocked sh script that tracks both
    /// concurrent in-flight invocations (per-pid marker files) and
    /// total invocations (single-byte appends). Each invocation:
    /// 1. Appends one byte to `invocations` (atomic on Linux).
    /// 2. Drops a marker file at `inflight/$pid`.
    /// 3. Polls until `release` exists.
    /// 4. Removes its marker on exit.
    #[cfg(unix)]
    struct BarrierFixture {
        _dir: tempfile::TempDir,
        counter_dir: PathBuf,
        release: PathBuf,
        invocations: PathBuf,
        script_path: PathBuf,
    }

    #[cfg(unix)]
    impl BarrierFixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let counter_dir = dir.path().join("inflight");
            std::fs::create_dir_all(&counter_dir).unwrap();
            let release = dir.path().join("release");
            let invocations = dir.path().join("invocations");
            let body = format!(
                "#!/bin/sh\nprintf x >> \"{}\"\nmarker=\"{}/$$\"\n: > \"$marker\"\n\
                 while [ ! -f \"{}\" ]; do sleep 0.02; done\nrm -f \"$marker\"\n",
                invocations.display(),
                counter_dir.display(),
                release.display(),
            );
            let script_path = write_test_script(dir.path(), "barrier.sh", body.as_bytes());
            Self {
                _dir: dir,
                counter_dir,
                release,
                invocations,
                script_path,
            }
        }

        fn count_markers(&self) -> usize {
            std::fs::read_dir(&self.counter_dir)
                .map(|it| it.flatten().count())
                .unwrap_or(0)
        }

        fn count_invocations(&self) -> usize {
            std::fs::read(&self.invocations)
                .map(|b| b.len())
                .unwrap_or(0)
        }

        fn release_barrier(&self) {
            std::fs::write(&self.release, b"").unwrap();
        }

        async fn wait_until<F: FnMut() -> bool>(&self, timeout: Duration, mut pred: F) {
            let deadline = tokio::time::Instant::now() + timeout;
            while tokio::time::Instant::now() < deadline {
                if pred() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }

    /// Fire more events than `NOTIFIER_MAX_INFLIGHT` at a barrier script
    /// and confirm the semaphore caps concurrent in-flight invocations.
    /// Without the cap, every event would spawn `/bin/sh` concurrently
    /// and the marker count would exceed `NOTIFIER_MAX_INFLIGHT`.
    #[cfg(unix)]
    #[tokio::test]
    async fn notifier_semaphore_caps_concurrent_inflight() {
        let fixture = BarrierFixture::new();
        let notifier = Notifier::new(Some(fixture.script_path.clone()), &notif_config(false));
        for _ in 0..NOTIFIER_MAX_INFLIGHT * 2 {
            notifier.notify(Event::SyncStarted, "msg", "user@example.com", None);
        }

        let mut max_concurrent = 0usize;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            max_concurrent = max_concurrent.max(fixture.count_markers());
            if max_concurrent >= NOTIFIER_MAX_INFLIGHT {
                for _ in 0..10 {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    max_concurrent = max_concurrent.max(fixture.count_markers());
                }
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        fixture.release_barrier();
        fixture
            .wait_until(Duration::from_secs(5), || fixture.count_markers() == 0)
            .await;

        assert!(max_concurrent >= 1, "no scripts ever ran -- test setup bug");
        assert!(
            max_concurrent <= NOTIFIER_MAX_INFLIGHT,
            "semaphore did not cap concurrent scripts: max observed {max_concurrent}, cap is {NOTIFIER_MAX_INFLIGHT}",
        );
    }

    /// When more than `NOTIFIER_MAX_INFLIGHT` events are fired while every
    /// permit is held, the surplus events must be **dropped**, not queued.
    /// With the old `acquire_owned().await` we'd spawn a task per event and
    /// the surplus would run as permits became free; with `try_acquire_owned`
    /// the surplus saturates and we drop on the floor. After permits are
    /// released, fresh events must still be able to acquire (no permit leak).
    #[cfg(unix)]
    #[tracing_test::traced_test]
    #[tokio::test]
    async fn notifier_drops_events_when_saturated() {
        let fixture = BarrierFixture::new();
        let notifier = Notifier::new(Some(fixture.script_path.clone()), &notif_config(false));
        for _ in 0..NOTIFIER_MAX_INFLIGHT * 4 {
            notifier.notify(Event::SyncStarted, "msg", "user@example.com", None);
        }

        fixture
            .wait_until(Duration::from_secs(5), || {
                fixture.count_markers() >= NOTIFIER_MAX_INFLIGHT
            })
            .await;
        assert_eq!(
            fixture.count_markers(),
            NOTIFIER_MAX_INFLIGHT,
            "expected exactly {NOTIFIER_MAX_INFLIGHT} scripts holding permits"
        );

        fixture.release_barrier();
        fixture
            .wait_until(Duration::from_secs(5), || fixture.count_markers() == 0)
            .await;
        assert_eq!(
            fixture.count_markers(),
            0,
            "scripts did not drain after release"
        );

        // Dropped events must not retroactively run once permits are free.
        assert_eq!(
            fixture.count_invocations(),
            NOTIFIER_MAX_INFLIGHT,
            "saturation drop regressed: surplus events ran retroactively"
        );
        assert!(
            logs_contain("Notifier saturated"),
            "expected a 'Notifier saturated' warning during the flood"
        );

        // Permit-leak guard: after drain, fresh events should run.
        // With `release` in place, each new script exits immediately.
        const FRESH_BATCH: usize = 4;
        for _ in 0..FRESH_BATCH {
            notifier.notify(Event::SyncStarted, "msg", "user@example.com", None);
        }
        let expected_total = NOTIFIER_MAX_INFLIGHT + FRESH_BATCH;
        fixture
            .wait_until(Duration::from_secs(5), || {
                fixture.count_invocations() >= expected_total
            })
            .await;
        assert_eq!(
            fixture.count_invocations(),
            expected_total,
            "permit leak: post-release events failed to acquire"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn notify_without_data_omits_extended_vars() {
        let dir = tempfile::tempdir().unwrap();
        let output_path = dir.path().join("test_no_data.txt");
        let body = format!(
            "#!/bin/sh\necho \"${{KEI_DOWNLOADED:-unset}}|${{KEI_FAILED:-unset}}\" > {}\n",
            output_path.display()
        );
        let script_path = write_test_script(dir.path(), "test_no_data.sh", body.as_bytes());

        let notifier = Notifier::new(Some(script_path), &notif_config(false));
        notifier.notify(Event::SyncComplete, "test", "user@example.com", None);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if output_path.exists() {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "notification script did not produce output"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let output = std::fs::read_to_string(&output_path).unwrap();
        assert_eq!(output.trim(), "unset|unset");
    }
}
