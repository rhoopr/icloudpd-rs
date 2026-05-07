//! Service-mode helpers consumed by `kei install`, `kei uninstall`, and
//! `kei service run`.
//!
//! Cross-platform plumbing (container detection, branding constants,
//! executable canonicalization) lives here. Per-platform installers
//! (launchd, systemd, Windows SCM) land in follow-up PRs that slot into
//! the dispatch tables introduced alongside the CLI surface.

pub(crate) mod env;
