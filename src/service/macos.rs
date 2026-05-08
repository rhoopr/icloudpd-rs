//! macOS backend for `kei install` / `kei uninstall` / `kei service status`.
//!
//! Per-user LaunchAgent only. v0.14 deliberately ships no LaunchDaemon
//! (`/Library/LaunchDaemons/`) path because that would require root and
//! brings a different threat model than the keychain-protected per-user
//! flow. `--system` therefore errors with a pointer at `--user`.
//!
//! `kei install` writes
//! `~/Library/LaunchAgents/com.rhoopr.kei.plist`, creates the matching
//! log directory at `~/Library/Logs/kei/`, and runs
//! `launchctl bootstrap gui/$(id -u) <plist>`. `bootstrap` is the modern
//! API; on hosted CI runners and other headless macOS environments where
//! the GUI domain is unavailable we fall back to the legacy
//! `launchctl load -w <plist>` path so the install still succeeds.
//!
//! Uninstall mirrors the same fallback: `launchctl bootout` first,
//! `launchctl unload` if the GUI domain refuses. The plist file is
//! removed last; with `--purge`, `~/.config/kei/` and the credential
//! entry go too (matching the linux backend).
//!
//! Plist rendering is pulled out as a pure function so tests can assert
//! key shape without spawning launchctl. The actual `launchctl bootstrap
//! / bootout` calls are exercised by PR 8's macOS smoke matrix; faithful
//! local mocking would require a live launchd domain.

#![allow(
    clippy::print_stdout,
    reason = "kei service status renders human-readable output to stdout, matching kei status / kei verify."
)]

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use plist::{Dictionary, Value as PlistValue};
use tokio::process::Command;

use crate::cli::{InstallArgs, UninstallArgs};
use crate::service::env::{current_executable, SERVICE_DESCRIPTION, SERVICE_IDENTIFIER};

const PLIST_FILE_NAME: &str = "com.rhoopr.kei.plist";

const LAUNCH_AGENTS_SUBDIR: &str = "Library/LaunchAgents";
const LOG_SUBDIR: &str = "Library/Logs/kei";

/// Renders the launchd property list as a `plist::Dictionary`.
///
/// Returned as a `Dictionary` rather than serialized XML so tests can
/// assert individual keys via `plist`'s typed accessors instead of
/// substring-matching free-form XML. The orchestrator passes the dict
/// through `plist::to_writer_xml` once.
///
/// `KeepAlive` uses the `NetworkState` predicate so launchd brings the
/// daemon back when network connectivity returns (post sleep/wake, VPN
/// toggle, Wi-Fi handoff). `RunAtLoad=true` covers the boot path.
fn render_user_plist(
    exec_path: &Path,
    config_path: &Path,
    log_dir: &Path,
    home_dir: &Path,
) -> Dictionary {
    let mut dict = Dictionary::new();
    dict.insert(
        "Label".to_string(),
        PlistValue::String(SERVICE_IDENTIFIER.to_string()),
    );

    let program_args = vec![
        PlistValue::String(exec_path.display().to_string()),
        PlistValue::String("service".to_string()),
        PlistValue::String("run".to_string()),
        PlistValue::String("--config".to_string()),
        PlistValue::String(config_path.display().to_string()),
    ];
    dict.insert(
        "ProgramArguments".to_string(),
        PlistValue::Array(program_args),
    );

    dict.insert("RunAtLoad".to_string(), PlistValue::Boolean(true));

    let mut keep_alive = Dictionary::new();
    keep_alive.insert("NetworkState".to_string(), PlistValue::Boolean(true));
    dict.insert("KeepAlive".to_string(), PlistValue::Dictionary(keep_alive));

    dict.insert(
        "StandardOutPath".to_string(),
        PlistValue::String(log_dir.join("stdout.log").display().to_string()),
    );
    dict.insert(
        "StandardErrorPath".to_string(),
        PlistValue::String(log_dir.join("stderr.log").display().to_string()),
    );
    dict.insert(
        "WorkingDirectory".to_string(),
        PlistValue::String(home_dir.display().to_string()),
    );

    // Description isn't part of launchd's documented schema, but several
    // GUI tools (LaunchControl, Lingon) surface it. Cheap to include and
    // keeps the human-readable label aligned across platforms.
    dict.insert(
        "ServiceDescription".to_string(),
        PlistValue::String(SERVICE_DESCRIPTION.to_string()),
    );

    // `ProcessType=Background` tells launchd this is a long-running
    // daemon rather than a UI app, so it is exempt from App Nap and
    // similar power-management throttling.
    dict.insert(
        "ProcessType".to_string(),
        PlistValue::String("Background".to_string()),
    );

    dict
}

