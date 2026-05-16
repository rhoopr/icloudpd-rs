//! HTTP observability server (watch-mode only).
//!
//! In watch mode, spawns an axum HTTP server on `--http-port` (default 9090) that serves:
//! - `GET /healthz`  — JSON health status (same data as `health.json`)
//! - `GET /metrics`  — Prometheus text format
//! - `GET /` plus `/control/*` and `/events` loopback-only control endpoints
//!
//! Metrics are updated after every sync cycle by calling [`MetricsHandle::update`].
//! On skipped cycles (no changes detected), call [`MetricsHandle::update_health_only`]
//! to refresh the health gauges without clobbering cycle counters or duration.
//! All counters are cumulative across cycles (they never reset while the process
//! is running), matching Prometheus conventions.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};

use chrono::{DateTime, Utc};

use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::{header, HeaderValue, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use prometheus_client::encoding::text::encode;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;
use tokio::sync::{broadcast, Mutex, Notify};
use tokio_util::sync::CancellationToken;

use crate::download::SyncStats;
use crate::health::HealthStatus;
use crate::state::types::SyncSummary;

/// Cumulative count of `mark_downloaded` calls that matched 0 rows in the
/// `assets` table — i.e. a download was promoted without a matching
/// `upsert_seen` row, or the row was deleted between upsert and download.
///
/// State-DB code can't depend on `MetricsHandle` (which is built only in
/// watch / metrics-port mode), so the counter lives here as a static and is
/// registered from `MetricsHandle::new`. Increments from `SqliteStateDb`
/// are no-ops without a registered handle; once the registry exists the
/// /metrics scrape exposes the cumulative count under the
/// `kei_state_mark_downloaded_zero_rows_total` series.
pub(crate) static MARK_DOWNLOADED_ZERO_ROWS: LazyLock<Counter> = LazyLock::new(Counter::default);

/// Cumulative count of `mark_failed` calls that matched 0 rows in the
/// `assets` table — i.e. a failure was recorded against an asset that
/// was never `upsert_seen`d. The producer-dispatch invariant promises
/// the upsert always runs first, so a non-zero count here is an actionable
/// invariant violation that needs investigation: the failure isn't
/// persisted, so the asset will re-enumerate and re-fail next sync until
/// the buggy call site is found. Mirrors `MARK_DOWNLOADED_ZERO_ROWS`.
pub(crate) static MARK_FAILED_ZERO_ROWS: LazyLock<Counter> = LazyLock::new(Counter::default);

// ── Label types ──────────────────────────────────────────────────────────────

/// Label set for the `kei_sync_skipped_total` counter family.
#[derive(Debug, Clone, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct SkipLabels {
    reason: &'static str,
}

/// Label set for the `kei_db_assets_total` and `kei_db_assets_size_bytes` gauge families.
#[derive(Clone, Debug, Hash, PartialEq, Eq, prometheus_client::encoding::EncodeLabelSet)]
struct StatusLabels {
    status: &'static str,
}

// ── State shared between the HTTP handlers and the sync loop ─────────────────

const SSE_CAPACITY: usize = 64;
const RECENT_ERROR_CAP: usize = 5;

#[derive(Debug, Clone)]
struct RecentError {
    at: DateTime<Utc>,
    message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StatusStatsSnapshot {
    assets_seen: u64,
    downloaded: usize,
    failed: usize,
    skipped: usize,
}

impl From<&SyncStats> for StatusStatsSnapshot {
    fn from(stats: &SyncStats) -> Self {
        Self {
            assets_seen: stats.assets_seen,
            downloaded: stats.downloaded,
            failed: stats.failed,
            skipped: stats.skipped.total(),
        }
    }
}

#[derive(Debug, Clone)]
struct StatusSnapshot {
    service_started_at: DateTime<Utc>,
    watch_interval_secs: Option<u64>,
    last_outcome: Option<String>,
    last_stats: Option<StatusStatsSnapshot>,
    library_summary: Option<SyncSummary>,
    recent_errors: Vec<RecentError>,
}

impl StatusSnapshot {
    fn new(watch_interval_secs: Option<u64>) -> Self {
        Self {
            service_started_at: Utc::now(),
            watch_interval_secs,
            last_outcome: None,
            last_stats: None,
            library_summary: None,
            recent_errors: Vec::new(),
        }
    }

    fn push_error(&mut self, message: String) {
        if self
            .recent_errors
            .last()
            .is_some_and(|last| last.message == message)
        {
            return;
        }
        self.recent_errors.push(RecentError {
            at: Utc::now(),
            message,
        });
        if self.recent_errors.len() > RECENT_ERROR_CAP {
            let over = self.recent_errors.len() - RECENT_ERROR_CAP;
            self.recent_errors.drain(0..over);
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct StructuredEvent {
    #[serde(rename = "type")]
    kind: &'static str,
    timestamp: DateTime<Utc>,
    data: serde_json::Value,
}

impl StructuredEvent {
    pub(crate) fn new(kind: &'static str, data: serde_json::Value) -> Self {
        Self {
            kind,
            timestamp: Utc::now(),
            data,
        }
    }
}

/// Health snapshot read by the /healthz handler. The registry is immutable
/// after construction so it lives directly on `MetricsHandle` behind an Arc,
/// letting /metrics encode without taking the lock.
#[derive(Debug)]
struct Inner {
    health_snapshot: Option<HealthStatus>,
    status_snapshot: StatusSnapshot,
    /// Maximum age of `last_success_at` before /healthz returns 503. `None`
    /// disables the staleness check (e.g. one-shot syncs where a single
    /// success is final). Set at construction time from `watch_interval * 2`
    /// so a single missed cycle is tolerable but two consecutive misses flip
    /// the endpoint to failing.
    staleness_threshold: Option<chrono::Duration>,
}

#[derive(Debug)]
struct StatusPageView {
    service_started_at: DateTime<Utc>,
    watch_interval_secs: Option<u64>,
    last_sync_at: Option<DateTime<Utc>>,
    last_success_at: Option<DateTime<Utc>>,
    last_outcome: String,
    last_stats: Option<StatusStatsSnapshot>,
    library_summary: Option<SyncSummary>,
    recent_errors: Vec<RecentError>,
    paused: bool,
}

/// Cheap-to-clone handle passed to the sync loop and into axum state.
#[derive(Clone)]
pub(crate) struct MetricsHandle {
    /// Prometheus registry — immutable after `new()`, so no lock needed for reads.
    registry: Arc<Registry>,
    /// Protects the /healthz snapshot only.
    inner: Arc<Mutex<Inner>>,
    pause_flag: Arc<AtomicBool>,
    sync_now_notify: Arc<Notify>,
    event_broadcast: broadcast::Sender<StructuredEvent>,
    // Metric handles use atomics internally; no lock needed for updates.
    assets_seen: Counter,
    downloaded: Counter,
    failed: Counter,
    skipped: Family<SkipLabels, Counter>,
    bytes_downloaded: Counter,
    disk_bytes_written: Counter,
    exif_failures: Counter,
    state_write_failures: Counter,
    enumeration_errors: Counter,
    session_expirations: Counter,
    cycle_duration_seconds: Gauge<f64, AtomicU64>,
    consecutive_failures: Gauge,
    last_success_timestamp: Gauge<f64, AtomicU64>,
    interrupted_cycles: Counter,
    // DB-backed gauges (only populated when --metrics-port is set).
    db_assets_total: Family<StatusLabels, Gauge>,
    db_assets_size_bytes: Family<StatusLabels, Gauge>,
    db_last_sync_assets_seen: Gauge,
    db_summary_read_failures: Counter,
    throttle_pressure: Gauge<f64, AtomicU64>,
    throttle_current_interval_seconds: Gauge<f64, AtomicU64>,
    disk_free: Gauge,
}

impl std::fmt::Debug for MetricsHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MetricsHandle").finish_non_exhaustive()
    }
}

impl MetricsHandle {
    /// Build the registry and register all metrics.
    #[cfg(test)]
    pub(crate) fn new(staleness_threshold: Option<chrono::Duration>) -> Self {
        Self::new_with_watch_interval(staleness_threshold, None)
    }

