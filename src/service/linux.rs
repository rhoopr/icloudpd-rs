//! Linux backend for `kei install` / `kei uninstall` / `kei service status`.
//!
//! Two install paths land here:
//!
//! - `--user` (default): writes
//!   `${XDG_CONFIG_HOME:-~/.config}/systemd/user/kei.service` and runs
//!   `systemctl --user daemon-reload && systemctl --user enable --now`.
//!   `loginctl enable-linger` is best-effort: a polkit denial logs a
//!   warning and the install still succeeds, since the unit will work
//!   for as long as the user is logged in. The prior linger state is
//!   recorded in the unit and restored on uninstall when available.
//! - `--system`: writes `/etc/systemd/system/kei.service` with `User=`
//!   pointing at `$SUDO_USER`. Refuses without `EUID=0` rather than
//!   shelling out to `sudo` itself; the operator who chose `--system`
//!   is the one who's expected to run with privilege.
//!
//! Unit-file rendering is split out as a pure function so tests can
//! assert key shape without spawning systemd. CI covers dry-run rendering
//! and unit syntax; real `daemon-reload` / `enable` / `disable` handoff
//! still needs an active user session.

#![allow(
    clippy::print_stdout,
    reason = "kei service status renders human-readable output to stdout, matching kei status / kei verify."
)]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Utc};
use tokio::process::Command;

use crate::cli::UninstallArgs;
use crate::service::env::{
    current_executable, effective_uid, purge_kei_state, SERVICE_DESCRIPTION, SERVICE_IDENTIFIER,
};
use crate::service::plan::{self, InstallPlan};
use crate::service::status::ServiceState;

const UNIT_FILE_NAME: &str = "kei.service";
const PREVIOUS_LINGER_MARKER: &str = "# X-Kei-Previous-Linger=";

const SYSTEM_UNIT_DIR: &str = "/etc/systemd/system";

/// Renders the per-user `kei.service` body.
///
/// Uses `Type=notify` + `WatchdogSec=120` so systemd treats kei as a
/// long-lived daemon and restarts it if the watchdog ping (sd-notify)
/// stops arriving. `WantedBy=default.target` is the right install
/// target for a user unit; `multi-user.target` is reserved for system
/// units.
fn render_user_unit(exec_path: &Path, config_path: &Path) -> String {
    render_user_unit_with_linger(exec_path, config_path, None)
}

fn render_user_unit_with_linger(
    exec_path: &Path,
    config_path: &Path,
    previous_linger: Option<LingerState>,
) -> String {
    render_unit(exec_path, config_path, UnitKind::User { previous_linger })
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
    User {
        previous_linger: Option<LingerState>,
    },
    System {
        user: String,
    },
}

