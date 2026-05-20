//! Shared install policy for `kei install`.
//!
//! Platform backends render and apply service-manager artifacts. This module
//! owns the CLI contract that decides which install shape was requested,
//! whether it is preview-only, and which system-scope requests are safe to
//! proceed with.

use anyhow::{bail, Result};

use crate::cli::InstallArgs;

#[cfg(target_os = "linux")]
use std::path::Path;

#[cfg(target_os = "linux")]
use crate::service::env::effective_uid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InstallScope {
    User,
    System,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InstallEffect {
    Preview,
    Apply,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct InstallPlan {
    scope: InstallScope,
    effect: InstallEffect,
}

impl InstallPlan {
    pub(crate) fn from_args(args: &InstallArgs) -> Self {
        let scope = if args.system {
            InstallScope::System
        } else {
            InstallScope::User
        };
        let effect = if args.dry_run {
            InstallEffect::Preview
        } else {
            InstallEffect::Apply
        };
        Self { scope, effect }
    }

    pub(crate) fn scope(self) -> InstallScope {
        self.scope
    }

    #[cfg(any(
        test,
        target_os = "linux",
        not(any(target_os = "linux", target_os = "macos", target_os = "windows"))
    ))]
    pub(crate) fn effect(self) -> InstallEffect {
        self.effect
    }

    pub(crate) fn is_preview(self) -> bool {
        self.effect == InstallEffect::Preview
    }
}

#[allow(
    dead_code,
    reason = "platform backend modules are compiled cross-platform; Windows builds do not call the macOS rejection helper"
)]
pub(crate) fn reject_macos_system_install() -> Result<()> {
    bail!(
        "macOS only ships a per-user LaunchAgent; \
         rerun without --system (or with --user) to install."
    )
}

pub(crate) fn reject_windows_system_install() -> Result<()> {
    bail!(
        "`kei install --system` is not supported on Windows; \
         use `kei install` (per-user) so the service shares your Credential Manager vault \
         and `~/.config/kei` state directory"
    )
}

#[cfg(target_os = "linux")]
pub(crate) fn linux_system_user(plan: InstallPlan, config_path: &Path) -> Result<String> {
    debug_assert_eq!(
        plan.scope(),
        InstallScope::System,
        "linux_system_user is only valid for system installs"
    );
    match plan.effect() {
        InstallEffect::Preview => linux_system_preview_user(),
        InstallEffect::Apply => {
            ensure_linux_system_apply_is_root(config_path)?;
            linux_sudo_user()
        }
    }
}

#[cfg(target_os = "linux")]
fn ensure_linux_system_apply_is_root(config_path: &Path) -> Result<()> {
    if effective_uid() == 0 {
        return Ok(());
    }
    bail!(
        "`kei install --system` must be run as root (EUID=0); \
         rerun:  sudo kei install --system --config {}  \
         Or use `kei install --user` for a per-user install.",
        config_path.display()
    )
}

#[cfg(target_os = "linux")]
fn linux_sudo_user() -> Result<String> {
    if let Some(user) = non_root_env_user("SUDO_USER") {
        return Ok(user);
    }
    bail!(
        "$SUDO_USER not set; rerun via `sudo kei install --system` so the service \
         can be configured to run as your account rather than root"
    )
}

#[cfg(target_os = "linux")]
fn linux_system_preview_user() -> Result<String> {
    if let Some(user) = non_root_env_user("SUDO_USER") {
        return Ok(user);
    }

    if effective_uid() == 0 {
        bail!(
            "$SUDO_USER not set; rerun via `sudo kei install --system --dry-run` so the service \
             can be previewed for your account rather than root"
        );
    }

    for key in ["USER", "LOGNAME"] {
        if let Some(user) = non_root_env_user(key) {
            return Ok(user);
        }
    }
    bail!(
        "could not determine which user the system unit would run as; \
         set SUDO_USER or USER before running `kei install --system --dry-run`"
    )
}

#[cfg(target_os = "linux")]
fn non_root_env_user(key: &str) -> Option<String> {
    non_root_user_value(std::env::var(key).ok())
}

#[cfg(target_os = "linux")]
fn non_root_user_value(value: Option<String>) -> Option<String> {
    value.filter(|user| is_valid_linux_run_user(user))
}

#[cfg(target_os = "linux")]
fn is_valid_linux_run_user(user: &str) -> bool {
    !user.is_empty()
        && user != "root"
        && !user.starts_with('-')
        && user
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(system: bool, dry_run: bool) -> InstallArgs {
        InstallArgs {
            user: !system,
            system,
            dry_run,
        }
    }

    #[test]
    fn default_install_is_user_apply() {
        let plan = InstallPlan::from_args(&args(false, false));
        assert_eq!(plan.scope(), InstallScope::User);
        assert_eq!(plan.effect(), InstallEffect::Apply);
        assert!(!plan.is_preview());
    }

    #[test]
    fn dry_run_system_install_is_preview_without_artifact_writes() {
        let plan = InstallPlan::from_args(&args(true, true));
        assert_eq!(plan.scope(), InstallScope::System);
        assert_eq!(plan.effect(), InstallEffect::Preview);
        assert!(plan.is_preview());
    }

    #[test]
    fn unsupported_platform_system_errors_live_in_policy() {
        assert!(reject_macos_system_install()
            .expect_err("macOS system install must fail")
            .to_string()
            .contains("per-user LaunchAgent"));
        assert!(reject_windows_system_install()
            .expect_err("Windows system install must fail")
            .to_string()
            .contains("not supported on Windows"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_preview_user_rejects_empty_and_root_values() {
        assert_eq!(non_root_user_value(None), None);
        assert_eq!(non_root_user_value(Some(String::new())), None);
        assert_eq!(non_root_user_value(Some("root".to_string())), None);
        assert_eq!(non_root_user_value(Some("-alice".to_string())), None);
        assert_eq!(non_root_user_value(Some("alice bob".to_string())), None);
        assert_eq!(
            non_root_user_value(Some("alice\nEnvironment=KEI_INJECTED=1".to_string())),
            None
        );
        assert_eq!(
            non_root_user_value(Some("alice".to_string())),
            Some("alice".to_string())
        );
        assert_eq!(
            non_root_user_value(Some("alice.sync-1".to_string())),
            Some("alice.sync-1".to_string())
        );
    }
}
