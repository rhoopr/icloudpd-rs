//! Windows backend for `kei install` / `kei uninstall` /
//! `kei service status` plus the SCM service-main bridge for
//! `kei service run`.
//!
//! Registers kei with the Windows Service Control Manager (SCM) under a
//! per-user account. On install we prompt for the operator's Windows
//! login password via rpassword and pass it to `CreateServiceW` as the
//! LSA secret; SCM launches the daemon under that user's profile so the
//! Credential Manager vault and `~/.config/kei` data dir match the
//! interactive login. Same convention as the macOS / linux backends.
//!
//! Domain users / roaming profiles are documented as a v0.14 limitation
//! (the `.\<user>` LSA-secret form covers local-machine accounts only).
//!
//! `kei service run` on Windows does double duty: when SCM launches the
//! binary it must call `StartServiceCtrlDispatcher` within 30s or SCM
//! kills the process. [`run_under_scm_or_foreground`] tries the
//! dispatcher first; on `ERROR_FAILED_SERVICE_CONTROLLER_CONNECT` it
//! falls through to a foreground sync-loop run for `kei service run`
//! invoked from a terminal.
//!
//! The PR 8 smoke matrix is what exercises a full install -> stop ->
//! uninstall round-trip on a real Windows runner. Locally on linux/macOS
//! only the renderer + status-formatter helpers compile and run; those
//! carry inline unit tests so a regression in their shape is caught
//! before the windows-latest job ever sees the change.

#![allow(
    clippy::print_stdout,
    reason = "kei service status renders human-readable output to stdout, matching kei status / kei verify."
)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};

use crate::cli::{InstallArgs, UninstallArgs};
use crate::service::env::{
    current_executable, purge_kei_state, SERVICE_DESCRIPTION, SERVICE_IDENTIFIER,
};

/// SCM service name (matches `SERVICE_IDENTIFIER` so `sc.exe query
/// com.rhoopr.kei` works for ad-hoc inspection).
pub(crate) const SERVICE_NAME: &str = SERVICE_IDENTIFIER;

/// Restart-on-failure cadence applied via `ChangeServiceConfig2W`.
const RESTART_DELAY: Duration = Duration::from_secs(10);

/// Window over which SCM counts crashes against the failure action list.
const FAILURE_RESET_PERIOD: Duration = Duration::from_secs(86_400);

/// Subdirectory used everywhere: linux honours XDG, macOS hard-codes
/// `~/.config/kei`, Windows mirrors macOS so the operator sees a
/// consistent path across platforms (and so `kei uninstall --purge`
/// removes the same tree `kei sync` populated).
const KEI_STATE_SUBDIR: &str = ".config/kei";

// ── Public surface ──────────────────────────────────────────────────────

/// Top-level entry for `kei install` (also the bare default; on Windows
/// `--user` and the default both produce the same per-user SCM entry).
pub(crate) async fn install_user(args: &InstallArgs, config_path: &Path) -> Result<()> {
    let exe = current_executable()?;
    let user = current_user_name()
        .context("could not resolve current Windows user (USERNAME / USERPROFILE unset?)")?;

    let inputs = ServiceInfoInputs {
        exec: &exe,
        config: config_path,
        account_user: &user,
    };

    if args.dry_run {
        let preview = render_service_info_preview(&inputs);
        tracing::info!(
            service = SERVICE_NAME,
            executable = %exe.display(),
            config = %config_path.display(),
            account = %inputs.account_name(),
            dry_run = true,
            "previewing kei service registration",
        );
        for line in preview.lines() {
            println!("{line}");
        }
        return Ok(());
    }

    let password = prompt_windows_password(&user)?;
    scm_impl::install(&inputs, &password).await?;

    tracing::info!(
        service = SERVICE_NAME,
        executable = %exe.display(),
        config = %config_path.display(),
        account = %inputs.account_name(),
        "registered kei with the Windows Service Control Manager",
    );
    Ok(())
}