/// Where the per-user plist lives. Returns `None` when `$HOME` is unset,
/// which is the right answer because there is no reasonable place to
/// write the file in that case.
fn user_plist_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(LAUNCH_AGENTS_SUBDIR).join(PLIST_FILE_NAME))
}

fn user_log_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(LOG_SUBDIR))
}

/// kei state directory on macOS: `~/.config/kei`, matching linux. This
/// deliberately does *not* use `dirs::config_dir()` because that resolves
/// to `~/Library/Application Support` on macOS, which conflicts with the
/// rest of the codebase (config.rs / setup.rs hard-code the dotted path).
fn kei_state_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".config/kei"))
}

/// Top-level entry for `kei install --user` (and the bare `kei install`
/// default on macOS).
pub(crate) async fn install_user(args: &InstallArgs, config_path: &Path) -> Result<()> {
    let exe = current_executable()?;
    let plist_path = user_plist_path()
        .ok_or_else(|| anyhow!("could not resolve $HOME; cannot locate ~/Library/LaunchAgents"))?;
    let log_dir = user_log_dir()
        .ok_or_else(|| anyhow!("could not resolve $HOME; cannot locate ~/Library/Logs/kei"))?;
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow!("could not resolve $HOME; required for plist WorkingDirectory"))?;

    std::fs::create_dir_all(&log_dir)
        .with_context(|| format!("failed to create log directory {}", log_dir.display()))?;

    let dict = render_user_plist(&exe, config_path, &log_dir, &home);
    let xml = serialize_plist(&dict)?;
    write_plist(&plist_path, &xml)?;
    tracing::info!(
        service = SERVICE_IDENTIFIER,
        path = %plist_path.display(),
        executable = %exe.display(),
        config = %config_path.display(),
        log_dir = %log_dir.display(),
        dry_run = args.dry_run,
        "wrote per-user launchd plist",
    );

    if args.dry_run {
        tracing::info!(
            "dry run: skipped launchctl bootstrap (use `launchctl bootstrap gui/$(id -u) {}` to load manually)",
            plist_path.display(),
        );
        return Ok(());
    }

    bootstrap_or_load(&plist_path).await?;

    tracing::info!(
        "kei is now running as a per-user launchd agent; \
         check `launchctl list {SERVICE_IDENTIFIER}` to verify"
    );
    Ok(())
}

/// `--system` rejection. macOS LaunchDaemons require root and a different
/// security review (system-context FDA, keychain access constraints) that
/// is explicitly out of scope for v0.14. Errors with a pointer at the
/// supported flag rather than silently downgrading.
pub(crate) async fn install_system(_args: &InstallArgs, _config_path: &Path) -> Result<()> {
    bail!(
        "macOS only ships a per-user LaunchAgent in v0.14; \
         rerun without --system (or with --user) to install. \
         System-wide LaunchDaemons (root, /Library/LaunchDaemons) are tracked for a future release."
    )
}

/// Top-level entry for `kei uninstall` on macOS.
pub(crate) async fn uninstall(args: &UninstallArgs) -> Result<()> {
    let plist_path = user_plist_path().filter(|p| p.exists());

    if plist_path.is_none() {
        tracing::info!(
            "no kei launchd plist found at ~/Library/LaunchAgents/{PLIST_FILE_NAME}; \
             nothing to uninstall"
        );
    }

    if let Some(path) = plist_path.as_ref() {
        // bootout / unload may legitimately fail in environments where
        // the GUI domain is unavailable (CI, SSH session into headless
        // mac, plist-not-loaded-but-present). The plist removal is the
        // load-bearing step; log+proceed.
        let _ = bootout_or_unload(path).await;
        remove_plist_file(path)?;
        tracing::info!(path = %path.display(), "removed per-user launchd plist");
    }

    if args.purge {
        purge_user_data().await?;
    }

    Ok(())
}