    /// Build the registry, register all metrics, and seed the status page with
    /// the configured watch interval.
    #[allow(
        clippy::too_many_lines,
        reason = "metric registration is intentionally centralized so series names, help text, and handles stay together"
    )]
    pub(crate) fn new_with_watch_interval(
        staleness_threshold: Option<chrono::Duration>,
        watch_interval_secs: Option<u64>,
    ) -> Self {
        let mut registry = Registry::default();

        let assets_seen = Counter::default();
        registry.register(
            "kei_sync_assets_seen",
            "Total number of assets enumerated from iCloud across all sync cycles",
            assets_seen.clone(),
        );

        let downloaded = Counter::default();
        registry.register(
            "kei_sync_downloaded",
            "Total number of assets successfully downloaded",
            downloaded.clone(),
        );

        let failed = Counter::default();
        registry.register(
            "kei_sync_failed",
            "Total number of asset download failures",
            failed.clone(),
        );

        let skipped: Family<SkipLabels, Counter> = Family::default();
        registry.register(
            "kei_sync_skipped",
            "Total number of assets skipped, by reason",
            skipped.clone(),
        );

        let bytes_downloaded = Counter::default();
        registry.register(
            "kei_sync_bytes_downloaded",
            "Total bytes received over the network",
            bytes_downloaded.clone(),
        );

        let disk_bytes_written = Counter::default();
        registry.register(
            "kei_sync_disk_bytes_written",
            "Total bytes written to disk",
            disk_bytes_written.clone(),
        );

        let exif_failures = Counter::default();
        registry.register(
            "kei_sync_exif_failures",
            "Total number of EXIF stamping failures",
            exif_failures.clone(),
        );

        let state_write_failures = Counter::default();
        registry.register(
            "kei_sync_state_write_failures",
            "Total number of SQLite state write failures",
            state_write_failures.clone(),
        );

        let enumeration_errors = Counter::default();
        registry.register(
            "kei_sync_enumeration_errors",
            "Total number of iCloud API enumeration errors",
            enumeration_errors.clone(),
        );

        let session_expirations = Counter::default();
        registry.register(
            "kei_sync_session_expirations",
            "Total number of sync cycles aborted due to an expired iCloud session",
            session_expirations.clone(),
        );

        let cycle_duration_seconds: Gauge<f64, AtomicU64> = Gauge::default();
        registry.register(
            "kei_sync_cycle_duration_seconds",
            "Wall-clock duration of the most recent sync cycle in seconds",
            cycle_duration_seconds.clone(),
        );

        let consecutive_failures: Gauge = Gauge::default();
        registry.register(
            "kei_health_consecutive_failures",
            "Number of consecutive sync cycle failures",
            consecutive_failures.clone(),
        );

        let last_success_timestamp: Gauge<f64, AtomicU64> = Gauge::default();
        registry.register(
            "kei_health_last_success_timestamp_seconds",
            "Unix timestamp of the last successful sync cycle (0 if never succeeded)",
            last_success_timestamp.clone(),
        );

        let interrupted_cycles = Counter::default();
        registry.register(
            "kei_sync_interrupted_cycles",
            "Total number of sync cycles interrupted by a shutdown signal",
            interrupted_cycles.clone(),
        );

        let db_assets_total: Family<StatusLabels, Gauge> = Family::default();
        registry.register(
            "kei_db_assets_total",
            "Current number of assets in the database, by status",
            db_assets_total.clone(),
        );

        let db_assets_size_bytes: Family<StatusLabels, Gauge> = Family::default();
        registry.register(
            "kei_db_assets_size_bytes",
            "Current total size in bytes of assets in the database, by status",
            db_assets_size_bytes.clone(),
        );

        let db_last_sync_assets_seen: Gauge = Gauge::default();
        registry.register(
            "kei_db_last_sync_assets_seen",
            "Number of assets enumerated from iCloud in the most recent sync cycle",
            db_last_sync_assets_seen.clone(),
        );

        let db_summary_read_failures = Counter::default();
        registry.register(
            "kei_db_summary_read_failures",
            "Total number of failed attempts to read the DB summary for metrics",
            db_summary_read_failures.clone(),
        );

        let throttle_pressure: Gauge<f64, AtomicU64> = Gauge::default();
        registry.register(
            "kei_throttle_pressure",
            "Current adaptive throttle pressure (0.0 = calm, 1.0 = max)",
            throttle_pressure.clone(),
        );

        let throttle_current_interval_seconds: Gauge<f64, AtomicU64> = Gauge::default();
        registry.register(
            "kei_throttle_current_interval_seconds",
            "Current watch interval in seconds (may be scaled above baseline by throttle)",
            throttle_current_interval_seconds.clone(),
        );

        let disk_free: Gauge = Gauge::default();
        registry.register(
            "kei_disk_free_bytes",
            "Current free disk space on the download volume in bytes",
            disk_free.clone(),
        );

        registry.register(
            "kei_state_mark_downloaded_zero_rows",
            "Total number of mark_downloaded calls that matched 0 rows in the assets table — \
             a downloaded file with no matching upsert_seen row (asset deleted between upsert \
             and download, or producer-dispatch invariant violated)",
            MARK_DOWNLOADED_ZERO_ROWS.clone(),
        );

        registry.register(
            "kei_state_mark_failed_zero_rows",
            "Total number of mark_failed calls that matched 0 rows in the assets table — \
             a failure recorded for an asset that was never upsert_seen'd \
             (producer-dispatch invariant violated; failure not persisted so the \
             asset will re-enumerate and re-fail next sync)",
            MARK_FAILED_ZERO_ROWS.clone(),
        );

        let (event_broadcast, _) = broadcast::channel(SSE_CAPACITY);

        Self {
            registry: Arc::new(registry),
            inner: Arc::new(Mutex::new(Inner {
                health_snapshot: None,
                status_snapshot: StatusSnapshot::new(watch_interval_secs),
                staleness_threshold,
            })),
            pause_flag: Arc::new(AtomicBool::new(false)),
            sync_now_notify: Arc::new(Notify::new()),
            event_broadcast,
            assets_seen,
            downloaded,
            failed,
            skipped,
            bytes_downloaded,
            disk_bytes_written,
            exif_failures,
            state_write_failures,
            enumeration_errors,
            session_expirations,
            cycle_duration_seconds,
            consecutive_failures,
            last_success_timestamp,
            interrupted_cycles,
            db_assets_total,
            db_assets_size_bytes,
            db_last_sync_assets_seen,
            db_summary_read_failures,
            throttle_pressure,
            throttle_current_interval_seconds,
            disk_free,
        }
    }

