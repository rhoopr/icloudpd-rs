//! Cross-platform helpers shared by every service-install backend.
//!
//! `is_in_container` lets `kei install` no-op cleanly inside Docker so
//! containerized deployments keep using the existing compose workflow
//! instead of attempting to register a launchd/systemd/SCM service that
//! the host would never invoke. `service_identifier` and
//! `SERVICE_DESCRIPTION` are the branding constants every backend writes
//! into its registry entry. `current_executable` returns a canonical
//! absolute path so service files survive PATH changes, working-directory
//! shifts (Windows SCM starts services in `C:\Windows\System32`), and
//! symlinked installs (Homebrew Cellar -> /usr/local/bin).

// PR 2 (CLI surface for `kei install` / `kei uninstall` / `kei service run`)
// is what wires these helpers into actual call sites. Until that lands,
// every item below has no production consumer in the lib build and would
// trip `dead_code`. The unit tests at the bottom of this file exercise
// each item; the allow can come off as soon as PR 2 merges.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Marker file Docker creates inside every container.
const DEFAULT_DOCKERENV_PATH: &str = "/.dockerenv";

const DEFAULT_CGROUP_PATH: &str = "/proc/1/cgroup";

/// Substrings in `/proc/1/cgroup` that signal a container runtime.
/// Covers Docker, containerd (Kubernetes worker default), Kubernetes
/// pod cgroup hierarchy, Podman, and LXC. Only consulted on Linux.
#[cfg(target_os = "linux")]
const CONTAINER_CGROUP_MARKERS: &[&str] = &["docker", "containerd", "kubepods", "podman", "lxc"];

/// Human-readable description shown in service-manager registries.
pub(crate) const SERVICE_DESCRIPTION: &str = "kei Media Sync Engine";

/// Service identifier used by the per-platform installer.
///
/// Reverse-DNS form on macOS (launchd `Label`) and Windows (SCM service
/// name); plain `kei` on Linux (systemd unit file basename).
pub(crate) const SERVICE_IDENTIFIER: &str = if cfg!(target_os = "linux") {
    "kei"
} else {
    "com.rhoopr.kei"
};

/// Returns `true` when the current process is running inside a container.
///
/// Two-signal probe: the marker file Docker drops at `/.dockerenv`, then
/// the cgroup membership of PID 1 (covers Podman, containerd, Kubernetes,
/// LXC). Cgroup parsing is Linux-only; on macOS and Windows only the
/// marker-file check runs, which catches Docker Desktop edge cases
/// without needing platform-specific container APIs.
pub(crate) fn is_in_container() -> bool {
    is_in_container_at(
        Path::new(DEFAULT_DOCKERENV_PATH),
        Path::new(DEFAULT_CGROUP_PATH),
    )
}

/// Path-injecting form of [`is_in_container`] used by unit tests.
///
/// Production callers go through [`is_in_container`]; tests pass tempdir
/// paths so the result is deterministic regardless of where the test
/// binary runs.
pub(crate) fn is_in_container_at(dockerenv: &Path, cgroup: &Path) -> bool {
    if dockerenv.exists() {
        return true;
    }

    #[cfg(target_os = "linux")]
    {
        match std::fs::read_to_string(cgroup) {
            Ok(contents) => CONTAINER_CGROUP_MARKERS
                .iter()
                .any(|marker| contents.contains(marker)),
            Err(_) => false,
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = cgroup;
        false
    }
}

/// Canonical absolute path to the running `kei` executable.
///
/// Service unit files / launchd plists / SCM entries reference an
/// absolute path so they keep working after `PATH` changes or
/// working-directory shifts. `std::env::current_exe()` may return a
/// path containing symlinks (e.g. Homebrew's Cellar shim);
/// `canonicalize` resolves those so the registry entry points at the
/// real binary.
pub(crate) fn current_executable() -> Result<PathBuf> {
    let raw = std::env::current_exe().context("failed to resolve current executable path")?;
    raw.canonicalize()
        .with_context(|| format!("failed to canonicalize executable path {}", raw.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn service_identifier_matches_platform_branding() {
        if cfg!(target_os = "linux") {
            assert_eq!(SERVICE_IDENTIFIER, "kei");
        } else {
            assert_eq!(SERVICE_IDENTIFIER, "com.rhoopr.kei");
        }
    }

    #[test]
    fn service_description_is_pinned() {
        // Phase-1 plan locked this string. Regression-guard so a casual
        // rename doesn't slip past review and orphan every installed
        // service entry on user machines.
        assert_eq!(SERVICE_DESCRIPTION, "kei Media Sync Engine");
    }

    #[test]
    fn dockerenv_marker_short_circuits_cgroup_check() {
        let tmp = TempDir::new().unwrap();
        let dockerenv = tmp.path().join("dockerenv");
        fs::write(&dockerenv, "").unwrap();
        let cgroup = tmp.path().join("cgroup");
        fs::write(&cgroup, "").unwrap();
        assert!(is_in_container_at(&dockerenv, &cgroup));
    }

    #[test]
    fn dockerenv_absent_and_cgroup_clean_returns_false() {
        let tmp = TempDir::new().unwrap();
        let dockerenv = tmp.path().join("dockerenv-missing");
        let cgroup = tmp.path().join("cgroup");
        fs::write(&cgroup, "0::/user.slice/user-1000.slice\n").unwrap();
        assert!(!is_in_container_at(&dockerenv, &cgroup));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cgroup_with_container_marker_returns_true() {
        let cases: &[(&str, &str)] = &[
            (
                "docker",
                "0::/docker/abc123def456\n12:cpu:/docker/abc123def456\n",
            ),
            ("containerd", "0::/system.slice/containerd.service\n"),
            (
                "kubepods",
                "0::/kubepods.slice/kubepods-burstable.slice/kubepods-burstable-podabc.slice\n",
            ),
            (
                "podman",
                "0::/user.slice/user-1000.slice/user@1000.service/podman-1234.scope\n",
            ),
            ("lxc", "0::/lxc/container-name\n"),
        ];

        for (name, contents) in cases {
            let tmp = TempDir::new().unwrap();
            let dockerenv = tmp.path().join("dockerenv-missing");
            let cgroup = tmp.path().join("cgroup");
            fs::write(&cgroup, contents).unwrap();
            assert!(
                is_in_container_at(&dockerenv, &cgroup),
                "expected container detection for {name} cgroup contents",
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn missing_cgroup_file_is_not_a_container() {
        // Bare-metal hosts without /proc/1/cgroup (some sandboxes, chroots)
        // must answer "not in a container" rather than propagate an error.
        let tmp = TempDir::new().unwrap();
        let dockerenv = tmp.path().join("dockerenv-missing");
        let cgroup = tmp.path().join("cgroup-does-not-exist");
        assert!(!is_in_container_at(&dockerenv, &cgroup));
    }

    #[test]
    fn current_executable_returns_existing_absolute_path() {
        let exe = current_executable().unwrap();
        assert!(
            exe.is_absolute(),
            "expected absolute path, got {}",
            exe.display()
        );
        assert!(
            exe.exists(),
            "executable must exist on disk: {}",
            exe.display()
        );
    }
}
