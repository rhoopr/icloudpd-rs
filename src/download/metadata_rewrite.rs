//! Metadata write orchestration for downloaded files and retry markers.
//!
//! The download pipeline owns byte transfer and `.part` promotion. This module
//! owns the opt-in local-file metadata mutation work that can happen around
//! that transfer: embed writes before publish, sidecar writes after publish,
//! metadata-only retry marker tagging, and pending retry marker draining.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, FixedOffset};
use tokio_util::sync::CancellationToken;

use crate::download::filter::MetadataPayload;
use crate::icloud::photos::PhotoAsset;
use crate::state::{MembershipStore, MetadataRewriteStore, VersionSizeKey};

use super::{AssetGroupings, DownloadConfig, DownloadContext};

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

    fn uses_xmp_groupings(self) -> bool {
        self.intersects(Self::EMBED_XMP | Self::XMP_SIDECAR)
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

#[must_use]
pub(crate) fn writers_enabled(metadata: &crate::config::MetadataConfig) -> bool {
    MetadataFlags::from(metadata).has_any_write()
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
    pub(super) created_local: DateTime<FixedOffset>,
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
    // CONTRACT: METADATA_WRITES_REQUIRE_OPT_IN
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
    created_local: DateTime<FixedOffset>,
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
    created_local: DateTime<FixedOffset>,
    temp_suffix: &str,
) -> bool {
    let sidecar_path = path.to_path_buf();
    let sidecar_temp_suffix = temp_suffix.to_string();
    let log_path = sidecar_path.clone();
    match tokio::task::spawn_blocking(move || -> anyhow::Result<bool> {
        let write = plan_sidecar_write(&sidecar_path, &payload, &created_local)?;
        if write.is_empty() {
            return Ok(true);
        }
        super::metadata::write_sidecar(&sidecar_path, &write, &sidecar_temp_suffix)?;
        Ok(true)
    })
    .await
    {
        Ok(Ok(ok)) => ok,
        Ok(Err(e)) => {
            tracing::warn!(path = %log_path.display(), error = %e, "Failed to write XMP sidecar");
            false
        }
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

fn offset_time_original(payload: &MetadataPayload) -> Option<String> {
    let offset = payload.timezone_offset.and_then(FixedOffset::east_opt)?;
    let seconds = offset.local_minus_utc();
    if seconds % 60 != 0 {
        return None;
    }
    let minutes = i64::from(seconds).abs() / 60;
    let sign = if seconds < 0 { '-' } else { '+' };
    Some(format!("{sign}{:02}:{:02}", minutes / 60, minutes % 60))
}

/// Comprehensive snapshot of every field a payload can contribute. Used as
/// the sidecar plan (sidecars are fresh files; no probe gating applies).
/// Source-media GPS facts are read here on every attempt so metadata-only
/// retries do not depend on a reduced durable payload.
#[cfg(feature = "xmp")]
fn plan_sidecar_write(
    path: &Path,
    payload: &MetadataPayload,
    created_local: &DateTime<FixedOffset>,
) -> anyhow::Result<super::metadata::MetadataWrite> {
    let source_gps = super::metadata::read_source_gps(path)?;
    let mut write = super::metadata::MetadataWrite {
        datetime: Some(created_local.format("%Y:%m:%d %H:%M:%S").to_string()),
        offset_time_original: offset_time_original(payload),
        gps_datetime: source_gps.datetime,
        gps_speed: source_gps.speed,
        gps_speed_ref: source_gps.speed_ref,
        gps_h_positioning_error: source_gps.horizontal_positioning_error,
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
    Ok(write)
}

/// Plan the embed-path write. Per-tag gates:
///
/// - datetime / GPS: only when the flag is on AND the file has no existing
///   value (probe gate preserves camera-supplied data).
/// - offset: only alongside a timestamp this pass writes, or one the probe
///   proves already renders the capture-local instant.
/// - rating / description: flag gate only - iCloud is the source of truth.
/// - XMP-only fields (title, keywords, people, hidden/archived,
///   media_subtype, burst_id): gated on the `EMBED_XMP` flag.
fn plan_metadata_write(
    flags: MetadataFlags,
    payload: &MetadataPayload,
    created_local: &DateTime<FixedOffset>,
    probe: &super::metadata::ExifProbe,
) -> super::metadata::MetadataWrite {
    let mut write = super::metadata::MetadataWrite::default();

    if flags.contains(MetadataFlags::DATETIME) {
        if probe.datetime_original.is_none() {
            write.datetime = Some(created_local.format("%Y:%m:%d %H:%M:%S").to_string());
            write.clear_datetime_offsets = probe.has_any_datetime_offset();
        }
        // An offset describes one specific timestamp. Attach it only to a
        // timestamp this pass writes, or to one already proven to render the
        // capture-local instant.
        if write.datetime.is_some()
            || (probe.offset_time_original.is_none() && probe.denotes_capture_time(created_local))
        {
            write.offset_time_original = offset_time_original(payload);
        }
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
/// marker; on failure leaves it so the next pass retries. Returns the
/// per-batch counts.
pub(super) async fn run_pending<D>(
    db: &D,
    metadata_flags: MetadataFlags,
    temp_suffix: Arc<str>,
    shutdown_token: &CancellationToken,
) -> RewritePass
where
    D: MembershipStore + MetadataRewriteStore + ?Sized,
{
    run_pending_page(db, metadata_flags, temp_suffix, shutdown_token, None, 0).await
}

async fn load_pending_groupings<D>(
    db: &D,
    pending: &[crate::state::types::AssetRecord],
) -> (HashMap<String, AssetGroupings>, HashSet<String>)
where
    D: MembershipStore + ?Sized,
{
    let mut ids_by_library: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for record in pending {
        ids_by_library
            .entry(record.library.as_ref())
            .or_default()
            .push(record.id.as_ref());
    }

    let mut groupings_by_library = HashMap::with_capacity(ids_by_library.len());
    let mut failed_libraries = HashSet::new();
    for (library, asset_ids) in ids_by_library {
        match db.get_asset_groupings(library, &asset_ids).await {
            Ok(rows) => {
                let mut groupings = AssetGroupings::default();
                for (asset_id, album) in rows.albums {
                    groupings.albums.entry(asset_id).or_default().push(album);
                }
                for (asset_id, person) in rows.people {
                    groupings.people.entry(asset_id).or_default().push(person);
                }
                groupings_by_library.insert(library.to_owned(), groupings);
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    library,
                    "Failed to load asset groupings for metadata rewrites; leaving markers for retry"
                );
                failed_libraries.insert(library.to_owned());
            }
        }
    }
    (groupings_by_library, failed_libraries)
}

/// Drops a recorded hash that a rewrite may already have invalidated. Keeping a
/// known-stale hash would make the next pass read kei's own rewrite as damage
/// and refuse it forever, so recording "unknown" is both truthful and the only
/// state a later pass can recover from.
async fn forget_stale_checksum<D>(
    db: &D,
    record: &crate::state::types::AssetRecord,
    version_size: &str,
) where
    D: MetadataRewriteStore + ?Sized,
{
    if record.local_checksum.is_none() {
        return;
    }
    if let Err(e) = db
        .set_metadata_rewrite_checksums(&record.library, &record.id, version_size, None, None)
        .await
    {
        tracing::warn!(
            asset_id = %record.id,
            error = %e,
            "Could not clear the stale media checksum; `kei reconcile` can still repair the file"
        );
    }
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
    D: MembershipStore + MetadataRewriteStore + ?Sized,
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
    let (groupings_by_library, grouping_read_failures) = if metadata_flags.uses_xmp_groupings() {
        load_pending_groupings(db, &pending).await
    } else {
        (HashMap::new(), HashSet::new())
    };
    let pending_count = pending.len();
    tracing::info!(
        count = pending_count,
        "Applying metadata rewrites to on-disk files"
    );
    let mut applied = 0usize;
    let mut skipped_missing = 0usize;
    let mut skipped_drifted = 0usize;
    let mut errored = 0usize;
    let mut deferred = 0usize;
    for (idx, record) in pending.into_iter().enumerate() {
        if shutdown_token.is_cancelled() {
            deferred += pending_count - idx;
            tracing::info!("Shutdown requested, deferring remaining metadata rewrites");
            break;
        }
        if grouping_read_failures.contains(record.library.as_ref()) {
            errored += 1;
            continue;
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

        let payload = Arc::new(
            groupings_by_library
                .get(record.library.as_ref())
                .map_or_else(
                    || MetadataPayload::from_metadata(&record.metadata),
                    |groupings| groupings.metadata_payload(&record.id, &record.metadata),
                ),
        );
        let created_local = record.metadata.capture_local(record.created_at);
        let version_size = record.version_size;

        // Only an embedded write touches media bytes, so the drain needs the
        // pre-write hash to tell its own rewrite apart from damage that
        // arrived some other way.
        let pre_rewrite_checksum = if metadata_flags.any_embed() {
            match super::file::compute_sha256(&path).await {
                Ok(checksum) => Some(checksum),
                Err(e) => {
                    tracing::warn!(
                        asset_id = %record.id,
                        path = %path.display(),
                        error = %e,
                        "Could not hash file before metadata rewrite; leaving marker for future retry"
                    );
                    errored += 1;
                    continue;
                }
            }
        } else {
            None
        };

        // A file that no longer matches the recorded hash is not kei's to
        // rewrite: embedding would overwrite the evidence that `verify` and
        // `reconcile` rely on, and re-hashing would bless the damage. The
        // sidecar is a separate file, so it still runs.
        let drifted = matches!(
            (&pre_rewrite_checksum, &record.local_checksum),
            (Some(actual), Some(recorded)) if actual != recorded
        );
        if drifted {
            tracing::warn!(
                asset_id = %record.id,
                path = %path.display(),
                "On-disk file does not match its recorded checksum; leaving the media and the \
                 marker alone. Run `kei verify --checksums` or `kei reconcile`"
            );
            skipped_drifted += 1;
        }

        let outcome = write_download_metadata(MetadataWriteRequest {
            final_path: &path,
            embed_path: if drifted { None } else { Some(&path) },
            sidecar_path: Some(&path),
            payload,
            created_local,
            flags: metadata_flags,
            temp_suffix: &temp_suffix,
        })
        .await;

        if drifted {
            // The media still owes its metadata, so the marker cannot retire
            // no matter how the sidecar fared.
            continue;
        }

        // Hash whenever an embed could have run. A reported failure can still
        // leave replaced bytes, because the durable install renames before it
        // syncs the parent directory.
        //
        // `download_checksum` asserts the provider's pre-metadata bytes. Only a
        // row that already agreed with its file can make that claim, so a row
        // without a recorded checksum establishes `local_checksum` alone and
        // stays visible to reconcile's size check.
        let verified_before = record.local_checksum.is_some();
        if let Some(before) = &pre_rewrite_checksum {
            match super::file::compute_sha256(&path).await {
                // Stored before the marker retires below, so a later failure
                // cannot leave a rewritten file behind a stale hash. An
                // unchanged file is left alone: kei only vouches for bytes it
                // wrote.
                Ok(after) if after != *before => {
                    if let Err(e) = db
                        .set_metadata_rewrite_checksums(
                            &record.library,
                            &record.id,
                            version_size.as_str(),
                            Some(after.as_str()),
                            verified_before.then_some(before.as_str()),
                        )
                        .await
                    {
                        tracing::warn!(asset_id = %record.id, error = %e, "Failed to record rewritten media checksum");
                        forget_stale_checksum(db, &record, version_size.as_str()).await;
                        errored += 1;
                        continue;
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(
                        asset_id = %record.id,
                        path = %path.display(),
                        error = %e,
                        "Could not hash file after metadata rewrite; leaving marker for future retry"
                    );
                    // The rewrite may already have replaced the bytes, so the
                    // recorded hash can no longer be trusted either way.
                    forget_stale_checksum(db, &record, version_size.as_str()).await;
                    errored += 1;
                    continue;
                }
            }
        }

        if !outcome.any_failed() {
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
        skipped_drifted,
        deferred,
        "Metadata rewrite pass complete"
    );
    RewritePass {
        fetched: pending_count,
        applied,
        failed: errored + deferred + skipped_drifted,
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "xmp")]
    use std::sync::Arc;

    #[cfg(feature = "xmp")]
    use xmp_toolkit::{XmpMeta, xmp_ns};

    use super::*;
    use chrono::TimeZone;

    /// Capture-local time for an asset whose stored offset is +11:00, which is
    /// what every payload below carries. Production derives this offset and
    /// the written `OffsetTimeOriginal` from that same stored value, so the
    /// two always agree.
    fn now_local() -> DateTime<FixedOffset> {
        FixedOffset::east_opt(39_600)
            .unwrap()
            .with_ymd_and_hms(2024, 6, 15, 10, 0, 0)
            .unwrap()
    }

    #[cfg(feature = "xmp")]
    fn rich_payload() -> MetadataPayload {
        MetadataPayload {
            timezone_offset: Some(39_600),
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
        assert!(w.offset_time_original.is_none());

        let flags_embed = MetadataFlags::DATETIME | MetadataFlags::EMBED_XMP;
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
        assert_eq!(w.offset_time_original.as_deref(), Some("+11:00"));
    }

    #[test]
    fn plan_metadata_write_respects_probe_skip_for_datetime_and_gps() {
        let payload = MetadataPayload {
            timezone_offset: Some(39_600),
            latitude: Some(37.7),
            longitude: Some(-122.4),
            ..MetadataPayload::default()
        };
        let flags = MetadataFlags::DATETIME | MetadataFlags::GPS;
        let created_local = now_local();
        let capture_local = created_local.format("%Y:%m:%d %H:%M:%S").to_string();

        let matching_clock = crate::download::metadata::ExifProbe {
            datetime_original: Some(capture_local),
            offset_time_original: None,
            has_other_datetime_offset: false,
            has_gps: true,
        };
        let write = plan_metadata_write(flags, &payload, &created_local, &matching_clock);
        assert!(
            write.datetime.is_none(),
            "must skip datetime when file already has one"
        );
        assert_eq!(
            write.offset_time_original.as_deref(),
            Some("+11:00"),
            "an offset may join a timestamp already rendering capture-local time"
        );
        assert!(
            write.gps.is_none(),
            "must skip gps when file already has one"
        );

        let existing_offset = crate::download::metadata::ExifProbe {
            offset_time_original: Some("+10:00".into()),
            ..matching_clock
        };
        let write = plan_metadata_write(flags, &payload, &created_local, &existing_offset);
        assert!(write.datetime.is_none());
        assert!(write.offset_time_original.is_none());
        assert!(write.gps.is_none());
    }

    #[test]
    fn plan_metadata_write_replaces_offsets_orphaned_from_datetime_original() {
        let payload = MetadataPayload {
            timezone_offset: Some(39_600),
            ..MetadataPayload::default()
        };
        let flags = MetadataFlags::DATETIME;
        let created_local = now_local();

        for probe in [
            crate::download::metadata::ExifProbe {
                offset_time_original: Some("+10:00".into()),
                ..crate::download::metadata::ExifProbe::default()
            },
            crate::download::metadata::ExifProbe {
                has_other_datetime_offset: true,
                ..crate::download::metadata::ExifProbe::default()
            },
        ] {
            let write = plan_metadata_write(flags, &payload, &created_local, &probe);
            assert!(write.datetime.is_some());
            assert!(write.clear_datetime_offsets);
            assert_eq!(write.offset_time_original.as_deref(), Some("+11:00"));
        }

        let write = plan_metadata_write(
            flags,
            &MetadataPayload::default(),
            &created_local,
            &crate::download::metadata::ExifProbe {
                offset_time_original: Some("+10:00".into()),
                ..crate::download::metadata::ExifProbe::default()
            },
        );
        assert!(write.datetime.is_some());
        assert!(write.clear_datetime_offsets);
        assert!(write.offset_time_original.is_none());
    }

    /// A file written before capture-local resolution holds a wall clock in the
    /// backup host's timezone. Apple's offset does not describe that clock, so
    /// pairing the two would publish an instant the asset never had.
    #[test]
    fn plan_metadata_write_withholds_offset_from_an_unverified_timestamp() {
        let payload = MetadataPayload {
            timezone_offset: Some(39_600),
            ..MetadataPayload::default()
        };
        let flags = MetadataFlags::DATETIME;
        let created_local = now_local();
        let host_local = (created_local - chrono::Duration::hours(11))
            .format("%Y:%m:%d %H:%M:%S")
            .to_string();

        for existing in [
            host_local,
            "not a timestamp".to_string(),
            String::new(),
            "2024-06-15".to_string(),
            // Capture-local wall clock, but claiming a different zone.
            created_local.format("%Y-%m-%dT%H:%M:%S+05:00").to_string(),
        ] {
            let probe = crate::download::metadata::ExifProbe {
                datetime_original: Some(existing.clone()),
                offset_time_original: None,
                has_other_datetime_offset: false,
                has_gps: false,
            };
            let write = plan_metadata_write(flags, &payload, &created_local, &probe);
            assert!(write.datetime.is_none());
            assert!(
                write.offset_time_original.is_none(),
                "offset must not join the unverified timestamp {existing:?}"
            );
        }
    }

    /// XMP stores capture times as ISO 8601, and kei's own writer appends the
    /// offset. Such a timestamp is already capture-local, so it still accepts
    /// the offset tag.
    #[test]
    fn plan_metadata_write_accepts_iso_timestamps_from_the_xmp_probe() {
        let payload = MetadataPayload {
            timezone_offset: Some(39_600),
            ..MetadataPayload::default()
        };
        let created_local = now_local();

        for existing in [
            created_local.format("%Y-%m-%dT%H:%M:%S").to_string(),
            created_local.format("%Y-%m-%dT%H:%M:%S+11:00").to_string(),
            format!("  {}\0", created_local.format("%Y:%m:%d %H:%M:%S")),
        ] {
            let probe = crate::download::metadata::ExifProbe {
                datetime_original: Some(existing.clone()),
                offset_time_original: None,
                has_other_datetime_offset: false,
                has_gps: false,
            };
            let write =
                plan_metadata_write(MetadataFlags::DATETIME, &payload, &created_local, &probe);
            assert_eq!(
                write.offset_time_original.as_deref(),
                Some("+11:00"),
                "capture-local timestamp {existing:?} must accept its offset"
            );
        }
    }

    #[cfg(feature = "xmp")]
    #[test]
    fn plan_sidecar_write_is_comprehensive_regardless_of_flags() {
        let payload = rich_payload();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("photo.jpg");
        std::fs::write(&path, minimal_jpeg_bytes()).unwrap();
        let w = plan_sidecar_write(&path, &payload, &now_local()).unwrap();
        // Every payload field should land in the sidecar write, no flag gating.
        assert!(w.datetime.is_some());
        assert_eq!(w.offset_time_original.as_deref(), Some("+11:00"));
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
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("photo.jpg");
        std::fs::write(&path, minimal_jpeg_bytes()).unwrap();
        let w = plan_sidecar_write(&path, &MetadataPayload::default(), &now_local()).unwrap();
        assert!(w.datetime.is_some());
        assert!(w.gps_datetime.is_none());
        assert!(w.gps_speed.is_none());
        assert!(w.gps_speed_ref.is_none());
        assert!(w.gps_h_positioning_error.is_none());
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

    #[tokio::test]
    async fn contract_metadata_writes_require_opt_in_leaves_files_untouched() {
        let dir = tempfile::tempdir().expect("metadata temp dir");
        let photo_path = dir.path().join("photo.jpg");
        std::fs::write(&photo_path, b"original-media").expect("seed media");

        write_download_metadata(MetadataWriteRequest {
            final_path: &photo_path,
            embed_path: Some(&photo_path),
            sidecar_path: Some(&photo_path),
            payload: Arc::new(MetadataPayload::default()),
            created_local: now_local(),
            flags: MetadataFlags::default(),
            temp_suffix: ".metadata-test",
        })
        .await;

        assert_eq!(
            std::fs::read(&photo_path).expect("read media"),
            b"original-media",
            "disabled metadata options must not change media bytes"
        );
        let files = std::fs::read_dir(dir.path())
            .expect("read metadata temp dir")
            .count();
        assert_eq!(files, 1, "disabled metadata options created another file");
    }

    /// Minimal valid JPEG (SOI + APP0 JFIF + EOI). XMP Toolkit can write
    /// into this container; small enough to keep the test hermetic.
    fn minimal_jpeg_bytes() -> Vec<u8> {
        vec![
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00,
            0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
        ]
    }

    #[tokio::test]
    async fn embed_path_preserves_unverified_host_local_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let photo_path = dir.path().join("host-local.jpg");
        std::fs::write(&photo_path, minimal_jpeg_bytes()).unwrap();
        crate::download::metadata::apply_metadata(
            &photo_path,
            &crate::download::metadata::MetadataWrite {
                datetime: Some("2024:06:14 23:00:00".into()),
                ..crate::download::metadata::MetadataWrite::default()
            },
            ".seed-tmp",
        )
        .unwrap();
        let before = crate::download::metadata::probe_exif(&photo_path).unwrap();
        assert!(before.datetime_original.is_some());
        assert!(before.offset_time_original.is_none());

        assert!(
            write_embed_metadata(
                &photo_path,
                Arc::new(MetadataPayload {
                    timezone_offset: Some(39_600),
                    ..MetadataPayload::default()
                }),
                now_local(),
                MetadataFlags::DATETIME,
                ".metadata-test",
            )
            .await
        );

        let after = crate::download::metadata::probe_exif(&photo_path).unwrap();
        assert_eq!(after.datetime_original, before.datetime_original);
        assert!(after.offset_time_original.is_none());
    }

    #[tokio::test]
    async fn embed_path_replaces_orphaned_offset_before_writing_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let photo_path = dir.path().join("orphaned-offset.jpg");
        std::fs::write(&photo_path, minimal_jpeg_bytes()).unwrap();
        crate::download::metadata::apply_metadata(
            &photo_path,
            &crate::download::metadata::MetadataWrite {
                offset_time_original: Some("+10:00".into()),
                ..crate::download::metadata::MetadataWrite::default()
            },
            ".seed-tmp",
        )
        .unwrap();
        let before = crate::download::metadata::probe_exif(&photo_path).unwrap();
        assert!(before.datetime_original.is_none());
        assert_eq!(before.offset_time_original.as_deref(), Some("+10:00"));

        let created_local = now_local();
        assert!(
            write_embed_metadata(
                &photo_path,
                Arc::new(MetadataPayload {
                    timezone_offset: Some(39_600),
                    ..MetadataPayload::default()
                }),
                created_local,
                MetadataFlags::DATETIME,
                ".metadata-test",
            )
            .await
        );

        let after = crate::download::metadata::probe_exif(&photo_path).unwrap();
        assert!(after.denotes_capture_time(&created_local));
        assert_eq!(after.offset_time_original.as_deref(), Some("+11:00"));
    }

    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn sidecar_write_carries_source_gps_and_corrected_coordinates() {
        let dir = tempfile::tempdir().expect("metadata temp dir");
        let media_path = dir.path().join("source.jpg");
        let source_bytes = crate::test_helpers::minimal_jpeg_with_source_gps();
        std::fs::write(&media_path, &source_bytes).expect("write source media");
        let payload = MetadataPayload {
            latitude: Some(12.3456),
            longitude: Some(-78.9012),
            altitude: Some(9.25),
            ..MetadataPayload::default()
        };
        let request = || MetadataWriteRequest {
            final_path: &media_path,
            embed_path: None,
            sidecar_path: Some(&media_path),
            payload: Arc::new(payload.clone()),
            created_local: now_local(),
            flags: MetadataFlags::XMP_SIDECAR,
            temp_suffix: ".gps-sidecar-test",
        };

        let outcome = write_download_metadata(request()).await;
        assert!(!outcome.any_failed());
        let sidecar_path = media_path.with_file_name("source.jpg.xmp");
        let first = std::fs::read(&sidecar_path).expect("read generated sidecar");
        let xmp = String::from_utf8(first.clone())
            .expect("sidecar UTF-8")
            .parse::<XmpMeta>()
            .expect("parse generated sidecar");
        crate::test_helpers::assert_source_gps_in_xmp(&xmp);
        let coord = |name: &str| xmp.property(xmp_ns::EXIF, name).expect(name).value;
        assert_eq!(coord("GPSLatitude"), "12,20.7360N");
        assert_eq!(coord("GPSLongitude"), "78,54.0720W");
        assert_eq!(coord("GPSAltitude"), "9250/1000");
        assert_eq!(
            std::fs::read(&media_path).expect("read media after sidecar"),
            source_bytes
        );

        let second_outcome = write_download_metadata(request()).await;
        assert!(!second_outcome.any_failed());
        assert_eq!(
            std::fs::read(&sidecar_path).expect("read repeated sidecar"),
            first,
            "repeating the same sidecar write must be idempotent"
        );

        use crate::state::{AssetMetadata, SqliteStateDb};
        let db = SqliteStateDb::open_in_memory().expect("metadata state DB");
        let checksum = crate::download::file::compute_sha256(&media_path)
            .await
            .expect("media checksum");
        seed_downloaded_marker(
            &db,
            "GPS_RETRY",
            "source.jpg",
            &media_path,
            &checksum,
            AssetMetadata {
                latitude: Some(12.3456),
                longitude: Some(-78.9012),
                altitude: Some(9.25),
                metadata_hash: Some("gps-retry-hash".into()),
                ..AssetMetadata::default()
            },
        )
        .await;
        run_pending(
            &db,
            MetadataFlags::XMP_SIDECAR,
            Arc::from(".gps-retry-test"),
            &CancellationToken::new(),
        )
        .await;
        assert!(
            db.get_pending_metadata_rewrites(10)
                .await
                .expect("read retry markers")
                .is_empty(),
            "successful sidecar retry must retire its marker"
        );
        let retry_xmp = std::fs::read_to_string(&sidecar_path)
            .expect("read sidecar after retry")
            .parse::<XmpMeta>()
            .expect("parse sidecar after retry");
        crate::test_helpers::assert_source_gps_in_xmp(&retry_xmp);
    }

    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn sidecar_retry_recovers_exif_less_media_and_keeps_marker_for_unreadable_source() {
        use crate::state::{AssetMetadata, SqliteStateDb};

        let dir = tempfile::tempdir().expect("metadata temp dir");

        // A structurally valid HEIC with no EXIF block. There is no source GPS
        // to read, so the sidecar carries only the CloudKit payload and the
        // marker must retire.
        let exif_less_path = dir.path().join("exif-less.heic");
        std::fs::write(
            &exif_less_path,
            crate::test_helpers::heif_ftyp_without_meta_bytes(),
        )
        .expect("write EXIF-less HEIC");

        // A source the reader cannot open at all. Planning the sidecar fails,
        // so the durable marker survives for a future retry.
        let unreadable_path = dir.path().join("unreadable.heic");
        std::fs::create_dir(&unreadable_path).expect("create unreadable source");

        let db = SqliteStateDb::open_in_memory().expect("metadata state DB");
        let exif_less_checksum = crate::download::file::compute_sha256(&exif_less_path)
            .await
            .expect("media checksum");
        seed_downloaded_marker(
            &db,
            "GPS_EXIF_LESS",
            "exif-less.heic",
            &exif_less_path,
            &exif_less_checksum,
            AssetMetadata {
                metadata_hash: Some("exif-less-hash".into()),
                ..AssetMetadata::default()
            },
        )
        .await;
        seed_downloaded_marker(
            &db,
            "GPS_UNREADABLE",
            "unreadable.heic",
            &unreadable_path,
            "0000000000000000000000000000000000000000000000000000000000000000",
            AssetMetadata {
                metadata_hash: Some("unreadable-hash".into()),
                ..AssetMetadata::default()
            },
        )
        .await;

        let pass = run_pending(
            &db,
            MetadataFlags::XMP_SIDECAR,
            Arc::from(".gps-parse-retry-test"),
            &CancellationToken::new(),
        )
        .await;

        assert_eq!(
            pass.applied, 1,
            "EXIF-less media must still complete its sidecar"
        );
        assert_eq!(
            pass.failed, 1,
            "an unreadable source must fail so the retry marker survives"
        );
        assert!(
            exif_less_path.with_file_name("exif-less.heic.xmp").exists(),
            "EXIF-less media must still publish its sidecar"
        );
        assert!(
            !unreadable_path
                .with_file_name("unreadable.heic.xmp")
                .exists(),
            "an unreadable source must not publish a sidecar"
        );

        let pending = db
            .get_pending_metadata_rewrites(10)
            .await
            .expect("read retry markers");
        assert_eq!(
            pending.len(),
            1,
            "only the unreadable source keeps its durable marker"
        );
        assert_eq!(
            pending[0].id.as_ref(),
            "GPS_UNREADABLE",
            "the retained marker must be the unreadable source"
        );
    }

    /// End-to-end test of the metadata-rewrite pass. Seeds a downloaded row
    /// with a `metadata_write_failed_at` marker, then proves the configured
    /// metadata is applied while durable download state remains coherent.
    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn run_pending_applies_embed_and_clears_marker() {
        use crate::state::types::AssetMetadata;
        use crate::state::{AssetStatus, SqliteStateDb};

        let dir = tempfile::tempdir().unwrap();
        let photo_path = dir.path().join("rewrite_target.jpg");
        std::fs::write(&photo_path, minimal_jpeg_bytes()).unwrap();

        let db = SqliteStateDb::open_in_memory().unwrap();
        let seeded_checksum = crate::download::file::compute_sha256(&photo_path)
            .await
            .unwrap();

        let seeded_hash = "seed_hash_before_rewrite".to_string();
        let metadata = AssetMetadata {
            rating: Some(4),
            timezone_offset: Some(39_600),
            metadata_hash: Some(seeded_hash.clone()),
            ..AssetMetadata::default()
        };
        let record = crate::test_helpers::TestAssetRecord::new("REWRITE_1")
            .filename("rewrite_target.jpg")
            .checksum("rewrite_ck")
            .size(22)
            .created_at(
                chrono::Utc
                    .with_ymd_and_hms(2026, 1, 31, 22, 31, 59)
                    .unwrap(),
            )
            .metadata(metadata)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "REWRITE_1",
            "original",
            &photo_path,
            &seeded_checksum,
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

        let flags = MetadataFlags::DATETIME | MetadataFlags::RATING | MetadataFlags::EMBED_XMP;
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
            "a successful rewrite leaves the recorded metadata_hash in place"
        );

        let current_checksum = crate::download::file::compute_sha256(&photo_path)
            .await
            .unwrap();
        assert_ne!(current_checksum, seeded_checksum);
        let downloaded = db.get_downloaded_page(0, 1).await.unwrap();
        assert_eq!(downloaded[0].checksum.as_ref(), "rewrite_ck");
        assert_eq!(
            downloaded[0].local_checksum.as_deref(),
            Some(current_checksum.as_str())
        );

        let probe = crate::download::metadata::probe_exif(&photo_path).unwrap();
        assert_eq!(
            probe.datetime_original.as_deref(),
            Some("2026-02-01T09:31:59")
        );
        assert_eq!(probe.offset_time_original.as_deref(), Some("+11:00"));

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

    #[cfg(feature = "xmp")]
    fn grouped_xmp(meta: &xmp_toolkit::XmpMeta) -> (Vec<String>, Vec<String>) {
        let keywords: Vec<String> = meta
            .property_array(xmp_toolkit::xmp_ns::DC, "subject")
            .map(|value| value.value)
            .collect();
        let people: Vec<String> = meta
            .property_array(xmp_toolkit::xmp_ns::IPTC_EXT, "PersonInImage")
            .map(|value| value.value)
            .collect();
        (keywords, people)
    }

    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn run_pending_matches_normal_grouped_xmp_payload() {
        use crate::state::SqliteStateDb;
        use crate::state::types::AssetMetadata;
        use xmp_toolkit::{OpenFileOptions, XmpFile};

        let dir = tempfile::tempdir().unwrap();
        let photo_path = dir.path().join("grouped.jpg");
        std::fs::write(&photo_path, minimal_jpeg_bytes()).unwrap();
        let checksum = crate::download::file::compute_sha256(&photo_path)
            .await
            .unwrap();
        let metadata = AssetMetadata {
            keywords: Some(r#"["beach"]"#.into()),
            metadata_hash: Some("grouped-hash".into()),
            ..AssetMetadata::default()
        };
        let record = crate::test_helpers::TestAssetRecord::new("GROUPED")
            .filename("grouped.jpg")
            .metadata(metadata)
            .build();
        let db = SqliteStateDb::open_in_memory().unwrap();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "GROUPED",
            "original",
            &photo_path,
            &checksum,
            None,
        )
        .await
        .unwrap();
        db.record_metadata_write_failure("PrimarySync", "GROUPED", "original")
            .await
            .unwrap();
        db.add_asset_album("PrimarySync", "GROUPED", "Vacation", "icloud")
            .await
            .unwrap();
        db.add_asset_album("PrimarySync", "GROUPED", "beach", "icloud")
            .await
            .unwrap();
        {
            let conn = db.acquire_lock("seed person").unwrap();
            conn.execute(
                "INSERT INTO asset_people (library, asset_id, person_name) \
                 VALUES ('PrimarySync', 'GROUPED', 'Alice')",
                [],
            )
            .unwrap();
        }

        let grouping_rows = db
            .get_asset_groupings("PrimarySync", &["GROUPED"])
            .await
            .unwrap();
        let mut normal_groupings = AssetGroupings::default();
        for (asset_id, album) in grouping_rows.albums {
            normal_groupings
                .albums
                .entry(asset_id)
                .or_default()
                .push(album);
        }
        for (asset_id, person) in grouping_rows.people {
            normal_groupings
                .people
                .entry(asset_id)
                .or_default()
                .push(person);
        }
        let normal_payload = normal_groupings.metadata_payload("GROUPED", &record.metadata);
        assert_eq!(normal_payload.keywords, vec!["beach", "Vacation"]);
        assert_eq!(normal_payload.people, vec!["Alice"]);

        let normal_path = dir.path().join("normal.jpg");
        std::fs::write(&normal_path, minimal_jpeg_bytes()).unwrap();
        let normal_outcome = write_download_metadata(MetadataWriteRequest {
            final_path: &normal_path,
            embed_path: Some(&normal_path),
            sidecar_path: Some(&normal_path),
            payload: Arc::new(normal_payload),
            created_local: record.metadata.capture_local(record.created_at),
            flags: MetadataFlags::EMBED_XMP | MetadataFlags::XMP_SIDECAR,
            temp_suffix: ".meta-tmp",
        })
        .await;
        assert!(!normal_outcome.any_failed());

        let mut normal_file = XmpFile::new().unwrap();
        normal_file
            .open_file(&normal_path, OpenFileOptions::default().for_read())
            .unwrap();
        let normal_embedded = normal_file.xmp().expect("normal embedded XMP");
        let normal_sidecar = std::fs::read_to_string(dir.path().join("normal.jpg.xmp"))
            .unwrap()
            .parse()
            .unwrap();

        let pass = run_pending(
            &db,
            MetadataFlags::EMBED_XMP | MetadataFlags::XMP_SIDECAR,
            Arc::from(".meta-tmp"),
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(pass.applied, 1);

        let mut embedded_file = XmpFile::new().unwrap();
        embedded_file
            .open_file(&photo_path, OpenFileOptions::default().for_read())
            .unwrap();
        assert_eq!(
            grouped_xmp(&embedded_file.xmp().expect("rewritten embedded XMP")),
            grouped_xmp(&normal_embedded)
        );

        let sidecar = std::fs::read_to_string(dir.path().join("grouped.jpg.xmp"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(grouped_xmp(&sidecar), grouped_xmp(&normal_sidecar));
        assert_eq!(
            grouped_xmp(&sidecar),
            (
                vec!["beach".into(), "Vacation".into()],
                vec!["Alice".into()]
            )
        );
        assert!(
            db.get_pending_metadata_rewrites(1)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn grouping_read_failure_keeps_marker_without_writing() {
        use crate::state::SqliteStateDb;
        use crate::state::types::AssetMetadata;

        let dir = tempfile::tempdir().unwrap();
        let photo_path = dir.path().join("grouping_read_failure.jpg");
        let before = minimal_jpeg_bytes();
        std::fs::write(&photo_path, &before).unwrap();
        let checksum = crate::download::file::compute_sha256(&photo_path)
            .await
            .unwrap();
        let record = crate::test_helpers::TestAssetRecord::new("GROUPING_READ_FAILURE")
            .filename("grouping_read_failure.jpg")
            .metadata(AssetMetadata {
                keywords: Some(r#"["beach"]"#.into()),
                ..AssetMetadata::default()
            })
            .build();
        let db = SqliteStateDb::open_in_memory().unwrap();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "GROUPING_READ_FAILURE",
            "original",
            &photo_path,
            &checksum,
            None,
        )
        .await
        .unwrap();
        db.record_metadata_write_failure("PrimarySync", "GROUPING_READ_FAILURE", "original")
            .await
            .unwrap();
        db.acquire_lock("break grouping read")
            .unwrap()
            .execute("DROP TABLE asset_people", [])
            .unwrap();

        let pass = run_pending(
            &db,
            MetadataFlags::EMBED_XMP | MetadataFlags::XMP_SIDECAR,
            Arc::from(".meta-tmp"),
            &CancellationToken::new(),
        )
        .await;

        assert_eq!(pass.failed, 1);
        assert_eq!(std::fs::read(&photo_path).unwrap(), before);
        assert!(!dir.path().join("grouping_read_failure.jpg.xmp").exists());
        assert_eq!(db.get_pending_metadata_rewrites(1).await.unwrap().len(), 1);
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
        assert_eq!(pending.len(), 4);
        assert!(
            pending
                .iter()
                .any(|record| record.id.as_ref() == "UNSELECTED"),
            "unselected library marker must remain untouched"
        );
        assert!(
            !pending
                .iter()
                .any(|record| record.id.as_ref() == "SOFT_DELETED"),
            "a soft-deleted row must not be offered for rewrite, since its \
             metadata is frozen at the values held before the deletion"
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
        let seeded_checksum = crate::download::file::compute_sha256(&path).await.unwrap();
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
            &seeded_checksum,
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

    /// Seeds one downloaded asset carrying a rewrite marker into `db`, running
    /// the same sequence a real download failure leaves behind: `upsert_seen`,
    /// `mark_downloaded`, then `record_metadata_write_failure`. The caller
    /// supplies the on-disk path and its checksum so both readable media and
    /// deliberately unreadable sources can be staged.
    #[cfg(feature = "xmp")]
    async fn seed_downloaded_marker(
        db: &crate::state::SqliteStateDb,
        asset_id: &str,
        filename: &str,
        path: &std::path::Path,
        checksum: &str,
        metadata: crate::state::types::AssetMetadata,
    ) {
        let record = crate::test_helpers::TestAssetRecord::new(asset_id)
            .filename(filename)
            .metadata(metadata)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded("PrimarySync", asset_id, "original", path, checksum, None)
            .await
            .unwrap();
        db.record_metadata_write_failure("PrimarySync", asset_id, "original")
            .await
            .unwrap();
    }

    /// Seeds a downloaded JPEG carrying a rewrite marker. Returns the
    /// database, the media path, and the checksum recorded for the file.
    /// `rating` drives whether the embedded writer has anything to write.
    #[cfg(feature = "xmp")]
    async fn seed_marked_jpeg(
        dir: &std::path::Path,
        asset_id: &'static str,
        rating: Option<u8>,
    ) -> (crate::state::SqliteStateDb, PathBuf, String) {
        use crate::state::SqliteStateDb;
        use crate::state::types::AssetMetadata;

        let path = dir.join(format!("{asset_id}.jpg"));
        std::fs::write(&path, minimal_jpeg_bytes()).unwrap();
        let recorded = crate::download::file::compute_sha256(&path).await.unwrap();

        // File backed so a test can reopen it and drop connection-scoped
        // failure triggers between passes.
        let db = SqliteStateDb::open(&dir.join("state.db")).await.unwrap();
        seed_downloaded_marker(
            &db,
            asset_id,
            &format!("{asset_id}.jpg"),
            &path,
            &recorded,
            AssetMetadata {
                rating,
                metadata_hash: Some("fresh-hash".to_string()),
                ..AssetMetadata::default()
            },
        )
        .await;

        (db, path, recorded)
    }

    #[cfg(feature = "xmp")]
    fn embedded_rating_flags() -> MetadataFlags {
        MetadataFlags::RATING | MetadataFlags::EMBED_XMP
    }

    #[cfg(feature = "xmp")]
    async fn stored_checksums(
        db: &crate::state::SqliteStateDb,
    ) -> (Option<String>, Option<String>) {
        let row = db.get_downloaded_page(0, 1).await.unwrap().remove(0);
        (row.local_checksum, row.download_checksum)
    }

    /// #707 review: the drain must not re-hash a file it did not write. A file
    /// that already drifted from its recorded checksum is damage `verify` and
    /// `reconcile` must keep reporting, so the rewrite is refused outright.
    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn drain_refuses_to_rewrite_a_file_that_drifted_from_its_checksum() {
        let dir = tempfile::tempdir().unwrap();
        let (db, path, recorded) = seed_marked_jpeg(dir.path(), "DRIFTED", Some(3)).await;
        // Trailing bytes after EOI keep the container readable, so the writer
        // would still accept the file if the drain offered it.
        let mut drifted_bytes = minimal_jpeg_bytes();
        drifted_bytes.push(0x00);
        std::fs::write(&path, &drifted_bytes).unwrap();

        let pass = run_pending_page(
            &db,
            embedded_rating_flags() | MetadataFlags::XMP_SIDECAR,
            Arc::from(".meta-tmp"),
            &CancellationToken::new(),
            None,
            0,
        )
        .await;

        assert_eq!(
            std::fs::read(&path).unwrap(),
            drifted_bytes,
            "a file that does not match its checksum must not be rewritten"
        );
        let mut sidecar = path.clone().into_os_string();
        sidecar.push(".xmp");
        assert!(
            PathBuf::from(sidecar).exists(),
            "the sidecar is a separate file, so the export still runs"
        );
        assert_eq!(
            pass.failed, 1,
            "a refused rewrite is unfinished work, not a clean pass"
        );
        let (local, download) = stored_checksums(&db).await;
        assert_eq!(
            local.as_deref(),
            Some(recorded.as_str()),
            "the recorded checksum is the evidence of damage and must survive"
        );
        assert_eq!(download, None, "a refused rewrite records no download hash");
        assert_eq!(
            db.get_pending_metadata_rewrites(10).await.unwrap().len(),
            1,
            "the marker stays because the metadata never reached the file"
        );
    }

    /// #707 review: an embedded rewrite changes the media, so the row must
    /// carry the new hash and keep the pre-rewrite hash as the provider
    /// download checksum.
    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn drain_records_the_rewritten_media_checksum() {
        let dir = tempfile::tempdir().unwrap();
        let (db, path, recorded) = seed_marked_jpeg(dir.path(), "REWRITTEN", Some(3)).await;

        run_pending(
            &db,
            embedded_rating_flags(),
            Arc::from(".meta-tmp"),
            &CancellationToken::new(),
        )
        .await;

        let on_disk = crate::download::file::compute_sha256(&path).await.unwrap();
        assert_ne!(
            on_disk, recorded,
            "precondition: the rewrite must change the media bytes"
        );
        let (local, download) = stored_checksums(&db).await;
        assert_eq!(local.as_deref(), Some(on_disk.as_str()));
        assert_eq!(download.as_deref(), Some(recorded.as_str()));
        assert!(
            db.get_pending_metadata_rewrites(10)
                .await
                .unwrap()
                .is_empty(),
            "a complete rewrite retires its marker"
        );
    }

    /// #707 review: the checksum is stored before the marker retires, so a
    /// failure in between leaves a rewritten file the next pass can still
    /// recognise as its own.
    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn drain_keeps_the_marker_when_the_rewritten_checksum_cannot_be_stored() {
        let dir = tempfile::tempdir().unwrap();
        let (db, path, recorded) = seed_marked_jpeg(dir.path(), "CKFAIL", Some(3)).await;
        db.fail_metadata_checksum_write_for_test();

        run_pending(
            &db,
            embedded_rating_flags(),
            Arc::from(".meta-tmp"),
            &CancellationToken::new(),
        )
        .await;

        let on_disk = crate::download::file::compute_sha256(&path).await.unwrap();
        assert_ne!(on_disk, recorded, "precondition: the media was rewritten");
        assert_eq!(
            db.get_pending_metadata_rewrites(10).await.unwrap().len(),
            1,
            "the marker must survive so the rewrite is retried"
        );
        let (local, _) = stored_checksums(&db).await;
        assert_eq!(
            local, None,
            "a hash kei could not confirm must read as unknown, not as the              pre-rewrite value the next pass would treat as damage"
        );

        // A later pass, once the state write works again, must be able to
        // finish the job rather than refuse its own rewrite forever.
        let db = crate::state::SqliteStateDb::open(db.path()).await.unwrap();
        run_pending(
            &db,
            embedded_rating_flags(),
            Arc::from(".meta-tmp"),
            &CancellationToken::new(),
        )
        .await;

        let healed = crate::download::file::compute_sha256(&path).await.unwrap();
        let (local, _) = stored_checksums(&db).await;
        assert_eq!(
            local.as_deref(),
            Some(healed.as_str()),
            "the retry must record the bytes on disk"
        );
        assert!(
            db.get_pending_metadata_rewrites(10)
                .await
                .unwrap()
                .is_empty(),
            "the retry must retire the marker"
        );
    }

    /// #707 review: a sidecar-only rewrite leaves the media untouched, so it
    /// must not restate either checksum.
    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn drain_leaves_checksums_untouched_for_a_sidecar_only_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let (db, path, recorded) = seed_marked_jpeg(dir.path(), "SIDECAR", Some(3)).await;
        let before = std::fs::read(&path).unwrap();

        run_pending(
            &db,
            MetadataFlags::XMP_SIDECAR,
            Arc::from(".meta-tmp"),
            &CancellationToken::new(),
        )
        .await;

        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "a sidecar write must not touch the media"
        );
        let (local, download) = stored_checksums(&db).await;
        assert_eq!(local.as_deref(), Some(recorded.as_str()));
        assert_eq!(
            download, None,
            "no media rewrite happened, so no pre-rewrite hash is established"
        );
        assert!(
            db.get_pending_metadata_rewrites(10)
                .await
                .unwrap()
                .is_empty(),
            "the sidecar rewrite completed, so its marker retires"
        );
    }

    /// #682: a metadata-only drain must remove source fields that a prior kei
    /// sidecar write established, then retire the marker only after that
    /// updated sidecar lands.
    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn drain_clears_previously_owned_sidecar_fields() {
        use crate::state::SqliteStateDb;
        use crate::state::types::AssetMetadata;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SIDECAR_CLEAR.jpg");
        std::fs::write(&path, minimal_jpeg_bytes()).unwrap();
        let checksum = crate::download::file::compute_sha256(&path).await.unwrap();
        let db = SqliteStateDb::open_in_memory().unwrap();

        let initial = crate::test_helpers::TestAssetRecord::new("SIDECAR_CLEAR")
            .filename("SIDECAR_CLEAR.jpg")
            .metadata(AssetMetadata {
                rating: Some(5),
                description: Some("Old description".into()),
                metadata_hash: Some("initial-hash".into()),
                ..AssetMetadata::default()
            })
            .build();
        db.upsert_seen(&initial).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "SIDECAR_CLEAR",
            "original",
            &path,
            &checksum,
            None,
        )
        .await
        .unwrap();
        db.record_metadata_write_failure("PrimarySync", "SIDECAR_CLEAR", "original")
            .await
            .unwrap();

        run_pending(
            &db,
            MetadataFlags::XMP_SIDECAR,
            Arc::from(".meta-tmp"),
            &CancellationToken::new(),
        )
        .await;
        let sidecar_path = dir.path().join("SIDECAR_CLEAR.jpg.xmp");
        let initial_sidecar = std::fs::read_to_string(&sidecar_path).unwrap();
        assert!(initial_sidecar.contains("Rating"));
        assert!(initial_sidecar.contains("Old description"));

        let cleared = crate::test_helpers::TestAssetRecord::new("SIDECAR_CLEAR")
            .filename("SIDECAR_CLEAR.jpg")
            .metadata(AssetMetadata {
                metadata_hash: Some("cleared-hash".into()),
                ..AssetMetadata::default()
            })
            .build();
        db.upsert_seen(&cleared).await.unwrap();
        db.record_metadata_write_failure("PrimarySync", "SIDECAR_CLEAR", "original")
            .await
            .unwrap();

        let pass = run_pending(
            &db,
            MetadataFlags::XMP_SIDECAR,
            Arc::from(".meta-tmp"),
            &CancellationToken::new(),
        )
        .await;

        assert_eq!(pass.applied, 1);
        assert_eq!(pass.failed, 0);
        assert!(
            db.get_pending_metadata_rewrites(10)
                .await
                .unwrap()
                .is_empty(),
            "the marker retires after the cleared sidecar lands"
        );
        let cleared_sidecar = std::fs::read_to_string(&sidecar_path).unwrap();
        assert!(!cleared_sidecar.contains("Old description"));
        assert!(
            !cleared_sidecar.contains("xmp:Rating"),
            "the old rating and its ownership marker must both be gone"
        );
    }

    /// #707 review: a legacy row carries no checksum, so there is nothing to
    /// verify against and nothing to protect. The rewrite proceeds and
    /// establishes the baseline.
    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn drain_establishes_a_checksum_baseline_when_none_was_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let (db, path, _recorded) = seed_marked_jpeg(dir.path(), "LEGACY", Some(3)).await;
        db.clear_local_checksum_for_test("PrimarySync", "LEGACY", "original");

        run_pending(
            &db,
            embedded_rating_flags(),
            Arc::from(".meta-tmp"),
            &CancellationToken::new(),
        )
        .await;

        let on_disk = crate::download::file::compute_sha256(&path).await.unwrap();
        let (local, download) = stored_checksums(&db).await;
        assert_eq!(
            local.as_deref(),
            Some(on_disk.as_str()),
            "the rewrite establishes the checksum a legacy row never had"
        );
        assert_eq!(
            download, None,
            "the pre-rewrite bytes were never verified, so kei cannot claim              them as the provider download"
        );
        assert!(
            db.get_pending_metadata_rewrites(10)
                .await
                .unwrap()
                .is_empty(),
            "a legacy row must not be stranded behind a missing checksum"
        );
        let (counts, _) = crate::commands::reconcile::scan_local_drift(
            &db,
            |_: &crate::commands::reconcile::LocalDriftAsset| {},
            |_: &str| {},
        )
        .await
        .unwrap();
        assert_eq!(
            counts.damaged, 1,
            "a legacy file short of its provider size must still read as damaged"
        );
    }

    /// #707 review: the rewritten hash is stored before the marker retires, so
    /// a failure later in the same attempt still leaves the row describing the
    /// bytes on disk. Without that, the next pass would read its own rewrite as
    /// damage and refuse to touch it again.
    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn drain_records_the_rewritten_checksum_even_when_the_sidecar_fails() {
        let dir = tempfile::tempdir().unwrap();
        let (db, path, recorded) = seed_marked_jpeg(dir.path(), "SIDEFAIL", Some(3)).await;

        // A directory where the sidecar belongs fails the sidecar write while
        // leaving the embedded write free to change the media.
        let mut sidecar = path.clone().into_os_string();
        sidecar.push(".xmp");
        std::fs::create_dir(PathBuf::from(sidecar)).unwrap();

        run_pending(
            &db,
            embedded_rating_flags() | MetadataFlags::XMP_SIDECAR,
            Arc::from(".meta-tmp"),
            &CancellationToken::new(),
        )
        .await;

        let on_disk = crate::download::file::compute_sha256(&path).await.unwrap();
        assert_ne!(
            on_disk, recorded,
            "precondition: the embedded write must land before the sidecar fails"
        );
        let (local, download) = stored_checksums(&db).await;
        assert_eq!(
            local.as_deref(),
            Some(on_disk.as_str()),
            "the row must describe the rewritten bytes even though the attempt failed"
        );
        assert_eq!(download.as_deref(), Some(recorded.as_str()));
        assert_eq!(
            db.get_pending_metadata_rewrites(10).await.unwrap().len(),
            1,
            "the sidecar still owes a write, so the marker stays"
        );
    }

    /// #707 review: a rewrite with nothing to write leaves the media alone, so
    /// the row must not gain a checksum for bytes kei never wrote. Vouching for
    /// an untouched file would hide damage that arrived some other way.
    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn drain_writing_nothing_does_not_vouch_for_the_file() {
        let dir = tempfile::tempdir().unwrap();
        // No rating to write, and only the rating writer is enabled, so the
        // embedded plan comes out empty.
        let (db, path, recorded) = seed_marked_jpeg(dir.path(), "UNTOUCHED", None).await;
        let before = std::fs::read(&path).unwrap();

        run_pending(
            &db,
            MetadataFlags::RATING,
            Arc::from(".meta-tmp"),
            &CancellationToken::new(),
        )
        .await;

        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "precondition: an empty plan must leave the media alone"
        );
        let (local, download) = stored_checksums(&db).await;
        assert_eq!(local.as_deref(), Some(recorded.as_str()));
        assert_eq!(
            download, None,
            "kei must not claim a pre-rewrite hash for a file it did not rewrite"
        );
    }

    /// #718 taught reconcile that a metadata-rewritten file is legitimately
    /// smaller than the provider size, proving it by hashing against
    /// `local_checksum`. That proof needs both checksums, so a drained file
    /// must not be reported as truncated damage.
    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn drained_file_is_not_reported_as_truncated_damage() {
        let dir = tempfile::tempdir().unwrap();
        let (db, _path, _recorded) = seed_marked_jpeg(dir.path(), "SHRUNK", Some(3)).await;

        let (before, _) = stored_checksums(&db).await;
        let (counts, drift) = crate::commands::reconcile::scan_local_drift(
            &db,
            |_: &crate::commands::reconcile::LocalDriftAsset| {},
            |_: &str| {},
        )
        .await
        .unwrap();
        assert_eq!(
            counts.damaged, 1,
            "precondition: the provider size is larger than the file, so an \
             unexplained difference reads as damage"
        );
        assert_eq!(drift.len(), 1);

        run_pending(
            &db,
            embedded_rating_flags(),
            Arc::from(".meta-tmp"),
            &CancellationToken::new(),
        )
        .await;

        let (after, download) = stored_checksums(&db).await;
        assert_ne!(after, before, "precondition: the drain rewrote the media");
        assert!(
            download.is_some(),
            "reconcile needs the pre-rewrite hash to run its proof"
        );

        let (counts, drift) = crate::commands::reconcile::scan_local_drift(
            &db,
            |_: &crate::commands::reconcile::LocalDriftAsset| {},
            |_: &str| {},
        )
        .await
        .unwrap();
        assert_eq!(counts.present, 1);
        assert_eq!(counts.damaged, 0, "a drained file is intact, not truncated");
        assert!(drift.is_empty());
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
        let invalid_checksum = crate::download::file::compute_sha256(&invalid_path)
            .await
            .unwrap();
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
            db.mark_downloaded(
                "PrimarySync",
                &id,
                "original",
                &invalid_path,
                &invalid_checksum,
                None,
            )
            .await
            .unwrap();
            db.record_metadata_write_failure("PrimarySync", &id, "original")
                .await
                .unwrap();
        }
        let valid_path = dir.path().join("valid.jpg");
        std::fs::write(&valid_path, minimal_jpeg_bytes()).unwrap();
        let valid_checksum = crate::download::file::compute_sha256(&valid_path)
            .await
            .unwrap();
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
            &valid_checksum,
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