/// `--system` is rejected here for the same reason as macOS: a true
/// system-wide install would mean LocalSystem (no user keyring) or a
/// virtual `NT SERVICE\kei` account (no Credential Manager). Both
/// would break the cross-platform "credentials follow the operator"
/// promise from the v0.14 phase-1 plan.
pub(crate) async fn install_system(_args: &InstallArgs, _config_path: &Path) -> Result<()> {
    bail!(
        "`kei install --system` is not supported on Windows; \
         use `kei install` (per-user) so the service shares your Credential Manager vault \
         and `~/.config/kei` state directory"
    )
}

/// Top-level entry for `kei uninstall`.
pub(crate) async fn uninstall(args: &UninstallArgs) -> Result<()> {
    match scm_impl::uninstall_existing().await? {
        true => tracing::info!(service = SERVICE_NAME, "removed kei from SCM"),
        false => tracing::info!(
            service = SERVICE_NAME,
            "no kei service registered with SCM; nothing to remove",
        ),
    }

    if args.purge {
        let kei_dir = kei_state_dir().ok_or_else(|| {
            anyhow!("--purge requested but USERPROFILE is not set; cannot locate kei state")
        })?;
        purge_kei_state(&kei_dir, &[])?;
    }

    Ok(())
}

/// `kei service status` implementation.
pub(crate) async fn status() -> Result<()> {
    let line = render_status(scm_impl::probe().await?);
    println!("{line}");
    Ok(())
}

/// `kei service run` entry on Windows.
///
/// Tries SCM dispatcher first. When the binary is launched by SCM, the
/// dispatcher attaches and blocks here for the lifetime of the service;
/// when launched from a terminal, dispatcher returns
/// `ERROR_FAILED_SERVICE_CONTROLLER_CONNECT` immediately and we fall
/// through to a foreground `sync_loop::run_sync` so `kei service run`
/// stays useful for local testing.
#[cfg(target_os = "windows")]
pub(crate) async fn run_under_scm_or_foreground(
    globals: crate::config::GlobalArgs,
    args: crate::sync_loop::SyncArgs,
) -> Result<()> {
    scm_impl::run_or_foreground(globals, args).await
}

// ── Inputs / pure renderers (testable on every target) ──────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ServiceInfoInputs<'a> {
    pub exec: &'a Path,
    pub config: &'a Path,
    pub account_user: &'a str,
}

impl ServiceInfoInputs<'_> {
    fn launch_arguments(&self) -> Vec<OsString> {
        vec![
            OsString::from("service"),
            OsString::from("run"),
            OsString::from("--config"),
            self.config.as_os_str().to_owned(),
        ]
    }

    fn account_name(&self) -> String {
        format!(r".\{}", self.account_user)
    }
}

/// Human-readable preview of the registration `--dry-run` would create.
/// Same lines `sc.exe qc com.rhoopr.kei` would surface post-install, in
/// the same order, so the operator can eyeball the install before
/// committing.
fn render_service_info_preview(inputs: &ServiceInfoInputs<'_>) -> String {
    let argv = std::iter::once(inputs.exec.display().to_string())
        .chain(
            inputs
                .launch_arguments()
                .iter()
                .map(|a| a.to_string_lossy().into_owned()),
        )
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "Service name        : {SERVICE_NAME}\n\
         Display name        : {SERVICE_DESCRIPTION}\n\
         Description         : {SERVICE_DESCRIPTION}\n\
         Account             : {account}\n\
         Service type        : OWN_PROCESS\n\
         Start type          : AUTO_START\n\
         Error control       : NORMAL\n\
         Failure actions     : restart x3, delay {delay}s, reset after {reset}s\n\
         Binary path         : {argv}",
        account = inputs.account_name(),
        delay = RESTART_DELAY.as_secs(),
        reset = FAILURE_RESET_PERIOD.as_secs(),
    )
}

