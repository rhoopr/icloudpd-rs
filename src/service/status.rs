//! `kei service status` dispatcher.
//!
//! Reports whether kei is registered as a service on this host and how
//! long it has been running. Per-platform queries (`systemctl --user
//! show`, `launchctl print`, `Get-Service`) live in the per-platform
//! backend modules. Linux is wired up; macOS and Windows currently
//! return a placeholder error rather than a misleading "not installed".

use anyhow::Result;

pub(crate) async fn run() -> Result<()> {
    dispatch().await
}

#[cfg(target_os = "linux")]
async fn dispatch() -> Result<()> {
    crate::service::linux::status().await
}

#[cfg(not(target_os = "linux"))]
async fn dispatch() -> Result<()> {
    Err(anyhow::anyhow!(
        "`kei service status` is not yet implemented on this platform; \
         macOS launchd and Windows SCM backends are still in flight"
    ))
}