/// Implementation for `kei service status` on macOS.
///
/// Calls `launchctl print gui/<uid>/com.rhoopr.kei` and parses the
/// `state = ...` line. `print` is the modern replacement for `list`
/// and returns enough structure to recover both running-state and the
/// last spawn time across recent macOS versions.
pub(crate) async fn status() -> Result<()> {
    let line = render_status(probe_status_inputs().await?);
    println!("{line}");
    Ok(())
}

#[derive(Debug)]
enum StatusInputs {
    NotInstalled,
    DomainUnavailable,
    Probed { state: String, pid: Option<String> },
}

async fn probe_status_inputs() -> Result<StatusInputs> {
    let plist_present = user_plist_path().is_some_and(|p| p.exists());
    if !plist_present {
        return Ok(StatusInputs::NotInstalled);
    }

    match launchctl_print().await? {
        ProbeOutcome::DomainUnavailable => Ok(StatusInputs::DomainUnavailable),
        ProbeOutcome::Properties { state, pid } => Ok(StatusInputs::Probed { state, pid }),
    }
}

fn render_status(inputs: StatusInputs) -> String {
    match inputs {
        StatusInputs::NotInstalled => "Service: not installed".to_string(),
        StatusInputs::DomainUnavailable => {
            // Plist exists but launchctl can't talk to the GUI domain
            // (typical of an SSH session into a headless mac without an
            // active console user). Same shape as the linux
            // BusUnavailable branch so consumers see a consistent
            // "installed but unprobeable" signal across platforms.
            "Service: installed (launchd user, domain unavailable)".to_string()
        }
        StatusInputs::Probed { state, pid } => {
            // launchctl reports `state = running` for healthy services.
            // Anything else (`not running`, `exited`, `waiting`) is
            // surfaced verbatim so the operator can grep `man launchd.plist`.
            let pid_suffix = pid
                .as_deref()
                .filter(|p| !p.is_empty() && *p != "-")
                .map(|p| format!(", pid {p}"))
                .unwrap_or_default();
            if state == "running" {
                format!("Service: running (launchd user{pid_suffix})")
            } else {
                format!("Service: {state} (launchd user{pid_suffix})")
            }
        }
    }
}

// ── Internals ───────────────────────────────────────────────────────────

fn write_plist(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create LaunchAgents directory {}",
                parent.display()
            )
        })?;
    }
    std::fs::write(path, contents)
        .with_context(|| format!("failed to write plist {}", path.display()))
}

fn remove_plist_file(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("failed to remove plist {}", path.display())),
    }
}

fn serialize_plist(dict: &Dictionary) -> Result<String> {
    let mut buf = Vec::new();
    plist::to_writer_xml(&mut buf, dict).context("failed to serialize launchd plist to XML")?;
    String::from_utf8(buf).context("plist serializer emitted non-UTF-8 bytes")
}