/// Inputs the status renderer accepts. Decoupled from the
/// windows-service crate's `ServiceStatus` so the formatter stays
/// testable on linux/macOS hosts where that type does not compile.
#[derive(Clone, Debug, PartialEq, Eq)]
enum StatusInputs {
    NotInstalled,
    ScmUnavailable,
    Probed {
        state: &'static str,
        pid: Option<u32>,
    },
}

fn render_status(inputs: StatusInputs) -> String {
    match inputs {
        StatusInputs::NotInstalled => "Service: not installed".to_string(),
        StatusInputs::ScmUnavailable => {
            // SCM is unavailable when called from a non-elevated shell or
            // (rare) when the Service Control Manager itself is down.
            // Either way, surface the cause; "not installed" would lie.
            "Service: SCM unavailable (run from an elevated PowerShell to query state)".to_string()
        }
        StatusInputs::Probed { state, pid } => match (state, pid) {
            ("running", Some(pid)) => format!("Service: running (windows scm, pid {pid})"),
            ("running", None) => "Service: running (windows scm)".to_string(),
            (state, _) => format!("Service: {state} (windows scm)"),
        },
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────

fn current_user_name() -> Option<String> {
    if let Ok(u) = std::env::var("USERNAME") {
        if !u.is_empty() {
            return Some(u);
        }
    }
    // USERPROFILE is `C:\Users\Alice`; the basename is the account name.
    let profile = std::env::var("USERPROFILE").ok()?;
    Path::new(&profile)
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
}

fn kei_state_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(KEI_STATE_SUBDIR))
}

fn prompt_windows_password(user: &str) -> Result<String> {
    let prompt = format!(
        "Windows password for {user} (used by SCM to launch kei under your account; \
         stored as an LSA secret, not in kei): "
    );
    rpassword::prompt_password(prompt).context("failed to read Windows password from stdin")
}

// ── SCM glue ───────────────────────────────────────────────────────────
//
// The real impl is `#[cfg(target_os = "windows")]`. Stubs on the other
// arms exist so the renderer/tests above compile and run on linux/macOS;
// the runtime arm bails because the dispatch tables in install.rs /
// uninstall.rs / status.rs route only Windows targets here.

#[cfg(target_os = "windows")]
mod scm_impl {
    use super::*;
    use std::sync::{Mutex, OnceLock};
    use windows_service::{
        define_windows_service,
        service::{
            ServiceAccess, ServiceAction, ServiceActionType, ServiceControl, ServiceControlAccept,
            ServiceErrorControl, ServiceExitCode, ServiceFailureActions, ServiceFailureResetPeriod,
            ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
        },
        service_control_handler::{self, ServiceControlHandlerResult},
        service_dispatcher,
        service_manager::{ServiceManager, ServiceManagerAccess},
    };

    /// 1060 = ERROR_SERVICE_DOES_NOT_EXIST.
    const WINAPI_SERVICE_DOES_NOT_EXIST: i32 = 1060;
    /// 1063 = ERROR_FAILED_SERVICE_CONTROLLER_CONNECT — what
    /// `StartServiceCtrlDispatcher` returns when the binary is run from a
    /// terminal instead of by SCM.
    const WINAPI_NOT_RUNNING_AS_SERVICE: i32 = 1063;

    /// Bridge between the async caller in `service::run::run` and the
    /// SCM service-main callback that runs on a thread spawned by the
    /// windows-service dispatcher. The OS thread that `kei_service_main`
    /// runs on cannot capture our async-context payload by closure (the
    /// callback signature is fixed by the FFI contract), so we stash it
    /// in this static, take it inside the callback, and the foreground
    /// fall-through path takes it back when the dispatcher refuses.
    static SCM_PAYLOAD: OnceLock<Mutex<Option<ScmPayload>>> = OnceLock::new();

    /// Channel sender published by the service-main thread once it has
    /// registered its event handler. The SCM stop event is delivered on
    /// a separate OS thread, so the handler signals shutdown by sending
    /// on this channel which the sync-loop runtime observes via select!.
    static SCM_SHUTDOWN_TX: OnceLock<Mutex<Option<tokio::sync::oneshot::Sender<()>>>> =
        OnceLock::new();

