//! Durable pending-retry identity resolution and targeted provider revalidation.

use std::sync::Arc;

use anyhow::Result;
use rustc_hash::{FxHashMap, FxHashSet};
use tokio_util::sync::CancellationToken;

use super::{
    DownloadConfig, DownloadTask, PENDING_RETRY_UNMATCHED_REASON, RetryTaskKey, UrlRetrySource,
    build_pass_configs_resolving_deferred_excludes, planner,
};
use crate::icloud::photos::{ProviderRecordId, RecordLookupRequest, RecordResolution};
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
    for (state_id, resolution) in resolutions {
        if pending_targets.is_empty() || shutdown_token.is_cancelled() {
            break;
        }
        match resolution {
            RecordResolution::Present(asset) => {
                for target in pending_targets
                    .iter()
                    .filter(|target| target.asset_id.as_ref() == state_id.as_str())
                {
                    db.clear_asset_verification(
                        &target.library,
                        &target.asset_id,
                        target.version_size.as_str(),
                    )
                    .await?;
                }
                for (pass_index, pass_config) in pass_configs.iter().enumerate() {
                    let plan = task_planner.plan_asset(&asset, pass_config).await;
                    if plan.filter_reason.is_some() {
                        continue;
                    }
                    let first_new_task = tasks.len();
                    take_matching_pending_retry_tasks(plan.tasks, &mut pending_targets, &mut tasks);
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
                for target in pending_targets
                    .iter()
                    .filter(|target| target.asset_id.as_ref() == state_id.as_str())
                {
                    db.set_asset_verification(
                        &target.library,
                        &target.asset_id,
                        target.version_size.as_str(),
                        AssetVerificationState::Unknown,
                        "provider lookup omitted or could not parse the requested record",
                    )
                    .await?;
                }
                tracing::warn!(
                    library = %config.library,
                    state_id = state_id.as_str(),
                    "Pending asset retained: provider lookup was inconclusive"
                );
            }
            RecordResolution::TransientFailure(error) => {
                for target in pending_targets
                    .iter()
                    .filter(|target| target.asset_id.as_ref() == state_id.as_str())
                {
                    db.set_asset_verification(
                        &target.library,
                        &target.asset_id,
                        target.version_size.as_str(),
                        AssetVerificationState::TransientFailure,
                        &error.to_string(),
                    )
                    .await?;
                }
                tracing::warn!(
                    library = %config.library,
                    state_id = state_id.as_str(),
                    error = %error,
                    "Pending asset retained: provider lookup failed transiently"
                );
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
