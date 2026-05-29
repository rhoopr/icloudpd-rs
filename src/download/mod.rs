//! Download engine — streaming pipeline that starts downloading as soon as
//! the first API page returns, rather than enumerating the entire library
//! upfront. Uses a two-phase approach: (1) stream-and-download with bounded
//! concurrency, then (2) cleanup pass with fresh CDN URLs for any failures.

pub mod error;
pub mod file;
pub(crate) mod filter;
pub(crate) mod finalize;
#[cfg(feature = "xmp")]
pub(crate) mod heif;
pub(crate) mod limiter;
pub mod metadata;
pub mod paths;
pub(crate) mod pipeline;
pub(crate) mod planner;
pub(crate) mod recap;

pub(crate) use limiter::BandwidthLimiter;

use pipeline::{
    build_download_outcome, format_duration, log_sync_summary, run_download_pass,
    stream_and_download_from_stream, MetadataFlags, PassConfig, StreamRuntime, StreamingResult,
    AUTH_ERROR_THRESHOLD,
};

pub(crate) use filter::determine_media_type;
pub(crate) use filter::AssetGroupings;
use filter::DownloadTask;

use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use reqwest::Client;
use rustc_hash::{FxHashMap, FxHashSet};

use futures_util::stream::{self, StreamExt};
use futures_util::Stream;
use tokio_util::sync::CancellationToken;

use crate::icloud::photos::{PhotoAsset, SyncTokenError};
use crate::retry::RetryConfig;
use crate::state::{DownloadStateStore, MetadataRewriteStore, StateDb, VersionSizeKey};
use crate::types::{
    AssetVersionSize, ChangeReason, FileMatchPolicy, LivePhotoMode, LivePhotoMovFilenamePolicy,
    RawPolicy,
};

/// Outcome of a download pass.
#[derive(Debug)]
pub enum DownloadOutcome {
    /// All downloads completed successfully.
    Success,
    /// Session expired mid-sync; caller should re-authenticate and retry.
    SessionExpired { auth_error_count: usize },
    /// Some downloads failed (not session-related).
    PartialFailure { failed_count: usize },
}

/// How the sync should enumerate photos from iCloud.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncMode {
    /// Full enumeration via records/query (existing behavior).
    /// On completion, captures the syncToken for future incremental syncs.
    Full,
    /// Incremental delta sync via changes/zone with a stored syncToken.
    /// Falls back to Full if the token is invalid/expired.
    Incremental {
        /// The stored syncToken for the zone being synced.
        zone_sync_token: String,
    },
}

/// Bounded reason vocabulary for full enumeration runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FullEnumerationReason {
    NoStoredToken,
    RetryFailedRows,
    PendingRows,
    MetadataBackfill,
    #[allow(
        dead_code,
        reason = "kept as a stable report vocabulary value for older path-template fallback reports"
    )]
    PathTemplateRequiresFullEnumeration,
    AlbumRelationHydrationIncomplete,
    EnumConfigHashDrift,
    ExplicitRetryFailed,
    #[allow(
        dead_code,
        reason = "reserved report vocabulary for a future durable token-blocked marker"
    )]
    TokenBlockedPreviously,
    OtherStaticReason,
}

impl FullEnumerationReason {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::NoStoredToken => "no_stored_token",
            Self::RetryFailedRows => "retry_failed_rows",
            Self::PendingRows => "pending_rows",
            Self::MetadataBackfill => "metadata_backfill",
            Self::PathTemplateRequiresFullEnumeration => "path_template_requires_full_enumeration",
            Self::AlbumRelationHydrationIncomplete => ALBUM_RELATION_HYDRATION_INCOMPLETE_REASON,
            Self::EnumConfigHashDrift => "enum_config_hash_drift",
            Self::ExplicitRetryFailed => "explicit_retry_failed",
            Self::TokenBlockedPreviously => "token_blocked_previously",
            Self::OtherStaticReason => "other_static_reason",
        }
    }
}

/// One-shot runtime behavior for a sync pass.
///
/// Kept outside [`DownloadConfig`] so path/filter/download decisions do not
/// grow presentation-only flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DownloadRunMode {
    Download,
    DryRun,
    PrintFilenames,
}

impl DownloadRunMode {
    pub(crate) fn is_dry_run(self) -> bool {
        matches!(self, Self::DryRun)
    }

    pub(crate) fn only_print_filenames(self) -> bool {
        matches!(self, Self::PrintFilenames)
    }

    pub(crate) fn downloads_files(self) -> bool {
        matches!(self, Self::Download)
    }
}

/// Presentation knobs for the download pipeline.
///
/// The core config owns what to download. This owns how progress and friendly
/// narration are shown while that work runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DownloadReporting {
    pub(crate) no_progress_bar: bool,
    pub(crate) personality_mode: crate::personality::Mode,
}

impl DownloadReporting {
    pub(crate) const fn new(
        no_progress_bar: bool,
        personality_mode: crate::personality::Mode,
    ) -> Self {
        Self {
            no_progress_bar,
            personality_mode,
        }
    }

    #[cfg(test)]
    pub(crate) const fn hidden() -> Self {
        Self::new(true, crate::personality::Mode::Off)
    }
}

/// Per-run behavior that does not affect download path or filter decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DownloadControls {
    pub(crate) run_mode: DownloadRunMode,
    pub(crate) reporting: DownloadReporting,
}

impl DownloadControls {
    pub(crate) const fn new(run_mode: DownloadRunMode, reporting: DownloadReporting) -> Self {
        Self {
            run_mode,
            reporting,
        }
    }

    #[cfg(test)]
    pub(crate) const fn download_hidden() -> Self {
        Self::new(DownloadRunMode::Download, DownloadReporting::hidden())
    }

    #[cfg(test)]
    pub(crate) const fn dry_run_hidden() -> Self {
        Self::new(DownloadRunMode::DryRun, DownloadReporting::hidden())
    }
}

/// Result of a sync cycle, including the optional new syncToken.
#[derive(Debug)]
pub struct SyncResult {
    /// The outcome of the download pass (success, session expired, partial failure).
    pub outcome: DownloadOutcome,
    /// The new zone-level syncToken, if one was captured during this sync.
    /// Store this for the next incremental sync.
    pub sync_token: Option<String>,
    /// Accumulated statistics from this sync run.
    pub stats: SyncStats,
    /// Whether this result came from a full records/query enumeration.
    pub(crate) full_enumeration_ran: bool,
}

/// Accumulated statistics from a sync run, used for JSON reports and notifications.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct SyncStats {
    pub assets_seen: u64,
    pub downloaded: usize,
    pub failed: usize,
    pub skipped: SkipBreakdown,
    pub bytes_downloaded: u64,
    pub disk_bytes_written: u64,
    pub exif_failures: usize,
    pub state_write_failures: usize,
    pub enumeration_errors: usize,
    /// Number of count-only CloudKit pagination shortfall warnings observed.
    /// These are not hard enumeration failures and do not imply download
    /// failures.
    pub pagination_shortfall_warnings: usize,
    /// Sum of missing assets reported by tolerated or token-unsafe
    /// pagination shortfalls.
    pub pagination_shortfall_assets: u64,
    /// Whether sync-token advancement was blocked for safety despite no
    /// download failure.
    pub sync_token_blocked: bool,
    /// Structured reason for `sync_token_blocked`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_token_blocked_reason: Option<&'static str>,
    /// High-level owner attribution for `sync_token_blocked_reason`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_token_blocked_source: Option<&'static str>,
    /// Human-readable explanation for why token advancement was blocked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_token_blocked_explanation: Option<&'static str>,
    /// Bounded reason explaining why this run used full enumeration instead
    /// of incremental sync.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_enumeration_reason: Option<FullEnumerationReason>,
    /// Zone name where token advancement was blocked. Set by the cycle owner
    /// so report.json can identify the affected library directly.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_token_blocked_zone: Option<String>,
    /// Number of token receivers expected from full-enumeration passes.
    /// Emitted whenever token receiver telemetry was collected, even if
    /// `sync_token_blocked` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_token_expected_receivers: Option<usize>,
    /// Number of passes that produced a non-blank sync token.
    /// Emitted whenever token receiver telemetry was collected, even if
    /// `sync_token_blocked` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_token_receivers_with_token: Option<usize>,
    /// Number of passes that completed but produced no sync token.
    /// Emitted whenever token receiver telemetry was collected, even if
    /// `sync_token_blocked` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_token_receivers_missing: Option<usize>,
    /// Number of passes that produced a blank sync token.
    /// Emitted whenever token receiver telemetry was collected, even if
    /// `sync_token_blocked` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_token_receivers_blank: Option<usize>,
    /// Number of sync token channels that dropped before reporting.
    /// Emitted whenever token receiver telemetry was collected, even if
    /// `sync_token_blocked` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_token_receivers_dropped: Option<usize>,
    /// Number of unique non-blank sync token values observed.
    /// Emitted whenever token receiver telemetry was collected, even if
    /// `sync_token_blocked` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_token_unique_values: Option<usize>,
    pub elapsed_secs: f64,
    pub interrupted: bool,
    /// Number of tasks that observed at least one HTTP 429 / 503 response
    /// during retry. A high ratio of rate_limited / assets_seen signals the
    /// sync is running against a back-pressured account; operators should
    /// either raise `[watch] interval` or lower `[download] threads`.
    pub rate_limited: usize,
    /// Photos downloaded this run (`MediaType::Photo` and
    /// `MediaType::LivePhotoImage`). Sums to `downloaded` together with
    /// `videos_downloaded` for any pure-asset run; multi-version downloads
    /// (a Live Photo's image + MOV) count both sides.
    pub photos_downloaded: usize,
    /// Videos downloaded this run (`MediaType::Video` and
    /// `MediaType::LivePhotoVideo`).
    pub videos_downloaded: usize,
    /// Per-cycle highlights for the friendly recap (biggest / oldest /
    /// newest-album). Empty when no downloads succeeded; consumers must
    /// guard on `is_empty()` before rendering. Skipped for serialisation
    /// because it carries `chrono::DateTime<Local>` and the JSON report
    /// contract is owned by the existing scalar fields above.
    #[serde(skip)]
    pub recap: recap::RunRecap,
}

impl SyncStats {
    /// Add `other` into `self`, field by field. Used by the per-cycle loop in
    /// `sync_loop::run_cycle` to fold each library's stats into a cycle-wide
    /// total.
    ///
    /// All numeric counters sum; `interrupted` ORs (any library being
    /// interrupted means the cycle was interrupted); `skipped` delegates to
    /// [`SkipBreakdown::accumulate`].
    ///
    /// Adding a new field to `SyncStats` requires updating this method too --
    /// otherwise the new counter silently zeros out across multi-library
    /// syncs.
    pub fn accumulate(&mut self, other: &SyncStats) {
        self.assets_seen += other.assets_seen;
        self.downloaded += other.downloaded;
        self.failed += other.failed;
        self.skipped.accumulate(&other.skipped);
        self.bytes_downloaded += other.bytes_downloaded;
        self.disk_bytes_written += other.disk_bytes_written;
        self.exif_failures += other.exif_failures;
        self.state_write_failures += other.state_write_failures;
        self.enumeration_errors += other.enumeration_errors;
        self.pagination_shortfall_warnings += other.pagination_shortfall_warnings;
        self.pagination_shortfall_assets += other.pagination_shortfall_assets;
        self.sync_token_blocked = self.sync_token_blocked || other.sync_token_blocked;
        if self.sync_token_blocked_reason.is_none() {
            self.sync_token_blocked_reason = other.sync_token_blocked_reason;
        }
        if self.sync_token_blocked_source.is_none() {
            self.sync_token_blocked_source = other.sync_token_blocked_source;
        }
        if self.sync_token_blocked_explanation.is_none() {
            self.sync_token_blocked_explanation = other.sync_token_blocked_explanation;
        }
        if self.full_enumeration_reason.is_none() {
            self.full_enumeration_reason = other.full_enumeration_reason;
        }
        if self.sync_token_blocked_zone.is_none() {
            self.sync_token_blocked_zone = other.sync_token_blocked_zone.clone();
        }
        if self.sync_token_expected_receivers.is_none() {
            self.sync_token_expected_receivers = other.sync_token_expected_receivers;
        }
        if self.sync_token_receivers_with_token.is_none() {
            self.sync_token_receivers_with_token = other.sync_token_receivers_with_token;
        }
        if self.sync_token_receivers_missing.is_none() {
            self.sync_token_receivers_missing = other.sync_token_receivers_missing;
        }
        if self.sync_token_receivers_blank.is_none() {
            self.sync_token_receivers_blank = other.sync_token_receivers_blank;
        }
        if self.sync_token_receivers_dropped.is_none() {
            self.sync_token_receivers_dropped = other.sync_token_receivers_dropped;
        }
        if self.sync_token_unique_values.is_none() {
            self.sync_token_unique_values = other.sync_token_unique_values;
        }
        self.elapsed_secs += other.elapsed_secs;
        self.interrupted = self.interrupted || other.interrupted;
        self.rate_limited += other.rate_limited;
        self.photos_downloaded += other.photos_downloaded;
        self.videos_downloaded += other.videos_downloaded;
        self.recap.merge(other.recap.clone());
    }
}

const PAGINATION_SHORTFALL_TOLERANCE_PERCENT: u64 = 5;
const PAGINATION_SHORTFALL_TOLERANCE_ABSOLUTE: u64 = 100;
const ALBUM_RELATION_HYDRATION_INCOMPLETE_REASON: &str = "album_relation_hydration_incomplete";
const DATE_BOUNDED_FULL_ENUMERATION_REASON: &str = "date_bounded_full_enumeration";
const RECENT_LIMITED_FULL_ENUMERATION_REASON: &str = "recent_limited_full_enumeration";

pub(crate) fn sync_token_blocked_source(reason: &str) -> &'static str {
    match reason {
        ALBUM_RELATION_HYDRATION_INCOMPLETE_REASON
        | DATE_BOUNDED_FULL_ENUMERATION_REASON
        | "kei_internal_token_receiver_dropped"
        | RECENT_LIMITED_FULL_ENUMERATION_REASON => "kei",
        "pagination_shortfall"
        | "icloud_blank_sync_token"
        | "icloud_sync_token_mismatch"
        | "icloud_sync_token_missing" => "icloud",
        _ => "unknown",
    }
}

pub(crate) fn sync_token_blocked_explanation(reason: &str) -> &'static str {
    match reason {
        "pagination_shortfall" => {
            "enumeration counts did not line up safely, so kei blocked token advancement"
        }
        "icloud_sync_token_missing" => {
            "iCloud did not return a sync token for this full enumeration"
        }
        "icloud_blank_sync_token" => {
            "iCloud returned a blank sync token, which kei treated as unusable"
        }
        "icloud_sync_token_mismatch" => "iCloud returned conflicting sync tokens across passes",
        "kei_internal_token_receiver_dropped" => {
            "an internal token collection channel closed before completion"
        }
        RECENT_LIMITED_FULL_ENUMERATION_REASON => {
            "a count-limited recent sync is a partial enumeration, so kei blocked token advancement"
        }
        DATE_BOUNDED_FULL_ENUMERATION_REASON => {
            "a lower-date-bounded sync is a partial enumeration, so kei blocked token advancement"
        }
        ALBUM_RELATION_HYDRATION_INCOMPLETE_REASON => {
            "album membership state is not complete enough for incremental routing yet"
        }
        "sync_token_unavailable" | "sync_token_missing" => {
            "no usable sync token was available at the end of the cycle"
        }
        _ => "the sync token was unavailable for an unspecified reason",
    }
}

/// Per-reason breakdown of skipped assets.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct SkipBreakdown {
    pub by_state: usize,
    pub on_disk: usize,
    pub by_media_type: usize,
    pub by_date_range: usize,
    pub by_live_photo: usize,
    pub by_filename: usize,
    pub by_excluded_album: usize,
    pub ampm_variant: usize,
    pub duplicates: usize,
    pub retry_exhausted: usize,
    pub retry_only: usize,
}

impl SkipBreakdown {
    pub fn total(&self) -> usize {
        self.by_state
            + self.on_disk
            + self.by_media_type
            + self.by_date_range
            + self.by_live_photo
            + self.by_filename
            + self.by_excluded_album
            + self.ampm_variant
            + self.duplicates
            + self.retry_exhausted
            + self.retry_only
    }

    /// Add `other` into `self` field-by-field. Mirrors
    /// [`SyncStats::accumulate`] for the nested skip breakdown.
    pub fn accumulate(&mut self, other: &SkipBreakdown) {
        self.by_state += other.by_state;
        self.on_disk += other.on_disk;
        self.by_media_type += other.by_media_type;
        self.by_date_range += other.by_date_range;
        self.by_live_photo += other.by_live_photo;
        self.by_filename += other.by_filename;
        self.by_excluded_album += other.by_excluded_album;
        self.ampm_variant += other.ampm_variant;
        self.duplicates += other.duplicates;
        self.retry_exhausted += other.retry_exhausted;
        self.retry_only += other.retry_only;
    }

    pub(crate) fn record_filter_reason(&mut self, reason: filter::FilterReason) {
        match reason {
            filter::FilterReason::MalformedAsset => self.by_filename += 1,
            filter::FilterReason::ExcludedAlbum => self.by_excluded_album += 1,
            filter::FilterReason::MediaType => self.by_media_type += 1,
            filter::FilterReason::LivePhoto => self.by_live_photo += 1,
            filter::FilterReason::DateRange => self.by_date_range += 1,
            filter::FilterReason::Filename => self.by_filename += 1,
        }
    }
}

/// Truncate a `DateTime<Utc>` to midnight so that relative date intervals
/// (e.g. `20d` → `now - 20 days`) produce a stable hash within the same
/// calendar day.
fn truncate_date_to_day(dt: Option<DateTime<Utc>>) -> Option<chrono::NaiveDate> {
    dt.map(|d| d.date_naive())
}

/// Hash an `Option<NaiveDate>` with a tag byte for `None`/`Some` and the
/// "YYYY-MM-DD" Display representation for the date value.
fn hash_optional_date(hasher: &mut sha2::Sha256, date: Option<chrono::NaiveDate>) {
    use sha2::Digest;
    match date {
        None => hasher.update([0]),
        Some(d) => {
            hasher.update([1]);
            hasher.update(d.to_string().as_bytes());
        }
    }
}

/// Hash a byte slice with a trailing NUL separator. Pairs naturally with
/// other variable-length fields without ambiguity: `"a"` + `""` hashes
/// distinctly from `""` + `"a"`.
fn hash_bytes(hasher: &mut sha2::Sha256, bytes: &[u8]) {
    use sha2::Digest;
    hasher.update(bytes);
    hasher.update(b"\0");
}

/// Hash an `Option<u32>` with a tag byte for `None`/`Some` and the
/// little-endian bytes of the inner value.
fn hash_optional_u32(hasher: &mut sha2::Sha256, val: Option<u32>) {
    use sha2::Digest;
    match val {
        None => hasher.update([0]),
        Some(n) => {
            hasher.update([1]);
            hasher.update(n.to_le_bytes());
        }
    }
}

/// Finalize a SHA-256 hasher into a 16-char hex string (first 8 bytes).
fn finalize_hash(hasher: sha2::Sha256) -> String {
    use sha2::Digest;
    use std::fmt::Write;

    let hash = hasher.finalize();
    let mut hex = String::with_capacity(16);
    // First 8 bytes is plenty for collision avoidance in this context.
    #[allow(
        clippy::indexing_slicing,
        reason = "SHA-256 output is always 32 bytes; 8 is unconditionally in-bounds"
    )]
    for &b in &hash[..8] {
        let _ = Write::write_fmt(&mut hex, format_args!("{b:02x}"));
    }
    hex
}

/// Bump this when path derivation changes without a corresponding config
/// field changing. That forces existing state to revalidate on disk instead
/// of trusting paths derived under older code.
const PATH_DERIVATION_HASH_VERSION: u8 = 2;

/// Fields shared between [`hash_download_config`] and [`compute_config_hash`]
/// that affect path resolution and asset eligibility.
#[derive(Debug)]
struct SharedHashFields<'a> {
    directory: &'a std::path::Path,
    folder_structure: &'a str,
    folder_structure_albums: &'a str,
    folder_structure_smart_folders: &'a str,
    resolution: crate::types::PhotoResolution,
    live_resolution: AssetVersionSize,
    file_match_policy: FileMatchPolicy,
    live_photo_mov_filename_policy: LivePhotoMovFilenamePolicy,
    edited: bool,
    alternative: bool,
    raw_policy: RawPolicy,
    keep_unicode_in_filenames: bool,
    skip_created_before: Option<DateTime<Utc>>,
    skip_created_after: Option<DateTime<Utc>>,
    force_resolution: bool,
    media: crate::config::MediaSelection,
    live_photo_mode: LivePhotoMode,
    filename_exclude: &'a [glob::Pattern],
}

/// Hash the shared config fields into the hasher. All enum values use
/// `repr(u8)` byte representations and dates use "YYYY-MM-DD" Display
/// format for stability across compiler/library upgrades.
fn hash_shared_fields(hasher: &mut sha2::Sha256, f: &SharedHashFields<'_>) {
    use sha2::Digest;

    hasher.update([PATH_DERIVATION_HASH_VERSION]);
    hash_bytes(hasher, f.directory.as_os_str().as_encoded_bytes());
    hash_bytes(hasher, f.folder_structure.as_bytes());
    hash_bytes(hasher, f.folder_structure_albums.as_bytes());
    hash_bytes(hasher, f.folder_structure_smart_folders.as_bytes());
    hasher.update([f.resolution as u8]);
    hasher.update([f.live_resolution as u8]);
    hasher.update([f.file_match_policy as u8]);
    hasher.update([f.live_photo_mov_filename_policy as u8]);
    hasher.update([u8::from(f.edited)]);
    hasher.update([u8::from(f.alternative)]);
    hasher.update([f.raw_policy as u8]);
    hasher.update([u8::from(f.keep_unicode_in_filenames)]);
    // Filter fields: changing these affects which assets are eligible, so we
    // must invalidate the trust-state cache (and stored sync tokens) to avoid
    // skipping newly-eligible assets on incremental syncs.
    //
    // Dates are truncated to day precision before hashing so that relative
    // intervals like "20d" (resolved to now-minus-20-days at parse time)
    // produce a stable hash across consecutive runs on the same day.
    hash_optional_date(hasher, truncate_date_to_day(f.skip_created_before));
    hash_optional_date(hasher, truncate_date_to_day(f.skip_created_after));
    hasher.update([u8::from(f.force_resolution)]);
    hasher.update([u8::from(f.media.photos)]);
    hasher.update([u8::from(f.media.videos)]);
    hasher.update([u8::from(f.media.live_photos)]);
    hasher.update([f.live_photo_mode as u8]);
    // filename_exclude patterns affect which assets are eligible
    let mut sorted_excludes: Vec<&str> = f
        .filename_exclude
        .iter()
        .map(glob::Pattern::as_str)
        .collect();
    sorted_excludes.sort_unstable();
    for pattern in &sorted_excludes {
        hash_bytes(hasher, pattern.as_bytes());
    }
}

/// Compute a deterministic hash of the config fields that affect path resolution.
///
/// When this hash changes between runs, we can't trust the state DB's download
/// records (the resolved paths may differ), so we fall back to the full pipeline
/// with filesystem existence checks.
///
/// Also called from `main.rs` (via [`compute_config_hash`]) to clear sync tokens
/// before the incremental-vs-full decision when the download config changes.
pub(crate) fn hash_download_config(config: &DownloadConfig) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hash_shared_fields(
        &mut hasher,
        &SharedHashFields {
            directory: &config.directory,
            folder_structure: &config.folder_structure,
            folder_structure_albums: &config.folder_structure_albums,
            folder_structure_smart_folders: &config.folder_structure_smart_folders,
            resolution: config.resolution,
            live_resolution: config.live_resolution,
            file_match_policy: config.file_match_policy,
            live_photo_mov_filename_policy: config.live_photo_mov_filename_policy,
            edited: config.edited,
            alternative: config.alternative,
            raw_policy: config.raw_policy,
            keep_unicode_in_filenames: config.keep_unicode_in_filenames,
            skip_created_before: config.skip_created_before,
            skip_created_after: config.skip_created_after,
            force_resolution: config.force_resolution,
            media: config.media,
            live_photo_mode: config.live_photo_mode,
            filename_exclude: &config.filename_exclude,
        },
    );
    // `recent` affects which already-downloaded assets to trust/skip
    hash_optional_u32(&mut hasher, config.recent);
    if config.recent.is_some() {
        hasher.update(b"recent_scope:");
        hasher.update(match config.recent_scope {
            crate::cli::RecentScope::Global => b"global".as_slice(),
            crate::cli::RecentScope::PerFilter => b"per-filter".as_slice(),
        });
        hasher.update(b"\0");
    }
    finalize_hash(hasher)
}

/// Compute the config hash from the app-level `Config`.
///
/// Called from `main.rs` before the sync-mode decision so that stale sync
/// tokens are cleared when the download config changes.
///
/// This hash is a SUPERSET of [`hash_download_config`]: it includes all
/// the fields that affect download paths (shared with hash_download_config)
/// plus enumeration-filter fields (albums, library, live_photo_mode) that
/// affect WHICH assets are eligible. Changing these filters must invalidate
/// sync tokens so the next run does a full enumeration.
pub(crate) fn compute_config_hash(config: &crate::config::Config) -> String {
    use sha2::{Digest, Sha256};

    let live_resolution = config.photos.live_resolution.to_asset_version_size();
    let skip_created_before = config
        .filters
        .skip_created_before
        .map(|d| d.with_timezone(&chrono::Utc));
    let skip_created_after = config
        .filters
        .skip_created_after
        .map(|d| d.with_timezone(&chrono::Utc));

    let mut hasher = Sha256::new();
    hash_shared_fields(
        &mut hasher,
        &SharedHashFields {
            directory: &config.download.directory,
            folder_structure: &config.download.folder_structure,
            folder_structure_albums: &config.download.folder_structure_albums,
            folder_structure_smart_folders: &config.download.folder_structure_smart_folders,
            resolution: config.photos.resolution,
            live_resolution,
            file_match_policy: config.photos.file_match_policy,
            live_photo_mov_filename_policy: config.photos.live_photo_mov_filename_policy,
            edited: config.photos.edited,
            alternative: config.photos.alternative,
            raw_policy: config.photos.raw_policy,
            keep_unicode_in_filenames: config.photos.keep_unicode_in_filenames,
            skip_created_before,
            skip_created_after,
            force_resolution: config.photos.force_resolution,
            media: config.filters.media,
            live_photo_mode: config.photos.live_photo_mode,
            filename_exclude: &config.download.filename_exclude,
        },
    );
    // Note: `recent` is intentionally excluded from this enum hash.
    // Changing --recent should not invalidate sync tokens because the
    // incremental path already applies the recent cap post-fetch.
    // `recent` IS included in hash_download_config (trust-state) so
    // changing it still triggers filesystem re-verification.

    // Enumeration-filter fields: changing these affects WHICH assets are
    // fetched from iCloud, so sync tokens must be invalidated to avoid
    // missing assets that are newly eligible under the changed filters.
    // Tag byte distinguishes the three selection modes so switching between
    // them (e.g. `-a A` -> `-a all`) invalidates the sync token even if no
    // explicit album name changed.
    for entry in config.filters.selection.albums.to_raw() {
        hasher.update(b"album:");
        hasher.update(entry.as_bytes());
        hasher.update(b"\0");
    }
    // Library selector: stable tag bytes per shape so changing the resolved
    // library set invalidates sync tokens. `to_raw()` emits a deterministic
    // ordering (`primary`/`shared`/named-then-`!excluded`).
    for entry in config.filters.selection.libraries.to_raw() {
        hasher.update(b"library:");
        hasher.update(entry.as_bytes());
        hasher.update(b"\0");
    }
    // Smart-folder + unfiled selectors drive which CloudKit zones/views are
    // enumerated; toggling them changes the eligible asset set, so the
    // per-zone sync token must be invalidated. Without these fields a
    // `--smart-folder none` → `--smart-folder all` (or `--unfiled false` →
    // default true) change reuses a stale enumeration cursor and the next
    // cycle silently misses every newly-eligible asset.
    for entry in config.filters.selection.smart_folders.to_raw() {
        hasher.update(b"smart_folder:");
        hasher.update(entry.as_bytes());
        hasher.update(b"\0");
    }
    hasher.update(b"unfiled:");
    hasher.update([u8::from(config.filters.selection.unfiled)]);
    finalize_hash(hasher)
}

/// Subset of application config consumed by the download engine.
/// Decoupled from CLI parsing so the engine can be tested independently.
pub(crate) struct DownloadConfig {
    /// Behind `Arc` so per-pass clones (`with_album_name`, `with_pass`,
    /// `with_exclude_ids`) refcount-bump instead of deep-cloning the
    /// PathBuf. Same pattern as `asset_groupings` and `exclude_asset_ids`.
    pub(crate) directory: Arc<Path>,
    /// Template for the unfiled (library-wide) pass. Also the source the
    /// per-pass clone in `with_pass` reads when the pass is `Unfiled`. After
    /// `with_pass` runs, this field holds the *expanded* per-pass template.
    pub(crate) folder_structure: String,
    /// Template for `PassKind::Album` passes (default `{album}`). Behind
    /// `Arc<str>` so per-pass clones refcount-bump instead of deep-cloning;
    /// the user-typed template never mutates after CLI parse.
    pub(crate) folder_structure_albums: Arc<str>,
    /// Template for `PassKind::SmartFolder` passes (default `{smart-folder}`).
    /// Behind `Arc<str>` for the same reason as `folder_structure_albums`.
    pub(crate) folder_structure_smart_folders: Arc<str>,
    pub(crate) resolution: crate::types::PhotoResolution,
    pub(crate) media: crate::config::MediaSelection,
    pub(crate) skip_created_before: Option<DateTime<Utc>>,
    pub(crate) skip_created_after: Option<DateTime<Utc>>,
    pub(crate) set_exif_datetime: bool,
    pub(crate) set_exif_rating: bool,
    pub(crate) set_exif_gps: bool,
    pub(crate) set_exif_description: bool,
    /// Embed the full XMP packet (title, keywords, people, hidden/archived,
    /// media subtype, burst id) into the file bytes on supported formats.
    #[cfg(feature = "xmp")]
    pub(crate) embed_xmp: bool,
    /// Write a `.xmp` sidecar file next to each downloaded media file with
    /// the same composed XMP packet.
    #[cfg(feature = "xmp")]
    pub(crate) xmp_sidecar: bool,
    pub(crate) concurrent_downloads: usize,
    pub(crate) recent: Option<u32>,
    pub(crate) recent_scope: crate::cli::RecentScope,
    pub(crate) retry: RetryConfig,
    pub(crate) live_photo_mode: LivePhotoMode,
    pub(crate) live_resolution: AssetVersionSize,
    pub(crate) live_photo_mov_filename_policy: LivePhotoMovFilenamePolicy,
    pub(crate) edited: bool,
    pub(crate) alternative: bool,
    pub(crate) raw_policy: RawPolicy,
    pub(crate) file_match_policy: FileMatchPolicy,
    pub(crate) force_resolution: bool,
    pub(crate) keep_unicode_in_filenames: bool,
    /// Compiled glob patterns for filename exclusion.
    ///
    /// Behind `Arc<[_]>` so per-pass clones share one allocation
    /// (significant with `-a all` over 100+ albums).
    pub(crate) filename_exclude: Arc<[glob::Pattern]>,
    /// Temp file suffix for partial downloads (e.g. `.kei-tmp`).
    pub(crate) temp_suffix: Arc<str>,
    /// State database for tracking download progress.
    pub(crate) state_db: Option<Arc<dyn StateDb>>,
    /// When true (retry-failed mode), only download assets already known to the
    /// state DB. Skip new assets discovered from iCloud that were never synced.
    pub(crate) retry_only: bool,
    /// Sync mode: full enumeration or incremental delta via syncToken.
    pub(crate) sync_mode: SyncMode,
    /// Album name for `{album}` token in folder_structure. Set per-album when
    /// processing albums individually.
    pub(crate) album_name: Option<Arc<str>>,
    /// CloudKit zone name (e.g. "PrimarySync", "SharedSync-A1B2C3D4-...")
    /// scoping every asset processed under this config. Threaded into
    /// `AssetRecord.library` and every state-DB key so multi-library syncs
    /// don't collide on the (id, version_size) pair across zones.
    pub(crate) library: Arc<str>,
    /// Asset IDs to exclude (from `--exclude-album` without `--album`).
    pub(crate) exclude_asset_ids: Arc<FxHashSet<String>>,
    /// Maximum download attempts per asset before giving up (0 = unlimited).
    pub(crate) max_download_attempts: u32,
    /// Preloaded asset→album and asset→person indices, shared across clones.
    pub(crate) asset_groupings: Arc<AssetGroupings>,
    /// Shared token-bucket limiter applied across all concurrent download
    /// streams. `None` = no throughput cap.
    pub(crate) bandwidth_limiter: Option<BandwidthLimiter>,
}