    struct ScmPayload {
        globals: crate::config::GlobalArgs,
        sync: crate::sync_loop::SyncArgs,
    }

    fn payload_slot() -> &'static Mutex<Option<ScmPayload>> {
        SCM_PAYLOAD.get_or_init(|| Mutex::new(None))
    }

    fn shutdown_slot() -> &'static Mutex<Option<tokio::sync::oneshot::Sender<()>>> {
        SCM_SHUTDOWN_TX.get_or_init(|| Mutex::new(None))
    }

    pub(super) async fn install(inputs: &ServiceInfoInputs<'_>, password: &str) -> Result<()> {
        let exec = inputs.exec.to_path_buf();
        let config = inputs.config.to_path_buf();
        let account_user = inputs.account_user.to_string();
        let password = password.to_owned();
        tokio::task::spawn_blocking(move || {
            install_blocking(&exec, &config, &account_user, &password)
        })
        .await
        .context("install task panicked")?
    }

    fn install_blocking(
        exec: &Path,
        config: &Path,
        account_user: &str,
        password: &str,
    ) -> Result<()> {
        let manager =
            open_manager(ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)?;
        let inputs = ServiceInfoInputs {
            exec,
            config,
            account_user,
        };
        let info = ServiceInfo {
            name: OsString::from(SERVICE_NAME),
            display_name: OsString::from(SERVICE_DESCRIPTION),
            service_type: ServiceType::OWN_PROCESS,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: exec.to_path_buf(),
            launch_arguments: inputs.launch_arguments(),
            dependencies: Vec::new(),
            account_name: Some(OsString::from(inputs.account_name())),
            account_password: Some(OsString::from(password)),
        };
        let service = manager
            .create_service(
                &info,
                ServiceAccess::CHANGE_CONFIG | ServiceAccess::START | ServiceAccess::QUERY_STATUS,
            )
            .context("CreateServiceW failed (run from an elevated PowerShell?)")?;
        service
            .set_description(SERVICE_DESCRIPTION)
            .context("ChangeServiceConfig2W (description) failed")?;
        service
            .update_failure_actions(ServiceFailureActions {
                reset_period: ServiceFailureResetPeriod::After(FAILURE_RESET_PERIOD),
                reboot_msg: None,
                command: None,
                actions: Some(vec![
                    ServiceAction {
                        action_type: ServiceActionType::Restart,
                        delay: RESTART_DELAY,
                    };
                    3
                ]),
            })
            .context("ChangeServiceConfig2W (failure actions) failed")?;
        let no_args: &[&str] = &[];
        service.start(no_args).context("StartServiceW failed")?;
        Ok(())
    }

    pub(super) async fn uninstall_existing() -> Result<bool> {
        tokio::task::spawn_blocking(uninstall_blocking)
            .await
            .context("uninstall task panicked")?
    }

    fn uninstall_blocking() -> Result<bool> {
        let manager = open_manager(ServiceManagerAccess::CONNECT)?;
        let access = ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE;
        let service = match manager.open_service(SERVICE_NAME, access) {
            Ok(s) => s,
            Err(windows_service::Error::Winapi(ref e))
                if e.raw_os_error() == Some(WINAPI_SERVICE_DOES_NOT_EXIST) =>
            {
                return Ok(false);
            }
            Err(e) => return Err(anyhow!("OpenServiceW failed: {e}")),
        };

        if let Ok(status) = service.query_status() {
            if status.current_state != ServiceState::Stopped {
                let _ = service.stop();
                wait_for_stop(&service, Duration::from_secs(30));
            }
        }
        service.delete().context("DeleteService failed")?;
        Ok(true)
    }

    fn wait_for_stop(service: &windows_service::service::Service, timeout: Duration) {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            match service.query_status() {
                Ok(status) if status.current_state == ServiceState::Stopped => return,
                Ok(_) => std::thread::sleep(Duration::from_millis(250)),
                Err(_) => return,
            }
        }
    }

    pub(super) async fn probe() -> Result<StatusInputs> {
        tokio::task::spawn_blocking(probe_blocking)
            .await
            .context("status probe task panicked")?
    }

    fn probe_blocking() -> Result<StatusInputs> {
        let manager = match open_manager(ServiceManagerAccess::CONNECT) {
            Ok(m) => m,
            Err(_) => return Ok(StatusInputs::ScmUnavailable),
        };
        let service = match manager.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS) {
            Ok(s) => s,
            Err(windows_service::Error::Winapi(ref e))
                if e.raw_os_error() == Some(WINAPI_SERVICE_DOES_NOT_EXIST) =>
            {
                return Ok(StatusInputs::NotInstalled);
            }
            Err(e) => return Err(anyhow!("OpenServiceW failed: {e}")),
        };
        let status = service
            .query_status()
            .context("QueryServiceStatusEx failed")?;
        Ok(StatusInputs::Probed {
            state: scm_state_label(status.current_state),
            pid: status.process_id,
        })
    }

    fn open_manager(access: ServiceManagerAccess) -> Result<ServiceManager> {
        ServiceManager::local_computer(None::<&str>, access).context(
            "OpenSCManagerW failed -- `kei install` and `kei uninstall` on Windows require an \
             elevated PowerShell or Command Prompt",
        )
    }

    fn scm_state_label(state: ServiceState) -> &'static str {
        match state {
            ServiceState::Running => "running",
            ServiceState::Stopped => "stopped",
            ServiceState::StartPending => "start-pending",
            ServiceState::StopPending => "stop-pending",
            ServiceState::ContinuePending => "continue-pending",
            ServiceState::PausePending => "pause-pending",
            ServiceState::Paused => "paused",
        }
    }

    // -- Service main / SCM event handler --------------------------------

    define_windows_service!(ffi_service_main, kei_service_main);

    pub(super) async fn run_or_foreground(
        globals: crate::config::GlobalArgs,
        sync: crate::sync_loop::SyncArgs,
    ) -> Result<()> {
        // Stash payload so the SCM-spawned service-main thread can take
        // it; this happens *before* the dispatcher attempt so the OS
        // thread can never observe an empty slot.
        *payload_slot().lock().unwrap() = Some(ScmPayload { globals, sync });

        let dispatcher_result = tokio::task::spawn_blocking(|| {
            service_dispatcher::start(SERVICE_NAME, ffi_service_main)
        })
        .await
        .context("SCM dispatcher task panicked")?;

        match dispatcher_result {
            Ok(()) => {
                // SCM took over and the service ran to completion.
                tracing::info!(service = SERVICE_NAME, "SCM-managed service exited cleanly");
                Ok(())
            }
            Err(windows_service::Error::Winapi(ref e))
                if e.raw_os_error() == Some(WINAPI_NOT_RUNNING_AS_SERVICE) =>
            {
                // Foreground invocation -- e.g. operator running
                // `kei service run` from PowerShell. Recover the
                // payload and run the sync loop directly.
                tracing::info!(
                    service = SERVICE_NAME,
                    "kei service run invoked outside SCM; running in foreground"
                );
                let payload =
                    payload_slot().lock().unwrap().take().ok_or_else(|| {
                        anyhow!("internal: SCM payload missing on foreground path")
                    })?;
                crate::sync_loop::run_sync(&payload.globals, payload.sync).await
            }
            Err(e) => Err(anyhow!("StartServiceCtrlDispatcher failed: {e}")),
        }
    }

    fn kei_service_main(_arguments: Vec<OsString>) {
        if let Err(e) = service_main_inner() {
            tracing::error!(error = %e, "kei SCM service main failed");
        }
    }

    fn service_main_inner() -> Result<()> {
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        *shutdown_slot().lock().unwrap() = Some(shutdown_tx);

        let event_handler = move |control_event| -> ServiceControlHandlerResult {
            match control_event {
                ServiceControl::Stop | ServiceControl::Shutdown => {
                    if let Some(tx) = shutdown_slot().lock().unwrap().take() {
                        let _ = tx.send(());
                    }
                    ServiceControlHandlerResult::NoError
                }
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                _ => ServiceControlHandlerResult::NotImplemented,
            }
        };

        let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)
            .context("RegisterServiceCtrlHandlerExW failed")?;

        status_handle
            .set_service_status(running_status())
            .context("set_service_status(running) failed")?;

        let outcome = run_payload_under_scm(shutdown_rx);

        // Always report Stopped, even on error -- otherwise SCM keeps
        // the service in StartPending until the wait_hint elapses and
        // then force-kills the process, which is a worse signal than a
        // clean stop with a non-zero exit code.
        let report_result = status_handle.set_service_status(stopped_status(outcome.is_ok()));
        if let Err(e) = report_result {
            tracing::error!(error = %e, "failed to report Stopped to SCM");
        }
        outcome
    }

    fn run_payload_under_scm(mut shutdown_rx: tokio::sync::oneshot::Receiver<()>) -> Result<()> {
        let payload = payload_slot()
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| anyhow!("kei service main started without a stashed payload"))?;

        // Dedicated single-thread runtime for the sync loop. Lives on
        // this OS thread (the one SCM spawned for service main); the
        // foreground caller's runtime, if any, is on a different thread
        // and unaffected.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to build SCM-mode tokio runtime")?;

        runtime.block_on(async move {
            tokio::select! {
                result = crate::sync_loop::run_sync(&payload.globals, payload.sync) => result,
                _ = &mut shutdown_rx => {
                    tracing::info!(service = SERVICE_NAME, "SCM stop received; shutting down sync loop");
                    Ok(())
                }
            }
        })
    }

    fn running_status() -> ServiceStatus {
        ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Running,
            controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        }
    }

    fn stopped_status(success: bool) -> ServiceStatus {
        ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Stopped,
            controls_accepted: ServiceControlAccept::empty(),
            exit_code: ServiceExitCode::Win32(if success { 0 } else { 1 }),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        }
    }
}

