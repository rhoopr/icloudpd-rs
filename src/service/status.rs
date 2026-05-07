//! `kei service status` dispatcher.
//!
//! Reports whether kei is registered as a service on this host and how
//! long it has been running. The platform queries (`systemctl --user
//! show`, `launchctl print`, `Get-Service`) live in the per-platform
//! backend modules introduced by PR 3+; until those land this command
//! returns a placeholder error rather than a misleading "not installed".

use anyhow::{anyhow, Result};

pub(crate) async fn run() -> Result<()> {
    Err(anyhow!(
        "`kei service status` is not yet implemented; \
         status reporting lands alongside the per-platform install backends"
    ))
}
