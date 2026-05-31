//! Single-cycle sync runner.
//!
//! This module owns the per-cycle boundary that sits between the outer
//! process/watch loop and the download engine: mode choice, enumeration config
//! hash handling, grouping preload, download dispatch, zone-token writes, and
//! the aggregate cycle result.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::auth;
use crate::config;
use crate::download;
use crate::state;

/// Per-library state: zone name, sync token key, and resolved album plan.
pub(crate) struct LibraryState {
    pub(crate) library: crate::icloud::photos::PhotoLibrary,
    pub(crate) cross_zone_libraries: Vec<crate::icloud::photos::PhotoLibrary>,
    pub(crate) pass_scope: crate::commands::PassScope,
    pub(crate) zone_name: String,
    pub(crate) sync_token_key: String,
    /// Ordered list of download passes. Each pass carries its own
    /// exclude-asset-ids set. See [`crate::commands::AlbumPlan`].
    pub(crate) plan: crate::commands::AlbumPlan,
    /// True when `resolve_passes` failed at the end of the prior cycle and
    /// the plan above is the previous cycle's stale snapshot. Album
    /// membership data captured under a stale plan can route assets to the
    /// wrong pass (e.g. an asset added to a newly-created album shows up in
    /// the unfiled pass), so any cycle that consumes a stale plan must not
    /// advance the sync token for any zone -- doing so would skip the
    /// change events those assets generated and leave `asset_albums`
    /// permanently incomplete.
    pub(crate) plan_is_stale: bool,
    /// True after an idle watch sleep. Refreshing only when a later
    /// `changes/database` pre-check finds relevant work avoids burning album
    /// listing calls on quiet watch cycles.
    pub(crate) plan_needs_refresh: bool,
}

/// Metadata key holding the SHA-256 of the enumeration-affecting subset of
/// the user's download config. Distinct from the path-affecting
/// `config_hash` consumed by the download pipeline; using a single key for
/// both would cause each cycle to overwrite the other's value and
/// permanently invalidate incremental sync.
pub(crate) const ENUM_CONFIG_HASH_KEY: &str = "enum_config_hash";

/// Prefix for every per-zone CloudKit sync token row in the metadata
/// table. Cleared en masse when [`ENUM_CONFIG_HASH_KEY`] changes so the
/// next cycle falls back to full enumeration.
pub(crate) const SYNC_TOKEN_PREFIX: &str = "sync_token:";

/// Return the metadata key that stores the CloudKit sync token for a zone.
pub(crate) fn sync_token_key(zone_name: &str) -> String {
    format!("{SYNC_TOKEN_PREFIX}{zone_name}")
}

/// Outcome of a single sync cycle across all libraries.
#[derive(Debug)]
pub(crate) struct CycleResult {
    pub(crate) failed_count: usize,
    pub(crate) session_expired: bool,
    pub(crate) stats: download::SyncStats,
    pub(crate) db_sync_token_advance_safe: bool,
}

/// Decide whether the per-zone `sync_token` should be persisted to the state
/// DB after a download pass.
///
/// The contract is "advance only on full success and not in dry-run":
/// - On `PartialFailure`, a stored token would skip the failed assets on the
///   next incremental sync (they'd never appear in the delta again -- silent
///   data loss).
/// - On `SessionExpired`, the cycle aborts mid-stream; the token may be
///   stale or only reflect a subset of the work.
/// - In `--dry-run`, we promise to make no DB writes that survive the run
///   (apart from the `sync_runs` ledger). Advancing the token here would
///   silently break the next real sync.
///
/// The returned bool is the gate: callers still check that `sync_token` is
/// `Some(_)` and that a state DB is configured before persisting.
pub(crate) fn should_store_sync_token(outcome: &download::DownloadOutcome, dry_run: bool) -> bool {
    matches!(outcome, download::DownloadOutcome::Success) && !dry_run
}

/// Cycle-level gate that combines the per-library outcome check with the
/// cross-library "any plan is stale" override.
///
/// If any library entered the cycle with a reused plan (the prior
/// album refresh failed), suppress sync-token advancement for every library
/// in the cycle. A stale plan can route assets created or moved between
/// cycles to the wrong pass; advancing the token would skip the change
/// events that would surface those assets correctly on the next refresh,
/// leaving `asset_albums` permanently incomplete.
pub(crate) fn should_store_sync_token_for_cycle(
    outcome: &download::DownloadOutcome,
    dry_run: bool,
    cycle_has_stale_plan: bool,
) -> bool {
    should_store_sync_token(outcome, dry_run) && !cycle_has_stale_plan
}

