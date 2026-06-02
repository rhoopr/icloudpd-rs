//! Notification script support for unattended operation.
//!
//! Fires a user-provided script with event information as environment variables.
//! Used to notify users of 2FA expiry, sync completion, failures, etc.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Events that trigger notification scripts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Event {
    /// 2FA code is needed (session expired in headless mode)
    TwoFaRequired,
    /// A sync cycle is about to run (fires before run_cycle, after skip-check)
    SyncStarted,
    /// Sync cycle completed successfully
    SyncComplete,
    /// Sync cycle had failures
    SyncFailed,
    /// Session expired and re-authentication failed
    SessionExpired,
}

impl Event {
    const fn as_str(self) -> &'static str {
        match self {
            Self::TwoFaRequired => "2fa_required",
            Self::SyncStarted => "sync_started",
            Self::SyncComplete => "sync_complete",
            Self::SyncFailed => "sync_failed",
            Self::SessionExpired => "session_expired",
        }
    }
}

/// Notification dispatcher. Holds an optional script path.
/// When no script is configured, all methods are no-ops.
#[derive(Debug, Clone)]
pub(crate) struct Notifier {
    script: Option<PathBuf>,
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
    pub fn new(script: Option<PathBuf>) -> Self {
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
                concurrency: Arc::new(tokio::sync::Semaphore::new(NOTIFIER_MAX_INFLIGHT)),
            };
        }
        Self {
            script,
            concurrency: Arc::new(tokio::sync::Semaphore::new(NOTIFIER_MAX_INFLIGHT)),
        }
    }

    /// Fire the notification script with the given event.
    /// Fire-and-forget: spawns the script in a background task so it never blocks sync.
    pub fn notify(&self, event: Event, message: &str, report_path: Option<&Path>) {
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
        let report_path = report_path.map(Path::to_path_buf);

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
            match run_script(&script, event_str, &message, report_path.as_deref()).await {
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

async fn run_script(
    script: &Path,
    event: &str,
    message: &str,
    report_path: Option<&Path>,
) -> anyhow::Result<std::process::ExitStatus> {
    // Execute via /bin/sh to avoid ETXTBSY ("Text file busy") races when
    // the script file was recently written or replaced (e.g. config reload,
    // `kei setup`, parallel tests). Scripts with shebangs work fine via sh.
    let mut cmd = tokio::process::Command::new("/bin/sh");
    cmd.arg(script)
        .env("KEI_EVENT", event)
        .env("KEI_MESSAGE", message)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit());

    if let Some(path) = report_path {
        cmd.env("KEI_REPORT_JSON", path);
    }

    let mut child = cmd.spawn()?;

    if let Ok(result) = tokio::time::timeout(SCRIPT_TIMEOUT, child.wait()).await {
        Ok(result?)
    } else {
        tracing::warn!("Notification script timed out, killing");
        let _ = child.kill().await;
        anyhow::bail!(
            "Notification script timed out after {} seconds.",
            SCRIPT_TIMEOUT.as_secs()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_as_str() {
        assert_eq!(Event::TwoFaRequired.as_str(), "2fa_required");
        assert_eq!(Event::SyncStarted.as_str(), "sync_started");
        assert_eq!(Event::SyncComplete.as_str(), "sync_complete");
        assert_eq!(Event::SyncFailed.as_str(), "sync_failed");
        assert_eq!(Event::SessionExpired.as_str(), "session_expired");
    }

    #[cfg(windows)]
    #[test]
    fn notifier_drops_script_on_windows() {
        let notifier = Notifier::new(Some(PathBuf::from("C:/does/not/matter.sh")));
        assert!(notifier.script.is_none());
    }

    #[test]
    fn notifier_none_is_noop() {
        let notifier = Notifier::new(None);
        assert!(notifier.script.is_none());
    }

    #[test]
    fn notify_with_nonexistent_script() {
        let notifier = Notifier::new(Some(PathBuf::from("/tmp/codex/kei/nonexistent_notify.sh")));
        // Should not panic, just log a warning (script existence checked synchronously)
        notifier.notify(Event::SyncComplete, "test message", None);
    }

    /// Write a shell script to a temp dir. No executable permission needed
    /// since `run_script` invokes scripts via `/bin/sh`.
    #[cfg(unix)]
    fn write_test_script(dir: &Path, name: &str, body: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap_or_else(|err| {
            panic!("write notification test script {}: {err}", path.display())
        });
        path
    }

    #[cfg(unix)]
    fn notification_test_dir(context: &str) -> tempfile::TempDir {
        tempfile::tempdir().unwrap_or_else(|err| panic!("create temp dir for {context}: {err}"))
    }

    #[cfg(unix)]
    fn read_script_output(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap_or_else(|err| {
            panic!("read notification script output {}: {err}", path.display())
        })
    }

    #[cfg(unix)]
    async fn wait_until_file_contains(path: &Path, expected: &str) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let last_observed = match std::fs::read_to_string(path) {
                Ok(contents) if contents.contains(expected) => return,
                Ok(contents) => contents,
                Err(err) => format!("<read failed: {err}>"),
            };

            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {} to contain {expected:?}; last observed: {last_observed:?}",
                path.display()
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    #[cfg(unix)]
    async fn wait_until_available_permits(
        semaphore: &tokio::sync::Semaphore,
        expected_permits: usize,
    ) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let available = semaphore.available_permits();
            if available == expected_permits {
                return;
            }

            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for notifier permits; expected {expected_permits}, observed {available}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_script_success() {
        let dir = notification_test_dir("run_script_success");
        let script = write_test_script(dir.path(), "success.sh", b"#!/bin/sh\nexit 0\n");

        let status = run_script(&script, "test_event", "msg", None)
            .await
            .unwrap_or_else(|err| panic!("run notification script {}: {err}", script.display()));
        assert!(status.success());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_script_nonzero_exit() {
        let dir = notification_test_dir("run_script_nonzero_exit");
        let script = write_test_script(dir.path(), "fail.sh", b"#!/bin/sh\nexit 1\n");

        let status = run_script(&script, "test_event", "msg", None)
            .await
            .unwrap_or_else(|err| panic!("run notification script {}: {err}", script.display()));
        assert!(!status.success());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn notify_runs_script_with_env_vars() {
        let dir = notification_test_dir("notify_runs_script_with_env_vars");
        let output_path = dir.path().join("test_notify_output.txt");
        let body = format!(
            "#!/bin/sh\necho \"$KEI_EVENT|$KEI_MESSAGE\" > {}\n",
            output_path.display()
        );
        let script_path = write_test_script(dir.path(), "test_notify.sh", body.as_bytes());

        let status = run_script(
            &script_path,
            Event::TwoFaRequired.as_str(),
            "Need 2FA code",
            None,
        )
        .await
        .expect("run notification script");
        assert!(status.success());

        let output = read_script_output(&output_path);
        assert_eq!(output.trim(), "2fa_required|Need 2FA code");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn notify_with_report_path_sets_report_json_env_var() {
        let dir = notification_test_dir("notify_with_report_path_sets_report_json_env_var");
        let output_path = dir.path().join("test_data_output.txt");
        let report_path = dir.path().join("sync_report.json");
        let body = format!(
            "#!/bin/sh\necho \"$KEI_REPORT_JSON\" > {}\n",
            output_path.display()
        );
        let script_path = write_test_script(dir.path(), "test_data.sh", body.as_bytes());

        let status = run_script(
            &script_path,
            Event::SyncComplete.as_str(),
            "test",
            Some(&report_path),
        )
        .await
        .expect("run notification script with report path");
        assert!(status.success());

        let output = read_script_output(&output_path);
        assert_eq!(output.trim(), report_path.display().to_string());
    }

    /// The semaphore itself is the concurrency contract. Acquire every
    /// permit explicitly instead of coordinating shell scripts with marker
    /// files and sleep loops.
    #[cfg(unix)]
    #[tokio::test]
    async fn notifier_semaphore_caps_concurrent_inflight() {
        let notifier = Notifier::new(Some(PathBuf::from("/tmp/codex/kei/unused-notify.sh")));
        let held: Vec<_> = (0..NOTIFIER_MAX_INFLIGHT)
            .map(|_| {
                Arc::clone(&notifier.concurrency)
                    .try_acquire_owned()
                    .expect("permit should be available")
            })
            .collect();
        assert_eq!(
            held.len(),
            NOTIFIER_MAX_INFLIGHT,
            "test must hold every notifier permit"
        );
        assert_eq!(
            notifier.concurrency.available_permits(),
            0,
            "all notification permits should be held"
        );
        assert!(
            Arc::clone(&notifier.concurrency)
                .try_acquire_owned()
                .is_err(),
            "semaphore must reject the next concurrent invocation"
        );
        drop(held);
        assert_eq!(
            notifier.concurrency.available_permits(),
            NOTIFIER_MAX_INFLIGHT,
            "dropping held permits must restore full capacity"
        );
    }

    /// When more than `NOTIFIER_MAX_INFLIGHT` events are fired while every
    /// permit is held, the surplus events must be **dropped**, not queued.
    /// With the old `acquire_owned().await` we'd spawn a task per event and
    /// the surplus would run as permits became free; with `try_acquire_owned`
    /// the surplus saturates and we drop on the floor. After permits are
    /// released, fresh events must still be able to acquire (no permit leak).
    #[cfg(unix)]
    #[tokio::test]
    async fn notifier_drops_events_when_saturated() {
        let (capture, _guard) = crate::test_helpers::TracingCapture::install();
        let dir = notification_test_dir("saturated notifier");
        let output_path = dir.path().join("fresh-notify.txt");
        let body = format!("#!/bin/sh\necho fresh > {}\n", output_path.display());
        let script_path = write_test_script(dir.path(), "notify.sh", body.as_bytes());
        let notifier = Notifier::new(Some(script_path));
        let held: Vec<_> = (0..NOTIFIER_MAX_INFLIGHT)
            .map(|_| {
                Arc::clone(&notifier.concurrency)
                    .try_acquire_owned()
                    .expect("permit should be available")
            })
            .collect();
        assert_eq!(
            notifier.concurrency.available_permits(),
            0,
            "test must hold every permit before calling notify"
        );

        notifier.notify(Event::SyncStarted, "msg", None);

        let events = capture.events();
        let saturated = events
            .iter()
            .find(|event| {
                event.level == tracing::Level::WARN
                    && event.message() == Some("Notifier saturated, dropping event")
            })
            .unwrap_or_else(|| panic!("missing notifier saturation warning: {events:?}"));
        assert_eq!(saturated.field("event"), Some(Event::SyncStarted.as_str()));
        let expected_in_flight = NOTIFIER_MAX_INFLIGHT.to_string();
        assert_eq!(
            saturated.field("in_flight"),
            Some(expected_in_flight.as_str())
        );
        assert_eq!(
            notifier.concurrency.available_permits(),
            0,
            "dropped notification must not consume or leak a permit"
        );
        assert!(
            !output_path.exists(),
            "saturated notification should be dropped instead of queued"
        );

        drop(held);
        notifier.notify(Event::SyncStarted, "msg", None);
        wait_until_file_contains(&output_path, "fresh").await;
        wait_until_available_permits(&notifier.concurrency, NOTIFIER_MAX_INFLIGHT).await;
        assert_eq!(
            read_script_output(&output_path).trim(),
            "fresh",
            "fresh notification should run through Notifier::notify after saturation"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn notify_without_report_path_omits_report_env() {
        let dir = notification_test_dir("notify_without_data_omits_extended_vars");
        let output_path = dir.path().join("test_no_data.txt");
        let body = format!(
            "#!/bin/sh\necho \"${{KEI_REPORT_JSON:-unset}}\" > {}\n",
            output_path.display()
        );
        let script_path = write_test_script(dir.path(), "test_no_data.sh", body.as_bytes());

        let status = run_script(&script_path, Event::SyncComplete.as_str(), "test", None)
            .await
            .expect("run notification script without sync data");
        assert!(status.success());

        let output = read_script_output(&output_path);
        assert_eq!(output.trim(), "unset");
    }
}
