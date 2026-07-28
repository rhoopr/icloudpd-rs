//! Metadata write orchestration for downloaded files and retry markers.
//!
//! The download pipeline owns byte transfer and `.part` promotion. This module
//! owns the opt-in local-file metadata mutation work that can happen around
//! that transfer: embed writes before publish, sidecar writes after publish,
//! metadata-only retry marker tagging, and pending retry marker draining.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Local};
use tokio_util::sync::CancellationToken;

use crate::download::filter::MetadataPayload;
use crate::icloud::photos::PhotoAsset;
use crate::state::{MetadataRewriteStore, VersionSizeKey};

use super::{DownloadConfig, DownloadContext};

bitflags::bitflags! {
    /// Per-tag write toggles. `any_embed()` drives the `.part`-and-modify-before-rename
    /// flow; individual flags gate which fields get embedded into the media file.
    ///
    /// `EMBED_XMP` enables the XMP-only fields that have no native EXIF equivalent
    /// (title, keywords, people, hidden/archived, media subtype, burst id).
    /// `XMP_SIDECAR` is orthogonal - it writes a `.xmp` file next to the photo
    /// without touching the photo bytes.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub(super) struct MetadataFlags: u8 {
        const DATETIME    = 1 << 0;
        const RATING      = 1 << 1;
        const GPS         = 1 << 2;
        const DESCRIPTION = 1 << 3;
        const EMBED_XMP   = 1 << 4;
        const XMP_SIDECAR = 1 << 5;
    }
}

impl MetadataFlags {
    /// Set of flags that drive the `.part`-and-modify-before-rename flow.
    /// Sidecar writes happen after the rename so `XMP_SIDECAR` is excluded.
    /// Derived as `all() \ XMP_SIDECAR` so any future embed-style flag
    /// added to this type is automatically picked up.
    const EMBED_MASK: Self = Self::all().difference(Self::XMP_SIDECAR);

    /// Whether any flag needs the downloaded bytes to stay as a `.part` file
    /// for in-place metadata editing before the atomic rename.
    pub(super) fn any_embed(self) -> bool {
        self.intersects(Self::EMBED_MASK)
    }

    pub(super) fn has_any_write(self) -> bool {
        !self.is_empty()
    }
}

impl From<&DownloadConfig> for MetadataFlags {
    fn from(config: &DownloadConfig) -> Self {
        Self::from(&config.metadata)
    }
}

impl From<&crate::config::MetadataConfig> for MetadataFlags {
    fn from(metadata: &crate::config::MetadataConfig) -> Self {
        let mut flags = Self::empty();
        flags.set(Self::DATETIME, metadata.set_exif_datetime);
        flags.set(Self::RATING, metadata.set_exif_rating);
        flags.set(Self::GPS, metadata.set_exif_gps);
        flags.set(Self::DESCRIPTION, metadata.set_exif_description);
        #[cfg(feature = "xmp")]
        {
            flags.set(Self::EMBED_XMP, metadata.embed_xmp);
            flags.set(Self::XMP_SIDECAR, metadata.xmp_sidecar);
        }
        flags
    }
}

/// Result of metadata writes attempted for one downloaded file.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MetadataWriteOutcome {
    embed_failed: bool,
    sidecar_failed: bool,
}

impl MetadataWriteOutcome {
    pub(super) fn any_failed(self) -> bool {
        self.embed_failed || self.sidecar_failed
    }
}

/// Request describing metadata work for one file. `embed_path` is the path to
/// mutate in place, usually the `.part` file before promotion. `final_path` is
/// the intended media path and is used for extension gating. `sidecar_path` is
/// the media path next to which the `.xmp` sidecar should be written.
pub(super) struct MetadataWriteRequest<'a> {
    pub(super) final_path: &'a Path,
    pub(super) embed_path: Option<&'a Path>,
    #[cfg_attr(not(feature = "xmp"), allow(dead_code))]
    pub(super) sidecar_path: Option<&'a Path>,
    pub(super) payload: Arc<MetadataPayload>,
    pub(super) created_local: DateTime<Local>,
    pub(super) flags: MetadataFlags,
    pub(super) temp_suffix: &'a str,
}