fn has_active_passes(lib_state: &LibraryState) -> bool {
    !lib_state.plan.passes.is_empty()
}

fn should_warn_zero_assets(
    sync_result: &download::SyncResult,
    library_completed_without_errors: bool,
    run_mode: download::DownloadRunMode,
    is_retry_failed: bool,
    lib_state: &LibraryState,
) -> bool {
    sync_result.full_enumeration_ran
        && library_completed_without_errors
        && run_mode.downloads_files()
        && !is_retry_failed
        && sync_result.stats.assets_seen == 0
        && has_active_passes(lib_state)
}

/// Closure shape used to derive a per-library `DownloadConfig` from the
/// shared base config. Boxed dyn so `run_cycle` can accept a single
/// reference instead of a generic parameter (avoids reuse-by-monomorphization
/// blow-up in error messages).
pub(crate) type BuildDownloadConfigFn<'a> = dyn Fn(
        download::SyncMode,
        Arc<rustc_hash::FxHashSet<String>>,
        Arc<download::AssetGroupings>,
        Arc<str>,
    ) -> Arc<download::DownloadConfig>
    + 'a;

/// Outcome of [`check_and_persist_enum_config_hash`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnumConfigHashOutcome {
    /// No prior hash; current hash persisted. Sync tokens left alone:
    /// a first-run DB must not invalidate tokens another process may
    /// have written.
    Initial,
    Unchanged,
    /// Hash drifted; sync tokens cleared and new hash persisted so this
    /// cycle falls back to full enumeration.
    Changed,
    /// Hash drift was detected, but the stale sync-token rows could not be
    /// cleared. The new hash was not persisted, and the current cycle must
    /// force full enumeration rather than trusting any surviving tokens.
    ChangedTokenPurgeFailed,
    /// The stored hash could not be read, so the cycle cannot prove whether
    /// existing sync tokens still match the current config.
    ReadFailed,
}

impl EnumConfigHashOutcome {
    fn must_force_full_sync(self) -> bool {
        matches!(
            self,
            Self::Changed | Self::ChangedTokenPurgeFailed | Self::ReadFailed
        )
    }
}

/// State of the path-affecting download config hash before a cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DownloadConfigHashOutcome {
    Unchanged,
    Changed,
    ReadFailed,
    TokenPurgeFailed,
}

impl DownloadConfigHashOutcome {
    const fn must_force_full_sync(self) -> bool {
        !matches!(self, Self::Unchanged)
    }

