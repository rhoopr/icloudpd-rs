//! Sync loop: the watch-mode cycle that enumerates and downloads photos.
//!
//! Extracted from `main.rs` to keep the entry point focused on CLI dispatch.
//! The public entry point is [`run_sync`], which handles config resolution,
//! authentication, the download loop, and watch-mode re-sync.

use std::sync::Arc;

use anyhow::Context;

use crate::auth;
use crate::cli;
use crate::commands::{
    attempt_reauth, init_photos_service, resolve_cross_zone_libraries_for_album_hydration,
    resolve_libraries, resolve_passes, validate_smart_folder_fulfillability, wait_and_retry_2fa,
    MAX_REAUTH_ATTEMPTS,
};
use crate::config;
use crate::credential;
use crate::download;
use crate::health;
use crate::notifications::{self, Notifier};
use crate::password::{self, ExposeSecret, SecretString};
use crate::retry;
use crate::shutdown;
use crate::state;
use crate::sync_cycle::{run_cycle, sync_token_key as make_sync_token_key, LibraryState};

#[cfg(test)]
use crate::sync_cycle::{
    check_and_persist_enum_config_hash, determine_sync_mode, should_store_sync_token,
    should_store_sync_token_for_cycle, CycleResult, EnumConfigHashOutcome, ENUM_CONFIG_HASH_KEY,
    SYNC_TOKEN_PREFIX,
};

#[cfg(all(test, feature = "xmp"))]
use crate::sync_cycle::preload_asset_groupings;

use crate::systemd::SystemdNotifier;
use crate::{
    available_disk_space, check_min_disk_space, make_password_provider, PartialSyncError,
    PidFileGuard,
};
#[cfg(test)]
use tokio_util::sync::CancellationToken;

#[derive(Debug, PartialEq, Eq)]
enum WatchPrecheck {
    SkipAll,
    Proceed {
        changed_zones: Option<rustc_hash::FxHashSet<String>>,
        db_sync_token_after_success: Option<String>,
    },
}

impl WatchPrecheck {
    fn proceed_all() -> Self {
        Self::Proceed {
            changed_zones: None,
            db_sync_token_after_success: None,
        }
    }

    fn changed_zones(&self) -> Option<&rustc_hash::FxHashSet<String>> {
        match self {
            Self::SkipAll => None,
            Self::Proceed { changed_zones, .. } => changed_zones.as_ref(),
        }
    }

    fn db_sync_token_after_success(&self) -> Option<&str> {
        match self {
            Self::SkipAll => None,
            Self::Proceed {
                db_sync_token_after_success,
                ..
            } => db_sync_token_after_success.as_deref(),
        }
    }

    fn should_sync_zone(&self, zone_name: &str) -> bool {
        match self {
            Self::SkipAll => false,
            Self::Proceed {
                changed_zones: Some(zones),
                ..
            } => zones.contains(zone_name),
            Self::Proceed {
                changed_zones: None,
                ..
            } => true,
        }
    }
}

/// State-DB metadata key for the first-sync shared-library notice. Bumping
/// the version suffix (e.g. `_v2`) re-fires the notice for every existing
/// data dir the next time it's used.
const SHARED_LIBRARY_NOTICE_KEY: &str = "shared_library_notice_shown_v1";
const SHARED_LIBRARY_NOTICE_CHECKED_KEY: &str = "shared_library_notice_checked_at_v1";
const SHARED_LIBRARY_NOTICE_CHECK_TTL_SECS: i64 = 24 * 60 * 60;

/// Metadata key for the database-level token used by `/changes/database`.
const DB_SYNC_TOKEN_KEY: &str = "db_sync_token";

/// Classify whether an error from `init_photos_service` or
/// `resolve_libraries` indicates a stale session / routing state that
/// an SRP re-auth would fix.
///
/// Returns `true` for `ICloudError::SessionExpired` (CloudKit 401/403)
/// and `ICloudError::MisdirectedRequest` (persistent 421 after pool
/// reset) — the two classes that invalidate the cached session and
/// trigger the reauth retry branch. Extracted as a free function so
/// the classification is independently testable without spinning up
/// a full sync cycle.
fn is_session_error(err: &anyhow::Error) -> bool {
    err.downcast_ref::<crate::icloud::error::ICloudError>()
        .is_some_and(crate::icloud::error::ICloudError::is_session_error)
}

/// Whether a CloudKit init/query error should trigger an SRP re-auth retry.
///
/// Returns `true` only on the first session-error encounter. A second
/// session-error bails cleanly instead of looping under Docker's restart
/// policy.
fn should_retry_session_init(err: &anyhow::Error, already_retried: bool) -> bool {
    !already_retried && is_session_error(err)
}

fn take_pending_auth<T>(pending_auth: &mut Option<T>) -> anyhow::Result<T> {
    pending_auth
        .take()
        .ok_or_else(|| anyhow::anyhow!("internal auth retry state missing before attempt"))
}

/// Given the user's library selector, the count of iCloud shared libraries
/// on the account, and whether the notice has already fired, return the
/// warning message to emit, or `None` if no notice is warranted.
///
/// Pure function so the policy is unit-testable without mocking the
/// `PhotosService` or the state DB. The I/O wrapper lives in
/// [`maybe_notify_shared_libraries`].
fn should_notify_shared_libraries(
    selector: &crate::selection::LibrarySelector,
    shared_count: usize,
    already_notified: bool,
) -> Option<String> {
    if already_notified || shared_count == 0 {
        return None;
    }
    // Only users on the `primary`-only default see the notice. Anyone who
    // explicitly picked a different shape (`shared`, `all`, named zones,
    // exclusions) has already made a deliberate choice.
    if selector != &crate::selection::LibrarySelector::default() {
        return None;
    }
    let (word, verb) = if shared_count == 1 {
        ("library", "is")
    } else {
        ("libraries", "are")
    };
    Some(format!(
        "Detected {shared_count} iCloud shared {word} on this account; only the primary \
         library {verb} being synced. To include shared libraries too, set \
         `[filters] libraries = [\"all\"]` in config.toml. \
         Run `kei list libraries` to enumerate every zone."
    ))
}

fn shared_library_notice_recently_checked(checked_at: Option<&str>, now_ts: i64) -> bool {
    let Some(checked_at) = checked_at.and_then(|value| value.parse::<i64>().ok()) else {
        return false;
    };
    now_ts.saturating_sub(checked_at) < SHARED_LIBRARY_NOTICE_CHECK_TTL_SECS
}

/// Probe + warning for users on the `PrimarySync` default who also have
/// shared libraries. The notice marker (stored in the state DB's `metadata`
/// table under [`SHARED_LIBRARY_NOTICE_KEY`]) is set after the notice fires.
/// A separate short-lived negative cache records "no shared libraries seen"
/// so accounts without shared libraries do not pay a shared-zone listing on
/// every one-shot sync. The probe and marker writes are best-effort: failures
/// degrade to `tracing::debug!` and skip without breaking the sync.
async fn maybe_notify_shared_libraries(
    selector: &crate::selection::LibrarySelector,
    photos_service: &mut crate::icloud::photos::PhotosService,
    state_db: Option<&dyn state::StateDb>,
) {
    let Some(db) = state_db else {
        tracing::debug!("shared-library notice: no state DB available; skipping uncached probe");
        return;
    };

    let already_notified = match db.get_metadata(SHARED_LIBRARY_NOTICE_KEY).await {
        Ok(Some(_)) => true,
        Ok(None) => false,
        Err(e) => {
            tracing::debug!(
                error = %e,
                "shared-library notice: metadata read failed; skipping"
            );
            return;
        }
    };
    if already_notified {
        return;
    }

    // Skip the probe when the user explicitly picked a non-default library;
    // they've already opted in or out. `should_notify_shared_libraries`
    // repeats this check defensively.
    if selector != &crate::selection::LibrarySelector::default() {
        return;
    }

    match db.get_metadata(SHARED_LIBRARY_NOTICE_CHECKED_KEY).await {
        Ok(checked_at) => {
            if shared_library_notice_recently_checked(
                checked_at.as_deref(),
                chrono::Utc::now().timestamp(),
            ) {
                tracing::debug!("shared-library notice: recent no-shared check cached");
                return;
            }
        }
        Err(e) => {
            tracing::debug!(
                error = %e,
                "shared-library notice: checked-at read failed; probing"
            );
        }
    }

    let shared_count = match photos_service.fetch_shared_libraries().await {
        Ok(map) => map.len(),
        Err(e) => {
            tracing::debug!(
                error = %e,
                "shared-library notice: enumeration failed; skipping"
            );
            return;
        }
    };

    let Some(msg) = should_notify_shared_libraries(selector, shared_count, already_notified) else {
        if shared_count == 0 {
            if let Err(e) = db
                .set_metadata(
                    SHARED_LIBRARY_NOTICE_CHECKED_KEY,
                    &chrono::Utc::now().timestamp().to_string(),
                )
                .await
            {
                tracing::debug!(
                    error = %e,
                    "shared-library notice: failed to persist no-shared check marker"
                );
            }
        }
        return;
    };
    tracing::warn!(message = %msg, "Shared library notice");

    if let Err(e) = db.set_metadata(SHARED_LIBRARY_NOTICE_KEY, "1").await {
        tracing::debug!(
            error = %e,
            "shared-library notice: failed to persist marker"
        );
    }
}

/// Default watch interval applied when `kei service run` enters with no TOML
/// value set. 24 hours, matching the Docker image's always-on service shape.
pub(crate) const SERVICE_MODE_DEFAULT_WATCH_INTERVAL: u64 = 86400;

/// Decide whether to apply the service-mode watch-interval fallback.
///
/// Returns `Some(interval)` only when `service_mode` is true AND the
/// existing layered resolution (CLI > TOML > env) produced no value,
/// signaling that the daemon would otherwise run once and exit.
pub(crate) fn service_mode_default_interval(
    current: Option<u64>,
    service_mode: bool,
) -> Option<u64> {
    if service_mode && current.is_none() {
        Some(SERVICE_MODE_DEFAULT_WATCH_INTERVAL)
    } else {
        None
    }
}

/// Arguments that [`run_sync`] needs from the CLI dispatch layer.
#[derive(Clone)]
pub(crate) struct SyncArgs {
    pub is_one_shot: bool,
    /// True when invoked via `kei service run`. After [`Config::build`]
    /// resolves CLI > TOML > env, a still-unset watch interval falls
    /// through to [`SERVICE_MODE_DEFAULT_WATCH_INTERVAL`] so the daemon
    /// always polls (single-shot service-mode is meaningless).
    pub service_mode: bool,
    pub pw: cli::PasswordArgs,
    pub sync: cli::SyncArgs,
    pub toml_config: Option<config::TomlConfig>,
    pub config_explicitly_set: bool,
    pub config_path: std::path::PathBuf,
    pub redact_password: Arc<std::sync::Mutex<Option<SecretString>>>,
    /// Resolved friendly UX mode from lib.rs startup. Threaded into Config so
    /// the download pipeline picks the right bar template.
    pub personality_mode: crate::personality::Mode,
    /// User-stated friendly preference (CLI > TOML; `None` means neither was
    /// set, so the default-on-for-TTY policy is active). Threaded so the
    /// resolved Config exposes the same intent the gate saw, useful for
    /// downstream code that wants to know whether the user opted in.
    pub friendly_request: Option<bool>,
}

