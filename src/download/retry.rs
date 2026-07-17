//! Durable pending-retry identity resolution and targeted provider revalidation.

use std::sync::Arc;

use anyhow::Result;
use rustc_hash::{FxHashMap, FxHashSet};
use tokio_util::sync::CancellationToken;

use super::{
    DownloadConfig, DownloadStore, DownloadTask, PENDING_RETRY_UNMATCHED_REASON, RetryTaskKey,
    UrlRetrySource, build_pass_configs_resolving_deferred_excludes, planner,
};
use crate::icloud::photos::{PhotoAsset, ProviderRecordId, RecordLookupRequest, RecordResolution};
use crate::state::{AssetVerificationState, VersionSizeKey};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct PendingRetryTarget {
    pub(super) library: Arc<str>,
    pub(super) asset_id: Arc<str>,
    pub(super) version_size: VersionSizeKey,
}

impl PendingRetryTarget {
    pub(super) fn from_record(record: &crate::state::AssetRecord) -> Self {
        Self {
            library: Arc::clone(&record.library),
            asset_id: Arc::from(record.id.as_ref()),
            version_size: record.version_size,
        }
    }

    pub(super) fn from_task(task: &DownloadTask) -> Self {
        Self {
            library: Arc::clone(&task.library),
            asset_id: Arc::clone(&task.asset_id),
            version_size: task.version_size,
        }
    }
}

#[derive(Debug)]
struct PendingRetryEvidence {
    checksum: Arc<str>,
    size_bytes: u64,
}

impl PendingRetryEvidence {
    fn from_record(record: &crate::state::AssetRecord) -> Self {
        Self {
            checksum: Arc::from(record.checksum.as_ref()),
            size_bytes: record.size_bytes,
        }
    }
}

#[derive(Debug)]
enum LegacyCandidateSelection {
    Selected(PhotoAsset),
    Missing,
    Ambiguous { candidates: usize },
}

fn candidate_matches_durable_evidence(
    asset: &PhotoAsset,
    target: &PendingRetryTarget,
    evidence: &PendingRetryEvidence,
) -> bool {
    asset.versions().iter().any(|(version_size, version)| {
        VersionSizeKey::from(*version_size) == target.version_size
            && version.size == evidence.size_bytes
            && version.checksum.as_ref() == evidence.checksum.as_ref()
    })
}

fn select_legacy_candidate(
    candidates: Vec<PhotoAsset>,
    targets: &[&PendingRetryTarget],
    evidence: &FxHashMap<PendingRetryTarget, PendingRetryEvidence>,
) -> LegacyCandidateSelection {
    if candidates.is_empty() {
        return LegacyCandidateSelection::Missing;
    }
    if candidates.len() == 1 {
        return candidates.into_iter().next().map_or(
            LegacyCandidateSelection::Missing,
            LegacyCandidateSelection::Selected,
        );
    }

    let candidate_count = candidates.len();
    let mut matching = candidates.into_iter().filter(|asset| {
        targets.iter().any(|target| {
            evidence
                .get(*target)
                .is_some_and(|evidence| candidate_matches_durable_evidence(asset, target, evidence))
        })
    });
    let Some(selected) = matching.next() else {
        return LegacyCandidateSelection::Ambiguous {
            candidates: candidate_count,
        };
    };
    if matching.next().is_some() {
        return LegacyCandidateSelection::Ambiguous {
            candidates: candidate_count,
        };
    }
    LegacyCandidateSelection::Selected(selected)
}

pub(super) fn take_matching_pending_retry_tasks<I>(
    tasks: I,
    pending_targets: &mut FxHashSet<PendingRetryTarget>,
    out: &mut Vec<DownloadTask>,
) where
    I: IntoIterator<Item = DownloadTask>,
{
    for task in tasks {
        let target = PendingRetryTarget::from_task(&task);
        if pending_targets.remove(&target) {
            out.push(task);
            if pending_targets.is_empty() {
                break;
            }
        }
    }
}