fn render_unit(exec_path: &Path, config_path: &Path, kind: UnitKind) -> String {
    let exec = exec_path.display();
    let config = config_path.display();
    let install_target = match &kind {
        UnitKind::User { .. } => "default.target",
        UnitKind::System { .. } => "multi-user.target",
    };
    let user_line = match &kind {
        UnitKind::User { .. } => String::new(),
        UnitKind::System { user } => format!("User={user}\n"),
    };
    let previous_linger_comment = match &kind {
        UnitKind::User {
            previous_linger: Some(state),
        } => format!("{PREVIOUS_LINGER_MARKER}{}\n", state.as_marker_value()),
        UnitKind::User {
            previous_linger: None,
        }
        | UnitKind::System { .. } => String::new(),
    };

    format!(
        "[Unit]\n\
         {previous_linger_comment}\
         Description={SERVICE_DESCRIPTION}\n\
         Documentation=https://github.com/rhoopr/kei\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=notify\n\
         {user_line}\
         Environment=MALLOC_ARENA_MAX=2\n\
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
pub(crate) async fn install_user(plan: InstallPlan, config_path: &Path) -> Result<()> {
    let exe = current_executable()?;
    if plan.is_preview() {
        let contents = render_user_unit(&exe, config_path);
        print!("{contents}");
        tracing::info!("dry run: rendered per-user systemd unit; no files written");
        return Ok(());
    }

    let previous_linger = current_linger_state().await;
    let contents = render_user_unit_with_linger(&exe, config_path, previous_linger);
    let unit_path =
        user_unit_path().ok_or_else(|| anyhow!("could not resolve XDG_CONFIG_HOME or $HOME"))?;
    write_unit(&unit_path, &contents)?;
    tracing::info!(
        service = SERVICE_IDENTIFIER,
        path = %unit_path.display(),
        executable = %exe.display(),
        config = %config_path.display(),
        "wrote per-user systemd unit",
    );

    daemon_reload_user().await?;
    enable_now_user().await?;
    enable_linger_best_effort().await;

    tracing::info!(
        "kei is now running as a per-user systemd service; \
         check `systemctl --user status {SERVICE_IDENTIFIER}.service` to verify. \
         Run `kei uninstall` to remove this service.",
    );
    Ok(())
}

/// Top-level entry for `kei install --system`.
pub(crate) async fn install_system(plan: InstallPlan, config_path: &Path) -> Result<()> {
    let user = plan::linux_system_user(plan, config_path)?;
    if plan.is_preview() {
        let exe = current_executable()?;
        let contents = render_system_unit(&exe, config_path, &user);
        print!("{contents}");
        tracing::info!(
            run_as_user = user,
            "dry run: rendered system-wide systemd unit; no files written"
        );
        return Ok(());
    }

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
        "wrote system-wide systemd unit",
    );

    daemon_reload_system().await?;
    enable_now_system().await?;

    tracing::info!(
        "kei is now running as a system-wide systemd service; \
         check `systemctl status {SERVICE_IDENTIFIER}.service` to verify. \
         Run `kei uninstall` as root to remove this service.",
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
        tracing::info!("kei service was already removed. Nothing to do.");
        // Don't bail — the service is already gone, which is the desired state.
        // Still run purge if requested.
    }

    if let Some(path) = user_path.as_ref() {
        let previous_linger = read_previous_linger_state(path);
        // disable + daemon-reload may legitimately fail in a non-systemd
        // environment (tempdir-only test, chroot, sysvinit host). The
        // unit-file removal is the load-bearing step; log+proceed.
        let _ = disable_now_user().await;
        remove_unit_file(path)?;
        let _ = daemon_reload_user().await;
        restore_linger_best_effort(previous_linger).await;
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
        let Some(config_dir) = dirs::config_dir() else {
            bail!("--purge requested but no XDG config dir resolves; cannot locate kei state");
        };
        purge_kei_state(&config_dir.join("kei"), &[])?;
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

/// `service_state()` for the `Service:` section in `kei status`. Reuses
/// [`probe_status_inputs`] and converts the raw systemctl key/value map
/// into the platform-agnostic [`ServiceState`].
pub(crate) async fn service_state() -> Result<ServiceState> {
    Ok(match probe_status_inputs().await? {
        StatusInputs::NotInstalled => ServiceState::NotInstalled,
        StatusInputs::BusUnavailable { scope } => ServiceState::BackendUnavailable {
            backend: backend_label(scope),
            reason: "bus unavailable",
        },
        StatusInputs::Probed { scope, probe } => probe_to_state(scope, &probe),
    })
}

// probe_status_inputs only ever passes "user" or "system"; the unreachable!
// arm is a refactor tripwire, not a runtime concern, hence the allow.
#[allow(
    clippy::unreachable,
    reason = "scope strings are produced by probe_status_inputs; \
              third value would be a refactor bug worth panicking on"
)]
fn backend_label(scope: &'static str) -> &'static str {
    match scope {
        "user" => "systemd user",
        "system" => "systemd system",
        other => unreachable!("unexpected systemd scope: {other:?}"),
    }
}

fn probe_to_state(
    scope: &'static str,
    probe: &std::collections::BTreeMap<String, String>,
) -> ServiceState {
    let active = probe.get("ActiveState").map(String::as_str).unwrap_or("?");
    let state_label: &'static str = match active {
        "active" => crate::service::status::RUNNING_LABEL,
        "inactive" => "stopped",
        "failed" => "failed",
        "activating" => "starting",
        "deactivating" => "stopping",
        "reloading" => "reloading",
        _ => "unknown",
    };
    let since = probe
        .get("ActiveEnterTimestamp")
        .and_then(|s| parse_systemd_timestamp(s));
    let pid = probe
        .get("MainPID")
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|&p| p != 0);
    ServiceState::Installed {
        backend: backend_label(scope),
        state_label,
        since,
        pid,
    }
}

/// Parses systemd's `ActiveEnterTimestamp` into a UTC `DateTime`.
///
/// systemd formats the timestamp as `<weekday-abbrev> YYYY-MM-DD
/// HH:MM:SS <TZ>` where the timezone defaults to the host's local zone
/// unless `LC_ALL=C` or similar is in effect. Service units run with
/// the host's tzdata, so the value is usually `UTC` on servers and the
/// local zone on workstations. Only the UTC form is parsed here -- other
/// zones return `None` so the renderer omits the `since` clause rather
/// than guessing wrong. The returned timestamp is informational; missing
/// it is preferable to misleading the operator.
fn parse_systemd_timestamp(raw: &str) -> Option<DateTime<Utc>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    // `Thu 2026-05-07 14:32:01 UTC` -- weekday is decorative; chrono
    // matches `%a` against any 3-letter abbrev.
    let parsed = chrono::NaiveDateTime::parse_from_str(trimmed, "%a %Y-%m-%d %H:%M:%S UTC").ok()?;
    Some(DateTime::<Utc>::from_naive_utc_and_offset(parsed, Utc))
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

