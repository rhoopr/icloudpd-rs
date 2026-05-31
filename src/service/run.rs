//! `kei service run` worker entry point.
//!
//! The thin wrapper invoked by launchd, systemd, and Windows SCM (and
//! by users directly, for testing). Identical to `kei sync` except:
//!
//! - `service_mode` is propagated to [`crate::sync_loop::run_sync`], so
//!   when no other source supplies a watch interval the daemon falls
//!   through to [`crate::sync_loop::SERVICE_MODE_DEFAULT_WATCH_INTERVAL`]
//!   instead of running once and exiting.
//! - The canonical executable path is logged on entry. Service files
//!   reference an absolute exec path; surfacing it in the log lets
//!   operators confirm the registered binary is the one running.
//! - On Windows the entry double-dispatches: SCM-launched invocations
//!   route through [`crate::service::windows::run_under_scm_or_foreground`]
//!   so `StartServiceCtrlDispatcher` fires within the 30s SCM watchdog;
//!   foreground invocations fall through to the same `sync_loop::run_sync`
//!   path other platforms use.

use anyhow::Result;

use crate::config;
use crate::service::env::{current_executable, SERVICE_IDENTIFIER};
use crate::sync_loop;

pub(crate) async fn run(globals: &config::GlobalArgs, args: sync_loop::SyncArgs) -> Result<()> {
    let exe = current_executable()?;
    tracing::info!(
        service = SERVICE_IDENTIFIER,
        executable = %exe.display(),
        "starting kei service worker",
    );

    #[cfg(target_os = "windows")]
    {
        return crate::service::windows::run_under_scm_or_foreground(globals.clone(), args).await;
    }

    #[cfg(not(target_os = "windows"))]
    Box::pin(sync_loop::run_sync(globals, args)).await
}
