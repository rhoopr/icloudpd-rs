//! `kei uninstall` dispatcher.
//!
//! Symmetric to [`crate::service::install`]: removes the per-platform
//! service registration and, with `--purge`, the state database,
//! configuration, and stored credentials. Container short-circuit
//! matches install — compose-managed deployments aren't touched.

use anyhow::Result;

use crate::cli::UninstallArgs;
pub(crate) async fn run(args: UninstallArgs) -> Result<()> {
    if crate::service::env::container_supervisor().is_some() {
        tracing::info!(
            "kei uninstall is a no-op inside containers; \
             stop and remove the docker-compose stack instead"
        );
        return Ok(());
    }

    dispatch(args).await
}

#[cfg(target_os = "linux")]
async fn dispatch(args: UninstallArgs) -> Result<()> {
    crate::service::linux::uninstall(&args).await
}

#[cfg(target_os = "macos")]
async fn dispatch(args: UninstallArgs) -> Result<()> {
    crate::service::macos::uninstall(&args).await
}

#[cfg(target_os = "windows")]
async fn dispatch(args: UninstallArgs) -> Result<()> {
    crate::service::windows::uninstall(&args).await
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
async fn dispatch(args: UninstallArgs) -> Result<()> {
    use crate::service::env::SERVICE_IDENTIFIER;
    tracing::info!(
        service = SERVICE_IDENTIFIER,
        purge = args.purge,
        "preparing to uninstall kei service",
    );
    Err(anyhow::anyhow!(
        "`kei uninstall` is not available on this platform."
    ))
}