    const fn token_purge_failed(self) -> bool {
        matches!(self, Self::TokenPurgeFailed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SyncModeDecision {
    pub(crate) mode: download::SyncMode,
    pub(crate) full_enumeration_reason: Option<download::FullEnumerationReason>,
}

/// Compare the current download-config hash against the one stored in
/// the state DB and react to drift. Storage failures are logged at warn
/// and swallowed, but the new hash is never persisted unless stale sync
/// tokens were cleared first. Otherwise a failed purge could leave old
/// tokens behind while the updated hash makes later cycles trust them.
pub(crate) async fn check_and_persist_enum_config_hash<D>(
    db: &D,
    current_hash: &str,
) -> EnumConfigHashOutcome
where
    D: state::SyncTokenStore + ?Sized,
{
    let stored_hash = match db.get_metadata(ENUM_CONFIG_HASH_KEY).await {
        Ok(hash) => hash,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to read enum_config_hash");
            return EnumConfigHashOutcome::ReadFailed;
        }
    };
    let outcome = match stored_hash.as_deref() {
        Some(h) if h == current_hash => return EnumConfigHashOutcome::Unchanged,
        Some(_) => EnumConfigHashOutcome::Changed,
        None => EnumConfigHashOutcome::Initial,
    };

    if matches!(outcome, EnumConfigHashOutcome::Changed) {
        tracing::info!("Download config changed since last sync, clearing sync tokens");
        match db.delete_metadata_by_prefix(SYNC_TOKEN_PREFIX).await {
            Ok(n) if n > 0 => {
                tracing::debug!(cleared = n, "Cleared stale sync tokens");
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Failed to clear sync tokens; enum_config_hash will not advance"
                );
                return EnumConfigHashOutcome::ChangedTokenPurgeFailed;
            }
            _ => {}
        }
    }
    if let Err(e) = db.set_metadata(ENUM_CONFIG_HASH_KEY, current_hash).await {
        tracing::warn!(error = %e, "Failed to persist enum_config_hash");
    }
    outcome
}

/// Check the path-affecting download config hash.
///
/// This hash does not prove CloudKit cursor safety. It proves local
/// path/state trust: after a directory or filename-template change, an
/// incremental `/changes/zone` cycle would only see new deltas and could miss
/// already-known assets that must be written to the new local paths.
pub(crate) async fn check_download_config_hash_for_cycle<D>(
    db: &D,
    current_hash: &str,
) -> DownloadConfigHashOutcome
where
    D: state::SyncTokenStore + ?Sized,
{
    match db.get_metadata(download::DOWNLOAD_CONFIG_HASH_KEY).await {
        Ok(Some(stored)) if stored == current_hash => DownloadConfigHashOutcome::Unchanged,
        Ok(None) => DownloadConfigHashOutcome::Unchanged,
        Ok(Some(_stored)) => match db.delete_metadata_by_prefix(SYNC_TOKEN_PREFIX).await {
            Ok(n) if n > 0 => {
                tracing::debug!(
                    cleared = n,
                    "Cleared sync tokens after download config hash drift"
                );
                DownloadConfigHashOutcome::Changed
            }
            Ok(_) => DownloadConfigHashOutcome::Changed,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Failed to clear sync tokens after download config hash drift"
                );
                DownloadConfigHashOutcome::TokenPurgeFailed
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, "Failed to read download config_hash");
            DownloadConfigHashOutcome::ReadFailed
        }
    }
}