/// Run the sync command: authenticate, enumerate photos, download, and
/// optionally loop in watch mode.
pub(crate) async fn run_sync(globals: &config::GlobalArgs, args: SyncArgs) -> anyhow::Result<()> {
    let SyncArgs {
        is_one_shot,
        service_mode,
        pw,
        sync,
        toml_config,
        config_explicitly_set,
        config_path,
        redact_password,
        personality_mode,
        friendly_request,
    } = args;

    let is_retry_failed = sync.retry_failed;
    let toml_existed = toml_config.is_some();
    let cli_data_dir = globals.data_dir.clone();
    let mut config = config::Config::build_inner(
        globals,
        &pw,
        sync,
        config::SyncConfigOverrides::default(),
        toml_config.as_ref(),
        personality_mode,
        friendly_request,
    )?;

    // On first run (no config file), persist bootstrap values so subsequent
    // runs don't need the same env again. Only when the user explicitly chose
    // a config path (--config), to avoid surprise writes at the default
    // location during tests or one-off runs.
    if !toml_existed && config_explicitly_set {
        if let Err(e) =
            config::persist_first_run_config(&config_path, &config, cli_data_dir.as_deref())
        {
            tracing::warn!(error = %e, "Failed to save first-run config");
        }
    }

    // One-shot operations — never inherit watch mode from TOML config,
    // which would cause the process to loop forever instead of exiting.
    // retry-failed: one-shot by definition.
    // setup → "sync now": initial test sync, not a daemon.
    if is_one_shot {
        config.watch.interval = None;
    }

    // Service-mode contract: the daemon must poll. If neither CLI nor TOML
    // supplied an interval, fall through to the canonical 24h default so a
    // launchd/systemd/SCM unit still has a heartbeat.
    if let Some(applied) = service_mode_default_interval(config.watch.interval, service_mode) {
        config.watch.interval = Some(applied);
        tracing::info!(
            interval_secs = applied,
            "service mode: applied default watch interval"
        );
    }

    // Install password redaction now that we know the password
    if let Some(pw) = &config.auth.password {
        if let Ok(mut guard) = redact_password.lock() {
            *guard = Some(SecretString::from(pw.expose_secret().to_owned()));
        }
    }

    // Prevent core dumps from leaking in-memory credentials
    crate::harden_process();

    // Write PID file if requested (before auth so the PID is visible immediately)
    let _pid_guard = config
        .watch
        .pid_file
        .as_ref()
        .map(|p| PidFileGuard::new(p.clone()))
        .transpose()?;

    let sd_notifier = SystemdNotifier::new(config.watch.notify_systemd);
    let notifier = Notifier::new(config.notifications.script.clone());

    tracing::info!(concurrency = config.download.threads_num, "Starting kei");

    if config.auth.username.is_empty() {
        anyhow::bail!("username is required (set ICLOUD_USERNAME or [auth].username)");
    }

    // retry-failed + dry-run is unsupported: dry-run skips the state DB,
    // but retry-failed needs it to know which assets failed.
    if is_retry_failed && config.runtime.dry_run {
        anyhow::bail!(
            "--dry-run cannot be used with retry-failed (retry needs the state database)"
        );
    }

    // Validate download directory early (before auth) to avoid wasting a 2FA code
    // when the user simply forgot the destination.
    if config.download.directory.as_os_str().is_empty() {
        anyhow::bail!(
            "[download] directory is required for downloading \
             (set it in the config file)"
        );
    }

    // Validate download directory is writable before spending time on authentication.
    tokio::fs::create_dir_all(&config.download.directory)
        .await
        .with_context(|| {
            format!(
                "Failed to create download directory {}",
                config.download.directory.display()
            )
        })?;
    let probe = config.download.directory.join(".kei_probe");
    tokio::fs::write(&probe, b"").await.with_context(|| {
        format!(
            "Download directory {} is not writable",
            config.download.directory.display()
        )
    })?;
    if let Err(e) = tokio::fs::remove_file(&probe).await {
        tracing::trace!(
            probe = %probe.display(),
            error = %e,
            "Could not clean up writability-probe file; harmless leakage"
        );
    }

    // Abort if available disk space is too low. See `check_min_disk_space`
    // for the pure inner check.
    if let Some(avail) = available_disk_space(&config.download.directory) {
        check_min_disk_space(avail, &config.download.directory)?;
    }

    #[cfg(debug_assertions)]
    if maybe_write_offline_fake_sync_report(&config, &notifier).await? {
        return Ok(());
    }

    let cred_store =
        credential::CredentialStore::new(&config.auth.username, &config.auth.cookie_directory);
    let source = password::build_password_source(
        config.auth.password.as_ref(),
        config.auth.password_command.as_deref(),
        config.auth.password_file.as_deref(),
        cred_store,
    );
    // Snapshot the source kind before moving `source` into the provider
    // closure — used by the --save-password hook after auth succeeds.
    let password_source_kind = source.kind();
    let password_provider = make_password_provider(source);

    let auth_result = match auth::authenticate_with_mode(
        &config.auth.cookie_directory,
        &config.auth.username,
        &password_provider,
        config.auth.domain.as_str(),
        None,
        None,
        None,
        config.ui.personality_mode,
    )
    .await
    {
        Ok(result) => result,
        Err(e)
            if e.downcast_ref::<auth::error::AuthError>()
                .is_some_and(auth::error::AuthError::is_two_factor_required) =>
        {
            let msg = format!(
                "2FA required for {u}. Run: kei login get-code",
                u = config.auth.username
            );
            tracing::warn!(message = %msg, "2FA required");
            notifier.notify(
                notifications::Event::TwoFaRequired,
                &msg,
                &config.auth.username,
                None,
            );

            wait_and_retry_2fa(&config.auth.cookie_directory, &config.auth.username, || {
                auth::authenticate_with_mode(
                    &config.auth.cookie_directory,
                    &config.auth.username,
                    &password_provider,
                    config.auth.domain.as_str(),
                    None,
                    None,
                    None,
                    config.ui.personality_mode,
                )
            })
            .await?
        }
        Err(e) => return Err(e),
    };
    // Post-auth narration. Lands above any future bar; no-op in off mode.
    crate::personality::narration::auth_ok_to_stderr(
        config.ui.personality_mode,
        &config.auth.username,
    );

    // Save password to credential store if requested. Only the ephemeral
    // `Direct` source (CLI flag / env var) persists; File / Command /
    // Store / Interactive each emit a warning explaining why the flag is
    // a no-op for that source.
    if config.auth.save_password {
        match password::decide_save_password_action(password_source_kind) {
            password::SavePasswordAction::Save => {
                if let Some(ref pw) = config.auth.password {
                    let store = credential::CredentialStore::new(
                        &config.auth.username,
                        &config.auth.cookie_directory,
                    );
                    if let Err(e) = store.store(pw.expose_secret()) {
                        tracing::warn!(error = %e, "Failed to save password to credential store");
                    } else {
                        tracing::info!(
                            backend = store.backend_name(),
                            "Password saved to credential store"
                        );
                    }
                }
            }
            password::SavePasswordAction::SkipWithWarning(reason) => {
                tracing::warn!(reason = %reason, "Skipping save of password to credential store");
            }
        }
    }

    let api_retry_config = retry::RetryConfig {
        max_retries: config.retry.max_retries,
        base_delay_secs: config.retry.retry_delay_secs,
        max_delay_secs: 60,
    };
    api_retry_config.validate()?;

    // CloudKit session/routing recovery: if init or the first CloudKit query
    // surfaces a session-error signature (401 stale session, or 421 persisting
    // after a pool reset), strip routing state and force SRP re-auth. A second
    // failure bails cleanly instead of looping under Docker's restart policy.
    let mut pending_auth = Some(auth_result);
    let mut retried_after_session_error = false;
    let (shared_session, mut photos_service, libraries) = loop {
        let this_auth = take_pending_auth(&mut pending_auth)?;
        let init_result =
            init_photos_service(this_auth, api_retry_config, config.ui.personality_mode).await;
        let (ss, mut ps) = match init_result {
            Ok(pair) => pair,
            Err(e) if should_retry_session_init(&e, retried_after_session_error) => {
                tracing::warn!(
                    error = %e,
                    "CloudKit init failed with stale-session signature; forcing SRP re-authentication"
                );
                retried_after_session_error = true;
                pending_auth =
                    Some(reauth_with_srp(&config, &password_provider, &notifier, None).await?);
                continue;
            }
            Err(e) => return Err(e),
        };
        match resolve_libraries(&config.filters.selection.libraries, &mut ps).await {
            Ok(libs) => break (ss, ps, libs),
            Err(e) if should_retry_session_init(&e, retried_after_session_error) => {
                tracing::warn!(
                    error = %e,
                    "CloudKit returned stale-session signature; forcing SRP re-authentication"
                );
                retried_after_session_error = true;
                pending_auth = Some(
                    reauth_with_srp(&config, &password_provider, &notifier, Some((ss, ps))).await?,
                );
            }
            Err(e) => return Err(e),
        }
    };
    tracing::debug!(
        count = libraries.len(),
        zones = %libraries.iter().map(|l| l.zone_name().to_string()).collect::<Vec<_>>().join(", "),
        "Resolved libraries"
    );
    // Post-library-resolve narration. Friendly-mode-only.
    crate::personality::narration::libraries_resolved_to_stderr(
        config.ui.personality_mode,
        libraries.len(),
    );

    // CloudKit shared zones don't expose smart folders. Catch the
    // impossible-config case (e.g. shared libraries plus a smart-folder selector)
    // here, before any per-library work, so the user gets a clear error
    // instead of a silent zero-pass run with exit code 0.
    validate_smart_folder_fulfillability(&libraries, &config.filters.selection)?;

    // Initialize state database.
    // Skip for --dry-run so a preview doesn't create the DB or poison
    // sync tokens, which would cause a subsequent real sync to believe
    // nothing has changed and download 0 photos.
    let state_db: Option<Arc<dyn state::StateDb>> = if config.runtime.dry_run {
        None
    } else {
        let db_path = config.auth.cookie_directory.join(format!(
            "{}.db",
            auth::session::sanitize_username(&config.auth.username)
        ));
        match state::SqliteStateDb::open(&db_path).await {
            Ok(db) => {
                tracing::debug!(path = %db_path.display(), "State database opened");
                let db = Arc::new(db);

                // Promote any sync_runs rows left in status='running' from a
                // prior SIGKILL'd or crashed process. Runs once per process,
                // before any new sync starts.
                match db.promote_orphaned_sync_runs().await {
                    Ok(0) => {}
                    Ok(count) => {
                        tracing::warn!(
                            count,
                            "Promoted orphaned sync_runs rows to 'interrupted' \
                             (prior process exited uncleanly)"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Failed to promote orphaned sync_runs rows");
                    }
                }

                // Surface enum_in_progress:<zone> markers left by a prior
                // interrupted full enumeration so the operator understands
                // why the next full sync will re-enumerate from scratch.
                match db.list_interrupted_enumerations().await {
                    Ok(zones) if !zones.is_empty() => {
                        tracing::warn!(
                            zones = zones.join(","),
                            "Prior full enumeration was interrupted; next sync will re-enumerate \
                             the affected zones from offset 0"
                        );
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::debug!(
                            error = %e,
                            "Failed to list interrupted enumerations"
                        );
                    }
                }

                // For retry-failed, reset failed assets to pending
                if is_retry_failed {
                    match db.reset_failed().await {
                        Ok(0) => {
                            tracing::info!("No failed assets to retry");
                            return Ok(());
                        }
                        Ok(count) => {
                            tracing::debug!(count, "Reset failed assets to pending");
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "Failed to reset failed assets");
                        }
                    }
                }

                Some(db as Arc<dyn state::StateDb>)
            }
            Err(e) => {
                anyhow::bail!("Failed to open state database {}: {e}", db_path.display());
            }
        }
    };

    // First-sync notice: tell users on the `PrimarySync` default about any
    // shared libraries they could be syncing. Runs once per data dir,
    // gated by state DB metadata.
    maybe_notify_shared_libraries(
        &config.filters.selection.libraries,
        &mut photos_service,
        state_db.as_deref(),
    )
    .await;

    // Pre-compute config values used each cycle to build DownloadConfig.
    // DownloadConfig is rebuilt per-cycle so sync_mode can vary.
    let skip_created_before = config
        .filters
        .skip_created_before
        .map(|d| d.with_timezone(&chrono::Utc));
    let skip_created_after = config
        .filters
        .skip_created_after
        .map(|d| d.with_timezone(&chrono::Utc));
    let retry_config = api_retry_config;
    let live_resolution = config.photos.live_resolution.to_asset_version_size();
    // One shared limiter per sync run so the configured cap applies to
    // aggregate throughput across every concurrent download.
    let bandwidth_limiter = config
        .download
        .bandwidth_limit
        .map(download::limiter::BandwidthLimiter::new);
    if let Some(limiter) = &bandwidth_limiter {
        tracing::info!(
            bytes_per_sec = limiter.bytes_per_sec(),
            "Bandwidth limit enabled"
        );
    }
    // Promote the String / Vec / PathBuf config fields to their Arc
    // counterparts once, outside the per-library closure. Otherwise the
    // Arc-sharing win is half-defeated: each build_download_config
    // call would re-allocate directory / filename_exclude / temp_suffix
    // from scratch instead of refcount-bumping.
    let cfg_directory: Arc<std::path::Path> = Arc::from(config.download.directory.as_path());
    let cfg_filename_exclude: Arc<[glob::Pattern]> =
        Arc::from(config.download.filename_exclude.clone());
    let cfg_temp_suffix: Arc<str> = Arc::from(config.download.temp_suffix.as_str());
    let cfg_folder_structure_albums: Arc<str> =
        Arc::from(config.download.folder_structure_albums.as_str());
    let cfg_folder_structure_smart_folders: Arc<str> =
        Arc::from(config.download.folder_structure_smart_folders.as_str());

    let build_download_config = |sync_mode: download::SyncMode,
                                 exclude_asset_ids: Arc<rustc_hash::FxHashSet<String>>,
                                 asset_groupings: Arc<download::AssetGroupings>,
                                 library: Arc<str>|
     -> Arc<download::DownloadConfig> {
        Arc::new(download::DownloadConfig {
            directory: Arc::clone(&cfg_directory),
            folder_structure: config.download.folder_structure.clone(),
            folder_structure_albums: Arc::clone(&cfg_folder_structure_albums),
            folder_structure_smart_folders: Arc::clone(&cfg_folder_structure_smart_folders),
            library,
            resolution: config.photos.resolution,
            media: config.filters.media,
            skip_created_before,
            skip_created_after,
            set_exif_datetime: config.metadata.set_exif_datetime,
            set_exif_rating: config.metadata.set_exif_rating,
            set_exif_gps: config.metadata.set_exif_gps,
            set_exif_description: config.metadata.set_exif_description,
            #[cfg(feature = "xmp")]
            embed_xmp: config.metadata.embed_xmp,
            #[cfg(feature = "xmp")]
            xmp_sidecar: config.metadata.xmp_sidecar,
            concurrent_downloads: config.download.threads_num as usize,
            recent: config.filters.recent,
            retry: retry_config,
            live_photo_mode: config.photos.live_photo_mode,
            live_resolution,
            live_photo_mov_filename_policy: config.photos.live_photo_mov_filename_policy,
            edited: config.photos.edited,
            alternative: config.photos.alternative,
            raw_policy: config.photos.raw_policy,
            file_match_policy: config.photos.file_match_policy,
            force_resolution: config.photos.force_resolution,
            keep_unicode_in_filenames: config.photos.keep_unicode_in_filenames,
            filename_exclude: Arc::clone(&cfg_filename_exclude),
            temp_suffix: Arc::clone(&cfg_temp_suffix),
            state_db: state_db.clone(),
            retry_only: is_retry_failed,
            max_download_attempts: config.retry.max_download_attempts,
            sync_mode,
            album_name: None,
            exclude_asset_ids,
            asset_groupings,
            bandwidth_limiter: bandwidth_limiter.clone(),
        })
    };
    let run_mode = if config.runtime.only_print_filenames {
        download::DownloadRunMode::PrintFilenames
    } else if config.runtime.dry_run {
        download::DownloadRunMode::DryRun
    } else {
        download::DownloadRunMode::Download
    };
    let download_controls = download::DownloadControls::new(
        run_mode,
        download::DownloadReporting::new(
            config.download.no_progress_bar,
            config.ui.personality_mode,
        ),
    );

    let shutdown_token = shutdown::install_signal_handler(sd_notifier, config.ui.personality_mode)?;

    // Suppress the tty driver's `^C` echo for the lifetime of this sync run.
    // Without this, the echoed `^C` overflows the bar's right-edge filler
    // and pushes the cursor down one line, making indicatif's next redraw
    // leave a stale top rule above the live bar. Friendly + tty only;
    // restored on Drop and on the second-Ctrl+C force-exit path. See
    // `personality::tty_echo` for the full context.
    let _echo_guard = if config.ui.personality_mode.is_friendly() {
        crate::personality::tty_echo::EchoGuard::install()
    } else {
        None
    };

    let is_watch_mode = config.watch.interval.is_some();
    let mut reauth_attempts = 0u32;
    // Sum of per-cycle failed_counts across the lifetime of this process.
    // Surfaced at exit so watch-mode daemons don't mask earlier-cycle
    // failures behind a clean final cycle.
    let mut cumulative_failed_count = 0usize;

    let cross_zone_libraries = resolve_cross_zone_libraries_for_album_hydration(
        &config.filters.selection,
        photos_service.all_libraries(),
    )
    .await?;

    let mut library_states: Vec<LibraryState> = Vec::with_capacity(libraries.len());
    for library in &libraries {
        let zone_name = library.zone_name().to_string();
        let sync_token_key = make_sync_token_key(&zone_name);
        let plan =
            resolve_passes(library, &config.filters.selection, &cross_zone_libraries).await?;
        let (album_passes, smart_folder_passes, unfiled) = count_passes(&plan);
        tracing::info!(
            zone = %zone_name,
            album_passes,
            smart_folder_passes,
            unfiled,
            "Sync plan for library"
        );
        library_states.push(LibraryState {
            library: library.clone(),
            cross_zone_libraries: cross_zone_libraries.clone(),
            zone_name,
            sync_token_key,
            plan,
            plan_is_stale: false,
            plan_needs_refresh: false,
        });
    }
    warn_if_multi_library_paths_commingle(
        library_states.len(),
        &config.download.folder_structure,
        &config.download.folder_structure_albums,
        &config.download.folder_structure_smart_folders,
        &config.filters.selection,
    );
    sd_notifier.notify_ready();
    // Friendly-mode greeting. Lands above any future bar via
    // active_bar::with_suspended; no-op in off mode. Once per process.
    crate::personality::narration::greet_to_stderr(config.ui.personality_mode, is_watch_mode);

    // Spawn the HTTP server (/healthz + /metrics) only in watch mode.
    // A one-shot sync exits before anything could scrape /healthz, so there
    // is no value in binding the port. In watch mode, flag /healthz as stale
    // after two missed intervals so a single slow cycle doesn't flip to 503
    // but a stuck main loop does.
    // Binds synchronously so a misconfigured port fails at startup.
    let staleness_threshold = config
        .watch
        .interval
        .map(|secs| chrono::Duration::seconds((secs * 2) as i64));
    let (metrics_handle, metrics_task) = if config.watch.interval.is_some() {
        let (h, t, _addr) = crate::metrics::spawn_server(
            config.server.bind,
            config.server.port,
            shutdown_token.clone(),
            staleness_threshold,
        )?;
        (Some(h), Some(t))
    } else {
        (None, None)
    };

    let mut health = health::HealthStatus::new();
    let cycle_reporter =
        crate::cycle_reporter::CycleReporter::new(crate::cycle_reporter::CycleReporterConfig {
            username: &config.auth.username,
            watch_mode: is_watch_mode,
            report_path: config.report.json.as_deref(),
            run_options: crate::report::RunOptions::from_config(&config),
            health_dir: &config.auth.cookie_directory,
            personality_mode: config.ui.personality_mode,
            state_db: state_db.as_deref(),
            metrics_handle: metrics_handle.as_ref(),
            notifier: &notifier,
        });
    let mut consecutive_album_refresh_failures = 0u32;
    // 1-based cycle counter for periodic-reconcile cadence. Logged at
    // cycle start so an operator chasing missed reconciliation runs has a
    // breadcrumb. Cycle 1 is the first iteration of this loop, cycle 2 is the
    // first re-entry under `--watch`, etc.
    let mut cycle_index: u64 = 0;
    if is_watch_mode {
        match config.watch.reconcile_every_n_cycles {
            Some(n) if n > 0 => tracing::info!(
                every_n_cycles = n,
                "Periodic local-vs-state reconciliation enabled"
            ),
            _ => tracing::debug!(
                "Periodic local-vs-state reconciliation disabled \
                 (set [watch] reconcile_every_n_cycles in TOML to enable)"
            ),
        }
    }

    loop {
        if shutdown_token.is_cancelled() {
            tracing::info!("Shutdown requested, exiting...");
            break;
        }
        cycle_index = cycle_index.saturating_add(1);

        // In watch mode with incremental sync, use changes/database as a
        // cheap pre-check before refreshing album plans or running a sync.
        // No-change cycles should cost one CloudKit request, not a full
        // album/pass refresh per selected library.
        let watch_precheck = if is_watch_mode {
            check_changes_database(state_db.as_deref(), &library_states, &mut photos_service).await
        } else {
            WatchPrecheck::proceed_all()
        };

        if matches!(watch_precheck, WatchPrecheck::SkipAll) {
            cycle_reporter.report_skipped_watch_cycle(&mut health).await;
        } else {
            refresh_needed_library_plans(
                &mut library_states,
                &config.filters.selection,
                watch_precheck.changed_zones(),
                &mut consecutive_album_refresh_failures,
            )
            .await;
            let cycle_library_states: Vec<&LibraryState> = library_states
                .iter()
                .filter(|s| watch_precheck.should_sync_zone(&s.zone_name))
                .collect();
            debug_assert!(!cycle_library_states.is_empty());

            sd_notifier.notify_status("Syncing...");
            sd_notifier.notify_watchdog();
            notifier.notify(
                notifications::Event::SyncStarted,
                "Sync cycle starting",
                &config.auth.username,
                None,
            );

            let cycle_started_at = std::time::Instant::now();
            let cycle_result = run_cycle(
                &cycle_library_states,
                &config,
                state_db.as_deref(),
                is_retry_failed,
                &build_download_config,
                download_controls,
                &shared_session,
                &shutdown_token,
            )
            .await?;

            if let Some(token) = watch_precheck.db_sync_token_after_success() {
                if !cycle_result.session_expired
                    && cycle_result.failed_count == 0
                    && !cycle_result.stats.interrupted
                    && cycle_result.db_sync_token_advance_safe
                {
                    if let Some(db) = state_db.as_deref() {
                        store_db_sync_token(db, token).await;
                    }
                } else {
                    tracing::debug!(
                        "changes/database token not advanced because the sync cycle did not complete cleanly"
                    );
                }
            }

            cycle_reporter
                .report_completed_cycle(
                    &mut health,
                    crate::cycle_reporter::CycleReportInput {
                        stats: &cycle_result.stats,
                        failed_count: cycle_result.failed_count,
                        session_expired: cycle_result.session_expired,
                        elapsed: cycle_started_at.elapsed(),
                    },
                )
                .await;

            // Handle aggregate outcome across all libraries
            if cycle_result.session_expired {
                reauth_attempts += 1;
                if reauth_attempts >= MAX_REAUTH_ATTEMPTS {
                    anyhow::bail!(
                        "Session expired, giving up after {MAX_REAUTH_ATTEMPTS} re-auth attempts"
                    );
                }
                tracing::warn!(
                    reauth_attempts,
                    max_attempts = MAX_REAUTH_ATTEMPTS,
                    "Session expired, attempting re-auth"
                );
                match attempt_reauth(
                    &shared_session,
                    &config.auth.cookie_directory,
                    &config.auth.username,
                    config.auth.domain.as_str(),
                    &password_provider,
                )
                .await
                {
                    Ok(()) => {
                        tracing::info!("Re-auth successful, resuming download...");
                        continue; // Restart entire cycle
                    }
                    Err(e)
                        if e.downcast_ref::<auth::error::AuthError>()
                            .is_some_and(auth::error::AuthError::is_two_factor_required) =>
                    {
                        // 2FA is user action, not a failed attempt -- don't
                        // burn reauth_attempts so false wakeups from get-code
                        // can't exhaust the limit.
                        reauth_attempts -= 1;

                        let msg = format!(
                            "2FA required for {u}. Run: kei login get-code",
                            u = config.auth.username
                        );
                        tracing::warn!(message = %msg, "2FA required");
                        notifier.notify(
                            notifications::Event::TwoFaRequired,
                            &msg,
                            &config.auth.username,
                            None,
                        );
                        if !should_wait_for_2fa(is_watch_mode, &e) {
                            return Err(e);
                        }

                        wait_and_retry_2fa(
                            &config.auth.cookie_directory,
                            &config.auth.username,
                            || {
                                attempt_reauth(
                                    &shared_session,
                                    &config.auth.cookie_directory,
                                    &config.auth.username,
                                    config.auth.domain.as_str(),
                                    &password_provider,
                                )
                            },
                        )
                        .await?;
                        continue;
                    }
                    Err(e) => {
                        notifier.notify(
                            notifications::Event::SessionExpired,
                            &format!("Re-authentication failed: {e}"),
                            &config.auth.username,
                            None,
                        );
                        return Err(e);
                    }
                }
            } else if cycle_result.failed_count > 0 {
                cumulative_failed_count =
                    cumulative_failed_count.saturating_add(cycle_result.failed_count);
                if is_watch_mode {
                    tracing::warn!(
                        failed_count = cycle_result.failed_count,
                        cumulative = cumulative_failed_count,
                        "Some downloads failed this cycle, will retry next cycle"
                    );
                } else {
                    return Err(PartialSyncError(cycle_result.failed_count).into());
                }
            } else {
                reauth_attempts = 0;
            }
        }

        // Periodic local-vs-state reconciliation. Read-only walk that
        // surfaces missing files via `tracing::warn!`. State rows are NEVER
        // mutated here -- the manual `kei reconcile` subcommand still owns
        // the failed-status transition. Long-running daemons drift between
        // assets.local_path and what's on disk (manual rm, mount glitches,
        // etc.); a periodic visible signal beats waiting for the next sync
        // to stumble over the missing files.
        if is_watch_mode
            && should_reconcile_this_cycle(cycle_index, config.watch.reconcile_every_n_cycles)
        {
            if let Some(db) = state_db.as_ref() {
                run_periodic_reconcile(db.as_ref(), cycle_index).await;
            }
        }

        if let Some(interval) = config.watch.interval {
            if shutdown_token.is_cancelled() {
                tracing::info!("Shutdown requested, exiting...");
                break;
            }

            // Release the file lock during idle sleep so that docker exec
            // commands (login get-code, login submit-code) can acquire it.
            {
                let session = shared_session.read().await;
                if let Err(e) = session.release_lock() {
                    tracing::warn!(error = %e, "Failed to release lock before idle sleep");
                }
            }

            sd_notifier.notify_status(&format!("Waiting {interval} seconds..."));
            tracing::info!(interval_secs = interval, "Waiting before next cycle");
            // `interval` is u64 seconds; chrono Add panics on overflow.
            // Skip the heartbeat for the (impossible-in-practice) case where
            // the interval doesn't fit in a wall-clock instant.
            if let Some(wake_at) = i64::try_from(interval)
                .ok()
                .and_then(chrono::Duration::try_seconds)
                .and_then(|d| chrono::Local::now().checked_add_signed(d))
            {
                crate::personality::narration::sleeping_until_to_stderr(
                    config.ui.personality_mode,
                    wake_at,
                );
            }
            tokio::select! {
                () = tokio::time::sleep(std::time::Duration::from_secs(interval)) => {}
                () = shutdown_token.cancelled() => {
                    tracing::info!("Shutdown during wait, exiting...");
                    break;
                }
            }

            // Validate session before next cycle; re-authenticate if expired.
            reacquire_session(&shared_session, &config, &password_provider).await;

            // Mark album/pass plans stale after an idle sleep, but defer the
            // CloudKit refresh until `changes/database` says a selected
            // library actually has work. Quiet watch cycles can then go back
            // to sleep without listing albums for every selected library.
            for lib_state in &mut library_states {
                lib_state.plan_needs_refresh = true;
            }
        } else {
            break;
        }
    }

    // Friendly farewell line. Only on graceful Ctrl+C-driven exit: if the
    // loop fell through without `shutdown_token` ever being cancelled (e.g.
    // a one-shot run completing normally), the per-cycle signoff already
    // covered the closing sentiment and a second "Done." line would be noise.
    if shutdown_token.is_cancelled() {
        crate::personality::narration::farewell_to_stderr(config.ui.personality_mode);
    }

    // Signal the metrics server to shut down (idempotent if SIGINT already
    // fired) and await its graceful drain so the binary doesn't exit while
    // an in-flight /metrics scrape is still flushing.
    if let Some(task) = metrics_task {
        shutdown_token.cancel();
        if let Err(e) = task.await {
            tracing::warn!(error = %e, "metrics server task panicked");
        }
    }

    // Exit non-zero if any cycle in this watch session had failures, not
    // just the last one. A single successful final cycle must not mask a
    // multi-cycle failure backlog in Docker / systemd exit-code signalling.
    if cumulative_failed_count > 0 {
        Err(PartialSyncError(cumulative_failed_count).into())
    } else {
        Ok(())
    }
}