    /// Update all metrics from the latest completed sync cycle.
    ///
    /// Counters are incremented by this cycle's values; gauges are set to the
    /// latest value. Call this after every cycle that actually ran.
    pub(crate) async fn update(&self, stats: &SyncStats, health: &HealthStatus) {
        // Counters — increment by this cycle's values.
        self.assets_seen.inc_by(stats.assets_seen);
        self.downloaded.inc_by(stats.downloaded as u64);
        self.failed.inc_by(stats.failed as u64);
        self.bytes_downloaded.inc_by(stats.bytes_downloaded);
        self.disk_bytes_written.inc_by(stats.disk_bytes_written);
        self.exif_failures.inc_by(stats.exif_failures as u64);
        self.state_write_failures
            .inc_by(stats.state_write_failures as u64);
        self.enumeration_errors
            .inc_by(stats.enumeration_errors as u64);

        if stats.interrupted {
            self.interrupted_cycles.inc();
        }

        // Skip breakdown counters with reason labels.
        self.inc_skip("by_state", stats.skipped.by_state);
        self.inc_skip("on_disk", stats.skipped.on_disk);
        self.inc_skip("by_media_type", stats.skipped.by_media_type);
        self.inc_skip("by_date_range", stats.skipped.by_date_range);
        self.inc_skip("by_live_photo", stats.skipped.by_live_photo);
        self.inc_skip("by_filename", stats.skipped.by_filename);
        self.inc_skip("by_excluded_album", stats.skipped.by_excluded_album);
        self.inc_skip("ampm_variant", stats.skipped.ampm_variant);
        self.inc_skip("duplicates", stats.skipped.duplicates);
        self.inc_skip("retry_exhausted", stats.skipped.retry_exhausted);
        self.inc_skip("retry_only", stats.skipped.retry_only);

        // Gauges — set to latest value.
        self.cycle_duration_seconds.set(stats.elapsed_secs);

        self.update_health_gauges(health).await;
    }

    /// Update only the health gauges and the /healthz snapshot.
    ///
    /// Use this on skipped cycles (no changes detected in watch mode) so that
    /// `cycle_duration_seconds` and download counters are not clobbered.
    pub(crate) async fn update_health_only(&self, health: &HealthStatus) {
        self.update_health_gauges(health).await;
    }

    /// Increment the session expiration counter.
    ///
    /// Call this whenever a sync cycle is aborted due to an expired iCloud
    /// session, in addition to the normal health update.
    pub(crate) fn record_session_expiration(&self) {
        self.session_expirations.inc();
    }

    /// Update DB-backed gauges from the current state database summary.
    ///
    /// Call this after [`Self::update`] on every real sync cycle, guarded by a
    /// `state_db.as_ref()` check so dry-run and metrics-disabled paths are
    /// no-ops. `assets_seen` comes from [`SyncStats`] already in hand at the
    /// call site, so no extra DB query is needed for that gauge.
    pub(crate) fn update_db_stats(&self, summary: &SyncSummary, assets_seen: u64) {
        self.db_assets_total
            .get_or_create(&StatusLabels {
                status: "downloaded",
            })
            .set(i64::try_from(summary.downloaded).unwrap_or(i64::MAX));
        self.db_assets_total
            .get_or_create(&StatusLabels { status: "pending" })
            .set(i64::try_from(summary.pending).unwrap_or(i64::MAX));
        self.db_assets_total
            .get_or_create(&StatusLabels { status: "failed" })
            .set(i64::try_from(summary.failed).unwrap_or(i64::MAX));
        self.db_assets_size_bytes
            .get_or_create(&StatusLabels {
                status: "downloaded",
            })
            .set(i64::try_from(summary.downloaded_bytes).unwrap_or(i64::MAX));
        self.db_last_sync_assets_seen
            .set(i64::try_from(assets_seen).unwrap_or(i64::MAX));
    }

    /// Increment the counter for failed DB summary reads.
    ///
    /// Call this whenever [`get_summary`](crate::state::StateDb::get_summary)
    /// returns an error during a metrics update so the failure is visible in
    /// the `/metrics` output rather than only in the log.
    pub(crate) fn record_db_summary_failure(&self) {
        self.db_summary_read_failures.inc();
    }

    /// Update throttle-related gauges.
    ///
    /// Call this after [`Self::update`] on every cycle when a throttle
    /// controller is active. Mirrors the health-gauge pattern: no-op when
    /// the handle was never built (one-shot / metrics-disabled).
    pub(crate) fn update_throttle(&self, pressure: f64, interval_secs: u64) {
        self.throttle_pressure.set(pressure);
        #[allow(
            clippy::cast_precision_loss,
            reason = "OpenMetrics gauge format is f64 seconds; sub-second precision is below the metric's granularity"
        )]
        self.throttle_current_interval_seconds
            .set(interval_secs as f64);
    }

    /// Update the free-disk gauge from a consumer-side recheck.
    pub(crate) fn update_disk_free(&self, bytes: u64) {
        self.disk_free.set(i64::try_from(bytes).unwrap_or(i64::MAX));
    }

    pub(crate) fn is_paused(&self) -> bool {
        self.pause_flag.load(Ordering::SeqCst)
    }

    pub(crate) async fn wait_for_control_signal(&self) {
        self.sync_now_notify.notified().await;
    }

    pub(crate) fn publish_event(&self, event: StructuredEvent) {
        let _ignored = self.event_broadcast.send(event);
    }

    pub(crate) async fn update_status_snapshot(
        &self,
        outcome: &str,
        stats: Option<&SyncStats>,
        summary: Option<&SyncSummary>,
    ) {
        let mut inner = self.inner.lock().await;
        inner.status_snapshot.last_outcome = Some(outcome.to_string());
        if let Some(stats) = stats {
            inner.status_snapshot.last_stats = Some(StatusStatsSnapshot::from(stats));
        }
        if let Some(summary) = summary {
            inner.status_snapshot.library_summary = Some(summary.clone());
        }
    }

    pub(crate) fn publish_sync_started(&self, cycle: u64) {
        self.publish_event(StructuredEvent::new(
            "sync_started",
            serde_json::json!({ "cycle": cycle }),
        ));
    }

    pub(crate) fn publish_sync_skipped(&self, cycle: u64, reason: &'static str) {
        self.publish_event(StructuredEvent::new(
            "sync_skipped",
            serde_json::json!({ "cycle": cycle, "reason": reason }),
        ));
    }

    pub(crate) fn publish_sync_finished(&self, cycle: u64, outcome: &str, stats: &SyncStats) {
        self.publish_event(StructuredEvent::new(
            "sync_finished",
            serde_json::json!({
                "cycle": cycle,
                "outcome": outcome,
                "assets_seen": stats.assets_seen,
                "downloaded": stats.downloaded,
                "failed": stats.failed,
                "skipped": stats.skipped.total(),
            }),
        ));
    }

    async fn update_health_gauges(&self, health: &HealthStatus) {
        self.consecutive_failures
            .set(i64::from(health.consecutive_failures));
        let last_success_ts = health.last_success_at.map_or(0.0, |t| {
            #[allow(clippy::cast_precision_loss, reason = "OpenMetrics timestamp format is f64 seconds; precision loss at the second level is below the metric's granularity")]
            let ts = t.timestamp() as f64;
            ts
        });
        self.last_success_timestamp.set(last_success_ts);

        let mut inner = self.inner.lock().await;
        inner.health_snapshot = Some(HealthStatus {
            last_sync_at: health.last_sync_at,
            last_success_at: health.last_success_at,
            consecutive_failures: health.consecutive_failures,
            last_error: health.last_error.clone(),
        });
        if let Some(error) = &health.last_error {
            inner.status_snapshot.push_error(error.clone());
        }
    }

    fn inc_skip(&self, reason: &'static str, count: usize) {
        if count > 0 {
            self.skipped
                .get_or_create(&SkipLabels { reason })
                .inc_by(count as u64);
        }
    }
}

// ── HTTP handlers ─────────────────────────────────────────────────────────────

async fn handle_metrics(State(handle): State<MetricsHandle>) -> impl IntoResponse {
    let mut buf = String::new();
    encode(&mut buf, &handle.registry).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "Failed to encode Prometheus metrics");
    });

    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/openmetrics-text; version=1.0.0; charset=utf-8"),
        )],
        buf,
    )
}