/// Run one sync cycle: iterate all libraries, download photos, store sync tokens.
pub(crate) async fn run_cycle(
    library_states: &[&LibraryState],
    config: &config::Config,
    state_db: Option<&dyn state::StateDb>,
    is_retry_failed: bool,
    build_download_config: &BuildDownloadConfigFn<'_>,
    download_controls: download::DownloadControls,
    shared_session: &auth::SharedSession,
    shutdown_token: &CancellationToken,
) -> anyhow::Result<CycleResult> {
    let mut cycle_failed_count = 0usize;
    let mut cycle_session_expired = false;
    let mut cycle_stats = download::SyncStats::default();

    // If ANY library entered the cycle with a stale plan (the prior
    // album refresh failed and the previous plan is being reused), suppress
    // sync-token advancement for every library in this cycle. A reused plan
    // can route assets created or moved between cycles to the wrong pass and
    // produce silently incomplete `asset_albums` data; without this gate the
    // change events for those assets sit behind the advanced token and
    // never replay.
    let cycle_has_stale_plan = library_states.iter().any(|s| s.plan_is_stale);
    if cycle_has_stale_plan {
        tracing::warn!(
            "One or more libraries are running on a stale album plan; sync \
             token will not advance this cycle"
        );
    }
    let mut db_sync_token_advance_safe = !config.runtime.dry_run && !cycle_has_stale_plan;
    let mut force_full_for_config_hash = false;
    let mut force_full_for_download_config_hash = false;

    // Check if token-unsafe eligibility config changed since last sync. If
    // so, clear sync tokens and force full enumeration for this cycle -- the
    // stored incremental token would miss assets that are newly eligible
    // under the changed config (e.g. a user switching [photos].resolution or
    // changing [filters].media). The hash is
    // cycle-invariant across libraries,
    // so this runs once per cycle, not once per library.
    //
    // The metadata key `enum_config_hash` is distinct from the download
    // pipeline's `config_hash` (which tracks path-affecting fields only).
    // Using a single key for both would cause the two hashes to overwrite
    // each other every cycle, permanently preventing incremental sync.
    if !config.runtime.dry_run {
        if let Some(db) = state_db {
            let config_hash = download::compute_config_hash(config);
            let config_hash_outcome = check_and_persist_enum_config_hash(db, &config_hash).await;
            force_full_for_config_hash = config_hash_outcome.must_force_full_sync();
            if matches!(
                config_hash_outcome,
                EnumConfigHashOutcome::ChangedTokenPurgeFailed | EnumConfigHashOutcome::ReadFailed
            ) {
                db_sync_token_advance_safe = false;
            }
        }
    }

    // A path-affecting config drift does not make the CloudKit token itself
    // unsafe, but it does make an incremental cycle insufficient: with no
    // changed assets, `/changes/zone` would never re-plan already-known media
    // into the new directory/template. Detect that before choosing
    // incremental mode and force a full reconciliation for this cycle.
    if !config.runtime.dry_run {
        if let (Some(db), Some(first_library)) = (state_db, library_states.first()) {
            let probe_config = build_download_config(
                download::SyncMode::Full,
                Arc::new(rustc_hash::FxHashSet::default()),
                Arc::new(download::AssetGroupings::default()),
                Arc::from(first_library.zone_name.as_str()),
            );
            let download_config_hash = download::hash_download_config(&probe_config);
            let outcome = check_download_config_hash_for_cycle(db, &download_config_hash).await;
            force_full_for_download_config_hash = outcome.must_force_full_sync();
            if outcome.token_purge_failed() {
                db_sync_token_advance_safe = false;
            }
        }
    }

    for lib_state in library_states {
        if shutdown_token.is_cancelled() {
            break;
        }

        // Determine sync mode per-library
        // retry-failed must always use full enumeration: incremental
        // sync only returns NEW iCloud changes, missing previously-
        // failed assets that were already enumerated but not downloaded.
        let sync_mode_decision = if force_full_for_config_hash {
            let reason = download::FullEnumerationReason::EnumConfigHashDrift;
            tracing::debug!(
                zone = %lib_state.zone_name,
                full_enumeration_reason = reason.as_str(),
                "Forcing full sync because config-hash validation invalidated stored sync tokens"
            );
            SyncModeDecision {
                mode: download::SyncMode::Full,
                full_enumeration_reason: Some(reason),
            }
        } else if force_full_for_download_config_hash {
            let reason = download::FullEnumerationReason::DownloadConfigHashDrift;
            tracing::debug!(
                zone = %lib_state.zone_name,
                full_enumeration_reason = reason.as_str(),
                "Forcing full sync because download config hash drift requires local path reconciliation"
            );
            SyncModeDecision {
                mode: download::SyncMode::Full,
                full_enumeration_reason: Some(reason),
            }
        } else {
            determine_sync_mode_decision(
                is_retry_failed,
                library_states.len(),
                state_db,
                &lib_state.sync_token_key,
                &lib_state.zone_name,
            )
            .await
        };
        let sync_mode = sync_mode_decision.mode;

        let sync_mode_label = match &sync_mode {
            download::SyncMode::Full => "full",
            download::SyncMode::Incremental { .. } => "incremental",
        };
        if let Some(reason) = sync_mode_decision.full_enumeration_reason {
            tracing::info!(
                sync_mode = sync_mode_label,
                zone = %lib_state.zone_name,
                full_enumeration_reason = reason.as_str(),
                "Starting full-enumeration sync cycle"
            );
        } else {
            tracing::debug!(sync_mode = sync_mode_label, zone = %lib_state.zone_name, "Starting sync cycle");
        }

        // Skip the DB scan entirely when nothing downstream will read it.
        #[cfg(feature = "xmp")]
        let asset_groupings = if config.metadata.embed_xmp || config.metadata.xmp_sidecar {
            preload_asset_groupings(state_db, &lib_state.zone_name).await
        } else {
            Arc::new(download::AssetGroupings::default())
        };
        #[cfg(not(feature = "xmp"))]
        let asset_groupings = Arc::new(download::AssetGroupings::default());
        // Each pass carries its own exclude-asset-ids, so the config built
        // here starts with an empty set; download_photos_with_sync derives
        // per-pass configs internally via `with_exclude_ids`.
        let download_config = build_download_config(
            sync_mode,
            Arc::new(rustc_hash::FxHashSet::default()),
            asset_groupings,
            Arc::from(lib_state.zone_name.as_str()),
        );
        let download_client = shared_session.read().await.download_client().clone();
        let mut sync_result = download::download_photos_with_sync(
            &download_client,
            &lib_state.plan.passes,
            download_config,
            download_controls,
            shutdown_token.clone(),
        )
        .await?;

        if sync_result.full_enumeration_ran && sync_result.stats.full_enumeration_reason.is_none() {
            sync_result.stats.full_enumeration_reason = sync_mode_decision.full_enumeration_reason;
        }

        let library_completed_without_errors =
            matches!(&sync_result.outcome, download::DownloadOutcome::Success)
                && !sync_result.stats.interrupted
                && sync_result.stats.enumeration_errors == 0
                && !shutdown_token.is_cancelled();
        if should_warn_zero_assets(
            &sync_result,
            library_completed_without_errors,
            download_controls.run_mode,
            is_retry_failed,
            lib_state,
        ) {
            tracing::warn!(
                library = %lib_state.zone_name,
                library_count = library_states.len(),
                assets_seen = sync_result.stats.assets_seen,
                "Sync completed after enumerating zero assets; check iCloud library \
                 access and filters if this was unexpected"
            );
        }

        // Store the zone token only after the download engine has returned a
        // clean result and flushed all batch state writes. `Success` excludes
        // partial failures; the extra interrupted and shutdown gates below
        // catch cancellation paths that can still carry a token. A crash
        // before this metadata write leaves the old token in place, so the
        // zone replays next cycle instead of skipping unfinalized work.
        let should_store_token = should_store_sync_token_for_cycle(
            &sync_result.outcome,
            config.runtime.dry_run,
            cycle_has_stale_plan,
        ) && !sync_result.stats.interrupted
            && !shutdown_token.is_cancelled();
        if should_store_token {
            match (&sync_result.sync_token, state_db) {
                (Some(token), Some(db)) => {
                    if let Err(e) = db.set_metadata(&lib_state.sync_token_key, token).await {
                        db_sync_token_advance_safe = false;
                        tracing::warn!(error = %e, "Failed to store sync token");
                    } else {
                        tracing::debug!(zone = %lib_state.zone_name, "Stored sync token for next incremental sync");
                    }
                }
                (Some(_), None) => {
                    db_sync_token_advance_safe = false;
                    tracing::debug!(
                        zone = %lib_state.zone_name,
                        "Sync token available but no state DB is configured"
                    );
                }
                (None, _) => {
                    db_sync_token_advance_safe = false;
                    let reason = sync_result
                        .stats
                        .sync_token_blocked_reason
                        .unwrap_or("sync_token_missing");
                    let source = sync_result
                        .stats
                        .sync_token_blocked_source
                        .unwrap_or_else(|| download::sync_token_blocked_source(reason));
                    let explanation = sync_result
                        .stats
                        .sync_token_blocked_explanation
                        .as_ref()
                        .copied()
                        .unwrap_or_else(|| download::sync_token_blocked_explanation(reason));
                    let observation = match (
                        sync_result.stats.sync_token_expected_receivers,
                        sync_result.stats.sync_token_receivers_with_token,
                    ) {
                        (Some(expected), Some(with_token)) => {
                            let missing = sync_result.stats.sync_token_receivers_missing.unwrap_or(0);
                            let blank = sync_result.stats.sync_token_receivers_blank.unwrap_or(0);
                            let dropped = sync_result.stats.sync_token_receivers_dropped.unwrap_or(0);
                            let unique = sync_result.stats.sync_token_unique_values.unwrap_or(0);
                            format!(
                                "Observed usable sync tokens on {with_token}/{expected} passes (missing: {missing}, blank: {blank}, dropped: {dropped}, unique values: {unique})"
                            )
                        }
                        _ => "No per-pass sync token observation details were collected for this reason"
                            .to_string(),
                    };
                    tracing::warn!(
                        zone = %lib_state.zone_name,
                        reason,
                        source,
                        explanation,
                        observation,
                        "Sync token did not advance after this successful sync. Here's why: {}. {}. Next cycle will run full enumeration",
                        explanation,
                        observation
                    );
                }
            }
        } else if sync_result.sync_token.is_some() {
            db_sync_token_advance_safe = false;
            tracing::info!(
                zone = %lib_state.zone_name,
                "Sync token NOT advanced (incomplete sync -- will replay changes next cycle)"
            );
        }

        if sync_result.stats.sync_token_blocked
            && sync_result.stats.sync_token_blocked_zone.is_none()
        {
            sync_result.stats.sync_token_blocked_zone = Some(lib_state.zone_name.clone());
        }

        // Accumulate stats across libraries.
        cycle_stats.accumulate(&sync_result.stats);

        match sync_result.outcome {
            download::DownloadOutcome::Success => {}
            download::DownloadOutcome::SessionExpired { auth_error_count } => {
                tracing::warn!(
                    auth_error_count,
                    zone = %lib_state.zone_name,
                    "Session expired during library sync"
                );
                cycle_session_expired = true;
                break; // Stop iterating libraries -- need re-auth
            }
            download::DownloadOutcome::PartialFailure { failed_count } => {
                cycle_failed_count += failed_count;
            }
        }
    }

    Ok(CycleResult {
        failed_count: cycle_failed_count,
        session_expired: cycle_session_expired,
        stats: cycle_stats,
        db_sync_token_advance_safe,
    })
}