fn is_root() -> bool {
    effective_uid() == 0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LingerState {
    Enabled,
    Disabled,
}

impl LingerState {
    fn as_marker_value(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
        }
    }

    fn from_marker_value(value: &str) -> Option<Self> {
        match value.trim() {
            "enabled" => Some(Self::Enabled),
            "disabled" => Some(Self::Disabled),
            _ => None,
        }
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

fn login_user() -> Option<String> {
    match std::env::var("USER") {
        Ok(u) if !u.is_empty() => Some(u),
        _ => None,
    }
}

async fn current_linger_state() -> Option<LingerState> {
    let Some(user) = login_user() else {
        tracing::warn!("$USER not set; cannot record prior loginctl linger state");
        return None;
    };
    let output = match Command::new("loginctl")
        .arg("show-user")
        .arg(&user)
        .arg("-p")
        .arg("Linger")
        .output()
        .await
    {
        Ok(output) => output,
        Err(e) => {
            tracing::warn!(user, error = %e, "loginctl not found; cannot record prior linger state");
            return None;
        }
    };
    if !output.status.success() {
        tracing::warn!(
            user,
            code = output.status.code().unwrap_or(-1),
            stderr = %String::from_utf8_lossy(&output.stderr).trim(),
            "loginctl show-user failed; cannot record prior linger state"
        );
        return None;
    }
    let parsed = parse_linger_state(&String::from_utf8_lossy(&output.stdout));
    if let Some(state) = parsed {
        tracing::debug!(user, ?state, "recorded prior loginctl linger state");
    } else {
        tracing::warn!(
            user,
            stdout = %String::from_utf8_lossy(&output.stdout).trim(),
            "loginctl show-user returned an unrecognized linger state"
        );
    }
    parsed
}

fn parse_linger_state(stdout: &str) -> Option<LingerState> {
    stdout.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        if key != "Linger" {
            return None;
        }
        match value.trim() {
            "yes" => Some(LingerState::Enabled),
            "no" => Some(LingerState::Disabled),
            _ => None,
        }
    })
}

fn read_previous_linger_state(unit_path: &Path) -> Option<LingerState> {
    match std::fs::read_to_string(unit_path) {
        Ok(contents) => previous_linger_state_from_unit(&contents),
        Err(e) => {
            tracing::warn!(
                path = %unit_path.display(),
                error = %e,
                "could not read unit file to restore prior linger state"
            );
            None
        }
    }
}

fn previous_linger_state_from_unit(contents: &str) -> Option<LingerState> {
    contents.lines().find_map(|line| {
        line.strip_prefix(PREVIOUS_LINGER_MARKER)
            .and_then(LingerState::from_marker_value)
    })
}

async fn restore_linger_best_effort(previous_linger: Option<LingerState>) {
    match previous_linger {
        Some(LingerState::Disabled) => disable_linger_best_effort().await,
        Some(LingerState::Enabled) => {
            tracing::info!(
                "leaving loginctl linger enabled because it was enabled before kei install"
            );
        }
        None => {
            tracing::info!(
                "no prior loginctl linger state was recorded; leaving current linger state unchanged"
            );
        }
    }
}

