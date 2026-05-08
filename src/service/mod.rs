//! Service-mode helpers consumed by `kei install`, `kei uninstall`, and
//! `kei service run`.
//!
//! Cross-platform plumbing (container detection, branding constants,
//! executable canonicalization) lives in `env`. The four dispatchers
//! (`install`, `uninstall`, `run`, `status`) route through
//! `cfg(target_os = ...)` to per-platform backends. Linux dispatches
//! to `linux`; macOS and Windows currently return a clean "not yet
//! implemented" error until those backends ship.

pub(crate) mod env;
pub(crate) mod install;
pub(crate) mod run;
pub(crate) mod status;
pub(crate) mod uninstall;

#[cfg(target_os = "linux")]
pub(crate) mod linux;
