//! `kei install` dispatcher.
//!
//! Routes to a per-platform backend (systemd on Linux, launchd on macOS,
//! Windows SCM on Windows) that registers kei to start at boot and run
//! continuously.
//!
//! Inside containers the command short-circuits: Docker / Kubernetes /
//! Podman supervise the process themselves, and writing a launchd plist
//! or systemd unit on the container's rootfs would never be invoked. The
//! existing `docker-compose.yml` workflow stays the supported path.

use std::path::Path;

use anyhow::Result;

use crate::cli::InstallArgs;
use crate::service::plan::{InstallPlan, InstallScope};

pub(crate) async fn run(args: InstallArgs, config_path: &Path) -> Result<()> {
    if crate::service::env::container_supervisor().is_some() {
        tracing::info!(
            "kei install is a no-op inside containers; \
             continue using docker-compose.yml to manage the daemon"
        );
        return Ok(());
    }

    let plan = InstallPlan::from_args(&args);
    dispatch(plan, config_path).await
}

#[cfg(target_os = "linux")]
async fn dispatch(plan: InstallPlan, config_path: &Path) -> Result<()> {
    use crate::service::linux;
    match plan.scope() {
        InstallScope::User => linux::install_user(plan, config_path).await,
        InstallScope::System => linux::install_system(plan, config_path).await,
    }
}

#[cfg(target_os = "macos")]
async fn dispatch(plan: InstallPlan, config_path: &Path) -> Result<()> {
    use crate::service::macos;
    match plan.scope() {
        InstallScope::User => macos::install_user(plan, config_path).await,
        InstallScope::System => macos::install_system(plan, config_path),
    }
}

#[cfg(target_os = "windows")]
async fn dispatch(plan: InstallPlan, config_path: &Path) -> Result<()> {
    use crate::service::windows;
    match plan.scope() {
        InstallScope::User => windows::install_user(plan, config_path).await,
        InstallScope::System => windows::install_system(plan, config_path),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
async fn dispatch(plan: InstallPlan, config_path: &Path) -> Result<()> {
    use crate::service::env::{current_executable, SERVICE_DESCRIPTION, SERVICE_IDENTIFIER};
    let exe = current_executable()?;
    tracing::info!(
        service = SERVICE_IDENTIFIER,
        description = SERVICE_DESCRIPTION,
        executable = %exe.display(),
        config = %config_path.display(),
        scope = ?plan.scope(),
        effect = ?plan.effect(),
        "preparing to install kei service",
    );
    Err(anyhow::anyhow!(
        "`kei install` is not available on this platform."
    ))
}