async fn plan_resolved_pending_asset(
    asset: &PhotoAsset,
    state_id: &str,
    db: &dyn DownloadStore,
    pass_configs: &[Arc<DownloadConfig>],
    pending_targets: &mut FxHashSet<PendingRetryTarget>,
    task_planner: &mut planner::TaskPlanner,
    tasks: &mut Vec<DownloadTask>,
    retry_sources: &mut FxHashMap<RetryTaskKey, UrlRetrySource>,
) -> Result<()> {
    for target in pending_targets
        .iter()
        .filter(|target| target.asset_id.as_ref() == state_id)
    {
        db.clear_asset_verification(
            &target.library,
            &target.asset_id,
            target.version_size.as_str(),
        )
        .await?;
    }
    for (pass_index, pass_config) in pass_configs.iter().enumerate() {
        let plan = task_planner.plan_asset(asset, pass_config).await;
        if plan.filter_reason.is_some() {
            continue;
        }
        let first_new_task = tasks.len();
        take_matching_pending_retry_tasks(plan.tasks, pending_targets, tasks);
        for task in tasks.iter().skip(first_new_task) {
            retry_sources.insert(
                RetryTaskKey::from(task),
                UrlRetrySource {
                    asset_record_name: asset.asset_record_name_arc(),
                    pass_index,
                },
            );
        }
    }
    Ok(())
}

async fn set_verification_for_state_id(
    db: &dyn DownloadStore,
    pending_targets: &FxHashSet<PendingRetryTarget>,
    state_id: &str,
    state: AssetVerificationState,
    reason: &str,
) -> Result<()> {
    for target in pending_targets
        .iter()
        .filter(|target| target.asset_id.as_ref() == state_id)
    {
        db.set_asset_verification(
            &target.library,
            &target.asset_id,
            target.version_size.as_str(),
            state,
            reason,
        )
        .await?;
    }
    Ok(())
}

#[derive(Debug, Default)]
pub(super) struct PendingRetryPlan {
    pub(super) tasks: Vec<DownloadTask>,
    pub(super) retry_sources: FxHashMap<RetryTaskKey, UrlRetrySource>,
    pub(super) pass_configs: Vec<Arc<DownloadConfig>>,
    pub(super) unmatched_targets: Vec<PendingRetryTarget>,
    pub(super) requested: usize,
}