#[cfg(not(target_os = "windows"))]
mod scm_impl {
    use super::*;

    pub(super) async fn install(_inputs: &ServiceInfoInputs<'_>, _password: &str) -> Result<()> {
        bail!(
            "internal error: Windows install path reached on a non-Windows target; \
             this is a build configuration bug"
        )
    }

    pub(super) async fn uninstall_existing() -> Result<bool> {
        Ok(false)
    }

    pub(super) async fn probe() -> Result<StatusInputs> {
        Ok(StatusInputs::NotInstalled)
    }
}

// ── Tests ──────────────────────────────────────────────────────────────
//
// Pure renderer / formatter coverage. These run on every unix host as
// well as on Windows so a regression in shape (preview lines, status
// strings) is caught on linux CI before windows-latest sees the change.

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn sample_inputs() -> ServiceInfoInputs<'static> {
        static EXE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        static CFG: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        let exe = EXE.get_or_init(|| PathBuf::from(r"C:\Program Files\kei\kei.exe"));
        let cfg = CFG.get_or_init(|| PathBuf::from(r"C:\Users\Alice\.config\kei\config.toml"));
        ServiceInfoInputs {
            exec: exe.as_path(),
            config: cfg.as_path(),
            account_user: "Alice",
        }
    }

    #[test]
    fn launch_arguments_pass_service_run_with_config() {
        let argv = sample_inputs().launch_arguments();
        let strs: Vec<String> = argv
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(strs[0], "service");
        assert_eq!(strs[1], "run");
        assert_eq!(strs[2], "--config");
        assert_eq!(strs[3], r"C:\Users\Alice\.config\kei\config.toml");
    }

    #[test]
    fn account_name_uses_local_machine_prefix() {
        // `.\Alice` is the SCM "local machine, account Alice" form.
        // Domain users (`DOMAIN\Alice`) are out of scope for v0.14.
        assert_eq!(sample_inputs().account_name(), r".\Alice");
    }

    #[test]
    fn render_service_info_preview_lists_every_field() {
        // SERVICE_NAME / SERVICE_DESCRIPTION are platform-resolved
        // constants -- on linux SERVICE_IDENTIFIER is "kei", on
        // macOS/Windows it is "com.rhoopr.kei". Reference the constant
        // in the assertion so the test passes on every host that runs
        // it (linux + macOS + windows; the Windows-resolved string is
        // what the smoke matrix verifies against `sc.exe qc`).
        let preview = render_service_info_preview(&sample_inputs());
        for needle in [
            format!("Service name        : {SERVICE_NAME}"),
            format!("Display name        : {SERVICE_DESCRIPTION}"),
            format!("Description         : {SERVICE_DESCRIPTION}"),
            r"Account             : .\Alice".to_string(),
            "Service type        : OWN_PROCESS".to_string(),
            "Start type          : AUTO_START".to_string(),
            "Error control       : NORMAL".to_string(),
            "Failure actions     : restart x3, delay 10s, reset after 86400s".to_string(),
            r"Binary path         : C:\Program Files\kei\kei.exe service run --config C:\Users\Alice\.config\kei\config.toml".to_string(),
        ] {
            assert!(
                preview.contains(&needle),
                "expected preview to contain {needle:?}; got:\n{preview}"
            );
        }
    }

    #[test]
    fn render_status_reports_not_installed() {
        assert_eq!(
            render_status(StatusInputs::NotInstalled),
            "Service: not installed"
        );
    }

    #[test]
    fn render_status_reports_scm_unavailable() {
        let line = render_status(StatusInputs::ScmUnavailable);
        assert!(line.starts_with("Service: SCM unavailable"));
        // Operator hint about elevation belongs in the same line so a
        // tail -f / log-collector picks it up alongside the verdict.
        assert!(line.contains("elevated PowerShell"));
    }

    #[test]
    fn render_status_includes_pid_when_running() {
        let line = render_status(StatusInputs::Probed {
            state: "running",
            pid: Some(4321),
        });
        assert_eq!(line, "Service: running (windows scm, pid 4321)");
    }

    #[test]
    fn render_status_running_without_pid_is_still_running() {
        // SCM occasionally returns process_id = None during the
        // start-pending -> running transition; we should not lose the
        // "running" verdict just because the pid was racing.
        let line = render_status(StatusInputs::Probed {
            state: "running",
            pid: None,
        });
        assert_eq!(line, "Service: running (windows scm)");
    }

    #[test]
    fn render_status_renders_non_running_states() {
        for state in [
            "stopped",
            "start-pending",
            "stop-pending",
            "paused",
            "continue-pending",
            "pause-pending",
        ] {
            let line = render_status(StatusInputs::Probed {
                state,
                pid: Some(1),
            });
            assert_eq!(line, format!("Service: {state} (windows scm)"));
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn current_user_falls_back_to_userprofile_basename() {
        // Direct unit test of the parsing shape used by the fallback
        // path. We don't mutate process env on every CI host; this
        // covers the basename extraction independently. Gated to
        // Windows because `Path::file_name` only treats '\' as a
        // separator on Windows targets -- on linux the same input
        // round-trips as a single filename.
        let basename = Path::new(r"C:\Users\Alice")
            .file_name()
            .map(|f| f.to_string_lossy().into_owned());
        assert_eq!(basename.as_deref(), Some("Alice"));
    }

    #[test]
    fn kei_state_subdir_matches_macos_and_linux_convention() {
        // The other backends hard-code `~/.config/kei`. Regression-guard
        // against a casual edit that would split Windows off into
        // `%APPDATA%\kei`.
        assert_eq!(KEI_STATE_SUBDIR, ".config/kei");
    }
}
