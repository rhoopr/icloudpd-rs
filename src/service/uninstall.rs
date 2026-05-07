//! `kei uninstall` dispatcher.
//!
//! Symmetric to [`crate::service::install`]: removes the per-platform
//! service registration and, with `--purge`, the state database,
//! configuration, and stored credentials. Container short-circuit
//! matches install — compose-managed deployments aren't touched.

use anyhow::{anyhow, Result};

use crate::cli::UninstallArgs;
use crate::service::env::{is_in_container, SERVICE_IDENTIFIER};

pub(crate) async fn run(args: UninstallArgs) -> Result<()> {
    if is_in_container() {
        tracing::info!(
            "kei uninstall is a no-op inside containers; \
             stop and remove the docker-compose stack instead"
        );
        return Ok(());
    }

    tracing::info!(
        service = SERVICE_IDENTIFIER,
        purge = args.purge,
        "preparing to uninstall kei service",
    );

    Err(anyhow!(
        "`kei uninstall` is not yet implemented on this platform; \
         per-platform support lands alongside `kei install` in follow-up PRs"
    ))
}
