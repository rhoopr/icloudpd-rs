//! Post-cycle reporting boundary for sync/watch cycles.
//!
//! The sync loop owns CloudKit/auth/download orchestration. This module owns
//! the side effects that happen after a cycle has produced facts: friendly
//! narration, health state, Prometheus metrics, JSON reports, and completion
//! notifications.

use std::path::Path;
use std::time::Duration;

use crate::download::SyncStats;
use crate::health::HealthStatus;
use crate::metrics::MetricsHandle;
use crate::notifications::{self, Notifier};
use crate::personality::Mode;
use crate::report::{self, RunOptions};
use crate::state::{self, ReportStateStore};

/// Stable dependencies and resolved options used by the post-cycle reporter.
pub(crate) struct CycleReporter<'a, D: ReportStateStore + ?Sized> {
    username: &'a str,
    watch_mode: bool,
    report_path: Option<&'a Path>,
    run_options: RunOptions,
    health_dir: &'a Path,
    personality_mode: Mode,
    state_db: Option<&'a D>,
    metrics_handle: Option<&'a MetricsHandle>,
    notifier: &'a Notifier,
}

impl<D> std::fmt::Debug for CycleReporter<'_, D>
where
    D: ReportStateStore + ?Sized,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CycleReporter")
            .field("username", &self.username)
            .field("watch_mode", &self.watch_mode)
            .field("report_path", &self.report_path)
            .field("run_options", &self.run_options)
            .field("health_dir", &self.health_dir)
            .field("personality_mode", &self.personality_mode)
            .field("has_state_db", &self.state_db.is_some())
            .field("has_metrics_handle", &self.metrics_handle.is_some())
            .field("notifier", &self.notifier)
            .finish()
    }
}

/// Constructor input for [`CycleReporter`].
pub(crate) struct CycleReporterConfig<'a, D: ReportStateStore + ?Sized> {
    pub(crate) username: &'a str,
    pub(crate) watch_mode: bool,
    pub(crate) report_path: Option<&'a Path>,
    pub(crate) run_options: RunOptions,
    pub(crate) health_dir: &'a Path,
    pub(crate) personality_mode: Mode,
    pub(crate) state_db: Option<&'a D>,
    pub(crate) metrics_handle: Option<&'a MetricsHandle>,
    pub(crate) notifier: &'a Notifier,
}

impl<D> std::fmt::Debug for CycleReporterConfig<'_, D>
where
    D: ReportStateStore + ?Sized,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CycleReporterConfig")
            .field("username", &self.username)
            .field("watch_mode", &self.watch_mode)
            .field("report_path", &self.report_path)
            .field("run_options", &self.run_options)
            .field("health_dir", &self.health_dir)
            .field("personality_mode", &self.personality_mode)
            .field("has_state_db", &self.state_db.is_some())
            .field("has_metrics_handle", &self.metrics_handle.is_some())
            .field("notifier", &self.notifier)
            .finish()
    }
}

/// Facts produced by one sync cycle that reporters may render or persist.
#[derive(Debug)]
pub(crate) struct CycleReportInput<'a> {
    pub(crate) stats: &'a SyncStats,
    pub(crate) failed_count: usize,
    pub(crate) session_expired: bool,
    pub(crate) elapsed: Duration,
}

