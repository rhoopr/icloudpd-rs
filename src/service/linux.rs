//! Linux backend for `kei install` / `kei uninstall` / `kei service status`.
//!
//! Two install paths land here:
//!
//! - `--user` (default): writes
//!   `${XDG_CONFIG_HOME:-~/.config}/systemd/user/kei.service` and runs
//!   `systemctl --user daemon-reload && systemctl --user enable --now`.
//!   `loginctl enable-linger` is best-effort: a polkit denial logs a
//!   warning and the install still succeeds, since the unit will work
//!   for as long as the user is logged in.
//! - `--system`: writes `/etc/systemd/system/kei.service` with `User=`
//!   pointing at `$SUDO_USER`. Refuses without `EUID=0` rather than
//!   shelling out to `sudo` itself; the operator who chose `--system`
//!   is the one who's expected to run with privilege.
//!
//! Unit-file rendering is split out as a pure function so tests can
//! assert key shape without spawning systemd. The systemd command
//! pipeline (`daemon-reload`, `enable`, `disable`) is exercised by the
//! per-platform smoke matrix, since faithful local mocking of
//! `systemctl --user` requires an active user session.

#![allow(
    clippy::print_stdout,
    reason = "kei service status renders human-readable output to stdout, matching kei status / kei verify."
)]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use tokio::process::Command;

use crate::cli::{InstallArgs, UninstallArgs};
use crate::service::env::{current_executable, SERVICE_DESCRIPTION, SERVICE_IDENTIFIER};

const UNIT_FILE_NAME: &str = "kei.service";

const SYSTEM_UNIT_DIR: &str = "/etc/systemd/system";

/// Renders the per-user `kei.service` body.
///
/// Uses `Type=notify` + `WatchdogSec=120` so systemd treats kei as a
/// long-lived daemon and restarts it if the watchdog ping (sd-notify)
/// stops arriving. `WantedBy=default.target` is the right install
/// target for a user unit; `multi-user.target` is reserved for system
/// units.
fn render_user_unit(exec_path: &Path, config_path: &Path) -> String {
    render_unit(exec_path, config_path, UnitKind::User)
}

/// Renders the system-wide `kei.service` body.
///
/// `User=` pins the daemon to a specific local account so the service
/// uses that user's HOME (and thereby `~/.config/kei`) rather than
/// running as root. `WantedBy=multi-user.target` is the boot-time
/// target every non-graphical Linux server reaches.
fn render_system_unit(exec_path: &Path, config_path: &Path, user: &str) -> String {
    render_unit(
        exec_path,
        config_path,
        UnitKind::System {
            user: user.to_string(),
        },
    )
}

#[derive(Debug, Clone)]
enum UnitKind {
    User,
    System { user: String },
}

fn render_unit(exec_path: &Path, config_path: &Path, kind: UnitKind) -> String {
    let exec = exec_path.display();
    let config = config_path.display();
    let install_target = match &kind {
        UnitKind::User => "default.target",
        UnitKind::System { .. } => "multi-user.target",
    };
    let user_line = match &kind {
        UnitKind::User => String::new(),
        UnitKind::System { user } => format!("User={user}\n"),
    };

    format!(
        "[Unit]\n\
         Description={SERVICE_DESCRIPTION}\n\
         Documentation=https://github.com/rhoopr/kei\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=notify\n\
         {user_line}\
         ExecStart={exec} service run --config {config}\n\
         Restart=on-failure\n\
         RestartSec=10s\n\
         WatchdogSec=120\n\
         NotifyAccess=main\n\
         \n\
         [Install]\n\
         WantedBy={install_target}\n",
    )
}

/// Where the per-user unit lives. Honors `XDG_CONFIG_HOME`; falls back
/// to `~/.config/systemd/user/kei.service`. Returns `None` when neither
/// `XDG_CONFIG_HOME` nor `HOME` is set, which is the right answer
/// because there's no reasonable place to write the file in that case.
fn user_unit_path() -> Option<PathBuf> {
    let dir = dirs::config_dir()?;
    Some(dir.join("systemd/user").join(UNIT_FILE_NAME))
}

fn system_unit_path() -> PathBuf {
    Path::new(SYSTEM_UNIT_DIR).join(UNIT_FILE_NAME)
}