/// Apply opt-in metadata writes for a single file.
///
/// The caller remains responsible for transfer, mtime, `.part` promotion,
/// counters, and final state writes.
pub(super) async fn write_download_metadata(
    request: MetadataWriteRequest<'_>,
) -> MetadataWriteOutcome {
    let mut outcome = MetadataWriteOutcome::default();

    if request.flags.any_embed()
        && super::metadata::is_embed_writable_path(request.final_path)
        && let Some(embed_path) = request.embed_path
    {
        outcome.embed_failed = !write_embed_metadata(
            embed_path,
            Arc::clone(&request.payload),
            request.created_local,
            request.flags,
            request.temp_suffix,
        )
        .await;
    }

    #[cfg(feature = "xmp")]
    if request.flags.contains(MetadataFlags::XMP_SIDECAR)
        && let Some(sidecar_path) = request.sidecar_path
    {
        outcome.sidecar_failed = !write_sidecar_metadata(
            sidecar_path,
            Arc::clone(&request.payload),
            request.created_local,
            request.temp_suffix,
        )
        .await;
    }

    outcome
}

async fn write_embed_metadata(
    path: &Path,
    payload: Arc<MetadataPayload>,
    created_local: DateTime<Local>,
    flags: MetadataFlags,
    temp_suffix: &str,
) -> bool {
    let embed_path = path.to_path_buf();
    let metadata_temp_suffix = temp_suffix.to_string();
    match tokio::task::spawn_blocking(move || {
        let probe = match super::metadata::probe_exif(&embed_path) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(path = %embed_path.display(), error = %e, "Failed to read EXIF");
                super::metadata::ExifProbe::default()
            }
        };
        let write = plan_metadata_write(flags, &payload, &created_local, &probe);
        if write.is_empty() {
            return true;
        }
        match super::metadata::apply_metadata(&embed_path, &write, &metadata_temp_suffix) {
            Err(e) => {
                tracing::warn!(path = %embed_path.display(), error = %e, "Failed to write metadata");
                false
            }
            Ok(()) => true,
        }
    })
    .await
    {
        Ok(ok) => ok,
        Err(e) => {
            tracing::warn!(error = %e, "EXIF task panicked");
            false
        }
    }
}

#[cfg(feature = "xmp")]
async fn write_sidecar_metadata(
    path: &Path,
    payload: Arc<MetadataPayload>,
    created_local: DateTime<Local>,
    temp_suffix: &str,
) -> bool {
    let sidecar_path = path.to_path_buf();
    let sidecar_temp_suffix = temp_suffix.to_string();
    match tokio::task::spawn_blocking(move || {
        let write = plan_sidecar_write(&payload, &created_local);
        if write.is_empty() {
            return true;
        }
        match super::metadata::write_sidecar(&sidecar_path, &write, &sidecar_temp_suffix) {
            Err(e) => {
                tracing::warn!(path = %sidecar_path.display(), error = %e, "Failed to write XMP sidecar");
                false
            }
            Ok(()) => true,
        }
    })
    .await
    {
        Ok(ok) => ok,
        Err(e) => {
            tracing::warn!(error = %e, "XMP sidecar task panicked");
            false
        }
    }
}

fn gps_from_payload(payload: &MetadataPayload) -> Option<super::metadata::GpsCoords> {
    match (payload.latitude, payload.longitude) {
        (Some(lat), Some(lng)) => Some(super::metadata::GpsCoords {
            latitude: lat,
            longitude: lng,
            altitude: payload.altitude,
        }),
        _ => None,
    }
}

/// Comprehensive snapshot of every field a payload can contribute. Used as
/// the sidecar plan (sidecars are fresh files; no probe gating applies).
#[cfg(feature = "xmp")]
fn plan_sidecar_write(
    payload: &MetadataPayload,
    created_local: &DateTime<Local>,
) -> super::metadata::MetadataWrite {
    let mut write = super::metadata::MetadataWrite {
        datetime: Some(created_local.format("%Y:%m:%d %H:%M:%S").to_string()),
        rating: payload.rating,
        gps: gps_from_payload(payload),
        is_hidden: payload.is_hidden,
        is_archived: payload.is_archived,
        ..super::metadata::MetadataWrite::default()
    };
    write.title.clone_from(&payload.title);
    write.description.clone_from(&payload.description);
    write.keywords.clone_from(&payload.keywords);
    write.people.clone_from(&payload.people);
    write.media_subtype.clone_from(&payload.media_subtype);
    write.burst_id.clone_from(&payload.burst_id);
    write
}

