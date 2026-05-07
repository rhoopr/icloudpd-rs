//! Service-mode helpers consumed by `kei install`, `kei uninstall`, and
//! `kei service run`.
//!
//! Cross-platform plumbing (container detection, branding constants,
//! executable canonicalization) lives in `env`. The four dispatchers
//! (`install`, `uninstall`, `run`, `status`) currently route through
//! `cfg(target_os = ...)` to per-platform backends that do not yet
//! exist; they return a clean "not yet implemented" error until PR 3+
//! land launchd / systemd / Windows SCM support.

pub(crate) mod env;
pub(crate) mod install;
pub(crate) mod run;
pub(crate) mod status;
pub(crate) mod uninstall;