/// Top-level entry for `kei install --user` (and the bare `kei install`
/// default on Linux).
pub(crate) async fn install_user(args: &InstallArgs, config_path: &Path) -> Result<()> {
    let exe = current_executable()?;
    let unit_path =
        user_unit_path().ok_or_else(|| anyhow!("could not resolve XDG_CONFIG_HOME or $HOME"))?;
    let contents = render_user_unit(&exe, config_path);
    write_unit(&unit_path, &contents)?;
    tracing::info!(
        service = SERVICE_IDENTIFIER,
        path = %unit_path.display(),
        executable = %exe.display(),
        config = %config_path.display(),
        dry_run = args.dry_run,
        "wrote per-user systemd unit",
    );

    if args.dry_run {
        tracing::info!(
            "dry run: skipped systemctl daemon-reload / enable / loginctl enable-linger"
        );
        return Ok(());
    }

    daemon_reload_user().await?;
    enable_now_user().await?;
    enable_linger_best_effort().await;

    tracing::info!(
        "kei is now running as a per-user systemd service; \
         check `systemctl --user status {SERVICE_IDENTIFIER}.service` to verify",
    );
    Ok(())
}

/// Top-level entry for `kei install --system`.
pub(crate) async fn install_system(args: &InstallArgs, config_path: &Path) -> Result<()> {
    if !is_root() {
        bail!(
            "`kei install --system` must be run as root (EUID=0); \
             rerun under sudo or use `kei install --user` for a per-user install"
        );
    }
    let user = sudo_user_or_bail()?;
    let exe = current_executable()?;
    let unit_path = system_unit_path();
    let contents = render_system_unit(&exe, config_path, &user);
    write_unit(&unit_path, &contents)?;
    tracing::info!(
        service = SERVICE_IDENTIFIER,
        path = %unit_path.display(),
        executable = %exe.display(),
        config = %config_path.display(),
        run_as_user = user,
        dry_run = args.dry_run,
        "wrote system-wide systemd unit",
    );

    if args.dry_run {
        tracing::info!("dry run: skipped systemctl daemon-reload / enable");
        return Ok(());
    }

    daemon_reload_system().await?;
    enable_now_system().await?;

    tracing::info!(
        "kei is now running as a system-wide systemd service; \
         check `systemctl status {SERVICE_IDENTIFIER}.service` to verify",
    );
    Ok(())
}

/// Top-level entry for `kei uninstall` on Linux. Tries the per-user
/// path first; if no user unit exists, falls back to the system unit
/// (which the operator will need root for).
pub(crate) async fn uninstall(args: &UninstallArgs) -> Result<()> {
    let user_path = user_unit_path().filter(|p| p.exists());
    let system_path = Some(system_unit_path()).filter(|p| p.exists());

    if user_path.is_none() && system_path.is_none() {
        tracing::info!(
            "no kei.service unit file found at the per-user or system path; \
             nothing to uninstall"
        );
    }

    if let Some(path) = user_path.as_ref() {
        // disable + daemon-reload may legitimately fail in a non-systemd
        // environment (tempdir-only test, chroot, sysvinit host). The
        // unit-file removal is the load-bearing step; log+proceed.
        let _ = disable_now_user().await;
        remove_unit_file(path)?;
        let _ = daemon_reload_user().await;
        tracing::info!(path = %path.display(), "removed per-user systemd unit");
    }

    if let Some(path) = system_path.as_ref() {
        if !is_root() {
            bail!(
                "system-wide kei.service is registered at {}; \
                 rerun `kei uninstall` as root to remove it",
                path.display()
            );
        }
        let _ = disable_now_system().await;
        remove_unit_file(path)?;
        let _ = daemon_reload_system().await;
        tracing::info!(path = %path.display(), "removed system-wide systemd unit");
    }

    if args.purge {
        purge_user_data().await?;
    }

    Ok(())
}

/// Implementation for `kei service status` on Linux.
///
/// Calls `systemctl --user show kei.service` and parses the resulting
/// key=value pairs. We do not read the full status output (`systemctl
/// status`) because it embeds escape codes and free-form journal lines
/// that complicate parsing; `show` returns a clean machine-readable
/// projection.
pub(crate) async fn status() -> Result<()> {
    let line = render_status(probe_status_inputs().await?);
    println!("{line}");
    Ok(())
}