// Debug-only seam for the offline binary-boundary report test. Release builds
// must never skip authentication or CloudKit work from an environment variable.
#[cfg(debug_assertions)]
async fn maybe_write_offline_fake_sync_report(
    config: &config::Config,
    notifier: &Notifier,
) -> anyhow::Result<bool> {
    const ENV: &str = "KEI_UNSTABLE_FAKE_SYNC_REPORT_FOR_TESTS";
    if std::env::var(ENV).as_deref() != Ok("1") {
        return Ok(false);
    }

    let Some(report_path) = config.report.json.as_deref() else {
        anyhow::bail!("{ENV}=1 requires [report].json");
    };

    tracing::warn!(
        env = ENV,
        "Offline fake sync report test seam enabled; exiting before authentication"
    );

    let reporter = crate::cycle_reporter::CycleReporter::<state::SqliteStateDb>::new(
        crate::cycle_reporter::CycleReporterConfig {
            username: &config.auth.username,
            watch_mode: config.watch.interval.is_some(),
            report_path: Some(report_path),
            run_options: crate::report::RunOptions::from_config(config),
            health_dir: &config.auth.cookie_directory,
            personality_mode: config.ui.personality_mode,
            state_db: None,
            metrics_handle: None,
            notifier,
        },
    );
    let stats = download::SyncStats {
        assets_seen: 3,
        downloaded: 2,
        skipped: download::SkipBreakdown {
            by_state: 1,
            ..download::SkipBreakdown::default()
        },
        bytes_downloaded: 4096,
        disk_bytes_written: 4096,
        elapsed_secs: 0.125,
        photos_downloaded: 1,
        videos_downloaded: 1,
        ..download::SyncStats::default()
    };
    let mut health = health::HealthStatus::new();
    reporter
        .report_completed_cycle(
            &mut health,
            crate::cycle_reporter::CycleReportInput {
                stats: &stats,
                failed_count: 0,
                session_expired: false,
                elapsed: std::time::Duration::from_millis(125),
            },
        )
        .await;

    if !report_path.is_file() {
        anyhow::bail!("offline fake sync did not write {}", report_path.display());
    }

    Ok(true)
}

/// Re-authenticate via SRP after a session-error signature from CloudKit.
///
/// Drops any live session + service (releasing the file lock), strips routing
/// state from the session file so `auth::authenticate` is forced onto SRP,
/// then runs authentication — handling the 2FA-required case by notifying and
/// waiting for `kei login submit-code`.
async fn reauth_with_srp(
    config: &config::Config,
    password_provider: &crate::password::PasswordProvider,
    notifier: &Notifier,
    live: Option<(auth::SharedSession, crate::icloud::photos::PhotosService)>,
) -> anyhow::Result<auth::AuthResult> {
    if let Some((ss, ps)) = live {
        ss.read().await.release_lock()?;
        drop(ps);
        drop(ss);
    }
    let session_file =
        auth::session_file_path(&config.auth.cookie_directory, &config.auth.username);
    auth::strip_session_routing_state(&session_file).await;

    match auth::authenticate_with_mode(
        &config.auth.cookie_directory,
        &config.auth.username,
        password_provider,
        config.auth.domain.as_str(),
        None,
        None,
        None,
        config.ui.personality_mode,
    )
    .await
    {
        Ok(result) => Ok(result),
        Err(e)
            if e.downcast_ref::<auth::error::AuthError>()
                .is_some_and(auth::error::AuthError::is_two_factor_required) =>
        {
            let msg = format!(
                "2FA required for {u}. Run: kei login get-code",
                u = config.auth.username
            );
            tracing::warn!(message = %msg, "2FA required");
            notifier.notify(
                notifications::Event::TwoFaRequired,
                &msg,
                &config.auth.username,
                None,
            );
            wait_and_retry_2fa(&config.auth.cookie_directory, &config.auth.username, || {
                auth::authenticate_with_mode(
                    &config.auth.cookie_directory,
                    &config.auth.username,
                    password_provider,
                    config.auth.domain.as_str(),
                    None,
                    None,
                    None,
                    config.ui.personality_mode,
                )
            })
            .await
        }
        Err(e) => Err(e),
    }
}

/// Walk every `downloaded` row in the state DB and warn when the
/// recorded `local_path` is missing on disk. Read-only — no rows are mutated
/// (the manual `kei reconcile` CLI still owns the `downloaded -> failed`
/// transition). Triggered on a fixed cadence by the watch loop; surfaces
/// long-running drift that the next sync would otherwise re-discover only
/// after stumbling over the missing files.
///
/// Errors from the DB scan are logged at `warn!` rather than propagated:
/// the periodic walk is a diagnostic, not a load-bearing correctness gate,
/// and a transient SQLite hiccup must not crash the watch daemon.
async fn run_periodic_reconcile(db: &dyn state::StateDb, cycle_index: u64) {
    use crate::commands::reconcile::{scan_missing, MissingAsset};
    tracing::info!(
        cycle_index,
        "Periodic reconciliation: scanning state DB for missing local files"
    );
    let mut sample_logged = 0usize;
    const SAMPLE_LOG_CAP: usize = 25;
    // Cap per-cycle log spam at SAMPLE_LOG_CAP missing entries; the
    // aggregate count is logged below regardless of how many fired.
    let report_missing = |m: &MissingAsset| {
        if sample_logged < SAMPLE_LOG_CAP {
            tracing::warn!(
                asset_id = %m.id,
                version_size = m.version_size.as_str(),
                path = %m.local_path.display(),
                "Reconcile: state row marks asset downloaded but local file is missing"
            );
            sample_logged += 1;
        }
    };
    let report_no_path = |id: &str| {
        tracing::debug!(asset_id = %id, "Reconcile: downloaded row has no local_path recorded");
    };
    let scan = scan_missing(db, report_missing, report_no_path).await;
    match scan {
        Ok((counts, missing)) => {
            if missing.is_empty() && counts.no_path == 0 {
                tracing::info!(
                    present = counts.present,
                    "Periodic reconciliation: all downloaded files present on disk"
                );
            } else {
                tracing::warn!(
                    present = counts.present,
                    missing = counts.missing,
                    no_path = counts.no_path,
                    sample_logged,
                    "Periodic reconciliation: drift detected; run `kei reconcile` to mark missing files for re-download"
                );
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "Periodic reconciliation scan failed; will retry on next interval");
        }
    }
}

/// Should this watch cycle run a periodic local-vs-state reconciliation?
///
/// Returns `true` for the very first cycle whose 1-based index is a multiple
/// of `every_n` (e.g. `every_n = 24` fires on cycle 24, 48, ...). The first
/// firing is at cycle `every_n` rather than cycle 0 so a freshly-started
/// daemon doesn't burn its startup time walking the disk before a single
/// sync has run. Disabled (`None`) or `Some(0)` always returns `false`; the
/// `cycle_index` is also 1-based so the first cycle is `1`.
///
/// Pure function so the cadence is unit-testable without spinning up a real
/// watch loop or filesystem walk.
pub(crate) fn should_reconcile_this_cycle(cycle_index: u64, every_n: Option<u64>) -> bool {
    let n = match every_n {
        Some(n) if n > 0 => n,
        _ => return false,
    };
    cycle_index > 0 && cycle_index.is_multiple_of(n)
}

/// Decide whether the reauth path inside the sync loop should block on a
/// 2FA prompt or surface the error to the caller.
///
/// In **watch mode** a 2FA-required error is recoverable: the loop notifies
/// the user, parks `wait_and_retry_2fa`, and resumes once a code arrives.
///
/// In **one-shot mode** there is nothing to wait on -- the caller (a CI run,
/// a cron, the systemd unit's first start) needs the error so it can exit
/// non-zero and the operator can run `kei login get-code`.
///
/// Note that the **entry-point** auth path (`run_sync`'s initial
/// `auth::authenticate` call) intentionally does NOT use this predicate --
/// it always parks on `wait_and_retry_2fa` because the user is presumed
/// present at a terminal during the initial command. This helper exists
/// because the *reauth* branch fires mid-cycle, by which point a one-shot
/// caller has long since detached and there is no operator to type a code.
pub(crate) fn should_wait_for_2fa(is_watch_mode: bool, err: &anyhow::Error) -> bool {
    is_watch_mode
        && err
            .downcast_ref::<auth::error::AuthError>()
            .is_some_and(auth::error::AuthError::is_two_factor_required)
}

async fn refresh_needed_library_plans(
    library_states: &mut [LibraryState],
    selection: &crate::selection::Selection,
    changed_zones: Option<&rustc_hash::FxHashSet<String>>,
    consecutive_album_refresh_failures: &mut u32,
) {
    for lib_state in library_states {
        if !lib_state.plan_needs_refresh {
            continue;
        }
        if changed_zones.is_some_and(|zones| !zones.contains(&lib_state.zone_name)) {
            continue;
        }

        // Re-resolve albums per-library to discover newly created iCloud albums.
        // Full sync resolves unfiled album-member exclusions in the download
        // phase; incremental/cleanup paths resolve them before planning tasks.
        // This refresh is intentionally delayed until a selected zone has
        // changes so quiet watch cycles avoid the album-listing traffic.
        match resolve_passes(
            &lib_state.library,
            selection,
            &lib_state.cross_zone_libraries,
        )
        .await
        {
            Ok(refreshed) => {
                lib_state.plan = refreshed;
                lib_state.plan_is_stale = false;
                lib_state.plan_needs_refresh = false;
                *consecutive_album_refresh_failures = 0;
            }
            Err(e) => {
                *consecutive_album_refresh_failures += 1;
                lib_state.plan_is_stale = true;
                lib_state.plan_needs_refresh = true;
                if *consecutive_album_refresh_failures >= 3 {
                    tracing::error!(
                        zone = %lib_state.zone_name,
                        error = %e,
                        consecutive_failures = *consecutive_album_refresh_failures,
                        "Repeated album refresh failures, reusing previous set"
                    );
                } else {
                    tracing::warn!(
                        zone = %lib_state.zone_name,
                        error = %e,
                        "Failed to refresh albums, reusing previous set"
                    );
                }
            }
        }
    }
}

async fn store_db_sync_token(db: &dyn state::StateDb, token: &str) {
    if let Err(e) = db.set_metadata(DB_SYNC_TOKEN_KEY, token).await {
        tracing::warn!(error = %e, "Failed to store db_sync_token");
    }
}

/// Check `changes/database` to determine if this watch cycle can be skipped.
///
/// Returns `SkipAll` when no selected zones report changes and `moreComing` is false.
async fn check_changes_database(
    state_db: Option<&dyn state::StateDb>,
    library_states: &[LibraryState],
    photos_service: &mut crate::icloud::photos::PhotosService,
) -> WatchPrecheck {
    let Some(db) = state_db else {
        return WatchPrecheck::proceed_all();
    };
    if library_states.is_empty() {
        return WatchPrecheck::SkipAll;
    }
    for lib_state in library_states {
        let has_token = match db.get_metadata(&lib_state.sync_token_key).await {
            Ok(token) => token.is_some_and(|t| !t.is_empty()),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    zone = %lib_state.zone_name,
                    metadata_key = %lib_state.sync_token_key,
                    "Failed to read zone sync token; proceeding with sync"
                );
                return WatchPrecheck::proceed_all();
            }
        };
        if !has_token {
            return WatchPrecheck::proceed_all();
        }
    }
    let db_token = match db.get_metadata(DB_SYNC_TOKEN_KEY).await {
        Ok(token) => token.filter(|t| !t.is_empty()),
        Err(e) => {
            tracing::warn!(
                error = %e,
                metadata_key = DB_SYNC_TOKEN_KEY,
                "Failed to read changes/database sync token; proceeding with sync"
            );
            return WatchPrecheck::proceed_all();
        }
    };
    match photos_service.changes_database(db_token.as_deref()).await {
        Ok(db_resp) => {
            let selected_zones: rustc_hash::FxHashSet<&str> = library_states
                .iter()
                .map(|s| s.zone_name.as_str())
                .collect();
            let mut changed_selected_zones = rustc_hash::FxHashSet::default();
            if db_resp.more_coming {
                tracing::debug!("changes/database has more pages (moreComing=true)");
            }
            for z in &db_resp.zones {
                tracing::debug!(
                    zone = %z.zone_id.zone_name,
                    zone_sync_token = %z.sync_token,
                    "changes/database: zone has changes"
                );
                if selected_zones.contains(z.zone_id.zone_name.as_str()) {
                    changed_selected_zones.insert(z.zone_id.zone_name.clone());
                }
            }

            if changed_selected_zones.is_empty() {
                if db_resp.more_coming {
                    return WatchPrecheck::Proceed {
                        changed_zones: None,
                        db_sync_token_after_success: Some(db_resp.sync_token),
                    };
                }
                store_db_sync_token(db, &db_resp.sync_token).await;
                tracing::info!(
                    "No selected library changes detected (changes/database), skipping cycle"
                );
                return WatchPrecheck::SkipAll;
            }

            WatchPrecheck::Proceed {
                changed_zones: if db_resp.more_coming {
                    None
                } else {
                    Some(changed_selected_zones)
                },
                db_sync_token_after_success: Some(db_resp.sync_token),
            }
        }
        Err(e) => {
            tracing::debug!(
                error = %e,
                "changes/database pre-check failed, proceeding with sync"
            );
            WatchPrecheck::proceed_all()
        }
    }
}

/// Identify active-pass templates that lack `{library}` when multiple
/// libraries are selected. Returns a sorted list of CLI flag names whose
/// templates would let same-named assets from different zones land in the
/// same on-disk path. Empty list means the multi-library plan is unambiguous.
///
/// The check is per-pass-kind: each *active* template (one whose pass kind
/// will actually run under the current Selection) must contain `{library}`.
pub(crate) fn count_passes(plan: &crate::commands::AlbumPlan) -> (usize, usize, bool) {
    use crate::commands::PassKind;
    let mut album = 0;
    let mut smart_folder = 0;
    let mut unfiled = false;
    for pass in &plan.passes {
        match pass.kind {
            PassKind::Album => album += 1,
            PassKind::SmartFolder => smart_folder += 1,
            PassKind::Unfiled => unfiled = true,
        }
    }
    (album, smart_folder, unfiled)
}

/// Template strings whose pass is disabled (e.g. `folder_structure_smart_folders`
/// when `--smart-folder none`) don't render any path so they don't need
/// `{library}` to keep the sync safe.
fn find_multi_library_commingle_flags(
    library_count: usize,
    folder_structure: &str,
    folder_structure_albums: &str,
    folder_structure_smart_folders: &str,
    selection: &crate::selection::Selection,
) -> Vec<&'static str> {
    use crate::selection::{AlbumSelector, SmartFolderSelector};

    if library_count < 2 {
        return Vec::new();
    }

    let unfiled_active = selection.unfiled;
    let album_active = !matches!(selection.albums, AlbumSelector::None);
    let smart_folder_active = !matches!(selection.smart_folders, SmartFolderSelector::None);

    // All passes disabled — resolve_passes returns an empty plan, no path
    // ever renders, multi-library can't commingle.
    if !unfiled_active && !album_active && !smart_folder_active {
        return Vec::new();
    }

    let mut missing: Vec<&'static str> = Vec::new();
    if unfiled_active && !folder_structure.contains("{library}") {
        missing.push("--folder-structure");
    }
    if album_active && !folder_structure_albums.contains("{library}") {
        missing.push("--folder-structure-albums");
    }
    if smart_folder_active && !folder_structure_smart_folders.contains("{library}") {
        missing.push("--folder-structure-smart-folders");
    }
    missing
}

/// Emit a startup warning when multi-library paths commingle. Informational:
/// the run continues, and `file_match_policy` (default
/// `name-size-dedup-with-suffix`) keeps two libraries from silently
/// overwriting each other -- collisions land at `<name>-1.<ext>` rather
/// than overwriting. The warning surfaces the namespace ambiguity so the
/// user can add `{library}` to their templates if they want zone-disjoint
/// trees.
fn warn_if_multi_library_paths_commingle(
    library_count: usize,
    folder_structure: &str,
    folder_structure_albums: &str,
    folder_structure_smart_folders: &str,
    selection: &crate::selection::Selection,
) {
    let missing = find_multi_library_commingle_flags(
        library_count,
        folder_structure,
        folder_structure_albums,
        folder_structure_smart_folders,
        selection,
    );
    if missing.is_empty() {
        return;
    }
    tracing::warn!(
        library_count,
        missing = ?missing,
        "Multi-library sync: active template(s) lack `{{library}}`; same-named \
         assets from different zones will share an on-disk namespace. \
         File-match policy keeps writes from overwriting (collisions get a \
         `-N` suffix), but cross-library files end up interleaved. Add \
         `{{library}}` to each listed template for zone-disjoint trees."
    );
}