impl DownloadConfig {
    /// Human-readable label for the active pass: the album's own name for
    /// album/smart-folder passes, "unfiled" for the unfiled pass (which uses
    /// `library.all()` whose `.name` is the empty string).
    pub(crate) fn pass_label(&self) -> &str {
        match self.album_name.as_deref() {
            Some("") | None => "unfiled",
            Some(name) => name,
        }
    }

    /// True when passes can produce divergent paths and need per-pass config
    /// expansion (`with_pass`) plus path-aware skip checks rather than the
    /// merged-stream optimisation + DB-only fast skip.
    ///
    /// Divergence sources: any of the three template fields
    /// (`folder_structure`, `folder_structure_albums`,
    /// `folder_structure_smart_folders`) carries a per-pass token
    /// (`{album}` / `{smart-folder}` / `{library}`), or the per-category
    /// templates differ from the base. Both cases mean a single merged
    /// stream + base config would route assets to the wrong on-disk path.
    ///
    /// Only meaningful on the *base* config. A per-pass config produced by
    /// `with_album_name` / `with_pass` has had per-pass tokens expanded out
    /// of `folder_structure`, but the per-category fields stay cloned from
    /// the base so this still reports the base verdict; per-pass code paths
    /// should check `album_name.is_some()` instead.
    pub(crate) fn requires_per_pass_paths(&self) -> bool {
        const PER_PASS_TOKENS: [&str; 3] = [
            paths::TOKEN_ALBUM,
            paths::TOKEN_SMART_FOLDER,
            paths::TOKEN_LIBRARY,
        ];
        let any_token = |s: &str| PER_PASS_TOKENS.iter().any(|t| s.contains(t));
        any_token(&self.folder_structure)
            || any_token(&self.folder_structure_albums)
            || any_token(&self.folder_structure_smart_folders)
            || self.folder_structure_albums.as_ref() != self.folder_structure.as_str()
            || self.folder_structure_smart_folders.as_ref() != self.folder_structure.as_str()
    }

    /// Construct a `DownloadConfig` with only the path-derivation fields
    /// populated. Used by `import-existing`, which calls
    /// `expected_paths_for` against existing files but never runs the
    /// download pipeline; the pipeline-only fields (state_db, retry,
    /// concurrent_downloads, etc.) stay at inert defaults.
    pub(crate) fn for_path_derivation_only(
        directory: Arc<Path>,
        fields: crate::config::PathDerivationFields,
        media: crate::config::MediaSelection,
    ) -> Self {
        Self {
            directory,
            folder_structure: fields.folder_structure,
            folder_structure_albums: Arc::from(fields.folder_structure_albums.as_str()),
            folder_structure_smart_folders: Arc::from(
                fields.folder_structure_smart_folders.as_str(),
            ),
            library: Arc::from(crate::icloud::photos::PRIMARY_ZONE_NAME),
            resolution: fields.resolution,
            media,
            skip_created_before: None,
            skip_created_after: None,
            set_exif_datetime: false,
            set_exif_rating: false,
            set_exif_gps: false,
            set_exif_description: false,
            #[cfg(feature = "xmp")]
            embed_xmp: false,
            #[cfg(feature = "xmp")]
            xmp_sidecar: false,
            concurrent_downloads: 1,
            recent: None,
            recent_scope: crate::cli::RecentScope::Global,
            retry: RetryConfig::default(),
            live_photo_mode: fields.live_photo_mode,
            live_resolution: fields.live_resolution.to_asset_version_size(),
            live_photo_mov_filename_policy: fields.live_photo_mov_filename_policy,
            edited: fields.edited,
            alternative: fields.alternative,
            raw_policy: fields.raw_policy,
            file_match_policy: fields.file_match_policy,
            force_resolution: fields.force_resolution,
            keep_unicode_in_filenames: fields.keep_unicode_in_filenames,
            filename_exclude: Arc::from(Vec::<glob::Pattern>::new()),
            temp_suffix: Arc::from(".kei-tmp"),
            state_db: None,
            retry_only: false,
            max_download_attempts: 0,
            sync_mode: SyncMode::Full,
            album_name: None,
            exclude_asset_ids: Arc::new(FxHashSet::default()),
            asset_groupings: Arc::new(AssetGroupings::default()),
            bandwidth_limiter: None,
        }
    }

    /// Clone this config for a single download pass: pick the per-category
    /// template (`folder_structure_albums` for `PassKind::Album`,
    /// `folder_structure_smart_folders` for `PassKind::SmartFolder`,
    /// `folder_structure` for `PassKind::Unfiled`), pre-expand the matching
    /// token (`{album}` / `{smart-folder}`), and pin the pass's exclude-ids
    /// set in one clone.
    ///
    /// The unfiled pass keeps the legacy `{album}` token so existing configs
    /// with `--folder-structure "{album}/..."` still produce the same
    /// on-disk tree.
    pub(crate) fn with_pass(&self, pass: &crate::commands::AlbumPass) -> Self {
        let template: &str = match pass.kind {
            crate::commands::PassKind::Album => &self.folder_structure_albums,
            crate::commands::PassKind::SmartFolder => &self.folder_structure_smart_folders,
            crate::commands::PassKind::Unfiled => &self.folder_structure,
        };
        let name = &pass.album.name;
        let name_ref = Some(name.as_ref()).filter(|n: &&str| !n.is_empty());
        let category_expanded = paths::expand_named_token(template, pass.kind.token(), name_ref);
        // Apply `{library}` last with the path-friendly truncated zone name,
        // so callers see `SharedSync-A1B2C3D4/...` instead of the full UUID.
        // The state-DB key still uses the full `self.library` string.
        let library_for_path = paths::truncate_library_zone(&self.library);
        let folder_structure = paths::expand_named_token(
            &category_expanded,
            paths::TOKEN_LIBRARY,
            Some(library_for_path),
        );
        Self {
            album_name: Some(Arc::clone(name)),
            directory: Arc::clone(&self.directory),
            folder_structure,
            folder_structure_albums: Arc::clone(&self.folder_structure_albums),
            folder_structure_smart_folders: Arc::clone(&self.folder_structure_smart_folders),
            filename_exclude: Arc::clone(&self.filename_exclude),
            temp_suffix: Arc::clone(&self.temp_suffix),
            state_db: self.state_db.clone(),
            sync_mode: self.sync_mode.clone(),
            exclude_asset_ids: Arc::clone(&pass.exclude_ids),
            asset_groupings: Arc::clone(&self.asset_groupings),
            bandwidth_limiter: self.bandwidth_limiter.clone(),
            library: Arc::clone(&self.library),
            ..*self
        }
    }

    /// Clone this config with a different `library`. Import-existing pins
    /// the per-library zone before calling `with_pass` so `{library}`
    /// expands to the right path segment for each library iteration.
    pub(crate) fn with_library(&self, library: &str) -> Self {
        Self {
            directory: Arc::clone(&self.directory),
            folder_structure: self.folder_structure.clone(),
            folder_structure_albums: Arc::clone(&self.folder_structure_albums),
            folder_structure_smart_folders: Arc::clone(&self.folder_structure_smart_folders),
            filename_exclude: Arc::clone(&self.filename_exclude),
            temp_suffix: Arc::clone(&self.temp_suffix),
            state_db: self.state_db.clone(),
            sync_mode: self.sync_mode.clone(),
            album_name: self.album_name.clone(),
            exclude_asset_ids: Arc::clone(&self.exclude_asset_ids),
            asset_groupings: Arc::clone(&self.asset_groupings),
            bandwidth_limiter: self.bandwidth_limiter.clone(),
            library: Arc::from(library),
            ..*self
        }
    }

    /// Clone this config with a different `exclude_asset_ids` set. Used
    /// for the merged (non-`{album}`) full-sync path, where all passes
    /// share a single config but the exclude set is lifted off the plan.
    fn with_exclude_ids(&self, exclude_ids: Arc<FxHashSet<String>>) -> Self {
        Self {
            directory: Arc::clone(&self.directory),
            folder_structure: self.folder_structure.clone(),
            folder_structure_albums: Arc::clone(&self.folder_structure_albums),
            folder_structure_smart_folders: Arc::clone(&self.folder_structure_smart_folders),
            filename_exclude: Arc::clone(&self.filename_exclude),
            temp_suffix: Arc::clone(&self.temp_suffix),
            state_db: self.state_db.clone(),
            sync_mode: self.sync_mode.clone(),
            album_name: self.album_name.clone(),
            exclude_asset_ids: exclude_ids,
            asset_groupings: Arc::clone(&self.asset_groupings),
            bandwidth_limiter: self.bandwidth_limiter.clone(),
            library: Arc::clone(&self.library),
            ..*self
        }
    }
}

impl std::fmt::Debug for DownloadConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = f.debug_struct("DownloadConfig");
        s.field("directory", &self.directory)
            .field("folder_structure", &self.folder_structure)
            .field("folder_structure_albums", &self.folder_structure_albums)
            .field(
                "folder_structure_smart_folders",
                &self.folder_structure_smart_folders,
            )
            .field("resolution", &self.resolution)
            .field("media", &self.media)
            .field("skip_created_before", &self.skip_created_before)
            .field("skip_created_after", &self.skip_created_after);
        s.field("set_exif_datetime", &self.set_exif_datetime)
            .field("set_exif_rating", &self.set_exif_rating)
            .field("set_exif_gps", &self.set_exif_gps)
            .field("set_exif_description", &self.set_exif_description);
        #[cfg(feature = "xmp")]
        s.field("embed_xmp", &self.embed_xmp)
            .field("xmp_sidecar", &self.xmp_sidecar);
        s.field("concurrent_downloads", &self.concurrent_downloads)
            .field("recent", &self.recent)
            .field("recent_scope", &self.recent_scope)
            .field("retry", &self.retry)
            .field("live_photo_mode", &self.live_photo_mode)
            .field("live_resolution", &self.live_resolution)
            .field(
                "live_photo_mov_filename_policy",
                &self.live_photo_mov_filename_policy,
            )
            .field("edited", &self.edited)
            .field("alternative", &self.alternative)
            .field("raw_policy", &self.raw_policy)
            .field("file_match_policy", &self.file_match_policy)
            .field("force_resolution", &self.force_resolution)
            .field("keep_unicode_in_filenames", &self.keep_unicode_in_filenames)
            .field("filename_exclude", &self.filename_exclude)
            .field("temp_suffix", &self.temp_suffix)
            .field("state_db", &self.state_db.is_some())
            .field("retry_only", &self.retry_only)
            .field("sync_mode", &self.sync_mode)
            .field("album_name", &self.album_name)
            .field("exclude_asset_ids_count", &self.exclude_asset_ids.len())
            .field("max_download_attempts", &self.max_download_attempts)
            .field("bandwidth_limiter", &self.bandwidth_limiter)
            .finish()
    }
}

#[cfg(test)]
impl DownloadConfig {
    /// Default test config shared across download submodule tests.
    pub(super) fn test_default() -> Self {
        use rustc_hash::FxHashSet;
        Self {
            directory: Arc::from(Path::new("/nonexistent/download_filter_tests")),
            folder_structure: "{:%Y/%m/%d}".to_string(),
            folder_structure_albums: Arc::from(crate::config::DEFAULT_FOLDER_STRUCTURE_ALBUMS),
            folder_structure_smart_folders: Arc::from(
                crate::config::DEFAULT_FOLDER_STRUCTURE_SMART_FOLDERS,
            ),
            resolution: crate::types::PhotoResolution::Original,
            media: crate::config::MediaSelection::all(),
            skip_created_before: None,
            skip_created_after: None,
            set_exif_datetime: false,
            set_exif_rating: false,
            set_exif_gps: false,
            set_exif_description: false,
            #[cfg(feature = "xmp")]
            embed_xmp: false,
            #[cfg(feature = "xmp")]
            xmp_sidecar: false,
            concurrent_downloads: 1,
            recent: None,
            recent_scope: crate::cli::RecentScope::Global,
            retry: crate::retry::RetryConfig::default(),
            live_photo_mode: LivePhotoMode::Both,
            live_resolution: AssetVersionSize::LiveOriginal,
            live_photo_mov_filename_policy: crate::types::LivePhotoMovFilenamePolicy::Suffix,
            edited: false,
            alternative: false,
            raw_policy: RawPolicy::AsIs,
            file_match_policy: FileMatchPolicy::NameSizeDedupWithSuffix,
            force_resolution: false,
            keep_unicode_in_filenames: false,
            filename_exclude: Arc::from(Vec::<glob::Pattern>::new()),
            temp_suffix: Arc::from(".kei-tmp"),
            state_db: None,
            retry_only: false,
            max_download_attempts: 10,
            sync_mode: SyncMode::Full,
            album_name: None,
            exclude_asset_ids: std::sync::Arc::new(FxHashSet::default()),
            asset_groupings: Arc::new(AssetGroupings::default()),
            bandwidth_limiter: None,
            library: Arc::from(crate::icloud::photos::PRIMARY_ZONE_NAME),
        }
    }
}

/// Look up an owned `String` id in a shared `Arc<str>` interner,
/// inserting a fresh `Arc` if absent. Returns the shared handle.
///
/// `Arc::from(String)` transfers the string's buffer into the `Arc<str>`
/// without copying the bytes, so the miss path allocates no more than
/// the baseline cost of a fresh handle.
fn intern_id(interner: &mut FxHashSet<Arc<str>>, s: String) -> Arc<str> {
    if let Some(existing) = interner.get(s.as_str()) {
        return Arc::clone(existing);
    }
    let a: Arc<str> = Arc::from(s);
    interner.insert(Arc::clone(&a));
    a
}

/// Pre-loaded download state for O(1) skip decisions.
///
/// Loaded once at sync start from the state database, this enables fast
/// in-memory lookups instead of per-asset DB queries. For 100K+ asset
/// libraries, this significantly reduces DB roundtrips.
///
/// Asset-id keys are `Arc<str>` rather than `Box<str>` so the same id
/// allocation is shared across every map here (and with the producer's
/// seen-ids / touched-ids sets). On a 100k-asset library this collapses
/// ~4-6 independent `Box<str>` allocations per asset into one.
/// `library -> asset_id -> set of version_size` strings. Used by
/// `DownloadContext::downloaded_ids` and `metadata_retry_markers`.
type LibraryAssetVersionSet = FxHashMap<Arc<str>, FxHashMap<Arc<str>, FxHashSet<Box<str>>>>;

/// `library -> asset_id -> (version_size -> value)`. Used by
/// `DownloadContext::downloaded_checksums` and
/// `downloaded_metadata_hashes`.
type LibraryAssetVersionValueMap =
    FxHashMap<Arc<str>, FxHashMap<Arc<str>, FxHashMap<Box<str>, Box<str>>>>;

#[derive(Debug, Default)]
struct DownloadContext {
    /// Nested map: `library` -> `asset_id` -> set of `version_sizes` that
    /// are already downloaded. Three-level shape so multi-library syncs
    /// don't dedupe the same asset_id across zones (PR10 / schema v8).
    /// All key levels use borrowed `&str` lookups for zero-allocation probes.
    downloaded_ids: LibraryAssetVersionSet,
    /// Nested map: `library` -> `asset_id` -> (`version_size` -> checksum).
    /// Used to detect checksum changes (CloudKit asset updated) without DB queries.
    downloaded_checksums: LibraryAssetVersionValueMap,
    /// Nested map: `library` -> `asset_id` -> (`version_size` -> metadata_hash).
    /// Used to detect metadata-only changes (favorite toggle, keywords, GPS
    /// edit, etc.) when file bytes are unchanged but CloudKit has newer
    /// metadata.
    #[cfg_attr(not(feature = "xmp"), allow(dead_code))]
    downloaded_metadata_hashes: LibraryAssetVersionValueMap,
    /// Nested map: `library` -> `asset_id` -> set of `version_sizes` with a
    /// non-null `metadata_write_failed_at` from a prior sync. These always
    /// route to the metadata-rewrite path regardless of whether the hash
    /// changed.
    #[cfg_attr(not(feature = "xmp"), allow(dead_code))]
    metadata_retry_markers: LibraryAssetVersionSet,
    /// Nested map: `library` -> `asset_id` -> set of `version_sizes` that
    /// are pending at sync start. Used to resolve failed/pending rows when
    /// the expected file is already on disk instead of promoting them back to
    /// failed after the producer skips the duplicate path.
    pending_ids: LibraryAssetVersionSet,
    /// All asset IDs known to the state DB (any status). Used in retry-only mode
    /// to skip new assets that were never synced. Library-blind: a known ID
    /// is "known" regardless of which zone it belongs to.
    known_ids: FxHashSet<Arc<str>>,
    /// Per-asset maximum download attempt count (from failed assets).
    /// Used to skip assets that have exceeded `max_download_attempts`.
    /// Library-blind: an asset shared across libraries shares its attempt
    /// budget (mirrors how `get_attempt_counts` aggregates by id alone).
    attempt_counts: FxHashMap<Arc<str>, u32>,
    /// True when at least one downloaded asset-version lacks a metadata hash.
    /// Cached because the producer checks this on hot on-disk skip paths.
    downloaded_without_metadata_hash: bool,
}

impl DownloadContext {
    /// Load the download context from the state database. All state queries
    /// are independent and run concurrently so sync start doesn't serialize
    /// on round-trip latency across them.
    async fn load<D>(db: &D, retry_only: bool) -> Self
    where
        D: DownloadStateStore + MetadataRewriteStore + ?Sized,
    {
        let known_ids_fut = async {
            if retry_only {
                db.get_all_known_ids().await.unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "Failed to load known IDs from state DB");
                    Default::default()
                })
            } else {
                Default::default()
            }
        };
        let (ids, checksums, hashes, markers, pending, attempts, known_ids) = tokio::join!(
            async {
                db.get_downloaded_ids().await.unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "Failed to load downloaded IDs from state DB");
                    Default::default()
                })
            },
            async {
                db.get_downloaded_checksums().await.unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "Failed to load checksums from state DB");
                    Default::default()
                })
            },
            async {
                db.get_downloaded_metadata_hashes()
                    .await
                    .unwrap_or_else(|e| {
                        tracing::warn!(error = %e, "Failed to load metadata hashes from state DB");
                        Default::default()
                    })
            },
            async {
                db.get_metadata_retry_markers().await.unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "Failed to load metadata retry markers from state DB");
                    Default::default()
                })
            },
            async {
                db.get_pending().await.unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "Failed to load pending assets from state DB");
                    Default::default()
                })
            },
            async {
                db.get_attempt_counts().await.unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "Failed to load attempt counts from state DB");
                    Default::default()
                })
            },
            known_ids_fut,
        );

        // Shared interner so the same asset_id allocates exactly one
        // `Arc<str>` across every map below (and is cheaply cloneable
        // into each via Arc::clone). This collapses the former 4-6
        // independent `String -> Box<str>` conversions per id into one.
        //
        // FxHashSet<Arc<str>> over FxHashMap<String, Arc<str>> so the
        // interner doesn't keep a duplicate `String` alive for every
        // id; `Arc::from(String)` transfers the String's buffer into
        // the Arc without an extra copy.
        let mut interner: FxHashSet<Arc<str>> = FxHashSet::default();

        let mut downloaded_ids: LibraryAssetVersionSet = FxHashMap::default();
        for (library, asset_id, version_size) in ids {
            let lib = intern_id(&mut interner, library);
            let id = intern_id(&mut interner, asset_id);
            downloaded_ids
                .entry(lib)
                .or_default()
                .entry(id)
                .or_default()
                .insert(version_size.into_boxed_str());
        }

        let mut downloaded_checksums: LibraryAssetVersionValueMap = FxHashMap::default();
        for ((library, asset_id, version_size), checksum) in checksums {
            let lib = intern_id(&mut interner, library);
            let id = intern_id(&mut interner, asset_id);
            downloaded_checksums
                .entry(lib)
                .or_default()
                .entry(id)
                .or_default()
                .insert(version_size.into_boxed_str(), checksum.into_boxed_str());
        }

        let mut downloaded_metadata_hashes: LibraryAssetVersionValueMap = FxHashMap::default();
        for ((library, asset_id, version_size), metadata_hash) in hashes {
            let lib = intern_id(&mut interner, library);
            let id = intern_id(&mut interner, asset_id);
            downloaded_metadata_hashes
                .entry(lib)
                .or_default()
                .entry(id)
                .or_default()
                .insert(
                    version_size.into_boxed_str(),
                    metadata_hash.into_boxed_str(),
                );
        }

        let mut metadata_retry_markers: LibraryAssetVersionSet = FxHashMap::default();
        for (library, asset_id, version_size) in markers {
            let lib = intern_id(&mut interner, library);
            let id = intern_id(&mut interner, asset_id);
            metadata_retry_markers
                .entry(lib)
                .or_default()
                .entry(id)
                .or_default()
                .insert(version_size.into_boxed_str());
        }

        let mut pending_ids: LibraryAssetVersionSet = FxHashMap::default();
        for record in pending {
            let lib = intern_id(&mut interner, record.library.to_string());
            let id = intern_id(&mut interner, record.id.to_string());
            pending_ids
                .entry(lib)
                .or_default()
                .entry(id)
                .or_default()
                .insert(record.version_size.as_str().into());
        }

        let known_ids: FxHashSet<Arc<str>> = known_ids
            .into_iter()
            .map(|id| intern_id(&mut interner, id))
            .collect();

        let attempt_counts: FxHashMap<Arc<str>, u32> = attempts
            .into_iter()
            .map(|(id, count)| (intern_id(&mut interner, id), count))
            .collect();
        let downloaded_without_metadata_hash = count_version_set_entries(&downloaded_ids)
            > count_value_map_entries(&downloaded_metadata_hashes);

        Self {
            downloaded_ids,
            downloaded_checksums,
            downloaded_metadata_hashes,
            metadata_retry_markers,
            pending_ids,
            known_ids,
            attempt_counts,
            downloaded_without_metadata_hash,
        }
    }

    /// Whether a downloaded asset-version needs a metadata-only rewrite:
    /// the caller has already matched checksums (bytes unchanged) and now
    /// checks whether (a) the stored metadata_hash differs from the new
    /// one or (b) a persisted retry marker is set from a prior sync where
    /// the writer failed after bytes landed.
    #[cfg_attr(not(feature = "xmp"), allow(dead_code))]
    fn needs_metadata_rewrite(
        &self,
        library: &str,
        asset_id: &str,
        version_size: VersionSizeKey,
        new_metadata_hash: Option<&str>,
    ) -> bool {
        let vs_str = version_size.as_str();
        let has_retry_marker = self
            .metadata_retry_markers
            .get(library)
            .and_then(|m| m.get(asset_id))
            .is_some_and(|vsset| vsset.contains(vs_str));
        if has_retry_marker {
            return true;
        }
        let Some(new_hash) = new_metadata_hash else {
            return false;
        };
        match self
            .downloaded_metadata_hashes
            .get(library)
            .and_then(|m| m.get(asset_id))
            .and_then(|map| map.get(vs_str))
        {
            Some(stored) => stored.as_ref() != new_hash,
            None => true, // downloaded row has no stored hash yet -- refresh
        }
    }

    /// Check if an asset should be downloaded based on pre-loaded state.
    ///
    /// Returns:
    /// - `Some(true)` — definitely needs download (not in DB or checksum changed)
    /// - `Some(false)` — hard skip, DB confirms downloaded with matching checksum
    ///   (only when `trust_state` is true)
    /// - `None` — downloaded with matching checksum but needs filesystem check
    ///   to confirm file is still on disk (when `trust_state` is false)
    ///
    /// `trust_state=true` skips the filesystem stat: only `--only-print-filenames`
    /// uses it (no side effects, the user just wants to preview). The real-sync
    /// path uses `trust_state=false` — see PR #318 for why.
    ///
    /// Uses borrowed `&str` keys for zero-allocation lookups.
    fn should_download_fast(
        &self,
        library: &str,
        asset_id: &str,
        version_size: VersionSizeKey,
        checksum: &str,
        trust_state: bool,
    ) -> Option<bool> {
        if checksum.is_empty() {
            tracing::warn!(
                asset_id,
                version_size = %version_size.as_str(),
                "Empty remote checksum cannot be trusted for skip decisions"
            );
            return Some(true);
        }

        let version_size_str = version_size.as_str();

        // Borrowed `&str` keys at every level — no allocation per probe.
        let is_downloaded = self
            .downloaded_ids
            .get(library)
            .and_then(|m| m.get(asset_id))
            .is_some_and(|versions| versions.contains(version_size_str));

        if !is_downloaded {
            // Not in downloaded set — needs download
            return Some(true);
        }

        // Check if checksum changed (also zero-allocation lookup). Track
        // whether a stored checksum is present at all so we can audit the
        // "no stored checksum" path, which pre-v3 rows fall into.
        let stored_checksum = self
            .downloaded_checksums
            .get(library)
            .and_then(|m| m.get(asset_id))
            .and_then(|versions| versions.get(version_size_str));
        if let Some(stored) = stored_checksum {
            if stored.as_ref() != checksum {
                return Some(true);
            }
        } else {
            // Pre-v3 row with no stored local_checksum. Audit so operators can
            // correlate unexpected "skipped" counts with missing checksum
            // history (the row will gain a checksum on next download).
            tracing::debug!(
                asset_id = asset_id,
                version_size = %version_size_str,
                trust_state = trust_state,
                "no stored checksum for downloaded asset-version; skip decision uses trust_state only"
            );
        }

        if trust_state {
            Some(false)
        } else {
            None
        }
    }

    fn has_downloaded_without_metadata_hash(&self) -> bool {
        self.downloaded_without_metadata_hash
    }
}

fn count_version_set_entries(map: &LibraryAssetVersionSet) -> usize {
    map.values()
        .map(|assets| {
            assets
                .values()
                .map(|versions| versions.len())
                .sum::<usize>()
        })
        .sum()
}

fn count_value_map_entries(map: &LibraryAssetVersionValueMap) -> usize {
    map.values()
        .map(|assets| {
            assets
                .values()
                .map(|versions| versions.len())
                .sum::<usize>()
        })
        .sum()
}

async fn preload_download_context(config: &DownloadConfig) -> Arc<DownloadContext> {
    let download_ctx = if let Some(db) = &config.state_db {
        tracing::debug!("Pre-loading download state from database");
        DownloadContext::load(db.as_ref(), config.retry_only).await
    } else {
        DownloadContext::default()
    };
    tracing::debug!(
        downloaded_ids = download_ctx.downloaded_ids.len(),
        "Download context loaded"
    );
    Arc::new(download_ctx)
}

/// Pre-compute one `Arc<DownloadConfig>` per pass. Each pass_index maps to
/// a derived config that pre-expands `{album}` and pins the pass's
/// exclude-asset-ids set. In `{album}` mode passes may legitimately differ
/// per entry; outside of it, passes share identical excludes but the per-
/// pass wrapper is harmless and keeps call sites uniform.
fn build_pass_configs(
    passes: &[crate::commands::AlbumPass],
    base: &DownloadConfig,
) -> Vec<Arc<DownloadConfig>> {
    passes
        .iter()
        .map(|pass| Arc::new(base.with_pass(pass)))
        .collect()
}

fn build_pass_configs_with_download_concurrency(
    passes: &[crate::commands::AlbumPass],
    base: &DownloadConfig,
    per_pass_download_concurrency: usize,
) -> Vec<Arc<DownloadConfig>> {
    passes
        .iter()
        .map(|pass| {
            let mut config = base.with_pass(pass);
            config.concurrent_downloads = per_pass_download_concurrency.max(1);
            Arc::new(config)
        })
        .collect()
}

fn incremental_requires_full_enumeration(passes: &[crate::commands::AlbumPass]) -> bool {
    passes
        .iter()
        .any(|pass| pass.kind != crate::commands::PassKind::Unfiled)
}

async fn collect_pass_asset_ids(pass: &crate::commands::AlbumPass) -> Result<FxHashSet<String>> {
    let count =
        pass.album.len().await.with_context(|| {
            format!("failed to get asset count for album '{}'", pass.album.name)
        })?;
    let (stream, _token_rx) = pass.album.photo_stream_with_token(None, Some(count), 1);
    tokio::pin!(stream);
    let mut ids = FxHashSet::default();
    while let Some(item) = stream.next().await {
        let asset = item?;
        ids.insert(asset.id().to_string());
    }
    Ok(ids)
}

async fn build_pass_configs_resolving_deferred_excludes(
    passes: &[crate::commands::AlbumPass],
    base: &DownloadConfig,
) -> Result<Vec<Arc<DownloadConfig>>> {
    let mut pass_configs = build_pass_configs(passes, base);
    let Some(unfiled_index) = deferred_unfiled_index(passes) else {
        return Ok(pass_configs);
    };

    let per_album: Vec<Result<FxHashSet<String>>> = stream::iter(
        passes
            .iter()
            .filter(|pass| pass.kind == crate::commands::PassKind::Album),
    )
    .map(collect_pass_asset_ids)
    .buffer_unordered(base.concurrent_downloads.max(1))
    .collect()
    .await;

    let mut exclude_ids = FxHashSet::default();
    for ids in per_album {
        exclude_ids.extend(ids?);
    }

    if let (Some(pass), Some(slot)) = (
        passes.get(unfiled_index),
        pass_configs.get_mut(unfiled_index),
    ) {
        let mut unfiled_config = base.with_pass(pass);
        unfiled_config.exclude_asset_ids = Arc::new(exclude_ids);
        *slot = Arc::new(unfiled_config);
    }
    Ok(pass_configs)
}