async fn enable_linger_best_effort() {
    let user = match login_user() {
        Some(u) => u,
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

async fn disable_linger_best_effort() {
    let user = match login_user() {
        Some(u) => u,
        _ => {
            tracing::warn!("$USER not set; skipping `loginctl disable-linger`");
            return;
        }
    };
    let status = Command::new("loginctl")
        .arg("disable-linger")
        .arg(&user)
        .status()
        .await;
    match status {
        Ok(s) if s.success() => {
            tracing::info!(
                user,
                "restored loginctl linger to disabled because it was disabled before kei install"
            );
        }
        Ok(s) => {
            tracing::warn!(
                user,
                code = s.code().unwrap_or(-1),
                "loginctl disable-linger failed; linger may remain enabled"
            );
        }
        Err(e) => {
            tracing::warn!(error = %e, "loginctl not found; skipping disable-linger");
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
        "--property=ActiveState,SubState,ActiveEnterTimestamp,MainPID",
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
        assert!(unit.contains("Environment=MALLOC_ARENA_MAX=2"));
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
    fn user_unit_records_previous_linger_state_when_known() {
        let unit = render_user_unit_with_linger(
            &PathBuf::from("/usr/local/bin/kei"),
            &PathBuf::from("/home/alice/.config/kei/config.toml"),
            Some(LingerState::Disabled),
        );
        assert!(unit.contains("# X-Kei-Previous-Linger=disabled\n"));
        assert_eq!(
            previous_linger_state_from_unit(&unit),
            Some(LingerState::Disabled)
        );
    }

    #[test]
    fn user_unit_omits_linger_marker_when_unknown() {
        let unit = render_user_unit(
            &PathBuf::from("/usr/local/bin/kei"),
            &PathBuf::from("/home/alice/.config/kei/config.toml"),
        );
        assert!(!unit.contains(PREVIOUS_LINGER_MARKER));
        assert_eq!(previous_linger_state_from_unit(&unit), None);
    }

    #[test]
    fn previous_linger_state_ignores_unknown_marker_values() {
        assert_eq!(
            previous_linger_state_from_unit("# X-Kei-Previous-Linger=enabled\n"),
            Some(LingerState::Enabled)
        );
        assert_eq!(
            previous_linger_state_from_unit("# X-Kei-Previous-Linger=disabled\n"),
            Some(LingerState::Disabled)
        );
        assert_eq!(
            previous_linger_state_from_unit("# X-Kei-Previous-Linger=maybe\n"),
            None
        );
    }

    #[test]
    fn parse_linger_state_reads_loginctl_output() {
        assert_eq!(
            parse_linger_state("Linger=yes\n"),
            Some(LingerState::Enabled)
        );
        assert_eq!(
            parse_linger_state("Linger=no\n"),
            Some(LingerState::Disabled)
        );
        assert_eq!(
            parse_linger_state("Name=alice\nLinger=no\n"),
            Some(LingerState::Disabled)
        );
        assert_eq!(parse_linger_state("Linger=\n"), None);
        assert_eq!(parse_linger_state("Name=alice\n"), None);
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
        assert!(unit.contains("Environment=MALLOC_ARENA_MAX=2"));
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
    fn parse_systemd_timestamp_accepts_utc_form() {
        let parsed = parse_systemd_timestamp("Thu 2026-05-07 14:32:01 UTC")
            .expect("UTC-form timestamp must parse");
        assert_eq!(
            parsed,
            chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 5, 7, 14, 32, 1).unwrap(),
        );
    }

    #[test]
    fn parse_systemd_timestamp_rejects_local_tz_and_blank() {
        // Anything other than "UTC" returns None so we don't render a
        // misleading "since X UTC" against a local-zone wall clock.
        assert_eq!(parse_systemd_timestamp(""), None);
        assert_eq!(parse_systemd_timestamp("Thu 2026-05-07 14:32:01 EDT"), None);
        assert_eq!(parse_systemd_timestamp("not a timestamp"), None);
    }

    #[test]
    fn probe_to_state_running_with_main_pid() {
        let mut probe = std::collections::BTreeMap::new();
        probe.insert("ActiveState".to_string(), "active".to_string());
        probe.insert("SubState".to_string(), "running".to_string());
        probe.insert(
            "ActiveEnterTimestamp".to_string(),
            "Thu 2026-05-07 14:32:01 UTC".to_string(),
        );
        probe.insert("MainPID".to_string(), "12345".to_string());
        match probe_to_state("user", &probe) {
            ServiceState::Installed {
                backend,
                state_label,
                since,
                pid,
            } => {
                assert_eq!(backend, "systemd user");
                assert_eq!(state_label, "running");
                assert!(since.is_some());
                assert_eq!(pid, Some(12345));
            }
            other => panic!("expected Installed, got {other:?}"),
        }
    }

    #[test]
    fn probe_to_state_inactive_drops_zero_main_pid() {
        // systemd reports `MainPID=0` for inactive units; do not
        // surface that as a real PID in the rendered status line.
        let mut probe = std::collections::BTreeMap::new();
        probe.insert("ActiveState".to_string(), "inactive".to_string());
        probe.insert("SubState".to_string(), "dead".to_string());
        probe.insert("ActiveEnterTimestamp".to_string(), String::new());
        probe.insert("MainPID".to_string(), "0".to_string());
        match probe_to_state("user", &probe) {
            ServiceState::Installed {
                state_label,
                pid,
                since,
                ..
            } => {
                assert_eq!(state_label, "stopped");
                assert_eq!(pid, None);
                assert_eq!(since, None);
            }
            other => panic!("expected Installed, got {other:?}"),
        }
    }

    #[test]
    fn backend_label_distinguishes_user_and_system_scopes() {
        assert_eq!(backend_label("user"), "systemd user");
        assert_eq!(backend_label("system"), "systemd system");
    }

    #[test]
    #[should_panic(expected = "unexpected systemd scope")]
    fn backend_label_panics_on_unknown_scope() {
        // probe_status_inputs only emits "user" or "system" today; if a
        // future refactor adds a third scope without updating
        // backend_label, the panic surfaces it loud.
        let _ = backend_label("other");
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