enum StatusInputs {
    NotInstalled,
    BusUnavailable {
        scope: &'static str,
    },
    Probed {
        scope: &'static str,
        probe: std::collections::BTreeMap<String, String>,
    },
}

async fn probe_status_inputs() -> Result<StatusInputs> {
    let user_unit = user_unit_path().is_some_and(|p| p.exists());
    let system_unit = system_unit_path().exists();

    if !user_unit && !system_unit {
        return Ok(StatusInputs::NotInstalled);
    }

    let scope = if user_unit { "user" } else { "system" };
    let scope_args: &[&str] = if user_unit { &["--user"] } else { &[] };
    Ok(match show_unit(scope_args).await? {
        ProbeOutcome::BusUnavailable => StatusInputs::BusUnavailable { scope },
        ProbeOutcome::Properties(probe) => StatusInputs::Probed { scope, probe },
    })
}

fn render_status(inputs: StatusInputs) -> String {
    match inputs {
        StatusInputs::NotInstalled => "Service: not installed".to_string(),
        StatusInputs::BusUnavailable { scope } => {
            // Unit file exists but we can't talk to systemd (no active
            // session bus, e.g. SSH without `loginctl enable-linger`).
            // Saying "not installed" would be wrong; surface the cause.
            format!("Service: installed (systemd {scope}, bus unavailable)")
        }
        StatusInputs::Probed { scope, probe } => {
            let active = probe.get("ActiveState").map(String::as_str).unwrap_or("?");
            let sub = probe.get("SubState").map(String::as_str).unwrap_or("?");
            let since = probe
                .get("ActiveEnterTimestamp")
                .filter(|s| !s.is_empty())
                .map(String::as_str);

            if active == "active" {
                let when = since.map(|s| format!(" since {s}")).unwrap_or_default();
                format!("Service: running (systemd {scope}, {sub}{when})")
            } else {
                format!("Service: {active} (systemd {scope}, {sub})")
            }
        }
    }
}

// ── Internals ───────────────────────────────────────────────────────────

fn write_unit(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create unit directory {}", parent.display()))?;
    }
    std::fs::write(path, contents)
        .with_context(|| format!("failed to write unit file {}", path.display()))
}

fn remove_unit_file(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("failed to remove unit file {}", path.display())),
    }
}

async fn purge_user_data() -> Result<()> {
    let Some(config_dir) = dirs::config_dir() else {
        bail!("--purge requested but no XDG config dir resolves; cannot locate kei state");
    };
    let kei_dir = config_dir.join("kei");

    // Read username out of the config before deleting, so the OS keyring
    // entry kept by `CredentialStore` can be cleared too. Without this the
    // credential survives `--purge`, contradicting the docs and leaving a
    // password the user thinks they removed.
    if let Some(username) = read_config_username(&kei_dir).await {
        let store = crate::credential::CredentialStore::new(&username, &kei_dir);
        if let Err(e) = store.delete() {
            // delete() bails when neither backend has anything to remove,
            // which is fine for purge (we're cleaning up regardless).
            tracing::debug!(error = %e, "credential delete during purge: nothing to remove");
        } else {
            tracing::info!(username, "cleared stored credential");
        }
    }

    match std::fs::remove_dir_all(&kei_dir) {
        Ok(()) => {
            tracing::info!(path = %kei_dir.display(), "purged kei state directory");
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!(path = %kei_dir.display(), "no kei state directory to purge");
            Ok(())
        }
        Err(e) => Err(e)
            .with_context(|| format!("failed to remove state directory {}", kei_dir.display())),
    }
}

async fn read_config_username(kei_dir: &Path) -> Option<String> {
    let config_path = kei_dir.join("config.toml");
    let toml = crate::config::load_toml_config(&config_path, false).ok()??;
    toml.auth?.username.filter(|u| !u.is_empty())
}

fn is_root() -> bool {
    // SAFETY: libc::geteuid() is a stateless POSIX FFI call with no
    // preconditions, no side effects, and a uid_t return value; it cannot
    // violate Rust memory safety. Same pattern as src/state/db.rs.
    unsafe { libc::geteuid() == 0 }
}

fn sudo_user_or_bail() -> Result<String> {
    match std::env::var("SUDO_USER") {
        Ok(u) if !u.is_empty() && u != "root" => Ok(u),
        _ => bail!(
            "$SUDO_USER not set; rerun via `sudo kei install --system` so the service \
             can be configured to run as your account rather than root"
        ),
    }
}