async fn handle_healthz(State(handle): State<MetricsHandle>) -> impl IntoResponse {
    let inner = handle.inner.lock().await;
    let staleness_threshold = inner.staleness_threshold;
    match &inner.health_snapshot {
        Some(h) => {
            let consecutive_failures = h.consecutive_failures;
            let stale = match (staleness_threshold, h.last_success_at) {
                (Some(max_age), Some(last_success)) => (Utc::now() - last_success) > max_age,
                _ => false,
            };
            match serde_json::to_string_pretty(h) {
                Ok(json) => {
                    let status = if consecutive_failures >= 5 || stale {
                        StatusCode::SERVICE_UNAVAILABLE
                    } else {
                        StatusCode::OK
                    };
                    (
                        status,
                        [(
                            header::CONTENT_TYPE,
                            HeaderValue::from_static("application/json"),
                        )],
                        json,
                    )
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to serialize health status for /healthz");
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        [(
                            header::CONTENT_TYPE,
                            HeaderValue::from_static("application/json"),
                        )],
                        r#"{"error":"serialization failed"}"#.to_string(),
                    )
                }
            }
        }
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            )],
            r#"{"status":"no sync cycle completed yet"}"#.to_string(),
        ),
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "the status endpoint is a small server-rendered HTML page kept in one place for readability"
)]
async fn handle_status_page(State(handle): State<MetricsHandle>) -> impl IntoResponse {
    let view = {
        let inner = handle.inner.lock().await;
        let health = inner.health_snapshot.as_ref();
        let snapshot = &inner.status_snapshot;
        StatusPageView {
            service_started_at: snapshot.service_started_at,
            watch_interval_secs: snapshot.watch_interval_secs,
            last_sync_at: health.and_then(|h| h.last_sync_at),
            last_success_at: health.and_then(|h| h.last_success_at),
            last_outcome: snapshot
                .last_outcome
                .clone()
                .unwrap_or_else(|| "no sync cycle completed yet".to_string()),
            last_stats: snapshot.last_stats,
            library_summary: snapshot.library_summary.clone(),
            recent_errors: snapshot.recent_errors.clone(),
            paused: handle.is_paused(),
        }
    };

    let uptime = Utc::now() - view.service_started_at;
    let uptime = format_duration_human(uptime);
    let watch_interval = view
        .watch_interval_secs
        .map_or_else(|| "one-shot".to_string(), |secs| format!("{secs}s"));
    let last_sync = view
        .last_sync_at
        .map_or_else(|| "never".to_string(), |t| t.to_rfc3339());
    let last_success = view
        .last_success_at
        .map_or_else(|| "never".to_string(), |t| t.to_rfc3339());
    let outcome = view.last_outcome;
    let stats = view.last_stats;
    let assets_seen = stats.map_or(0, |s| s.assets_seen);
    let downloaded = stats.map_or(0, |s| s.downloaded);
    let failed = stats.map_or(0, |s| s.failed);
    let skipped = stats.map_or(0, |s| s.skipped);
    let library = view.library_summary.as_ref();
    let total_assets = library.map_or(0, |s| s.total_assets);
    let downloaded_assets = library.map_or(0, |s| s.downloaded);
    let pending_assets = library.map_or(0, |s| s.pending);
    let failed_assets = library.map_or(0, |s| s.failed);
    let downloaded_bytes = library.map_or(0, |s| s.downloaded_bytes);
    let errors = if view.recent_errors.is_empty() {
        "<li>No recent errors</li>".to_string()
    } else {
        view.recent_errors
            .iter()
            .rev()
            .map(|e| {
                format!(
                    "<li><time>{}</time> {}</li>",
                    e.at.to_rfc3339(),
                    escape_html(&e.message)
                )
            })
            .collect::<String>()
    };

    let html = format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta http-equiv="refresh" content="30">
  <title>kei status</title>
  <style>
    body {{ font-family: system-ui, sans-serif; max-width: 56rem; margin: 2rem auto; padding: 0 1rem; color: #1f2937; }}
    header {{ display: flex; justify-content: space-between; align-items: baseline; border-bottom: 1px solid #d1d5db; margin-bottom: 1rem; }}
    h1 {{ margin: 0 0 .5rem; }}
    section {{ border: 1px solid #e5e7eb; border-radius: .5rem; padding: 1rem; margin: 1rem 0; }}
    dl {{ display: grid; grid-template-columns: 12rem 1fr; gap: .35rem 1rem; }}
    dt {{ font-weight: 700; }}
    dd {{ margin: 0; }}
    form {{ display: inline; margin-right: .5rem; }}
    button {{ padding: .4rem .75rem; }}
    .pill {{ border-radius: 999px; padding: .2rem .6rem; background: #e5e7eb; }}
  </style>
</head>
<body>
  <header>
    <h1>kei <span class="pill">{}</span></h1>
    <time>{}</time>
  </header>
  <section>
    <h2>Service status</h2>
    <dl>
      <dt>State</dt><dd>{}</dd>
      <dt>Uptime</dt><dd>{}</dd>
      <dt>Watch interval</dt><dd>{}</dd>
    </dl>
  </section>
  <section>
    <h2>Last sync</h2>
    <dl>
      <dt>Last sync</dt><dd>{}</dd>
      <dt>Last success</dt><dd>{}</dd>
      <dt>Outcome</dt><dd>{}</dd>
      <dt>Assets seen</dt><dd>{}</dd>
      <dt>Downloaded</dt><dd>{}</dd>
      <dt>Failed</dt><dd>{}</dd>
      <dt>Skipped</dt><dd>{}</dd>
    </dl>
  </section>
  <section>
    <h2>Library</h2>
    <dl>
      <dt>Total assets</dt><dd>{}</dd>
      <dt>Downloaded assets</dt><dd>{}</dd>
      <dt>Pending assets</dt><dd>{}</dd>
      <dt>Failed assets</dt><dd>{}</dd>
      <dt>Downloaded bytes</dt><dd>{}</dd>
    </dl>
  </section>
  <section>
    <h2>Recent errors</h2>
    <ul>{}</ul>
  </section>
  <section>
    <h2>Controls</h2>
    <form method="post" action="/control/pause"><button type="submit">Pause</button></form>
    <form method="post" action="/control/resume"><button type="submit">Resume</button></form>
    <form method="post" action="/control/sync-now"><button type="submit">Sync Now</button></form>
  </section>
</body>
</html>"#,
        env!("CARGO_PKG_VERSION"),
        Utc::now().to_rfc3339(),
        if view.paused { "paused" } else { "running" },
        uptime,
        watch_interval,
        last_sync,
        last_success,
        escape_html(&outcome),
        assets_seen,
        downloaded,
        failed,
        skipped,
        total_assets,
        downloaded_assets,
        pending_assets,
        failed_assets,
        downloaded_bytes,
        errors
    );

    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        )],
        html,
    )
}

async fn handle_pause(State(handle): State<MetricsHandle>) -> impl IntoResponse {
    handle.pause_flag.store(true, Ordering::SeqCst);
    handle.sync_now_notify.notify_one();
    handle.publish_event(StructuredEvent::new(
        "paused",
        serde_json::json!({ "status": "paused" }),
    ));
    json_response(r#"{"status":"paused"}"#)
}

async fn handle_resume(State(handle): State<MetricsHandle>) -> impl IntoResponse {
    handle.pause_flag.store(false, Ordering::SeqCst);
    handle.sync_now_notify.notify_one();
    handle.publish_event(StructuredEvent::new(
        "resumed",
        serde_json::json!({ "status": "running" }),
    ));
    json_response(r#"{"status":"running"}"#)
}

async fn handle_sync_now(State(handle): State<MetricsHandle>) -> impl IntoResponse {
    handle.sync_now_notify.notify_one();
    handle.publish_event(StructuredEvent::new(
        "sync_now",
        serde_json::json!({ "status": "sync triggered" }),
    ));
    json_response(r#"{"status":"sync triggered"}"#)
}

async fn handle_sse_events(State(handle): State<MetricsHandle>) -> impl IntoResponse {
    let rx = handle.event_broadcast.subscribe();
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let data = serde_json::to_string(&event)
                        .unwrap_or_else(|_| r#"{"type":"serialization_failed"}"#.to_string());
                    return Some((Ok::<_, Infallible>(SseEvent::default().data(data)), rx));
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    });

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(std::time::Duration::from_secs(15))
            .text("ping"),
    )
}

async fn require_loopback_control(req: Request<Body>, next: Next) -> Response {
    let loopback = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .is_some_and(|ConnectInfo(addr)| addr.ip().is_loopback());
    if loopback {
        next.run(req).await
    } else {
        (
            StatusCode::FORBIDDEN,
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            )],
            r#"{"error":"control endpoints are loopback-only"}"#,
        )
            .into_response()
    }
}

fn format_duration_human(duration: chrono::Duration) -> String {
    let secs = duration.num_seconds().max(0);
    let hours = secs / 3600;
    let minutes = (secs % 3600) / 60;
    let seconds = secs % 60;
    format!("{hours}h {minutes}m {seconds}s")
}

fn escape_html(input: &str) -> String {
    let mut escaped = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn json_response(body: &'static str) -> impl IntoResponse {
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )],
        body,
    )
}

// ── Server entrypoint ─────────────────────────────────────────────────────────

/// Bind and spawn the metrics HTTP server as a background tokio task.
///
/// Binds synchronously so that a misconfigured port fails at startup rather
/// than silently. Returns a `MetricsHandle` the sync loop uses to push metrics
/// after each cycle and a `JoinHandle` so the sync loop can await graceful
/// shutdown before the runtime drops. The server shuts down gracefully when
/// `shutdown_token` is cancelled.
pub(crate) fn spawn_server(
    bind: std::net::IpAddr,
    control_bind: std::net::IpAddr,
    port: u16,
    shutdown_token: CancellationToken,
    staleness_threshold: Option<chrono::Duration>,
    watch_interval_secs: Option<u64>,
) -> anyhow::Result<(MetricsHandle, tokio::task::JoinHandle<()>, SocketAddr)> {
    anyhow::ensure!(
        control_bind.is_loopback(),
        "control endpoints must bind to a loopback address, got {control_bind}"
    );

    let handle = MetricsHandle::new_with_watch_interval(staleness_threshold, watch_interval_secs);
    let metrics_app = Router::new()
        .route("/metrics", get(handle_metrics))
        .route("/healthz", get(handle_healthz))
        .with_state(handle.clone());
    let control_app = Router::new()
        .route("/", get(handle_status_page))
        .route("/control/pause", post(handle_pause))
        .route("/control/resume", post(handle_resume))
        .route("/control/sync-now", post(handle_sync_now))
        .route("/events", get(handle_sse_events))
        .route_layer(middleware::from_fn(require_loopback_control))
        .with_state(handle.clone());

    let addr = SocketAddr::new(bind, port);
    let std_listener = std::net::TcpListener::bind(addr)
        .map_err(|e| anyhow::anyhow!("Failed to bind HTTP server on {bind}:{port}: {e}"))?;
    let local_addr = std_listener.local_addr()?;
    std_listener.set_nonblocking(true)?;
    let listener = tokio::net::TcpListener::from_std(std_listener)?;

    let use_single_listener = bind == control_bind || bind.is_unspecified();
    if use_single_listener {
        let app = metrics_app.merge(control_app);
        tracing::info!(
            bind = %local_addr.ip(),
            port = local_addr.port(),
            "HTTP server listening (serving /healthz, /metrics, and loopback-gated control endpoints)"
        );

        let task = tokio::spawn(async move {
            if let Err(e) = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async move { shutdown_token.cancelled().await })
            .await
            {
                tracing::warn!(error = %e, "HTTP server error");
            }
            tracing::info!(port = local_addr.port(), "HTTP server stopped");
        });

        return Ok((handle, task, local_addr));
    }

    let control_addr = SocketAddr::new(control_bind, port);
    let control_std_listener = std::net::TcpListener::bind(control_addr).map_err(|e| {
        anyhow::anyhow!("Failed to bind control HTTP server on {control_bind}:{port}: {e}")
    })?;
    let control_local_addr = control_std_listener.local_addr()?;
    control_std_listener.set_nonblocking(true)?;
    let control_listener = tokio::net::TcpListener::from_std(control_std_listener)?;

    tracing::info!(
        bind = %local_addr.ip(),
        port = local_addr.port(),
        control_bind = %control_local_addr.ip(),
        control_port = control_local_addr.port(),
        "HTTP server listening with separate metrics and control listeners"
    );

    let task = tokio::spawn(async move {
        let http_shutdown = shutdown_token.clone();
        let control_shutdown = shutdown_token;
        let http = axum::serve(
            listener,
            metrics_app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move { http_shutdown.cancelled().await });
        let control = axum::serve(
            control_listener,
            control_app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move { control_shutdown.cancelled().await });

        let (http_result, control_result) = tokio::join!(http, control);
        if let Err(e) = http_result {
            tracing::warn!(error = %e, "HTTP metrics server error");
        }
        if let Err(e) = control_result {
            tracing::warn!(error = %e, "HTTP control server error");
        }
        tracing::info!(
            port = local_addr.port(),
            control_port = control_local_addr.port(),
            "HTTP server stopped"
        );
    });

    Ok((handle, task, local_addr))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;

    fn healthy_status(consecutive_failures: u32) -> HealthStatus {
        let mut h = HealthStatus::new();
        if consecutive_failures == 0 {
            h.record_success();
        } else {
            for i in 0..consecutive_failures {
                h.record_failure(&format!("error {i}"));
            }
        }
        h
    }

    fn stats_with(downloaded: usize, failed: usize, bytes: u64) -> SyncStats {
        SyncStats {
            downloaded,
            failed,
            bytes_downloaded: bytes,
            ..SyncStats::default()
        }
    }

    async fn render_metrics(handle: &MetricsHandle) -> String {
        let response = handle_metrics(State(handle.clone())).await;
        let body = axum::body::to_bytes(
            axum::response::IntoResponse::into_response(response).into_body(),
            usize::MAX,
        )
        .await
        .unwrap();
        String::from_utf8(body.to_vec()).unwrap()
    }

    async fn render_healthz(handle: &MetricsHandle) -> (axum::http::StatusCode, String) {
        let response = axum::response::IntoResponse::into_response(
            handle_healthz(State(handle.clone())).await,
        );
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    async fn render_status_page(handle: &MetricsHandle) -> (axum::http::StatusCode, String) {
        let response = axum::response::IntoResponse::into_response(
            handle_status_page(State(handle.clone())).await,
        );
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    // ── /metrics content-type ─────────────────────────────────────────────────

    #[tokio::test]
    async fn metrics_response_has_openmetrics_content_type() {
        let handle = MetricsHandle::new(None);
        let response =
            axum::response::IntoResponse::into_response(handle_metrics(State(handle)).await);
        let content_type = response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            content_type.contains("application/openmetrics-text"),
            "unexpected content-type: {content_type}"
        );
    }

    // ── counter accumulation ──────────────────────────────────────────────────

    #[tokio::test]
    async fn counters_reflect_single_cycle() {
        let handle = MetricsHandle::new(None);
        let stats = stats_with(5, 2, 1024);
        handle.update(&stats, &healthy_status(0)).await;

        let output = render_metrics(&handle).await;
        assert!(
            output.contains("kei_sync_downloaded_total 5"),
            "output:\n{output}"
        );
        assert!(
            output.contains("kei_sync_failed_total 2"),
            "output:\n{output}"
        );
        assert!(
            output.contains("kei_sync_bytes_downloaded_total 1024"),
            "output:\n{output}"
        );
    }

    #[tokio::test]
    async fn counters_accumulate_across_cycles() {
        let handle = MetricsHandle::new(None);
        handle
            .update(&stats_with(3, 1, 500), &healthy_status(0))
            .await;
        handle
            .update(&stats_with(4, 0, 300), &healthy_status(0))
            .await;

        let output = render_metrics(&handle).await;
        assert!(
            output.contains("kei_sync_downloaded_total 7"),
            "output:\n{output}"
        );
        assert!(
            output.contains("kei_sync_failed_total 1"),
            "output:\n{output}"
        );
        assert!(
            output.contains("kei_sync_bytes_downloaded_total 800"),
            "output:\n{output}"
        );
    }

    #[tokio::test]
    async fn gauges_reflect_only_the_latest_cycle() {
        let handle = MetricsHandle::new(None);
        let stats1 = SyncStats {
            elapsed_secs: 10.0,
            ..Default::default()
        };
        handle.update(&stats1, &healthy_status(0)).await;

        let stats2 = SyncStats {
            elapsed_secs: 25.0,
            ..Default::default()
        };
        handle.update(&stats2, &healthy_status(0)).await;

        let output = render_metrics(&handle).await;
        assert!(
            output.contains("kei_sync_cycle_duration_seconds 25"),
            "output:\n{output}"
        );
        assert!(
            !output.contains("kei_sync_cycle_duration_seconds 10"),
            "old gauge value should not appear:\n{output}"
        );
    }

    // ── update_health_only does not clobber cycle duration ────────────────────

    #[tokio::test]
    async fn cycle_duration_not_clobbered_by_health_only_update() {
        let handle = MetricsHandle::new(None);
        let stats = SyncStats {
            elapsed_secs: 25.0,
            ..Default::default()
        };
        handle.update(&stats, &healthy_status(0)).await;

        // Simulate a skipped cycle: should not reset duration to 0.
        handle.update_health_only(&healthy_status(0)).await;

        let output = render_metrics(&handle).await;
        assert!(
            output.contains("kei_sync_cycle_duration_seconds 25"),
            "cycle_duration_seconds should not be clobbered by update_health_only:\n{output}"
        );
    }

    #[tokio::test]
    async fn health_only_update_still_refreshes_health_gauges() {
        let handle = MetricsHandle::new(None);
        // First real cycle with 3 failures.
        handle
            .update(&SyncStats::default(), &healthy_status(3))
            .await;
        // Skipped cycle that resolves to healthy.
        handle.update_health_only(&healthy_status(0)).await;

        let output = render_metrics(&handle).await;
        assert!(
            output.contains("kei_health_consecutive_failures 0"),
            "health gauge should be updated by update_health_only:\n{output}"
        );
    }

    #[tokio::test]
    async fn health_only_update_refreshes_healthz_snapshot() {
        let handle = MetricsHandle::new(None);
        handle.update_health_only(&healthy_status(0)).await;

        let (status, _body) = render_healthz(&handle).await;
        assert_eq!(
            status,
            axum::http::StatusCode::OK,
            "/healthz should return 200 after update_health_only with no failures"
        );
    }

    // ── interrupted counter ───────────────────────────────────────────────────

    #[tokio::test]
    async fn interrupted_flag_increments_counter() {
        let handle = MetricsHandle::new(None);
        let stats = SyncStats {
            interrupted: true,
            ..Default::default()
        };
        handle.update(&stats, &healthy_status(0)).await;

        let output = render_metrics(&handle).await;
        assert!(
            output.contains("kei_sync_interrupted_cycles_total 1"),
            "output:\n{output}"
        );
    }

    #[tokio::test]
    async fn non_interrupted_cycle_does_not_increment_counter() {
        let handle = MetricsHandle::new(None);
        handle
            .update(&SyncStats::default(), &healthy_status(0))
            .await;

        let output = render_metrics(&handle).await;
        assert!(
            !output.contains("kei_sync_interrupted_cycles_total 1"),
            "output:\n{output}"
        );
    }

    // ── session expiration counter ────────────────────────────────────────────

    #[tokio::test]
    async fn session_expiration_counter_increments() {
        let handle = MetricsHandle::new(None);
        handle.record_session_expiration();
        handle.record_session_expiration();

        let output = render_metrics(&handle).await;
        assert!(
            output.contains("kei_sync_session_expirations_total 2"),
            "output:\n{output}"
        );
    }

    // ── skip breakdown labels ─────────────────────────────────────────────────

    #[tokio::test]
    async fn skip_breakdown_emits_labelled_counters() {
        let handle = MetricsHandle::new(None);
        let mut stats = SyncStats::default();
        stats.skipped.by_state = 10;
        stats.skipped.on_disk = 3;
        stats.skipped.retry_exhausted = 1;
        handle.update(&stats, &healthy_status(0)).await;

        let output = render_metrics(&handle).await;
        assert!(
            output.contains(r#"reason="by_state""#) && output.contains("10"),
            "by_state label missing:\n{output}"
        );
        assert!(
            output.contains(r#"reason="on_disk""#) && output.contains("3"),
            "on_disk label missing:\n{output}"
        );
        assert!(
            output.contains(r#"reason="retry_exhausted""#) && output.contains("1"),
            "retry_exhausted label missing:\n{output}"
        );
    }

    /// The `mark_downloaded_zero_rows` counter is registered into the global
    /// registry by `MetricsHandle::new`, so a /metrics scrape should
    /// surface the series even before the state DB has incremented it.
    /// Pinning the rendered name guards the registration call (which is
    /// easy to drop on a future refactor of `new`).
    #[tokio::test]
    async fn mark_downloaded_zero_rows_counter_is_registered() {
        let handle = MetricsHandle::new(None);
        let output = render_metrics(&handle).await;
        assert!(
            output.contains("kei_state_mark_downloaded_zero_rows"),
            "mark_downloaded_zero_rows series missing from /metrics:\n{output}"
        );
    }

    #[tokio::test]
    async fn zero_skips_do_not_create_label_series() {
        let handle = MetricsHandle::new(None);
        handle
            .update(&SyncStats::default(), &healthy_status(0))
            .await;

        let output = render_metrics(&handle).await;
        assert!(
            !output.contains(r#"reason="by_state""#),
            "zero-count skip series should not appear:\n{output}"
        );
    }

    // ── health gauges ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn consecutive_failures_gauge_tracks_health() {
        let handle = MetricsHandle::new(None);
        handle
            .update(&SyncStats::default(), &healthy_status(3))
            .await;

        let output = render_metrics(&handle).await;
        assert!(
            output.contains("kei_health_consecutive_failures 3"),
            "output:\n{output}"
        );
    }

    #[tokio::test]
    async fn consecutive_failures_gauge_resets_on_success() {
        let handle = MetricsHandle::new(None);
        handle
            .update(&SyncStats::default(), &healthy_status(3))
            .await;
        handle
            .update(&SyncStats::default(), &healthy_status(0))
            .await;

        let output = render_metrics(&handle).await;
        assert!(
            output.contains("kei_health_consecutive_failures 0"),
            "output:\n{output}"
        );
    }

    // ── /healthz endpoint ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn healthz_returns_503_before_first_cycle() {
        let handle = MetricsHandle::new(None);
        let (status, _body) = render_healthz(&handle).await;
        assert_eq!(status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn healthz_returns_200_after_successful_cycle() {
        let handle = MetricsHandle::new(None);
        handle
            .update(&SyncStats::default(), &healthy_status(0))
            .await;

        let (status, body) = render_healthz(&handle).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        serde_json::from_str::<serde_json::Value>(&body)
            .expect("healthz body should be valid JSON");
    }

    #[tokio::test]
    async fn healthz_returns_503_when_consecutive_failures_reaches_threshold() {
        let handle = MetricsHandle::new(None);
        handle
            .update(&SyncStats::default(), &healthy_status(5))
            .await;

        let (status, _body) = render_healthz(&handle).await;
        assert_eq!(status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn healthz_returns_200_when_consecutive_failures_below_threshold() {
        let handle = MetricsHandle::new(None);
        handle
            .update(&SyncStats::default(), &healthy_status(4))
            .await;

        let (status, _body) = render_healthz(&handle).await;
        assert_eq!(status, axum::http::StatusCode::OK);
    }

    #[tokio::test]
    async fn healthz_body_contains_expected_fields() {
        let handle = MetricsHandle::new(None);
        handle
            .update(&SyncStats::default(), &healthy_status(0))
            .await;

        let (_status, body) = render_healthz(&handle).await;
        let json: serde_json::Value =
            serde_json::from_str(&body).expect("healthz body should be valid JSON");
        assert!(
            json.get("consecutive_failures").is_some(),
            "missing consecutive_failures"
        );
        assert!(json.get("last_sync_at").is_some(), "missing last_sync_at");
        assert!(
            json.get("last_success_at").is_some(),
            "missing last_success_at"
        );
    }

    // ── /healthz staleness threshold ───────────────────────────────────────

    async fn set_threshold(handle: &MetricsHandle, max_age: Option<chrono::Duration>) {
        handle.inner.lock().await.staleness_threshold = max_age;
    }

    async fn backdate_last_success(handle: &MetricsHandle, secs_ago: i64) {
        let mut inner = handle.inner.lock().await;
        if let Some(snap) = inner.health_snapshot.as_mut() {
            let past = Utc::now() - chrono::Duration::seconds(secs_ago);
            snap.last_sync_at = Some(past);
            snap.last_success_at = Some(past);
        }
    }

    #[tokio::test]
    async fn healthz_returns_503_when_last_success_is_stale() {
        let handle = MetricsHandle::new(None);
        handle
            .update(&SyncStats::default(), &healthy_status(0))
            .await;
        // 600s threshold; backdate last_success to 1200s ago
        set_threshold(&handle, Some(chrono::Duration::seconds(600))).await;
        backdate_last_success(&handle, 1200).await;

        let (status, _body) = render_healthz(&handle).await;
        assert_eq!(status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn healthz_returns_200_when_last_success_is_fresh() {
        let handle = MetricsHandle::new(None);
        handle
            .update(&SyncStats::default(), &healthy_status(0))
            .await;
        set_threshold(&handle, Some(chrono::Duration::seconds(600))).await;
        backdate_last_success(&handle, 100).await;

        let (status, _body) = render_healthz(&handle).await;
        assert_eq!(status, axum::http::StatusCode::OK);
    }

    #[tokio::test]
    async fn healthz_staleness_disabled_when_threshold_is_none() {
        let handle = MetricsHandle::new(None);
        handle
            .update(&SyncStats::default(), &healthy_status(0))
            .await;
        // No threshold set, last_success 10 years ago — must still be 200
        backdate_last_success(&handle, 10 * 365 * 24 * 3600).await;

        let (status, _body) = render_healthz(&handle).await;
        assert_eq!(status, axum::http::StatusCode::OK);
    }

    #[tokio::test]
    async fn healthz_staleness_tripped_also_when_failures_high() {
        // Both conditions at once -> 503
        let handle = MetricsHandle::new(None);
        handle
            .update(&SyncStats::default(), &healthy_status(5))
            .await;
        set_threshold(&handle, Some(chrono::Duration::seconds(60))).await;
        backdate_last_success(&handle, 120).await;

        let (status, _body) = render_healthz(&handle).await;
        assert_eq!(status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn healthz_staleness_ignored_when_last_success_never_set() {
        // First cycle failed -> no last_success_at. Staleness must not trip
        // because we have no anchor timestamp yet; failures alone drive 503.
        let handle = MetricsHandle::new(None);
        let mut h = HealthStatus::new();
        h.record_failure("first ever");
        handle.update(&SyncStats::default(), &h).await;
        set_threshold(&handle, Some(chrono::Duration::seconds(1))).await;

        let (status, _body) = render_healthz(&handle).await;
        // 1 consecutive failure is below 5, and last_success is None, so 200
        assert_eq!(status, axum::http::StatusCode::OK);
    }

    // ── status page and control endpoints ────────────────────────────────────

    #[tokio::test]
    async fn status_page_contains_version_and_service_state() {
        let handle = MetricsHandle::new_with_watch_interval(None, Some(900));
        handle
            .update_status_snapshot("success", Some(&stats_with(2, 0, 2048)), None)
            .await;

        let (status, body) = render_status_page(&handle).await;

        assert_eq!(status, axum::http::StatusCode::OK);
        assert!(
            body.contains("kei"),
            "status page should name the app:\n{body}"
        );
        assert!(
            body.contains(env!("CARGO_PKG_VERSION")),
            "status page should include the package version:\n{body}"
        );
        assert!(
            body.contains("running"),
            "status page should show service state:\n{body}"
        );
        assert!(
            body.contains("900s"),
            "status page should show watch interval:\n{body}"
        );
    }

    #[tokio::test]
    async fn pause_and_resume_handlers_toggle_pause_flag() {
        let handle = MetricsHandle::new(None);

        let _ = handle_pause(State(handle.clone())).await;
        assert!(handle.is_paused(), "pause handler should set pause flag");

        let _ = handle_resume(State(handle.clone())).await;
        assert!(
            !handle.is_paused(),
            "resume handler should clear pause flag"
        );
    }

    #[tokio::test]
    async fn sync_now_handler_notifies_waiter() {
        let handle = MetricsHandle::new(None);
        let waiter = {
            let handle = handle.clone();
            tokio::spawn(async move {
                handle.wait_for_control_signal().await;
            })
        };

        let _ = handle_sync_now(State(handle)).await;

        tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .expect("sync-now should notify the waiter")
            .expect("waiter task should not panic");
    }

    #[tokio::test]
    async fn event_broadcast_reaches_subscribers() {
        let handle = MetricsHandle::new(None);
        let mut rx = handle.event_broadcast.subscribe();

        handle.publish_sync_started(7);

        let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("subscriber should receive event")
            .expect("broadcast channel should stay open");
        assert_eq!(event.kind, "sync_started");
        assert_eq!(event.data["cycle"], 7);
    }

    #[tokio::test]
    async fn control_guard_rejects_requests_without_connect_info() {
        let handle = MetricsHandle::new(None);
        let app = Router::new()
            .route("/control/pause", post(handle_pause))
            .route_layer(middleware::from_fn(require_loopback_control))
            .with_state(handle.clone());
        let token = CancellationToken::new();
        let std_listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = std_listener.local_addr().unwrap();
        std_listener.set_nonblocking(true).unwrap();
        let listener = tokio::net::TcpListener::from_std(std_listener).unwrap();
        let task = tokio::spawn({
            let token = token.clone();
            async move {
                axum::serve(listener, app.into_make_service())
                    .with_graceful_shutdown(async move { token.cancelled().await })
                    .await
                    .unwrap();
            }
        });

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap();
        let response = client
            .post(format!("http://127.0.0.1:{}/control/pause", addr.port()))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);
        assert!(!handle.is_paused(), "rejected request must not pause sync");
        token.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;
    }

    #[test]
    fn spawn_server_rejects_non_loopback_control_bind() {
        let token = CancellationToken::new();
        let result = spawn_server(
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            std::net::IpAddr::V4(std::net::Ipv4Addr::new(0, 0, 0, 0)),
            0,
            token,
            None,
            Some(900),
        );

        let err = result.expect_err("non-loopback control bind should fail");
        assert!(
            err.to_string().contains("loopback"),
            "error should explain loopback restriction: {err}"
        );
    }

    // ── DB-backed gauges ──────────────────────────────────────────────────────

    fn make_summary(
        downloaded: u64,
        pending: u64,
        failed: u64,
        downloaded_bytes: u64,
    ) -> SyncSummary {
        SyncSummary {
            total_assets: downloaded + pending + failed,
            downloaded,
            pending,
            failed,
            downloaded_bytes,
            last_sync_completed: None,
            last_sync_started: None,
        }
    }

    #[tokio::test]
    async fn update_db_stats_sets_asset_count_gauges() {
        let handle = MetricsHandle::new(None);
        let summary = make_summary(100, 5, 2, 1_000_000);
        handle.update_db_stats(&summary, 107);

        let output = render_metrics(&handle).await;
        assert!(
            output.contains(r#"kei_db_assets_total{status="downloaded"} 100"#),
            "downloaded count missing or wrong:\n{output}"
        );
        assert!(
            output.contains(r#"kei_db_assets_total{status="pending"} 5"#),
            "pending count missing or wrong:\n{output}"
        );
        assert!(
            output.contains(r#"kei_db_assets_total{status="failed"} 2"#),
            "failed count missing or wrong:\n{output}"
        );
    }

    #[tokio::test]
    async fn update_db_stats_sets_size_bytes_gauge() {
        let handle = MetricsHandle::new(None);
        let summary = make_summary(50, 0, 0, 2_048_000);
        handle.update_db_stats(&summary, 50);

        let output = render_metrics(&handle).await;
        assert!(
            output.contains(r#"kei_db_assets_size_bytes{status="downloaded"} 2048000"#),
            "downloaded_bytes gauge missing or wrong:\n{output}"
        );
    }

    #[tokio::test]
    async fn update_db_stats_sets_last_sync_assets_seen_gauge() {
        let handle = MetricsHandle::new(None);
        let summary = make_summary(28_000, 3_000, 71, 0);
        handle.update_db_stats(&summary, 31_071);

        let output = render_metrics(&handle).await;
        assert!(
            output.contains("kei_db_last_sync_assets_seen 31071"),
            "last_sync_assets_seen gauge missing or wrong:\n{output}"
        );
    }

    #[tokio::test]
    async fn update_db_stats_gauges_reflect_latest_call() {
        let handle = MetricsHandle::new(None);
        handle.update_db_stats(&make_summary(100, 10, 0, 500_000), 110);
        // Second call should overwrite (gauges, not counters).
        handle.update_db_stats(&make_summary(105, 5, 0, 525_000), 110);

        let output = render_metrics(&handle).await;
        assert!(
            !output.contains("kei_db_assets_total{status=\"pending\"} 10"),
            "stale pending value should not appear:\n{output}"
        );
        assert!(
            output.contains("kei_db_last_sync_assets_seen 110"),
            "assets_seen gauge wrong:\n{output}"
        );
    }

    #[tokio::test]
    async fn db_metric_names_present_in_output() {
        let handle = MetricsHandle::new(None);
        handle.update_db_stats(&make_summary(1, 0, 0, 1024), 1);

        let output = render_metrics(&handle).await;
        assert!(
            output.contains("kei_db_assets_total"),
            "kei_db_assets_total missing from /metrics output:\n{output}"
        );
        assert!(
            output.contains("kei_db_assets_size_bytes"),
            "kei_db_assets_size_bytes missing from /metrics output:\n{output}"
        );
        assert!(
            output.contains("kei_db_last_sync_assets_seen"),
            "kei_db_last_sync_assets_seen missing from /metrics output:\n{output}"
        );
    }

    #[tokio::test]
    async fn db_summary_failure_counter_increments() {
        let handle = MetricsHandle::new(None);
        handle.record_db_summary_failure();
        handle.record_db_summary_failure();

        let output = render_metrics(&handle).await;
        assert!(
            output.contains("kei_db_summary_read_failures_total 2"),
            "db_summary_read_failures counter missing or wrong:\n{output}"
        );
    }

    /// Regression test for https://github.com/rhoopr/kei/issues/248
    ///
    /// `spawn_server()` must not panic when called from within a tokio runtime
    /// with a `staleness_threshold` set. Before the fix, `blocking_lock()` was
    /// used inside the tokio runtime, which panics unconditionally regardless
    /// of lock contention.
    #[tokio::test]
    async fn spawn_server_with_staleness_threshold_does_not_panic_inside_runtime() {
        let token = CancellationToken::new();
        // Port 0 lets the OS pick a free ephemeral port.
        let result = spawn_server(
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            0,
            token.clone(),
            Some(chrono::Duration::seconds(3600)),
            Some(1800),
        );
        assert!(
            result.is_ok(),
            "spawn_server panicked or errored inside tokio runtime: {:?}",
            result.err()
        );
        token.cancel();
    }

    /// Full end-to-end smoke test: the real axum stack, a real TCP socket, and a
    /// real HTTP client. Catches regressions anywhere between `spawn_server` and
    /// the response body that the handler unit tests (which call `handle_*`
    /// directly against a `State`) cannot see.
    #[tokio::test]
    async fn spawn_server_serves_metrics_and_healthz_over_http() {
        let token = CancellationToken::new();
        let (handle, task, addr) = spawn_server(
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            0,
            token.clone(),
            Some(chrono::Duration::seconds(3600)),
            Some(1800),
        )
        .expect("spawn_server should bind and spawn");

        // Push a healthy cycle so /healthz returns 200 instead of the pre-first-cycle 503.
        handle
            .update(&SyncStats::default(), &healthy_status(0))
            .await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap();

        // Test binds loopback explicitly; dial the same. (Prod binds 0.0.0.0
        // by default, but keeping the test on loopback sidesteps Windows'
        // AddrNotAvailable-on-connect behavior for 0.0.0.0.)
        let base = format!("http://127.0.0.1:{}", addr.port());

        let metrics_resp = client
            .get(format!("{base}/metrics"))
            .send()
            .await
            .expect("GET /metrics should succeed");
        assert_eq!(metrics_resp.status(), reqwest::StatusCode::OK);
        let ct = metrics_resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        assert!(
            ct.contains("application/openmetrics-text"),
            "unexpected content-type: {ct}"
        );
        let body = metrics_resp.text().await.unwrap();
        assert!(
            body.contains("kei_sync_assets_seen_total"),
            "expected kei_sync_* counters in body:\n{body}"
        );
        assert!(
            body.contains("kei_state_mark_downloaded_zero_rows"),
            "expected mark_downloaded_zero_rows series in body:\n{body}"
        );

        let healthz_resp = client
            .get(format!("{base}/healthz"))
            .send()
            .await
            .expect("GET /healthz should succeed");
        assert_eq!(healthz_resp.status(), reqwest::StatusCode::OK);

        let status_resp = client
            .get(format!("{base}/"))
            .send()
            .await
            .expect("GET / should succeed");
        assert_eq!(status_resp.status(), reqwest::StatusCode::OK);
        let status_body = status_resp.text().await.unwrap();
        assert!(
            status_body.contains("kei"),
            "status page should contain app name:\n{status_body}"
        );

        let pause_resp = client
            .post(format!("{base}/control/pause"))
            .send()
            .await
            .expect("POST /control/pause should succeed");
        assert_eq!(pause_resp.status(), reqwest::StatusCode::OK);
        assert!(handle.is_paused(), "HTTP pause endpoint should set flag");

        let resume_resp = client
            .post(format!("{base}/control/resume"))
            .send()
            .await
            .expect("POST /control/resume should succeed");
        assert_eq!(resume_resp.status(), reqwest::StatusCode::OK);
        assert!(
            !handle.is_paused(),
            "HTTP resume endpoint should clear flag"
        );

        token.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;
    }

    // ── disk free gauge ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn disk_free_gauge_updates_and_renders() {
        let handle = MetricsHandle::new(None);
        handle.update_disk_free(42_000_000);

        let output = render_metrics(&handle).await;
        assert!(
            output.contains("kei_disk_free_bytes 42000000"),
            "disk_free gauge missing or wrong:\n{output}"
        );
    }
}