async fn purge_user_data() -> Result<()> {
    let Some(kei_dir) = kei_state_dir() else {
        bail!("--purge requested but $HOME does not resolve; cannot locate kei state");
    };

    if let Some(username) = read_config_username(&kei_dir).await {
        let store = crate::credential::CredentialStore::new(&username, &kei_dir);
        if let Err(e) = store.delete() {
            tracing::debug!(error = %e, "credential delete during purge: nothing to remove");
        } else {
            tracing::info!(username, "cleared stored credential");
        }
    }

    if let Some(log_dir) = user_log_dir() {
        match std::fs::remove_dir_all(&log_dir) {
            Ok(()) => tracing::info!(path = %log_dir.display(), "removed kei log directory"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(error = %e, path = %log_dir.display(), "failed to remove log directory")
            }
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

/// Tries `launchctl bootstrap gui/<uid>` first; falls back to the legacy
/// `launchctl load -w` path on hosts where the GUI domain is unavailable
/// (headless CI runners, screen-locked hosts without an active session).
async fn bootstrap_or_load(plist_path: &Path) -> Result<()> {
    let domain = gui_domain()?;
    let bootstrap = run_launchctl(&["bootstrap", &domain, &plist_path.display().to_string()]).await;
    match bootstrap {
        Ok(()) => Ok(()),
        Err(e) if is_domain_unavailable(&e.to_string()) => {
            tracing::warn!(
                error = %e,
                "launchctl bootstrap failed (no GUI domain); falling back to legacy `load -w`"
            );
            run_launchctl(&["load", "-w", &plist_path.display().to_string()]).await
        }
        Err(e) => Err(e),
    }
}

async fn bootout_or_unload(plist_path: &Path) -> Result<()> {
    let target = format!("{}/{SERVICE_IDENTIFIER}", gui_domain()?);
    let bootout = run_launchctl(&["bootout", &target]).await;
    match bootout {
        Ok(()) => Ok(()),
        Err(e) if is_domain_unavailable(&e.to_string()) || is_not_loaded(&e.to_string()) => {
            // Either no GUI domain, or already booted out. Try the
            // legacy unload as a belt-and-braces clean up.
            tracing::debug!(
                error = %e,
                "launchctl bootout fell through; running legacy `unload`"
            );
            run_launchctl(&["unload", &plist_path.display().to_string()]).await
        }
        Err(e) => Err(e),
    }
}

fn gui_domain() -> Result<String> {
    // SAFETY: libc::geteuid is a stateless POSIX FFI call with no
    // memory-safety preconditions; same pattern as src/service/linux.rs.
    let uid = unsafe { libc::geteuid() };
    Ok(format!("gui/{uid}"))
}

fn is_domain_unavailable(stderr: &str) -> bool {
    // launchctl emits a small set of fingerprints when the GUI domain
    // can't be reached (typical of CI runners or SSH sessions into a
    // mac without an active GUI login). The error matrix is documented
    // (loosely) in `launchctl(1)`; these are the strings observed in
    // the wild.
    stderr.contains("Could not find domain")
        || stderr.contains("Bootstrap failed: 5: Input/output error")
        || stderr.contains("Could not bootstrap")
        || stderr.contains("Operation not permitted")
}

fn is_not_loaded(stderr: &str) -> bool {
    stderr.contains("No such process") || stderr.contains("Could not find specified service")
}

async fn run_launchctl(args: &[&str]) -> Result<()> {
    let output = Command::new("launchctl")
        .args(args)
        .output()
        .await
        .context("failed to invoke `launchctl` (is this macOS?)")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = if stderr.trim().is_empty() {
            stdout.trim().to_string()
        } else {
            stderr.trim().to_string()
        };
        bail!("`launchctl {}` failed: {detail}", args.join(" "));
    }
    Ok(())
}

#[derive(Debug)]
enum ProbeOutcome {
    DomainUnavailable,
    Properties { state: String, pid: Option<String> },
}

async fn launchctl_print() -> Result<ProbeOutcome> {
    let target = format!("{}/{SERVICE_IDENTIFIER}", gui_domain()?);
    let output = Command::new("launchctl")
        .args(["print", &target])
        .output()
        .await
        .context("failed to invoke `launchctl print`")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if is_domain_unavailable(&stderr) || is_not_loaded(&stderr) {
            return Ok(ProbeOutcome::DomainUnavailable);
        }
        bail!("`launchctl print {target}` failed: {}", stderr.trim());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed = parse_launchctl_print(&stdout);
    Ok(ProbeOutcome::Properties {
        state: parsed.state.unwrap_or_else(|| "unknown".to_string()),
        pid: parsed.pid,
    })
}

#[derive(Debug, Default)]
struct LaunchctlPrint {
    state: Option<String>,
    pid: Option<String>,
}

/// Parses `launchctl print` stdout for the `state` and `pid` fields.
///
/// `launchctl print` output is loosely structured `key = value`-ish
/// indented blocks. We pull only the two fields kei surfaces; the rest
/// is left for an operator to read directly. Whitespace and `=`
/// alignment vary between macOS releases, so the parser is forgiving.
fn parse_launchctl_print(stdout: &str) -> LaunchctlPrint {
    let mut out = LaunchctlPrint::default();
    for line in stdout.lines() {
        let trimmed = line.trim();
        if let Some(value) = strip_kv(trimmed, "state") {
            out.state = Some(value);
        } else if let Some(value) = strip_kv(trimmed, "pid") {
            out.pid = Some(value);
        }
    }
    out
}

fn strip_kv(line: &str, key: &str) -> Option<String> {
    let rest = line.strip_prefix(key)?;
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('=')?;
    Some(rest.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn user_plist_contains_required_keys() {
        let dict = render_user_plist(
            &PathBuf::from("/usr/local/bin/kei"),
            &PathBuf::from("/Users/alice/.config/kei/config.toml"),
            &PathBuf::from("/Users/alice/Library/Logs/kei"),
            &PathBuf::from("/Users/alice"),
        );
        assert_eq!(
            dict.get("Label").and_then(|v| v.as_string()),
            Some(SERVICE_IDENTIFIER),
        );
        assert_eq!(
            dict.get("RunAtLoad").and_then(|v| v.as_boolean()),
            Some(true),
        );
        assert_eq!(
            dict.get("WorkingDirectory").and_then(|v| v.as_string()),
            Some("/Users/alice"),
        );
        assert_eq!(
            dict.get("StandardOutPath").and_then(|v| v.as_string()),
            Some("/Users/alice/Library/Logs/kei/stdout.log"),
        );
        assert_eq!(
            dict.get("StandardErrorPath").and_then(|v| v.as_string()),
            Some("/Users/alice/Library/Logs/kei/stderr.log"),
        );
        assert_eq!(
            dict.get("ProcessType").and_then(|v| v.as_string()),
            Some("Background"),
        );
    }

    #[test]
    fn program_arguments_are_absolute_and_carry_config_flag() {
        let dict = render_user_plist(
            &PathBuf::from("/opt/homebrew/bin/kei"),
            &PathBuf::from("/Users/bob/.config/kei/config.toml"),
            &PathBuf::from("/Users/bob/Library/Logs/kei"),
            &PathBuf::from("/Users/bob"),
        );
        let args = dict
            .get("ProgramArguments")
            .and_then(|v| v.as_array())
            .expect("ProgramArguments must be an array");
        let strings: Vec<&str> = args.iter().filter_map(|v| v.as_string()).collect();
        assert_eq!(
            strings,
            vec![
                "/opt/homebrew/bin/kei",
                "service",
                "run",
                "--config",
                "/Users/bob/.config/kei/config.toml",
            ],
        );
    }

    #[test]
    fn keep_alive_uses_network_state_predicate() {
        let dict = render_user_plist(
            &PathBuf::from("/usr/local/bin/kei"),
            &PathBuf::from("/tmp/config.toml"),
            &PathBuf::from("/tmp/logs"),
            &PathBuf::from("/tmp"),
        );
        let keep = dict
            .get("KeepAlive")
            .and_then(|v| v.as_dictionary())
            .expect("KeepAlive must be a dict");
        assert_eq!(
            keep.get("NetworkState").and_then(|v| v.as_boolean()),
            Some(true),
        );
    }

    #[test]
    fn rendered_plist_round_trips_through_serializer() {
        // Round-trip via plist::to_writer_xml + plist::from_bytes is the
        // closest local check to what launchctl will see at install
        // time. If the dict has an invalid type or unsupported value
        // shape, this test fails before the user does.
        let dict = render_user_plist(
            &PathBuf::from("/usr/local/bin/kei"),
            &PathBuf::from("/Users/carol/.config/kei/config.toml"),
            &PathBuf::from("/Users/carol/Library/Logs/kei"),
            &PathBuf::from("/Users/carol"),
        );
        let xml = serialize_plist(&dict).expect("serialize");
        let reparsed: Dictionary = plist::from_bytes(xml.as_bytes())
            .expect("plist must round-trip through XML serializer");
        assert_eq!(
            reparsed.get("Label").and_then(|v| v.as_string()),
            Some(SERVICE_IDENTIFIER),
        );
        // Sanity: the XML payload itself should look like a plist.
        assert!(
            xml.contains("<plist") && xml.contains("<dict>"),
            "expected plist XML, got:\n{xml}",
        );
    }

    #[test]
    fn render_status_reports_not_installed() {
        assert_eq!(
            render_status(StatusInputs::NotInstalled),
            "Service: not installed",
        );
    }

    #[test]
    fn render_status_reports_domain_unavailable() {
        assert_eq!(
            render_status(StatusInputs::DomainUnavailable),
            "Service: installed (launchd user, domain unavailable)",
        );
    }

    #[test]
    fn render_status_running_includes_pid_when_present() {
        assert_eq!(
            render_status(StatusInputs::Probed {
                state: "running".to_string(),
                pid: Some("12345".to_string()),
            }),
            "Service: running (launchd user, pid 12345)",
        );
    }

    #[test]
    fn render_status_running_omits_dash_pid() {
        // launchctl print emits `pid = -` for loaded-but-stopped
        // services in some macOS versions; that's not a real PID and
        // shouldn't pollute the status line.
        assert_eq!(
            render_status(StatusInputs::Probed {
                state: "running".to_string(),
                pid: Some("-".to_string()),
            }),
            "Service: running (launchd user)",
        );
    }

    #[test]
    fn render_status_passes_through_non_running_state() {
        assert_eq!(
            render_status(StatusInputs::Probed {
                state: "not running".to_string(),
                pid: None,
            }),
            "Service: not running (launchd user)",
        );
    }

    #[test]
    fn parse_launchctl_print_extracts_state_and_pid() {
        let raw = "\
com.rhoopr.kei = {
    type = LaunchAgent
    handle = 12345
    state = running
    pid = 12345
    program = /usr/local/bin/kei
    arguments = {
        /usr/local/bin/kei
        service
        run
        --config
        /Users/alice/.config/kei/config.toml
    }
}
";
        let parsed = parse_launchctl_print(raw);
        assert_eq!(parsed.state.as_deref(), Some("running"));
        assert_eq!(parsed.pid.as_deref(), Some("12345"));
    }

    #[test]
    fn parse_launchctl_print_handles_loaded_but_stopped() {
        let raw = "\
com.rhoopr.kei = {
    state = not running
    pid = -
}
";
        let parsed = parse_launchctl_print(raw);
        assert_eq!(parsed.state.as_deref(), Some("not running"));
        assert_eq!(parsed.pid.as_deref(), Some("-"));
    }

    #[test]
    fn detects_domain_unavailable_strings() {
        assert!(is_domain_unavailable("Could not find domain for: gui/501"));
        assert!(is_domain_unavailable(
            "Bootstrap failed: 5: Input/output error"
        ));
        assert!(is_domain_unavailable(
            "Operation not permitted while System Integrity Protection is engaged"
        ));
        assert!(!is_domain_unavailable("service already loaded\n"));
    }

    #[test]
    fn detects_not_loaded_strings() {
        assert!(is_not_loaded("No such process"));
        assert!(is_not_loaded("Could not find specified service"));
        assert!(!is_not_loaded("Bootstrap failed"));
    }

    #[test]
    fn user_plist_path_ends_at_launch_agents() {
        if let Some(p) = user_plist_path() {
            assert!(
                p.ends_with("Library/LaunchAgents/com.rhoopr.kei.plist"),
                "expected LaunchAgents path, got {}",
                p.display(),
            );
            assert!(p.is_absolute());
        }
    }

    #[test]
    fn kei_state_dir_uses_dotted_config_path() {
        // Match the rest of kei: ~/.config/kei everywhere, not
        // ~/Library/Application Support. Regression-guard so a casual
        // refactor toward dirs::config_dir() doesn't break the macOS
        // state location.
        if let Some(p) = kei_state_dir() {
            assert!(
                p.ends_with(".config/kei"),
                "expected ~/.config/kei, got {}",
                p.display(),
            );
        }
    }
}