async fn daemon_reload_user() -> Result<()> {
    run_systemctl(&["--user", "daemon-reload"]).await
}

async fn enable_now_user() -> Result<()> {
    run_systemctl(&["--user", "enable", "--now", UNIT_FILE_NAME]).await
}

async fn disable_now_user() -> Result<()> {
    run_systemctl(&["--user", "disable", "--now", UNIT_FILE_NAME]).await
}

async fn daemon_reload_system() -> Result<()> {
    run_systemctl(&["daemon-reload"]).await
}

async fn enable_now_system() -> Result<()> {
    run_systemctl(&["enable", "--now", UNIT_FILE_NAME]).await
}

async fn disable_now_system() -> Result<()> {
    run_systemctl(&["disable", "--now", UNIT_FILE_NAME]).await
}

async fn enable_linger_best_effort() {
    let user = match std::env::var("USER") {
        Ok(u) if !u.is_empty() => u,
        _ => {
            tracing::warn!("$USER not set; skipping `loginctl enable-linger`");
            return;
        }
    };
    let status = Command::new("loginctl")
        .arg("enable-linger")
        .arg(&user)
        .status()
        .await;
    match status {
        Ok(s) if s.success() => {
            tracing::info!(
                user,
                "enabled loginctl linger so the service survives logout"
            );
        }
        Ok(s) => {
            tracing::warn!(
                user,
                code = s.code().unwrap_or(-1),
                "loginctl enable-linger failed; service will only run while {user} is logged in. \
                 Run `sudo loginctl enable-linger {user}` to enable it manually."
            );
        }
        Err(e) => {
            tracing::warn!(error = %e, "loginctl not found; skipping enable-linger");
        }
    }
}

async fn run_systemctl<I, S>(args: I) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<_> = args.into_iter().collect();
    let output = Command::new("systemctl")
        .args(&args)
        .output()
        .await
        .context("failed to invoke `systemctl` (is systemd installed and on PATH?)")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let argv = args
            .iter()
            .map(|a| a.as_ref().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ");
        bail!("`systemctl {argv}` failed: {}", stderr.trim());
    }
    Ok(())
}

enum ProbeOutcome {
    Properties(std::collections::BTreeMap<String, String>),
    BusUnavailable,
}

async fn show_unit(scope: &[&str]) -> Result<ProbeOutcome> {
    let mut argv: Vec<&str> = scope.to_vec();
    argv.extend([
        "show",
        UNIT_FILE_NAME,
        "--property=ActiveState,SubState,ActiveEnterTimestamp",
    ]);
    let output = Command::new("systemctl")
        .args(&argv)
        .output()
        .await
        .context("failed to invoke `systemctl show`")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if is_session_bus_unavailable(&stderr) {
            return Ok(ProbeOutcome::BusUnavailable);
        }
        bail!("`systemctl show` failed: {}", stderr.trim());
    }
    Ok(ProbeOutcome::Properties(parse_show_output(
        &String::from_utf8_lossy(&output.stdout),
    )))
}

fn is_session_bus_unavailable(stderr: &str) -> bool {
    // systemd / dbus emit one of these when there's no active user
    // session to talk to. We treat that as "can't probe state" rather
    // than "service is broken" so `kei service status` over SSH
    // reports something useful instead of a hard error.
    stderr.contains("Failed to connect to bus")
        || stderr.contains("Failed to connect to user scope bus")
        || stderr.contains("No medium found")
}