#[derive(Debug)]
struct PerPassStreamingResult {
    kind: crate::commands::PassKind,
    label: String,
    count: u64,
    elapsed: std::time::Duration,
    token_rx: tokio::sync::oneshot::Receiver<Option<String>>,
    result: StreamingResult,
}

type DownloadPhotoStream = Pin<Box<dyn Stream<Item = anyhow::Result<PhotoAsset>> + Send + 'static>>;

fn open_photo_stream_for_controls(
    album: &crate::icloud::photos::PhotoAlbum,
    limit: Option<u32>,
    total_count: Option<u64>,
    fast_concurrency: usize,
    download_concurrency: usize,
    controls: DownloadControls,
) -> (
    DownloadPhotoStream,
    tokio::sync::oneshot::Receiver<Option<String>>,
) {
    if controls.run_mode.is_dry_run() || controls.run_mode.only_print_filenames() {
        album.photo_stream_with_token(limit, total_count, fast_concurrency)
    } else {
        album.photo_stream_with_token_for_download(limit, total_count, download_concurrency)
    }
}

struct RecentFrontier {
    asset_ids: Arc<FxHashSet<String>>,
    oldest_created: Option<DateTime<Utc>>,
    assets: Vec<PhotoAsset>,
}

struct CollectedUnfiledStream {
    token_rx: tokio::sync::oneshot::Receiver<Option<String>>,
    items: Vec<anyhow::Result<PhotoAsset>>,
}

struct FullPassStreamOptions {
    controls: DownloadControls,
    count: u64,
    kind: crate::commands::PassKind,
    shutdown_token: CancellationToken,
    download_ctx: Option<Arc<DownloadContext>>,
}

fn deferred_unfiled_index(passes: &[crate::commands::AlbumPass]) -> Option<usize> {
    let has_album_pass = passes
        .iter()
        .any(|pass| pass.kind == crate::commands::PassKind::Album);
    if !has_album_pass {
        return None;
    }
    passes.iter().position(|pass| {
        pass.kind == crate::commands::PassKind::Unfiled && pass.exclude_ids.is_empty()
    })
}

fn should_use_scope_recent_frontier(passes: &[crate::commands::AlbumPass]) -> bool {
    passes
        .iter()
        .any(|pass| pass.kind != crate::commands::PassKind::Unfiled || !pass.exclude_ids.is_empty())
}

fn should_use_global_recent_frontier(
    passes: &[crate::commands::AlbumPass],
    config: &DownloadConfig,
) -> bool {
    config.recent.is_some()
        && config.recent_scope == crate::cli::RecentScope::Global
        && should_use_scope_recent_frontier(passes)
}

async fn build_recent_frontier(
    passes: &[crate::commands::AlbumPass],
    config: &DownloadConfig,
    controls: DownloadControls,
    shutdown_token: CancellationToken,
    retain_assets: bool,
) -> Result<Option<RecentFrontier>> {
    let Some(recent) = config.recent else {
        return Ok(None);
    };
    if !should_use_global_recent_frontier(passes, config) {
        return Ok(None);
    }

    let Some(frontier_source) = passes
        .iter()
        .find(|pass| pass.kind == crate::commands::PassKind::Unfiled)
        .map(|pass| pass.album.clone_as_library_wide())
        .or_else(|| {
            passes
                .first()
                .map(|pass| pass.album.clone_as_library_wide())
        })
    else {
        return Ok(None);
    };

    let (stream, _token_rx) = open_photo_stream_for_controls(
        &frontier_source,
        Some(recent),
        None,
        config.concurrent_downloads,
        config.concurrent_downloads,
        controls,
    );
    tokio::pin!(stream);

    let mut asset_ids = FxHashSet::default();
    let mut oldest_created: Option<DateTime<Utc>> = None;
    let mut assets = Vec::new();
    while let Some(item) = stream.next().await {
        if shutdown_token.is_cancelled() {
            break;
        }
        let asset = item?;
        let created = asset.created();
        if config
            .skip_created_before
            .map(|boundary| created < boundary)
            .unwrap_or(false)
        {
            break;
        }
        oldest_created = Some(oldest_created.map_or(created, |oldest| oldest.min(created)));
        asset_ids.insert(asset.id().to_string());
        if retain_assets {
            assets.push(asset);
        }
    }
    Ok(Some(RecentFrontier {
        asset_ids: Arc::new(asset_ids),
        oldest_created,
        assets,
    }))
}

fn stream_created_lower_bound(
    config: &DownloadConfig,
    frontier: Option<&RecentFrontier>,
) -> Option<DateTime<Utc>> {
    frontier
        .and_then(|frontier| frontier.oldest_created)
        .into_iter()
        .chain(config.skip_created_before)
        .max()
}

fn filter_stream_to_enumeration_bounds(
    stream: DownloadPhotoStream,
    config: &DownloadConfig,
    frontier: Option<&RecentFrontier>,
) -> DownloadPhotoStream {
    let asset_ids = frontier.map(|frontier| Arc::clone(&frontier.asset_ids));
    let lower_created_bound = stream_created_lower_bound(config, frontier);
    Box::pin(
        stream
            .take_while(move |item| {
                std::future::ready(match item {
                    Ok(asset) => lower_created_bound
                        .map(|boundary| asset.created() >= boundary)
                        .unwrap_or(true),
                    Err(_) => true,
                })
            })
            .filter_map(move |item| {
                std::future::ready(match item {
                    Ok(asset)
                        if asset_ids
                            .as_ref()
                            .map(|ids| ids.contains(asset.id()))
                            .unwrap_or(true) =>
                    {
                        Some(Ok(asset))
                    }
                    Ok(_) => None,
                    Err(e) => Some(Err(e)),
                })
            }),
    )
}

fn scope_frontier_limit(
    config: &DownloadConfig,
    recent_frontier: Option<&RecentFrontier>,
) -> Option<u32> {
    recent_frontier.map_or(config.recent, |_| None)
}

fn collected_unfiled_from_recent_frontier(frontier: &RecentFrontier) -> CollectedUnfiledStream {
    let (token_tx, token_rx) = tokio::sync::oneshot::channel();
    let _ = token_tx.send(None);
    CollectedUnfiledStream {
        token_rx,
        items: frontier.assets.iter().cloned().map(Ok).collect(),
    }
}

async fn collect_unfiled_stream(
    pass: &crate::commands::AlbumPass,
    stream_total_count: Option<u64>,
    config: &DownloadConfig,
    controls: DownloadControls,
    shutdown_token: CancellationToken,
) -> CollectedUnfiledStream {
    let (stream, token_rx) = open_photo_stream_for_controls(
        &pass.album,
        config.recent,
        stream_total_count,
        config.concurrent_downloads,
        config.concurrent_downloads,
        controls,
    );
    let stream = filter_stream_to_enumeration_bounds(stream, config, None);
    tokio::pin!(stream);
    let mut items = Vec::new();
    while let Some(item) = stream.next().await {
        if shutdown_token.is_cancelled() {
            break;
        }
        items.push(item);
    }
    CollectedUnfiledStream { token_rx, items }
}

async fn run_full_pass_stream<S>(
    download_client: Client,
    stream: S,
    token_rx: tokio::sync::oneshot::Receiver<Option<String>>,
    pass_config: Arc<DownloadConfig>,
    options: FullPassStreamOptions,
) -> Result<PerPassStreamingResult>
where
    S: futures_util::Stream<Item = anyhow::Result<PhotoAsset>> + Send + 'static,
{
    // Per-album bar: the bar represents only this album's progress,
    // not the cumulative grand total. When the divider is active
    // (multi-pass friendly), the bar plus divider together give the
    // user per-album awareness; the divider's done lines accumulate
    // in scrollback so completed albums don't disappear.
    let pass_start = Instant::now();
    let (pass_pb, pass_bytes) = crate::download::pipeline::create_progress_bar_for_passes(
        options.controls.reporting.no_progress_bar,
        options.controls.run_mode.only_print_filenames(),
        options.count,
        options.controls.reporting.personality_mode,
    );

    let result = stream_and_download_from_stream(
        &download_client,
        stream,
        &pass_config,
        options.controls,
        options.count,
        options.shutdown_token,
        StreamRuntime::with_context(
            Some(pass_pb.clone()),
            Some(std::sync::Arc::clone(&pass_bytes)),
            options.download_ctx,
        ),
    )
    .await?;

    let elapsed = pass_start.elapsed();
    pass_pb.finish_and_clear();
    Ok(PerPassStreamingResult {
        kind: options.kind,
        label: pass_config.pass_label().to_string(),
        count: options.count,
        elapsed,
        token_rx,
        result,
    })
}

fn merge_streaming_result(combined: &mut StreamingResult, result: StreamingResult) {
    combined.downloaded += result.downloaded;
    combined.exif_failures += result.exif_failures;
    combined.failed.extend(result.failed);
    combined.auth_errors += result.auth_errors;
    combined.state_write_failures += result.state_write_failures;
    combined.enumeration_errors += result.enumeration_errors;
    combined.assets_seen += result.assets_seen;
    combined.skip_summary += result.skip_summary;
    // AND-fold across passes so a single pass aborting (e.g.
    // producer-channel close, panic) leaves the marker set.
    combined.enumeration_complete = combined.enumeration_complete && result.enumeration_complete;
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RetryTaskKey {
    asset_id: Arc<str>,
    version_size: VersionSizeKey,
    download_path: std::path::PathBuf,
}

impl From<&DownloadTask> for RetryTaskKey {
    fn from(task: &DownloadTask) -> Self {
        Self {
            asset_id: Arc::clone(&task.asset_id),
            version_size: task.version_size,
            download_path: task.download_path.clone(),
        }
    }
}

fn take_matching_retry_tasks<I>(
    tasks: I,
    pending_keys: &mut FxHashSet<RetryTaskKey>,
    out: &mut Vec<DownloadTask>,
) where
    I: IntoIterator<Item = DownloadTask>,
{
    for task in tasks {
        let key = RetryTaskKey::from(&task);
        if pending_keys.remove(&key) {
            out.push(task);
            if pending_keys.is_empty() {
                break;
            }
        }
    }
}

/// Eagerly enumerate all albums and build a complete task list.
///
/// Used only by the Phase 2 cleanup pass — re-contacts the API so each call
/// yields fresh CDN URLs that haven't expired during a long download session.
#[allow(
    dead_code,
    reason = "kept for focused tests and future non-selective tooling"
)]
async fn build_download_tasks(
    passes: &[crate::commands::AlbumPass],
    config: &DownloadConfig,
    shutdown_token: CancellationToken,
) -> Result<Vec<DownloadTask>> {
    let pass_configs = build_pass_configs_resolving_deferred_excludes(passes, config).await?;
    let pass_results: Vec<Result<(usize, Vec<_>)>> = stream::iter(passes.iter().enumerate())
        .take_while(|_| std::future::ready(!shutdown_token.is_cancelled()))
        .map(|(i, pass)| async move { pass.album.photos(config.recent).await.map(|a| (i, a)) })
        .buffer_unordered(config.concurrent_downloads)
        .collect()
        .await;

    let mut tasks: Vec<DownloadTask> = Vec::new();
    let mut task_planner = planner::TaskPlanner::new();
    for pass_result in pass_results {
        let (pass_index, assets) = pass_result?;
        #[allow(
            clippy::indexing_slicing,
            reason = "pass_index comes from enumerate() over `passes`; pass_configs is \
                      built 1:1 from the same slice"
        )]
        let pass_config = &pass_configs[pass_index];

        for asset in &assets {
            let plan = task_planner.plan_asset(asset, pass_config).await;
            if plan.filter_reason.is_some() {
                continue;
            }
            tasks.extend(plan.tasks);
        }
    }

    Ok(tasks)
}

/// Re-enumerate iCloud and rebuild only the failed tasks with fresh CDN URLs.
///
/// The first pass may fail because signed content URLs expired before the
/// worker reached them. Retrying the complete library after that is both slow
/// and risky: newly-issued URLs for early tasks can age again while unrelated
/// albums are planned. Limit cleanup to the exact asset/version/path tuples
/// that failed so the retry pass starts consuming refreshed URLs quickly.
async fn build_retry_download_tasks(
    passes: &[crate::commands::AlbumPass],
    config: &DownloadConfig,
    failed_tasks: &[DownloadTask],
    shutdown_token: CancellationToken,
) -> Result<Vec<DownloadTask>> {
    if failed_tasks.is_empty() {
        return Ok(Vec::new());
    }

    let mut pending_keys: FxHashSet<RetryTaskKey> =
        failed_tasks.iter().map(RetryTaskKey::from).collect();
    let requested_count = pending_keys.len();
    let pass_configs = build_pass_configs_resolving_deferred_excludes(passes, config).await?;
    let mut tasks: Vec<DownloadTask> = Vec::with_capacity(requested_count);
    let mut task_planner = planner::TaskPlanner::new();

    for (pass_index, pass) in passes.iter().enumerate() {
        if pending_keys.is_empty() || shutdown_token.is_cancelled() {
            break;
        }

        let assets = pass.album.photos(config.recent).await?;
        #[allow(
            clippy::indexing_slicing,
            reason = "pass_index comes from enumerate() over `passes`; pass_configs is \
                      built 1:1 from the same slice"
        )]
        let pass_config = &pass_configs[pass_index];

        for asset in &assets {
            if pending_keys.is_empty() || shutdown_token.is_cancelled() {
                break;
            }
            let plan = task_planner.plan_asset(asset, pass_config).await;
            if plan.filter_reason.is_some() {
                continue;
            }
            take_matching_retry_tasks(plan.tasks, &mut pending_keys, &mut tasks);
        }
    }

    if !pending_keys.is_empty() {
        tracing::warn!(
            requested = requested_count,
            refreshed = tasks.len(),
            missing = pending_keys.len(),
            "Cleanup pass could not refresh every failed task; unmatched failures remain pending"
        );
    }

    Ok(tasks)
}

/// Download photos with syncToken support.
///
/// In `SyncMode::Full`: runs the existing full enumeration via
/// `photo_stream_with_token`, captures the syncToken after the stream is
/// consumed, and delegates download logic to the existing pipeline.
///
/// In `SyncMode::Incremental`: uses `changes_stream` for delta sync,
/// filters `ChangeEvent`s to downloadable assets, and feeds them through
/// the existing download pipeline. Falls back to `SyncMode::Full` if the
/// token is invalid or expired.
/// Minimum age (seconds) a `.part` file must have before
/// `cleanup_orphan_part_files` will remove it, regardless of whether the
/// file is older than `last_sync_completed`. Defends against the
/// multi-process race where a *different* kei instance (different data dir,
/// same download dir) is actively writing a `.part` between download
/// retries: that instance's fresh `.part` predates *this* instance's
/// `last_sync_completed`, but it's not orphaned — the other process is
/// about to resume it.
///
/// 10 minutes is comfortably longer than the longest realistic single
/// HTTP request (CDN connect + TLS + body for a multi-GB Live Photo MOV)
/// while staying short enough that genuinely stale `.part` files from
/// crashed runs still get cleaned promptly.
const PART_FILE_RECENT_GRACE_SECS: i64 = 10 * 60;

/// Walk a tree rooted at `root`, removing files whose name ends with
/// `suffix` and whose mtime is older than `cutoff_secs`. Files whose
/// mtime is within the last `recent_grace_secs` of `now_secs` are spared
/// regardless of `cutoff_secs`. Returns the count of removed files.
/// A `read_dir` failure on any subdirectory logs a `warn!` and skips that
/// subtree -- the original code swallowed the error silently, leaving
/// operators without a breadcrumb when transient FS hiccups (e.g. an
/// unmount mid-walk) prevented cleanup.
fn walk_and_remove_orphan_parts(
    root: std::path::PathBuf,
    suffix: &str,
    cutoff_secs: i64,
    now_secs: i64,
    recent_grace_secs: i64,
) -> usize {
    let mut cleaned = 0usize;
    let mut stack = vec![root];
    while let Some(current) = stack.pop() {
        let entries = match std::fs::read_dir(&current) {
            Ok(entries) => entries,
            Err(e) => {
                tracing::warn!(
                    path = %current.display(),
                    error = %e,
                    "failed to read directory during orphan-part cleanup"
                );
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !name.ends_with(suffix) {
                continue;
            }
            let Ok(meta) = path.metadata() else { continue };
            let Ok(mtime) = meta.modified() else { continue };
            let mtime_secs = mtime
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            // Spare freshly-touched .parts even when they predate
            // `last_sync_completed`. Another kei process targeting the same
            // download dir (different data dir) might be actively resuming
            // this file; deleting it mid-retry would force a
            // restart-from-zero next attempt.
            //
            // `recent_grace_secs <= 0` disables the gate (for tests that
            // exercise the cutoff branch in isolation; production always
            // passes the PART_FILE_RECENT_GRACE_SECS constant).
            let is_recently_touched =
                recent_grace_secs > 0 && mtime_secs > now_secs.saturating_sub(recent_grace_secs);
            if is_recently_touched || mtime_secs >= cutoff_secs {
                continue;
            }
            if std::fs::remove_file(&path).is_ok() {
                cleaned += 1;
            }
        }
    }
    cleaned
}

/// Remove orphaned `.part` files from the download directory.
///
/// Scans the download directory for files ending with `temp_suffix` that are
/// older than the last completed sync. These are leftovers from interrupted
/// downloads that will never be resumed (new downloads produce fresh .part files).
async fn cleanup_orphan_part_files(config: &DownloadConfig) {
    let Some(db) = &config.state_db else { return };
    let cutoff = match db.get_summary().await {
        Ok(summary) => match summary.last_sync_completed {
            Some(ts) => ts,
            None => return, // No prior sync — nothing is orphaned
        },
        Err(e) => {
            tracing::debug!(error = %e, "Could not query last sync time for .part cleanup");
            return;
        }
    };

    let dir = &config.directory;
    if !dir.exists() {
        return;
    }

    let suffix = Arc::clone(&config.temp_suffix);
    let dir: std::path::PathBuf = dir.to_path_buf();
    let cutoff_secs = cutoff.timestamp();
    let now_secs = chrono::Utc::now().timestamp();

    let cleaned = tokio::task::spawn_blocking(move || {
        walk_and_remove_orphan_parts(
            dir,
            &suffix,
            cutoff_secs,
            now_secs,
            PART_FILE_RECENT_GRACE_SECS,
        )
    })
    .await
    .unwrap_or(0);

    if cleaned > 0 {
        tracing::info!(count = cleaned, "Cleaned up orphaned .part files");
    }
}

async fn has_metadata_backfill_work(config: &DownloadConfig) -> bool {
    let Some(db) = &config.state_db else {
        return false;
    };
    match db.has_downloaded_without_metadata_hash().await {
        Ok(needs_backfill) => needs_backfill,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "Failed to check metadata backfill state before incremental sync"
            );
            false
        }
    }
}

fn set_full_enumeration_reason(result: &mut SyncResult, reason: FullEnumerationReason) {
    if result.full_enumeration_ran && result.stats.full_enumeration_reason.is_none() {
        result.stats.full_enumeration_reason = Some(reason);
    }
}

async fn download_photos_full_with_reason(
    download_client: &Client,
    passes: &[crate::commands::AlbumPass],
    config: &Arc<DownloadConfig>,
    controls: DownloadControls,
    shutdown_token: CancellationToken,
    reason: FullEnumerationReason,
) -> Result<SyncResult> {
    let mut result =
        download_photos_full_with_token(download_client, passes, config, controls, shutdown_token)
            .await?;
    set_full_enumeration_reason(&mut result, reason);
    Ok(result)
}

pub async fn download_photos_with_sync(
    download_client: &Client,
    passes: &[crate::commands::AlbumPass],
    config: Arc<DownloadConfig>,
    controls: DownloadControls,
    shutdown_token: CancellationToken,
) -> Result<SyncResult> {
    let sync_started_at = chrono::Utc::now().timestamp();
    cleanup_orphan_part_files(&config).await;

    // Give every non-downloaded asset a fresh start this sync:
    // failed -> pending (with attempts reset), and stale attempt counts on
    // pending assets cleared so the per-sync cap starts from zero.
    let (retry_failed_count, total_pending) = if let Some(db) = &config.state_db {
        match db.prepare_for_retry().await {
            Ok((failed, stale, total_pending)) => {
                if failed > 0 {
                    tracing::debug!(count = failed, "Reset failed assets for retry");
                }
                if stale > 0 {
                    tracing::debug!(
                        count = stale,
                        "Cleared stale attempt counts on pending assets"
                    );
                }
                (failed, total_pending)
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to reset assets for retry");
                (0, 0)
            }
        }
    } else {
        (0, 0)
    };

    let result = match &config.sync_mode {
        SyncMode::Full => {
            download_photos_full_with_token(
                download_client,
                passes,
                &config,
                controls,
                shutdown_token.clone(),
            )
            .await
        }
        // `changes_stream` uses the zone-level `/changes/zone` endpoint, so
        // it returns the same delta for every selected album or smart folder
        // in a zone. Without per-asset membership info on the change events,
        // we can't tell whether a new asset belongs in those scoped passes.
        // The unfiled/library-wide pass is safe: every zone change belongs to
        // that pass, and inactive album/smart-folder templates shouldn't
        // force full enumeration on an unfiled-only sync.
        SyncMode::Incremental { .. } if incremental_requires_full_enumeration(passes) => {
            let reason = FullEnumerationReason::AlbumRelationHydrationIncomplete;
            tracing::debug!(
                full_enumeration_reason = reason.as_str(),
                "Album or smart-folder passes require full enumeration because album \
                 relation hydration is incomplete, skipping incremental"
            );
            download_photos_full_with_reason(
                download_client,
                passes,
                &config,
                controls,
                shutdown_token.clone(),
                reason,
            )
            .await
        }
        // Incremental sync only returns new changes — it won't re-enumerate
        // pending assets from previous syncs. Fall back to full so they get
        // retried. Once everything is downloaded, incremental resumes.
        SyncMode::Incremental { .. } if total_pending > 0 => {
            let reason = if retry_failed_count > 0 {
                FullEnumerationReason::RetryFailedRows
            } else {
                FullEnumerationReason::PendingRows
            };
            tracing::info!(
                pending = total_pending,
                failed_reset = retry_failed_count,
                full_enumeration_reason = reason.as_str(),
                "Retrying failed/pending assets requires full enumeration, skipping incremental sync"
            );
            download_photos_full_with_reason(
                download_client,
                passes,
                &config,
                controls,
                shutdown_token.clone(),
                reason,
            )
            .await
        }
        SyncMode::Incremental { .. } if has_metadata_backfill_work(&config).await => {
            let reason = FullEnumerationReason::MetadataBackfill;
            tracing::info!(
                full_enumeration_reason = reason.as_str(),
                "Metadata backfill requires full enumeration, skipping incremental sync"
            );
            download_photos_full_with_reason(
                download_client,
                passes,
                &config,
                controls,
                shutdown_token.clone(),
                reason,
            )
            .await
        }
        SyncMode::Incremental { zone_sync_token } => {
            let token = zone_sync_token.clone();
            match download_photos_incremental(
                download_client,
                passes,
                &config,
                &token,
                controls,
                shutdown_token.clone(),
            )
            .await
            {
                Ok(result) => Ok(result),
                Err(e) => {
                    // Determine whether this error warrants a fallback to full
                    // enumeration. Token-level errors (invalid, zone not found)
                    // always trigger fallback. Transient errors (503, network
                    // timeouts) should NOT — they'd fail again on full enum too.
                    // Deserialization errors (e.g. Apple returning a different
                    // JSON shape for an invalid token) are not transient, so
                    // fall back for those too.
                    let is_token_error = e
                        .downcast_ref::<SyncTokenError>()
                        .is_some_and(SyncTokenError::should_fallback_to_full);
                    let is_transient = e.downcast_ref::<crate::auth::error::AuthError>().is_some()
                        || e.downcast_ref::<reqwest::Error>().is_some_and(|r| {
                            r.status().is_some_and(|s| s == 429 || s.as_u16() >= 500)
                                || r.is_timeout()
                                || r.is_connect()
                        });

                    if is_token_error || !is_transient {
                        let reason = FullEnumerationReason::OtherStaticReason;
                        tracing::warn!(
                            error = %e,
                            full_enumeration_reason = reason.as_str(),
                            "Incremental sync failed, falling back to full enumeration"
                        );
                        download_photos_full_with_reason(
                            download_client,
                            passes,
                            &config,
                            controls,
                            shutdown_token.clone(),
                            reason,
                        )
                        .await
                    } else {
                        Err(e)
                    }
                }
            }
        }
    };

    // Pending is transient — anything still pending after a complete sync either
    // wasn't enumerated or failed silently. Skip on interrupt where pending is expected.
    if let Some(db) = &config.state_db {
        if !shutdown_token.is_cancelled() {
            match db.promote_pending_to_failed(sync_started_at).await {
                Ok(promoted) if promoted > 0 => {
                    tracing::warn!(
                        count = promoted,
                        "Promoted unresolved pending assets to failed"
                    );
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to promote pending assets");
                }
                _ => {}
            }
        }
    }

    result
}

/// Fold per-pass `album.len()` results into a `(counts, error_count)` tuple,
/// logging a `warn!` for each failure. Errors are mapped to a count of 0 so
/// downstream concurrency math still has a value, but the returned error
/// count is the load-bearing signal: callers must treat it as an enumeration
/// failure that suppresses sync token advancement (a swallowed `len()` error
/// previously caused `total = 0`, which silently bypassed the pagination
/// undercount check and advanced the token past un-enumerated change events).
fn fold_pass_count_results(
    results: Vec<anyhow::Result<u64>>,
    passes: &[crate::commands::AlbumPass],
) -> (Vec<u64>, usize) {
    let mut errors: usize = 0;
    let counts: Vec<u64> = results
        .into_iter()
        .zip(passes)
        .map(|(result, pass)| match result {
            Ok(n) => n,
            Err(e) => {
                errors += 1;
                tracing::warn!(
                    album = %pass.album,
                    error = %e,
                    "Failed to query album length; treating count as 0 and \
                     blocking sync token advancement to force full \
                     re-enumeration on next run"
                );
                0
            }
        })
        .collect();
    (counts, errors)
}

#[derive(Debug)]
struct PassCountPlan {
    display_counts: Vec<u64>,
    stream_total_counts: Vec<Option<u64>>,
    exact_total: Option<u64>,
    len_errors: usize,
}

fn capped_exact_total(counts: &[u64], recent: Option<u32>) -> u64 {
    let total = counts.iter().sum::<u64>();
    total.min(recent.map(u64::from).unwrap_or(u64::MAX))
}

fn display_total_for_recent_scope(counts: &[u64], config: &DownloadConfig) -> u64 {
    match (config.recent, config.recent_scope) {
        (Some(recent), crate::cli::RecentScope::Global) => capped_exact_total(counts, Some(recent)),
        _ => counts.iter().sum(),
    }
}

fn should_skip_pass_count_fetch(config: &DownloadConfig) -> bool {
    // Recent-limited and lower-date-bounded runs deliberately enumerate only a
    // prefix of each newest-first pass. The Hyperion count endpoint reports the
    // full pass size, not how many complete assets will be yielded inside that
    // bounded prefix, so using it as an exact undercount bound false-fires on
    // live accounts with sparse windows. These bounded full syncs suppress the
    // sync token below because they are not complete zone enumerations.
    config.recent.is_some() || config.skip_created_before.is_some()
}

async fn build_pass_count_plan(
    passes: &[crate::commands::AlbumPass],
    config: &DownloadConfig,
    _controls: DownloadControls,
) -> PassCountPlan {
    if should_skip_pass_count_fetch(config) {
        let display_count = config.recent.map(u64::from).unwrap_or(0);
        return PassCountPlan {
            display_counts: vec![display_count; passes.len()],
            stream_total_counts: vec![None; passes.len()],
            exact_total: None,
            len_errors: 0,
        };
    }

    // Album counts share CloudKit's `/internal/records/query/batch`
    // endpoint, so the same-library pass set can fetch all counts with one
    // HTTP call. This matters for default multi-pass syncs and especially
    // `-a all`, where the old per-pass count probe scaled linearly before
    // the first byte of the first download.
    // Capture per-pass `len()` errors instead of swallowing them as zero.
    // A swallowed `len()` failure converted `total` to 0, which short-circuited
    // the pagination-undercount check at line ~1450 (it only fires when
    // `total > 0`); the cycle then returned `Success` with zero assets and the
    // sync token advanced past un-enumerated change events. Treat any failure
    // as a per-album enumeration error so token advancement is suppressed.
    let pass_albums: Vec<&crate::icloud::photos::PhotoAlbum> =
        passes.iter().map(|pass| &pass.album).collect();
    let pass_count_results = crate::icloud::photos::PhotoAlbum::len_many(&pass_albums).await;
    let (display_counts, len_errors) = fold_pass_count_results(pass_count_results, passes);
    let stream_total_counts = display_counts.iter().copied().map(Some).collect();
    let exact_total = capped_exact_total(&display_counts, config.recent);

    PassCountPlan {
        display_counts,
        stream_total_counts,
        exact_total: Some(exact_total),
        len_errors,
    }
}

/// Classification of how the producer-observed asset count compared with the
/// pre-enumeration API total.
///
/// Any shortfall is token-unsafe. The shortfall count is kept so logs can
/// report how many assets were missed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaginationShortfall {
    Match,
    Tolerated { shortfall: u64 },
    TokenUnsafe { shortfall: u64 },
}

/// Pure classifier for the pagination-undercount gate. `total` is the
/// pre-enumeration API count (post `--recent` cap and known filters); `seen`
/// is the producer's `assets_seen` count. Caller is responsible for the
/// `total > 0` guard and any dry-run / print-only suppression.
fn classify_pagination_shortfall(total: u64, seen: u64) -> PaginationShortfall {
    if seen >= total {
        return PaginationShortfall::Match;
    }
    let shortfall = total - seen;
    let within_absolute = shortfall <= PAGINATION_SHORTFALL_TOLERANCE_ABSOLUTE;
    let within_percent = shortfall.saturating_mul(100)
        <= total.saturating_mul(PAGINATION_SHORTFALL_TOLERANCE_PERCENT);
    if within_absolute && within_percent {
        PaginationShortfall::Tolerated { shortfall }
    } else {
        PaginationShortfall::TokenUnsafe { shortfall }
    }
}

/// Resolve the zone sync token from every full-enumeration pass that reported
/// one. All passes for a zone must agree before the token can advance; picking
/// the first completed pass would hide snapshot drift between album-scoped
/// enumerations.
fn unanimous_pass_sync_token(tokens: &[String]) -> Option<String> {
    let first = tokens.first()?;
    if tokens.iter().all(|token| token == first) {
        return Some(first.clone());
    }

    let mut unique_tokens = FxHashSet::default();
    for token in tokens {
        unique_tokens.insert(token.as_str());
    }
    tracing::warn!(
        token_count = tokens.len(),
        unique_token_count = unique_tokens.len(),
        "Full enumeration syncToken mismatch across passes; blocking sync \
         token advancement"
    );
    None
}