/// Bulk-load `asset_albums` + `asset_people` into an in-memory index so the
/// filter phase can enrich payloads without per-asset DB hits. Scoped to a
/// single library so multi-library accounts don't cross-attribute album /
/// person memberships across zones (the v9 schema scopes both join tables
/// per library; this reader honours that scope).
#[cfg(feature = "xmp")]
pub(crate) async fn preload_asset_groupings<D>(
    state_db: Option<&D>,
    library: &str,
) -> Arc<download::AssetGroupings>
where
    D: state::MembershipStore + ?Sized,
{
    let Some(db) = state_db else {
        return Arc::new(download::AssetGroupings::default());
    };
    let albums = db.get_all_asset_albums(library).await;
    let people = db.get_all_asset_people(library).await;
    let mut groupings = download::AssetGroupings::default();
    match albums {
        Ok(rows) => {
            for (asset_id, album) in rows {
                groupings.albums.entry(asset_id).or_default().push(album);
            }
        }
        Err(e) => tracing::warn!(error = %e, library, "Failed to preload asset_albums"),
    }
    match people {
        Ok(rows) => {
            for (asset_id, person) in rows {
                groupings.people.entry(asset_id).or_default().push(person);
            }
        }
        Err(e) => tracing::warn!(error = %e, library, "Failed to preload asset_people"),
    }
    Arc::new(groupings)
}