/// Re-validate the session after an idle sleep and re-acquire the lock.
async fn reacquire_session(
    shared_session: &auth::SharedSession,
    config: &config::Config,
    password_provider: &crate::password::PasswordProvider,
) {
    match attempt_reauth(
        shared_session,
        &config.auth.cookie_directory,
        &config.auth.username,
        config.auth.domain.as_str(),
        password_provider,
    )
    .await
    {
        Ok(()) => {
            // Re-acquire the lock. If attempt_reauth performed a full
            // re-auth, the new Session already holds its own lock, so
            // LockContention here is expected and harmless.
            let session = shared_session.read().await;
            if let Err(e) = session.reacquire_lock() {
                if e.downcast_ref::<auth::error::AuthError>()
                    .is_some_and(auth::error::AuthError::is_lock_contention)
                {
                    tracing::debug!("Lock held by new session after reauth");
                } else {
                    tracing::warn!(error = %e, "Failed to reacquire lock after idle");
                }
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "Pre-cycle reauth failed, will retry mid-sync");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_mode_default_locked_at_one_day() {
        // The roadmap promises the kei daemon polls every 24h out of the
        // box. Regression-guard the constant so a casual rename doesn't
        // silently shorten the interval and 10x the API call rate.
        assert_eq!(SERVICE_MODE_DEFAULT_WATCH_INTERVAL, 86_400);
    }

    #[test]
    fn service_mode_default_applied_when_no_other_source_set_interval() {
        assert_eq!(
            service_mode_default_interval(None, true),
            Some(SERVICE_MODE_DEFAULT_WATCH_INTERVAL)
        );
    }

    #[test]
    fn service_mode_default_skipped_when_cli_or_toml_already_set_interval() {
        // A user-provided interval (CLI / TOML / env) must always win;
        // service mode never silently overrides an explicit choice.
        assert_eq!(service_mode_default_interval(Some(60), true), None);
        assert_eq!(service_mode_default_interval(Some(3600), true), None);
    }

    #[test]
    fn service_mode_default_skipped_outside_service_mode() {
        // Plain `kei sync` must remain single-shot when no interval is
        // configured. Only the service entry point applies the fallback.
        assert_eq!(service_mode_default_interval(None, false), None);
    }

    #[test]
    fn watch_precheck_skip_all_blocks_every_zone_and_token() {
        let precheck = WatchPrecheck::SkipAll;

        assert!(precheck.changed_zones().is_none());
        assert!(precheck.db_sync_token_after_success().is_none());
        assert!(!precheck.should_sync_zone("PrimarySync"));
        assert!(!precheck.should_sync_zone("SharedSync-123"));
    }

    #[test]
    fn watch_precheck_proceed_all_allows_every_zone_without_db_token() {
        let precheck = WatchPrecheck::proceed_all();

        assert!(precheck.changed_zones().is_none());
        assert!(precheck.db_sync_token_after_success().is_none());
        assert!(precheck.should_sync_zone("PrimarySync"));
        assert!(precheck.should_sync_zone("SharedSync-123"));
    }

    #[test]
    fn watch_precheck_changed_zones_scopes_sync_and_carries_db_token() {
        let mut zones = rustc_hash::FxHashSet::default();
        zones.insert("PrimarySync".to_string());
        let precheck = WatchPrecheck::Proceed {
            changed_zones: Some(zones),
            db_sync_token_after_success: Some("db-token-after-cycle".to_string()),
        };

        assert_eq!(
            precheck.db_sync_token_after_success(),
            Some("db-token-after-cycle")
        );
        assert_eq!(
            precheck
                .changed_zones()
                .expect("changed zone filter should be present")
                .len(),
            1
        );
        assert!(precheck.should_sync_zone("PrimarySync"));
        assert!(!precheck.should_sync_zone("SharedSync-123"));
    }

    fn primary() -> crate::selection::LibrarySelector {
        crate::selection::LibrarySelector::default()
    }

    fn all_libraries() -> crate::selection::LibrarySelector {
        crate::selection::parse_library_selector(&["all".to_string()]).unwrap()
    }

    fn shared_zone() -> crate::selection::LibrarySelector {
        crate::selection::parse_library_selector(&["SharedSync-ABCD1234".to_string()]).unwrap()
    }

    #[test]
    fn notice_suppressed_when_already_shown() {
        // The marker overrides everything: even a user with 5 shared libraries
        // and the default selection gets no notice on re-entry.
        assert!(should_notify_shared_libraries(&primary(), 5, true).is_none());
    }

    #[test]
    fn notice_suppressed_when_no_shared_libraries() {
        assert!(should_notify_shared_libraries(&primary(), 0, false).is_none());
    }

    #[test]
    fn notice_suppressed_when_user_picked_all() {
        // Anyone who explicitly set all libraries has already opted in;
        // nothing to tell them.
        assert!(should_notify_shared_libraries(&all_libraries(), 3, false).is_none());
    }

    #[test]
    fn notice_suppressed_when_user_picked_shared_zone_explicitly() {
        // A user who configured `SharedSync-ABCD1234` has also made
        // a choice; don't second-guess them.
        assert!(should_notify_shared_libraries(&shared_zone(), 3, false).is_none());
    }

    #[test]
    fn notice_fires_with_singular_wording_for_one_library() {
        let msg = should_notify_shared_libraries(&primary(), 1, false).unwrap();
        assert!(
            msg.contains("1 iCloud shared library"),
            "singular 'library' wording expected; got: {msg}"
        );
        assert!(
            msg.contains("is being synced"),
            "singular verb 'is' expected; got: {msg}"
        );
        // The guidance is what the notice is for - it must name the config
        // key, the CLI flag, and the discovery subcommand.
        assert!(
            msg.contains("[filters] libraries = [\"all\"]"),
            "TOML guidance missing: {msg}"
        );
        assert!(
            !msg.contains("--library all"),
            "CLI guidance should be gone: {msg}"
        );
        assert!(
            msg.contains("kei list libraries"),
            "discovery guidance missing: {msg}"
        );
    }

    #[test]
    fn notice_fires_with_plural_wording_for_multiple_libraries() {
        let msg = should_notify_shared_libraries(&primary(), 3, false).unwrap();
        assert!(
            msg.contains("3 iCloud shared libraries"),
            "plural 'libraries' wording expected; got: {msg}"
        );
        assert!(
            msg.contains("are being synced"),
            "plural verb 'are' expected; got: {msg}"
        );
    }

    #[test]
    fn notice_suppressed_when_both_user_opted_out_and_already_notified() {
        // Belt-and-braces: every suppression condition stacks correctly.
        assert!(should_notify_shared_libraries(&all_libraries(), 0, true).is_none());
    }

    #[test]
    fn shared_library_notice_recent_check_uses_ttl() {
        let now = 1_800_000_000;
        assert!(shared_library_notice_recently_checked(
            Some(&(now - 60).to_string()),
            now
        ));
        assert!(!shared_library_notice_recently_checked(
            Some(&(now - SHARED_LIBRARY_NOTICE_CHECK_TTL_SECS - 1).to_string()),
            now
        ));
        assert!(!shared_library_notice_recently_checked(
            Some("not-a-ts"),
            now
        ));
    }

    #[derive(Clone)]
    struct CountingSharedLibrarySession {
        shared_calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl crate::icloud::photos::PhotosSession for CountingSharedLibrarySession {
        async fn post(
            &self,
            url: &str,
            _body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<serde_json::Value> {
            if url.contains("/shared/zones/list") {
                self.shared_calls
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                return Ok(serde_json::json!({"zones": []}));
            }
            anyhow::bail!("unexpected URL: {url}")
        }

        fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
            Box::new(self.clone())
        }
    }

    fn shared_notice_service(
        calls: Arc<std::sync::atomic::AtomicUsize>,
    ) -> crate::icloud::photos::PhotosService {
        crate::icloud::photos::PhotosService::for_testing(
            Box::new(CountingSharedLibrarySession {
                shared_calls: calls,
            }),
            std::collections::HashMap::new(),
        )
    }

    #[tokio::test]
    async fn shared_library_notice_skips_uncached_dry_run_probe_without_state_db() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut service = shared_notice_service(Arc::clone(&calls));

        maybe_notify_shared_libraries(&primary(), &mut service, None).await;

        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "without a state DB the shared-library probe cannot be cached, so dry-run should not pay the API call"
        );
    }

    #[tokio::test]
    async fn shared_library_notice_caches_no_shared_libraries() {
        let db = make_state_db();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let mut first = shared_notice_service(Arc::clone(&calls));
        maybe_notify_shared_libraries(&primary(), &mut first, Some(db.as_ref())).await;
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        let mut second = shared_notice_service(Arc::clone(&calls));
        maybe_notify_shared_libraries(&primary(), &mut second, Some(db.as_ref())).await;

        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "fresh no-shared marker should suppress the next shared-zone listing"
        );
    }

    // The run_sync reauth retry branch keys on whether an error from
    // init_photos_service or resolve_libraries is a session error.
    // Misclassifying either retries SRP on a non-session failure
    // (burning an Apple rate-limit slot) or fails to retry on a real
    // 401/421 (visible as an immediate Docker restart). Pin every
    // variant so a future ICloudError refactor can't silently regress.

    #[test]
    fn is_session_error_true_for_cloudkit_401_403() {
        let e: anyhow::Error =
            crate::icloud::error::ICloudError::SessionExpired { status: 401 }.into();
        assert!(is_session_error(&e), "401 must trigger reauth");

        let e: anyhow::Error =
            crate::icloud::error::ICloudError::SessionExpired { status: 403 }.into();
        assert!(is_session_error(&e), "403 must trigger reauth");
    }

    #[test]
    fn is_session_error_true_for_cloudkit_421() {
        let e: anyhow::Error = crate::icloud::error::ICloudError::MisdirectedRequest.into();
        assert!(
            is_session_error(&e),
            "persistent 421 must trigger reauth (stale routing state needs fresh SRP)"
        );
    }

    #[test]
    fn is_session_error_false_for_service_not_activated() {
        // ADP / ZONE_NOT_FOUND is a permanent failure, not a session issue.
        // Reauth would burn an Apple rate-limit slot for nothing.
        let e: anyhow::Error = crate::icloud::error::ICloudError::ServiceNotActivated {
            code: "ADP".into(),
            reason: "Advanced Data Protection".into(),
        }
        .into();
        assert!(!is_session_error(&e));
    }

    #[test]
    fn is_session_error_false_for_connection_and_io() {
        let e: anyhow::Error =
            crate::icloud::error::ICloudError::Connection("DNS failure".into()).into();
        assert!(!is_session_error(&e));

        let io = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "x");
        let e: anyhow::Error = crate::icloud::error::ICloudError::from(io).into();
        assert!(!is_session_error(&e));
    }

    #[test]
    fn is_session_error_false_for_non_icloud_error() {
        // Any other anyhow error (config parsing, state DB, etc.) must not
        // be classified as a session error — that would trigger an
        // inappropriate SRP cycle.
        let e = anyhow::anyhow!("unrelated top-level error");
        assert!(!is_session_error(&e));
    }

    #[test]
    fn is_session_error_peers_through_context() {
        // Real error chains are wrapped in .context() before hitting the
        // retry branch. The classifier downcasts on the root cause, which
        // anyhow exposes as downcast_ref — wrap here to pin the contract.
        let root = crate::icloud::error::ICloudError::SessionExpired { status: 401 };
        let e = anyhow::Error::from(root).context("while initializing photos service");
        assert!(
            is_session_error(&e),
            "classifier must downcast through context wrappers"
        );
    }

    /// Comprehensive classification table: every `ICloudError` variant plus
    /// a generic anyhow error. Prevents silent regressions from future enum
    /// additions — adding a new variant requires updating this table.
    #[test]
    fn is_session_error_classification_table() {
        use crate::icloud::error::ICloudError;

        // Variants that SHOULD trigger re-auth (session errors). Each entry
        // is a (label, anyhow::Error) pair so downcast_ref inside
        // is_session_error works correctly without needing Clone.
        let session_errors: Vec<(&str, anyhow::Error)> = vec![
            (
                "SessionExpired-401",
                ICloudError::SessionExpired { status: 401 }.into(),
            ),
            (
                "SessionExpired-403",
                ICloudError::SessionExpired { status: 403 }.into(),
            ),
            ("MisdirectedRequest", ICloudError::MisdirectedRequest.into()),
        ];
        for (label, e) in session_errors {
            assert!(
                is_session_error(&e),
                "expected {label} to be a session error"
            );
        }

        // Variants that must NOT trigger re-auth (non-session errors).
        let non_session: Vec<(&str, anyhow::Error)> = vec![
            (
                "Connection",
                ICloudError::Connection("DNS timeout".into()).into(),
            ),
            (
                "ServiceNotActivated",
                ICloudError::ServiceNotActivated {
                    code: "ZONE_NOT_FOUND".into(),
                    reason: "ADP".into(),
                }
                .into(),
            ),
            (
                "Io",
                ICloudError::from(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "denied",
                ))
                .into(),
            ),
            (
                "Json",
                ICloudError::from(
                    serde_json::from_str::<serde_json::Value>("not json").unwrap_err(),
                )
                .into(),
            ),
            ("non-ICloudError", anyhow::anyhow!("config parse error")),
        ];
        for (label, e) in non_session {
            assert!(
                !is_session_error(&e),
                "expected {label} to NOT be a session error"
            );
        }
    }

    // Classifier sees through `Box<dyn Error + Send + Sync>` wrappers.
    //
    // anyhow lets callers wrap raw boxed errors via `anyhow::Error::from`
    // when adapting third-party error returns. The downcast walk used by
    // `is_session_error` must conclude "not a session error" for any error
    // chain that was never an `ICloudError` — otherwise a future refactor
    // through a boxed-error adapter could silently flip every config /
    // network / state error into "burn an Apple SRP slot".
    #[test]
    fn is_session_error_through_boxed_error_returns_false() {
        // A boxed error of a foreign type wrapped via anyhow::Error::from.
        let boxed: Box<dyn std::error::Error + Send + Sync> =
            Box::new(std::io::Error::other("foreign"));
        let e: anyhow::Error = anyhow::Error::from_boxed(boxed);
        assert!(
            !is_session_error(&e),
            "boxed-error wrapper must not be classified as a session error"
        );

        // Also: anyhow::Error::msg(string) must be non-session — this is
        // the most common path through our error pipeline (a `bail!` from
        // a non-icloud module).
        let plain = anyhow::anyhow!("config parse failed at line 7");
        assert!(
            !is_session_error(&plain),
            "free-form anyhow::Error must not be classified as a session error"
        );
    }

    // ── find_multi_library_commingle_flags ───────────────────────────
    //
    // Multi-library sync without a `{library}` token lets same-named
    // assets from different zones share an on-disk namespace. The
    // `file_match_policy` keeps writes from silently overwriting (the
    // default policy adds a `-N` suffix on collision), but the user
    // probably wanted zone-disjoint trees. `warn_if_*` emits a startup
    // warning; these tests pin the underlying `find_*` truth table.

    /// Build a Selection that activates every pass kind. The default
    /// `LibrarySelector` is fine — the guard reads only `albums`,
    /// `smart_folders`, `unfiled` from the Selection.
    fn selection_all_passes_active() -> crate::selection::Selection {
        use crate::selection::{AlbumSelector, LibrarySelector, Selection, SmartFolderSelector};
        Selection {
            albums: AlbumSelector::All {
                excluded: std::collections::BTreeSet::new(),
            },
            smart_folders: SmartFolderSelector::All {
                include_sensitive: false,
                excluded: std::collections::BTreeSet::new(),
            },
            libraries: LibrarySelector::default(),
            unfiled: true,
        }
    }

    /// Build a Selection that activates only the unfiled pass.
    fn selection_unfiled_only() -> crate::selection::Selection {
        use crate::selection::{AlbumSelector, LibrarySelector, Selection, SmartFolderSelector};
        Selection {
            albums: AlbumSelector::None,
            smart_folders: SmartFolderSelector::None,
            libraries: LibrarySelector::default(),
            unfiled: true,
        }
    }

    #[test]
    fn find_multi_library_commingle_flags_short_circuits_under_two_libraries() {
        // Zero or one library never flags any template, regardless of
        // template content or active-pass selection.
        let sel = selection_all_passes_active();
        assert!(find_multi_library_commingle_flags(
            0,
            "%Y/%m/%d",
            "{album}",
            "{smart-folder}",
            &sel
        )
        .is_empty());
        assert!(find_multi_library_commingle_flags(
            1,
            "%Y/%m/%d",
            "{album}",
            "{smart-folder}",
            &sel
        )
        .is_empty());
    }

    #[test]
    fn find_multi_library_commingle_flags_accepts_library_token_in_active_template_only() {
        // CG-7 contract: when every active template carries `{library}`,
        // multi-library is safe. Inactive templates are irrelevant -
        // their pass kind doesn't run.
        let all = selection_all_passes_active();
        assert!(
            find_multi_library_commingle_flags(
                2,
                "{library}/%Y/%m/%d",
                "{library}/{album}",
                "{library}/{smart-folder}",
                &all,
            )
            .is_empty(),
            "every active template carries `{{library}}` - no commingle"
        );

        // When only the unfiled pass is active, the unfiled template is
        // the only one that needs `{library}`. The album / smart-folder
        // templates can be anything because no pass reads them.
        let unfiled = selection_unfiled_only();
        assert!(
            find_multi_library_commingle_flags(
                2,
                "{library}/%Y/%m/%d",
                "{album}",
                "{smart-folder}",
                &unfiled,
            )
            .is_empty(),
            "only unfiled active and its template has `{{library}}`"
        );
    }

    #[test]
    fn find_multi_library_commingle_flags_reports_per_active_pass() {
        // CG-6 contract: only *active* passes whose templates lack
        // `{library}` show up in the missing-flags list.
        //
        // Scenario: --folder-structure-albums '{library}/{album}' (token
        // present, active) + --folder-structure-smart-folders
        // '{smart-folder}' (no token, active because --smart-folder all)
        // + --unfiled false (inactive).
        use crate::selection::{AlbumSelector, LibrarySelector, Selection, SmartFolderSelector};
        let sel = Selection {
            albums: AlbumSelector::All {
                excluded: std::collections::BTreeSet::new(),
            },
            smart_folders: SmartFolderSelector::All {
                include_sensitive: false,
                excluded: std::collections::BTreeSet::new(),
            },
            libraries: LibrarySelector::default(),
            unfiled: false,
        };
        let missing = find_multi_library_commingle_flags(
            2,
            "%Y/%m/%d",
            "{library}/{album}",
            "{smart-folder}",
            &sel,
        );
        assert_eq!(
            missing,
            vec!["--folder-structure-smart-folders"],
            "only the active smart-folder pass with `{{library}}`-less template should be listed"
        );

        // Negative: same templates but smart-folder pass disabled. No
        // active pass lacks `{library}`, so the list is empty.
        let sel_no_smart = Selection {
            albums: AlbumSelector::All {
                excluded: std::collections::BTreeSet::new(),
            },
            smart_folders: SmartFolderSelector::None,
            libraries: LibrarySelector::default(),
            unfiled: false,
        };
        assert!(
            find_multi_library_commingle_flags(
                2,
                "%Y/%m/%d",
                "{library}/{album}",
                "{smart-folder}",
                &sel_no_smart,
            )
            .is_empty(),
            "smart-folder inactive - its `{{library}}`-less template is irrelevant"
        );
    }

    #[test]
    fn find_multi_library_commingle_flags_reports_all_missing_when_every_active_template_lacks_token(
    ) {
        let sel = selection_all_passes_active();
        let missing =
            find_multi_library_commingle_flags(2, "%Y/%m/%d", "{album}", "{smart-folder}", &sel);
        assert_eq!(
            missing,
            vec![
                "--folder-structure",
                "--folder-structure-albums",
                "--folder-structure-smart-folders",
            ],
            "every active template lacks `{{library}}` - all three should be listed",
        );
    }

    #[test]
    fn find_multi_library_commingle_flags_reports_with_none_folder_structure_too() {
        // `none` (date hierarchy disabled) is the worst-case commingle:
        // every asset lands directly in the download dir. Still surfaces
        // the unfiled flag so the user knows the namespace is shared.
        let sel = selection_all_passes_active();
        let missing =
            find_multi_library_commingle_flags(5, "none", "{album}", "{smart-folder}", &sel);
        assert!(missing.contains(&"--folder-structure"));
    }

    #[test]
    fn find_multi_library_commingle_flags_short_circuits_when_no_passes_active() {
        // --album none + --smart-folder none + --unfiled false: every
        // pass is disabled, resolve_passes returns an empty plan, no
        // path is ever rendered, so multi-library can't commingle even
        // without `{library}` in any template.
        use crate::selection::{AlbumSelector, LibrarySelector, Selection, SmartFolderSelector};
        let sel = Selection {
            albums: AlbumSelector::None,
            smart_folders: SmartFolderSelector::None,
            libraries: LibrarySelector::default(),
            unfiled: false,
        };
        assert!(
            find_multi_library_commingle_flags(3, "%Y/%m/%d", "{album}", "{smart-folder}", &sel)
                .is_empty(),
            "no active passes - find_* must report empty"
        );
    }

    /// CG-6 (2026-05-03 test review): the existing suite asserts the
    /// *return value* of `find_multi_library_commingle_flags`, but
    /// nothing pins the contract that `warn_if_multi_library_paths_commingle`
    /// emits the `library_count` and `missing` lists as structured tracing
    /// fields. A future refactor that drops the named args (or replaces
    /// them with a positional message) would silently lose operator
    /// visibility into commingle scenarios. Pin the field shape so the
    /// regression is loud.
    #[tracing_test::traced_test]
    #[test]
    fn warn_if_multi_library_paths_commingle_emits_structured_fields() {
        let sel = selection_all_passes_active();
        warn_if_multi_library_paths_commingle(3, "%Y/%m/%d", "{album}", "{smart-folder}", &sel);
        assert!(
            logs_contain("library_count=3"),
            "structured library_count field expected on warn line"
        );
        // `missing = ?missing` debug-formats the Vec; the rendered output
        // contains the bare flag names. Pin at least the unfiled-related
        // root flag and the album-template flag.
        assert!(
            logs_contain("missing="),
            "structured `missing` field expected on warn line"
        );
        assert!(
            logs_contain("--folder-structure"),
            "missing list should include the root --folder-structure flag"
        );
        assert!(
            logs_contain("--folder-structure-albums"),
            "missing list should include the album-template flag"
        );
        assert!(
            logs_contain("--folder-structure-smart-folders"),
            "missing list should include the smart-folder-template flag"
        );
    }

    /// CG-6 negative: when `{library}` is present in every active
    /// template, the warn must NOT fire. Catches the inverse mutation
    /// (warn fires unconditionally).
    #[tracing_test::traced_test]
    #[test]
    fn warn_if_multi_library_paths_commingle_silent_when_no_commingle() {
        let sel = selection_all_passes_active();
        warn_if_multi_library_paths_commingle(
            3,
            "{library}/%Y/%m/%d",
            "{library}/{album}",
            "{library}/{smart-folder}",
            &sel,
        );
        assert!(
            !logs_contain("library_count="),
            "warn must not fire when every active template carries `{{library}}`"
        );
    }

    // ── count_passes ────────────────────────────────────────────────────
    //
    // Pass tally feeds the per-library `Sync plan for library` info line.
    // The numbers map directly to API surface (one enumeration per album /
    // smart-folder pass + one for unfiled), so a regression that drops a
    // category would silently mislead the operator.

    fn make_pass(name: &str, kind: crate::commands::PassKind) -> crate::commands::AlbumPass {
        crate::commands::AlbumPass {
            kind,
            album: crate::icloud::photos::PhotoAlbum::stub_for_test(std::sync::Arc::from(name)),
            exclude_ids: std::sync::Arc::new(rustc_hash::FxHashSet::default()),
        }
    }

    fn make_incremental_album(zone_sync_token: &str) -> crate::icloud::photos::PhotoAlbum {
        use serde_json::json;
        crate::icloud::photos::PhotoAlbum::new(
            crate::icloud::photos::PhotoAlbumConfig {
                params: Arc::new(std::collections::HashMap::new()),
                service_endpoint: Arc::from("https://example.com"),
                name: Arc::from("TestAlbum"),
                list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
                obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
                query_filter: None,
                page_size: 100,
                zone_id: Arc::new(json!({"zoneName": "PrimarySync"})),
                retry_config: retry::RetryConfig::default(),
                container_id: None,
                cross_zone_sources: Vec::new(),
            },
            Box::new(crate::test_helpers::MockPhotosSession::new().ok(json!({
                "zones": [{
                    "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                    "syncToken": zone_sync_token,
                    "moreComing": false,
                    "records": []
                }]
            }))),
        )
    }

    fn make_one_photo_incremental_album_for_zone(
        zone: &str,
        zone_sync_token: &str,
    ) -> crate::icloud::photos::PhotoAlbum {
        use serde_json::json;
        let page = full_album_page(zone, &format!("master-{zone}"), zone_sync_token);
        let records = page
            .get("records")
            .expect("full album page records")
            .clone();

        make_full_album_with_session(
            zone,
            crate::test_helpers::MockPhotosSession::new().ok(json!({
                "zones": [{
                    "zoneID": {"zoneName": zone, "ownerRecordName": "_defaultOwner"},
                    "syncToken": zone_sync_token,
                    "moreComing": false,
                    "records": records
                }]
            })),
        )
    }

    fn album_count_response(count: u64) -> serde_json::Value {
        serde_json::json!({
            "batch": [{"records": [{"fields": {"itemCount": {"value": count}}}]}]
        })
    }

    fn full_album_page(zone: &str, record_name: &str, sync_token: &str) -> serde_json::Value {
        full_album_page_with_download(
            zone,
            record_name,
            sync_token,
            "https://p01.icloud-content.com/photo.jpg",
            1024,
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
        )
    }

    fn full_album_page_with_download(
        zone: &str,
        record_name: &str,
        sync_token: &str,
        download_url: &str,
        size: u64,
        checksum: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "records": [
                {
                    "recordName": record_name,
                    "recordType": "CPLMaster",
                    "fields": {
                        "filenameEnc": {"value": "cGhvdG8uanBn", "type": "STRING"},
                        "resOriginalRes": {
                            "value": {
                                "downloadURL": download_url,
                                "size": size,
                                "fileChecksum": checksum
                            }
                        },
                        "resOriginalWidth": {"value": 100, "type": "INT64"},
                        "resOriginalHeight": {"value": 100, "type": "INT64"},
                        "resOriginalFileType": {"value": "public.jpeg"},
                        "itemType": {"value": "public.jpeg"},
                        "adjustmentRenderType": {"value": 0, "type": "INT64"}
                    },
                    "recordChangeTag": "ct-master"
                },
                {
                    "recordName": format!("asset-{record_name}"),
                    "recordType": "CPLAsset",
                    "fields": {
                        "masterRef": {
                            "value": {"recordName": record_name, "zoneID": {"zoneName": zone}},
                            "type": "REFERENCE"
                        },
                        "assetDate": {"value": 1700000000000i64, "type": "TIMESTAMP"},
                        "addedDate": {"value": 1700000000000i64, "type": "TIMESTAMP"}
                    },
                    "recordChangeTag": "ct-asset"
                }
            ],
            "syncToken": sync_token
        })
    }

    fn make_full_album_with_session(
        zone: &str,
        session: crate::test_helpers::MockPhotosSession,
    ) -> crate::icloud::photos::PhotoAlbum {
        make_full_album_with_boxed_session(zone, Box::new(session))
    }

    fn make_full_album_with_boxed_session(
        zone: &str,
        session: Box<dyn crate::icloud::photos::PhotosSession>,
    ) -> crate::icloud::photos::PhotoAlbum {
        use serde_json::json;
        crate::icloud::photos::PhotoAlbum::new(
            crate::icloud::photos::PhotoAlbumConfig {
                params: Arc::new(std::collections::HashMap::new()),
                service_endpoint: Arc::from("https://example.com"),
                name: Arc::from("TestAlbum"),
                list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
                obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
                query_filter: None,
                page_size: 100,
                zone_id: Arc::new(json!({"zoneName": zone})),
                retry_config: retry::RetryConfig::default(),
                container_id: None,
                cross_zone_sources: Vec::new(),
            },
            session,
        )
    }

    fn make_empty_full_album(zone_sync_token: &str) -> crate::icloud::photos::PhotoAlbum {
        make_empty_full_album_for_zone("PrimarySync", zone_sync_token)
    }

    fn make_empty_full_album_for_zone(
        zone: &str,
        zone_sync_token: &str,
    ) -> crate::icloud::photos::PhotoAlbum {
        make_full_album_with_session(
            zone,
            crate::test_helpers::MockPhotosSession::new()
                .ok(album_count_response(0))
                .ok(serde_json::json!({"records": [], "syncToken": zone_sync_token})),
        )
    }

    fn make_one_photo_full_album_for_zone(
        zone: &str,
        zone_sync_token: &str,
    ) -> crate::icloud::photos::PhotoAlbum {
        make_full_album_with_session(
            zone,
            crate::test_helpers::MockPhotosSession::new()
                .ok(album_count_response(1))
                .ok(full_album_page(
                    zone,
                    &format!("master-{zone}"),
                    zone_sync_token,
                )),
        )
    }

    fn make_run_cycle_library_state(
        zone: &str,
        sync_token_key: &str,
        zone_sync_token: &str,
    ) -> LibraryState {
        make_run_cycle_library_state_with_album(
            zone,
            sync_token_key,
            make_incremental_album(zone_sync_token),
        )
    }

    fn make_run_cycle_library_state_with_album(
        zone: &str,
        sync_token_key: &str,
        album: crate::icloud::photos::PhotoAlbum,
    ) -> LibraryState {
        LibraryState {
            library: crate::icloud::photos::PhotoLibrary::new_stub_with_zone(
                Box::new(crate::test_helpers::MockPhotosSession::new()),
                zone,
            ),
            zone_name: zone.to_string(),
            sync_token_key: sync_token_key.to_string(),
            plan: crate::commands::AlbumPlan {
                passes: vec![crate::commands::AlbumPass {
                    kind: crate::commands::PassKind::Unfiled,
                    album,
                    exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
                }],
            },
            plan_is_stale: false,
            plan_needs_refresh: false,
            cross_zone_libraries: Vec::new(),
        }
    }

    async fn make_shared_session_for_run_cycle() -> (tempfile::TempDir, auth::SharedSession) {
        let dir = tempfile::tempdir().expect("session tempdir");
        let session = auth::session::Session::new(
            dir.path(),
            "test@example.com",
            "https://example.com",
            None,
        )
        .await
        .expect("test session");
        (dir, Arc::new(tokio::sync::RwLock::new(session)))
    }

    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
    struct RunCycleDownloadConfigOptions {
        media: config::MediaSelection,
        per_pass_paths: bool,
        recent: Option<u32>,
    }

    fn media_without_photo_downloads() -> config::MediaSelection {
        config::MediaSelection {
            photos: false,
            videos: true,
            live_photos: true,
        }
    }

    fn make_run_cycle_download_config_builder(
        download_dir: &std::path::Path,
        db: Arc<dyn state::StateDb>,
    ) -> impl Fn(
        download::SyncMode,
        Arc<rustc_hash::FxHashSet<String>>,
        Arc<download::AssetGroupings>,
        Arc<str>,
    ) -> Arc<download::DownloadConfig>
           + '_ {
        make_run_cycle_download_config_builder_with_options(
            download_dir,
            db,
            RunCycleDownloadConfigOptions::default(),
        )
    }

    fn make_run_cycle_download_config_builder_with_options(
        download_dir: &std::path::Path,
        db: Arc<dyn state::StateDb>,
        options: RunCycleDownloadConfigOptions,
    ) -> impl Fn(
        download::SyncMode,
        Arc<rustc_hash::FxHashSet<String>>,
        Arc<download::AssetGroupings>,
        Arc<str>,
    ) -> Arc<download::DownloadConfig>
           + '_ {
        move |sync_mode, exclude_asset_ids, asset_groupings, library| {
            let mut config = download::DownloadConfig::test_default();
            config.directory = Arc::from(download_dir);
            config.folder_structure = "%Y/%m/%d".to_string();
            config.folder_structure_albums = Arc::from("%Y/%m/%d");
            config.folder_structure_smart_folders = Arc::from("%Y/%m/%d");
            if options.per_pass_paths {
                config.folder_structure_albums = Arc::from("{album}");
            }
            config.media = options.media;
            config.recent = options.recent;
            config.state_db = Some(Arc::clone(&db));
            config.sync_mode = sync_mode;
            config.exclude_asset_ids = exclude_asset_ids;
            config.asset_groupings = asset_groupings;
            config.library = library;
            Arc::new(config)
        }
    }

    fn make_recording_run_cycle_download_config_builder(
        download_dir: &std::path::Path,
        db: Arc<dyn state::StateDb>,
        observed_modes: Arc<std::sync::Mutex<Vec<download::SyncMode>>>,
    ) -> impl Fn(
        download::SyncMode,
        Arc<rustc_hash::FxHashSet<String>>,
        Arc<download::AssetGroupings>,
        Arc<str>,
    ) -> Arc<download::DownloadConfig>
           + '_ {
        let build_download_config = make_run_cycle_download_config_builder(download_dir, db);
        move |sync_mode, exclude_asset_ids, asset_groupings, library| {
            observed_modes
                .lock()
                .expect("recorded modes lock")
                .push(sync_mode.clone());
            build_download_config(sync_mode, exclude_asset_ids, asset_groupings, library)
        }
    }

    fn make_run_cycle_config() -> config::Config {
        let data_dir = tempfile::tempdir().expect("config data dir");
        let globals = config::GlobalArgs {
            username: Some("test@example.com".to_string()),
            domain: None,
            data_dir: Some(data_dir.path().to_string_lossy().into_owned()),
        };
        config::Config::build(
            &globals,
            &cli::PasswordArgs::default(),
            cli::SyncArgs::default(),
            None,
        )
        .expect("test config")
    }

    async fn run_full_cycle_with_album(
        album: crate::icloud::photos::PhotoAlbum,
        is_retry_failed: bool,
        controls: download::DownloadControls,
    ) -> CycleResult {
        let config = make_run_cycle_config();
        let db = make_state_db();
        let download_dir = tempfile::tempdir().expect("download tempdir");
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

        let lib_state =
            make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
        let states = vec![&lib_state];
        let build_download_config =
            make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));

        run_cycle(
            &states,
            &config,
            Some(db.as_ref()),
            is_retry_failed,
            &build_download_config,
            controls,
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("run cycle")
    }

    async fn run_empty_full_cycle(is_retry_failed: bool) -> CycleResult {
        run_empty_full_cycle_with_controls(
            is_retry_failed,
            download::DownloadControls::download_hidden(),
        )
        .await
    }

    async fn run_empty_full_cycle_with_controls(
        is_retry_failed: bool,
        controls: download::DownloadControls,
    ) -> CycleResult {
        run_full_cycle_with_album(
            make_empty_full_album("zone-tok-empty"),
            is_retry_failed,
            controls,
        )
        .await
    }

    async fn run_one_photo_full_cycle_with_controls(
        controls: download::DownloadControls,
    ) -> CycleResult {
        run_full_cycle_with_album(
            make_one_photo_full_album_for_zone("PrimarySync", "zone-tok-one"),
            false,
            controls,
        )
        .await
    }

    #[tokio::test]
    async fn run_cycle_recent_full_download_does_not_store_zone_token() {
        let mut config = make_run_cycle_config();
        config.filters.recent = Some(40);
        let db = make_state_db();
        let download_dir = tempfile::tempdir().expect("download tempdir");
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
        let session = crate::test_helpers::DynamicRecentPhotosSession::new(40)
            .with_filename_prefix("cycle-recent")
            .with_token("zone-tok-recent");
        let album = make_full_album_with_boxed_session("PrimarySync", Box::new(session));
        let lib_state =
            make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
        let states = vec![&lib_state];
        let build_download_config = make_run_cycle_download_config_builder_with_options(
            download_dir.path(),
            Arc::clone(&db),
            RunCycleDownloadConfigOptions {
                media: media_without_photo_downloads(),
                recent: Some(40),
                ..RunCycleDownloadConfigOptions::default()
            },
        );

        let result = run_cycle(
            &states,
            &config,
            Some(db.as_ref()),
            false,
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("run recent cycle");

        assert_eq!(result.failed_count, 0);
        assert_eq!(result.stats.assets_seen, 40);
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .expect("read zone token"),
            None,
            "recent-limited full cycle must not persist a zone token"
        );
    }

    #[tokio::test]
    async fn watch_recent_first_cycle_does_not_seed_incremental_token() {
        let mut config = make_run_cycle_config();
        config.filters.recent = Some(20);
        let db = make_state_db();
        let download_dir = tempfile::tempdir().expect("download tempdir");
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
        let session = crate::test_helpers::DynamicRecentPhotosSession::new(20)
            .with_filename_prefix("watch-recent")
            .with_token("zone-tok-watch");
        let album = make_full_album_with_boxed_session("PrimarySync", Box::new(session));
        let lib_state =
            make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
        let states = vec![&lib_state];
        let build_download_config = make_run_cycle_download_config_builder_with_options(
            download_dir.path(),
            Arc::clone(&db),
            RunCycleDownloadConfigOptions {
                media: media_without_photo_downloads(),
                recent: Some(20),
                ..RunCycleDownloadConfigOptions::default()
            },
        );

        let result = run_cycle(
            &states,
            &config,
            Some(db.as_ref()),
            false,
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("run first watch-like recent cycle");

        assert_eq!(result.failed_count, 0);
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .expect("read zone token"),
            None
        );
        let next_mode = determine_sync_mode(
            false,
            1,
            Some(db.as_ref()),
            "sync_token:PrimarySync",
            "PrimarySync",
        )
        .await;
        assert!(
            matches!(next_mode, download::SyncMode::Full),
            "a later watch cycle must not switch to incremental from a recent-limited token"
        );
    }

    #[tokio::test]
    async fn run_cycle_recent_multiple_libraries_downloads_each_zone_without_token_advance() {
        let mut config = make_run_cycle_config();
        config.filters.recent = Some(20);
        let db = make_state_db();
        let download_dir = tempfile::tempdir().expect("download tempdir");
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

        let primary_session = crate::test_helpers::DynamicRecentPhotosSession::new(20)
            .with_filename_prefix("primary-recent")
            .with_zone("PrimarySync")
            .with_token("zone-tok-primary");
        let shared_session_photos = crate::test_helpers::DynamicRecentPhotosSession::new(20)
            .with_filename_prefix("shared-recent")
            .with_zone("SharedSync-TEST")
            .with_token("zone-tok-shared");
        let primary_state = make_run_cycle_library_state_with_album(
            "PrimarySync",
            "sync_token:PrimarySync",
            make_full_album_with_boxed_session("PrimarySync", Box::new(primary_session)),
        );
        let shared_state = make_run_cycle_library_state_with_album(
            "SharedSync-TEST",
            "sync_token:SharedSync-TEST",
            make_full_album_with_boxed_session("SharedSync-TEST", Box::new(shared_session_photos)),
        );
        let states = vec![&primary_state, &shared_state];
        let build_download_config = make_run_cycle_download_config_builder_with_options(
            download_dir.path(),
            Arc::clone(&db),
            RunCycleDownloadConfigOptions {
                media: media_without_photo_downloads(),
                recent: Some(20),
                ..RunCycleDownloadConfigOptions::default()
            },
        );

        let result = run_cycle(
            &states,
            &config,
            Some(db.as_ref()),
            false,
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("run recent multi-library cycle");

        assert_eq!(result.failed_count, 0);
        assert_eq!(result.stats.assets_seen, 40);
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .expect("read primary token"),
            None
        );
        assert_eq!(
            db.get_metadata("sync_token:SharedSync-TEST")
                .await
                .expect("read shared token"),
            None
        );
    }

    const ZERO_ASSET_WARNING_PREFIX: &str = "Sync completed after enumerating zero assets";

    #[test]
    fn count_passes_empty_plan_is_all_zero() {
        let plan = crate::commands::AlbumPlan { passes: Vec::new() };
        assert_eq!(count_passes(&plan), (0, 0, false));
    }

    #[test]
    fn count_passes_tallies_each_kind_independently() {
        use crate::commands::PassKind;
        let plan = crate::commands::AlbumPlan {
            passes: vec![
                make_pass("Vacation", PassKind::Album),
                make_pass("Family", PassKind::Album),
                make_pass("Favorites", PassKind::SmartFolder),
                make_pass("PrimarySync", PassKind::Unfiled),
            ],
        };
        assert_eq!(count_passes(&plan), (2, 1, true));
    }

    #[test]
    fn count_passes_unfiled_only_returns_zero_album_zero_smart_folder() {
        use crate::commands::PassKind;
        let plan = crate::commands::AlbumPlan {
            passes: vec![make_pass("PrimarySync", PassKind::Unfiled)],
        };
        assert_eq!(count_passes(&plan), (0, 0, true));
    }

    // ── should_retry_session_init ──────────────────────────────────────
    //
    // The init retry guard allows exactly one SRP re-auth on a session-error,
    // then bails. This prevents infinite loops under Docker restart policies.

    #[test]
    fn should_retry_session_init_true_on_first_421() {
        let err: anyhow::Error = crate::icloud::error::ICloudError::MisdirectedRequest.into();
        assert!(should_retry_session_init(&err, false));
    }

    #[test]
    fn should_retry_session_init_false_on_second_421() {
        let err: anyhow::Error = crate::icloud::error::ICloudError::MisdirectedRequest.into();
        assert!(!should_retry_session_init(&err, true));
    }

    #[test]
    fn should_retry_session_init_false_for_non_session_error() {
        let err: anyhow::Error =
            crate::icloud::error::ICloudError::Connection("timeout".into()).into();
        assert!(!should_retry_session_init(&err, false));
    }

    #[test]
    fn should_retry_session_init_true_for_first_401() {
        let err: anyhow::Error =
            crate::icloud::error::ICloudError::SessionExpired { status: 401 }.into();
        assert!(should_retry_session_init(&err, false));
    }

    #[test]
    fn take_pending_auth_returns_value_once() {
        let mut pending = Some("auth");

        assert_eq!(take_pending_auth(&mut pending).unwrap(), "auth");
        assert!(pending.is_none());
    }

    #[test]
    fn take_pending_auth_empty_state_returns_error() {
        let mut pending: Option<&str> = None;

        let err = take_pending_auth(&mut pending).unwrap_err();

        assert!(
            err.to_string()
                .contains("internal auth retry state missing before attempt"),
            "unexpected error: {err}"
        );
    }

    // ── determine_sync_mode ──────────────────────────────────────────
    //
    // Sync-mode decision is the gatekeeper for the kei "user data is sacred"
    // invariant: pick Full vs Incremental wrong and either (a) re-download
    // the world (waste) or (b) skip previously-failed assets (silent loss).
    // None of the four critical branches had a direct unit test before.

    fn make_state_db() -> Arc<dyn state::StateDb> {
        Arc::new(state::SqliteStateDb::open_in_memory().expect("open in-memory state DB"))
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum MetadataSetFailure {
        Exact(&'static str),
        Prefix(&'static str),
    }

    impl MetadataSetFailure {
        fn matches(self, key: &str) -> bool {
            match self {
                Self::Exact(expected) => key == expected,
                Self::Prefix(prefix) => key.starts_with(prefix),
            }
        }
    }

    struct FailingMetadataSetDb {
        inner: Arc<dyn state::StateDb>,
        failure: MetadataSetFailure,
        get_failure: Option<MetadataSetFailure>,
        delete_prefix_failure: Option<&'static str>,
        message: &'static str,
        cancel_on_upsert: Option<CancellationToken>,
        replace_download_dir_on_upsert: Option<std::path::PathBuf>,
    }

    impl std::fmt::Debug for FailingMetadataSetDb {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("FailingMetadataSetDb")
                .field("failure", &self.failure)
                .field("get_failure", &self.get_failure)
                .field("delete_prefix_failure", &self.delete_prefix_failure)
                .field("message", &self.message)
                .finish_non_exhaustive()
        }
    }

    impl FailingMetadataSetDb {
        fn new(
            inner: Arc<dyn state::StateDb>,
            failure: MetadataSetFailure,
            message: &'static str,
        ) -> Self {
            Self {
                inner,
                failure,
                get_failure: None,
                delete_prefix_failure: None,
                message,
                cancel_on_upsert: None,
                replace_download_dir_on_upsert: None,
            }
        }

        fn without_set_failure(inner: Arc<dyn state::StateDb>, message: &'static str) -> Self {
            Self::new(
                inner,
                MetadataSetFailure::Exact("__unused_metadata_key__"),
                message,
            )
        }

        fn with_get_failure(mut self, failure: MetadataSetFailure) -> Self {
            self.get_failure = Some(failure);
            self
        }

        fn with_delete_prefix_failure(mut self, prefix: &'static str) -> Self {
            self.delete_prefix_failure = Some(prefix);
            self
        }

        fn with_cancel_on_upsert(mut self, token: CancellationToken) -> Self {
            self.cancel_on_upsert = Some(token);
            self
        }

        fn with_download_dir_replaced_on_upsert(mut self, path: std::path::PathBuf) -> Self {
            self.replace_download_dir_on_upsert = Some(path);
            self
        }
    }

    #[async_trait::async_trait]
    impl state::StateDb for FailingMetadataSetDb {
        #[cfg(test)]
        async fn should_download(
            &self,
            library: &str,
            id: &str,
            version_size: &str,
            checksum: &str,
            local_path: &std::path::Path,
        ) -> Result<bool, state::error::StateError> {
            self.inner
                .should_download(library, id, version_size, checksum, local_path)
                .await
        }

        async fn upsert_seen(
            &self,
            record: &state::types::AssetRecord,
        ) -> Result<(), state::error::StateError> {
            let result = self.inner.upsert_seen(record).await;
            if result.is_ok() {
                if let Some(path) = &self.replace_download_dir_on_upsert {
                    let _ = std::fs::remove_dir_all(path);
                    std::fs::write(path, b"destination replaced by fault injection")
                        .expect("replace download dir with file");
                }
                if let Some(token) = &self.cancel_on_upsert {
                    token.cancel();
                }
            }
            result
        }

        async fn mark_downloaded(
            &self,
            library: &str,
            id: &str,
            version_size: &str,
            local_path: &std::path::Path,
            local_checksum: &str,
            download_checksum: Option<&str>,
        ) -> Result<(), state::error::StateError> {
            self.inner
                .mark_downloaded(
                    library,
                    id,
                    version_size,
                    local_path,
                    local_checksum,
                    download_checksum,
                )
                .await
        }

        async fn import_adopt(
            &self,
            record: &state::types::AssetRecord,
            local_path: &std::path::Path,
            local_checksum: &str,
            imported_size: u64,
            imported_mtime: Option<i64>,
        ) -> Result<(), state::error::StateError> {
            self.inner
                .import_adopt(
                    record,
                    local_path,
                    local_checksum,
                    imported_size,
                    imported_mtime,
                )
                .await
        }

        async fn mark_failed(
            &self,
            library: &str,
            id: &str,
            version_size: &str,
            error: &str,
        ) -> Result<(), state::error::StateError> {
            self.inner
                .mark_failed(library, id, version_size, error)
                .await
        }

        async fn get_failed(
            &self,
        ) -> Result<Vec<state::types::AssetRecord>, state::error::StateError> {
            self.inner.get_failed().await
        }

        async fn get_failed_sample(
            &self,
            limit: u32,
        ) -> Result<(Vec<state::types::AssetRecord>, u64), state::error::StateError> {
            self.inner.get_failed_sample(limit).await
        }

        async fn get_pending(
            &self,
        ) -> Result<Vec<state::types::AssetRecord>, state::error::StateError> {
            self.inner.get_pending().await
        }

        async fn get_summary(&self) -> Result<state::types::SyncSummary, state::error::StateError> {
            self.inner.get_summary().await
        }

        async fn get_downloaded_page(
            &self,
            offset: u64,
            limit: u32,
        ) -> Result<Vec<state::types::AssetRecord>, state::error::StateError> {
            self.inner.get_downloaded_page(offset, limit).await
        }

        async fn start_sync_run(&self) -> Result<i64, state::error::StateError> {
            self.inner.start_sync_run().await
        }

        async fn complete_sync_run(
            &self,
            run_id: i64,
            stats: &state::types::SyncRunStats,
        ) -> Result<(), state::error::StateError> {
            self.inner.complete_sync_run(run_id, stats).await
        }

        async fn promote_orphaned_sync_runs(&self) -> Result<u64, state::error::StateError> {
            self.inner.promote_orphaned_sync_runs().await
        }

        async fn begin_enum_progress(&self, zone: &str) -> Result<(), state::error::StateError> {
            self.inner.begin_enum_progress(zone).await
        }

        async fn end_enum_progress(&self, zone: &str) -> Result<(), state::error::StateError> {
            self.inner.end_enum_progress(zone).await
        }

        async fn list_interrupted_enumerations(
            &self,
        ) -> Result<Vec<String>, state::error::StateError> {
            self.inner.list_interrupted_enumerations().await
        }

        async fn reset_failed(&self) -> Result<u64, state::error::StateError> {
            self.inner.reset_failed().await
        }

        async fn prepare_for_retry(&self) -> Result<(u64, u64, u64), state::error::StateError> {
            self.inner.prepare_for_retry().await
        }

        async fn promote_pending_to_failed(
            &self,
            seen_since: i64,
        ) -> Result<u64, state::error::StateError> {
            self.inner.promote_pending_to_failed(seen_since).await
        }

        async fn get_downloaded_ids(
            &self,
        ) -> Result<std::collections::HashSet<(String, String, String)>, state::error::StateError>
        {
            self.inner.get_downloaded_ids().await
        }

        async fn get_all_known_ids(
            &self,
        ) -> Result<std::collections::HashSet<String>, state::error::StateError> {
            self.inner.get_all_known_ids().await
        }

        async fn get_downloaded_checksums(
            &self,
        ) -> Result<
            std::collections::HashMap<(String, String, String), String>,
            state::error::StateError,
        > {
            self.inner.get_downloaded_checksums().await
        }

        async fn get_attempt_counts(
            &self,
        ) -> Result<std::collections::HashMap<String, u32>, state::error::StateError> {
            self.inner.get_attempt_counts().await
        }

        async fn get_metadata(
            &self,
            key: &str,
        ) -> Result<Option<String>, state::error::StateError> {
            if self.get_failure.is_some_and(|failure| failure.matches(key)) {
                Err(state::error::StateError::LockPoisoned(self.message.into()))
            } else {
                self.inner.get_metadata(key).await
            }
        }

        async fn set_metadata(
            &self,
            key: &str,
            value: &str,
        ) -> Result<(), state::error::StateError> {
            if self.failure.matches(key) {
                Err(state::error::StateError::LockPoisoned(self.message.into()))
            } else {
                self.inner.set_metadata(key, value).await
            }
        }

        async fn delete_metadata_by_prefix(
            &self,
            prefix: &str,
        ) -> Result<u64, state::error::StateError> {
            if self.delete_prefix_failure == Some(prefix) {
                Err(state::error::StateError::LockPoisoned(self.message.into()))
            } else {
                self.inner.delete_metadata_by_prefix(prefix).await
            }
        }

        async fn touch_last_seen_many(
            &self,
            library: &str,
            asset_ids: &[&str],
        ) -> Result<(), state::error::StateError> {
            self.inner.touch_last_seen_many(library, asset_ids).await
        }

        async fn add_asset_album(
            &self,
            library: &str,
            asset_id: &str,
            album_name: &str,
            source: &str,
        ) -> Result<(), state::error::StateError> {
            self.inner
                .add_asset_album(library, asset_id, album_name, source)
                .await
        }

        async fn get_all_asset_albums(
            &self,
            library: &str,
        ) -> Result<Vec<(String, String)>, state::error::StateError> {
            self.inner.get_all_asset_albums(library).await
        }

        async fn get_all_asset_people(
            &self,
            library: &str,
        ) -> Result<Vec<(String, String)>, state::error::StateError> {
            self.inner.get_all_asset_people(library).await
        }

        async fn mark_soft_deleted(
            &self,
            library: &str,
            asset_id: &str,
            deleted_at: Option<chrono::DateTime<chrono::Utc>>,
        ) -> Result<(), state::error::StateError> {
            self.inner
                .mark_soft_deleted(library, asset_id, deleted_at)
                .await
        }

        async fn mark_hidden_at_source(
            &self,
            library: &str,
            asset_id: &str,
        ) -> Result<(), state::error::StateError> {
            self.inner.mark_hidden_at_source(library, asset_id).await
        }

        async fn record_metadata_write_failure(
            &self,
            library: &str,
            asset_id: &str,
            version_size: &str,
        ) -> Result<(), state::error::StateError> {
            self.inner
                .record_metadata_write_failure(library, asset_id, version_size)
                .await
        }

        async fn get_downloaded_metadata_hashes(
            &self,
        ) -> Result<
            std::collections::HashMap<(String, String, String), String>,
            state::error::StateError,
        > {
            self.inner.get_downloaded_metadata_hashes().await
        }

        async fn get_metadata_retry_markers(
            &self,
        ) -> Result<std::collections::HashSet<(String, String, String)>, state::error::StateError>
        {
            self.inner.get_metadata_retry_markers().await
        }

        async fn get_pending_metadata_rewrites(
            &self,
            limit: usize,
        ) -> Result<Vec<state::types::AssetRecord>, state::error::StateError> {
            self.inner.get_pending_metadata_rewrites(limit).await
        }

        async fn update_metadata_hash(
            &self,
            library: &str,
            asset_id: &str,
            version_size: &str,
            metadata_hash: &str,
        ) -> Result<(), state::error::StateError> {
            self.inner
                .update_metadata_hash(library, asset_id, version_size, metadata_hash)
                .await
        }

        async fn clear_metadata_write_failure(
            &self,
            library: &str,
            asset_id: &str,
            version_size: &str,
        ) -> Result<(), state::error::StateError> {
            self.inner
                .clear_metadata_write_failure(library, asset_id, version_size)
                .await
        }

        async fn has_downloaded_without_metadata_hash(
            &self,
        ) -> Result<bool, state::error::StateError> {
            self.inner.has_downloaded_without_metadata_hash().await
        }
    }

    /// `is_retry_failed=true` MUST force `SyncMode::Full` even when a
    /// sync token is stored. A regression that picked Incremental during
    /// retry-failed would silently skip the previously-failed assets the
    /// user explicitly asked to retry — silent data loss.
    #[tokio::test]
    async fn determine_sync_mode_retry_failed_with_token_returns_full() {
        let db = make_state_db();
        let sync_token_key = "sync_token:PrimarySync";
        // Pre-populate a stored token so we can verify it is ignored.
        db.set_metadata(sync_token_key, "stored-token-abc")
            .await
            .expect("set token");

        let mode = determine_sync_mode(
            true, // is_retry_failed
            1,
            Some(db.as_ref()),
            sync_token_key,
            "PrimarySync",
        )
        .await;

        assert!(
            matches!(mode, download::SyncMode::Full),
            "retry-failed must force Full, got {mode:?}"
        );
    }

    #[tokio::test]
    async fn determine_sync_mode_empty_stored_token_falls_back_to_full() {
        let db = make_state_db();
        let sync_token_key = "sync_token:PrimarySync";
        // Empty-string token is the malformed case — should be ignored.
        db.set_metadata(sync_token_key, "")
            .await
            .expect("set empty token");

        let mode =
            determine_sync_mode(false, 1, Some(db.as_ref()), sync_token_key, "PrimarySync").await;

        assert!(
            matches!(mode, download::SyncMode::Full),
            "empty stored token must yield Full, got {mode:?}"
        );

        // And a present, non-empty token must trigger Incremental — pin the
        // happy path here too so the empty-string check can't be dropped
        // without breaking both assertions.
        db.set_metadata(sync_token_key, "real-token")
            .await
            .expect("set real token");
        let mode =
            determine_sync_mode(false, 1, Some(db.as_ref()), sync_token_key, "PrimarySync").await;
        assert!(
            matches!(mode, download::SyncMode::Incremental { ref zone_sync_token } if zone_sync_token == "real-token"),
            "non-empty token must yield Incremental with that token, got {mode:?}"
        );
    }

    /// When the state DB read fails, fall back to Full rather than
    /// propagating. The watch loop must keep going even if sqlite hiccups —
    /// silently biasing toward Incremental on errors would mask data loss.
    ///
    /// The inline `FailingDb` implements only `SyncTokenStore`, so any future
    /// reroute inside `determine_sync_mode` has to either stay inside the
    /// token role or fail to compile.
    #[tokio::test]
    async fn determine_sync_mode_state_db_error_falls_back_to_full() {
        // Minimal failing impl: only `get_metadata` is reachable from
        // `determine_sync_mode`; the narrow role trait keeps any silent
        // reroute from compiling against this stub.
        struct FailingDb;

        #[async_trait::async_trait]
        impl state::SyncTokenStore for FailingDb {
            async fn get_metadata(
                &self,
                _: &str,
            ) -> Result<Option<String>, state::error::StateError> {
                Err(state::error::StateError::LockPoisoned("simulated".into()))
            }

            async fn set_metadata(&self, _: &str, _: &str) -> Result<(), state::error::StateError> {
                unimplemented!()
            }

            async fn delete_metadata_by_prefix(
                &self,
                _: &str,
            ) -> Result<u64, state::error::StateError> {
                unimplemented!()
            }

            async fn begin_enum_progress(&self, _: &str) -> Result<(), state::error::StateError> {
                unimplemented!()
            }

            async fn end_enum_progress(&self, _: &str) -> Result<(), state::error::StateError> {
                unimplemented!()
            }

            async fn list_interrupted_enumerations(
                &self,
            ) -> Result<Vec<String>, state::error::StateError> {
                unimplemented!()
            }
        }

        let db = FailingDb;

        let mode =
            determine_sync_mode(false, 1, Some(&db), "sync_token:PrimarySync", "PrimarySync").await;

        assert!(
            matches!(mode, download::SyncMode::Full),
            "DB read error must fall back to Full, got {mode:?}"
        );
    }

    /// Sanity: `state_db = None` (e.g. legacy run with no state path) must
    /// still produce Full. The match-arm exists in production today; pin
    /// it so a future refactor that drops the `else` branch tells us.
    #[tokio::test]
    async fn determine_sync_mode_no_state_db_returns_full() {
        let mode = determine_sync_mode::<state::SqliteStateDb>(
            false,
            1,
            None,
            "sync_token:PrimarySync",
            "PrimarySync",
        )
        .await;

        assert!(
            matches!(mode, download::SyncMode::Full),
            "no state DB must yield Full, got {mode:?}"
        );
    }

    // ── check_changes_database ───────────────────────────────────────
    //
    // Watch-mode wakes the sync loop on a fixed interval. The first thing
    // each cycle does is hit the `changes/database` endpoint to ask Apple
    // "anything actually changed?" If we mis-classify the response we
    // either hammer Apple uselessly (no changes but proceeded) or silently
    // skip a real delta (changes pending but skipped). Pin every branch.

    /// Build a `LibraryState` that's just enough for `check_changes_database`.
    /// The `plan` and `library` fields are unused by that function, so an
    /// empty plan + a stub library is safe.
    fn make_library_state(zone: &str, sync_token_key: &str) -> LibraryState {
        let stub_session = Box::new(
            crate::test_helpers::MockPhotosSession::new().ok(serde_json::json!({"records": []})),
        );
        LibraryState {
            library: crate::icloud::photos::PhotoLibrary::new_stub(stub_session),
            zone_name: zone.to_string(),
            sync_token_key: sync_token_key.to_string(),
            plan: crate::commands::AlbumPlan { passes: Vec::new() },
            plan_is_stale: false,
            plan_needs_refresh: false,
            cross_zone_libraries: Vec::new(),
        }
    }

    fn assert_proceed_changed(precheck: &WatchPrecheck, expected_zone: &str, expected_db: &str) {
        let WatchPrecheck::Proceed {
            changed_zones: Some(zones),
            db_sync_token_after_success: Some(db_token),
        } = precheck
        else {
            panic!("expected changed-zone proceed, got {precheck:?}");
        };
        assert_eq!(zones.len(), 1);
        assert!(
            zones.contains(expected_zone),
            "missing zone {expected_zone}"
        );
        assert_eq!(db_token, expected_db);
    }

    async fn check_single_library_changes_database(
        db: Option<&dyn state::StateDb>,
        lib_state: &LibraryState,
        svc: &mut crate::icloud::photos::PhotosService,
    ) -> WatchPrecheck {
        check_changes_database(db, std::slice::from_ref(lib_state), svc).await
    }

    /// `more_coming=true` with empty zones must NOT skip the cycle.
    /// Production logic: `if zones.is_empty() && !more_coming { skip }`.
    /// A regression that flipped the conjunction would silently skip every
    /// page-bearing wakeup -- silent loss of pending changes.
    #[tokio::test]
    async fn check_changes_database_more_coming_does_not_skip() {
        use serde_json::json;
        let session = crate::test_helpers::MockPhotosSession::new().ok(json!({
            "syncToken": "db-tok-2",
            "moreComing": true,
            "zones": []
        }));
        let mut svc = crate::icloud::photos::PhotosService::for_testing(
            Box::new(session),
            std::collections::HashMap::new(),
        );

        let db: Arc<dyn state::StateDb> = make_state_db();
        // Pre-populate a stored sync token so the function actually
        // makes the changes/database HTTP call (the `has_token` early
        // return otherwise short-circuits).
        db.set_metadata("sync_token:PrimarySync", "zone-tok-1")
            .await
            .expect("set token");

        let lib_state = make_library_state("PrimarySync", "sync_token:PrimarySync");

        let precheck =
            check_single_library_changes_database(Some(db.as_ref()), &lib_state, &mut svc).await;

        assert!(
            matches!(
                precheck,
                WatchPrecheck::Proceed {
                    changed_zones: None,
                    db_sync_token_after_success: Some(ref token)
                } if token == "db-tok-2"
            ),
            "more_coming=true must not skip the cycle (more pages pending)"
        );
        assert!(
            db.get_metadata(DB_SYNC_TOKEN_KEY)
                .await
                .expect("read db_sync_token")
                .is_none(),
            "more_coming=true must defer db_sync_token advancement until the sync cycle succeeds"
        );
    }

    /// Empty zones + `more_coming=false` must return `skip=true`.
    /// This is the optimistic short-circuit: Apple confirmed there are no
    /// pending changes, so we save a full enumeration cycle. A regression
    /// that flipped this branch would either burn a CloudKit query per
    /// idle wakeup (cost) or silently skip a real cycle (loss).
    #[tokio::test]
    async fn check_changes_database_empty_zones_skips_cycle() {
        use serde_json::json;
        let session = crate::test_helpers::MockPhotosSession::new().ok(json!({
            "syncToken": "db-tok-3",
            "moreComing": false,
            "zones": []
        }));
        let mut svc = crate::icloud::photos::PhotosService::for_testing(
            Box::new(session),
            std::collections::HashMap::new(),
        );

        let db: Arc<dyn state::StateDb> = make_state_db();
        db.set_metadata("sync_token:PrimarySync", "zone-tok-prev")
            .await
            .expect("set token");

        let lib_state = make_library_state("PrimarySync", "sync_token:PrimarySync");

        let precheck =
            check_single_library_changes_database(Some(db.as_ref()), &lib_state, &mut svc).await;

        assert_eq!(
            precheck,
            WatchPrecheck::SkipAll,
            "empty zones + more_coming=false must skip the cycle"
        );
        // The new db_sync_token must still be persisted even on skip:
        // otherwise the next call re-asks from scratch and we'd get an
        // unbounded list of all zones.
        let stored = db
            .get_metadata(DB_SYNC_TOKEN_KEY)
            .await
            .expect("read db_sync_token")
            .expect("token persisted on skip");
        assert_eq!(stored, "db-tok-3");
    }

    /// A non-empty zones list MUST NOT skip — even
    /// when more_coming=false. This is the real-work path; pinning it
    /// alongside the skip path catches a flipped branch in either
    /// direction.
    #[tokio::test]
    async fn check_changes_database_zone_changes_present_does_not_skip() {
        use serde_json::json;
        let session = crate::test_helpers::MockPhotosSession::new().ok(json!({
            "syncToken": "db-tok-4",
            "moreComing": false,
            "zones": [
                {"zoneID": {"zoneName": "PrimarySync"}, "syncToken": "ps-tok-new"}
            ]
        }));
        let mut svc = crate::icloud::photos::PhotosService::for_testing(
            Box::new(session),
            std::collections::HashMap::new(),
        );

        let db: Arc<dyn state::StateDb> = make_state_db();
        db.set_metadata("sync_token:PrimarySync", "zone-tok-prev")
            .await
            .expect("set token");

        let lib_state = make_library_state("PrimarySync", "sync_token:PrimarySync");

        let precheck =
            check_single_library_changes_database(Some(db.as_ref()), &lib_state, &mut svc).await;
        assert_proceed_changed(&precheck, "PrimarySync", "db-tok-4");
        assert!(
            db.get_metadata(DB_SYNC_TOKEN_KEY)
                .await
                .expect("read db_sync_token")
                .is_none(),
            "changed-zone precheck must not advance db_sync_token before the sync succeeds"
        );
    }

    #[tokio::test]
    async fn check_changes_database_multi_library_runs_only_changed_selected_zone() {
        use serde_json::json;
        let session = crate::test_helpers::MockPhotosSession::new().ok(json!({
            "syncToken": "db-tok-shared",
            "moreComing": false,
            "zones": [
                {"zoneID": {"zoneName": "SharedSync-ABCD"}, "syncToken": "shared-tok-new"}
            ]
        }));
        let mut svc = crate::icloud::photos::PhotosService::for_testing(
            Box::new(session),
            std::collections::HashMap::new(),
        );

        let db: Arc<dyn state::StateDb> = make_state_db();
        db.set_metadata("sync_token:PrimarySync", "primary-tok-prev")
            .await
            .expect("set primary token");
        db.set_metadata("sync_token:SharedSync-ABCD", "shared-tok-prev")
            .await
            .expect("set shared token");
        db.set_metadata(DB_SYNC_TOKEN_KEY, "db-tok-prev")
            .await
            .expect("set db token");

        let states = vec![
            make_library_state("PrimarySync", "sync_token:PrimarySync"),
            make_library_state("SharedSync-ABCD", "sync_token:SharedSync-ABCD"),
        ];

        let precheck = check_changes_database(Some(db.as_ref()), &states, &mut svc).await;
        assert_proceed_changed(&precheck, "SharedSync-ABCD", "db-tok-shared");
    }

    #[tokio::test]
    async fn check_changes_database_unselected_zone_change_skips_selected_libraries() {
        use serde_json::json;
        let session = crate::test_helpers::MockPhotosSession::new().ok(json!({
            "syncToken": "db-tok-unselected",
            "moreComing": false,
            "zones": [
                {"zoneID": {"zoneName": "SharedSync-ABCD"}, "syncToken": "shared-tok-new"}
            ]
        }));
        let mut svc = crate::icloud::photos::PhotosService::for_testing(
            Box::new(session),
            std::collections::HashMap::new(),
        );

        let db: Arc<dyn state::StateDb> = make_state_db();
        db.set_metadata("sync_token:PrimarySync", "primary-tok-prev")
            .await
            .expect("set primary token");
        db.set_metadata(DB_SYNC_TOKEN_KEY, "db-tok-prev")
            .await
            .expect("set db token");

        let states = vec![make_library_state("PrimarySync", "sync_token:PrimarySync")];

        let precheck = check_changes_database(Some(db.as_ref()), &states, &mut svc).await;
        assert_eq!(precheck, WatchPrecheck::SkipAll);
        let stored = db
            .get_metadata(DB_SYNC_TOKEN_KEY)
            .await
            .expect("read db_sync_token")
            .expect("token persisted");
        assert_eq!(stored, "db-tok-unselected");
    }

    /// No stored sync token at all must return false (don't
    /// skip) without making the HTTP call. Pinning this prevents a future
    /// refactor that flipped the early return from silently consuming an
    /// Apple call slot on bootstrap.
    #[tokio::test]
    async fn check_changes_database_no_stored_token_does_not_skip() {
        let session = crate::test_helpers::MockPhotosSession::new();
        let mut svc = crate::icloud::photos::PhotosService::for_testing(
            Box::new(session),
            std::collections::HashMap::new(),
        );

        // Empty DB — no `sync_token:PrimarySync` set.
        let db: Arc<dyn state::StateDb> = make_state_db();
        let lib_state = make_library_state("PrimarySync", "sync_token:PrimarySync");

        let precheck =
            check_single_library_changes_database(Some(db.as_ref()), &lib_state, &mut svc).await;
        assert!(
            matches!(
                precheck,
                WatchPrecheck::Proceed {
                    changed_zones: None,
                    db_sync_token_after_success: None
                }
            ),
            "no stored token must continue without a changes/database call"
        );
    }

    #[tokio::test]
    async fn check_changes_database_zone_token_read_failure_proceeds_without_precheck() {
        let session = crate::test_helpers::MockPhotosSession::new();
        let mut svc = crate::icloud::photos::PhotosService::for_testing(
            Box::new(session),
            std::collections::HashMap::new(),
        );
        let inner = make_state_db();
        inner
            .set_metadata("sync_token:PrimarySync", "zone-tok-prev")
            .await
            .expect("seed token");
        let db: Arc<dyn state::StateDb> = Arc::new(
            FailingMetadataSetDb::without_set_failure(inner, "simulated zone-token read failure")
                .with_get_failure(MetadataSetFailure::Exact("sync_token:PrimarySync")),
        );

        let lib_state = make_library_state("PrimarySync", "sync_token:PrimarySync");
        let precheck =
            check_single_library_changes_database(Some(db.as_ref()), &lib_state, &mut svc).await;

        assert_eq!(
            precheck,
            WatchPrecheck::proceed_all(),
            "metadata read failure should fall back to the safe full cycle path"
        );
    }

    #[tokio::test]
    async fn check_changes_database_db_token_read_failure_proceeds_without_precheck() {
        let session = crate::test_helpers::MockPhotosSession::new();
        let mut svc = crate::icloud::photos::PhotosService::for_testing(
            Box::new(session),
            std::collections::HashMap::new(),
        );
        let inner = make_state_db();
        inner
            .set_metadata("sync_token:PrimarySync", "zone-tok-prev")
            .await
            .expect("seed zone token");
        inner
            .set_metadata(DB_SYNC_TOKEN_KEY, "db-tok-prev")
            .await
            .expect("seed db token");
        let db: Arc<dyn state::StateDb> = Arc::new(
            FailingMetadataSetDb::without_set_failure(inner, "simulated db-token read failure")
                .with_get_failure(MetadataSetFailure::Exact(DB_SYNC_TOKEN_KEY)),
        );

        let lib_state = make_library_state("PrimarySync", "sync_token:PrimarySync");
        let precheck =
            check_single_library_changes_database(Some(db.as_ref()), &lib_state, &mut svc).await;

        assert_eq!(
            precheck,
            WatchPrecheck::proceed_all(),
            "db token read failure should fall back to the safe full cycle path"
        );
    }

    #[tokio::test]
    async fn refresh_needed_library_plans_filters_to_changed_zones() {
        let mut states = vec![
            make_library_state("PrimarySync", "sync_token:PrimarySync"),
            make_library_state("SharedSync-ABCD", "sync_token:SharedSync-ABCD"),
        ];
        for state in &mut states {
            state.plan_needs_refresh = true;
        }
        let mut changed_zones = rustc_hash::FxHashSet::default();
        changed_zones.insert("PrimarySync".to_string());
        let selection = crate::selection::Selection {
            albums: crate::selection::AlbumSelector::None,
            smart_folders: crate::selection::SmartFolderSelector::None,
            libraries: crate::selection::LibrarySelector::default(),
            unfiled: false,
        };
        let mut failures = 0;

        refresh_needed_library_plans(&mut states, &selection, Some(&changed_zones), &mut failures)
            .await;

        assert!(
            !states[0].plan_needs_refresh,
            "changed zone should refresh before syncing"
        );
        assert!(
            states[1].plan_needs_refresh,
            "unchanged zone must not refresh albums on this cycle"
        );
        assert_eq!(failures, 0);
    }

    #[tokio::test]
    async fn refresh_needed_library_plans_without_zone_filter_refreshes_every_stale_plan() {
        let mut states = vec![
            make_library_state("PrimarySync", "sync_token:PrimarySync"),
            make_library_state("SharedSync-ABCD", "sync_token:SharedSync-ABCD"),
        ];
        for state in &mut states {
            state.plan_needs_refresh = true;
            state.plan_is_stale = true;
        }
        let selection = crate::selection::Selection {
            albums: crate::selection::AlbumSelector::None,
            smart_folders: crate::selection::SmartFolderSelector::None,
            libraries: all_libraries(),
            unfiled: false,
        };
        let mut failures = 2;

        refresh_needed_library_plans(&mut states, &selection, None, &mut failures).await;

        assert!(
            states.iter().all(|state| !state.plan_needs_refresh),
            "every stale plan should refresh when no changed-zone precheck filtered the cycle"
        );
        assert!(
            states.iter().all(|state| !state.plan_is_stale),
            "successful refresh should clear stale-plan token gates"
        );
        assert_eq!(failures, 0);
    }

    /// A `set_metadata(DB_SYNC_TOKEN_KEY, ...)` write failure must
    /// NOT break the cycle. The current implementation logs a warning and
    /// continues. A regression that propagated the error would crash watch
    /// mode whenever a sqlite hiccup hit that single write.
    #[tokio::test]
    async fn check_changes_database_token_persist_failure_does_not_skip() {
        use serde_json::json;
        // Inner DB has the stored sync token so the changes/database call
        // is actually attempted.
        let inner = make_state_db();
        inner
            .set_metadata("sync_token:PrimarySync", "zone-tok-prev")
            .await
            .expect("seed token");
        let db: Arc<dyn state::StateDb> = Arc::new(FailingMetadataSetDb::new(
            inner,
            MetadataSetFailure::Exact(DB_SYNC_TOKEN_KEY),
            "simulated db_sync_token write failure",
        ));

        let session = crate::test_helpers::MockPhotosSession::new().ok(json!({
            "syncToken": "db-tok-bad-write",
            "moreComing": false,
            "zones": [
                {"zoneID": {"zoneName": "PrimarySync"}, "syncToken": "ps-tok-new"}
            ]
        }));
        let mut svc = crate::icloud::photos::PhotosService::for_testing(
            Box::new(session),
            std::collections::HashMap::new(),
        );

        let lib_state = make_library_state("PrimarySync", "sync_token:PrimarySync");

        // Zone changes hold db_sync_token advancement until after the sync
        // succeeds, so a db token write failure here must not affect the
        // pre-check decision.
        let precheck =
            check_single_library_changes_database(Some(db.as_ref()), &lib_state, &mut svc).await;
        assert_proceed_changed(&precheck, "PrimarySync", "db-tok-bad-write");
    }

    // ── preload_asset_groupings ──────────────────────────────────────
    //
    // `preload_asset_groupings` must be best-effort: a hiccup
    // loading people must NOT empty the albums map, and vice versa.
    // XMP-sidecar runs read this struct; biasing the entire grouping
    // empty would silently strip metadata from every downloaded photo.

    /// When `get_all_asset_albums` succeeds but
    /// `get_all_asset_people` fails, the result still includes albums.
    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn preload_asset_groupings_partial_people_failure_keeps_albums() {
        struct PartialDb {
            inner: Arc<dyn state::StateDb>,
        }

        #[async_trait::async_trait]
        impl state::MembershipStore for PartialDb {
            async fn add_asset_album(
                &self,
                library: &str,
                asset_id: &str,
                album_name: &str,
                source: &str,
            ) -> Result<(), state::error::StateError> {
                self.inner
                    .add_asset_album(library, asset_id, album_name, source)
                    .await
            }

            async fn get_all_asset_albums(
                &self,
                library: &str,
            ) -> Result<Vec<(String, String)>, state::error::StateError> {
                self.inner.get_all_asset_albums(library).await
            }

            async fn get_all_asset_people(
                &self,
                _: &str,
            ) -> Result<Vec<(String, String)>, state::error::StateError> {
                Err(state::error::StateError::LockPoisoned(
                    "simulated people-table read failure".into(),
                ))
            }
        }

        // Seed the inner DB with two album memberships across two assets,
        // so we can verify the surviving map is non-empty.
        let inner = make_state_db();
        inner
            .add_asset_album("PrimarySync", "ASSET_A", "Vacation", "icloud")
            .await
            .expect("add album A");
        inner
            .add_asset_album("PrimarySync", "ASSET_B", "Family", "icloud")
            .await
            .expect("add album B");

        let db = PartialDb { inner };

        let groupings = preload_asset_groupings(Some(&db), "PrimarySync").await;
        // Albums must survive intact.
        assert_eq!(
            groupings.albums.len(),
            2,
            "two assets with album memberships expected, got {}",
            groupings.albums.len()
        );
        assert!(groupings.albums.contains_key("ASSET_A"));
        assert!(groupings.albums.contains_key("ASSET_B"));
        // People map is empty (the read failed) — but the function still
        // returns Some groupings rather than panicking.
        assert!(
            groupings.people.is_empty(),
            "people map should be empty when its read failed; got {} entries",
            groupings.people.len()
        );
    }

    /// Companion: `state_db = None` returns an empty grouping struct.
    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn preload_asset_groupings_no_db_returns_empty() {
        let groupings = preload_asset_groupings::<state::SqliteStateDb>(None, "PrimarySync").await;
        assert!(groupings.albums.is_empty());
        assert!(groupings.people.is_empty());
    }

    // `should_store_sync_token` is the single decision gate
    // protecting the sync-token from being advanced after a partial sync or
    // a dry run. Both situations would lose change events on the next
    // incremental cycle ("user data is sacred"). The matrix below pins every
    // (outcome, dry_run) combination so a future refactor can't relax the
    // contract without a failing test.

    /// A partial download failure MUST NOT advance the stored sync
    /// token. Otherwise the next incremental sync would skip past the
    /// failed assets' change events and never retry them.
    #[test]
    fn sync_loop_partial_failure_does_not_advance_sync_token() {
        let outcome = download::DownloadOutcome::PartialFailure { failed_count: 3 };
        assert!(
            !should_store_sync_token(&outcome, false),
            "PartialFailure must NOT advance the sync token, even outside dry-run"
        );
        // dry_run=true cannot rescue a partial failure either.
        assert!(
            !should_store_sync_token(&outcome, true),
            "PartialFailure + dry_run must still NOT advance the sync token"
        );
    }

    /// `SessionExpired` is also a non-success outcome and
    /// MUST NOT advance the token. The cycle aborts mid-stream; the captured
    /// token may only reflect a subset of the work.
    #[test]
    fn sync_loop_session_expired_does_not_advance_sync_token() {
        let outcome = download::DownloadOutcome::SessionExpired {
            auth_error_count: 5,
        };
        assert!(!should_store_sync_token(&outcome, false));
        assert!(!should_store_sync_token(&outcome, true));
    }

    /// In `--dry-run`, even a fully-successful pass MUST NOT advance
    /// the token. Dry-run promises no DB writes that affect the next real
    /// sync; advancing the token would silently break the next incremental.
    #[test]
    fn sync_loop_dry_run_does_not_advance_sync_token() {
        let outcome = download::DownloadOutcome::Success;
        assert!(
            !should_store_sync_token(&outcome, true),
            "dry_run must NOT advance the sync token even on Success"
        );
    }

    /// Positive control: only the (Success, dry_run=false) combination
    /// advances the token. Pinning this branch prevents a future refactor
    /// from accidentally inverting the predicate.
    #[test]
    fn sync_loop_full_success_outside_dry_run_advances_sync_token() {
        let outcome = download::DownloadOutcome::Success;
        assert!(
            should_store_sync_token(&outcome, false),
            "(Success, dry_run=false) is the ONLY combination that should advance the token"
        );
    }

    /// A cycle that consumed a stale plan from a prior
    /// failed `resolve_passes` MUST NOT advance the sync token even when the
    /// per-library outcome is `Success`. A reused plan can route assets to
    /// the wrong pass; advancing the token would skip the change events
    /// that would surface the corrected membership on the next cycle.
    #[test]
    fn sync_loop_stale_plan_blocks_sync_token_advance_even_on_success() {
        let outcome = download::DownloadOutcome::Success;
        // Baseline: without a stale plan, Success advances the token.
        assert!(should_store_sync_token_for_cycle(&outcome, false, false));
        // With a stale plan: even Success must NOT advance the token.
        assert!(
            !should_store_sync_token_for_cycle(&outcome, false, true),
            "stale-plan flag must veto token advancement on Success"
        );
    }

    /// Stale-plan companion: dry_run and PartialFailure already block; pinning
    /// the matrix so a future refactor can't silently change the AND/OR
    /// shape of the gate.
    #[test]
    fn sync_loop_stale_plan_combines_with_existing_gates() {
        let success = download::DownloadOutcome::Success;
        let partial = download::DownloadOutcome::PartialFailure { failed_count: 1 };

        // PartialFailure: blocked regardless of stale-plan flag.
        assert!(!should_store_sync_token_for_cycle(&partial, false, false));
        assert!(!should_store_sync_token_for_cycle(&partial, false, true));

        // Dry-run: blocked regardless of stale-plan flag.
        assert!(!should_store_sync_token_for_cycle(&success, true, false));
        assert!(!should_store_sync_token_for_cycle(&success, true, true));

        // Only (Success, dry_run=false, stale=false) advances.
        assert!(should_store_sync_token_for_cycle(&success, false, false));
        assert!(!should_store_sync_token_for_cycle(&success, false, true));
    }

    #[tokio::test]
    async fn run_cycle_stale_plan_blocks_database_precheck_token() {
        let config = make_run_cycle_config();
        let db = make_state_db();
        db.set_metadata("sync_token:PrimarySync", "zone-tok-prev")
            .await
            .expect("seed zone token");
        let download_dir = tempfile::tempdir().expect("download tempdir");
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

        let mut lib_state =
            make_run_cycle_library_state("PrimarySync", "sync_token:PrimarySync", "zone-tok-new");
        lib_state.plan_is_stale = true;
        let states = vec![&lib_state];
        let build_download_config =
            make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));

        let result = run_cycle(
            &states,
            &config,
            Some(db.as_ref()),
            false,
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("run cycle");

        assert_eq!(result.failed_count, 0);
        assert!(
            !result.db_sync_token_advance_safe,
            "database precheck token must wait until stale plans stop suppressing zone tokens"
        );
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .expect("read zone token")
                .as_deref(),
            Some("zone-tok-prev"),
            "stale plan must leave the old zone token in place for replay"
        );
        assert!(!config.runtime.dry_run);
    }

    #[tokio::test]
    async fn run_cycle_zone_token_write_failure_blocks_database_precheck_token() {
        let config = make_run_cycle_config();
        let inner = make_state_db();
        inner
            .set_metadata("sync_token:PrimarySync", "zone-tok-prev")
            .await
            .expect("seed zone token");
        let db: Arc<dyn state::StateDb> = Arc::new(FailingMetadataSetDb::new(
            Arc::clone(&inner),
            MetadataSetFailure::Prefix(SYNC_TOKEN_PREFIX),
            "simulated sync-token write failure",
        ));
        let download_dir = tempfile::tempdir().expect("download tempdir");
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

        let lib_state =
            make_run_cycle_library_state("PrimarySync", "sync_token:PrimarySync", "zone-tok-new");
        let states = vec![&lib_state];
        let build_download_config =
            make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));

        let result = run_cycle(
            &states,
            &config,
            Some(db.as_ref()),
            false,
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("run cycle");

        assert_eq!(result.failed_count, 0);
        assert!(
            !result.db_sync_token_advance_safe,
            "database precheck token must not advance after a zone-token write failure"
        );
        assert_eq!(
            inner
                .get_metadata("sync_token:PrimarySync")
                .await
                .expect("read zone token")
                .as_deref(),
            Some("zone-tok-prev"),
            "failed zone-token write must leave old token in place for replay"
        );
    }

    #[tokio::test]
    async fn run_cycle_config_hash_purge_failure_forces_full_without_persisting_hash() {
        let config = make_run_cycle_config();
        let current_hash = download::compute_config_hash(&config);
        assert_ne!(current_hash, "old-hash");

        let inner = make_state_db();
        inner
            .set_metadata(ENUM_CONFIG_HASH_KEY, "old-hash")
            .await
            .expect("seed enum hash");
        inner
            .set_metadata("sync_token:PrimarySync", "zone-tok-prev")
            .await
            .expect("seed zone token");
        let db: Arc<dyn state::StateDb> = Arc::new(
            FailingMetadataSetDb::without_set_failure(
                Arc::clone(&inner),
                "simulated token purge failure",
            )
            .with_delete_prefix_failure(SYNC_TOKEN_PREFIX),
        );
        let download_dir = tempfile::tempdir().expect("download tempdir");
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

        let lib_state = make_run_cycle_library_state_with_album(
            "PrimarySync",
            "sync_token:PrimarySync",
            make_empty_full_album("zone-tok-new"),
        );
        let states = vec![&lib_state];
        let observed_modes = Arc::new(std::sync::Mutex::new(Vec::<download::SyncMode>::new()));
        let build_download_config = make_recording_run_cycle_download_config_builder(
            download_dir.path(),
            Arc::clone(&db),
            Arc::clone(&observed_modes),
        );

        let result = run_cycle(
            &states,
            &config,
            Some(db.as_ref()),
            false,
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("run cycle");

        assert_eq!(result.failed_count, 0);
        assert_eq!(
            observed_modes
                .lock()
                .expect("recorded modes lock")
                .as_slice(),
            &[download::SyncMode::Full],
            "config drift must not trust a surviving old incremental token in this cycle"
        );
        assert!(
            !result.db_sync_token_advance_safe,
            "database precheck token must not advance until config-hash invalidation can persist safely"
        );
        assert_eq!(
            inner
                .get_metadata(ENUM_CONFIG_HASH_KEY)
                .await
                .expect("read enum hash")
                .as_deref(),
            Some("old-hash"),
            "new hash must not be persisted when the stale-token purge failed"
        );
        assert_eq!(
            inner
                .get_metadata("sync_token:PrimarySync")
                .await
                .expect("read zone token")
                .as_deref(),
            Some("zone-tok-new"),
            "the forced full pass should still refresh the selected zone token after success"
        );
    }

    #[tokio::test]
    async fn run_cycle_interrupted_incremental_download_blocks_sync_token_advance() {
        let config = make_run_cycle_config();
        let inner = make_state_db();
        inner
            .set_metadata("sync_token:PrimarySync", "zone-tok-prev")
            .await
            .expect("seed zone token");
        let shutdown_token = CancellationToken::new();
        let db: Arc<dyn state::StateDb> = Arc::new(
            FailingMetadataSetDb::without_set_failure(Arc::clone(&inner), "unused")
                .with_cancel_on_upsert(shutdown_token.clone()),
        );
        let download_dir = tempfile::tempdir().expect("download tempdir");
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

        let lib_state = make_run_cycle_library_state_with_album(
            "PrimarySync",
            "sync_token:PrimarySync",
            make_one_photo_incremental_album_for_zone("PrimarySync", "zone-tok-new"),
        );
        let states = vec![&lib_state];
        let build_download_config =
            make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));

        let result = run_cycle(
            &states,
            &config,
            Some(db.as_ref()),
            false,
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &shutdown_token,
        )
        .await
        .expect("run cycle");

        assert_eq!(result.failed_count, 0);
        assert!(
            result.stats.interrupted,
            "cancellation before the download pass must be visible in cycle stats"
        );
        assert!(
            !result.db_sync_token_advance_safe,
            "database precheck token must not advance after an interrupted download cycle"
        );
        assert_eq!(
            inner
                .get_metadata("sync_token:PrimarySync")
                .await
                .expect("read zone token")
                .as_deref(),
            Some("zone-tok-prev"),
            "interrupted cycle must leave the old zone token in place for replay"
        );
        assert!(
            !download_dir.path().join("2023/11/14/photo.jpg").exists(),
            "test must not pass by completing the download before cancellation"
        );
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn run_cycle_destination_replaced_after_enumeration_reports_partial_failure() {
        let config = make_run_cycle_config();
        let inner = make_state_db();
        let download_dir = tempfile::tempdir().expect("download tempdir");
        let download_root = download_dir.path().to_path_buf();
        let db: Arc<dyn state::StateDb> = Arc::new(
            FailingMetadataSetDb::without_set_failure(Arc::clone(&inner), "unused")
                .with_download_dir_replaced_on_upsert(download_root.clone()),
        );
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
        let master_record_name = "master-mid-sync-destination-fault";
        let album = make_full_album_with_session(
            "PrimarySync",
            crate::test_helpers::MockPhotosSession::new()
                .ok(album_count_response(1))
                .ok(full_album_page_with_download(
                    "PrimarySync",
                    master_record_name,
                    "zone-tok-after-fault",
                    "https://p01.icloud-content.com/mid-sync-destination-unavailable.jpg",
                    8,
                    "AAAA",
                )),
        );
        let lib_state =
            make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
        let states = vec![&lib_state];
        let build_download_config =
            make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));

        let result = run_cycle(
            &states,
            &config,
            Some(db.as_ref()),
            false,
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("run cycle");

        assert_eq!(
            result.failed_count, 1,
            "mid-sync destination loss must produce a failed sync result"
        );
        assert_eq!(result.stats.failed, 1);
        assert_eq!(result.stats.downloaded, 0);
        assert!(
            !result.db_sync_token_advance_safe,
            "database precheck token must not advance after a download failure"
        );
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .expect("read zone token"),
            None,
            "partial sync must not store the post-fault zone token"
        );

        let failed = db.get_failed().await.expect("read failed assets");
        assert_eq!(failed.len(), 1, "failed asset should be persisted");
        let last_error = failed[0].last_error.as_deref().expect("failed asset error");
        assert!(
            last_error.contains("Failed to open temp download file")
                || last_error.contains("failed to create directory"),
            "failed asset error should name the failing filesystem operation, got: {last_error}"
        );
        let root = download_root.display().to_string();
        assert!(
            logs_contain("Download failed") && logs_contain(&root),
            "download failure log should include the target path context under {root}"
        );
        assert!(
            logs_contain("Download failed")
                && logs_contain(&format!("asset_id={master_record_name}")),
            "download failure log should include structured asset_id={master_record_name}"
        );

        if download_root.is_file() {
            std::fs::remove_file(&download_root).expect("remove injected download-root file");
        }
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn run_cycle_full_zero_assets_warns_once() {
        let result = run_empty_full_cycle(false).await;

        assert_eq!(result.failed_count, 0);
        assert_eq!(result.stats.assets_seen, 0);
        assert!(
            logs_contain(ZERO_ASSET_WARNING_PREFIX),
            "completed full sync with zero assets should be visible in normal logs"
        );
        assert!(
            logs_contain("library_count=1"),
            "zero-asset warning should carry structured library_count"
        );
        assert!(
            logs_contain("assets_seen=0"),
            "zero-asset warning should carry structured assets_seen"
        );
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn run_cycle_retry_failed_zero_assets_does_not_warn() {
        let result = run_empty_full_cycle(true).await;

        assert_eq!(result.failed_count, 0);
        assert_eq!(result.stats.assets_seen, 0);
        assert!(
            !logs_contain(ZERO_ASSET_WARNING_PREFIX),
            "retry-failed no-op cycles must stay quiet"
        );
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn run_cycle_incremental_fallback_zero_assets_warns() {
        let config = make_run_cycle_config();
        let db = make_state_db();
        db.set_metadata("sync_token:PrimarySync", "zone-tok-prev")
            .await
            .expect("seed sync token");
        let download_dir = tempfile::tempdir().expect("download tempdir");
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

        let lib_state = make_run_cycle_library_state_with_album(
            "PrimarySync",
            "sync_token:PrimarySync",
            make_empty_full_album("zone-tok-empty"),
        );
        let states = vec![&lib_state];
        let build_download_config = make_run_cycle_download_config_builder_with_options(
            download_dir.path(),
            Arc::clone(&db),
            RunCycleDownloadConfigOptions {
                per_pass_paths: true,
                ..RunCycleDownloadConfigOptions::default()
            },
        );

        let result = run_cycle(
            &states,
            &config,
            Some(db.as_ref()),
            false,
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("run cycle");

        assert_eq!(result.failed_count, 0);
        assert_eq!(result.stats.assets_seen, 0);
        assert!(
            logs_contain(ZERO_ASSET_WARNING_PREFIX),
            "incremental requests that fall back to full enumeration must warn"
        );
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn run_cycle_warns_for_empty_library_when_another_library_has_assets() {
        let config = make_run_cycle_config();
        let db = make_state_db();
        let download_dir = tempfile::tempdir().expect("download tempdir");
        let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

        let empty_state = make_run_cycle_library_state_with_album(
            "PrimarySync",
            "sync_token:PrimarySync",
            make_empty_full_album_for_zone("PrimarySync", "zone-tok-empty"),
        );
        let nonempty_state = make_run_cycle_library_state_with_album(
            "SharedSync-TEST",
            "sync_token:SharedSync-TEST",
            make_one_photo_full_album_for_zone("SharedSync-TEST", "zone-tok-one"),
        );
        let states = vec![&empty_state, &nonempty_state];
        let build_download_config = make_run_cycle_download_config_builder_with_options(
            download_dir.path(),
            Arc::clone(&db),
            RunCycleDownloadConfigOptions {
                media: config::MediaSelection {
                    photos: false,
                    videos: true,
                    live_photos: true,
                },
                ..RunCycleDownloadConfigOptions::default()
            },
        );

        let result = run_cycle(
            &states,
            &config,
            Some(db.as_ref()),
            false,
            &build_download_config,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .expect("run cycle");

        assert_eq!(result.failed_count, 0);
        assert_eq!(result.stats.assets_seen, 1);
        assert!(
            logs_contain(ZERO_ASSET_WARNING_PREFIX),
            "empty library must warn even when the cycle-wide asset count is nonzero"
        );
        assert!(
            logs_contain("library=PrimarySync"),
            "warning must name the empty library"
        );
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn run_cycle_dry_run_nonempty_full_cycle_does_not_warn() {
        let result =
            run_one_photo_full_cycle_with_controls(download::DownloadControls::dry_run_hidden())
                .await;

        assert_eq!(result.failed_count, 0);
        assert_eq!(result.stats.assets_seen, 0);
        assert!(
            !logs_contain(ZERO_ASSET_WARNING_PREFIX),
            "dry-run asset scans must not warn just because assets_seen stays zero"
        );
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn run_cycle_print_filenames_nonempty_full_cycle_does_not_warn() {
        let controls = download::DownloadControls::new(
            download::DownloadRunMode::PrintFilenames,
            download::DownloadReporting::hidden(),
        );
        let result = run_one_photo_full_cycle_with_controls(controls).await;

        assert_eq!(result.failed_count, 0);
        assert_eq!(result.stats.assets_seen, 0);
        assert!(
            !logs_contain(ZERO_ASSET_WARNING_PREFIX),
            "print-only asset scans must not warn just because assets_seen stays zero"
        );
    }

    #[tokio::test]
    async fn offline_replay_full_pass_reaches_sync_loop_planning_boundary() {
        let album = make_full_album_with_session(
            "PrimarySync",
            crate::test_helpers::MockPhotosFlow::new()
                .album_count(1)
                .query_photo_page("master-replay-sync-loop", Some("zone-tok-replay"))
                .empty_query_page(Some("zone-tok-replay"))
                .build(),
        );

        let result =
            run_full_cycle_with_album(album, false, download::DownloadControls::dry_run_hidden())
                .await;

        assert_eq!(result.failed_count, 0);
        assert_eq!(
            result.stats.downloaded, 1,
            "dry-run sync-loop replay must reach download planning"
        );
        assert_eq!(result.stats.failed, 0);
    }

    // Periodic reconciliation cadence. The watch loop calls
    // `should_reconcile_this_cycle` once per cycle to decide whether to walk
    // the state DB and warn on missing local files. Tests pin the cadence
    // so a future refactor can't silently disable the schedule.

    /// When `every_n` is `None`, the predicate must NEVER fire — this
    /// is the default-disabled behaviour for daemons that don't opt into
    /// periodic reconciliation.
    #[test]
    fn periodic_reconcile_disabled_when_every_n_is_none() {
        for cycle in [1u64, 2, 24, 1_000, u64::MAX] {
            assert!(
                !should_reconcile_this_cycle(cycle, None),
                "cycle {cycle} with every_n=None must NOT trigger reconciliation"
            );
        }
    }

    /// `Some(0)` is treated identically to `None` — the config
    /// resolver also filters this case, but the predicate is the load-bearing
    /// gate so we pin both spellings here.
    #[test]
    fn periodic_reconcile_disabled_when_every_n_is_zero() {
        for cycle in [1u64, 2, 24, 1_000] {
            assert!(
                !should_reconcile_this_cycle(cycle, Some(0)),
                "cycle {cycle} with every_n=Some(0) must NOT trigger"
            );
        }
    }

    /// The first firing must be at cycle == every_n, NOT cycle 0 or
    /// cycle 1. A freshly-started daemon must run at least one full sync
    /// before burning startup time on a state-DB walk.
    #[test]
    fn periodic_reconcile_first_fires_at_cycle_n_not_at_cycle_zero() {
        // every_n = 24: cycles 1..23 must NOT fire; cycle 24 fires.
        for cycle in 1u64..24 {
            assert!(
                !should_reconcile_this_cycle(cycle, Some(24)),
                "cycle {cycle} must NOT trigger when every_n=24"
            );
        }
        assert!(
            should_reconcile_this_cycle(24, Some(24)),
            "cycle 24 with every_n=24 must trigger"
        );
        // Cycle 0 is the pre-loop sentinel and must never fire even when
        // 0 is divisible by N.
        assert!(
            !should_reconcile_this_cycle(0, Some(24)),
            "cycle 0 (pre-loop sentinel) must NEVER trigger"
        );
    }

    /// Subsequent firings must repeat at every multiple of `every_n`.
    /// Pinning a few cycles past the first firing guards against an
    /// off-by-one that lets the cadence drift over a long run.
    #[test]
    fn periodic_reconcile_fires_on_every_multiple_of_n() {
        let n = 24;
        for &cycle in &[24u64, 48, 72, 240, 24_000] {
            assert!(
                should_reconcile_this_cycle(cycle, Some(n)),
                "cycle {cycle} (multiple of {n}) must trigger"
            );
        }
        for &cycle in &[25u64, 47, 49, 71, 73, 239, 241] {
            assert!(
                !should_reconcile_this_cycle(cycle, Some(n)),
                "cycle {cycle} (NOT a multiple of {n}) must NOT trigger"
            );
        }
    }

    /// `every_n=1` makes every cycle trigger reconciliation. Allowed
    /// (chatty but not a bug) and pinned because users debugging a drift
    /// suspicion are likely to set it to 1 temporarily.
    #[test]
    fn periodic_reconcile_every_one_fires_every_cycle() {
        for cycle in 1u64..=10 {
            assert!(
                should_reconcile_this_cycle(cycle, Some(1)),
                "cycle {cycle} with every_n=1 must trigger"
            );
        }
        // Sentinel still excluded.
        assert!(!should_reconcile_this_cycle(0, Some(1)));
    }

    // `should_wait_for_2fa` decides whether the reauth-time 2FA
    // branch parks the loop on a code prompt or surfaces the error. In
    // one-shot mode there is no operator at the keyboard; the error MUST
    // bubble up so cron / systemd / CI exits non-zero.

    /// A 2FA-required error in one-shot (`is_watch_mode = false`)
    /// MUST NOT cause the helper to return `true`. The caller will then
    /// surface the error to the user instead of blocking forever on a
    /// 2FA prompt that no one is watching.
    #[test]
    fn run_sync_2fa_required_in_one_shot_returns_error() {
        let err: anyhow::Error = auth::error::AuthError::TwoFactorRequired.into();
        assert!(
            !should_wait_for_2fa(false, &err),
            "one-shot + 2FA-required must surface the error, not park on a prompt"
        );
    }

    /// In watch mode the same error is recoverable;
    /// the helper returns `true` so the loop can park.
    #[test]
    fn run_sync_2fa_required_in_watch_mode_waits() {
        let err: anyhow::Error = auth::error::AuthError::TwoFactorRequired.into();
        assert!(
            should_wait_for_2fa(true, &err),
            "watch + 2FA-required must wait on a code"
        );
    }

    /// Negative control: any non-2FA error MUST surface in both modes.
    /// Otherwise the loop could silently swallow a real failure (e.g.
    /// `FailedLogin`) by treating it as "wait for a code that won't come".
    #[test]
    fn run_sync_non_2fa_error_never_waits() {
        let err: anyhow::Error = auth::error::AuthError::FailedLogin("bad password".into()).into();
        assert!(
            !should_wait_for_2fa(false, &err),
            "one-shot + non-2FA must surface"
        );
        assert!(
            !should_wait_for_2fa(true, &err),
            "watch + non-2FA must surface; do not park on a code that will never arrive"
        );
    }

    /// Negative control: a non-AuthError (e.g. an arbitrary anyhow error
    /// with no downcast target) MUST NOT be treated as 2FA. Without this
    /// guard the predicate could mis-park on transport errors.
    #[test]
    fn run_sync_non_auth_error_never_waits() {
        let err: anyhow::Error = anyhow::anyhow!("network unreachable");
        assert!(!should_wait_for_2fa(false, &err));
        assert!(!should_wait_for_2fa(true, &err));
    }

    // ── check_and_persist_enum_config_hash ─────────────────────────────

    #[tokio::test]
    async fn enum_config_hash_initial_persists_only() {
        let db = state::SqliteStateDb::open_in_memory().expect("open in-memory state DB");
        db.set_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"), "tok-abc")
            .await
            .expect("set token");

        let outcome = check_and_persist_enum_config_hash(&db, "hash-1").await;

        assert_eq!(outcome, EnumConfigHashOutcome::Initial);
        assert_eq!(
            db.get_metadata(ENUM_CONFIG_HASH_KEY)
                .await
                .unwrap()
                .as_deref(),
            Some("hash-1"),
        );
        // First run must NOT clear pre-existing sync tokens.
        assert_eq!(
            db.get_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"))
                .await
                .unwrap()
                .as_deref(),
            Some("tok-abc"),
        );
    }

    #[tokio::test]
    async fn enum_config_hash_drift_clears_tokens_and_persists() {
        let db = state::SqliteStateDb::open_in_memory().expect("open in-memory state DB");
        db.set_metadata(ENUM_CONFIG_HASH_KEY, "old-hash")
            .await
            .expect("seed old hash");
        db.set_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"), "tok-primary")
            .await
            .expect("seed primary token");
        db.set_metadata(
            &format!("{SYNC_TOKEN_PREFIX}SharedSync-AAAA1111"),
            "tok-shared",
        )
        .await
        .expect("seed shared token");

        let outcome = check_and_persist_enum_config_hash(&db, "new-hash").await;

        assert_eq!(outcome, EnumConfigHashOutcome::Changed);
        assert_eq!(
            db.get_metadata(ENUM_CONFIG_HASH_KEY)
                .await
                .unwrap()
                .as_deref(),
            Some("new-hash"),
        );
        // Every sync_token:* row must clear, including shared zones.
        assert!(db
            .get_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"))
            .await
            .unwrap()
            .is_none());
        assert!(db
            .get_metadata(&format!("{SYNC_TOKEN_PREFIX}SharedSync-AAAA1111"))
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn enum_config_hash_purge_failure_keeps_old_hash_and_tokens() {
        let inner = make_state_db();
        inner
            .set_metadata(ENUM_CONFIG_HASH_KEY, "old-hash")
            .await
            .expect("seed enum hash");
        inner
            .set_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"), "old-zone-token")
            .await
            .expect("seed zone token");
        let db: Arc<dyn state::StateDb> = Arc::new(
            FailingMetadataSetDb::without_set_failure(
                Arc::clone(&inner),
                "simulated token purge failure",
            )
            .with_delete_prefix_failure(SYNC_TOKEN_PREFIX),
        );

        let outcome = check_and_persist_enum_config_hash(db.as_ref(), "new-hash").await;

        assert_eq!(outcome, EnumConfigHashOutcome::ChangedTokenPurgeFailed);
        assert_eq!(
            inner
                .get_metadata(ENUM_CONFIG_HASH_KEY)
                .await
                .expect("read enum hash")
                .as_deref(),
            Some("old-hash"),
            "new hash must not be persisted while old sync tokens may still exist"
        );
        assert_eq!(
            inner
                .get_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"))
                .await
                .expect("read zone token")
                .as_deref(),
            Some("old-zone-token"),
            "the test must prove the stale token survived the failed purge"
        );
    }

    #[tokio::test]
    async fn enum_config_hash_unchanged_is_noop() {
        let db = state::SqliteStateDb::open_in_memory().expect("open in-memory state DB");
        db.set_metadata(ENUM_CONFIG_HASH_KEY, "stable-hash")
            .await
            .expect("seed stable hash");
        db.set_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"), "tok-keep")
            .await
            .expect("seed token");

        let outcome = check_and_persist_enum_config_hash(&db, "stable-hash").await;

        assert_eq!(outcome, EnumConfigHashOutcome::Unchanged);
        assert_eq!(
            db.get_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"))
                .await
                .unwrap()
                .as_deref(),
            Some("tok-keep"),
        );
    }

    /// When a previously-downloaded asset's local file exists but the
    /// asset is absent from the API response, kei must NOT delete the
    /// local file by default. This guards against a pagination bug
    /// being interpreted as mass remote deletion.
    #[tokio::test]
    async fn remote_deletion_local_file_preserved_per_default_keep_policy() {
        let db = state::SqliteStateDb::open_in_memory().expect("open in-memory state DB");
        let dir = tempfile::tempdir().unwrap();
        let local_path = dir.path().join("2025/06/15/deleted_asset.jpg");

        // Pre-seed: asset was previously downloaded
        tokio::fs::create_dir_all(local_path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&local_path, b"old photo bytes")
            .await
            .unwrap();
        let record = crate::test_helpers::TestAssetRecord::new("DEL_ASSET")
            .checksum("old_ck")
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "DEL_ASSET",
            "original",
            &local_path,
            "old_ck",
            None,
        )
        .await
        .unwrap();

        // Verify the asset is still marked as downloaded and the file
        // is recognized as present
        let should_dl = db
            .should_download(
                "PrimarySync",
                "DEL_ASSET",
                "original",
                "old_ck",
                &local_path,
            )
            .await
            .unwrap();
        assert!(!should_dl, "unchanged asset must not need download");
        assert!(
            local_path.exists(),
            "local file preserved per default policy"
        );
    }

    /// An empty remote library (zero assets) must exit cleanly with
    /// exit code 0 and a summary showing zero synced — not treated as
    /// an error or incomplete response.
    #[tokio::test]
    async fn empty_remote_library_summary_shows_zero_synced() {
        let db = state::SqliteStateDb::open_in_memory().expect("open in-memory state DB");
        let summary = db.get_summary().await.unwrap();
        // Fresh in-memory DB starts empty
        assert_eq!(summary.total_assets, 0, "empty DB must have zero assets");
        assert_eq!(summary.downloaded, 0, "empty DB must have zero downloaded");
    }
}
