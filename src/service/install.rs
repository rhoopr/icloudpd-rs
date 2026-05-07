//! `kei install` dispatcher.
//!
//! Routes to a per-platform backend (launchd on macOS, systemd on Linux,
//! Windows SCM on Windows) that registers kei to start at boot and run
//! continuously. PR 3 lands the Linux backend, PR 4 macOS, PR 5 Windows;
//! until then every platform returns a clean "not yet implemented" error.
//!
//! Inside containers the command short-circuits: Docker / Kubernetes /
//! Podman supervise the process themselves, and writing a launchd plist
//! or systemd unit on the container's rootfs would never be invoked. The
//! existing `docker-compose.yml` workflow stays the supported path.

use std::path::Path;

use anyhow::{anyhow, Result};

use crate::cli::InstallArgs;
use crate::service::env::{
    current_executable, is_in_container, SERVICE_DESCRIPTION, SERVICE_IDENTIFIER,
};

pub(crate) async fn run(args: InstallArgs, config_path: &Path) -> Result<()> {
    if is_in_container() {
        tracing::info!(
            "kei install is a no-op inside containers; \
             continue using docker-compose.yml to manage the daemon"
        );
        return Ok(());
    }

    let exe = current_executable()?;
    tracing::info!(
        service = SERVICE_IDENTIFIER,
        description = SERVICE_DESCRIPTION,
        executable = %exe.display(),
        config = %config_path.display(),
        user = args.user,
        system = args.system,
        "preparing to install kei service",
    );

    Err(anyhow!(
        "`kei install` is not yet implemented on this platform; \
         per-platform installers land in follow-up PRs (Linux first)"
    ))
}