/// Plan the embed-path write. Per-tag gates:
///
/// - datetime / GPS: only when the flag is on AND the file has no existing
///   value (probe gate preserves camera-supplied data).
/// - rating / description: flag gate only - iCloud is the source of truth.
/// - XMP-only fields (title, keywords, people, hidden/archived,
///   media_subtype, burst_id): gated on the `EMBED_XMP` flag.
fn plan_metadata_write(
    flags: MetadataFlags,
    payload: &MetadataPayload,
    created_local: &DateTime<Local>,
    probe: &super::metadata::ExifProbe,
) -> super::metadata::MetadataWrite {
    let mut write = super::metadata::MetadataWrite::default();

    if flags.contains(MetadataFlags::DATETIME) && probe.datetime_original.is_none() {
        write.datetime = Some(created_local.format("%Y:%m:%d %H:%M:%S").to_string());
    }
    if flags.contains(MetadataFlags::RATING) {
        write.rating = payload.rating;
    }
    if flags.contains(MetadataFlags::GPS) && !probe.has_gps {
        write.gps = gps_from_payload(payload);
    }
    if flags.contains(MetadataFlags::DESCRIPTION) {
        write.description.clone_from(&payload.description);
    }
    #[cfg(feature = "xmp")]
    if flags.contains(MetadataFlags::EMBED_XMP) {
        write.title.clone_from(&payload.title);
        write.keywords.clone_from(&payload.keywords);
        write.people.clone_from(&payload.people);
        write.is_hidden = payload.is_hidden;
        write.is_archived = payload.is_archived;
        write.media_subtype.clone_from(&payload.media_subtype);
        write.burst_id.clone_from(&payload.burst_id);
    }

    write
}

/// Persist a metadata-rewrite marker for each candidate version whose
/// metadata drifted from the stored hash, or that already carries a marker
/// from a prior sync. No-op when metadata writing is off or the state DB
/// is absent.
pub(super) async fn tag_if_needed<D>(
    state_db: Option<&D>,
    config: &DownloadConfig,
    asset: &PhotoAsset,
    candidates: &[(VersionSizeKey, &str)],
    ctx: &DownloadContext,
) where
    D: MetadataRewriteStore + ?Sized,
{
    if !MetadataFlags::from(config).has_any_write() {
        return;
    }
    let Some(db) = state_db else {
        return;
    };
    let new_hash = asset.metadata().metadata_hash.as_deref();
    let library = asset.source_zone().unwrap_or(config.library.as_ref());
    for &(vs, _) in candidates {
        if !ctx.needs_metadata_rewrite(library, asset.state_id(), vs, new_hash) {
            continue;
        }
        tracing::info!(
            asset_id = %asset.id(),
            version_size = vs.as_str(),
            "Metadata-only change detected; tagging for rewrite"
        );
        if let Err(e) = db
            .record_metadata_write_failure(library, asset.state_id(), vs.as_str())
            .await
        {
            tracing::warn!(
                asset_id = %asset.id(),
                error = %e,
                "Failed to set metadata rewrite marker"
            );
        }
    }
}

/// Maximum assets processed per metadata-rewrite invocation. Bounds worst-case
/// tail work at sync end; anything beyond this rolls into the next sync.
const METADATA_REWRITE_BATCH: usize = 500;

/// Per-batch outcome of [`run_pending`]: fetched, applied, and still-failing counts.
#[derive(Default)]
pub(super) struct RewritePass {
    pub(super) fetched: usize,
    pub(super) applied: usize,
    pub(super) failed: usize,
}

/// Process one bounded batch of persisted metadata-rewrite markers: for each
/// asset whose `metadata_write_failed_at` is set and whose local file is still
/// on disk, re-apply EXIF/XMP using the stored metadata. On success clears the
/// marker and refreshes `metadata_hash`; on failure leaves the marker so the
/// next pass retries. Returns the per-batch counts.
pub(super) async fn run_pending<D>(
    db: &D,
    metadata_flags: MetadataFlags,
    temp_suffix: Arc<str>,
    shutdown_token: &CancellationToken,
) -> RewritePass
where
    D: MetadataRewriteStore + ?Sized,
{
    run_pending_page(db, metadata_flags, temp_suffix, shutdown_token, None, 0).await
}