impl<'a, D> CycleReporter<'a, D>
where
    D: ReportStateStore + ?Sized,
{
    pub(crate) fn new(config: CycleReporterConfig<'a, D>) -> Self {
        Self {
            username: config.username,
            watch_mode: config.watch_mode,
            report_path: config.report_path,
            run_options: config.run_options,
            health_dir: config.health_dir,
            personality_mode: config.personality_mode,
            state_db: config.state_db,
            metrics_handle: config.metrics_handle,
            notifier: config.notifier,
        }
    }

    /// Report a skipped watch cycle where `changes/database` found no selected
    /// library changes. This is intentionally health-only: Prometheus health
    /// gauges and `health.json` need fresh timestamps, but cycle counters,
    /// duration, reports, and completion notifications must not change.
    pub(crate) async fn report_skipped_watch_cycle(&self, health: &mut HealthStatus) {
        health.record_success();
        health.write(self.health_dir);
        if let Some(handle) = self.metrics_handle {
            handle.update_health_only(health).await;
        }
    }

    /// Run all post-cycle reporting side effects for a completed sync cycle.
    pub(crate) async fn report_completed_cycle(
        &self,
        health: &mut HealthStatus,
        input: CycleReportInput<'_>,
    ) {
        let friendly_summary = self.friendly_summary().await;
        self.report_friendly_output(&input, friendly_summary.as_ref());
        self.update_health(health, &input);
        self.update_metrics(health, &input, friendly_summary.as_ref())
            .await;
        self.write_json_report(&input).await;
        self.notify_cycle_outcome(&input);
    }

    async fn friendly_summary(&self) -> Option<state::types::SyncSummary> {
        if !self.personality_mode.is_friendly() {
            return None;
        }
        let db = self.state_db?;
        match db.get_summary().await {
            Ok(summary) => Some(summary),
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    "post-cycle summary unavailable; rendering card without library totals"
                );
                None
            }
        }
    }

    fn report_friendly_output(
        &self,
        input: &CycleReportInput<'_>,
        library_after_summary: Option<&state::types::SyncSummary>,
    ) {
        let downloaded_u64 = u64::try_from(input.stats.downloaded).unwrap_or(u64::MAX);
        if let Some(library_after) = library_after_summary {
            let after = library_after.downloaded_bytes;
            let before = after.saturating_sub(input.stats.bytes_downloaded);
            crate::personality::narration::downloaded_phase_to_stderr(
                self.personality_mode,
                downloaded_u64,
                before,
                after,
            );
        }
        crate::personality::narration::verified_phase_to_stderr(
            self.personality_mode,
            downloaded_u64,
        );
        if let Some(library_after) = library_after_summary {
            let stats = input.stats;
            let card = crate::personality::summary::SummaryCard {
                photos_new: u64::try_from(stats.photos_downloaded).unwrap_or(u64::MAX),
                videos_new: u64::try_from(stats.videos_downloaded).unwrap_or(u64::MAX),
                skipped_total: u64::try_from(stats.skipped.total() - stats.skipped.duplicates)
                    .unwrap_or(u64::MAX),
                skipped_already_present: u64::try_from(
                    stats.skipped.by_state + stats.skipped.on_disk,
                )
                .unwrap_or(u64::MAX),
                failed: u64::try_from(stats.failed).unwrap_or(u64::MAX),
                elapsed: input.elapsed,
                bytes_downloaded: stats.bytes_downloaded,
                library_total_assets: library_after.total_assets,
                library_total_bytes: library_after.downloaded_bytes,
            };
            card.print_to_stderr(self.personality_mode);
            crate::personality::summary::print_recap_to_stderr(
                self.personality_mode,
                &input.stats.recap,
            );
        }

        crate::personality::narration::signoff_to_stderr(
            self.personality_mode,
            &crate::personality::narration::CycleSummary {
                downloaded: downloaded_u64,
                failed: u64::try_from(input.failed_count).unwrap_or(u64::MAX),
                elapsed: input.elapsed,
                watch_mode: self.watch_mode,
            },
        );
    }

    fn update_health(&self, health: &mut HealthStatus, input: &CycleReportInput<'_>) {
        if input.session_expired {
            health.record_failure("session expired");
        } else if input.failed_count > 0 {
            health.record_failure(&format!("{} sync failures", input.failed_count));
        } else {
            health.record_success();
        }
        health.write(self.health_dir);
    }

    async fn update_metrics(
        &self,
        health: &HealthStatus,
        input: &CycleReportInput<'_>,
        friendly_summary: Option<&state::types::SyncSummary>,
    ) {
        let Some(handle) = self.metrics_handle else {
            return;
        };
        if input.session_expired {
            handle.record_session_expiration();
        }
        handle.update(input.stats, health).await;

        if let Some(summary) = friendly_summary {
            handle.update_db_stats(summary, input.stats.assets_seen);
        } else if let Some(db) = self.state_db {
            match db.get_summary().await {
                Ok(summary) => {
                    handle.update_db_stats(&summary, input.stats.assets_seen);
                }
                Err(e) => {
                    handle.record_db_summary_failure();
                    tracing::warn!(
                        error = %e,
                        "Failed to fetch DB summary for metrics; skipping DB gauge update"
                    );
                }
            }
        }
    }

    async fn write_json_report(&self, input: &CycleReportInput<'_>) {
        let Some(report_path) = self.report_path else {
            return;
        };
        let status = report::sync_status_str(
            input.session_expired,
            input.stats.interrupted,
            input.failed_count,
        );
        let (failed_assets, failed_assets_truncated) = self.failed_asset_sample().await;
        let report = report::SyncReport {
            version: "2",
            kei_version: env!("CARGO_PKG_VERSION"),
            timestamp: chrono::Utc::now().to_rfc3339(),
            status: status.to_string(),
            options: self.run_options.clone(),
            stats: input.stats.clone(),
            failed_assets,
            failed_assets_truncated,
        };
        if let Err(e) = report::write_report(report_path, &report).await {
            tracing::warn!(
                error = %e,
                path = %report_path.display(),
                "Failed to write JSON report"
            );
        }
    }

    async fn failed_asset_sample(&self) -> (Vec<report::FailedAssetEntry>, usize) {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "FAILED_ASSETS_CAP is a small compile-time constant well under u32::MAX"
        )]
        let cap_u32 = report::FAILED_ASSETS_CAP as u32;
        match self.state_db {
            Some(db) => match db.get_failed_sample(cap_u32).await {
                Ok((records, total)) => {
                    #[allow(
                        clippy::cast_possible_truncation,
                        reason = "failed-asset totals are persisted counts of per-sync failures, comfortably below usize::MAX on 64-bit"
                    )]
                    let total_usize = total as usize;
                    let truncated = total_usize.saturating_sub(report::FAILED_ASSETS_CAP);
                    let entries = records
                        .iter()
                        .map(report::FailedAssetEntry::from_record)
                        .collect();
                    (entries, truncated)
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "Failed to load failed_assets for sync_report.json"
                    );
                    (Vec::new(), 0)
                }
            },
            None => (Vec::new(), 0),
        }
    }

    fn notify_cycle_outcome(&self, input: &CycleReportInput<'_>) {
        if input.session_expired {
            return;
        }
        let data = notifications::SyncNotificationData::from(input.stats);
        if input.failed_count > 0 {
            self.notifier.notify(
                notifications::Event::SyncFailed,
                &format!("{} sync failures", input.failed_count),
                self.username,
                Some(&data),
            );
        } else {
            self.notifier.notify(
                notifications::Event::SyncComplete,
                "Sync completed successfully",
                self.username,
                Some(&data),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_options() -> RunOptions {
        let download_dir = tempfile::tempdir().unwrap();
        let globals = crate::config::GlobalArgs {
            username: Some("reporter@example.com".to_string()),
            domain: None,
            data_dir: None,
        };
        let sync = crate::cli::SyncArgs {
            config_overrides: crate::config::SyncConfigOverrides {
                download_dir: Some(download_dir.path().display().to_string()),
                ..Default::default()
            },
            ..crate::cli::SyncArgs::default()
        };
        let config = crate::config::Config::build(
            &globals,
            &crate::cli::PasswordArgs::default(),
            sync,
            None,
        )
        .unwrap();
        RunOptions::from_config(&config)
    }

    fn reporter<'a>(
        dir: &'a Path,
        report_path: Option<&'a Path>,
        notifier: &'a Notifier,
    ) -> CycleReporter<'a, state::SqliteStateDb> {
        CycleReporter::new(CycleReporterConfig {
            username: "reporter@example.com",
            watch_mode: false,
            report_path,
            run_options: run_options(),
            health_dir: dir,
            personality_mode: Mode::Off,
            state_db: None,
            metrics_handle: None,
            notifier,
        })
    }

    fn reporter_with_db<'a>(
        dir: &'a Path,
        report_path: Option<&'a Path>,
        notifier: &'a Notifier,
        db: &'a state::SqliteStateDb,
    ) -> CycleReporter<'a, state::SqliteStateDb> {
        CycleReporter::new(CycleReporterConfig {
            username: "reporter@example.com",
            watch_mode: false,
            report_path,
            run_options: run_options(),
            health_dir: dir,
            personality_mode: Mode::Off,
            state_db: Some(db),
            metrics_handle: None,
            notifier,
        })
    }

    fn parse_json(path: &Path) -> serde_json::Value {
        let contents = std::fs::read_to_string(path).unwrap();
        serde_json::from_str(&contents).unwrap()
    }

    fn assert_sync_token_observation_fields(
        stats_json: &serde_json::Value,
        expected_receivers: usize,
        with_token: usize,
        missing: usize,
        blank: usize,
        dropped: usize,
        unique: usize,
    ) {
        assert_eq!(
            stats_json["sync_token_expected_receivers"],
            expected_receivers
        );
        assert_eq!(stats_json["sync_token_receivers_with_token"], with_token);
        assert_eq!(stats_json["sync_token_receivers_missing"], missing);
        assert_eq!(stats_json["sync_token_receivers_blank"], blank);
        assert_eq!(stats_json["sync_token_receivers_dropped"], dropped);
        assert_eq!(stats_json["sync_token_unique_values"], unique);
    }

    async fn report_cycle(
        reporter: &CycleReporter<'_, state::SqliteStateDb>,
        health: &mut HealthStatus,
        stats: &SyncStats,
        failed_count: usize,
        session_expired: bool,
    ) {
        reporter
            .report_completed_cycle(
                health,
                CycleReportInput {
                    stats,
                    failed_count,
                    session_expired,
                    elapsed: Duration::from_secs(7),
                },
            )
            .await;
    }

    #[tokio::test]
    async fn success_updates_health_and_writes_success_report() {
        let dir = tempfile::tempdir().unwrap();
        let report_path = dir.path().join("sync_report.json");
        let notifier = Notifier::new(None);
        let reporter = reporter(dir.path(), Some(&report_path), &notifier);
        let mut health = HealthStatus::new();
        let stats = SyncStats {
            downloaded: 2,
            bytes_downloaded: 1024,
            ..SyncStats::default()
        };

        report_cycle(&reporter, &mut health, &stats, 0, false).await;

        let health_json = parse_json(&dir.path().join("health.json"));
        assert_eq!(health_json["consecutive_failures"], 0);
        assert!(health_json["last_error"].is_null());
        let report_json = parse_json(&report_path);
        assert_eq!(report_json["status"], "success");
        assert_eq!(report_json["stats"]["downloaded"], 2);
    }

    #[tokio::test]
    async fn success_report_can_include_preexisting_failed_assets_sample() {
        let dir = tempfile::tempdir().unwrap();
        let report_path = dir.path().join("sync_report.json");
        let notifier = Notifier::new(None);
        let db = state::SqliteStateDb::open_in_memory().expect("open db");
        let failed_record = state::AssetRecord::new_pending(
            std::sync::Arc::from("PrimarySync"),
            "FAILED_OLD".to_string(),
            crate::state::VersionSizeKey::Original,
            "checksum".to_string(),
            "failed_old.jpg".to_string(),
            chrono::Utc::now(),
            None,
            1_024,
            crate::state::MediaType::Photo,
        );
        db.upsert_seen(&failed_record)
            .await
            .expect("upsert failed row");
        db.mark_failed("PrimarySync", "FAILED_OLD", "original", "old failure")
            .await
            .expect("mark failed");
        let reporter = reporter_with_db(dir.path(), Some(&report_path), &notifier, &db);
        let mut health = HealthStatus::new();
        let stats = SyncStats {
            downloaded: 1,
            failed: 0,
            ..SyncStats::default()
        };

        report_cycle(&reporter, &mut health, &stats, 0, false).await;

        let report_json = parse_json(&report_path);
        assert_eq!(report_json["status"], "success");
        assert_eq!(report_json["stats"]["failed"], 0);
        assert_eq!(report_json["failed_assets"][0]["id"], "FAILED_OLD");
        assert_eq!(
            report_json["failed_assets"][0]["error_message"],
            "old failure"
        );
    }

    #[tokio::test]
    async fn partial_failure_updates_health_and_writes_partial_report() {
        let dir = tempfile::tempdir().unwrap();
        let report_path = dir.path().join("sync_report.json");
        let notifier = Notifier::new(None);
        let reporter = reporter(dir.path(), Some(&report_path), &notifier);
        let mut health = HealthStatus::new();
        let stats = SyncStats {
            failed: 3,
            ..SyncStats::default()
        };

        report_cycle(&reporter, &mut health, &stats, 3, false).await;

        let health_json = parse_json(&dir.path().join("health.json"));
        assert_eq!(health_json["consecutive_failures"], 1);
        assert_eq!(health_json["last_error"], "3 sync failures");
        let report_json = parse_json(&report_path);
        assert_eq!(report_json["status"], "partial_failure");
        assert_eq!(report_json["stats"]["failed"], 3);
    }

    #[tokio::test]
    async fn session_expiry_updates_health_and_writes_session_expired_report() {
        let dir = tempfile::tempdir().unwrap();
        let report_path = dir.path().join("sync_report.json");
        let notifier = Notifier::new(None);
        let reporter = reporter(dir.path(), Some(&report_path), &notifier);
        let mut health = HealthStatus::new();

        report_cycle(&reporter, &mut health, &SyncStats::default(), 0, true).await;

        let health_json = parse_json(&dir.path().join("health.json"));
        assert_eq!(health_json["consecutive_failures"], 1);
        assert_eq!(health_json["last_error"], "session expired");
        let report_json = parse_json(&report_path);
        assert_eq!(report_json["status"], "session_expired");
    }

    #[tokio::test]
    async fn interrupted_cycle_writes_interrupted_report() {
        let dir = tempfile::tempdir().unwrap();
        let report_path = dir.path().join("sync_report.json");
        let notifier = Notifier::new(None);
        let reporter = reporter(dir.path(), Some(&report_path), &notifier);
        let mut health = HealthStatus::new();
        let stats = SyncStats {
            interrupted: true,
            ..SyncStats::default()
        };

        report_cycle(&reporter, &mut health, &stats, 0, false).await;

        let health_json = parse_json(&dir.path().join("health.json"));
        assert_eq!(health_json["consecutive_failures"], 0);
        let report_json = parse_json(&report_path);
        assert_eq!(report_json["status"], "interrupted");
    }

    #[tokio::test]
    async fn skipped_watch_cycle_is_health_only() {
        let dir = tempfile::tempdir().unwrap();
        let report_path = dir.path().join("sync_report.json");
        let notifier = Notifier::new(None);
        let reporter = reporter(dir.path(), Some(&report_path), &notifier);
        let mut health = HealthStatus::new();

        reporter.report_skipped_watch_cycle(&mut health).await;

        let health_json = parse_json(&dir.path().join("health.json"));
        assert_eq!(health_json["consecutive_failures"], 0);
        assert!(
            !report_path.exists(),
            "skipped cycles should not overwrite sync_report.json"
        );
    }

    #[tokio::test]
    async fn report_write_failure_does_not_block_health_update() {
        let dir = tempfile::tempdir().unwrap();
        let notifier = Notifier::new(None);
        let reporter = reporter(dir.path(), Some(dir.path()), &notifier);
        let mut health = HealthStatus::new();

        report_cycle(&reporter, &mut health, &SyncStats::default(), 0, false).await;

        let health_json = parse_json(&dir.path().join("health.json"));
        assert_eq!(health_json["consecutive_failures"], 0);
    }

    #[tokio::test]
    async fn pagination_shortfall_warning_does_not_mark_cycle_failed() {
        let dir = tempfile::tempdir().unwrap();
        let report_path = dir.path().join("sync_report.json");
        let notifier = Notifier::new(None);
        let reporter = reporter(dir.path(), Some(&report_path), &notifier);
        let mut health = HealthStatus::new();
        let stats = SyncStats {
            failed: 0,
            enumeration_errors: 0,
            pagination_shortfall_warnings: 1,
            pagination_shortfall_assets: 45,
            sync_token_blocked: true,
            sync_token_blocked_reason: Some("pagination_shortfall"),
            ..SyncStats::default()
        };

        report_cycle(&reporter, &mut health, &stats, 0, false).await;

        let health_json = parse_json(&dir.path().join("health.json"));
        assert_eq!(health_json["consecutive_failures"], 0);
        assert!(health_json["last_error"].is_null());

        let report_json = parse_json(&report_path);
        assert_eq!(report_json["status"], "success");
        assert_eq!(report_json["stats"]["failed"], 0);
        assert_eq!(report_json["stats"]["enumeration_errors"], 0);
        assert_eq!(report_json["stats"]["pagination_shortfall_warnings"], 1);
        assert_eq!(report_json["stats"]["pagination_shortfall_assets"], 45);
        assert_eq!(report_json["stats"]["sync_token_blocked"], true);
        assert_eq!(
            report_json["stats"]["sync_token_blocked_reason"],
            "pagination_shortfall"
        );
    }

    #[tokio::test]
    async fn sync_token_blocked_diagnostics_serialize_to_report_json() {
        let dir = tempfile::tempdir().unwrap();
        let report_path = dir.path().join("sync_report.json");
        let notifier = Notifier::new(None);
        let reporter = reporter(dir.path(), Some(&report_path), &notifier);
        let mut health = HealthStatus::new();
        let stats = SyncStats {
            sync_token_blocked: true,
            sync_token_blocked_reason: Some("icloud_sync_token_missing"),
            sync_token_blocked_source: Some("icloud"),
            sync_token_blocked_explanation: Some(
                "iCloud did not return a sync token for this full enumeration",
            ),
            sync_token_blocked_zone: Some("PrimarySync".to_string()),
            sync_token_expected_receivers: Some(3),
            sync_token_receivers_with_token: Some(0),
            sync_token_receivers_missing: Some(3),
            sync_token_receivers_blank: Some(0),
            sync_token_receivers_dropped: Some(0),
            sync_token_unique_values: Some(0),
            ..SyncStats::default()
        };

        report_cycle(&reporter, &mut health, &stats, 0, false).await;

        let report_json = parse_json(&report_path);
        let stats_json = &report_json["stats"];
        assert_eq!(
            stats_json["sync_token_blocked_reason"],
            "icloud_sync_token_missing"
        );
        assert_eq!(stats_json["sync_token_blocked_source"], "icloud");
        assert_eq!(
            stats_json["sync_token_blocked_explanation"],
            "iCloud did not return a sync token for this full enumeration"
        );
        assert_eq!(stats_json["sync_token_blocked_zone"], "PrimarySync");
        assert_sync_token_observation_fields(stats_json, 3, 0, 3, 0, 0, 0);
    }

    #[tokio::test]
    async fn sync_token_observation_fields_serialize_even_when_not_blocked() {
        let dir = tempfile::tempdir().unwrap();
        let report_path = dir.path().join("sync_report.json");
        let notifier = Notifier::new(None);
        let reporter = reporter(dir.path(), Some(&report_path), &notifier);
        let mut health = HealthStatus::new();
        let stats = SyncStats {
            sync_token_blocked: false,
            sync_token_expected_receivers: Some(2),
            sync_token_receivers_with_token: Some(2),
            sync_token_receivers_missing: Some(0),
            sync_token_receivers_blank: Some(0),
            sync_token_receivers_dropped: Some(0),
            sync_token_unique_values: Some(1),
            ..SyncStats::default()
        };

        report_cycle(&reporter, &mut health, &stats, 0, false).await;

        let report_json = parse_json(&report_path);
        let stats_json = &report_json["stats"];
        assert_eq!(stats_json["sync_token_blocked"], false);
        assert_sync_token_observation_fields(stats_json, 2, 2, 0, 0, 0, 1);
        assert!(
            stats_json["sync_token_blocked_reason"].is_null(),
            "reason should stay absent when sync token was not blocked"
        );
    }
}
