//! `kei install` dispatcher.
//!
//! Routes to a per-platform backend (launchd on macOS, systemd on Linux,
//! Windows SCM on Windows) that registers kei to start at boot and run
//! continuously. Linux and macOS are wired up; Windows currently returns
//! a clean "not yet implemented" error.
//!
//! Inside containers the command short-circuits: Docker / Kubernetes /
//! Podman supervise the process themselves, and writing a launchd plist
//! or systemd unit on the container's rootfs would never be invoked. The
//! existing `docker-compose.yml` workflow stays the supported path.

use std::path::Path;

use anyhow::Result;

use crate::cli::InstallArgs;
use crate::service::env::is_in_container;

pub(crate) async fn run(args: InstallArgs, config_path: &Path) -> Result<()> {
    if is_in_container() {
        tracing::info!(
            "kei install is a no-op inside containers; \
             continue using docker-compose.yml to manage the daemon"
        );
        return Ok(());
    }

    dispatch(args, config_path).await
}

#[cfg(target_os = "linux")]
async fn dispatch(args: InstallArgs, config_path: &Path) -> Result<()> {
    use crate::service::linux;
    if args.system {
        linux::install_system(&args, config_path).await
    } else {
        linux::install_user(&args, config_path).await
    }
}

#[cfg(target_os = "macos")]
async fn dispatch(args: InstallArgs, config_path: &Path) -> Result<()> {
    use crate::service::macos;
    if args.system {
        macos::install_system(&args, config_path).await
    } else {
        macos::install_user(&args, config_path).await
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
async fn dispatch(args: InstallArgs, config_path: &Path) -> Result<()> {
    use crate::service::env::{current_executable, SERVICE_DESCRIPTION, SERVICE_IDENTIFIER};
    let exe = current_executable()?;
    tracing::info!(
        service = SERVICE_IDENTIFIER,
        description = SERVICE_DESCRIPTION,
        executable = %exe.display(),
        config = %config_path.display(),
        user = args.user,
        system = args.system,
        dry_run = args.dry_run,
        "preparing to install kei service",
    );
    Err(anyhow::anyhow!(
        "`kei install` is not yet implemented on this platform; \
         the Windows SCM backend is still in flight"
    ))
}