fn parse_show_output(stdout: &str) -> std::collections::BTreeMap<String, String> {
    stdout
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn user_unit_contains_required_sections_and_keys() {
        let unit = render_user_unit(
            &PathBuf::from("/usr/local/bin/kei"),
            &PathBuf::from("/home/alice/.config/kei/config.toml"),
        );
        assert!(unit.contains("[Unit]"));
        assert!(unit.contains("[Service]"));
        assert!(unit.contains("[Install]"));
        assert!(unit.contains(&format!("Description={SERVICE_DESCRIPTION}")));
        assert!(unit.contains("Type=notify"));
        assert!(unit.contains("WatchdogSec=120"));
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains(
            "ExecStart=/usr/local/bin/kei service run --config /home/alice/.config/kei/config.toml"
        ));
        assert!(unit.contains("WantedBy=default.target"));
        // Per-user unit must not pin a User= (it inherits from the
        // user-systemd-instance owner).
        assert!(
            !unit.contains("\nUser="),
            "per-user unit must not declare User=:\n{unit}"
        );
    }

    #[test]
    fn system_unit_pins_user_and_targets_multi_user() {
        let unit = render_system_unit(
            &PathBuf::from("/opt/kei/bin/kei"),
            &PathBuf::from("/etc/kei/config.toml"),
            "alice",
        );
        assert!(unit.contains("User=alice"));
        assert!(unit.contains("WantedBy=multi-user.target"));
        assert!(
            unit.contains("ExecStart=/opt/kei/bin/kei service run --config /etc/kei/config.toml")
        );
        assert!(unit.contains("Type=notify"));
    }

    #[test]
    fn parse_show_output_extracts_known_keys() {
        let raw = "ActiveState=active\nSubState=running\nActiveEnterTimestamp=Thu 2026-05-07 14:32:01 UTC\n";
        let parsed = parse_show_output(raw);
        assert_eq!(
            parsed.get("ActiveState").map(String::as_str),
            Some("active")
        );
        assert_eq!(parsed.get("SubState").map(String::as_str), Some("running"));
        assert_eq!(
            parsed.get("ActiveEnterTimestamp").map(String::as_str),
            Some("Thu 2026-05-07 14:32:01 UTC"),
        );
    }

    #[test]
    fn parse_show_output_handles_blank_timestamp() {
        let raw = "ActiveState=inactive\nSubState=dead\nActiveEnterTimestamp=\n";
        let parsed = parse_show_output(raw);
        assert_eq!(
            parsed.get("ActiveEnterTimestamp").map(String::as_str),
            Some(""),
        );
    }

    #[test]
    fn user_unit_path_is_under_config_dir() {
        // Result depends on $HOME / $XDG_CONFIG_HOME, but if either is
        // set the path must end with systemd/user/kei.service.
        if let Some(p) = user_unit_path() {
            assert!(
                p.ends_with("systemd/user/kei.service"),
                "expected systemd user path, got {}",
                p.display()
            );
            assert!(p.is_absolute());
        }
    }

    #[test]
    fn system_unit_path_is_etc_systemd_system() {
        let p = system_unit_path();
        assert_eq!(p, Path::new("/etc/systemd/system/kei.service"));
    }

    #[test]
    fn render_status_reports_not_installed() {
        assert_eq!(
            render_status(StatusInputs::NotInstalled),
            "Service: not installed"
        );
    }

    #[test]
    fn render_status_reports_bus_unavailable_when_no_session() {
        let line = render_status(StatusInputs::BusUnavailable { scope: "user" });
        assert_eq!(line, "Service: installed (systemd user, bus unavailable)");
    }

    #[test]
    fn render_status_includes_active_enter_timestamp() {
        let mut probe = std::collections::BTreeMap::new();
        probe.insert("ActiveState".to_string(), "active".to_string());
        probe.insert("SubState".to_string(), "running".to_string());
        probe.insert(
            "ActiveEnterTimestamp".to_string(),
            "Thu 2026-05-07 14:32:01 UTC".to_string(),
        );
        let line = render_status(StatusInputs::Probed {
            scope: "user",
            probe,
        });
        assert_eq!(
            line,
            "Service: running (systemd user, running since Thu 2026-05-07 14:32:01 UTC)"
        );
    }

    #[test]
    fn render_status_handles_inactive_with_blank_timestamp() {
        let mut probe = std::collections::BTreeMap::new();
        probe.insert("ActiveState".to_string(), "inactive".to_string());
        probe.insert("SubState".to_string(), "dead".to_string());
        probe.insert("ActiveEnterTimestamp".to_string(), String::new());
        let line = render_status(StatusInputs::Probed {
            scope: "user",
            probe,
        });
        assert_eq!(line, "Service: inactive (systemd user, dead)");
    }

    #[test]
    fn detects_session_bus_unavailable_strings() {
        assert!(is_session_bus_unavailable(
            "Failed to connect to bus: No such file or directory\n"
        ));
        assert!(is_session_bus_unavailable(
            "Failed to connect to user scope bus via local transport\n"
        ));
        assert!(!is_session_bus_unavailable(
            "Unit kei.service could not be found.\n"
        ));
    }
}
