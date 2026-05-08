//! `kei service status` dispatcher.
//!
//! Reports whether kei is registered as a service on this host and how
//! long it has been running. Per-platform queries (`systemctl --user
//! show`, `launchctl print`, `Get-Service`) live in the per-platform
//! backend modules. Linux and macOS are wired up; Windows currently
//! returns a placeholder error rather than a misleading "not installed".

use anyhow::Result;

pub(crate) async fn run() -> Result<()> {
    dispatch().await
}

#[cfg(target_os = "linux")]
async fn dispatch() -> Result<()> {
    crate::service::linux::status().await
}

#[cfg(target_os = "macos")]
async fn dispatch() -> Result<()> {
    crate::service::macos::status().await
}

#[cfg(target_os = "windows")]
async fn dispatch() -> Result<()> {
    crate::service::windows::status().await
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
async fn dispatch() -> Result<()> {
    Err(anyhow::anyhow!(
        "`kei service status` is not implemented on this platform"
    ))
}