/// Determine the sync mode for a library: full enumeration or incremental.
pub(crate) async fn determine_sync_mode_decision<D>(
    is_retry_failed: bool,
    library_count: usize,
    state_db: Option<&D>,
    sync_token_key: &str,
    zone_name: &str,
) -> SyncModeDecision
where
    D: state::SyncTokenStore + ?Sized,
{
    if is_retry_failed {
        let reason = download::FullEnumerationReason::ExplicitRetryFailed;
        if library_count == 1 {
            tracing::info!(
                full_enumeration_reason = reason.as_str(),
                "Retry-failed always runs full enumeration because incremental sync only returns new iCloud changes and can miss older failed assets"
            );
        }
        SyncModeDecision {
            mode: download::SyncMode::Full,
            full_enumeration_reason: Some(reason),
        }
    } else if let Some(db) = state_db {
        match db.get_metadata(sync_token_key).await {
            Ok(Some(ref token)) if !token.is_empty() => {
                tracing::debug!(zone = %zone_name, "Stored sync token found, using incremental sync");
                SyncModeDecision {
                    mode: download::SyncMode::Incremental {
                        zone_sync_token: token.clone(),
                    },
                    full_enumeration_reason: None,
                }
            }
            Ok(_) => {
                let reason = download::FullEnumerationReason::NoStoredToken;
                tracing::debug!(
                    zone = %zone_name,
                    full_enumeration_reason = reason.as_str(),
                    "No sync token found, performing full enumeration"
                );
                SyncModeDecision {
                    mode: download::SyncMode::Full,
                    full_enumeration_reason: Some(reason),
                }
            }
            Err(e) => {
                let reason = download::FullEnumerationReason::OtherStaticReason;
                tracing::warn!(
                    error = %e,
                    full_enumeration_reason = reason.as_str(),
                    "Failed to load sync token, falling back to full enumeration"
                );
                SyncModeDecision {
                    mode: download::SyncMode::Full,
                    full_enumeration_reason: Some(reason),
                }
            }
        }
    } else {
        SyncModeDecision {
            mode: download::SyncMode::Full,
            full_enumeration_reason: Some(download::FullEnumerationReason::NoStoredToken),
        }
    }
}