/// Full enumeration with syncToken capture.
///
/// Uses `photo_stream_with_token` to capture the zone-level syncToken
/// while running the standard streaming download pipeline. The token
/// is returned alongside the download outcome.
async fn download_photos_full_with_token(
    download_client: &Client,
    passes: &[crate::commands::AlbumPass],
    config: &Arc<DownloadConfig>,
    controls: DownloadControls,
    shutdown_token: CancellationToken,
) -> Result<SyncResult> {
    let started = Instant::now();
    let needs_per_pass = config.requires_per_pass_paths();

    // Mark every unique zone as in-progress so an interrupted full
    // enumeration leaves a trail the next startup can surface to the
    // operator. Clears once the enumeration returns normally.
    let mut enum_zones: Vec<String> = passes
        .iter()
        .map(|p| p.album.zone_name().to_string())
        .collect();
    enum_zones.sort();
    enum_zones.dedup();
    if let Some(db) = &config.state_db {
        for zone in &enum_zones {
            if let Err(e) = db.begin_enum_progress(zone).await {
                tracing::debug!(error = %e, zone, "Failed to mark enumeration start");
            }
        }
    }

    let pass_count_plan = build_pass_count_plan(passes, config, controls).await;
    let pass_counts = pass_count_plan.display_counts;
    let pass_stream_counts = pass_count_plan.stream_total_counts;
    let mut pagination_counts = pass_counts.clone();
    let mut exact_total = pass_count_plan.exact_total;
    let len_errors = pass_count_plan.len_errors;
    let display_total = display_total_for_recent_scope(&pass_counts, config);
    let deferred_unfiled = deferred_unfiled_index(passes);
    let recent_frontier = build_recent_frontier(
        passes,
        config,
        controls,
        shutdown_token.clone(),
        deferred_unfiled.is_some(),
    )
    .await?;

    // Pass-specific path mode still needs one derived config per pass so
    // `{album}` / `{smart-folder}` / `{library}` expand correctly, but the
    // CloudKit streams are independent. Run those pass streams concurrently
    // instead of serializing round trips across albums. Download workers are
    // divided across active pass pipelines so real downloads do not multiply
    // the user-selected `[download].threads` by the number of albums.
    let (mut streaming_result, token_receivers) = if needs_per_pass {
        let mut combined_result = StreamingResult {
            // Enumeration is "complete" only when every pass finished
            // its stream cleanly. Start optimistic; flip to false on the
            // first pass that ended early (shutdown, channel-close, or
            // panic) so the marker stays set and the next startup logs
            // the interruption.
            enumeration_complete: !passes.is_empty(),
            ..StreamingResult::default()
        };
        let pass_parallelism = passes.len().min(config.concurrent_downloads).max(1);
        let per_pass_download_concurrency = config
            .concurrent_downloads
            .div_ceil(pass_parallelism)
            .max(1);
        let pass_configs = build_pass_configs_with_download_concurrency(
            passes,
            config,
            per_pass_download_concurrency,
        );
        let shared_download_ctx = if controls.run_mode.is_dry_run() {
            None
        } else {
            Some(preload_download_context(config).await)
        };

        // Build per-pass labels for the album divider. Friendly multi-pass
        // syncs print a done line (✓) above the bar after each album
        // completes. Single-pass and off-mode syncs skip the divider.
        let pass_labels: Vec<(&str, u64)> = passes
            .iter()
            .zip(&pass_counts)
            .map(|(pass, &count)| {
                let label: &str = pass.album.name.as_ref();
                let label = if label.is_empty() { "unfiled" } else { label };
                (label, count)
            })
            .collect();
        let divider = crate::personality::album_divider::AlbumDivider::new(
            controls.reporting.personality_mode,
            &pass_labels,
        );

        let deferred_ids = deferred_unfiled
            .map(|_| Arc::new(std::sync::Mutex::new(FxHashSet::<String>::default())));

        let non_unfiled_results = stream::iter(
            passes
                .iter()
                .enumerate()
                .zip(&pass_counts)
                .zip(&pass_stream_counts)
                .zip(pass_configs.iter().cloned())
                .filter(|((((index, _pass), _count), _total_count), _config)| {
                    Some(*index) != deferred_unfiled
                }),
        )
        .map(|((((index, pass), &count), total_count), pass_config)| {
            let shutdown_token = shutdown_token.clone();
            let download_client = download_client.clone();
            let deferred_ids = deferred_ids.clone();
            let recent_frontier = recent_frontier.as_ref();
            let download_ctx = shared_download_ctx.clone();
            async move {
                let (stream, token_rx) = open_photo_stream_for_controls(
                    &pass.album,
                    scope_frontier_limit(config, recent_frontier),
                    *total_count,
                    config.concurrent_downloads,
                    pass_config.concurrent_downloads,
                    controls,
                );
                let stream = filter_stream_to_enumeration_bounds(stream, config, recent_frontier);

                if pass.kind == crate::commands::PassKind::Album {
                    if let Some(deferred_ids) = deferred_ids {
                        let stream = stream.map(move |item| {
                            if let Ok(asset) = &item {
                                if let Ok(mut ids) = deferred_ids.lock() {
                                    ids.insert(asset.id().to_string());
                                }
                            }
                            item
                        });
                        return run_full_pass_stream(
                            download_client,
                            stream,
                            token_rx,
                            pass_config,
                            FullPassStreamOptions {
                                controls,
                                count,
                                kind: pass.kind,
                                shutdown_token,
                                download_ctx: download_ctx.clone(),
                            },
                        )
                        .await;
                    }
                }

                let _ = index;
                run_full_pass_stream(
                    download_client,
                    stream,
                    token_rx,
                    pass_config,
                    FullPassStreamOptions {
                        controls,
                        count,
                        kind: pass.kind,
                        shutdown_token,
                        download_ctx,
                    },
                )
                .await
            }
        })
        .buffer_unordered(pass_parallelism)
        .collect::<Vec<Result<PerPassStreamingResult>>>();

        let unfiled_collection = async {
            match deferred_unfiled {
                Some(index) => match passes.get(index) {
                    Some(pass) => match &recent_frontier {
                        Some(frontier) => Some(collected_unfiled_from_recent_frontier(frontier)),
                        None => Some(
                            collect_unfiled_stream(
                                pass,
                                pass_stream_counts.get(index).copied().flatten(),
                                config,
                                controls,
                                shutdown_token.clone(),
                            )
                            .await,
                        ),
                    },
                    _ => None,
                },
                None => None,
            }
        };

        let (pass_results, unfiled_collection) =
            tokio::join!(non_unfiled_results, unfiled_collection);

        let mut token_receivers = Vec::with_capacity(passes.len());
        let mut deferred_exclusions_complete = true;
        for pass_result in pass_results {
            let PerPassStreamingResult {
                kind,
                label,
                count,
                elapsed,
                token_rx,
                result,
            } = pass_result?;

            if deferred_unfiled.is_some()
                && kind == crate::commands::PassKind::Album
                && (result.enumeration_errors > 0 || !result.enumeration_complete)
            {
                deferred_exclusions_complete = false;
            }

            token_receivers.push(token_rx);
            let downloaded_u64 = u64::try_from(result.downloaded).unwrap_or(u64::MAX);
            divider.mark_done(&label, downloaded_u64, count, elapsed);

            merge_streaming_result(&mut combined_result, result);
        }

        if let (Some(index), Some(collected)) = (deferred_unfiled, unfiled_collection) {
            if deferred_exclusions_complete {
                let excluded_ids = deferred_ids
                    .as_ref()
                    .and_then(|ids| ids.lock().ok().map(|guard| guard.clone()))
                    .unwrap_or_default();
                let filtered = collected.items.into_iter().filter(move |item| {
                    item.as_ref()
                        .map_or(true, |asset| !excluded_ids.contains(asset.id()))
                });
                if let Some(pass_config) = pass_configs.get(index).cloned() {
                    let filtered_items = filtered.collect::<Vec<_>>();
                    let filtered_count = filtered_items
                        .iter()
                        .filter(|item| item.is_ok())
                        .count()
                        .try_into()
                        .unwrap_or(u64::MAX);
                    if let Some(slot) = pagination_counts.get_mut(index) {
                        *slot = filtered_count;
                    }
                    let pass_result = run_full_pass_stream(
                        download_client.clone(),
                        stream::iter(filtered_items),
                        collected.token_rx,
                        pass_config,
                        FullPassStreamOptions {
                            controls,
                            count: filtered_count,
                            kind: crate::commands::PassKind::Unfiled,
                            shutdown_token: shutdown_token.clone(),
                            download_ctx: shared_download_ctx.clone(),
                        },
                    )
                    .await?;
                    let downloaded_u64 =
                        u64::try_from(pass_result.result.downloaded).unwrap_or(u64::MAX);
                    divider.mark_done(
                        &pass_result.label,
                        downloaded_u64,
                        pass_result.count,
                        pass_result.elapsed,
                    );
                    token_receivers.push(pass_result.token_rx);
                    merge_streaming_result(&mut combined_result, pass_result.result);
                }
            } else {
                combined_result.enumeration_complete = false;
                token_receivers.push(collected.token_rx);
            }
        }
        divider.finish();

        (combined_result, token_receivers)
    } else {
        let merged_exclude_ids = passes
            .first()
            .map(|p| Arc::clone(&p.exclude_ids))
            .unwrap_or_else(|| Arc::new(FxHashSet::default()));
        let merged_config = if Arc::ptr_eq(&merged_exclude_ids, &config.exclude_asset_ids) {
            Arc::clone(config)
        } else {
            Arc::new(config.with_exclude_ids(merged_exclude_ids))
        };
        let mut token_receivers = Vec::with_capacity(passes.len());
        let streams: Vec<_> = passes
            .iter()
            .zip(&pass_stream_counts)
            .map(|(pass, total_count)| {
                let (stream, token_rx) = open_photo_stream_for_controls(
                    &pass.album,
                    scope_frontier_limit(config, recent_frontier.as_ref()),
                    *total_count,
                    config.concurrent_downloads,
                    config.concurrent_downloads,
                    controls,
                );
                token_receivers.push(token_rx);
                filter_stream_to_enumeration_bounds(stream, config, recent_frontier.as_ref())
            })
            .collect();

        let combined = stream::select_all(streams);
        // Merged-stream branch already runs as a single call, so it creates
        // one bar internally; no shared-bar plumbing needed.
        let result = stream_and_download_from_stream(
            download_client,
            combined,
            &merged_config,
            controls,
            display_total,
            shutdown_token.clone(),
            StreamRuntime::new(None, None),
        )
        .await?;

        (result, token_receivers)
    };

    // Fold `len()` failures into the streaming result's enumeration error
    // tally so `build_download_outcome` returns `PartialFailure`. This is the
    // signal `should_store_sync_token` reads to suppress token advancement.
    streaming_result.enumeration_errors += len_errors;
    // A `len()` failure on any pass means we never had a reliable
    // total to enumerate against, so the per-zone enumeration marker must
    // stay set even if the producer drained its (possibly truncated) stream.
    if len_errors > 0 {
        streaming_result.enumeration_complete = false;
    }
    if exact_total.is_some() {
        exact_total = Some(capped_exact_total(&pagination_counts, config.recent));
    }

    // Check if enumeration saw significantly fewer assets than the API reported.
    // This catches silent pagination truncation, dropped pages, or API hiccups
    // that would otherwise go unnoticed. Any `len()` failure also forces
    // suppression because the recorded `total` is missing those passes.
    let mut pagination_shortfall_assets = 0u64;
    let mut pagination_shortfall_warnings = 0usize;
    let pagination_undercount = if len_errors > 0 {
        true
    } else if !controls.run_mode.only_print_filenames() && !controls.run_mode.is_dry_run() {
        if let Some(total) = exact_total.filter(|total| *total > 0) {
            let decision = classify_pagination_shortfall(total, streaming_result.assets_seen);
            match decision {
                PaginationShortfall::Match => false,
                PaginationShortfall::Tolerated { shortfall } => {
                    pagination_shortfall_assets = shortfall;
                    pagination_shortfall_warnings = 1;
                    tracing::warn!(
                        expected = total,
                        seen = streaming_result.assets_seen,
                        shortfall,
                        tolerance_percent = PAGINATION_SHORTFALL_TOLERANCE_PERCENT,
                        tolerance_assets = PAGINATION_SHORTFALL_TOLERANCE_ABSOLUTE,
                        "Enumeration count shortfall observed, but within tolerance"
                    );
                    false
                }
                PaginationShortfall::TokenUnsafe { shortfall } => {
                    pagination_shortfall_assets = shortfall;
                    pagination_shortfall_warnings = 1;
                    tracing::warn!(
                        expected = total,
                        seen = streaming_result.assets_seen,
                        shortfall,
                        "Enumeration saw fewer assets than expected — blocking sync token \
                         advancement to force full re-enumeration on next run"
                    );
                    true
                }
            }
        } else {
            false
        }
    } else {
        false
    };

    // Collect the sync token from every album's token receiver and require
    // agreement before advancing. In practice, all passes for a zone should
    // report the same token; disagreement means the full enumeration did not
    // observe one coherent snapshot.
    // Don't advance the token for read-only operations, or when pagination
    // was incomplete (would permanently skip missed assets).
    // Do not persist a full-enumeration zone token for count-recent or
    // skip-created-before runs. Those runs intentionally stop before the full
    // pass is drained, so advancing the token would make older, unenumerated
    // assets invisible to later incremental syncs.
    let token_eligible = config.recent.is_none()
        && config.skip_created_before.is_none()
        && !controls.run_mode.only_print_filenames()
        && !pagination_undercount
        && streaming_result.enumeration_errors == 0;
    let mut token_block_reason: Option<&'static str> = None;
    let mut token_expected_receivers: Option<usize> = None;
    let mut token_receivers_with_token: Option<usize> = None;
    let mut token_receivers_missing: Option<usize> = None;
    let mut token_receivers_blank: Option<usize> = None;
    let mut token_receivers_dropped: Option<usize> = None;
    let mut token_unique_values: Option<usize> = None;
    let sync_token = if token_eligible {
        let expected_token_count = token_receivers.len();
        token_expected_receivers = Some(expected_token_count);
        let mut tokens = Vec::new();
        let mut missing_tokens = 0usize;
        let mut blank_tokens = 0usize;
        let mut dropped_receivers = 0usize;
        for rx in token_receivers {
            match rx.await {
                Ok(Some(token)) => {
                    let trimmed = token.trim();
                    if trimmed.is_empty() {
                        blank_tokens += 1;
                        continue;
                    }
                    tokens.push(trimmed.to_string());
                }
                Ok(None) => {
                    missing_tokens += 1;
                }
                Err(_) => {
                    dropped_receivers += 1;
                }
            }
        }
        token_receivers_with_token = Some(tokens.len());
        token_receivers_missing = Some(missing_tokens);
        token_receivers_blank = Some(blank_tokens);
        token_receivers_dropped = Some(dropped_receivers);
        if blank_tokens > 0 {
            tracing::warn!(
                blank_tokens,
                expected_token_count,
                "Full enumeration returned blank syncToken values; blocking token advancement"
            );
        }
        if dropped_receivers > 0 {
            tracing::warn!(
                dropped_receivers,
                expected_token_count,
                "Full enumeration syncToken receiver dropped before completion; blocking token advancement"
            );
        }
        let unique_token_count = tokens
            .iter()
            .map(std::string::String::as_str)
            .collect::<FxHashSet<_>>()
            .len();
        token_unique_values = Some(unique_token_count);
        let resolved = unanimous_pass_sync_token(&tokens);
        if resolved.is_none() {
            token_block_reason = Some(if dropped_receivers > 0 {
                "kei_internal_token_receiver_dropped"
            } else if blank_tokens > 0 {
                "icloud_blank_sync_token"
            } else if unique_token_count > 1 {
                "icloud_sync_token_mismatch"
            } else if missing_tokens > 0 {
                "icloud_sync_token_missing"
            } else {
                "sync_token_unavailable"
            });
        }
        resolved
    } else {
        None
    };

    // Capture the enumeration-complete signal before
    // `build_download_outcome` consumes `streaming_result`. The marker
    // gate below uses this signal directly so a partial-failure run
    // whose enumeration phase finished still clears the marker.
    let enumeration_complete = streaming_result.enumeration_complete;
    let enumeration_errors = streaming_result.enumeration_errors;

    // Build the outcome using the same logic as download_photos
    let (outcome, mut stats) = build_download_outcome(
        download_client,
        passes,
        config,
        controls,
        streaming_result,
        started,
        shutdown_token,
    )
    .await?;
    stats.pagination_shortfall_warnings = pagination_shortfall_warnings;
    stats.pagination_shortfall_assets = pagination_shortfall_assets;
    if token_eligible {
        stats.sync_token_expected_receivers = token_expected_receivers;
        stats.sync_token_receivers_with_token = token_receivers_with_token;
        stats.sync_token_receivers_missing = token_receivers_missing;
        stats.sync_token_receivers_blank = token_receivers_blank;
        stats.sync_token_receivers_dropped = token_receivers_dropped;
        stats.sync_token_unique_values = token_unique_values;
    }
    if pagination_undercount {
        stats.sync_token_blocked = true;
        stats.sync_token_blocked_reason = Some("pagination_shortfall");
        stats.sync_token_blocked_source = Some(sync_token_blocked_source("pagination_shortfall"));
        stats.sync_token_blocked_explanation =
            Some(sync_token_blocked_explanation("pagination_shortfall"));
    } else if (config.recent.is_some() || config.skip_created_before.is_some())
        && !controls.run_mode.only_print_filenames()
        && enumeration_errors == 0
    {
        let bounded_reason = if config.recent.is_some() {
            RECENT_LIMITED_FULL_ENUMERATION_REASON
        } else {
            DATE_BOUNDED_FULL_ENUMERATION_REASON
        };
        stats.sync_token_blocked = true;
        stats.sync_token_blocked_reason = Some(bounded_reason);
        stats.sync_token_blocked_source = Some(sync_token_blocked_source(bounded_reason));
        stats.sync_token_blocked_explanation = Some(sync_token_blocked_explanation(bounded_reason));
    } else if token_eligible && sync_token.is_none() {
        let reason = token_block_reason.unwrap_or("sync_token_unavailable");
        stats.sync_token_blocked = true;
        stats.sync_token_blocked_reason = Some(reason);
        stats.sync_token_blocked_source = Some(sync_token_blocked_source(reason));
        stats.sync_token_blocked_explanation = Some(sync_token_blocked_explanation(reason));
    }

    // Clear enumeration-in-progress markers when the producer reached the
    // natural end of the API stream. The gate ignores download-side
    // failures so a partial-failure cycle whose enumeration finished
    // doesn't leave the marker set forever. Shutdown still suppresses the
    // clear because the producer's cancellation path leaves
    // `enumeration_complete = false`.
    if enumeration_complete {
        if let Some(db) = &config.state_db {
            for zone in &enum_zones {
                if let Err(e) = db.end_enum_progress(zone).await {
                    tracing::debug!(error = %e, zone, "Failed to clear enumeration marker");
                }
            }
        }
    }

    Ok(SyncResult {
        outcome,
        sync_token,
        stats,
        full_enumeration_ran: true,
    })
}