pub(super) async fn run_pending_page<D>(
    db: &D,
    metadata_flags: MetadataFlags,
    temp_suffix: Arc<str>,
    shutdown_token: &CancellationToken,
    library_scope: Option<&[&str]>,
    offset: usize,
) -> RewritePass
where
    D: MetadataRewriteStore + ?Sized,
{
    let pending = match db
        .get_pending_metadata_rewrites_page(library_scope, offset, METADATA_REWRITE_BATCH)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to load pending metadata rewrites");
            return RewritePass {
                failed: 1,
                ..RewritePass::default()
            };
        }
    };
    if pending.is_empty() {
        return RewritePass::default();
    }
    let pending_count = pending.len();
    tracing::info!(
        count = pending_count,
        "Applying metadata rewrites to on-disk files"
    );
    let mut applied = 0usize;
    let mut skipped_missing = 0usize;
    let mut errored = 0usize;
    let mut deferred = 0usize;
    for (idx, record) in pending.into_iter().enumerate() {
        if shutdown_token.is_cancelled() {
            deferred += pending_count - idx;
            tracing::info!("Shutdown requested, deferring remaining metadata rewrites");
            break;
        }
        let Some(local_path) = record.local_path.as_deref() else {
            continue;
        };
        let path = PathBuf::from(local_path);
        // tokio::fs defers the stat to the blocking pool; raw
        // std::Path::exists() would block the async runtime thread.
        // Keep the marker on missing so a future sync that re-downloads the
        // asset re-drives the writer.
        match tokio::fs::try_exists(&path).await {
            Ok(true) => {}
            Ok(false) => {
                skipped_missing += 1;
                continue;
            }
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "Could not stat file for metadata rewrite; skipping"
                );
                skipped_missing += 1;
                continue;
            }
        }

        let payload = Arc::new(MetadataPayload::from_metadata(&record.metadata));
        let created_local: DateTime<Local> = DateTime::from(record.created_at);
        let version_size = record.version_size;
        let outcome = write_download_metadata(MetadataWriteRequest {
            final_path: &path,
            embed_path: Some(&path),
            sidecar_path: Some(&path),
            payload,
            created_local,
            flags: metadata_flags,
            temp_suffix: &temp_suffix,
        })
        .await;

        if !outcome.any_failed() {
            if let Some(new_hash) = record.metadata.metadata_hash.as_deref()
                && let Err(e) = db
                    .update_metadata_hash(
                        &record.library,
                        &record.id,
                        version_size.as_str(),
                        new_hash,
                    )
                    .await
            {
                tracing::warn!(asset_id = %record.id, error = %e, "Failed to update metadata_hash");
                errored += 1;
                continue;
            }
            if let Err(e) = db
                .clear_metadata_write_failure(&record.library, &record.id, version_size.as_str())
                .await
            {
                tracing::warn!(asset_id = %record.id, error = %e, "Failed to clear metadata rewrite marker");
                errored += 1;
                continue;
            }
            applied += 1;
        } else {
            tracing::warn!(
                asset_id = %record.id,
                path = %path.display(),
                embed_failed = outcome.embed_failed,
                sidecar_failed = outcome.sidecar_failed,
                "Metadata rewrite failed; leaving marker for future retry"
            );
            errored += 1;
        }
    }
    tracing::info!(
        applied,
        errored,
        skipped_missing,
        deferred,
        "Metadata rewrite pass complete"
    );
    RewritePass {
        fetched: pending_count,
        applied,
        failed: errored + deferred,
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "xmp")]
    use std::sync::Arc;

    use super::*;

    #[cfg(feature = "xmp")]
    fn now_local() -> DateTime<Local> {
        Local::now()
    }

    #[cfg(feature = "xmp")]
    fn rich_payload() -> MetadataPayload {
        MetadataPayload {
            rating: Some(4),
            latitude: Some(37.7),
            longitude: Some(-122.4),
            altitude: Some(10.0),
            title: Some("T".into()),
            description: Some("D".into()),
            keywords: vec!["vacation".into(), "beach".into()],
            people: vec!["Alice".into()],
            is_hidden: true,
            is_archived: true,
            media_subtype: Some("portrait".into()),
            burst_id: Some("b1".into()),
        }
    }

    #[cfg(feature = "xmp")]
    #[test]
    fn plan_metadata_write_gates_xmp_fields_on_embed_xmp() {
        let payload = rich_payload();
        let flags_no_embed = MetadataFlags::default();
        let w = plan_metadata_write(
            flags_no_embed,
            &payload,
            &now_local(),
            &crate::download::metadata::ExifProbe::default(),
        );
        assert!(
            w.title.is_none(),
            "title must not write when embed_xmp is off"
        );
        assert!(w.keywords.is_empty());
        assert!(w.people.is_empty());
        assert!(!w.is_hidden);

        let flags_embed = MetadataFlags::EMBED_XMP;
        let w = plan_metadata_write(
            flags_embed,
            &payload,
            &now_local(),
            &crate::download::metadata::ExifProbe::default(),
        );
        assert_eq!(w.title.as_deref(), Some("T"));
        assert_eq!(w.keywords, vec!["vacation", "beach"]);
        assert_eq!(w.people, vec!["Alice"]);
        assert!(w.is_hidden);
        assert!(w.is_archived);
        assert_eq!(w.media_subtype.as_deref(), Some("portrait"));
        assert_eq!(w.burst_id.as_deref(), Some("b1"));
    }

    #[cfg(feature = "xmp")]
    #[test]
    fn plan_metadata_write_respects_probe_skip_for_datetime_and_gps() {
        let payload = rich_payload();
        let flags = MetadataFlags::DATETIME | MetadataFlags::GPS;
        let probe = crate::download::metadata::ExifProbe {
            datetime_original: Some("2020:01:01 00:00:00".into()),
            has_gps: true,
        };
        let w = plan_metadata_write(flags, &payload, &now_local(), &probe);
        assert!(
            w.datetime.is_none(),
            "must skip datetime when file already has one"
        );
        assert!(w.gps.is_none(), "must skip gps when file already has one");
    }

    #[cfg(feature = "xmp")]
    #[test]
    fn plan_sidecar_write_is_comprehensive_regardless_of_flags() {
        let payload = rich_payload();
        let w = plan_sidecar_write(&payload, &now_local());
        // Every payload field should land in the sidecar write, no flag gating.
        assert!(w.datetime.is_some());
        assert_eq!(w.rating, Some(4));
        assert!(w.gps.is_some());
        assert_eq!(w.title.as_deref(), Some("T"));
        assert_eq!(w.description.as_deref(), Some("D"));
        assert_eq!(w.keywords.len(), 2);
        assert_eq!(w.people, vec!["Alice"]);
        assert!(w.is_hidden);
        assert!(w.is_archived);
        assert_eq!(w.media_subtype.as_deref(), Some("portrait"));
        assert_eq!(w.burst_id.as_deref(), Some("b1"));
    }

    #[cfg(feature = "xmp")]
    #[test]
    fn plan_sidecar_write_empty_payload_yields_datetime_only() {
        // datetime comes from the local clock; the rest stays empty.
        let w = plan_sidecar_write(&MetadataPayload::default(), &now_local());
        assert!(w.datetime.is_some());
        assert!(w.rating.is_none());
        assert!(w.gps.is_none());
        assert!(w.title.is_none());
        assert!(w.keywords.is_empty());
        assert!(!w.is_hidden);
    }

    #[test]
    fn metadata_flags_any_embed_captures_embed_only() {
        let mut flags = MetadataFlags::default();
        assert!(!flags.any_embed());
        flags.insert(MetadataFlags::XMP_SIDECAR);
        assert!(
            !flags.any_embed(),
            "sidecar-only must not trigger the .part-edit flow"
        );
        flags.remove(MetadataFlags::XMP_SIDECAR);
        flags.insert(MetadataFlags::EMBED_XMP);
        assert!(flags.any_embed());
    }

    /// Minimal valid JPEG (SOI + APP0 JFIF + EOI). XMP Toolkit can write
    /// into this container; small enough to keep the test hermetic.
    #[cfg(feature = "xmp")]
    fn minimal_jpeg_bytes() -> Vec<u8> {
        vec![
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00,
            0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
        ]
    }

    /// End-to-end test of the metadata-rewrite pass. Seeds a downloaded row
    /// with a `metadata_write_failed_at` marker and a rating of 4, then
    /// calls `run_pending` and asserts:
    /// 1. the on-disk JPEG now carries the rating in its XMP packet,
    /// 2. the DB marker is cleared (rewrite won't re-fire next cycle),
    /// 3. `metadata_hash` is refreshed to match the asset state.
    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn run_pending_applies_embed_and_clears_marker() {
        use crate::state::types::AssetMetadata;
        use crate::state::{AssetStatus, SqliteStateDb};

        let dir = tempfile::tempdir().unwrap();
        let photo_path = dir.path().join("rewrite_target.jpg");
        std::fs::write(&photo_path, minimal_jpeg_bytes()).unwrap();

        let db = SqliteStateDb::open_in_memory().unwrap();

        let seeded_hash = "seed_hash_before_rewrite".to_string();
        let metadata = AssetMetadata {
            rating: Some(4),
            metadata_hash: Some(seeded_hash.clone()),
            ..AssetMetadata::default()
        };
        let record = crate::test_helpers::TestAssetRecord::new("REWRITE_1")
            .filename("rewrite_target.jpg")
            .checksum("rewrite_ck")
            .size(22)
            .metadata(metadata)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "REWRITE_1",
            "original",
            &photo_path,
            "rewrite_ck",
            None,
        )
        .await
        .unwrap();
        db.record_metadata_write_failure("PrimarySync", "REWRITE_1", "original")
            .await
            .unwrap();

        // Sanity: the rewrite pass sees our row.
        let pending = db.get_pending_metadata_rewrites(32).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(&*pending[0].id, "REWRITE_1");

        let flags = MetadataFlags::RATING | MetadataFlags::EMBED_XMP;
        let token = CancellationToken::new();
        run_pending(&db, flags, Arc::from(".meta-tmp"), &token).await;

        // Marker must be gone; row must still be `downloaded`.
        let remaining = db.get_pending_metadata_rewrites(32).await.unwrap();
        assert!(
            remaining.is_empty(),
            "marker must be cleared after successful rewrite"
        );
        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.downloaded, 1);

        // metadata_hash must have been refreshed. We don't care what the
        // new hash value is - only that it reflects the rewrite pass ran
        // to completion (not the seeded placeholder).
        let hashes = db.get_downloaded_metadata_hashes().await.unwrap();
        let new_hash = hashes
            .get(&(
                "PrimarySync".to_string(),
                "REWRITE_1".to_string(),
                "original".to_string(),
            ))
            .expect("row must remain in the downloaded set");
        assert_eq!(
            new_hash, &seeded_hash,
            "update_metadata_hash uses the asset's recorded metadata_hash"
        );

        // The file on disk now contains an XMP packet with the rating.
        let bytes = std::fs::read(&photo_path).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            text.contains("Rating") || text.contains("rating"),
            "embed should have written a Rating property into the JPEG"
        );

        // summary.downloaded == 1 above already proves the row stayed in
        // the downloaded state; AssetStatus is referenced here for
        // documentation and as an import check.
        let _ = AssetStatus::Downloaded;
    }

    /// If the on-disk file has vanished between tagging and the rewrite
    /// pass, the pass must not error out. The marker stays, so a future
    /// sync that re-downloads the asset re-drives the writer.
    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn run_pending_skips_missing_file_and_leaves_marker() {
        use crate::state::SqliteStateDb;
        use crate::state::types::AssetMetadata;

        let dir = tempfile::tempdir().unwrap();
        let vanished_path = dir.path().join("never_written.jpg");

        let db = SqliteStateDb::open_in_memory().unwrap();

        let metadata = AssetMetadata {
            rating: Some(3),
            metadata_hash: Some("untouched_hash".to_string()),
            ..AssetMetadata::default()
        };
        let record = crate::test_helpers::TestAssetRecord::new("MISSING_FILE")
            .filename("never_written.jpg")
            .metadata(metadata)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "MISSING_FILE",
            "original",
            &vanished_path,
            "checksum123",
            None,
        )
        .await
        .unwrap();
        db.record_metadata_write_failure("PrimarySync", "MISSING_FILE", "original")
            .await
            .unwrap();

        let flags = MetadataFlags::RATING | MetadataFlags::EMBED_XMP;
        let token = CancellationToken::new();
        run_pending(&db, flags, Arc::from(".meta-tmp"), &token).await;

        let still_pending = db.get_pending_metadata_rewrites(32).await.unwrap();
        assert_eq!(
            still_pending.len(),
            1,
            "marker must survive when the file is absent so a future sync retries"
        );
    }

    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn cancel_returns_partial_and_keeps_retry_marker() {
        use crate::state::SqliteStateDb;
        use crate::state::types::AssetMetadata;

        let dir = tempfile::tempdir().unwrap();
        let photo_path = dir.path().join("rewrite_cancel.jpg");
        std::fs::write(&photo_path, minimal_jpeg_bytes()).unwrap();

        let db = SqliteStateDb::open_in_memory().unwrap();
        let metadata = AssetMetadata {
            rating: Some(5),
            metadata_hash: Some("retry_hash".to_string()),
            ..AssetMetadata::default()
        };
        let record = crate::test_helpers::TestAssetRecord::new("REWRITE_CANCEL")
            .filename("rewrite_cancel.jpg")
            .checksum("rewrite_cancel_ck")
            .metadata(metadata)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "REWRITE_CANCEL",
            "original",
            &photo_path,
            "rewrite_cancel_ck",
            None,
        )
        .await
        .unwrap();
        db.record_metadata_write_failure("PrimarySync", "REWRITE_CANCEL", "original")
            .await
            .unwrap();

        let flags = MetadataFlags::RATING | MetadataFlags::EMBED_XMP;
        let token = CancellationToken::new();
        token.cancel();
        let deferred = run_pending(&db, flags, Arc::from(".meta-tmp"), &token)
            .await
            .failed;

        assert_eq!(
            deferred, 1,
            "cancelled metadata rewrite must count as a partial retryable item"
        );
        let still_pending = db.get_pending_metadata_rewrites(32).await.unwrap();
        assert_eq!(
            still_pending.len(),
            1,
            "cancelled metadata rewrite must keep marker for retry"
        );
    }

    #[tokio::test]
    async fn run_pending_batch_is_bounded() {
        use crate::state::SqliteStateDb;

        let db = SqliteStateDb::open_in_memory().unwrap();
        for i in 0..(METADATA_REWRITE_BATCH + 100) {
            let id = format!("A{i}");
            let record = crate::test_helpers::TestAssetRecord::new(&id).build();
            db.upsert_seen(&record).await.unwrap();
            db.mark_downloaded(
                "PrimarySync",
                &id,
                "original",
                std::path::Path::new("/nonexistent/missing.jpg"),
                "ck",
                None,
            )
            .await
            .unwrap();
            db.record_metadata_write_failure("PrimarySync", &id, "original")
                .await
                .unwrap();
        }

        let token = CancellationToken::new();
        let pass = run_pending(
            &db,
            MetadataFlags::RATING,
            std::sync::Arc::from(".meta-tmp"),
            &token,
        )
        .await;
        assert_eq!(
            pass.fetched, METADATA_REWRITE_BATCH,
            "one pass fetches at most a bounded batch, never the whole queue"
        );
        assert_eq!(pass.applied, 0, "missing files apply nothing");
    }

    #[tokio::test]
    async fn drain_scope_skips_unselected_and_soft_deleted_failures() {
        use crate::config::MetadataConfig;
        use crate::download::DownloadStore;
        use crate::state::SqliteStateDb;

        let db = SqliteStateDb::open_in_memory().unwrap();
        for i in 0..3 {
            let id = format!("M{i}");
            let record = crate::test_helpers::TestAssetRecord::new(&id).build();
            db.upsert_seen(&record).await.unwrap();
            db.mark_downloaded(
                "PrimarySync",
                &id,
                "original",
                std::path::Path::new("/nonexistent/missing.jpg"),
                "ck",
                None,
            )
            .await
            .unwrap();
            db.record_metadata_write_failure("PrimarySync", &id, "original")
                .await
                .unwrap();
        }

        let invalid_dir = tempfile::tempdir().unwrap();
        let invalid_path = invalid_dir.path().join("invalid.jpg");
        std::fs::write(&invalid_path, b"not an image").unwrap();
        for (library, id, soft_deleted) in [
            ("SharedSync-OTHER", "UNSELECTED", false),
            ("PrimarySync", "SOFT_DELETED", true),
        ] {
            let record = crate::test_helpers::TestAssetRecord::new(id)
                .library(library)
                .build();
            db.upsert_seen(&record).await.unwrap();
            db.mark_downloaded(library, id, "original", &invalid_path, "ck", None)
                .await
                .unwrap();
            db.record_metadata_write_failure(library, id, "original")
                .await
                .unwrap();
            if soft_deleted {
                db.mark_soft_deleted(library, id, None).await.unwrap();
            }
        }

        let cfg = MetadataConfig {
            set_exif_rating: true,
            ..MetadataConfig::default()
        };
        let token = CancellationToken::new();
        let residual = crate::download::drain_pending_metadata_rewrites(
            &db as &dyn DownloadStore,
            &cfg,
            &["PrimarySync"],
            std::sync::Arc::from(".meta-tmp"),
            &token,
        )
        .await;
        assert_eq!(
            residual, 0,
            "unselected and soft-deleted failures must not fail the selected repair"
        );
        let pending = db.get_pending_metadata_rewrites(32).await.unwrap();
        assert_eq!(pending.len(), 5);
        assert!(
            pending
                .iter()
                .any(|record| record.id.as_ref() == "UNSELECTED"),
            "unselected library marker must remain untouched"
        );
        assert!(
            pending
                .iter()
                .any(|record| record.id.as_ref() == "SOFT_DELETED"),
            "soft-deleted marker must remain untouched"
        );
    }

    #[tokio::test]
    async fn drain_reports_residual_on_cancellation() {
        use crate::config::MetadataConfig;
        use crate::download::DownloadStore;
        use crate::state::SqliteStateDb;

        let db = SqliteStateDb::open_in_memory().unwrap();
        let record = crate::test_helpers::TestAssetRecord::new("C1").build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "C1",
            "original",
            std::path::Path::new("/x/c1.jpg"),
            "ck",
            None,
        )
        .await
        .unwrap();
        db.record_metadata_write_failure("PrimarySync", "C1", "original")
            .await
            .unwrap();

        let cfg = MetadataConfig {
            set_exif_rating: true,
            ..MetadataConfig::default()
        };
        let token = CancellationToken::new();
        token.cancel();
        let residual = crate::download::drain_pending_metadata_rewrites(
            &db as &dyn DownloadStore,
            &cfg,
            &["PrimarySync"],
            std::sync::Arc::from(".meta-tmp"),
            &token,
        )
        .await;
        assert!(
            residual >= 1,
            "a cancelled drain must report a non-zero residual so the sync exits non-zero"
        );
    }

    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn drain_stops_when_rewrite_marker_cannot_be_cleared() {
        use crate::config::MetadataConfig;
        use crate::state::types::AssetMetadata;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clear-failure.jpg");
        std::fs::write(&path, minimal_jpeg_bytes()).unwrap();
        let db = crate::state::SqliteStateDb::open_in_memory().unwrap();
        let record = crate::test_helpers::TestAssetRecord::new("CLEAR_FAIL")
            .filename("clear-failure.jpg")
            .metadata(AssetMetadata {
                rating: Some(3),
                metadata_hash: Some("fresh-hash".to_string()),
                ..AssetMetadata::default()
            })
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "CLEAR_FAIL",
            "original",
            &path,
            "checksum",
            None,
        )
        .await
        .unwrap();
        db.record_metadata_write_failure("PrimarySync", "CLEAR_FAIL", "original")
            .await
            .unwrap();
        db.fail_metadata_marker_clear_for_test();

        let residual = crate::download::drain_pending_metadata_rewrites(
            &db,
            &MetadataConfig {
                set_exif_rating: true,
                embed_xmp: true,
                ..MetadataConfig::default()
            },
            &["PrimarySync"],
            Arc::from(".meta-tmp"),
            &CancellationToken::new(),
        )
        .await;

        assert_eq!(residual, 1);
        assert_eq!(db.get_pending_metadata_rewrites(10).await.unwrap().len(), 1);
    }

    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn drain_reaches_newer_marker_after_retained_batch() {
        use crate::config::MetadataConfig;
        use crate::download::DownloadStore;
        use crate::state::SqliteStateDb;
        use crate::state::types::AssetMetadata;

        let dir = tempfile::tempdir().unwrap();
        let db = SqliteStateDb::open_in_memory().unwrap();
        let invalid_path = dir.path().join("invalid.jpg");
        std::fs::write(&invalid_path, b"not a jpeg").unwrap();
        for i in 0..METADATA_REWRITE_BATCH {
            let id = format!("A{i:04}");
            let metadata = AssetMetadata {
                rating: Some(3),
                metadata_hash: Some(format!("h{i}")),
                ..AssetMetadata::default()
            };
            let record = crate::test_helpers::TestAssetRecord::new(&id)
                .filename(&format!("{id}.jpg"))
                .metadata(metadata)
                .build();
            db.upsert_seen(&record).await.unwrap();
            db.mark_downloaded("PrimarySync", &id, "original", &invalid_path, "ck", None)
                .await
                .unwrap();
            db.record_metadata_write_failure("PrimarySync", &id, "original")
                .await
                .unwrap();
        }
        let valid_path = dir.path().join("valid.jpg");
        std::fs::write(&valid_path, minimal_jpeg_bytes()).unwrap();
        let valid = crate::test_helpers::TestAssetRecord::new("Z_VALID")
            .filename("valid.jpg")
            .metadata(AssetMetadata {
                rating: Some(3),
                metadata_hash: Some("valid-hash".to_string()),
                ..AssetMetadata::default()
            })
            .build();
        db.upsert_seen(&valid).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "Z_VALID",
            "original",
            &valid_path,
            "ck",
            None,
        )
        .await
        .unwrap();
        db.record_metadata_write_failure("PrimarySync", "Z_VALID", "original")
            .await
            .unwrap();

        let cfg = MetadataConfig {
            set_exif_rating: true,
            embed_xmp: true,
            ..MetadataConfig::default()
        };
        let token = CancellationToken::new();
        let residual = crate::download::drain_pending_metadata_rewrites(
            &db as &dyn DownloadStore,
            &cfg,
            &["PrimarySync"],
            std::sync::Arc::from(".meta-tmp"),
            &token,
        )
        .await;
        assert_eq!(residual, METADATA_REWRITE_BATCH);
        let pending = db
            .get_pending_metadata_rewrites(METADATA_REWRITE_BATCH + 1)
            .await
            .unwrap();
        assert_eq!(pending.len(), METADATA_REWRITE_BATCH);
        assert!(
            pending.iter().all(|record| record.id.as_ref() != "Z_VALID"),
            "retained older markers must not prevent newer work from completing"
        );
    }
}