#[cfg(test)]
pub(crate) async fn determine_sync_mode<D>(
    is_retry_failed: bool,
    library_count: usize,
    state_db: Option<&D>,
    sync_token_key: &str,
    zone_name: &str,
) -> download::SyncMode
where
    D: state::SyncTokenStore + ?Sized,
{
    determine_sync_mode_decision(
        is_retry_failed,
        library_count,
        state_db,
        sync_token_key,
        zone_name,
    )
    .await
    .mode
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::PassKind;

    fn make_library_state(has_passes: bool) -> LibraryState {
        LibraryState {
            library: crate::icloud::photos::PhotoLibrary::new_stub_with_zone(
                Box::new(crate::test_helpers::MockPhotosSession::new()),
                "PrimarySync",
            ),
            cross_zone_libraries: Vec::new(),
            pass_scope: crate::commands::PassScope {
                include_albums: false,
                include_smart_folders: false,
                include_unfiled: has_passes,
            },
            zone_name: "PrimarySync".to_string(),
            sync_token_key: "sync_token:PrimarySync".to_string(),
            plan: crate::commands::AlbumPlan {
                passes: if has_passes {
                    vec![crate::commands::AlbumPass {
                        kind: PassKind::Unfiled,
                        album: crate::icloud::photos::PhotoAlbum::stub_for_test(Arc::from(
                            "PrimarySync",
                        )),
                        exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
                    }]
                } else {
                    Vec::new()
                },
            },
            plan_is_stale: false,
            plan_needs_refresh: false,
        }
    }

    #[test]
    fn should_warn_zero_assets_requires_active_passes() {
        let sync_result = download::SyncResult {
            outcome: download::DownloadOutcome::Success,
            sync_token: Some("zone-token".to_string()),
            stats: download::SyncStats {
                assets_seen: 0,
                ..download::SyncStats::default()
            },
            full_enumeration_ran: true,
        };
        assert!(!should_warn_zero_assets(
            &sync_result,
            true,
            download::DownloadRunMode::Download,
            false,
            &make_library_state(false),
        ));
    }

    #[test]
    fn should_warn_zero_assets_when_all_gates_are_true() {
        let sync_result = download::SyncResult {
            outcome: download::DownloadOutcome::Success,
            sync_token: Some("zone-token".to_string()),
            stats: download::SyncStats {
                assets_seen: 0,
                ..download::SyncStats::default()
            },
            full_enumeration_ran: true,
        };
        assert!(should_warn_zero_assets(
            &sync_result,
            true,
            download::DownloadRunMode::Download,
            false,
            &make_library_state(true),
        ));
    }

    #[tokio::test]
    async fn determine_sync_mode_two_normal_syncs_reuse_stored_token() {
        let db = state::SqliteStateDb::open_in_memory().expect("state db");
        let sync_token_key = sync_token_key("PrimarySync");
        db.set_metadata(&sync_token_key, "stored-token-abc")
            .await
            .expect("set token");

        for cycle in 1..=2 {
            let mode =
                determine_sync_mode(false, 1, Some(&db), &sync_token_key, "PrimarySync").await;
            assert!(
                matches!(mode, download::SyncMode::Incremental { ref zone_sync_token } if zone_sync_token == "stored-token-abc"),
                "normal sync cycle {cycle} should use the stored token, got {mode:?}"
            );
        }

        assert_eq!(
            db.get_metadata(&sync_token_key).await.expect("read token"),
            Some("stored-token-abc".to_string()),
            "mode selection must not consume or clear the stored token"
        );
    }

    #[tokio::test]
    async fn determine_sync_mode_decision_records_explicit_full_reasons() {
        let db = state::SqliteStateDb::open_in_memory().expect("state db");
        let sync_token_key = sync_token_key("PrimarySync");
        db.set_metadata(&sync_token_key, "stored-token-abc")
            .await
            .expect("set token");

        let retry =
            determine_sync_mode_decision(true, 1, Some(&db), &sync_token_key, "PrimarySync").await;
        assert!(matches!(retry.mode, download::SyncMode::Full));
        assert_eq!(
            retry.full_enumeration_reason,
            Some(download::FullEnumerationReason::ExplicitRetryFailed)
        );

        db.delete_metadata_by_prefix(SYNC_TOKEN_PREFIX)
            .await
            .expect("clear token");
        let no_token =
            determine_sync_mode_decision(false, 1, Some(&db), &sync_token_key, "PrimarySync").await;
        assert!(matches!(no_token.mode, download::SyncMode::Full));
        assert_eq!(
            no_token.full_enumeration_reason,
            Some(download::FullEnumerationReason::NoStoredToken)
        );
    }
}