pub(super) async fn build_pending_retry_download_tasks(
    passes: &[crate::commands::AlbumPass],
    config: &DownloadConfig,
    shutdown_token: CancellationToken,
) -> Result<PendingRetryPlan> {
    let Some(db) = &config.state_db else {
        return Ok(PendingRetryPlan::default());
    };

    let pending = db.get_pending().await?;
    let mut pending_targets: FxHashSet<PendingRetryTarget> = pending
        .iter()
        .filter(|record| record.library.as_ref() == config.library.as_ref())
        .map(PendingRetryTarget::from_record)
        .collect();
    if pending_targets.is_empty() {
        return Ok(PendingRetryPlan::default());
    }
    let pending_evidence: FxHashMap<PendingRetryTarget, PendingRetryEvidence> = pending
        .iter()
        .filter(|record| record.library.as_ref() == config.library.as_ref())
        .map(|record| {
            (
                PendingRetryTarget::from_record(record),
                PendingRetryEvidence::from_record(record),
            )
        })
        .collect();

    let backfilled = db
        .backfill_asset_master_mappings_from_album_memberships()
        .await?;
    if backfilled > 0 {
        tracing::info!(
            inserted = backfilled,
            library = %config.library,
            "Backfilled asset/master mappings before pending retry"
        );
    }

    let requested = pending_targets.len();
    let pass_configs = build_pass_configs_resolving_deferred_excludes(passes, config).await?;
    let mut tasks: Vec<DownloadTask> = Vec::with_capacity(requested);
    let mut retry_sources: FxHashMap<RetryTaskKey, UrlRetrySource> = FxHashMap::default();
    let mut task_planner = planner::TaskPlanner::new();
    let mut lookup_requests = Vec::new();
    let mut seen_requests = FxHashSet::default();
    let mut master_by_state_id: FxHashMap<String, String> = FxHashMap::default();
    for record in pending
        .iter()
        .filter(|record| record.library.as_ref() == config.library.as_ref())
    {
        let mapped_master = db
            .get_master_record_name_for_asset(&config.library, &record.id)
            .await?;
        let master = mapped_master
            .as_deref()
            .unwrap_or(record.id.as_ref())
            .to_string();
        let asset_record_names = if mapped_master.is_some() {
            vec![record.id.to_string()]
        } else {
            db.get_asset_record_names_for_master(&config.library, &master)
                .await?
        };
        master_by_state_id.insert(record.id.to_string(), master.clone());
        if asset_record_names.is_empty() {
            let request_key = (record.id.to_string(), master.clone(), None);
            if seen_requests.insert(request_key.clone()) {
                lookup_requests.push(RecordLookupRequest::master_only(
                    ProviderRecordId::new(request_key.0),
                    ProviderRecordId::new(request_key.1),
                ));
            }
            continue;
        }
        for asset_record_name in asset_record_names {
            let request_key = (
                record.id.to_string(),
                master.clone(),
                Some(asset_record_name.clone()),
            );
            if seen_requests.insert(request_key.clone()) {
                lookup_requests.push(RecordLookupRequest::paired(
                    ProviderRecordId::new(request_key.0),
                    ProviderRecordId::new(request_key.1),
                    ProviderRecordId::new(asset_record_name),
                ));
            }
        }
    }

    let requested_state_ids: FxHashSet<&str> = lookup_requests
        .iter()
        .map(|request| request.state_id.as_str())
        .collect();
    for target in &pending_targets {
        if !requested_state_ids.contains(target.asset_id.as_ref()) {
            db.set_asset_verification(
                &target.library,
                &target.asset_id,
                target.version_size.as_str(),
                AssetVerificationState::Unknown,
                "stable provider asset/master mapping is unavailable",
            )
            .await?;
        }
    }

    let resolutions = if let Some(pass) = passes.first() {
        let batch = pass.album.resolve_records(&lookup_requests).await;
        if !batch.complete {
            tracing::warn!(
                library = %config.library,
                requested = lookup_requests.len(),
                "Pending provider revalidation completed with inconclusive results"
            );
        }
        batch.results
    } else {
        Vec::new()
    };
    let mut legacy_present_state_ids = FxHashSet::default();
    for (state_id, resolution) in resolutions {
        if pending_targets.is_empty() || shutdown_token.is_cancelled() {
            break;
        }
        match resolution {
            RecordResolution::Present(asset) => {
                plan_resolved_pending_asset(
                    &asset,
                    state_id.as_str(),
                    db.as_ref(),
                    &pass_configs,
                    &mut pending_targets,
                    &mut task_planner,
                    &mut tasks,
                    &mut retry_sources,
                )
                .await?;
            }
            RecordResolution::MasterPresent => {
                legacy_present_state_ids.insert(state_id.as_str().to_string());
            }
            RecordResolution::Deleted {
                deleted_at,
                master_family,
            } => {
                let state_id = state_id.as_str();
                let resolved = if master_family {
                    let master = master_by_state_id
                        .get(state_id)
                        .map(String::as_str)
                        .unwrap_or(state_id);
                    db.resolve_master_family_source_deleted_affected(
                        &config.library,
                        master,
                        deleted_at,
                    )
                    .await?
                } else {
                    db.resolve_source_deleted_affected(&config.library, state_id, deleted_at)
                        .await?
                };
                tracing::info!(
                    library = %config.library,
                    state_id,
                    resolved,
                    master_family,
                    "Pending asset cleared: provider confirmed source deletion"
                );
            }
            RecordResolution::Unknown => {
                set_verification_for_state_id(
                    db.as_ref(),
                    &pending_targets,
                    state_id.as_str(),
                    AssetVerificationState::Unknown,
                    "provider lookup omitted or could not parse the requested record",
                )
                .await?;
                tracing::warn!(
                    library = %config.library,
                    state_id = state_id.as_str(),
                    "Pending asset retained: provider lookup was inconclusive"
                );
            }
            RecordResolution::TransientFailure(error) => {
                set_verification_for_state_id(
                    db.as_ref(),
                    &pending_targets,
                    state_id.as_str(),
                    AssetVerificationState::TransientFailure,
                    &error.to_string(),
                )
                .await?;
                tracing::warn!(
                    library = %config.library,
                    state_id = state_id.as_str(),
                    error = %error,
                    "Pending asset retained: provider lookup failed transiently"
                );
            }
        }
    }

    if !legacy_present_state_ids.is_empty() && !shutdown_token.is_cancelled() {
        tracing::info!(
            library = %config.library,
            masters = legacy_present_state_ids.len(),
            "Hydrating missing CPLAsset identities for live legacy pending masters"
        );
        let (hydrated, hydration_failed) = match passes.first() {
            Some(pass) => match pass
                .album
                .hydrate_matching_master_assets_from_changes(
                    &legacy_present_state_ids,
                    &shutdown_token,
                    |asset| {
                        pending_evidence.iter().any(|(target, evidence)| {
                            target.asset_id.as_ref() == asset.id()
                                && candidate_matches_durable_evidence(asset, target, evidence)
                        })
                    },
                )
                .await
            {
                Ok(assets) => (assets, false),
                Err(error) => {
                    let reason = error.to_string();
                    for state_id in &legacy_present_state_ids {
                        set_verification_for_state_id(
                            db.as_ref(),
                            &pending_targets,
                            state_id,
                            AssetVerificationState::TransientFailure,
                            &reason,
                        )
                        .await?;
                    }
                    tracing::warn!(
                        library = %config.library,
                        error = %error,
                        "Pending legacy asset hydration failed transiently"
                    );
                    (Vec::new(), true)
                }
            },
            None => (Vec::new(), false),
        };
        let mut candidates_by_master: FxHashMap<String, Vec<PhotoAsset>> = FxHashMap::default();
        for asset in hydrated {
            candidates_by_master
                .entry(asset.id().to_string())
                .or_default()
                .push(asset);
        }

        for state_id in legacy_present_state_ids {
            if shutdown_token.is_cancelled() {
                break;
            }
            if hydration_failed {
                continue;
            }
            let matching_targets: Vec<&PendingRetryTarget> = pending_targets
                .iter()
                .filter(|target| target.asset_id.as_ref() == state_id)
                .collect();
            let candidates = candidates_by_master.remove(&state_id).unwrap_or_default();
            match select_legacy_candidate(candidates, &matching_targets, &pending_evidence) {
                LegacyCandidateSelection::Selected(asset) => {
                    db.upsert_asset_master_mapping(
                        &config.library,
                        asset.asset_record_name(),
                        asset.id(),
                    )
                    .await?;
                    tracing::info!(
                        library = %config.library,
                        state_id,
                        asset_record_name = %asset.asset_record_name(),
                        "Recovered legacy pending asset/master mapping"
                    );
                    let asset = asset.with_state_record_name(Arc::from(state_id.as_str()));
                    plan_resolved_pending_asset(
                        &asset,
                        &state_id,
                        db.as_ref(),
                        &pass_configs,
                        &mut pending_targets,
                        &mut task_planner,
                        &mut tasks,
                        &mut retry_sources,
                    )
                    .await?;
                }
                LegacyCandidateSelection::Missing => {
                    set_verification_for_state_id(
                        db.as_ref(),
                        &pending_targets,
                        &state_id,
                        AssetVerificationState::Unknown,
                        "provider confirmed the master exists but no current CPLAsset pair was found",
                    )
                    .await?;
                    tracing::warn!(
                        library = %config.library,
                        state_id,
                        "Pending asset retained: live master had no current CPLAsset pair"
                    );
                }
                LegacyCandidateSelection::Ambiguous { candidates } => {
                    set_verification_for_state_id(
                        db.as_ref(),
                        &pending_targets,
                        &state_id,
                        AssetVerificationState::Unknown,
                        "multiple provider asset records matched the legacy master",
                    )
                    .await?;
                    tracing::warn!(
                        library = %config.library,
                        state_id,
                        candidates,
                        "Pending asset retained: legacy master resolved to ambiguous CPLAsset siblings"
                    );
                }
            }
        }
    }

    // Explicit deletion tombstones leave catalog history in place but remove
    // those rows from the actionable pending reader. Present rows remain
    // actionable until their retry succeeds.
    let still_pending: FxHashSet<PendingRetryTarget> = db
        .get_pending()
        .await?
        .iter()
        .filter(|record| record.library.as_ref() == config.library.as_ref())
        .map(PendingRetryTarget::from_record)
        .collect();
    pending_targets.retain(|target| still_pending.contains(target));

    if !pending_targets.is_empty() {
        tracing::warn!(
            requested,
            refreshed = tasks.len(),
            missing = pending_targets.len(),
            diagnostic = PENDING_RETRY_UNMATCHED_REASON,
            "Targeted retry could not refresh every pending asset; retaining durable retry work"
        );
    }

    Ok(PendingRetryPlan {
        tasks,
        retry_sources,
        pass_configs,
        unmatched_targets: pending_targets.into_iter().collect(),
        requested,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::test_helpers::TestAssetRecord;

    fn candidate(master: &str, asset: &str, checksum: &str, size: u64) -> PhotoAsset {
        PhotoAsset::new(
            json!({
                "recordName": master,
                "recordType": "CPLMaster",
                "fields": {
                    "filenameEnc": {"value": "legacy.jpg", "type": "STRING"},
                    "itemType": {"value": "public.jpeg"},
                    "resOriginalFileType": {"value": "public.jpeg"},
                    "resOriginalRes": {"value": {
                        "downloadURL": "https://p01.icloud-content.com/legacy.jpg",
                        "fileChecksum": checksum,
                        "size": size,
                    }},
                },
            }),
            json!({
                "recordName": asset,
                "recordType": "CPLAsset",
                "fields": {
                    "masterRef": {"value": {"recordName": master}},
                    "assetDate": {"value": 1700000000000i64},
                },
            }),
        )
    }

    #[test]
    fn legacy_candidate_selection_uses_unique_durable_fingerprint() {
        let record = TestAssetRecord::new("legacy-master")
            .checksum("checksum-b")
            .size(200)
            .build();
        let target = PendingRetryTarget::from_record(&record);
        let evidence =
            FxHashMap::from_iter([(target.clone(), PendingRetryEvidence::from_record(&record))]);
        let selection = select_legacy_candidate(
            vec![
                candidate("legacy-master", "asset-a", "checksum-a", 100),
                candidate("legacy-master", "asset-b", "checksum-b", 200),
            ],
            &[&target],
            &evidence,
        );

        let LegacyCandidateSelection::Selected(selected) = selection else {
            panic!("unique durable fingerprint should select one sibling");
        };
        assert_eq!(selected.asset_record_name(), "asset-b");
    }

    #[test]
    fn legacy_candidate_selection_retains_ambiguous_siblings() {
        let record = TestAssetRecord::new("legacy-master")
            .checksum("missing-checksum")
            .size(300)
            .build();
        let target = PendingRetryTarget::from_record(&record);
        let evidence =
            FxHashMap::from_iter([(target.clone(), PendingRetryEvidence::from_record(&record))]);
        let selection = select_legacy_candidate(
            vec![
                candidate("legacy-master", "asset-a", "checksum-a", 100),
                candidate("legacy-master", "asset-b", "checksum-b", 200),
            ],
            &[&target],
            &evidence,
        );

        assert!(matches!(
            selection,
            LegacyCandidateSelection::Ambiguous { candidates: 2 }
        ));
    }
}