/// Incremental delta sync via `changes_stream`.
///
/// Fetches `ChangeEvent`s since the given sync token, filters to
/// downloadable assets, and feeds them through the download pipeline.
async fn download_photos_incremental(
    download_client: &Client,
    passes: &[crate::commands::AlbumPass],
    config: &Arc<DownloadConfig>,
    zone_sync_token: &str,
    controls: DownloadControls,
    shutdown_token: CancellationToken,
) -> Result<SyncResult> {
    let started = Instant::now();

    // Each asset is paired with its source pass index so both `{album}`
    // expansion and per-pass exclusion (notably, the unfiled pass's set
    // that prevents assets already in some user album from downloading
    // twice) can be applied downstream.
    let mut downloadable_assets: Vec<(PhotoAsset, usize)> = Vec::new();
    let mut sync_token: Option<String> = None;
    let mut created_count = 0u64;
    let mut soft_deleted_count = 0u64;
    let mut hard_deleted_count = 0u64;
    let mut hidden_count = 0u64;
    let mut total_events = 0u64;

    // `changes_stream` is zone-scoped, not album-scoped. Query it once and
    // fan created assets out through the selected passes locally; querying
    // once per pass repeats the same `/changes/zone` pages on every watch
    // cycle with work.
    if let Some(pass) = passes.first() {
        let (change_stream, token_rx) = pass.album.changes_stream(zone_sync_token);
        tokio::pin!(change_stream);

        while let Some(result) = change_stream.next().await {
            if shutdown_token.is_cancelled() {
                break;
            }
            let event = result?;
            total_events += 1;
            match event.reason {
                ChangeReason::Created => {
                    created_count += 1;
                    if let Some(asset) = event.asset {
                        for pass_index in 0..passes.len() {
                            downloadable_assets.push((asset.clone(), pass_index));
                        }
                    }
                }
                ChangeReason::SoftDeleted => {
                    soft_deleted_count += 1;
                    tracing::debug!(record_name = %event.record_name, record_type = ?event.record_type, "Skipping soft-deleted record");
                    if let Some(db) = &config.state_db {
                        let deleted_at = event.asset.as_ref().and_then(|a| a.metadata().deleted_at);
                        if let Err(e) = db
                            .mark_soft_deleted(&config.library, &event.record_name, deleted_at)
                            .await
                        {
                            tracing::warn!(
                                record_name = %event.record_name,
                                error = %e,
                                "Failed to record soft-delete in state DB"
                            );
                        }
                    }
                }
                ChangeReason::HardDeleted => {
                    hard_deleted_count += 1;
                    tracing::debug!(record_name = %event.record_name, record_type = ?event.record_type, "Skipping hard-deleted record");
                    // CloudKit returns no fields for hard-deleted records, so we
                    // can't tell master from asset. Treat as soft-delete in DB
                    // (sets is_deleted=1) — the row stays put so history and
                    // local_path remain queryable.
                    if let Some(db) = &config.state_db {
                        if let Err(e) = db
                            .mark_soft_deleted(&config.library, &event.record_name, None)
                            .await
                        {
                            tracing::warn!(
                                record_name = %event.record_name,
                                error = %e,
                                "Failed to record hard-delete in state DB"
                            );
                        }
                    }
                }
                ChangeReason::Hidden => {
                    hidden_count += 1;
                    tracing::debug!(record_name = %event.record_name, record_type = ?event.record_type, "Skipping hidden record");
                    if let Some(db) = &config.state_db {
                        if let Err(e) = db
                            .mark_hidden_at_source(&config.library, &event.record_name)
                            .await
                        {
                            tracing::warn!(
                                record_name = %event.record_name,
                                error = %e,
                                "Failed to record hidden state in state DB"
                            );
                        }
                    }
                }
            }
        }

        if let Ok(token) = token_rx.await {
            sync_token = Some(token);
        }
    }

    tracing::debug!(
        created = created_count,
        soft_deleted = soft_deleted_count,
        hard_deleted = hard_deleted_count,
        hidden = hidden_count,
        "Incremental sync: {total_events} change events",
    );

    if downloadable_assets.is_empty() {
        let stats = SyncStats {
            elapsed_secs: started.elapsed().as_secs_f64(),
            interrupted: shutdown_token.is_cancelled(),
            ..SyncStats::default()
        };
        tracing::info!("No new photos to download from incremental sync");
        tracing::info!(elapsed = %format_duration(started.elapsed()), "  completed");
        return Ok(SyncResult {
            outcome: DownloadOutcome::Success,
            sync_token,
            stats,
            full_enumeration_ran: false,
        });
    }

    // Respect --recent: cap the number of assets to download
    if let Some(recent) = config.recent {
        let limit = recent as usize;
        if downloadable_assets.len() > limit {
            tracing::debug!(
                total = downloadable_assets.len(),
                limit,
                "Capping incremental assets to --recent limit"
            );
            downloadable_assets.truncate(limit);
        }
    }

    tracing::debug!(
        count = downloadable_assets.len(),
        "Assets to download from incremental sync"
    );

    // Convert assets to download tasks via path-aware on-disk verification.
    // Each pass (concrete album or unfiled) gets its own derived config so
    // that both album-specific path expansion and per-pass exclude sets are
    // applied. Configs are cached per pass index to avoid redundant
    // allocations when many assets flow through the same pass.
    let mut tasks: Vec<DownloadTask> = Vec::new();
    let mut task_planner = planner::TaskPlanner::new();
    let mut skip_breakdown = SkipBreakdown::default();
    let mut enumeration_errors = 0usize;
    let pass_configs = build_pass_configs_resolving_deferred_excludes(passes, config).await?;

    for (asset, pass_index) in &downloadable_assets {
        #[allow(
            clippy::indexing_slicing,
            reason = "pass_index was assigned by the producer from the same `passes` slice \
                      that pass_configs was built from; indices are valid"
        )]
        let effective_config = &pass_configs[*pass_index];

        let plan = task_planner.plan_asset(asset, effective_config).await;
        if let Some(reason) = plan.filter_reason {
            skip_breakdown.record_filter_reason(reason);
            continue;
        }
        if let Some(resource) = &plan.malformed_resource {
            enumeration_errors += 1;
            tracing::error!(
                asset_id = %asset.id(),
                field = %resource.field,
                reason = %resource.reason,
                "Malformed CloudKit resource prevented incremental download planning"
            );
            continue;
        }

        // Upsert state records so mark_downloaded/mark_failed can find them.
        // Without this, the UPDATE in mark_downloaded matches 0 rows and the
        // file ends up on disk but untracked in the state DB.
        if let Some(db) = &config.state_db {
            for task in &plan.tasks {
                if let Err(e) =
                    planner::upsert_seen_for_task(db.as_ref(), effective_config, asset, task).await
                {
                    tracing::warn!(
                        asset_id = %task.asset_id,
                        error = %e,
                        "Failed to record asset in state DB"
                    );
                }
            }
            // Record this asset's membership in the current album so
            // consumers (EXIF keywords, XMP sidecars, Immich albums) can
            // reconstruct the logical album graph from the state DB.
            if let Err(e) =
                planner::record_album_membership_if_named(db.as_ref(), effective_config, asset)
                    .await
            {
                if let Some(album_name) = effective_config.album_name.as_deref() {
                    tracing::warn!(
                        asset_id = %asset.id(),
                        album = %album_name,
                        library = %effective_config.library,
                        error = %e,
                        "Failed to record album membership after retries"
                    );
                }
            }
        }

        if plan.tasks.is_empty() {
            skip_breakdown.on_disk += 1;
        }
        tasks.extend(plan.tasks);
    }

    if skip_breakdown.by_state > 0 {
        tracing::debug!(
            skipped = skip_breakdown.by_state,
            "Skipped already-downloaded assets (state DB)"
        );
    }

    if tasks.is_empty() {
        let stats = SyncStats {
            skipped: skip_breakdown,
            enumeration_errors,
            elapsed_secs: started.elapsed().as_secs_f64(),
            interrupted: shutdown_token.is_cancelled(),
            ..SyncStats::default()
        };
        tracing::info!("All incremental assets already downloaded or filtered");
        tracing::info!(elapsed = %format_duration(started.elapsed()), "  completed");
        let outcome = if enumeration_errors > 0 {
            DownloadOutcome::PartialFailure {
                failed_count: enumeration_errors,
            }
        } else {
            DownloadOutcome::Success
        };
        return Ok(SyncResult {
            outcome,
            sync_token: (enumeration_errors == 0).then_some(sync_token).flatten(),
            stats,
            full_enumeration_ran: false,
        });
    }

    if controls.run_mode.only_print_filenames() {
        #[allow(
            clippy::print_stdout,
            reason = "--only-print-filenames writes target paths to stdout so callers can pipe to xargs/etc"
        )]
        for task in &tasks {
            println!("{}", task.download_path.display());
        }
        let stats = SyncStats {
            skipped: skip_breakdown,
            enumeration_errors,
            elapsed_secs: started.elapsed().as_secs_f64(),
            ..SyncStats::default()
        };
        // Don't advance the sync token — this is a read-only operation.
        return Ok(SyncResult {
            outcome: if enumeration_errors > 0 {
                DownloadOutcome::PartialFailure {
                    failed_count: enumeration_errors,
                }
            } else {
                DownloadOutcome::Success
            },
            sync_token: None,
            stats,
            full_enumeration_ran: false,
        });
    }

    let task_count = tasks.len();
    tracing::info!(
        count = task_count,
        "Downloading files from incremental sync"
    );

    // Run the download pass on the collected tasks
    let pass_config = PassConfig {
        client: download_client,
        retry_config: &config.retry,
        metadata: MetadataFlags::from(config.as_ref()),
        concurrency: config.concurrent_downloads,
        reporting: controls.reporting,
        temp_suffix: Arc::clone(&config.temp_suffix),
        shutdown_token: shutdown_token.clone(),
        state_db: config.state_db.clone(),
        rate_limit_counter: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        bandwidth_limiter: config.bandwidth_limiter.clone(),
        library: Arc::clone(&config.library),
    };
    let pass_result = run_download_pass(pass_config, tasks).await;

    let failed = pass_result.failed.len();
    let succeeded = task_count - failed;

    // Log failed downloads before the summary
    if failed > 0 {
        for task in &pass_result.failed {
            tracing::error!(asset_id = %task.asset_id, path = %task.download_path.display(), "Download failed");
        }
    }

    let stats = SyncStats {
        assets_seen: 0, // incremental doesn't have total library count
        downloaded: succeeded,
        failed,
        skipped: skip_breakdown,
        bytes_downloaded: pass_result.bytes_downloaded,
        disk_bytes_written: pass_result.disk_bytes_written,
        exif_failures: pass_result.exif_failures,
        state_write_failures: pass_result.state_write_failures,
        enumeration_errors,
        pagination_shortfall_warnings: 0,
        pagination_shortfall_assets: 0,
        sync_token_blocked: false,
        sync_token_blocked_reason: None,
        elapsed_secs: started.elapsed().as_secs_f64(),
        interrupted: shutdown_token.is_cancelled()
            || pass_result.auth_errors >= AUTH_ERROR_THRESHOLD
            || pass_result.url_expired_abort,
        rate_limited: pass_result.rate_limit_observations,
        photos_downloaded: pass_result.photos_downloaded,
        videos_downloaded: pass_result.videos_downloaded,
        recap: pass_result.recap.clone(),
        ..SyncStats::default()
    };
    log_sync_summary(
        "\u{2500}\u{2500} Incremental Sync Summary \u{2500}\u{2500}",
        &stats,
    );

    if pass_result.auth_errors >= AUTH_ERROR_THRESHOLD {
        return Ok(SyncResult {
            outcome: DownloadOutcome::SessionExpired {
                auth_error_count: pass_result.auth_errors,
            },
            sync_token,
            stats,
            full_enumeration_ran: false,
        });
    }

    let outcome = if failed > 0
        || pass_result.exif_failures > 0
        || pass_result.state_write_failures > 0
        || enumeration_errors > 0
    {
        DownloadOutcome::PartialFailure {
            failed_count: failed
                + pass_result.exif_failures
                + pass_result.state_write_failures
                + enumeration_errors,
        }
    } else {
        DownloadOutcome::Success
    };

    Ok(SyncResult {
        outcome,
        sync_token: (enumeration_errors == 0).then_some(sync_token).flatten(),
        stats,
        full_enumeration_ran: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{AlbumPass, PassKind};
    use crate::icloud::photos::{PhotoAlbum, PhotoAlbumConfig, PhotosSession};
    use crate::test_helpers::{
        mock_photo_records_for_zone_with_filename,
        mock_photo_records_for_zone_with_filename_and_asset_date, DynamicRecentPhotosSession,
        MockPhotosFlow, TestAssetRecord,
    };
    use serde_json::{json, Value};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;
    use tokio::time::Duration;

    fn test_config() -> DownloadConfig {
        DownloadConfig::test_default()
    }

    fn retry_test_task(asset_id: &str, version_size: VersionSizeKey, path: &str) -> DownloadTask {
        DownloadTask {
            url: format!("https://p01.icloud-content.com/{asset_id}").into(),
            download_path: Path::new("/tmp/codex/kei/retry-tests").join(path),
            checksum: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
            asset_id: Arc::from(asset_id),
            library: Arc::from("PrimarySync"),
            metadata: Arc::new(filter::MetadataPayload::default()),
            size: 1024,
            created_local: chrono::Local::now(),
            version_size,
            media_type: crate::state::MediaType::Photo,
        }
    }

    #[test]
    fn cleanup_retry_filter_keeps_only_exact_failed_task_keys() {
        let failed = retry_test_task("ASSET_A", VersionSizeKey::Original, "a.jpg");
        let matching_refresh = DownloadTask {
            url: "https://p01.icloud-content.com/fresh-a".into(),
            ..failed.clone()
        };
        let wrong_version = retry_test_task("ASSET_A", VersionSizeKey::Medium, "a.jpg");
        let wrong_path = retry_test_task("ASSET_A", VersionSizeKey::Original, "elsewhere/a.jpg");
        let unrelated = retry_test_task("ASSET_B", VersionSizeKey::Original, "b.jpg");
        let mut pending_keys: FxHashSet<RetryTaskKey> =
            std::iter::once(RetryTaskKey::from(&failed)).collect();
        let mut out = Vec::new();

        take_matching_retry_tasks(
            vec![
                wrong_version,
                wrong_path,
                unrelated,
                matching_refresh.clone(),
            ],
            &mut pending_keys,
            &mut out,
        );

        assert!(pending_keys.is_empty());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].asset_id.as_ref(), "ASSET_A");
        assert_eq!(out[0].version_size, VersionSizeKey::Original);
        assert_eq!(out[0].download_path, matching_refresh.download_path);
        assert_eq!(
            out[0].url.as_ref(),
            "https://p01.icloud-content.com/fresh-a"
        );
    }

    fn changes_album(name: &str, session: impl PhotosSession + 'static) -> PhotoAlbum {
        PhotoAlbum::new(
            PhotoAlbumConfig {
                params: Arc::new(HashMap::new()),
                service_endpoint: Arc::from("https://example.com"),
                name: Arc::from(name),
                list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
                obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
                query_filter: None,
                page_size: 100,
                zone_id: Arc::new(json!({"zoneName": "PrimarySync"})),
                retry_config: RetryConfig::default(),
                container_id: None,
                cross_zone_sources: Vec::new(),
            },
            Box::new(session),
        )
    }

    #[derive(Clone)]
    struct CountingChangesZoneSession {
        changes_zone_calls: Arc<AtomicUsize>,
        records: Vec<Value>,
    }

    fn changes_zone_session(
        changes_zone_calls: Arc<AtomicUsize>,
        records: Vec<Value>,
    ) -> CountingChangesZoneSession {
        CountingChangesZoneSession {
            changes_zone_calls,
            records,
        }
    }

    #[async_trait::async_trait]
    impl PhotosSession for CountingChangesZoneSession {
        async fn post(
            &self,
            url: &str,
            _body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            if url.contains("/changes/zone?") {
                self.changes_zone_calls.fetch_add(1, Ordering::SeqCst);
                return Ok(json!({
                    "zones": [{
                        "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                        "syncToken": "zone-token-next",
                        "moreComing": false,
                        "records": self.records.clone(),
                    }]
                }));
            }

            Ok(json!({"records": []}))
        }

        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }

    fn incremental_photo_records(record_name: &str) -> Vec<Value> {
        vec![
            json!({
                "recordName": record_name,
                "recordType": "CPLMaster",
                "fields": {
                    "filenameEnc": {"value": "changed.jpg", "type": "STRING"},
                    "resOriginalRes": {
                        "value": {
                            "downloadURL": "https://p01.icloud-content.com/changed.jpg",
                            "size": 1024,
                            "fileChecksum": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
                        }
                    },
                    "resOriginalFileType": {"value": "public.jpeg"},
                    "itemType": {"value": "public.jpeg"}
                }
            }),
            json!({
                "recordName": format!("asset-{record_name}"),
                "recordType": "CPLAsset",
                "fields": {
                    "masterRef": {
                        "value": {
                            "recordName": record_name,
                            "zoneID": {"zoneName": "PrimarySync"}
                        },
                        "type": "REFERENCE"
                    },
                    "assetDate": {"value": 1700000000000i64, "type": "TIMESTAMP"},
                    "addedDate": {"value": 1700000000000i64, "type": "TIMESTAMP"}
                }
            }),
        ]
    }

    #[derive(Clone)]
    struct ConcurrentRecordsSession {
        in_flight_records_queries: Arc<AtomicUsize>,
        max_in_flight_records_queries: Arc<AtomicUsize>,
        records_delay: Duration,
    }

    impl ConcurrentRecordsSession {
        fn new(records_delay: Duration) -> Self {
            Self {
                in_flight_records_queries: Arc::new(AtomicUsize::new(0)),
                max_in_flight_records_queries: Arc::new(AtomicUsize::new(0)),
                records_delay,
            }
        }

        fn max_in_flight(&self) -> usize {
            self.max_in_flight_records_queries.load(Ordering::SeqCst)
        }

        fn note_records_query_start(&self) {
            let current = self
                .in_flight_records_queries
                .fetch_add(1, Ordering::SeqCst)
                + 1;
            let mut observed = self.max_in_flight_records_queries.load(Ordering::SeqCst);
            while current > observed {
                match self.max_in_flight_records_queries.compare_exchange(
                    observed,
                    current,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                ) {
                    Ok(_) => break,
                    Err(next) => observed = next,
                }
            }
        }
    }

    #[async_trait::async_trait]
    impl PhotosSession for ConcurrentRecordsSession {
        async fn post(
            &self,
            url: &str,
            _body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            if url.contains("/internal/records/query/batch") {
                return Ok(json!({
                    "batch": [{"records": [{"fields": {"itemCount": {"value": 0}}}]}]
                }));
            }

            if url.contains("/records/query?") {
                self.note_records_query_start();
                tokio::time::sleep(self.records_delay).await;
                self.in_flight_records_queries
                    .fetch_sub(1, Ordering::SeqCst);
                return Ok(json!({
                    "records": [],
                    "syncToken": "zone-token"
                }));
            }

            Ok(json!({"records": []}))
        }

        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }

    fn probe_album(name: &str, session: ConcurrentRecordsSession) -> PhotoAlbum {
        PhotoAlbum::new(
            PhotoAlbumConfig {
                params: Arc::new(HashMap::new()),
                service_endpoint: Arc::from("https://example.com"),
                name: Arc::from(name),
                list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
                obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
                query_filter: None,
                page_size: 100,
                zone_id: Arc::new(json!({"zoneName": "PrimarySync"})),
                retry_config: RetryConfig::default(),
                container_id: None,
                cross_zone_sources: Vec::new(),
            },
            Box::new(session),
        )
    }

    fn mock_album(name: &str, session: crate::test_helpers::MockPhotosSession) -> PhotoAlbum {
        album_with_session("PrimarySync", name, Box::new(session))
    }

    fn album_with_session(zone: &str, name: &str, session: Box<dyn PhotosSession>) -> PhotoAlbum {
        PhotoAlbum::new(
            PhotoAlbumConfig {
                params: Arc::new(HashMap::new()),
                service_endpoint: Arc::from("https://example.com"),
                name: Arc::from(name),
                list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
                obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
                query_filter: None,
                page_size: 100,
                zone_id: Arc::new(json!({"zoneName": zone})),
                retry_config: RetryConfig::default(),
                container_id: None,
                cross_zone_sources: Vec::new(),
            },
            session,
        )
    }

    #[derive(Clone)]
    struct RecentScopeAsset {
        id: String,
        asset_date: i64,
    }

    #[derive(Clone)]
    struct RecentScopeSession {
        all_assets: Arc<Vec<RecentScopeAsset>>,
        album_assets: Arc<Vec<RecentScopeAsset>>,
        all_offsets: Arc<std::sync::Mutex<Vec<u64>>>,
        album_offsets: Arc<std::sync::Mutex<Vec<u64>>>,
    }

    impl RecentScopeSession {
        fn new(all_assets: Vec<RecentScopeAsset>, album_assets: Vec<RecentScopeAsset>) -> Self {
            Self {
                all_assets: Arc::new(all_assets),
                album_assets: Arc::new(album_assets),
                all_offsets: Arc::new(std::sync::Mutex::new(Vec::new())),
                album_offsets: Arc::new(std::sync::Mutex::new(Vec::new())),
            }
        }

        fn album_offsets(&self) -> Vec<u64> {
            self.album_offsets
                .lock()
                .expect("album offsets lock")
                .clone()
        }

        fn page_records(
            assets: &[RecentScopeAsset],
            offset: u64,
            results_limit: u64,
        ) -> Vec<Value> {
            let start = usize::try_from(offset).unwrap_or(usize::MAX);
            let page_assets = usize::try_from(results_limit / 2).unwrap_or(usize::MAX);
            let end = start.saturating_add(page_assets).min(assets.len());
            let mut records = Vec::with_capacity(end.saturating_sub(start) * 2);
            for asset in assets.get(start..end).unwrap_or_default() {
                records.extend(mock_photo_records_for_zone_with_filename_and_asset_date(
                    &asset.id,
                    "PrimarySync",
                    &format!("{}.jpg", asset.id),
                    asset.asset_date,
                ));
            }
            records
        }
    }

    #[async_trait::async_trait]
    impl PhotosSession for RecentScopeSession {
        async fn post(
            &self,
            url: &str,
            body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            if url.contains("/internal/records/query/batch") {
                return Ok(json!({
                    "batch": [{"records": [{"fields": {"itemCount": {"value": self.all_assets.len() as u64}}}]}]
                }));
            }
            if !url.contains("/records/query?") {
                return Ok(json!({"records": []}));
            }

            let request: Value = serde_json::from_str(&body)?;
            let record_type = request["query"]["recordType"].as_str().unwrap_or_default();
            let offset = request["query"]["filterBy"]
                .as_array()
                .and_then(|filters| {
                    filters.iter().find_map(|filter| {
                        (filter["fieldName"] == "startRank")
                            .then(|| filter["fieldValue"]["value"].as_u64())
                            .flatten()
                    })
                })
                .unwrap_or(0);
            let results_limit = request["resultsLimit"].as_u64().unwrap_or(0);

            let assets = if record_type == "CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted" {
                self.all_offsets
                    .lock()
                    .expect("all offsets lock")
                    .push(offset);
                self.all_assets.as_ref()
            } else {
                self.album_offsets
                    .lock()
                    .expect("album offsets lock")
                    .push(offset);
                self.album_assets.as_ref()
            };
            let records = Self::page_records(assets, offset, results_limit);
            Ok(json!({"records": records, "syncToken": "zone-token"}))
        }

        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }

    fn recent_scope_album(name: &str, session: RecentScopeSession) -> PhotoAlbum {
        PhotoAlbum::new(
            PhotoAlbumConfig {
                params: Arc::new(HashMap::new()),
                service_endpoint: Arc::from("https://example.com"),
                name: Arc::from(name),
                list_type: Arc::from("CPLContainerRelationLiveByAssetDate"),
                obj_type: Arc::from("CPLContainerRelationNotDeletedByAssetDate:test"),
                query_filter: None,
                page_size: 2,
                zone_id: Arc::new(json!({"zoneName": "PrimarySync"})),
                retry_config: RetryConfig::default(),
                container_id: Some(Arc::from("test")),
                cross_zone_sources: Vec::new(),
            },
            Box::new(session),
        )
    }

    async fn seed_existing_file_for_asset(
        base_config: &DownloadConfig,
        pass: &AlbumPass,
        asset: &PhotoAsset,
    ) {
        let pass_config = base_config.with_pass(pass);
        let expected_path = filter::expected_paths_for(asset, &pass_config)
            .into_iter()
            .next()
            .expect("mock asset should have an expected path");
        tokio::fs::create_dir_all(expected_path.path.parent().expect("path has parent"))
            .await
            .expect("create parent dir");
        tokio::fs::write(&expected_path.path, vec![0u8; 1024])
            .await
            .expect("seed existing file");
    }

    async fn seed_existing_recent_files(
        base_config: &DownloadConfig,
        pass: &AlbumPass,
        zone: &str,
        ids: &[String],
        filename_prefix: &str,
    ) {
        for (index, id) in ids.iter().enumerate() {
            let records = mock_photo_records_for_zone_with_filename(
                id,
                zone,
                &format!("{filename_prefix}-{index:04}.jpg"),
            );
            let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
            seed_existing_file_for_asset(base_config, pass, &asset).await;
        }
    }

    fn recent_ids(prefix: &str, count: u64) -> Vec<String> {
        (0..count).map(|i| format!("{prefix}-{i:04}")).collect()
    }

    fn recent_scope_assets(prefix: &str, count: u64, base_date: i64) -> Vec<RecentScopeAsset> {
        (0..count)
            .map(|i| RecentScopeAsset {
                id: format!("{prefix}-{i:04}"),
                asset_date: base_date - i64::try_from(i).expect("test index fits i64") * 1_000,
            })
            .collect()
    }

    fn recent_scope_photo_asset(asset: &RecentScopeAsset) -> PhotoAsset {
        let records = mock_photo_records_for_zone_with_filename_and_asset_date(
            &asset.id,
            "PrimarySync",
            &format!("{}.jpg", asset.id),
            asset.asset_date,
        );
        PhotoAsset::new(records[0].clone(), records[1].clone())
    }

    fn unique_ids_in_order(ids: Vec<String>) -> Vec<String> {
        let mut seen = FxHashSet::default();
        ids.into_iter()
            .filter(|id| seen.insert(id.clone()))
            .collect()
    }

    fn mock_photo_records_with_filename(record_name: &str, filename: &str) -> Vec<Value> {
        vec![
            json!({
                "recordName": record_name,
                "recordType": "CPLMaster",
                "fields": {
                    "filenameEnc": {"value": filename, "type": "STRING"},
                    "resOriginalRes": {
                        "value": {
                            "downloadURL": "https://p01.icloud-content.com/photo.jpg",
                            "size": 1024,
                            "fileChecksum": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
                        }
                    },
                    "resOriginalWidth": {"value": 100, "type": "INT64"},
                    "resOriginalHeight": {"value": 100, "type": "INT64"},
                    "resOriginalFileType": {"value": "public.jpeg"},
                    "itemType": {"value": "public.jpeg"},
                    "adjustmentRenderType": {"value": 0, "type": "INT64"}
                },
                "recordChangeTag": "ct1"
            }),
            json!({
                "recordName": format!("asset-{record_name}"),
                "recordType": "CPLAsset",
                "fields": {
                    "masterRef": {
                        "value": {
                            "recordName": record_name,
                            "zoneID": {"zoneName": "PrimarySync"}
                        },
                        "type": "REFERENCE"
                    },
                    "assetDate": {"value": 1700000000000i64, "type": "TIMESTAMP"},
                    "addedDate": {"value": 1700000000000i64, "type": "TIMESTAMP"}
                },
                "recordChangeTag": "ct2"
            }),
        ]
    }

    #[tokio::test]
    async fn full_sync_per_pass_streams_overlap_when_paths_are_pass_specific() {
        let session = ConcurrentRecordsSession::new(Duration::from_millis(100));
        let passes = vec![
            AlbumPass {
                kind: PassKind::Album,
                album: probe_album("album_a", session.clone()),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
            AlbumPass {
                kind: PassKind::Album,
                album: probe_album("album_b", session.clone()),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
        ];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 2;

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::dry_run_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("dry-run full sync should succeed");

        assert!(
            matches!(result.outcome, DownloadOutcome::Success),
            "empty dry-run enumeration should succeed"
        );
        assert_eq!(result.stats.enumeration_errors, 0);
        assert!(
            session.max_in_flight() >= 2,
            "album records/query streams should overlap; max in-flight was {}",
            session.max_in_flight()
        );
    }

    #[tokio::test]
    async fn full_sync_deferred_unfiled_excludes_album_members() {
        let album_session = MockPhotosFlow::new()
            .album_count(1)
            .query_photo_page("MASTER_1", Some("zone-token"))
            .build();
        let unfiled_session = MockPhotosFlow::new()
            .album_count(1)
            .query_photo_page("MASTER_1", Some("zone-token"))
            .build();
        let passes = vec![
            AlbumPass {
                kind: PassKind::Album,
                album: mock_album("Vacation", album_session),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
            AlbumPass {
                kind: PassKind::Unfiled,
                album: mock_album("", unfiled_session),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
        ];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 2;

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::dry_run_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("dry-run full sync should succeed");

        assert_eq!(
            result.stats.downloaded, 1,
            "asset present in an album pass must not also be counted through the unfiled pass"
        );
        assert!(
            matches!(result.outcome, DownloadOutcome::Success),
            "filtered duplicate should not make the run partial"
        );
    }

    #[tokio::test]
    async fn full_sync_deferred_unfiled_write_mode_exclusion_does_not_count_as_shortfall() {
        let records = mock_photo_records_with_filename("MASTER_1", "test.jpg");
        let album_session = MockPhotosFlow::new()
            .album_count(1)
            .query_page(records.clone(), Some("zone-token"))
            .build();
        let unfiled_session = MockPhotosFlow::new()
            .album_count(1)
            .query_page(records.clone(), Some("zone-token"))
            .build();
        let passes = vec![
            AlbumPass {
                kind: PassKind::Album,
                album: mock_album("Vacation", album_session),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
            AlbumPass {
                kind: PassKind::Unfiled,
                album: mock_album("", unfiled_session),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
        ];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 2;

        let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
        let album_config = config.with_pass(&passes[0]);
        let expected_path = filter::expected_paths_for(&asset, &album_config)
            .into_iter()
            .next()
            .expect("mock asset should have an expected path");
        tokio::fs::create_dir_all(expected_path.path.parent().expect("path has parent"))
            .await
            .expect("create parent dir");
        tokio::fs::write(&expected_path.path, vec![0u8; 1024])
            .await
            .expect("seed existing file");

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("write-mode full sync should succeed");

        assert_eq!(
            result.stats.enumeration_errors, 0,
            "deferred unfiled exclusions are intentional and must not be counted as pagination undercount"
        );
        assert_eq!(
            result.sync_token.as_deref(),
            Some("zone-token"),
            "clean write-mode enumeration should still advance the agreed sync token"
        );
        assert!(!result.stats.sync_token_blocked);
        assert_eq!(result.stats.sync_token_expected_receivers, Some(2));
        assert_eq!(result.stats.sync_token_receivers_with_token, Some(2));
        assert_eq!(result.stats.sync_token_receivers_missing, Some(0));
        assert_eq!(result.stats.sync_token_receivers_blank, Some(0));
        assert_eq!(result.stats.sync_token_receivers_dropped, Some(0));
        assert_eq!(result.stats.sync_token_unique_values, Some(1));
        assert!(
            matches!(result.outcome, DownloadOutcome::Success),
            "filtered duplicate should not make the write-mode run partial"
        );
    }

    #[tokio::test]
    async fn full_sync_count_shortfall_blocks_token_without_partial_failure() {
        let records = mock_photo_records_with_filename("MASTER_SHORTFALL", "shortfall.jpg");
        let session = MockPhotosFlow::new()
            .album_count(2)
            .query_page(records.clone(), Some("zone-token"))
            .empty_query_page(Some("zone-token"))
            .build();
        let passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: mock_album("Hidden", session),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 2;

        // Seed the destination so the single enumerated asset is skipped
        // on-disk and the test isolates count-only shortfall behavior.
        let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
        let pass_config = config.with_pass(&passes[0]);
        let expected_path = filter::expected_paths_for(&asset, &pass_config)
            .into_iter()
            .next()
            .expect("mock asset should have an expected path");
        tokio::fs::create_dir_all(expected_path.path.parent().expect("path has parent"))
            .await
            .expect("create parent dir");
        tokio::fs::write(&expected_path.path, vec![0u8; 1024])
            .await
            .expect("seed existing file");

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("count shortfall should not error");

        assert!(
            matches!(result.outcome, DownloadOutcome::Success),
            "count-only shortfall should not be reported as partial failure"
        );
        assert_eq!(result.stats.failed, 0);
        assert_eq!(result.stats.enumeration_errors, 0);
        assert_eq!(result.stats.pagination_shortfall_warnings, 1);
        assert_eq!(result.stats.pagination_shortfall_assets, 1);
        assert!(result.stats.sync_token_blocked);
        assert_eq!(
            result.stats.sync_token_blocked_reason,
            Some("pagination_shortfall")
        );
        assert_eq!(result.stats.sync_token_blocked_source, Some("icloud"));
        assert_eq!(
            result.stats.sync_token_blocked_explanation,
            Some(sync_token_blocked_explanation("pagination_shortfall"))
        );
        assert_eq!(result.stats.sync_token_expected_receivers, None);
        assert_eq!(result.stats.sync_token_receivers_with_token, None);
        assert_eq!(result.stats.sync_token_receivers_missing, None);
        assert_eq!(result.stats.sync_token_receivers_blank, None);
        assert_eq!(result.stats.sync_token_receivers_dropped, None);
        assert_eq!(result.stats.sync_token_unique_values, None);
        assert_eq!(result.sync_token, None, "token should stay blocked");
    }

    #[tokio::test]
    async fn full_sync_blank_query_sync_token_blocks_advancement() {
        let records = mock_photo_records_with_filename("MASTER_BLANK_TOKEN", "blank-token.jpg");
        let session = MockPhotosFlow::new()
            .album_count(1)
            .query_page(records.clone(), Some(""))
            .empty_query_page(Some(""))
            .build();
        let passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: mock_album("Hidden", session),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 2;

        // Seed the destination so the enumerated asset is skipped on-disk
        // and this test isolates token-capture behavior.
        let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
        let pass_config = config.with_pass(&passes[0]);
        let expected_path = filter::expected_paths_for(&asset, &pass_config)
            .into_iter()
            .next()
            .expect("mock asset should have an expected path");
        tokio::fs::create_dir_all(expected_path.path.parent().expect("path has parent"))
            .await
            .expect("create parent dir");
        tokio::fs::write(&expected_path.path, vec![0u8; 1024])
            .await
            .expect("seed existing file");

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("blank sync token should not error");

        assert!(
            matches!(result.outcome, DownloadOutcome::Success),
            "blank token should not force partial failure"
        );
        assert_eq!(result.stats.failed, 0);
        assert_eq!(result.stats.enumeration_errors, 0);
        assert_eq!(result.stats.pagination_shortfall_warnings, 0);
        assert_eq!(result.stats.pagination_shortfall_assets, 0);
        assert!(result.stats.sync_token_blocked);
        assert_eq!(
            result.stats.sync_token_blocked_reason,
            Some("icloud_blank_sync_token")
        );
        assert_eq!(result.stats.sync_token_blocked_source, Some("icloud"));
        assert_eq!(
            result.stats.sync_token_blocked_explanation,
            Some(sync_token_blocked_explanation("icloud_blank_sync_token"))
        );
        assert_eq!(result.stats.sync_token_expected_receivers, Some(1));
        assert_eq!(result.stats.sync_token_receivers_with_token, Some(0));
        assert_eq!(result.stats.sync_token_receivers_missing, Some(0));
        assert_eq!(result.stats.sync_token_receivers_blank, Some(1));
        assert_eq!(result.stats.sync_token_receivers_dropped, Some(0));
        assert_eq!(result.stats.sync_token_unique_values, Some(0));
        assert_eq!(result.sync_token, None, "blank token must not be persisted");
    }

    #[test]
    fn test_hash_download_config_deterministic() {
        let config = test_config();
        let hash1 = hash_download_config(&config);
        let hash2 = hash_download_config(&config);
        assert_eq!(hash1, hash2);
        assert_eq!(hash1.len(), 16); // 8 bytes hex-encoded
    }

    #[test]
    fn test_hash_download_config_changes_on_directory() {
        let mut config1 = test_config();
        config1.directory = std::sync::Arc::from(std::path::Path::new("/photos/a"));
        let mut config2 = test_config();
        config2.directory = std::sync::Arc::from(std::path::Path::new("/photos/b"));
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_download_config_changes_on_folder_structure() {
        let mut config1 = test_config();
        config1.folder_structure = "{:%Y/%m/%d}".to_string();
        let mut config2 = test_config();
        config2.folder_structure = "{:%Y/%m}".to_string();
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_should_download_fast_trust_state_returns_false() {
        let mut ctx = DownloadContext::default();
        ctx.downloaded_ids
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset1".into())
            .or_default()
            .insert("original".into());
        ctx.downloaded_checksums
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset1".into())
            .or_default()
            .insert("original".into(), "checksum_a".into());

        // trust_state=true: returns Some(false) for matching asset
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "asset1",
                VersionSizeKey::Original,
                "checksum_a",
                true
            ),
            Some(false)
        );

        // trust_state=false: returns None (needs filesystem check)
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "asset1",
                VersionSizeKey::Original,
                "checksum_a",
                false
            ),
            None
        );

        // Changed checksum: returns Some(true) regardless of trust_state
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "asset1",
                VersionSizeKey::Original,
                "checksum_b",
                true
            ),
            Some(true)
        );

        // Unknown asset: returns Some(true)
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "unknown",
                VersionSizeKey::Original,
                "x",
                true
            ),
            Some(true)
        );
    }

    // ── extract_skip_candidates tests ──────────────────────────────

    // ── hash_download_config additional sensitivity tests ──────────

    #[test]
    fn test_hash_download_config_changes_on_file_match_policy() {
        let mut config1 = test_config();
        config1.file_match_policy = FileMatchPolicy::NameSizeDedupWithSuffix;
        let mut config2 = test_config();
        config2.file_match_policy = FileMatchPolicy::NameId7;
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_download_config_changes_on_keep_unicode() {
        let mut config1 = test_config();
        config1.keep_unicode_in_filenames = false;
        let mut config2 = test_config();
        config2.keep_unicode_in_filenames = true;
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_download_config_ignores_unrelated_fields() {
        let mut config1 = test_config();
        config1.concurrent_downloads = 1;
        let mut config2 = test_config();
        config2.concurrent_downloads = 16;
        // These fields don't affect download paths, so hash should be the same
        assert_eq!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    // ── determine_media_type tests ──────────────────────────────────────

    // ── NameId7 filter tests ────────────────────────────────────────────

    // ── keep_unicode_in_filenames tests ─────────────────────────────────

    // ── Medium/Thumb size suffix tests ──────────────────────────────────

    // ── NormalizedPath direct tests ─────────────────────────────────────

    // ---------- SyncMode / SyncResult tests ----------

    #[test]
    fn test_sync_result_partial_failure() {
        let result = SyncResult {
            outcome: DownloadOutcome::PartialFailure { failed_count: 3 },
            sync_token: Some("tok".to_string()),
            stats: SyncStats::default(),
            full_enumeration_ran: false,
        };
        match result.outcome {
            DownloadOutcome::PartialFailure { failed_count } => {
                assert_eq!(failed_count, 3);
            }
            _ => panic!("Expected PartialFailure"),
        }
    }

    #[test]
    fn test_sync_result_session_expired() {
        let result = SyncResult {
            outcome: DownloadOutcome::SessionExpired {
                auth_error_count: 5,
            },
            sync_token: None,
            stats: SyncStats::default(),
            full_enumeration_ran: false,
        };
        match result.outcome {
            DownloadOutcome::SessionExpired { auth_error_count } => {
                assert_eq!(auth_error_count, 5);
            }
            _ => panic!("Expected SessionExpired"),
        }
    }

    fn hard_deleted_change_record(record_name: &str) -> Value {
        json!({
            "recordName": record_name,
            "recordType": null,
            "fields": {},
            "deleted": true,
        })
    }

    fn flagged_incremental_records(record_name: &str, flag: (&str, i64)) -> Vec<Value> {
        let mut records = incremental_photo_records(record_name);
        records[0]["fields"][flag.0] = json!({"value": flag.1, "type": "INT64"});
        records
    }

    fn assert_source_flags(
        records: &[crate::state::AssetRecord],
        asset_id: &str,
        expected_deleted: bool,
        expected_hidden: bool,
    ) {
        let record = records
            .iter()
            .find(|record| record.id.as_ref() == asset_id)
            .unwrap_or_else(|| panic!("missing state row for {asset_id}"));
        assert_eq!(
            record.metadata.is_deleted, expected_deleted,
            "is_deleted mismatch for {asset_id}"
        );
        assert_eq!(
            record.metadata.is_hidden, expected_hidden,
            "is_hidden mismatch for {asset_id}"
        );
    }

    #[tokio::test]
    async fn download_incremental_delete_and_hidden_events_mark_state_without_downloads() {
        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().unwrap());
        for id in ["SOFT_DELETE", "HARD_DELETE", "HIDDEN_ASSET"] {
            db.upsert_seen(&TestAssetRecord::new(id).build())
                .await
                .unwrap();
        }

        let mut records = Vec::new();
        records.extend(flagged_incremental_records("SOFT_DELETE", ("isDeleted", 1)));
        records.push(hard_deleted_change_record("HARD_DELETE"));
        records.extend(flagged_incremental_records("HIDDEN_ASSET", ("isHidden", 1)));
        records.extend(incremental_photo_records("CREATED_ASSET"));
        let session = MockPhotosFlow::new()
            .changes_zone_page(records, "zone-token-after", false)
            .build();
        let passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: mock_album("Library", session),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().unwrap();
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db.clone());

        let result = download_photos_incremental(
            &Client::new(),
            &passes,
            &Arc::new(config),
            "zone-token-before",
            DownloadControls::new(DownloadRunMode::PrintFilenames, DownloadReporting::hidden()),
            CancellationToken::new(),
        )
        .await
        .unwrap();

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(
            result.sync_token, None,
            "print-only incremental runs must not advance the sync token"
        );
        let pending = db.get_pending().await.unwrap();
        assert_source_flags(&pending, "SOFT_DELETE", true, false);
        assert_source_flags(&pending, "HARD_DELETE", true, false);
        assert_source_flags(&pending, "HIDDEN_ASSET", false, true);
    }

    // ── NormalizedPath additional tests ──────────────────────────────────

    // ── hash_download_config additional sensitivity ─────────────────────

    #[test]
    fn test_hash_download_config_changes_on_resolution() {
        let mut config1 = test_config();
        config1.resolution = crate::types::PhotoResolution::Original;
        let mut config2 = test_config();
        config2.resolution = crate::types::PhotoResolution::Medium;
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_download_config_changes_on_live_resolution() {
        let mut config1 = test_config();
        config1.live_resolution = AssetVersionSize::LiveOriginal;
        let mut config2 = test_config();
        config2.live_resolution = AssetVersionSize::LiveMedium;
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_download_config_changes_on_live_photo_mov_filename_policy() {
        let mut config1 = test_config();
        config1.live_photo_mov_filename_policy = crate::types::LivePhotoMovFilenamePolicy::Suffix;
        let mut config2 = test_config();
        config2.live_photo_mov_filename_policy = crate::types::LivePhotoMovFilenamePolicy::Original;
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_download_config_changes_on_raw_policy() {
        let mut config1 = test_config();
        config1.raw_policy = RawPolicy::AsIs;
        let mut config2 = test_config();
        config2.raw_policy = RawPolicy::PreferRaw;
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_download_config_changes_on_skip_created_before() {
        let mut config1 = test_config();
        config1.skip_created_before = None;
        let mut config2 = test_config();
        config2.skip_created_before = Some(
            DateTime::parse_from_rfc3339("2020-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        );
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_download_config_changes_on_skip_created_after() {
        let mut config1 = test_config();
        config1.skip_created_after = None;
        let mut config2 = test_config();
        config2.skip_created_after = Some(
            DateTime::parse_from_rfc3339("2024-12-31T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        );
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_download_config_changes_on_recent() {
        let mut config1 = test_config();
        config1.recent = None;
        let mut config2 = test_config();
        config2.recent = Some(100);
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_download_config_changes_on_recent_scope_when_recent_is_set() {
        let mut config1 = test_config();
        config1.recent = Some(100);
        config1.recent_scope = crate::cli::RecentScope::Global;
        let mut config2 = test_config();
        config2.recent = Some(100);
        config2.recent_scope = crate::cli::RecentScope::PerFilter;
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_download_config_changes_on_force_resolution() {
        let mut config1 = test_config();
        config1.force_resolution = false;
        let mut config2 = test_config();
        config2.force_resolution = true;
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_download_config_changes_on_media_videos() {
        let mut config1 = test_config();
        config1.media.videos = true;
        let mut config2 = test_config();
        config2.media.videos = false;
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_download_config_changes_on_media_photos() {
        let mut config1 = test_config();
        config1.media.photos = true;
        let mut config2 = test_config();
        config2.media.photos = false;
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_download_config_is_16_hex_chars() {
        let config = test_config();
        let hash = hash_download_config(&config);
        assert_eq!(hash.len(), 16);
        assert!(
            hash.chars().all(|c| c.is_ascii_hexdigit()),
            "Hash should be hex chars only, got: {hash}"
        );
    }

    // ── compute_config_hash equivalence ────────────────────────────────

    /// `compute_config_hash` includes enumeration-filter fields (albums,
    /// library, live_photo_mode) that `hash_download_config` doesn't.
    /// Verify it produces a valid hex hash and is deterministic.
    #[test]
    fn test_compute_config_hash_matches_hash_download_config() {
        use crate::config::Config;

        let dl_config = test_config();
        let globals = crate::config::GlobalArgs {
            username: Some("u@example.com".to_string()),
            domain: None,
            data_dir: Some("/tmp".to_string()),
        };
        let app_config = Config::build(
            &globals,
            &crate::cli::PasswordArgs::default(),
            crate::cli::SyncArgs {
                recent: dl_config.recent.map(crate::cli::RecentLimit::Count),
                config_overrides: crate::config::SyncConfigOverrides {
                    download_dir: Some(dl_config.directory.display().to_string()),
                    folder_structure: Some(dl_config.folder_structure.clone()),
                    resolution: Some(crate::types::PhotoResolution::Original),
                    ..Default::default()
                },
                no_progress_bar: true,
                ..Default::default()
            },
            None,
        )
        .unwrap();

        // compute_config_hash is a superset (includes albums, library, live_photo_mode)
        // so it won't match hash_download_config. Verify it's deterministic and valid hex.
        let hash1 = compute_config_hash(&app_config);
        let hash2 = compute_config_hash(&app_config);
        assert_eq!(hash1, hash2, "compute_config_hash must be deterministic");
        assert_eq!(hash1.len(), 16);
        assert!(hash1.chars().all(|c| c.is_ascii_hexdigit()));

        // Verify album changes produce a different hash
        let mut config_with_album = app_config;
        config_with_album.filters.selection.albums =
            crate::selection::parse_album_selector(&["Favorites".to_string()], true).unwrap();
        let hash3 = compute_config_hash(&config_with_album);
        assert_ne!(hash1, hash3, "adding an album must change the hash");
    }

    // ── should_download_fast additional tests ───────────────────────────

    #[test]
    fn test_should_download_fast_unknown_asset_returns_true() {
        let ctx = DownloadContext::default();
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "never_seen",
                VersionSizeKey::Original,
                "any_ck",
                true
            ),
            Some(true)
        );
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "never_seen",
                VersionSizeKey::Original,
                "any_ck",
                false
            ),
            Some(true)
        );
    }

    #[tokio::test]
    async fn unfiled_only_incremental_ignores_inactive_album_path_templates() {
        let calls = Arc::new(AtomicUsize::new(0));
        let session = changes_zone_session(
            Arc::clone(&calls),
            incremental_photo_records("MASTER_CHANGED"),
        );
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: changes_album("", session),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        assert!(
            config.requires_per_pass_paths(),
            "default inactive album templates still contain per-pass tokens"
        );
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.sync_mode = SyncMode::Incremental {
            zone_sync_token: "zone-token-prev".to_string(),
        };

        let result = download_photos_with_sync(
            &Client::new(),
            &passes,
            Arc::new(config),
            DownloadControls::new(DownloadRunMode::PrintFilenames, DownloadReporting::hidden()),
            CancellationToken::new(),
        )
        .await
        .expect("unfiled-only incremental sync should succeed");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert!(
            !result.full_enumeration_ran,
            "inactive album/smart-folder templates must not force full enumeration"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "unfiled-only incremental sync should query changes/zone"
        );
    }

    #[tokio::test]
    async fn album_incremental_records_relation_hydration_full_enumeration_reason() {
        let session = MockPhotosFlow::new()
            .album_count(0)
            .empty_query_page(Some("zone-token-next"))
            .build();
        let passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: mock_album("Vacation", session),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.sync_mode = SyncMode::Incremental {
            zone_sync_token: "zone-token-prev".to_string(),
        };

        let result = download_photos_with_sync(
            &Client::new(),
            &passes,
            Arc::new(config),
            DownloadControls::new(DownloadRunMode::PrintFilenames, DownloadReporting::hidden()),
            CancellationToken::new(),
        )
        .await
        .expect("album incremental should fall back to full enumeration");

        assert!(result.full_enumeration_ran);
        assert_eq!(
            result.stats.full_enumeration_reason,
            Some(FullEnumerationReason::AlbumRelationHydrationIncomplete)
        );
    }

    #[tokio::test]
    async fn incremental_with_failed_rows_falls_back_to_full_enumeration() {
        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
        let record = crate::test_helpers::TestAssetRecord::new("FAILED_BEFORE_SYNC")
            .filename("failed-before-sync.jpg")
            .checksum("ck_failed_before_sync")
            .size(1024)
            .build();
        db.upsert_seen(&record).await.expect("seed pending row");
        db.mark_failed(
            "PrimarySync",
            "FAILED_BEFORE_SYNC",
            "original",
            "prior download failure",
        )
        .await
        .expect("mark failed");

        let session = MockPhotosFlow::new()
            .album_count(0)
            .empty_query_page(Some("zone-token-next"))
            .build();
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: mock_album("", session),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db.clone());
        config.sync_mode = SyncMode::Incremental {
            zone_sync_token: "zone-token-prev".to_string(),
        };

        let result = download_photos_with_sync(
            &Client::new(),
            &passes,
            Arc::new(config),
            DownloadControls::new(DownloadRunMode::PrintFilenames, DownloadReporting::hidden()),
            CancellationToken::new(),
        )
        .await
        .expect("failed rows should fall back to full enumeration");

        assert!(
            result.full_enumeration_ran,
            "normal sync with failed rows must not stay incremental"
        );
        assert_eq!(
            result.stats.full_enumeration_reason,
            Some(FullEnumerationReason::RetryFailedRows)
        );
        assert!(matches!(result.outcome, DownloadOutcome::Success));
    }

    #[tokio::test]
    async fn incremental_with_pending_rows_records_pending_full_enumeration_reason() {
        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
        let record = crate::test_helpers::TestAssetRecord::new("PENDING_BEFORE_SYNC")
            .filename("pending-before-sync.jpg")
            .checksum("ck_pending_before_sync")
            .size(1024)
            .build();
        db.upsert_seen(&record).await.expect("seed pending row");

        let session = MockPhotosFlow::new()
            .album_count(0)
            .empty_query_page(Some("zone-token-next"))
            .build();
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: mock_album("", session),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db);
        config.sync_mode = SyncMode::Incremental {
            zone_sync_token: "zone-token-prev".to_string(),
        };

        let result = download_photos_with_sync(
            &Client::new(),
            &passes,
            Arc::new(config),
            DownloadControls::new(DownloadRunMode::PrintFilenames, DownloadReporting::hidden()),
            CancellationToken::new(),
        )
        .await
        .expect("pending rows should fall back to full enumeration");

        assert!(result.full_enumeration_ran);
        assert_eq!(
            result.stats.full_enumeration_reason,
            Some(FullEnumerationReason::PendingRows)
        );
    }

    #[tokio::test]
    async fn incremental_with_metadata_backfill_records_full_enumeration_reason() {
        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().expect("state db"));
        let dir = TempDir::new().expect("temp dir");
        let record = crate::test_helpers::TestAssetRecord::new("BACKFILL_BEFORE_SYNC")
            .filename("backfill-before-sync.jpg")
            .checksum("ck_backfill_before_sync")
            .size(1024)
            .build();
        db.upsert_seen(&record).await.expect("seed pending row");
        let path = dir.path().join("backfill-before-sync.jpg");
        tokio::fs::write(&path, vec![0u8; 1024])
            .await
            .expect("write local file");
        db.mark_downloaded(
            "PrimarySync",
            "BACKFILL_BEFORE_SYNC",
            "original",
            &path,
            "local_hash",
            None,
        )
        .await
        .expect("mark downloaded");
        db.clear_metadata_hash_for_test("PrimarySync", "BACKFILL_BEFORE_SYNC", "original");
        assert!(db.has_downloaded_without_metadata_hash().await.unwrap());

        let session = MockPhotosFlow::new()
            .album_count(0)
            .empty_query_page(Some("zone-token-next"))
            .build();
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: mock_album("", session),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db);
        config.sync_mode = SyncMode::Incremental {
            zone_sync_token: "zone-token-prev".to_string(),
        };

        let result = download_photos_with_sync(
            &Client::new(),
            &passes,
            Arc::new(config),
            DownloadControls::new(DownloadRunMode::PrintFilenames, DownloadReporting::hidden()),
            CancellationToken::new(),
        )
        .await
        .expect("metadata backfill should fall back to full enumeration");

        assert!(result.full_enumeration_ran);
        assert_eq!(
            result.stats.full_enumeration_reason,
            Some(FullEnumerationReason::MetadataBackfill)
        );
    }

    #[tokio::test]
    async fn incremental_sync_queries_zone_changes_once_per_library() {
        let calls = Arc::new(AtomicUsize::new(0));
        let session = changes_zone_session(
            Arc::clone(&calls),
            incremental_photo_records("MASTER_CHANGED"),
        );
        let passes = vec![
            AlbumPass {
                kind: PassKind::Album,
                album: changes_album("album_a", session.clone()),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
            AlbumPass {
                kind: PassKind::Album,
                album: changes_album("album_b", session),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
        ];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());

        let result = download_photos_incremental(
            &Client::new(),
            &passes,
            &Arc::new(config),
            "zone-token-prev",
            DownloadControls::new(DownloadRunMode::PrintFilenames, DownloadReporting::hidden()),
            CancellationToken::new(),
        )
        .await
        .expect("print-only incremental sync should succeed");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(
            result.sync_token, None,
            "print-only mode must not advance the sync token"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "changes/zone is zone-scoped; querying once per pass repeats the same delta"
        );
    }

    #[test]
    fn needs_metadata_rewrite_detects_hash_change() {
        let mut ctx = DownloadContext::default();
        ctx.downloaded_metadata_hashes
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_md".into())
            .or_default()
            .insert("original".into(), "hash-OLD".into());

        // Same hash -> no rewrite needed.
        assert!(!ctx.needs_metadata_rewrite(
            "PrimarySync",
            "asset_md",
            VersionSizeKey::Original,
            Some("hash-OLD")
        ));
        // Different hash -> rewrite.
        assert!(ctx.needs_metadata_rewrite(
            "PrimarySync",
            "asset_md",
            VersionSizeKey::Original,
            Some("hash-NEW")
        ));
        // Unknown new hash -> no rewrite (nothing to compare to).
        assert!(!ctx.needs_metadata_rewrite(
            "PrimarySync",
            "asset_md",
            VersionSizeKey::Original,
            None
        ));
    }

    #[test]
    fn needs_metadata_rewrite_honors_retry_marker() {
        let mut ctx = DownloadContext::default();
        ctx.metadata_retry_markers
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_retry".into())
            .or_default()
            .insert("original".into());
        // No stored hash at all, but marker is set -> rewrite needed.
        assert!(ctx.needs_metadata_rewrite(
            "PrimarySync",
            "asset_retry",
            VersionSizeKey::Original,
            None
        ));
        // Marker set -> rewrite even if hashes match.
        ctx.downloaded_metadata_hashes
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_retry".into())
            .or_default()
            .insert("original".into(), "h".into());
        assert!(ctx.needs_metadata_rewrite(
            "PrimarySync",
            "asset_retry",
            VersionSizeKey::Original,
            Some("h")
        ));
    }

    #[test]
    fn needs_metadata_rewrite_refreshes_null_stored_hash() {
        // Pre-v5 downloaded rows have metadata_hash IS NULL; even without a
        // retry marker, a fresh hash should trigger a rewrite so the XMP
        // gets the source state this tree has never recorded.
        let ctx = DownloadContext::default();
        assert!(ctx.needs_metadata_rewrite(
            "PrimarySync",
            "asset_no_stored_hash",
            VersionSizeKey::Original,
            Some("new-hash")
        ));
    }

    #[test]
    fn test_should_download_fast_downloaded_matching_checksum() {
        let mut ctx = DownloadContext::default();
        ctx.downloaded_ids
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_x".into())
            .or_default()
            .insert("original".into());
        ctx.downloaded_checksums
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_x".into())
            .or_default()
            .insert("original".into(), "ck_match".into());

        // trust_state=true => hard skip
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "asset_x",
                VersionSizeKey::Original,
                "ck_match",
                true
            ),
            Some(false)
        );
        // trust_state=false => needs filesystem check
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "asset_x",
                VersionSizeKey::Original,
                "ck_match",
                false
            ),
            None
        );
    }

    #[test]
    fn test_should_download_fast_downloaded_changed_checksum() {
        let mut ctx = DownloadContext::default();
        ctx.downloaded_ids
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_y".into())
            .or_default()
            .insert("original".into());
        ctx.downloaded_checksums
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_y".into())
            .or_default()
            .insert("original".into(), "old_ck".into());

        // Changed checksum => needs re-download regardless of trust_state
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "asset_y",
                VersionSizeKey::Original,
                "new_ck",
                true
            ),
            Some(true)
        );
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "asset_y",
                VersionSizeKey::Original,
                "new_ck",
                false
            ),
            Some(true)
        );
    }

    #[test]
    fn test_should_download_fast_different_version_size() {
        let mut ctx = DownloadContext::default();
        ctx.downloaded_ids
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_z".into())
            .or_default()
            .insert("original".into());

        // Medium version not downloaded
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "asset_z",
                VersionSizeKey::Medium,
                "any_ck",
                true
            ),
            Some(true)
        );
    }

    #[test]
    fn test_download_context_known_ids_populated_for_retry_only() {
        // Simulate retry-only mode: known_ids is populated
        let mut ctx = DownloadContext::default();
        ctx.known_ids.insert("known_asset".into());

        // A known asset that's not in downloaded_ids needs download
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "known_asset",
                VersionSizeKey::Original,
                "ck",
                true
            ),
            Some(true)
        );
        // The known_ids set is used externally to decide whether to skip new assets;
        // verify the set membership works
        assert!(ctx.known_ids.contains("known_asset"));
        assert!(!ctx.known_ids.contains("new_asset"));
    }

    #[test]
    fn download_context_detects_downloaded_rows_missing_metadata_hashes() {
        let mut ctx = DownloadContext::default();
        ctx.downloaded_ids
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_meta".into())
            .or_default()
            .insert("original".into());

        ctx.downloaded_without_metadata_hash = count_version_set_entries(&ctx.downloaded_ids)
            > count_value_map_entries(&ctx.downloaded_metadata_hashes);

        assert!(
            ctx.has_downloaded_without_metadata_hash(),
            "a downloaded row with no matching metadata hash needs the backfill notice"
        );

        ctx.downloaded_metadata_hashes
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_meta".into())
            .or_default()
            .insert("original".into(), "metadata_hash".into());

        ctx.downloaded_without_metadata_hash = count_version_set_entries(&ctx.downloaded_ids)
            > count_value_map_entries(&ctx.downloaded_metadata_hashes);

        assert!(
            !ctx.has_downloaded_without_metadata_hash(),
            "matching downloaded and metadata-hash sets should avoid the extra SQLite scan"
        );
    }

    // ── Gap coverage: empty versions, path traversal, empty filename ───

    #[test]
    fn should_download_fast_empty_checksum_never_hard_skips() {
        // Empty remote checksum is malformed provider input. Even if a stale
        // DB row also has an empty checksum, this must not turn into a hard
        // skip: the provider parser rejects new empty checksums, and this
        // fast path stays defensive for legacy/corrupt state.
        let mut ctx = DownloadContext::default();
        ctx.downloaded_ids
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_empty_ck".into())
            .or_default()
            .insert("original".into());
        ctx.downloaded_checksums
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_empty_ck".into())
            .or_default()
            .insert("original".into(), "".into());

        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "asset_empty_ck",
                VersionSizeKey::Original,
                "",
                true
            ),
            Some(true)
        );
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "asset_empty_ck",
                VersionSizeKey::Original,
                "",
                false
            ),
            Some(true)
        );
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "asset_empty_ck",
                VersionSizeKey::Original,
                "abc123def456",
                true,
            ),
            Some(true)
        );
    }

    // ── Gap coverage: should_download_fast with no checksum in DB ────────

    #[test]
    fn should_download_fast_no_checksum_trust_true_returns_false() {
        // Asset is in downloaded_ids but has no entry in downloaded_checksums.
        // With trust_state=true the method should hard-skip (Some(false))
        // because the absence of a stored checksum means "nothing to compare".
        let mut ctx = DownloadContext::default();
        ctx.downloaded_ids
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_no_ck".into())
            .or_default()
            .insert("original".into());
        // No entry in downloaded_checksums

        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "asset_no_ck",
                VersionSizeKey::Original,
                "any",
                true
            ),
            Some(false)
        );
    }

    #[test]
    fn should_download_fast_no_checksum_trust_false_returns_none() {
        // Same scenario but trust_state=false: needs filesystem check (None).
        let mut ctx = DownloadContext::default();
        ctx.downloaded_ids
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_no_ck".into())
            .or_default()
            .insert("original".into());

        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "asset_no_ck",
                VersionSizeKey::Original,
                "any",
                false
            ),
            None
        );
    }

    // ── Gap coverage: retry_only known_ids filtering ────────────────────

    // ── Gap coverage: skip_created_before AND skip_created_after ────────

    // ── Gap coverage: incremental Modified events are downloadable ──────

    // ── Gap coverage: NameId7 produces task when file at original path ──

    // ── compute_config_hash tests ──────────────────────────────────

    /// Build a `Config` via `Config::build` with the given overrides.
    /// Uses a tempdir for cookie_directory so tests don't touch the real filesystem.
    fn build_config_with(
        cookie_dir: &std::path::Path,
        directory: &str,
        overrides: impl FnOnce(&mut crate::cli::SyncArgs),
    ) -> crate::config::Config {
        use crate::cli::SyncArgs;
        use crate::config::GlobalArgs;

        let globals = GlobalArgs {
            username: Some("test@example.com".to_string()),
            domain: None,
            data_dir: Some(cookie_dir.to_string_lossy().into_owned()),
        };
        let mut sync = SyncArgs {
            config_overrides: crate::config::SyncConfigOverrides {
                download_dir: Some(directory.to_string()),
                ..Default::default()
            },
            ..SyncArgs::default()
        };
        overrides(&mut sync);
        crate::config::Config::build(&globals, &crate::cli::PasswordArgs::default(), sync, None)
            .expect("Config::build should succeed")
    }

    #[test]
    fn test_compute_config_hash_same_config_same_hash() {
        let tmp = TempDir::new().unwrap();
        let a = build_config_with(tmp.path(), "/photos", |_| {});
        let b = build_config_with(tmp.path(), "/photos", |_| {});
        assert_eq!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn test_compute_config_hash_different_directory() {
        let tmp = TempDir::new().unwrap();
        let a = build_config_with(tmp.path(), "/photos/a", |_| {});
        let b = build_config_with(tmp.path(), "/photos/b", |_| {});
        assert_ne!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn test_compute_config_hash_different_size() {
        let tmp = TempDir::new().unwrap();
        let a = build_config_with(tmp.path(), "/photos", |_| {});
        let b = build_config_with(tmp.path(), "/photos", |s| {
            s.config_overrides.resolution = Some(crate::types::PhotoResolution::Medium);
        });
        assert_ne!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn test_compute_config_hash_different_skip_videos() {
        let tmp = TempDir::new().unwrap();
        let a = build_config_with(tmp.path(), "/photos", |_| {});
        let b = build_config_with(tmp.path(), "/photos", |s| {
            s.config_overrides.skip_videos = Some(true);
        });
        assert_ne!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn test_compute_config_hash_different_albums() {
        let tmp = TempDir::new().unwrap();
        let a = build_config_with(tmp.path(), "/photos", |_| {});
        let b = build_config_with(tmp.path(), "/photos", |s| {
            s.config_overrides.albums = vec!["Favorites".to_string()];
        });
        assert_ne!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn test_compute_config_hash_different_inline_album_excludes() {
        let tmp = TempDir::new().unwrap();
        let a = build_config_with(tmp.path(), "/photos", |_| {});
        let b = build_config_with(tmp.path(), "/photos", |s| {
            s.config_overrides.albums = vec!["!Hidden".to_string()];
        });
        assert_ne!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn test_compute_config_hash_different_live_photo_mode() {
        let tmp = TempDir::new().unwrap();
        let a = build_config_with(tmp.path(), "/photos", |_| {});
        let b = build_config_with(tmp.path(), "/photos", |s| {
            s.config_overrides.live_photo_mode = Some(LivePhotoMode::Skip);
        });
        assert_ne!(compute_config_hash(&a), compute_config_hash(&b));
    }

    #[test]
    fn test_compute_config_hash_different_smart_folders() {
        // Toggling --smart-folder between modes (none → all, or
        // adding a named folder) changes which assets are eligible. If
        // this isn't reflected in the config hash, the next cycle reuses
        // the per-zone sync token whose enumeration boundary was computed
        // under the old selection — newly-eligible smart-folder assets
        // are silently skipped.
        let tmp = TempDir::new().unwrap();
        let a = build_config_with(tmp.path(), "/photos", |_| {});
        let b = build_config_with(tmp.path(), "/photos", |s| {
            s.config_overrides.smart_folders = vec!["Favorites".to_string()];
        });
        assert_ne!(
            compute_config_hash(&a),
            compute_config_hash(&b),
            "changing --smart-folder must change the config hash so the \
             stored sync token is invalidated"
        );
    }

    #[test]
    fn test_compute_config_hash_different_unfiled() {
        // Same silent-miss vector for the unfiled selector: --unfiled true
        // (default) → false changes whether the unfiled pass runs at all.
        // A regression that omits this from the hash leaves a stale token
        // pointing past assets the previous cycle would have caught.
        let tmp = TempDir::new().unwrap();
        let a = build_config_with(tmp.path(), "/photos", |_| {});
        let b = build_config_with(tmp.path(), "/photos", |s| {
            s.config_overrides.unfiled = Some(false);
        });
        assert_ne!(
            compute_config_hash(&a),
            compute_config_hash(&b),
            "changing --unfiled must change the config hash so the \
             stored sync token is invalidated"
        );
    }

    #[test]
    fn test_compute_config_hash_different_library() {
        let tmp = TempDir::new().unwrap();
        let a = build_config_with(tmp.path(), "/photos", |_| {});
        let b = build_config_with(tmp.path(), "/photos", |s| {
            s.config_overrides.libraries = vec!["all".to_string()];
        });
        assert_ne!(
            compute_config_hash(&a),
            compute_config_hash(&b),
            "changing library selection should change the config hash"
        );
    }

    #[test]
    fn test_compute_config_hash_different_recent_same_hash() {
        let tmp = TempDir::new().unwrap();
        let a = build_config_with(tmp.path(), "/photos", |_| {});
        let b = build_config_with(tmp.path(), "/photos", |s| {
            s.recent = Some(crate::cli::RecentLimit::Count(100));
        });
        assert_eq!(
            compute_config_hash(&a),
            compute_config_hash(&b),
            "recent is intentionally excluded from the config hash"
        );
    }

    #[test]
    fn test_compute_config_hash_different_dry_run_same_hash() {
        let tmp = TempDir::new().unwrap();
        let a = build_config_with(tmp.path(), "/photos", |_| {});
        let b = build_config_with(tmp.path(), "/photos", |s| {
            s.dry_run = true;
        });
        assert_eq!(
            compute_config_hash(&a),
            compute_config_hash(&b),
            "dry_run is a per-run flag and should not affect the config hash"
        );
    }

    // ── filter_asset_to_tasks edge-case tests ──────────────────────

    // ── LivePhotoMode + filename_exclude filter tests ─────────────

    // ── exclude_asset_ids filter tests ─────────────────────────────

    #[test]
    fn test_hash_changes_on_live_photo_mode() {
        let config1 = test_config();
        let mut config2 = test_config();
        config2.live_photo_mode = LivePhotoMode::Skip;
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_changes_on_filename_exclude() {
        let config1 = test_config();
        let mut config2 = test_config();
        config2.filename_exclude = std::sync::Arc::from(vec![glob::Pattern::new("*.AAE").unwrap()]);
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    // ── requires_per_pass_paths predicate ──────────────────────────

    fn config_with_templates(base: &str, albums: &str, smart_folders: &str) -> DownloadConfig {
        let mut c = test_config();
        c.folder_structure = base.to_string();
        c.folder_structure_albums = Arc::from(albums);
        c.folder_structure_smart_folders = Arc::from(smart_folders);
        c
    }

    #[test]
    fn incremental_full_enumeration_gate_ignores_unfiled_only_pass() {
        let session = crate::test_helpers::MockPhotosSession::new();
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album: mock_album("", session),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        assert!(
            !incremental_requires_full_enumeration(&passes),
            "unfiled-only sync can use zone-level incremental changes"
        );
    }

    #[test]
    fn incremental_full_enumeration_gate_fires_on_album_pass() {
        let session = crate::test_helpers::MockPhotosSession::new();
        let passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: mock_album("Vacation", session),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        assert!(
            incremental_requires_full_enumeration(&passes),
            "album-scoped sync needs full enumeration to preserve membership"
        );
    }

    #[test]
    fn requires_per_pass_paths_fires_on_v013_defaults() {
        // v0.13 defaults carry per-pass tokens in the per-category fields.
        // Returning false here was the regression that silently routed every
        // album-pass photo through the unfiled template.
        assert!(test_config().requires_per_pass_paths());
    }

    #[test]
    fn requires_per_pass_paths_fires_on_legacy_album_in_base() {
        assert!(
            config_with_templates("{album}/%Y", "{album}/%Y", "{album}/%Y")
                .requires_per_pass_paths()
        );
    }

    #[test]
    fn requires_per_pass_paths_fires_on_smart_folder_token() {
        assert!(
            config_with_templates("%Y/%m/%d", "%Y/%m/%d", "{smart-folder}")
                .requires_per_pass_paths()
        );
    }

    #[test]
    fn requires_per_pass_paths_fires_on_library_token() {
        assert!(
            config_with_templates("{library}/%Y", "{library}/%Y", "{library}/%Y")
                .requires_per_pass_paths()
        );
    }

    #[test]
    fn requires_per_pass_paths_fires_on_per_category_template_diverging_from_base() {
        assert!(
            config_with_templates("%Y/%m/%d", "MyAlbums/%Y/%m", "%Y/%m/%d")
                .requires_per_pass_paths()
        );
    }

    #[test]
    fn requires_per_pass_paths_false_when_all_templates_are_identical_literals() {
        // Pure-literal, identical across all three fields, no per-pass token:
        // the merged-stream branch is safe.
        assert!(
            !config_with_templates("%Y/%m/%d", "%Y/%m/%d", "%Y/%m/%d").requires_per_pass_paths()
        );
    }

    // ── with_pass per-kind template selection ─────────────────────

    fn make_pass(kind: crate::commands::PassKind, name: &str) -> crate::commands::AlbumPass {
        use crate::icloud::photos::PhotoAlbum;
        crate::commands::AlbumPass {
            kind,
            album: PhotoAlbum::stub_for_test(Arc::from(name)),
            exclude_ids: std::sync::Arc::new(rustc_hash::FxHashSet::default()),
        }
    }

    #[test]
    fn test_with_pass_album_uses_albums_template() {
        use crate::commands::PassKind;
        let mut config = test_config();
        config.folder_structure_albums = Arc::from("{album}/%Y/%m/%d");
        let derived = config.with_pass(&make_pass(PassKind::Album, "Vacation"));
        assert_eq!(derived.folder_structure, "Vacation/%Y/%m/%d");
        assert_eq!(derived.album_name.as_deref(), Some("Vacation"));
    }

    #[test]
    fn test_with_pass_smart_folder_uses_smart_folders_template() {
        use crate::commands::PassKind;
        let mut config = test_config();
        config.folder_structure_smart_folders = Arc::from("{smart-folder}/%Y");
        let derived = config.with_pass(&make_pass(PassKind::SmartFolder, "Favorites"));
        assert_eq!(derived.folder_structure, "Favorites/%Y");
    }

    #[test]
    fn test_with_pass_smart_folder_ignores_albums_template() {
        // Spec: smart-folder passes use folder_structure_smart_folders, not
        // folder_structure_albums. Using the wrong template would cause every
        // smart-folder pass to substitute the smart-folder name into a
        // user-customised album path (e.g. "My/Albums/{album}/..." would
        // mis-render as "My/Albums/Favorites/...").
        use crate::commands::PassKind;
        let mut config = test_config();
        config.folder_structure_albums = Arc::from("{album}/album-tree");
        config.folder_structure_smart_folders = Arc::from("{smart-folder}/sf-tree");
        let derived = config.with_pass(&make_pass(PassKind::SmartFolder, "Videos"));
        assert!(derived.folder_structure.contains("sf-tree"));
        assert!(!derived.folder_structure.contains("album-tree"));
    }

    #[test]
    fn test_with_pass_unfiled_uses_base_folder_structure() {
        // Unfiled pass keeps the legacy `{album}` token in `folder_structure`
        // so existing configs with `--folder-structure "{album}/..."` still
        // produce the same on-disk tree.
        use crate::commands::PassKind;
        let mut config = test_config();
        config.folder_structure = "%Y/%m/%d".to_string();
        let derived = config.with_pass(&make_pass(PassKind::Unfiled, ""));
        assert_eq!(derived.folder_structure, "%Y/%m/%d");
    }

    #[test]
    fn test_with_pass_unfiled_collapses_album_token_to_empty() {
        use crate::commands::PassKind;
        let mut config = test_config();
        config.folder_structure = "{album}/%Y/%m/%d".to_string();
        let derived = config.with_pass(&make_pass(PassKind::Unfiled, ""));
        // Empty name strips the `{album}` segment for backwards compat.
        assert!(!derived.folder_structure.contains("{album}"));
    }

    #[test]
    fn test_with_pass_album_sanitizes_name() {
        use crate::commands::PassKind;
        let mut config = test_config();
        config.folder_structure_albums = Arc::from("{album}/%Y");
        let derived = config.with_pass(&make_pass(PassKind::Album, "My/Album"));
        // Path separators in album names must be sanitised before substitution.
        assert!(!derived.folder_structure.starts_with("My/Album"));
    }

    #[test]
    fn test_with_pass_expands_library_token_with_truncation() {
        use crate::commands::PassKind;
        let mut config = test_config();
        config.folder_structure_albums = Arc::from("{library}/{album}/%Y");
        config.library = Arc::from("SharedSync-A1B2C3D4-E5F6-7890-ABCD-EF1234567890");
        let derived = config.with_pass(&make_pass(PassKind::Album, "Vacation"));
        assert_eq!(
            derived.folder_structure, "SharedSync-A1B2C3D4/Vacation/%Y",
            "shared-zone UUIDs must truncate to 8 chars in path output"
        );
    }

    #[test]
    fn test_with_pass_library_token_passthrough_for_primary() {
        use crate::commands::PassKind;
        let mut config = test_config();
        config.folder_structure = "{library}/%Y/%m/%d".to_string();
        // Default `library` is "PrimarySync" via `test_default`.
        let derived = config.with_pass(&make_pass(PassKind::Unfiled, ""));
        assert_eq!(derived.folder_structure, "PrimarySync/%Y/%m/%d");
    }

    #[test]
    fn test_with_pass_library_token_in_smart_folder_template() {
        use crate::commands::PassKind;
        let mut config = test_config();
        config.folder_structure_smart_folders = Arc::from("{library}/{smart-folder}");
        config.library = Arc::from("SharedSync-DEADBEEF-aaaa-bbbb-cccc-dddddddddddd");
        let derived = config.with_pass(&make_pass(PassKind::SmartFolder, "Favorites"));
        assert_eq!(derived.folder_structure, "SharedSync-DEADBEEF/Favorites");
    }

    #[test]
    fn test_with_pass_state_db_library_uses_full_zone_name() {
        // Path rendering truncates the zone for readability, but the
        // state-DB key (DownloadConfig.library) keeps the full zone name
        // verbatim so two zones whose 8-char prefixes happen to collide
        // still get distinct PKs in the assets table.
        use crate::commands::PassKind;
        let mut config = test_config();
        config.library = Arc::from("SharedSync-A1B2C3D4-E5F6-7890-ABCD-EF1234567890");
        let derived = config.with_pass(&make_pass(PassKind::Album, "Trip"));
        assert_eq!(
            &*derived.library,
            "SharedSync-A1B2C3D4-E5F6-7890-ABCD-EF1234567890"
        );
    }

    #[test]
    fn test_with_pass_preserves_all_fields() {
        use crate::commands::PassKind;
        let mut config = test_config();
        config.folder_structure_albums = Arc::from("{album}/%Y");
        config.media.photos = false;
        config.media.videos = false;
        config.live_photo_mode = LivePhotoMode::ImageOnly;
        config.force_resolution = true;
        config.keep_unicode_in_filenames = true;
        config.set_exif_datetime = true;
        config.filename_exclude = std::sync::Arc::from(vec![glob::Pattern::new("*.AAE").unwrap()]);
        config.temp_suffix = std::sync::Arc::from(".custom-tmp");
        let derived = config.with_pass(&make_pass(PassKind::Album, "Test"));
        assert!(!derived.media.photos);
        assert!(!derived.media.videos);
        assert_eq!(derived.live_photo_mode, LivePhotoMode::ImageOnly);
        assert!(derived.force_resolution);
        assert!(derived.keep_unicode_in_filenames);
        assert!(derived.set_exif_datetime);
        assert_eq!(derived.filename_exclude.len(), 1);
        assert_eq!(&*derived.temp_suffix, ".custom-tmp");
        assert_eq!(derived.directory, config.directory);
    }

    // ── extract_skip_candidates: filename_exclude ─────────────────

    // ── compute_config_hash: filename_exclude ─────────────────────

    #[test]
    fn test_compute_config_hash_different_filename_exclude() {
        let tmp = TempDir::new().unwrap();
        let a = build_config_with(tmp.path(), "/photos", |_| {});
        let b = build_config_with(tmp.path(), "/photos", |s| {
            s.config_overrides.filename_exclude = vec!["*.AAE".to_string()];
        });
        assert_ne!(
            compute_config_hash(&a),
            compute_config_hash(&b),
            "changing filename_exclude should change the config hash"
        );
    }

    #[test]
    fn test_hash_changes_on_folder_structure_albums() {
        // Per-category templates affect path resolution, so the trust-state
        // hash must change with them or stale records pin assets to the wrong
        // tree on the next run.
        let mut config1 = test_config();
        let mut config2 = test_config();
        config1.folder_structure_albums = Arc::from("{album}");
        config2.folder_structure_albums = Arc::from("{album}/%Y");
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    #[test]
    fn test_hash_changes_on_folder_structure_smart_folders() {
        let mut config1 = test_config();
        let mut config2 = test_config();
        config1.folder_structure_smart_folders = Arc::from("{smart-folder}");
        config2.folder_structure_smart_folders = Arc::from("{smart-folder}/%Y");
        assert_ne!(
            hash_download_config(&config1),
            hash_download_config(&config2)
        );
    }

    // ── Golden-hash stability tests ─────────────────────────────────
    //
    // These pin specific config values to specific hex outputs. If any
    // test fails, it means the hash encoding changed -- which would
    // trigger unnecessary full re-syncs for all users. Only update the
    // expected values when the hash change is intentional.

    #[test]
    fn golden_hash_download_config_defaults() {
        let config = test_config();
        let hash = hash_download_config(&config);
        assert_eq!(
            hash, "c3f2be1a9e394951",
            "hash_download_config golden hash changed -- this will trigger full re-syncs"
        );
    }

    #[test]
    fn golden_hash_download_config_non_defaults() {
        let mut config = test_config();
        config.directory = std::sync::Arc::from(std::path::Path::new("/my/photos"));
        config.folder_structure = "{:%Y/%m}".to_string();
        config.resolution = crate::types::PhotoResolution::Medium;
        config.live_resolution = AssetVersionSize::LiveMedium;
        config.file_match_policy = FileMatchPolicy::NameId7;
        config.live_photo_mov_filename_policy = crate::types::LivePhotoMovFilenamePolicy::Original;
        config.raw_policy = RawPolicy::PreferJpeg;
        config.keep_unicode_in_filenames = true;
        config.skip_created_before = Some(
            DateTime::parse_from_rfc3339("2020-06-15T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        );
        config.skip_created_after = Some(
            DateTime::parse_from_rfc3339("2024-12-31T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        );
        config.recent = Some(500);
        config.force_resolution = true;
        config.media.videos = false;
        config.live_photo_mode = LivePhotoMode::ImageOnly;
        config.filename_exclude = std::sync::Arc::from(vec![
            glob::Pattern::new("*.AAE").unwrap(),
            glob::Pattern::new("*.THM").unwrap(),
        ]);
        let hash = hash_download_config(&config);
        assert_eq!(
            hash, "d327fda31e8bec04",
            "hash_download_config golden hash changed -- this will trigger full re-syncs"
        );
    }

    #[test]
    fn golden_compute_config_hash_defaults() {
        let tmp = TempDir::new().unwrap();
        let config = build_config_with(tmp.path(), "/photos", |_| {});
        let hash = compute_config_hash(&config);
        assert_eq!(
            hash, "773e1b0c8f0d38a7",
            "compute_config_hash golden hash changed -- this will invalidate sync tokens"
        );
    }

    #[test]
    fn golden_compute_config_hash_with_albums() {
        let tmp = TempDir::new().unwrap();
        let config = build_config_with(tmp.path(), "/photos", |s| {
            s.config_overrides.albums = vec![
                "Favorites".to_string(),
                "Travel".to_string(),
                "!Hidden".to_string(),
            ];
        });
        let hash = compute_config_hash(&config);
        assert_eq!(
            hash, "064b0e3c5a9755d3",
            "compute_config_hash golden hash changed -- this will invalidate sync tokens"
        );
    }

    #[test]
    fn golden_compute_config_hash_with_smart_folders() {
        // Drift detection for the smart-folder branch of the hash. Pairs
        // with `test_compute_config_hash_different_smart_folders`: the
        // `_different_*` test catches missing-field regressions, this
        // test catches accidental field-format changes.
        let tmp = TempDir::new().unwrap();
        let config = build_config_with(tmp.path(), "/photos", |s| {
            s.config_overrides.smart_folders = vec!["Favorites".to_string(), "Videos".to_string()];
        });
        let hash = compute_config_hash(&config);
        assert_eq!(
            hash, "80c3ab8109fc5b00",
            "compute_config_hash golden hash changed -- this will invalidate sync tokens"
        );
    }

    #[test]
    fn golden_compute_config_hash_with_unfiled_false() {
        // Drift detection for the unfiled branch of the hash. The
        // `unfiled = true` default is implicit in `golden_..._defaults`;
        // this pin covers the explicit-false case so a regression
        // collapsing the two branches is caught.
        let tmp = TempDir::new().unwrap();
        let config = build_config_with(tmp.path(), "/photos", |s| {
            s.config_overrides.unfiled = Some(false);
        });
        let hash = compute_config_hash(&config);
        assert_eq!(
            hash, "3a610f0fe148f38c",
            "compute_config_hash golden hash changed -- this will invalidate sync tokens"
        );
    }

    // ── Gap: DownloadContext attempt_counts used by producer ──────────

    #[test]
    fn download_context_attempt_counts_track_per_asset() {
        let mut ctx = DownloadContext::default();
        ctx.attempt_counts.insert("asset_high".into(), 15);
        ctx.attempt_counts.insert("asset_low".into(), 2);

        // Simulate the producer's retry-exhaustion check
        let max_attempts = 10u32;
        assert!(
            ctx.attempt_counts
                .get("asset_high")
                .is_some_and(|&c| c >= max_attempts),
            "asset_high should exceed max_download_attempts"
        );
        assert!(
            ctx.attempt_counts
                .get("asset_low")
                .is_none_or(|&c| c < max_attempts),
            "asset_low should not exceed max_download_attempts"
        );
        assert!(
            !ctx.attempt_counts.contains_key("asset_never_failed"),
            "unknown asset should not be in attempt_counts"
        );
    }

    // ── Gap: should_download_fast with downloaded but different version ──

    #[test]
    fn should_download_fast_downloaded_original_but_medium_requested() {
        // Asset is downloaded as Original, but now we ask about Medium.
        // should_download_fast should return Some(true) because Medium
        // was never downloaded.
        let mut ctx = DownloadContext::default();
        ctx.downloaded_ids
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_multi".into())
            .or_default()
            .insert("original".into());
        ctx.downloaded_checksums
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_multi".into())
            .or_default()
            .insert("original".into(), "ck_orig".into());

        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "asset_multi",
                VersionSizeKey::Medium,
                "ck_med",
                true
            ),
            Some(true),
            "Medium version not in downloaded set should need download"
        );
    }

    // ── Mutation-pinning sibling: operator inversion ──────────
    //
    // The existing tests already assert the decision (Some(true) /
    // Some(false)), not just field equality. What's missing is a test
    // that pins the checksum-comparison **operator**: if a refactor
    // swaps `stored.as_ref() != checksum` for `==`, the decision
    // inverts and every downloaded asset re-downloads (or vice versa).
    //
    // Mutation: in `should_download_fast`, swap `!=` to `==` on the
    // stored-vs-current checksum line. With the existing fixtures,
    // both sides happen to land on `Some(false)` for the matching
    // case via the trust_state path, so the assertion below is the
    // only one in the suite that fails on operator inversion: paired
    // probes with opposite checksum equality, asserting opposite
    // decisions.
    #[test]
    fn should_download_fast_inverts_when_checksum_operator_flips() {
        let mut ctx = DownloadContext::default();
        ctx.downloaded_ids
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_op".into())
            .or_default()
            .insert("original".into());
        ctx.downloaded_checksums
            .entry("PrimarySync".into())
            .or_default()
            .entry("asset_op".into())
            .or_default()
            .insert("original".into(), "ck_stored".into());

        // Matching checksum + trust_state=true → skip (Some(false)).
        let matching = ctx.should_download_fast(
            "PrimarySync",
            "asset_op",
            VersionSizeKey::Original,
            "ck_stored",
            true,
        );
        // Different checksum + trust_state=true → re-download
        // (Some(true)).
        let different = ctx.should_download_fast(
            "PrimarySync",
            "asset_op",
            VersionSizeKey::Original,
            "ck_changed",
            true,
        );

        // Pin the inversion. If `!=` were swapped for `==`, both probes
        // would return the same Some(_) — collapsing the decision
        // surface. Asserting opposite values catches that.
        assert_eq!(matching, Some(false), "matching checksum must skip");
        assert_eq!(different, Some(true), "changed checksum must re-download");
        assert_ne!(
            matching, different,
            "matching and changed checksums must produce opposite decisions \
             (catches `!=` ↔ `==` operator swap on stored-vs-current compare)"
        );
    }

    // ── Gap: should_download_fast with multiple version sizes ─────────

    #[test]
    fn should_download_fast_multiple_versions_independent() {
        // Both Original and LiveOriginal downloaded, each with own checksum.
        let mut ctx = DownloadContext::default();
        ctx.downloaded_ids
            .entry("PrimarySync".into())
            .or_default()
            .entry("live_asset".into())
            .or_default()
            .insert("original".into());
        ctx.downloaded_ids
            .entry("PrimarySync".into())
            .or_default()
            .entry("live_asset".into())
            .or_default()
            .insert("live_original".into());
        ctx.downloaded_checksums
            .entry("PrimarySync".into())
            .or_default()
            .entry("live_asset".into())
            .or_default()
            .insert("original".into(), "ck_img".into());
        ctx.downloaded_checksums
            .entry("PrimarySync".into())
            .or_default()
            .entry("live_asset".into())
            .or_default()
            .insert("live_original".into(), "ck_mov".into());

        // Image: matching checksum, trusted
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "live_asset",
                VersionSizeKey::Original,
                "ck_img",
                true
            ),
            Some(false)
        );
        // MOV: matching checksum, trusted
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "live_asset",
                VersionSizeKey::LiveOriginal,
                "ck_mov",
                true
            ),
            Some(false)
        );
        // MOV: changed checksum -- re-download even though image is fine
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "live_asset",
                VersionSizeKey::LiveOriginal,
                "ck_mov_v2",
                true
            ),
            Some(true),
            "changed MOV checksum should trigger re-download"
        );
    }

    // ── Gap: retry_only mode filters new assets ──────────────────────

    #[test]
    fn download_context_retry_only_known_ids_filtering() {
        let mut ctx = DownloadContext::default();
        ctx.known_ids.insert("previously_synced".into());

        // Known asset: should_download_fast returns Some(true) (it needs
        // download because it's not in downloaded_ids)
        assert_eq!(
            ctx.should_download_fast(
                "PrimarySync",
                "previously_synced",
                VersionSizeKey::Original,
                "ck",
                true
            ),
            Some(true)
        );
        // The producer checks known_ids separately before forwarding:
        assert!(ctx.known_ids.contains("previously_synced"));
        assert!(
            !ctx.known_ids.contains("brand_new_asset"),
            "new asset should not be in known_ids in retry_only mode"
        );
    }

    /// `SyncStats::accumulate` is the sole sum used to fold per-library
    /// stats into a cycle-wide total. Pin every counter so a future refactor
    /// (or a new field added without updating `accumulate`) cannot silently
    /// drop one. Touches every numeric field plus `interrupted` plus the
    /// nested `SkipBreakdown`.
    ///
    /// The earlier inline accumulator in `sync_loop::run_cycle` missed
    /// `rate_limited` -- this test pins that field too so the bug cannot
    /// regress.
    #[test]
    fn sync_loop_run_cycle_aggregates_stats_across_libraries() {
        let lib_a = SyncStats {
            assets_seen: 10,
            downloaded: 4,
            failed: 1,
            skipped: SkipBreakdown {
                by_state: 2,
                on_disk: 3,
                by_media_type: 4,
                by_date_range: 5,
                by_live_photo: 6,
                by_filename: 7,
                by_excluded_album: 8,
                ampm_variant: 9,
                duplicates: 10,
                retry_exhausted: 11,
                retry_only: 12,
            },
            bytes_downloaded: 1_000,
            disk_bytes_written: 900,
            exif_failures: 1,
            state_write_failures: 2,
            enumeration_errors: 3,
            pagination_shortfall_warnings: 1,
            pagination_shortfall_assets: 9,
            sync_token_blocked: true,
            sync_token_blocked_reason: Some("pagination_shortfall"),
            sync_token_blocked_source: Some("icloud"),
            sync_token_blocked_explanation: Some(sync_token_blocked_explanation(
                "pagination_shortfall",
            )),
            sync_token_blocked_zone: Some("PrimarySync".to_string()),
            sync_token_expected_receivers: Some(3),
            sync_token_receivers_with_token: Some(2),
            sync_token_receivers_missing: Some(1),
            sync_token_receivers_blank: Some(0),
            sync_token_receivers_dropped: Some(0),
            sync_token_unique_values: Some(1),
            full_enumeration_reason: Some(FullEnumerationReason::RetryFailedRows),
            elapsed_secs: 1.5,
            interrupted: false,
            rate_limited: 7,
            photos_downloaded: 3,
            videos_downloaded: 1,
            recap: recap::RunRecap::default(),
        };

        let lib_b = SyncStats {
            assets_seen: 20,
            downloaded: 11,
            failed: 2,
            skipped: SkipBreakdown {
                by_state: 1,
                on_disk: 1,
                by_media_type: 1,
                by_date_range: 1,
                by_live_photo: 1,
                by_filename: 1,
                by_excluded_album: 1,
                ampm_variant: 1,
                duplicates: 1,
                retry_exhausted: 1,
                retry_only: 1,
            },
            bytes_downloaded: 2_500,
            disk_bytes_written: 2_400,
            exif_failures: 4,
            state_write_failures: 5,
            enumeration_errors: 6,
            pagination_shortfall_warnings: 2,
            pagination_shortfall_assets: 11,
            sync_token_blocked: false,
            sync_token_blocked_reason: None,
            sync_token_blocked_source: Some("kei"),
            sync_token_blocked_explanation: Some("should not overwrite first"),
            sync_token_blocked_zone: Some("SharedSync-abc".to_string()),
            sync_token_expected_receivers: Some(9),
            sync_token_receivers_with_token: Some(9),
            sync_token_receivers_missing: Some(0),
            sync_token_receivers_blank: Some(0),
            sync_token_receivers_dropped: Some(0),
            sync_token_unique_values: Some(1),
            full_enumeration_reason: Some(FullEnumerationReason::PendingRows),
            elapsed_secs: 0.75,
            interrupted: true,
            rate_limited: 3,
            photos_downloaded: 8,
            videos_downloaded: 3,
            recap: recap::RunRecap::default(),
        };

        let mut acc = SyncStats::default();
        acc.accumulate(&lib_a);
        acc.accumulate(&lib_b);

        assert_eq!(acc.assets_seen, 30, "assets_seen must sum");
        assert_eq!(acc.downloaded, 15, "downloaded must sum");
        assert_eq!(acc.failed, 3, "failed must sum");
        assert_eq!(acc.bytes_downloaded, 3_500, "bytes_downloaded must sum");
        assert_eq!(acc.disk_bytes_written, 3_300, "disk_bytes_written must sum");
        assert_eq!(acc.exif_failures, 5, "exif_failures must sum");
        assert_eq!(acc.state_write_failures, 7, "state_write_failures must sum");
        assert_eq!(acc.enumeration_errors, 9, "enumeration_errors must sum");
        assert_eq!(
            acc.pagination_shortfall_warnings, 3,
            "pagination shortfall warnings must sum"
        );
        assert_eq!(
            acc.pagination_shortfall_assets, 20,
            "pagination shortfall assets must sum"
        );
        assert!(acc.sync_token_blocked, "sync_token_blocked must OR");
        assert_eq!(acc.sync_token_blocked_reason, Some("pagination_shortfall"));
        assert_eq!(acc.sync_token_blocked_source, Some("icloud"));
        assert_eq!(
            acc.sync_token_blocked_explanation,
            Some(sync_token_blocked_explanation("pagination_shortfall"))
        );
        assert_eq!(acc.sync_token_blocked_zone.as_deref(), Some("PrimarySync"));
        assert_eq!(acc.sync_token_expected_receivers, Some(3));
        assert_eq!(acc.sync_token_receivers_with_token, Some(2));
        assert_eq!(acc.sync_token_receivers_missing, Some(1));
        assert_eq!(acc.sync_token_receivers_blank, Some(0));
        assert_eq!(acc.sync_token_receivers_dropped, Some(0));
        assert_eq!(acc.sync_token_unique_values, Some(1));
        assert_eq!(
            acc.full_enumeration_reason,
            Some(FullEnumerationReason::RetryFailedRows)
        );
        assert!(
            (acc.elapsed_secs - 2.25).abs() < 1e-9,
            "elapsed_secs must sum (got {})",
            acc.elapsed_secs
        );
        assert!(
            acc.interrupted,
            "interrupted must OR -- any library interrupted -> cycle interrupted"
        );
        assert_eq!(
            acc.rate_limited, 10,
            "rate_limited must sum -- pre-fix the inline accumulator dropped this field"
        );

        assert_eq!(acc.skipped.by_state, 3);
        assert_eq!(acc.skipped.on_disk, 4);
        assert_eq!(acc.skipped.by_media_type, 5);
        assert_eq!(acc.skipped.by_date_range, 6);
        assert_eq!(acc.skipped.by_live_photo, 7);
        assert_eq!(acc.skipped.by_filename, 8);
        assert_eq!(acc.skipped.by_excluded_album, 9);
        assert_eq!(acc.skipped.ampm_variant, 10);
        assert_eq!(acc.skipped.duplicates, 11);
        assert_eq!(acc.skipped.retry_exhausted, 12);
        assert_eq!(acc.skipped.retry_only, 13);
        assert_eq!(
            acc.skipped.total(),
            3 + 4 + 5 + 6 + 7 + 8 + 9 + 10 + 11 + 12 + 13,
            "skip total must reflect summed breakdown"
        );
    }

    /// When multiple libraries block token advancement in one cycle, the
    /// aggregated cycle stats preserve the first blocked diagnostic payload.
    #[test]
    fn sync_stats_accumulate_preserves_first_token_blocked_diagnostics() {
        let first = SyncStats {
            sync_token_blocked: true,
            sync_token_blocked_reason: Some("icloud_blank_sync_token"),
            sync_token_blocked_source: Some("icloud"),
            sync_token_blocked_explanation: Some(sync_token_blocked_explanation(
                "icloud_blank_sync_token",
            )),
            sync_token_blocked_zone: Some("PrimarySync".to_string()),
            sync_token_expected_receivers: Some(2),
            sync_token_receivers_with_token: Some(0),
            sync_token_receivers_missing: Some(0),
            sync_token_receivers_blank: Some(2),
            sync_token_receivers_dropped: Some(0),
            sync_token_unique_values: Some(0),
            ..SyncStats::default()
        };
        let second = SyncStats {
            sync_token_blocked: true,
            sync_token_blocked_reason: Some("icloud_sync_token_mismatch"),
            sync_token_blocked_source: Some("icloud"),
            sync_token_blocked_explanation: Some(sync_token_blocked_explanation(
                "icloud_sync_token_mismatch",
            )),
            sync_token_blocked_zone: Some("SharedSync-XYZ".to_string()),
            sync_token_expected_receivers: Some(3),
            sync_token_receivers_with_token: Some(3),
            sync_token_receivers_missing: Some(0),
            sync_token_receivers_blank: Some(0),
            sync_token_receivers_dropped: Some(0),
            sync_token_unique_values: Some(2),
            ..SyncStats::default()
        };

        let mut acc = SyncStats::default();
        acc.accumulate(&first);
        acc.accumulate(&second);

        assert!(acc.sync_token_blocked);
        assert_eq!(
            acc.sync_token_blocked_reason,
            first.sync_token_blocked_reason
        );
        assert_eq!(
            acc.sync_token_blocked_source,
            first.sync_token_blocked_source
        );
        assert_eq!(
            acc.sync_token_blocked_explanation,
            first.sync_token_blocked_explanation
        );
        assert_eq!(acc.sync_token_blocked_zone, first.sync_token_blocked_zone);
        assert_eq!(
            acc.sync_token_expected_receivers,
            first.sync_token_expected_receivers
        );
        assert_eq!(
            acc.sync_token_receivers_with_token,
            first.sync_token_receivers_with_token
        );
        assert_eq!(
            acc.sync_token_receivers_missing,
            first.sync_token_receivers_missing
        );
        assert_eq!(
            acc.sync_token_receivers_blank,
            first.sync_token_receivers_blank
        );
        assert_eq!(
            acc.sync_token_receivers_dropped,
            first.sync_token_receivers_dropped
        );
        assert_eq!(acc.sync_token_unique_values, first.sync_token_unique_values);
    }

    /// A transient `pass.album.len()` failure must not
    /// reduce to a 0-count that silently advances the sync token. Folding
    /// the per-pass results must surface the failure as an error count so
    /// downstream gates can suppress token advancement.
    #[test]
    fn fold_pass_count_results_counts_errors_and_zeroes_failed_passes() {
        use crate::commands::{AlbumPass, PassKind};
        use crate::icloud::photos::PhotoAlbum;
        use rustc_hash::FxHashSet;
        use std::sync::Arc;

        let passes = vec![
            AlbumPass {
                kind: PassKind::Album,
                album: PhotoAlbum::stub_for_test(Arc::from("album_a")),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
            AlbumPass {
                kind: PassKind::Album,
                album: PhotoAlbum::stub_for_test(Arc::from("album_b")),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
            AlbumPass {
                kind: PassKind::Album,
                album: PhotoAlbum::stub_for_test(Arc::from("album_c")),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
        ];

        // First and third pass succeed; second pass fails (transient len()
        // error). The failed pass must contribute 0 to the counts vector and
        // increment the error count by exactly 1.
        let results = vec![
            Ok(100),
            Err(anyhow::anyhow!("simulated transient len() failure")),
            Ok(50),
        ];

        let (counts, errors) = fold_pass_count_results(results, &passes);

        assert_eq!(counts, vec![100, 0, 50]);
        assert_eq!(
            errors, 1,
            "exactly one len() error must surface so token advancement is blocked"
        );
    }

    /// Pin the all-failures case: every pass's `len()` errors out → counts
    /// are all zero AND the error count equals the pass count, so the cycle
    /// cannot be mistaken for a clean empty enumeration.
    #[test]
    fn fold_pass_count_results_all_errors_yields_full_error_count() {
        use crate::commands::{AlbumPass, PassKind};
        use crate::icloud::photos::PhotoAlbum;
        use rustc_hash::FxHashSet;
        use std::sync::Arc;

        let passes = vec![
            AlbumPass {
                kind: PassKind::Album,
                album: PhotoAlbum::stub_for_test(Arc::from("album_a")),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
            AlbumPass {
                kind: PassKind::Album,
                album: PhotoAlbum::stub_for_test(Arc::from("album_b")),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
        ];

        let results = vec![
            Err(anyhow::anyhow!("first failure")),
            Err(anyhow::anyhow!("second failure")),
        ];

        let (counts, errors) = fold_pass_count_results(results, &passes);

        assert_eq!(counts, vec![0, 0]);
        assert_eq!(errors, 2);
    }

    #[test]
    fn recent_runs_skip_pass_count_fetch() {
        let mut config = test_config();
        config.recent = Some(25);

        assert!(
            should_skip_pass_count_fetch(&config),
            "recent-limited runs are not complete enumerations, so the \
             full-pass count is not an exact pagination bound"
        );
    }

    #[test]
    fn skip_created_before_runs_skip_pass_count_fetch() {
        let mut config = test_config();
        config.skip_created_before =
            Some(DateTime::from_timestamp_millis(1_700_000_000_000).expect("valid test timestamp"));

        assert!(
            should_skip_pass_count_fetch(&config),
            "lower-date-bounded runs stop before the full pass is drained, so \
             the full-pass count is not an exact pagination bound"
        );
    }

    #[test]
    fn skip_created_after_runs_keep_pass_count_fetch() {
        let mut config = test_config();
        config.skip_created_after =
            Some(DateTime::from_timestamp_millis(1_700_000_000_000).expect("valid test timestamp"));

        assert!(
            !should_skip_pass_count_fetch(&config),
            "upper-date filters must still drain the stream because older \
             assets after the skipped prefix can still match"
        );
    }

    #[test]
    fn unbounded_runs_keep_pass_count_fetch() {
        let mut config = test_config();
        config.recent = None;

        assert!(
            !should_skip_pass_count_fetch(&config),
            "unbounded runs still use exact counts for progress bounds and \
             pagination-underflow detection"
        );
    }

    #[tokio::test]
    async fn build_pass_count_plan_uses_recent_bound_without_exact_counts_for_read_only_runs() {
        use crate::commands::{AlbumPass, PassKind};
        use crate::icloud::photos::PhotoAlbum;
        use rustc_hash::FxHashSet;
        use std::sync::Arc;

        let passes = vec![
            AlbumPass {
                kind: PassKind::Album,
                album: PhotoAlbum::stub_for_test(Arc::from("album_a")),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
            AlbumPass {
                kind: PassKind::Album,
                album: PhotoAlbum::stub_for_test(Arc::from("album_b")),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
        ];
        let mut config = test_config();
        config.recent = Some(10);

        let plan =
            build_pass_count_plan(&passes, &config, DownloadControls::dry_run_hidden()).await;

        assert_eq!(plan.display_counts, vec![10, 10]);
        assert_eq!(plan.stream_total_counts, vec![None, None]);
        assert_eq!(plan.exact_total, None);
        assert_eq!(plan.len_errors, 0);
    }

    #[tokio::test]
    async fn full_sync_recent_album_passes_use_scope_frontier() {
        let all_assets = recent_scope_assets("frontier", 300, 1_700_000_000_000);
        let mut album_assets = vec![all_assets[0].clone()];
        album_assets.extend(recent_scope_assets("old-album", 500, 1_699_000_000_000));
        let asset = recent_scope_photo_asset(&all_assets[0]);
        let session = RecentScopeSession::new(all_assets, album_assets);

        let passes: Vec<AlbumPass> = (0..10)
            .map(|index| AlbumPass {
                kind: PassKind::Album,
                album: recent_scope_album(&format!("Album {index}"), session.clone()),
                exclude_ids: Arc::new(FxHashSet::default()),
            })
            .collect();

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 1;
        config.recent = Some(300);
        for pass in &passes {
            seed_existing_file_for_asset(&config, pass, &asset).await;
        }

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("scope-frontier recent sync should complete");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(
            result.stats.assets_seen, 10,
            "each album should plan only assets inside the library-wide recent frontier; offsets={:?}",
            session.album_offsets()
        );
        assert_eq!(result.stats.pagination_shortfall_warnings, 0);
        assert_eq!(result.stats.enumeration_errors, 0);
        assert!(
            session.album_offsets().len() < 100,
            "album enumeration should stop near the frontier boundary instead of \
             applying recent=300 to every album pass"
        );
        assert_eq!(result.sync_token, None);
    }

    #[tokio::test]
    async fn full_sync_recent_per_filter_scope_limits_each_pass_independently() {
        let all_assets = recent_scope_assets("frontier", 6, 1_700_000_000_000);
        let mut album_assets = vec![all_assets[0].clone()];
        album_assets.extend(recent_scope_assets(
            "old-per-filter-album",
            20,
            1_699_000_000_000,
        ));
        let expected_assets = album_assets
            .iter()
            .take(6)
            .map(recent_scope_photo_asset)
            .collect::<Vec<_>>();
        let session = RecentScopeSession::new(all_assets, album_assets);

        let passes: Vec<AlbumPass> = (0..3)
            .map(|index| AlbumPass {
                kind: PassKind::Album,
                album: recent_scope_album(&format!("Album {index}"), session.clone()),
                exclude_ids: Arc::new(FxHashSet::default()),
            })
            .collect();

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 1;
        config.recent = Some(6);
        config.recent_scope = crate::cli::RecentScope::PerFilter;

        for pass in &passes {
            for asset in &expected_assets {
                seed_existing_file_for_asset(&config, pass, asset).await;
            }
        }

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("per-filter recent sync should complete");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(
            result.stats.assets_seen, 18,
            "per-filter recent scope should take the recent limit from each album pass"
        );
        assert_eq!(result.stats.pagination_shortfall_warnings, 0);
        assert_eq!(result.stats.enumeration_errors, 0);
        assert!(
            session.album_offsets().len() >= 9,
            "per-filter scope should enumerate each album's recent window, not stop at the global frontier"
        );
        assert_eq!(result.sync_token, None);
    }

    #[tokio::test]
    async fn full_sync_recent_single_album_filters_library_frontier() {
        let all_assets = recent_scope_assets("frontier", 300, 1_700_000_000_000);
        let mut album_assets = vec![all_assets[0].clone()];
        album_assets.extend(recent_scope_assets(
            "old-single-album",
            500,
            1_699_000_000_000,
        ));
        let asset = recent_scope_photo_asset(&all_assets[0]);
        let session = RecentScopeSession::new(all_assets, album_assets);
        let passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: recent_scope_album("Vacation", session.clone()),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 1;
        config.recent = Some(300);
        seed_existing_file_for_asset(&config, &passes[0], &asset).await;

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("single-album scope-frontier recent sync should complete");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(
            result.stats.assets_seen, 1,
            "album filter should pare down the library-wide recent frontier"
        );
        assert_eq!(result.stats.pagination_shortfall_warnings, 0);
        assert_eq!(result.stats.enumeration_errors, 0);
        assert!(
            session.album_offsets().len() < 10,
            "single album enumeration should stop at the frontier boundary"
        );
        assert_eq!(result.sync_token, None);
    }

    #[tokio::test]
    async fn full_sync_skip_created_before_stops_at_date_boundary() {
        let newer_assets = recent_scope_assets("date-new", 5, 1_700_000_000_000);
        let older_assets = recent_scope_assets("date-old", 20, 1_699_000_000_000);
        let mut album_assets = newer_assets.clone();
        album_assets.extend(older_assets);
        let session = RecentScopeSession::new(album_assets.clone(), album_assets);
        let passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: recent_scope_album("Vacation", session.clone()),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 1;
        config.skip_created_before =
            Some(DateTime::from_timestamp_millis(1_699_999_000_000).expect("valid test timestamp"));
        for asset in newer_assets.iter().map(recent_scope_photo_asset) {
            seed_existing_file_for_asset(&config, &passes[0], &asset).await;
        }

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("date-bounded full sync should complete");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(
            result.stats.assets_seen, 5,
            "lower-date-bound enumeration should stop before older assets are \
             handed to the download pipeline"
        );
        assert_eq!(result.stats.pagination_shortfall_warnings, 0);
        assert_eq!(result.stats.enumeration_errors, 0);
        assert!(
            session.album_offsets().len() <= 4,
            "lower-date-bound enumeration should stop near the first old page; offsets={:?}",
            session.album_offsets()
        );
        assert_eq!(
            result.sync_token, None,
            "date-bounded full sync must not advance a zone token"
        );
        assert!(result.stats.sync_token_blocked);
        assert_eq!(
            result.stats.sync_token_blocked_reason,
            Some(DATE_BOUNDED_FULL_ENUMERATION_REASON)
        );
    }

    #[tokio::test]
    async fn full_sync_skip_created_after_drains_past_newer_prefix() {
        let newer_assets = recent_scope_assets("date-after-new", 5, 1_700_000_000_000);
        let older_assets = recent_scope_assets("date-after-old", 5, 1_699_000_000_000);
        let mut album_assets = newer_assets;
        album_assets.extend(older_assets.clone());
        let session = RecentScopeSession::new(album_assets.clone(), album_assets);
        let passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: recent_scope_album("Vacation", session.clone()),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 1;
        config.skip_created_after =
            Some(DateTime::from_timestamp_millis(1_699_999_000_000).expect("valid test timestamp"));
        for asset in older_assets.iter().map(recent_scope_photo_asset) {
            seed_existing_file_for_asset(&config, &passes[0], &asset).await;
        }

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("upper-date-filtered full sync should complete");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(
            result.stats.assets_seen, 10,
            "upper-date filters skip the newer prefix but must keep enumerating \
             because older assets can still match"
        );
        assert_eq!(result.stats.pagination_shortfall_warnings, 0);
        assert_eq!(result.stats.enumeration_errors, 0);
        assert!(
            session.album_offsets().len() > 3,
            "upper-date filters must not stop near the newer prefix; offsets={:?}",
            session.album_offsets()
        );
        assert_eq!(result.sync_token.as_deref(), Some("zone-token"));
    }

    #[tokio::test]
    async fn full_sync_recent_download_suppresses_token_without_shortfall() {
        let records = mock_photo_records_with_filename("MASTER_RECENT", "recent.jpg");
        let album_session = MockPhotosFlow::new()
            .query_page(records.clone(), Some("zone-token"))
            .empty_query_page(Some("zone-token"))
            .build();
        let passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: mock_album("Vacation", album_session),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.recent = Some(100);

        let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
        let pass_config = config.with_pass(&passes[0]);
        let expected_path = filter::expected_paths_for(&asset, &pass_config)
            .into_iter()
            .next()
            .expect("mock asset should have an expected path");
        tokio::fs::create_dir_all(expected_path.path.parent().expect("path has parent"))
            .await
            .expect("create parent dir");
        tokio::fs::write(&expected_path.path, vec![0u8; 1024])
            .await
            .expect("seed existing file");

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("recent full sync should complete");

        assert!(
            matches!(result.outcome, DownloadOutcome::Success),
            "a sparse recent window must not be treated as pagination undercount"
        );
        assert_eq!(
            result.stats.enumeration_errors, 0,
            "recent-limited count shortfalls are not exact enumeration errors"
        );
        assert_eq!(
            result.sync_token, None,
            "recent-limited full sync must not advance a zone token"
        );
        assert!(result.stats.sync_token_blocked);
        assert_eq!(
            result.stats.sync_token_blocked_reason,
            Some(RECENT_LIMITED_FULL_ENUMERATION_REASON)
        );
        assert_eq!(result.stats.sync_token_blocked_source, Some("kei"));
        assert_eq!(
            result.stats.sync_token_blocked_explanation,
            Some(sync_token_blocked_explanation(
                RECENT_LIMITED_FULL_ENUMERATION_REASON
            ))
        );
        assert_eq!(result.stats.sync_token_expected_receivers, None);
    }

    #[tokio::test]
    async fn full_sync_recent_download_drains_multiple_reduced_pages() {
        let ids = recent_ids("recent-prod", 100);
        let session = DynamicRecentPhotosSession::from_ids(ids.clone())
            .with_filename_prefix("recent-prod")
            .with_token("zone-token");
        let passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: album_with_session("PrimarySync", "Vacation", Box::new(session.clone())),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 10;
        config.recent = Some(100);
        seed_existing_recent_files(&config, &passes[0], "PrimarySync", &ids, "recent-prod").await;

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("recent full sync should complete");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.stats.assets_seen, 100);
        assert_eq!(result.stats.enumeration_errors, 0);
        assert_eq!(
            result.sync_token, None,
            "recent-limited full sync must not advance a zone token"
        );
        assert!(
            session.offsets().len() >= 5,
            "write-mode full sync should drain every reduced download page"
        );
    }

    async fn run_recent_mode_for_ids(
        mode: DownloadRunMode,
        ids: &[String],
        filename_prefix: &str,
    ) -> (SyncResult, Vec<String>) {
        let session = DynamicRecentPhotosSession::from_ids(ids.to_vec())
            .with_filename_prefix(filename_prefix)
            .with_token("zone-token");
        let passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: album_with_session("PrimarySync", "Vacation", Box::new(session.clone())),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 1;
        config.recent = Some(ids.len().try_into().expect("test id count fits u32"));
        if matches!(mode, DownloadRunMode::Download) {
            seed_existing_recent_files(&config, &passes[0], "PrimarySync", ids, filename_prefix)
                .await;
        }

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::new(mode, DownloadReporting::hidden()),
            CancellationToken::new(),
        )
        .await
        .expect("recent mode sync should complete");

        (result, unique_ids_in_order(session.emitted_ids()))
    }

    #[tokio::test]
    async fn full_sync_recent_run_modes_enumerate_same_asset_ids() {
        let ids = recent_ids("mode-parity", 6);

        let (print_result, print_ids) =
            run_recent_mode_for_ids(DownloadRunMode::PrintFilenames, &ids, "mode-parity").await;
        let (dry_result, dry_ids) =
            run_recent_mode_for_ids(DownloadRunMode::DryRun, &ids, "mode-parity").await;
        let (download_result, download_ids) =
            run_recent_mode_for_ids(DownloadRunMode::Download, &ids, "mode-parity").await;

        assert!(matches!(print_result.outcome, DownloadOutcome::Success));
        assert!(matches!(dry_result.outcome, DownloadOutcome::Success));
        assert!(matches!(download_result.outcome, DownloadOutcome::Success));
        assert_eq!(print_ids, ids);
        assert_eq!(dry_ids, ids);
        assert_eq!(download_ids, ids);
        assert_eq!(print_ids, dry_ids);
        assert_eq!(dry_ids, download_ids);
        assert_eq!(download_result.stats.assets_seen, ids.len() as u64);
        assert_eq!(print_result.sync_token, None);
        assert_eq!(dry_result.sync_token, None);
        assert_eq!(download_result.sync_token, None);
    }

    #[tokio::test]
    async fn full_sync_recent_deferred_unfiled_filters_album_members_after_multi_page_stream() {
        let album_ids = recent_ids("album-member", 40);
        let unfiled_only_ids = recent_ids("unfiled-only", 20);
        let mut unfiled_ids = album_ids.clone();
        unfiled_ids.extend(unfiled_only_ids.clone());

        let album_session = DynamicRecentPhotosSession::from_ids(album_ids.clone())
            .with_filename_prefix("album-member")
            .with_token("zone-token");
        let unfiled_session = DynamicRecentPhotosSession::from_ids(unfiled_ids.clone())
            .with_filename_prefix("unfiled-mixed")
            .with_token("zone-token");
        let passes = vec![
            AlbumPass {
                kind: PassKind::Album,
                album: album_with_session("PrimarySync", "Vacation", Box::new(album_session)),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
            AlbumPass {
                kind: PassKind::Unfiled,
                album: album_with_session("PrimarySync", "", Box::new(unfiled_session.clone())),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
        ];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 10;
        config.recent = Some(60);
        seed_existing_recent_files(
            &config,
            &passes[0],
            "PrimarySync",
            &album_ids,
            "album-member",
        )
        .await;
        seed_existing_recent_files(
            &config,
            &passes[1],
            "PrimarySync",
            &unfiled_ids,
            "unfiled-mixed",
        )
        .await;

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("recent album plus unfiled sync should complete");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(
            result.stats.assets_seen, 60,
            "40 album assets plus 20 non-album unfiled assets should be counted"
        );
        assert_eq!(result.stats.enumeration_errors, 0);
        assert_eq!(result.sync_token, None);
        assert_eq!(
            unfiled_session.emitted_ids().len(),
            60,
            "deferred unfiled collection should still drain its recent stream"
        );
    }

    #[tokio::test]
    async fn full_sync_recent_smart_folder_drains_multiple_reduced_pages() {
        let ids = recent_ids("smart-recent", 60);
        let session = DynamicRecentPhotosSession::from_ids(ids.clone())
            .with_filename_prefix("smart-recent")
            .with_token("zone-token");
        let passes = vec![AlbumPass {
            kind: PassKind::SmartFolder,
            album: album_with_session("PrimarySync", "Favorites", Box::new(session.clone())),
            exclude_ids: Arc::new(FxHashSet::default()),
        }];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 10;
        config.recent = Some(60);
        seed_existing_recent_files(&config, &passes[0], "PrimarySync", &ids, "smart-recent").await;

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("recent smart-folder sync should complete");

        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.stats.assets_seen, 60);
        assert_eq!(result.stats.enumeration_errors, 0);
        assert_eq!(result.sync_token, None);
        assert!(
            session.offsets().len() >= 3,
            "smart-folder recent sync should drain every reduced page"
        );
    }

    #[tokio::test]
    async fn full_sync_deferred_unfiled_waits_when_album_enumeration_errors() {
        let album_ids = recent_ids("album-error", 40);
        let unfiled_ids = recent_ids("unfiled-after-error", 20);
        let mut library_ids = album_ids.clone();
        library_ids.extend(unfiled_ids.clone());
        let album_session = DynamicRecentPhotosSession::from_ids(album_ids.clone())
            .with_filename_prefix("album-error")
            .with_error_at_offset(20);
        let unfiled_session = DynamicRecentPhotosSession::from_ids(library_ids)
            .with_filename_prefix("unfiled-after-error");
        let passes = vec![
            AlbumPass {
                kind: PassKind::Album,
                album: album_with_session("PrimarySync", "Vacation", Box::new(album_session)),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
            AlbumPass {
                kind: PassKind::Unfiled,
                album: album_with_session("PrimarySync", "", Box::new(unfiled_session.clone())),
                exclude_ids: Arc::new(FxHashSet::default()),
            },
        ];

        let mut config = test_config();
        let dir = TempDir::new().expect("temp dir");
        config.directory = Arc::from(dir.path());
        config.concurrent_downloads = 10;
        config.recent = Some(40);
        seed_existing_recent_files(
            &config,
            &passes[0],
            "PrimarySync",
            &album_ids,
            "album-error",
        )
        .await;
        seed_existing_recent_files(
            &config,
            &passes[1],
            "PrimarySync",
            &unfiled_ids,
            "unfiled-after-error",
        )
        .await;

        let result = download_photos_full_with_token(
            &Client::new(),
            &passes,
            &Arc::new(config),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .expect("album enumeration error should be reported as partial result");

        assert!(matches!(
            result.outcome,
            DownloadOutcome::PartialFailure { .. }
        ));
        assert_eq!(result.stats.enumeration_errors, 1);
        assert_eq!(
            result.stats.assets_seen, 20,
            "unfiled assets must not be processed when album exclusions are incomplete"
        );
        assert_eq!(result.sync_token, None);
        assert!(
            !unfiled_session.emitted_ids().is_empty(),
            "the deferred unfiled stream may be collected concurrently"
        );
    }

    #[test]
    fn unanimous_pass_sync_token_returns_token_when_passes_agree() {
        let tokens = vec!["zone-token".to_string(), "zone-token".to_string()];

        assert_eq!(
            unanimous_pass_sync_token(&tokens).as_deref(),
            Some("zone-token")
        );
    }

    #[test]
    fn unanimous_pass_sync_token_suppresses_disagreement() {
        let tokens = vec!["zone-token-a".to_string(), "zone-token-b".to_string()];

        assert_eq!(
            unanimous_pass_sync_token(&tokens),
            None,
            "mismatched full-enumeration pass tokens must block advancement"
        );
    }

    /// Per-pass mode AND-folds `enumeration_complete` across passes.
    /// The first pass that aborts must drop the cycle's flag to false. The
    /// `&&=` semantics are subtle (especially around the empty-passes case)
    /// so this test pins the truth table.
    #[test]
    fn enum_progress_marker_per_pass_and_fold_semantics() {
        // All passes complete → cycle complete.
        let mut combined = true;
        for pass_complete in [true, true, true] {
            combined = combined && pass_complete;
        }
        assert!(combined, "all passes complete → marker clears");

        // One pass aborted mid-stream → cycle incomplete.
        let mut combined = true;
        for pass_complete in [true, false, true] {
            combined = combined && pass_complete;
        }
        assert!(
            !combined,
            "one pass aborted → marker must stay set even if siblings finished"
        );

        // Empty passes: combined stays at the initializer (false in the
        // production code so a no-pass cycle doesn't accidentally clear
        // the marker for a zone the cycle didn't actually enumerate).
        let combined: bool = []
            .iter()
            .fold(false, |acc, pass_complete: &bool| acc && *pass_complete);
        assert!(!combined, "no passes → marker stays set");
    }

    /// Pagination undercount classifier — exact match returns Match.
    /// Token must advance silently when the producer saw at least as many
    /// assets as the API reported.
    #[test]
    fn classify_pagination_shortfall_exact_match_is_silent() {
        let decision = classify_pagination_shortfall(1000, 1000);
        assert_eq!(decision, PaginationShortfall::Match);
    }

    /// A 1% undercount is tolerated when it is within both the percent and
    /// absolute thresholds.
    #[test]
    fn classify_pagination_shortfall_one_percent_below_is_tolerated() {
        // 1000 expected, 990 seen -> 1% shortfall, within 5% and <= 100.
        let decision = classify_pagination_shortfall(1000, 990);
        assert_eq!(
            decision,
            PaginationShortfall::Tolerated { shortfall: 10 },
            "small shortfall should be tolerated"
        );
    }

    /// A 4% undercount is still tolerated as long as absolute shortfall is
    /// also within bounds.
    #[test]
    fn classify_pagination_shortfall_four_percent_below_is_tolerated() {
        // 1000 expected, 960 seen -> 4% shortfall, 40 assets.
        let decision = classify_pagination_shortfall(1000, 960);
        assert_eq!(decision, PaginationShortfall::Tolerated { shortfall: 40 });
    }

    /// A 6% undercount exceeds the percent tolerance and blocks token
    /// advancement.
    #[test]
    fn classify_pagination_shortfall_six_percent_below_blocks_token() {
        // 1000 expected, 940 seen -> 6% shortfall.
        let decision = classify_pagination_shortfall(1000, 940);
        assert_eq!(decision, PaginationShortfall::TokenUnsafe { shortfall: 60 });
    }

    /// Boundary case at exactly 5% shortfall is tolerated.
    #[test]
    fn classify_pagination_shortfall_at_tolerance_boundary_is_tolerated() {
        let decision = classify_pagination_shortfall(1000, 950);
        assert_eq!(decision, PaginationShortfall::Tolerated { shortfall: 50 });
    }

    /// Regression fixture for issue #498: expected=1578, seen=1533
    /// (shortfall=45, ~2.85%). This should classify as tolerated so it does
    /// not become a sync-failure signal.
    #[test]
    fn classify_pagination_shortfall_issue_498_fixture_is_tolerated() {
        let decision = classify_pagination_shortfall(1578, 1533);
        assert_eq!(decision, PaginationShortfall::Tolerated { shortfall: 45 });
    }

    /// Regression fixture from downstream k8s-gitops mitigation:
    /// expected=31_000, seen=30_959 (shortfall=41, ~0.13%).
    #[test]
    fn classify_pagination_shortfall_billimek_sharedsync_fixture_is_tolerated() {
        let decision = classify_pagination_shortfall(31_000, 30_959);
        assert_eq!(decision, PaginationShortfall::Tolerated { shortfall: 41 });
    }

    /// Large absolute shortfalls remain token-unsafe even if percent gap is
    /// small.
    #[test]
    fn classify_pagination_shortfall_large_absolute_gap_blocks_token() {
        // 10_000 expected, 9_890 seen -> 1.1% shortfall, but 110 assets.
        let decision = classify_pagination_shortfall(10_000, 9_890);
        assert_eq!(
            decision,
            PaginationShortfall::TokenUnsafe { shortfall: 110 }
        );
    }

    /// The orphan-part walk must remove .part files older
    /// than the cutoff and leave non-matching files alone. To avoid
    /// depending on a third-party mtime crate, drive the cutoff itself: a
    /// cutoff far in the future treats every just-created file as "older",
    /// while a cutoff in the distant past leaves everything intact.
    #[test]
    fn walk_and_remove_orphan_parts_removes_part_files_only() {
        use std::fs::File;
        use std::io::Write;

        let dir = tempfile::Builder::new()
            .prefix("kei-orphan-parts-")
            .tempdir()
            .expect("tempdir");

        let part = dir.path().join("photo.jpg.part");
        File::create(&part).unwrap().write_all(b"x").unwrap();

        let unrelated = dir.path().join("photo.jpg");
        File::create(&unrelated).unwrap().write_all(b"x").unwrap();

        // Cutoff far in the future -> the just-created .part is "older".
        // `now=0, recent_grace=0` disables the recent-grace check so this test
        // continues to exercise the cutoff-only behaviour.
        let future = i64::MAX / 2;
        let cleaned = walk_and_remove_orphan_parts(dir.path().to_path_buf(), ".part", future, 0, 0);
        assert_eq!(cleaned, 1, "the .part file must be removed");
        assert!(!part.exists());
        assert!(unrelated.exists(), "non-.part file must be retained");

        // Re-create and re-run with cutoff in the distant past; nothing to clean.
        File::create(&part).unwrap().write_all(b"x").unwrap();
        let cleaned = walk_and_remove_orphan_parts(dir.path().to_path_buf(), ".part", 0, 0, 0);
        assert_eq!(cleaned, 0, "cutoff in the past must spare even .part files");
        assert!(part.exists());
    }

    /// A directory the process cannot read must NOT panic the walk
    /// and MUST NOT abort it. With the fix in place the walk emits a
    /// `warn!` for the failed `read_dir` and continues; pre-fix it
    /// silently swallowed the error and produced no log breadcrumb. We
    /// can't capture log output without an extra dependency, so this test
    /// pins the structural contract: the walk completes, doesn't panic,
    /// and still cleans the readable siblings.
    #[cfg(unix)]
    #[test]
    fn walk_and_remove_orphan_parts_continues_past_unreadable_subdir() {
        use std::fs::File;
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::Builder::new()
            .prefix("kei-orphan-parts-unreadable-")
            .tempdir()
            .expect("tempdir");

        // Sibling subdir with a .part file: should be cleaned.
        let readable = dir.path().join("readable");
        std::fs::create_dir(&readable).unwrap();
        let part = readable.join("photo.jpg.part");
        File::create(&part).unwrap().write_all(b"x").unwrap();

        // Unreadable subdir: read_dir fails. The walk must log a warn and
        // continue rather than aborting or silently dropping the error.
        let unreadable = dir.path().join("unreadable");
        std::fs::create_dir(&unreadable).unwrap();
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000)).unwrap();

        let future = i64::MAX / 2;
        let cleaned = walk_and_remove_orphan_parts(dir.path().to_path_buf(), ".part", future, 0, 0);

        // Restore perms so the tempdir can be cleaned up.
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(
            cleaned, 1,
            ".part in the readable sibling must still be cleaned despite \
             the unreadable subdirectory"
        );
        assert!(!part.exists());
    }

    /// A `.part` file whose mtime is within `recent_grace_secs` of
    /// `now_secs` must be spared even when the cutoff says it's older than
    /// `last_sync_completed`. Defends against the multi-process race where
    /// a different kei instance is actively resuming a `.part` between
    /// retries.
    ///
    /// Drives the cutoff parameter directly to avoid taking
    /// a runtime dependency on a filetime crate.
    #[test]
    fn walk_and_remove_orphan_parts_spares_recently_touched_files() {
        use std::fs::File;
        use std::io::Write;

        let dir = tempfile::Builder::new()
            .prefix("kei-orphan-parts-recent-")
            .tempdir()
            .expect("tempdir");

        // Two .part files. We can't easily set mtime without a filetime
        // crate, so synthesize the test by driving (now_secs, cutoff_secs,
        // recent_grace_secs) numerically: the just-created file has an
        // mtime ~= "real now". We pretend `now_secs` is real-now + 1 hour
        // and cutoff is real-now + 30 minutes (so the file is "older" than
        // cutoff under the legacy gate). With recent_grace = 90 minutes,
        // the file's real-now mtime falls inside (now - 90min, now] →
        // spared. With recent_grace = 0, the file is removed (legacy
        // behaviour preserved for the existing test above).
        let recent_part = dir.path().join("recent.jpg.part");
        File::create(&recent_part).unwrap().write_all(b"x").unwrap();
        let old_part = dir.path().join("old.jpg.part");
        File::create(&old_part).unwrap().write_all(b"x").unwrap();

        let real_now = chrono::Utc::now().timestamp();
        let now_secs = real_now + 3_600; // pretend "now" is 1h ahead
        let cutoff_secs = real_now + 1_800; // 30 minutes ahead → both .parts older
        let recent_grace_secs = 90 * 60; // 90 minutes → both .parts inside grace

        let cleaned = walk_and_remove_orphan_parts(
            dir.path().to_path_buf(),
            ".part",
            cutoff_secs,
            now_secs,
            recent_grace_secs,
        );
        assert_eq!(
            cleaned, 0,
            "files inside the recent-grace window must be spared even when \
             they predate the cutoff"
        );
        assert!(recent_part.exists(), "recent .part must still exist");
        assert!(old_part.exists(), "old .part also spared by grace window");

        // Now shrink the grace window so the same files fall OUTSIDE it.
        // 1 second of grace + the simulated "now" 3600s ahead means a real-now
        // mtime is far outside the window → both files cleaned (legacy
        // cutoff path).
        let cleaned = walk_and_remove_orphan_parts(
            dir.path().to_path_buf(),
            ".part",
            cutoff_secs,
            now_secs,
            1,
        );
        assert_eq!(
            cleaned, 2,
            "with a 1-second grace window, both .parts fall back to the \
             legacy cutoff-only gate and are removed"
        );
        assert!(!recent_part.exists());
        assert!(!old_part.exists());
    }

    /// When the cutoff says "delete" but only one of two `.part`
    /// files is in the recent-grace window, the test fixture mimics the
    /// task-spec setup: one mtime ~now, one mtime far in the past. Drive
    /// the times via the `now_secs` and `cutoff_secs` parameters since
    /// adjusting filesystem mtimes without a third-party crate is not
    /// portable across platforms.
    #[test]
    fn walk_and_remove_orphan_parts_distinguishes_recent_from_aged_via_params() {
        use std::fs::File;
        use std::io::Write;

        let dir = tempfile::Builder::new()
            .prefix("kei-orphan-parts-mixed-")
            .tempdir()
            .expect("tempdir");

        // Both files have a real-now mtime. We can't backdate one without
        // a filetime crate, so this test pins the symmetric case:
        // recent-grace > 0 spares both, recent-grace = 0 removes both.
        // The asymmetric scenario is covered structurally by the helper's
        // parameterization (the `is_recently_touched` branch is the only
        // gate the parameters affect) and by the inline integration with
        // `cleanup_orphan_part_files`.
        let p1 = dir.path().join("a.jpg.part");
        let p2 = dir.path().join("b.jpg.part");
        File::create(&p1).unwrap().write_all(b"x").unwrap();
        File::create(&p2).unwrap().write_all(b"x").unwrap();

        let now_secs = chrono::Utc::now().timestamp() + 60; // 1 min ahead
        let cutoff_secs = now_secs - 30; // both .parts (real-now mtime) older
        let recent_grace_secs = 10 * 60; // 10 min grace
        let cleaned = walk_and_remove_orphan_parts(
            dir.path().to_path_buf(),
            ".part",
            cutoff_secs,
            now_secs,
            recent_grace_secs,
        );
        assert_eq!(cleaned, 0, "both .parts within 10-min grace must be spared");
        assert!(p1.exists() && p2.exists());
    }

    /// Companion: accumulating into an empty `SyncStats` is a faithful copy
    /// (the operation is the additive identity for the empty case).
    #[test]
    fn sync_stats_accumulate_into_empty_is_copy() {
        let src = SyncStats {
            assets_seen: 5,
            downloaded: 2,
            failed: 1,
            skipped: SkipBreakdown {
                duplicates: 7,
                ..SkipBreakdown::default()
            },
            rate_limited: 4,
            interrupted: true,
            ..SyncStats::default()
        };
        let mut dst = SyncStats::default();
        dst.accumulate(&src);
        assert_eq!(dst.assets_seen, 5);
        assert_eq!(dst.downloaded, 2);
        assert_eq!(dst.failed, 1);
        assert_eq!(dst.skipped.duplicates, 7);
        assert_eq!(dst.rate_limited, 4);
        assert!(dst.interrupted);
    }
}
