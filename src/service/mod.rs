//! Service-mode helpers consumed by `kei install`, `kei uninstall`, and
//! `kei service run`.
//!
//! Cross-platform plumbing (container detection, branding constants,
//! executable canonicalization) lives in `env`. The four dispatchers
//! (`install`, `uninstall`, `run`, `status`) route through
//! `cfg(target_os = ...)` to per-platform backends -- linux, macos, or
//! windows.

pub(crate) mod env;
pub(crate) mod install;
pub(crate) mod run;
pub(crate) mod status;
pub(crate) mod uninstall;

#[cfg(target_os = "linux")]
pub(crate) mod linux;

// Compiled on every unix target (linux + macOS) -- the renderer + parser
// are pure and their inline unit tests pick up regressions on linux CI
// before macos-latest ever sees them. Windows is excluded because
// `effective_uid` (POSIX `geteuid`) and `tokio::process::Command` calls
// to `launchctl` have no analogue there. Only the `kei install` /
// `kei uninstall` / `kei service status` dispatchers are runtime-gated
// to macOS -- see install.rs / uninstall.rs / status.rs.
#[cfg(unix)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) mod macos;

// Compiled on every target -- pure renderer / formatter helpers carry
// inline unit tests that run on linux + macOS, and only the
// windows-service FFI surface is `#[cfg(target_os = "windows")]`-gated
// inside the module. As with macos, the dispatchers in install.rs /
// uninstall.rs / status.rs / run.rs runtime-gate to Windows targets.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) mod windows;
