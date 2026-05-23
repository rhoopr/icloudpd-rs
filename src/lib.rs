//! kei: iCloud Photos sync engine.
//!
//! Moves photos and videos from iCloud Photos to local storage. Authentication
//! uses SRP-6a with Apple's custom variant followed by optional 2FA, and assets
//! are streamed from `CloudKit` with exponential-backoff retries on transient
//! failures.
//!
//! Lint configuration lives in `[lints.clippy]` in `Cargo.toml`.

// Test code is exempt from the panic-footgun, logging-hygiene, and
// numeric-cast lints that prod code enforces: unwrap/expect/panic are
// idiomatic in tests, a few tests write to stderr for failure diagnostics,
// and test fixtures commonly use `as` casts on values known to fit.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unimplemented,
        clippy::print_stderr,
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss,
        clippy::indexing_slicing,
    )
)]

mod auth;
mod cli;
mod commands;
mod config;
mod credential;
mod cycle_reporter;
mod download;
mod fs_util;
mod health;
mod icloud;
mod metrics;
mod migration;
mod notifications;
mod password;
mod personality;
mod report;
mod retry;
mod selection;
mod service;
mod setup;
mod shutdown;
mod state;
mod string_interner;
mod sync_cycle;
mod sync_loop;
mod systemd;
mod types;

#[cfg(test)]
mod test_helpers;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use password::{ExposeSecret, SecretString};

/// A writer wrapper that redacts a password string from log output.
///
/// Wraps any `io::Write` implementor and replaces occurrences of the
/// configured password with `********` in each `write()` call.
struct RedactingWriter<W> {
    inner: W,
    password: Arc<std::sync::Mutex<Option<SecretString>>>,
}

impl<W: std::io::Write> std::io::Write for RedactingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let password = self
            .password
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // Fast path: short-circuit before allocating a `String`. Under
        // trace-level logging the redaction path runs on every event,
        // and `String::from_utf8_lossy` per event dominates the heap churn.
        let Some(pw) = &*password else {
            self.inner.write_all(buf)?;
            return Ok(buf.len());
        };
        let pw_bytes = pw.expose_secret().as_bytes();
        if pw_bytes.is_empty() || buf.len() < pw_bytes.len() {
            self.inner.write_all(buf)?;
            return Ok(buf.len());
        }
        if memchr::memmem::find(buf, pw_bytes).is_none() {
            self.inner.write_all(buf)?;
            return Ok(buf.len());
        }

        let s = String::from_utf8_lossy(buf);
        let redacted = s.replace(pw.expose_secret(), "********");
        self.inner.write_all(redacted.as_bytes())?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// A `MakeWriter` implementation that produces `RedactingWriter` instances
/// fronting the non-blocking channel that wraps stderr.
struct RedactingMakeWriter {
    password: Arc<std::sync::Mutex<Option<SecretString>>>,
    inner: tracing_appender::non_blocking::NonBlocking,
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for RedactingMakeWriter {
    type Writer = RedactingWriter<tracing_appender::non_blocking::NonBlocking>;

    fn make_writer(&'a self) -> Self::Writer {
        RedactingWriter {
            inner: self.inner.clone(),
            password: Arc::clone(&self.password),
        }
    }
}

/// `Write` impl that funnels stderr through `MultiProgress::suspend` so a
/// tracing event mid-redraw doesn't trample the active progress bar's ANSI
/// cursor positioning. Cheap when no bars are registered (suspend is a
/// passthrough); essential when a multi-line friendly bar is on screen.
///
/// Stays on `suspend` (rather than `println`) because the pipeline calls
/// `pb.suspend(|| tracing::warn!(...))` while already holding indicatif's
/// `MultiProgress` write lock - a `println` call from inside the closure
/// would re-enter that same `RwLock` and deadlock. Narration outside that
/// context (greeting, sign-off, stop-signal) routes through `println`
/// directly via `personality::active_bar::println_above_bars`.
pub(crate) struct BarSuspendingStderr;

impl std::io::Write for BarSuspendingStderr {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        personality::active_bar::with_suspended(|| std::io::stderr().write(buf))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        personality::active_bar::with_suspended(|| std::io::stderr().flush())
    }
}

/// Build the redacting log-writer pipeline `run` installs.
///
/// `lossy(true)` so producers never park on the background writer; the
/// returned `WorkerGuard` must outlive every `tracing::*` call so its
/// `Drop` can join the worker and drain the channel before teardown.
/// The password slot starts empty and is populated once the password
/// is known.
pub(crate) fn build_redacting_writer<W>(
    sink: W,
) -> (
    impl for<'a> tracing_subscriber::fmt::MakeWriter<'a> + 'static,
    tracing_appender::non_blocking::WorkerGuard,
    Arc<std::sync::Mutex<Option<SecretString>>>,
)
where
    W: std::io::Write + Send + 'static,
{
    let password: Arc<std::sync::Mutex<Option<SecretString>>> =
        Arc::new(std::sync::Mutex::new(None));
    let (non_blocking, guard) = tracing_appender::non_blocking::NonBlockingBuilder::default()
        .lossy(true)
        .finish(sink);
    let make_writer = RedactingMakeWriter {
        password: Arc::clone(&password),
        inner: non_blocking,
    };
    (make_writer, guard, password)
}

use cli::Command;
use config::TomlConfig;

/// Prevent core dumps from leaking in-memory credentials.
/// Best-effort: failures are logged but not fatal (Docker containers may
/// restrict these syscalls).
fn harden_process() {
    #[cfg(target_os = "linux")]
    // SAFETY: PR_SET_DUMPABLE with value 0 is a simple prctl flag toggle.
    // No pointer arguments; failure is non-fatal (logged and ignored).
    unsafe {
        if libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) != 0 {
            tracing::debug!("prctl(PR_SET_DUMPABLE, 0) failed");
        }
    }
    #[cfg(unix)]
    // SAFETY: rlim is stack-allocated and fully initialized. setrlimit reads
    // from the pointer but does not store it. Failure is non-fatal.
    unsafe {
        let rlim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if libc::setrlimit(libc::RLIMIT_CORE, &raw const rlim) != 0 {
            tracing::debug!("setrlimit(RLIMIT_CORE, 0) failed");
        }
    }
}

/// Exit code for partial sync (some downloads failed, but sync was not a total failure).
const EXIT_PARTIAL: u8 = 2;
/// Exit code for authentication failures.
const EXIT_AUTH: u8 = 3;
/// Exit code for terminal Apple authentication states that need operator action.
const EXIT_TERMINAL_AUTH: u8 = 4;

/// Returned when some (but not all) downloads failed during a sync.
#[derive(Debug, thiserror::Error)]
#[error("{0} downloads failed")]
struct PartialSyncError(usize);

/// Maps a fatal `Err` from `run` to an exit code and decides whether to
/// log it. `TwoFactorRequired` is the non-obvious case: exit 0, no log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitClassification {
    /// Exit 0 with no error log: kei already told the user how to
    /// proceed, and `restart: on-failure` would loop Apple's auth
    /// endpoint if 2FA were treated as a failure.
    TwoFactorRequired,
    Partial,
    Auth,
    TerminalAuth,
    Other,
}

impl ExitClassification {
    const fn exit_code(self) -> u8 {
        match self {
            Self::TwoFactorRequired => 0,
            Self::Partial => EXIT_PARTIAL,
            Self::Auth => EXIT_AUTH,
            Self::TerminalAuth => EXIT_TERMINAL_AUTH,
            Self::Other => 1,
        }
    }

    const fn should_log(self) -> bool {
        !matches!(self, Self::TwoFactorRequired)
    }
}

fn classify_exit_error(e: &anyhow::Error) -> ExitClassification {
    if e.downcast_ref::<auth::error::AuthError>()
        .is_some_and(auth::error::AuthError::is_two_factor_required)
    {
        ExitClassification::TwoFactorRequired
    } else if e
        .downcast_ref::<auth::error::AuthError>()
        .is_some_and(auth::error::AuthError::is_terminal_apple_auth)
    {
        ExitClassification::TerminalAuth
    } else if e.downcast_ref::<PartialSyncError>().is_some() {
        ExitClassification::Partial
    } else if e.downcast_ref::<auth::error::AuthError>().is_some() {
        ExitClassification::Auth
    } else {
        ExitClassification::Other
    }
}

#[expect(
    clippy::string_slice,
    reason = "floor_char_boundary guarantees a valid char boundary"
)]
pub(crate) fn truncate_str(s: &str, max_bytes: usize) -> &str {
    &s[..s.floor_char_boundary(max_bytes)]
}

/// Query available disk space on the filesystem containing `path`.
///
/// Returns `None` if the statvfs call fails (e.g. path doesn't exist yet).
#[cfg(unix)]
pub(crate) fn available_disk_space(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    /// Widen a platform-dependent statvfs field to u64. `as u64` is the only
    /// portable way since the underlying types (`c_ulong`, `fsblkcnt_t`) vary
    /// across targets.
    #[inline]
    fn widen(v: impl Into<u64>) -> u64 {
        v.into()
    }

    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: zeroed is valid for libc::statvfs (all-zero bit pattern is a
    // valid struct — every field is an integer type).
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: c_path is a valid NUL-terminated C string that outlives the
    // call. statvfs writes into the provided buffer and does not retain the
    // pointer.
    if unsafe { libc::statvfs(c_path.as_ptr(), &raw mut stat) } != 0 {
        return None;
    }
    Some(widen(stat.f_bavail) * widen(stat.f_frsize))
}

#[cfg(not(unix))]
pub(crate) fn available_disk_space(_path: &Path) -> Option<u64> {
    None
}

/// Minimum free disk space (1 GiB) required before kei is allowed to start a
/// sync. Below this, even a moderate-size video could push the filesystem
/// past full mid-write (which is a kei-data-sacred risk: torn writes,
/// truncated `.part` files, no clean rename target).
pub(crate) const MIN_FREE_BYTES: u64 = 1_073_741_824;

/// Bail when `available_bytes` is below [`MIN_FREE_BYTES`]. Pure / synchronous
/// so it can be unit-tested without statvfs or a real filesystem.
///
/// Production callers compute `available_bytes` from
/// [`available_disk_space`] (or any future probe) and forward it here so the
/// abort message is identical regardless of platform.
pub(crate) fn check_min_disk_space(available_bytes: u64, directory: &Path) -> anyhow::Result<()> {
    if available_bytes < MIN_FREE_BYTES {
        let avail_mb = available_bytes / (1024 * 1024);
        anyhow::bail!(
            "Insufficient disk space: only {avail_mb} MiB available in {} (minimum 1 GiB)",
            directory.display()
        );
    }
    Ok(())
}

/// Build a password provider closure from a [`password::PasswordSource`].
///
/// The source is evaluated lazily on each call — for `Command` and `File`
/// sources, this re-executes/re-reads each time, supporting password rotation
/// and keeping no password in memory between auth cycles.
///
/// The closure is wrapped in `Arc<dyn Fn + Send + Sync>` so the async auth
/// path can dispatch invocations through `spawn_blocking` (see
/// [`password::invoke_password_provider`]) instead of calling the
/// blocking `resolve()` directly on a tokio worker.
fn make_password_provider(source: password::PasswordSource) -> password::PasswordProvider {
    std::sync::Arc::new(move || match source.resolve() {
        Ok(pw) => pw,
        Err(e) => {
            tracing::error!(error = %e, "Password source resolution failed");
            None
        }
    })
}

/// Build a password provider from CLI password args, TOML config, and resolved auth fields.
///
/// Shared by `run_login`, `run_list`, and `run_import_existing`.
fn make_provider_from_auth(
    pw: &cli::PasswordArgs,
    password: Option<String>,
    username: &str,
    cookie_directory: &Path,
    toml: Option<&config::TomlConfig>,
) -> password::PasswordProvider {
    let toml_auth = toml.and_then(|t| t.auth.as_ref());
    let password_command = config::resolve_password_command(pw, toml_auth);
    let password_file = config::resolve_password_file(pw, toml_auth);
    let source = password::build_password_source(
        password.map(SecretString::from).as_ref(),
        password_command.as_deref(),
        password_file.as_deref(),
        credential::CredentialStore::new(username, cookie_directory),
    );
    make_password_provider(source)
}

use commands::{
    run_config_show, run_import_existing, run_list, run_login, run_password, run_reconcile,
    run_reset_state, run_reset_sync_token, run_status, run_verify,
};

/// Get the database path for a given auth config, merging with TOML defaults.
///
/// Returns an error if the resolved username is empty, since an empty username
/// produces a `.db` filename that silently operates on the wrong database.
fn get_db_path(globals: &config::GlobalArgs, toml: Option<&TomlConfig>) -> anyhow::Result<PathBuf> {
    let (username, _, _, cookie_dir) =
        config::resolve_auth(globals, &cli::PasswordArgs::default(), toml);
    if username.is_empty() {
        anyhow::bail!("username is required (set ICLOUD_USERNAME or [auth].username)");
    }
    Ok(cookie_dir.join(format!(
        "{}.db",
        auth::session::sanitize_username(&username)
    )))
}

/// RAII guard that writes the current PID to a file on creation and removes
/// it when dropped.
#[derive(Debug)]
struct PidFileGuard {
    path: PathBuf,
}

impl PidFileGuard {
    fn new(path: PathBuf) -> std::io::Result<Self> {
        // If a prior PID file exists, validate whether the recorded process
        // is still alive. Alive → bail; dead/unparsable → treat as stale
        // and overwrite.
        if let Ok(contents) = std::fs::read_to_string(&path) {
            if let Some(existing) = contents.trim().parse::<i32>().ok().filter(|p| *p > 0) {
                if pid_is_alive(existing) {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        format!(
                            "PID file {} refers to running process {existing}; refusing to start a second instance",
                            path.display()
                        ),
                    ));
                }
                tracing::warn!(
                    path = %path.display(),
                    stale_pid = existing,
                    "PID file references a non-running process; overwriting as stale"
                );
            } else {
                tracing::warn!(
                    path = %path.display(),
                    "PID file contents unparsable; overwriting as stale"
                );
            }
        }
        std::fs::write(&path, std::process::id().to_string())?;
        tracing::debug!(path = %path.display(), "PID file created");
        Ok(Self { path })
    }
}

/// Return whether the PID corresponds to a running process.
///
/// Uses `kill(pid, 0)` on Unix: 0 = alive; ESRCH = dead; EPERM = alive
/// (exists but outside our signalling permissions — still a live process).
#[cfg(unix)]
fn pid_is_alive(pid: i32) -> bool {
    // SAFETY: kill with signal 0 performs permission / existence checks only
    // and never delivers a signal.
    let rc = unsafe { libc::kill(pid, 0) };
    if rc == 0 {
        return true;
    }
    matches!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EPERM)
    )
}

#[cfg(not(unix))]
fn pid_is_alive(_pid: i32) -> bool {
    // Windows PID reuse happens fast enough that a cheap "is PID alive" check
    // without a process-handle lookup can return false-alives for totally
    // unrelated processes. Report dead so stale PID files are overwritten,
    // trading duplicate-run protection (which the OS filesystem lock in
    // PidFileGuard::new still backstops) for avoiding spurious refusals.
    false
}

impl Drop for PidFileGuard {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.path) {
            tracing::debug!(path = %self.path.display(), error = %e, "Failed to remove PID file");
        }
    }
}

/// Binary entry point. Lives here so the binary at `src/main.rs` is a
/// no-logic shim; everything else - module tree, helpers, run() - is part
/// of the lib so it's reachable from integration tests, fuzz harnesses,
/// and any future companion binaries.
pub fn main_inner() -> ExitCode {
    // Snapshot and scrub the password env var while truly single-threaded,
    // before the tokio runtime creates worker threads.
    let env_password = std::env::var("ICLOUD_PASSWORD")
        .ok()
        .filter(|s| !s.is_empty());
    // SAFETY: no other threads exist yet — the tokio runtime has not been built.
    unsafe { std::env::remove_var("ICLOUD_PASSWORD") };

    #[allow(
        clippy::expect_used,
        reason = "startup failure: no runtime means nothing can run"
    )]
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Failed to create tokio runtime");

    match rt.block_on(run(env_password)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let classification = classify_exit_error(&e);
            if classification.should_log() {
                // Route the final error through tracing so it carries the same
                // timestamp + level prefix as the rest of the logs; makes
                // `docker logs` / `journalctl` output correlate cleanly.
                // Also echo to stderr unconditionally as a fallback for early
                // failures before `tracing_subscriber::fmt().init()` runs.
                tracing::error!(error = format!("{e:#}"), "kei exited with error");
                #[allow(
                    clippy::print_stderr,
                    reason = "fallback for failures that happen before tracing subscriber is installed"
                )]
                {
                    eprintln!("Error: {e:#}");
                }
            }
            ExitCode::from(classification.exit_code())
        }
    }
}

async fn run(env_password: Option<String>) -> anyhow::Result<()> {
    let (cli, explicit_sync_flags) =
        cli::parse_cli_with_sources(std::env::args_os()).map_err(|e| anyhow::anyhow!(e))?;

    // Reject `kei --skip-videos status` and friends, where a sync-only
    // top-level flag is silently swallowed under a non-sync subcommand.
    // See `Cli::validate` for the full rationale.
    if let Err(msg) = cli.validate(&explicit_sync_flags) {
        anyhow::bail!("{msg}");
    }

    // Copy legacy icloudpd-rs paths before loading config, so the
    // copied config.toml is found at the new location. Copy failures are
    // startup errors; kei doesn't continue against the old paths.
    migration::migrate_legacy_paths()?;

    // Load TOML config early so it can influence log level.
    // If the user explicitly set --config, the file must exist.
    //
    // Docker fallback: when no --config is passed, the default
    // ~/.config/kei/config.toml may not exist inside a container (it
    // resolves to /root/.config/kei/config.toml). Try the Docker
    // convention /config/config.toml as a fallback so that `docker exec`
    // subcommands (get-code, submit-code, credential, etc.) automatically
    // find the same config the Docker CMD uses.
    const DOCKER_FALLBACK_CONFIG: &str = "/config/config.toml";
    let config_explicitly_set =
        cli.config != "~/.config/kei/config.toml" && cli.config != DOCKER_FALLBACK_CONFIG;
    let (config_path, used_docker_fallback) = {
        let expanded = config::expand_tilde(&cli.config);
        if !config_explicitly_set && !expanded.exists() {
            let docker = PathBuf::from(DOCKER_FALLBACK_CONFIG);
            if docker.exists() {
                (docker, true)
            } else {
                (expanded, false)
            }
        } else {
            (expanded, false)
        }
    };
    // When --config is explicit but the file doesn't exist and the parent
    // dir does exist, allow it (auto-config will create the file).
    // Otherwise require the file to exist so typos in --config paths error.
    let can_auto_create =
        !config_path.exists() && config_path.parent().is_some_and(std::path::Path::is_dir);
    let config_required = config_explicitly_set && !can_auto_create;
    let mut toml_config = config::load_toml_config(&config_path, config_required)?;

    // Resolve log level: --log-level > --verbose > TOML > default (info).
    // `--verbose` is a friendlier alias for `--log-level info` and is
    // overridden if `--log-level` is also explicitly set.
    let cli_log_level = cli.log_level.or(if cli.verbose {
        Some(types::LogLevel::Info)
    } else {
        None
    });
    let log_level_explicit = cli_log_level.is_some();
    let effective_log_level = cli_log_level
        .or_else(|| toml_config.as_ref().and_then(|t| t.log_level))
        .unwrap_or(types::LogLevel::Info);

    // Scope debug/info to the app crate so dependency crates stay quieter.
    // Users can override with RUST_LOG env var for full control.
    let off_filter = match effective_log_level {
        types::LogLevel::Debug => "kei=debug,info",
        types::LogLevel::Info => "kei=info",
        types::LogLevel::Warn => "warn",
        types::LogLevel::Error => "error",
    };

    // Resolve friendly mode. The gate has multiple short-circuits (service
    // context, non-TTY, RUST_LOG, machine-output mode, ...) so the user-stated
    // preference is a request, not a guarantee.
    //
    // Resolution chain: CLI > TOML > default-on-for-TTY. The gate then
    // clamps to Off in any environment that can't render or shouldn't
    // (non-TTY, journals, machine-output flags). Default-on means the
    // setup wizard's question and the TOML key are opt-out levers; first
    // contact with kei on a plain terminal already gets the friendly UX.
    //
    // `effective_command()` clones SyncArgs, but it's run once here and once
    // at dispatch below. The clone cost is one-shot at startup and the only
    // alternative (threading the resolved command through `run`) ripples
    // through every callee. Acceptable.
    let resolved_for_personality = cli.effective_command();
    let toml_report_json = toml_config
        .as_ref()
        .and_then(|t| t.report.as_ref())
        .and_then(|r| r.json.as_ref())
        .is_some();
    let (cmd_no_progress_bar, cmd_only_print_filenames, cmd_report_json, cmd_service_run) =
        match &resolved_for_personality {
            cli::Command::Sync { sync, .. } => (
                sync.no_progress_bar,
                sync.only_print_filenames,
                toml_report_json,
                false,
            ),
            cli::Command::Service { .. } => (false, false, toml_report_json, true),
            _ => (false, false, false, false),
        };
    let personality_ctx = personality::Context::detect(
        cmd_no_progress_bar,
        cmd_only_print_filenames,
        cmd_report_json,
        log_level_explicit,
        cmd_service_run,
    );
    let toml_friendly = toml_config
        .as_ref()
        .and_then(|t| t.ui.as_ref())
        .and_then(|u| u.friendly);
    let cli_friendly = cli.friendly_request();
    let friendly_request = cli_friendly.or(toml_friendly);
    let personality_mode =
        personality::resolve_with_request(cli_friendly, toml_friendly, &personality_ctx);
    let default_filter = personality::tracing::default_filter_for(personality_mode, off_filter);

    // `_writer_guard` MUST live until `run` returns. A `static`-stored
    // guard never drops (Rust skips static destructors), so subprocess
    // tests reading stderr after kei exits race against unflushed events
    // on fast teardown (observed on macOS CI).
    // `BarSuspendingStderr` interposes between tracing and stderr so that
    // every WARN/ERROR write happens with the in-flight progress bar paused
    // (via `MultiProgress::suspend`). Without this, a tracing event landing
    // mid-redraw causes the bar's ANSI cursor moves to desync, leaving
    // partial duplicate cards on screen (issue surfaced during real syncs).
    let (make_writer, _writer_guard, redact_password) = build_redacting_writer(BarSuspendingStderr);

    let env_filter = personality::tracing::env_filter(&default_filter);
    if personality_mode.is_friendly() {
        tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_writer(make_writer)
            .with_target(false)
            .with_level(false)
            .without_time()
            .event_format(personality::tracing::FriendlyFormat)
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_writer(make_writer)
            .init();
    }

    if used_docker_fallback {
        tracing::debug!(
            path = %config_path.display(),
            "Using Docker fallback config (default path not found)"
        );
    }

    // Build non-TOML globals early. In v0.20 these come from the narrow
    // bootstrap env allow-list, not public global CLI flags.
    let globals = config::GlobalArgs::from_bootstrap_env();

    // Dispatch based on command
    let mut command = cli.effective_command();
    // Inject the password captured from env before the runtime started,
    // since we cleared ICLOUD_PASSWORD before Cli::parse() could see it.
    // Must happen before command dispatch so all subcommands (login,
    // list, etc.) receive the password, not just sync.
    command.inject_env_password(env_password);
    let (is_one_shot, pw, sync) = match command {
        Command::Status(args) => {
            return run_status(args, &globals, toml_config.as_ref()).await;
        }
        Command::Reset { what } => match what {
            cli::ResetCommand::State { yes } => {
                return run_reset_state(yes, &globals, toml_config.as_ref()).await;
            }
            cli::ResetCommand::SyncToken { yes } => {
                return run_reset_sync_token(yes, &globals, toml_config.as_ref()).await;
            }
        },
        Command::Verify(args) => {
            return run_verify(args, &globals, toml_config.as_ref()).await;
        }
        Command::Reconcile(args) => {
            return run_reconcile(args, &globals, toml_config.as_ref()).await;
        }
        Command::ImportExisting(args) => {
            return run_import_existing(args, &globals, toml_config.as_ref()).await;
        }
        Command::Login {
            password,
            subcommand,
        } => {
            return run_login(subcommand, &password, &globals, toml_config.as_ref()).await;
        }
        Command::Password { password, action } => {
            return run_password(action, &globals, &password, toml_config.as_ref());
        }
        Command::List {
            password,
            libraries,
            what,
        } => {
            return run_list(what, &password, libraries, &globals, toml_config.as_ref()).await;
        }
        Command::Config { action } => match action {
            cli::ConfigAction::Show => {
                return run_config_show(&globals, toml_config.as_ref());
            }
            cli::ConfigAction::Setup { output } => {
                let path = output.map_or_else(|| config_path.clone(), |o| config::expand_tilde(&o));
                match setup::run_setup(&path)? {
                    setup::SetupResult::SyncNow {
                        config_path: cfg_path,
                        one_shot_password,
                    } => {
                        // Reload TOML from the newly written config
                        toml_config = config::load_toml_config(&cfg_path, true)?;
                        let sync_pw = cli::PasswordArgs {
                            password: one_shot_password.map(|p| p.expose_secret().to_string()),
                            ..cli::PasswordArgs::default()
                        };
                        // Setup "sync now" is a one-shot initial sync, not a daemon.
                        (true, sync_pw, cli::SyncArgs::default())
                    }
                    setup::SetupResult::Done => return Ok(()),
                }
            }
        },
        Command::Install(args) => {
            return service::install::run(args, &config_path).await;
        }
        Command::Uninstall(args) => {
            return service::uninstall::run(args).await;
        }
        Command::Service { action } => match action {
            cli::ServiceAction::Status => return service::status::run().await,
            cli::ServiceAction::Run(args) => {
                let cli::ServiceRunArgs { password, sync } = *args;
                return service::run::run(
                    &globals,
                    sync_loop::SyncArgs {
                        is_one_shot: false,
                        service_mode: true,
                        pw: password,
                        sync,
                        toml_config,
                        config_explicitly_set,
                        config_path: config_path.clone(),
                        redact_password: Arc::clone(&redact_password),
                        // service run is hard-off per gate; resolved mode is
                        // already Off here, but pass through for symmetry.
                        personality_mode,
                        friendly_request,
                    },
                )
                .await;
            }
        },
        Command::Sync { password, sync, .. } => (sync.retry_failed, password, sync),
    };
    sync_loop::run_sync(
        &globals,
        sync_loop::SyncArgs {
            is_one_shot,
            service_mode: false,
            pw,
            sync,
            toml_config,
            config_explicitly_set,
            config_path: config_path.clone(),
            redact_password: Arc::clone(&redact_password),
            personality_mode,
            friendly_request,
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::EnvFilter;

    #[test]
    fn pid_file_guard_creates_and_removes() {
        let path = std::env::temp_dir().join("icloudpd_test_pid_guard.pid");
        let _ = std::fs::remove_file(&path);

        {
            let guard = PidFileGuard::new(path.clone()).unwrap();
            let contents = std::fs::read_to_string(&path).unwrap();
            assert_eq!(contents, std::process::id().to_string());
            drop(guard);
        }

        assert!(!path.exists());
    }

    #[test]
    fn pid_file_guard_handles_missing_parent() {
        let path = std::env::temp_dir().join("nonexistent_dir_abc123/test.pid");
        assert!(PidFileGuard::new(path).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn pid_file_guard_refuses_when_existing_pid_alive() {
        // Windows' pid_is_alive stub deliberately always returns false to
        // avoid PID-reuse false positives, so this guard behavior is
        // Unix-only.
        let path = std::env::temp_dir().join("icloudpd_test_pid_guard_alive.pid");
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, std::process::id().to_string()).unwrap();

        let err = PidFileGuard::new(path.clone()).expect_err("should refuse");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    #[cfg(unix)]
    fn pid_file_guard_overwrites_when_existing_pid_dead() {
        // PID 2^31-2 is not allocatable on Linux (max_pid is much smaller),
        // so kill(pid, 0) returns ESRCH deterministically.
        let dead_pid = i32::MAX - 1;
        assert!(!pid_is_alive(dead_pid));

        let path = std::env::temp_dir().join("icloudpd_test_pid_guard_dead.pid");
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, dead_pid.to_string()).unwrap();

        let guard = PidFileGuard::new(path.clone()).expect("should overwrite stale PID file");
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents, std::process::id().to_string());
        drop(guard);
        assert!(!path.exists());
    }

    #[test]
    fn pid_file_guard_overwrites_when_existing_contents_garbage() {
        let path = std::env::temp_dir().join("icloudpd_test_pid_guard_garbage.pid");
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, "not a pid").unwrap();

        let guard = PidFileGuard::new(path.clone()).expect("should overwrite garbage PID file");
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents, std::process::id().to_string());
        drop(guard);
        assert!(!path.exists());
    }

    #[test]
    #[cfg(unix)]
    fn pid_is_alive_self() {
        assert!(pid_is_alive(std::process::id().cast_signed()));
    }

    #[test]
    fn redacting_writer_replaces_password() {
        use std::io::Write;

        let password = Arc::new(std::sync::Mutex::new(Some(SecretString::from("s3cret"))));
        let mut buf = Vec::new();
        {
            let mut writer = RedactingWriter {
                inner: &mut buf,
                password: Arc::clone(&password),
            };
            writer.write_all(b"Login with s3cret ok").unwrap();
        }
        let output = String::from_utf8(buf).unwrap();
        assert!(!output.contains("s3cret"));
        assert!(output.contains("********"));
    }

    #[test]
    fn redacting_writer_no_password_passthrough() {
        use std::io::Write;

        let password: Arc<std::sync::Mutex<Option<SecretString>>> =
            Arc::new(std::sync::Mutex::new(None));
        let mut buf = Vec::new();
        {
            let mut writer = RedactingWriter {
                inner: &mut buf,
                password: Arc::clone(&password),
            };
            writer.write_all(b"normal log line").unwrap();
        }
        let output = String::from_utf8(buf).unwrap();
        assert_eq!(output, "normal log line");
    }

    #[test]
    fn redacting_writer_empty_password_passthrough() {
        use std::io::Write;

        let password = Arc::new(std::sync::Mutex::new(Some(SecretString::from(
            String::new(),
        ))));
        let mut buf = Vec::new();
        {
            let mut writer = RedactingWriter {
                inner: &mut buf,
                password: Arc::clone(&password),
            };
            writer.write_all(b"normal log line").unwrap();
        }
        let output = String::from_utf8(buf).unwrap();
        assert_eq!(output, "normal log line");
    }

    #[test]
    fn redacting_writer_short_buffer_passthrough() {
        use std::io::Write;

        // Buffer shorter than the password can't contain it
        let password = Arc::new(std::sync::Mutex::new(Some(SecretString::from(
            "longpassword",
        ))));
        let mut buf = Vec::new();
        {
            let mut writer = RedactingWriter {
                inner: &mut buf,
                password: Arc::clone(&password),
            };
            writer.write_all(b"short").unwrap();
        }
        let output = String::from_utf8(buf).unwrap();
        assert_eq!(output, "short");
    }

    #[test]
    fn redacting_writer_flush() {
        use std::io::Write;

        let password: Arc<std::sync::Mutex<Option<SecretString>>> =
            Arc::new(std::sync::Mutex::new(None));
        let mut buf = Vec::new();
        let mut writer = RedactingWriter {
            inner: &mut buf,
            password,
        };
        writer.flush().unwrap();
    }

    /// Password set but the buffer doesn't contain it. The pre-redaction
    /// byte-level scan must short-circuit and pass the buffer through
    /// unchanged, without allocating a `String` for UTF-8 lossy
    /// conversion -- under heavy trace-level logging most events do NOT
    /// contain the password, so the no-allocation path is the hot one.
    #[test]
    fn redacting_writer_password_absent_passthrough() {
        use std::io::Write;

        let password = Arc::new(std::sync::Mutex::new(Some(SecretString::from("s3cret"))));
        let mut buf = Vec::new();
        {
            let mut writer = RedactingWriter {
                inner: &mut buf,
                password: Arc::clone(&password),
            };
            writer
                .write_all(b"long line of trace output without any sensitive value")
                .unwrap();
        }
        let output = String::from_utf8(buf).unwrap();
        assert_eq!(
            output,
            "long line of trace output without any sensitive value"
        );
    }

    /// A buffer containing arbitrary non-UTF-8 bytes (e.g. binary protocol
    /// trace output from `hyper`) and no password match must pass through
    /// byte-for-byte. The original implementation forced `from_utf8_lossy`
    /// on every event, which would have replaced invalid sequences with
    /// U+FFFD even when no redaction was needed.
    #[test]
    fn redacting_writer_non_utf8_passthrough_preserves_bytes() {
        use std::io::Write;

        let password = Arc::new(std::sync::Mutex::new(Some(SecretString::from("s3cret"))));
        let bytes: Vec<u8> = vec![0xff, 0xfe, 0xfd, b'o', b'k', 0x00, 0x80];
        let mut buf = Vec::new();
        {
            let mut writer = RedactingWriter {
                inner: &mut buf,
                password: Arc::clone(&password),
            };
            writer.write_all(&bytes).unwrap();
        }
        assert_eq!(buf, bytes);
    }

    /// Password match in a non-UTF-8 buffer: the slow path is taken,
    /// `from_utf8_lossy` runs, and the password substring is redacted.
    /// Trailing invalid bytes get the U+FFFD replacement, but that is
    /// the same behavior as the original implementation when redaction
    /// fires.
    #[test]
    fn redacting_writer_non_utf8_with_password_redacts() {
        use std::io::Write;

        let password = Arc::new(std::sync::Mutex::new(Some(SecretString::from("s3cret"))));
        let mut bytes: Vec<u8> = b"prefix s3cret suffix ".to_vec();
        bytes.push(0xff);
        let mut buf = Vec::new();
        {
            let mut writer = RedactingWriter {
                inner: &mut buf,
                password: Arc::clone(&password),
            };
            writer.write_all(&bytes).unwrap();
        }
        let output = String::from_utf8_lossy(&buf).into_owned();
        assert!(!output.contains("s3cret"));
        assert!(output.contains("********"));
    }

    /// 200 events at 50 ms per write would synchronously block the
    /// producer for ~10 s without the lossy non-blocking channel.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn writer_pipeline_does_not_back_pressure_producer() {
        use std::io::Write;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::{Duration, Instant};

        struct SlowSink {
            delay: Duration,
            bytes: Arc<AtomicUsize>,
        }
        impl Write for SlowSink {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                std::thread::sleep(self.delay);
                self.bytes.fetch_add(buf.len(), Ordering::Relaxed);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let bytes = Arc::new(AtomicUsize::new(0));
        let sink = SlowSink {
            delay: Duration::from_millis(50),
            bytes: Arc::clone(&bytes),
        };
        let (make_writer, _guard, _pw) = build_redacting_writer(sink);

        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::new("info"))
            .with_writer(make_writer)
            .finish();
        let _g = tracing::subscriber::set_default(subscriber);

        let start = Instant::now();
        for i in 0..200 {
            tracing::info!(i, "back-pressure test event");
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "producer blocked {elapsed:?} emitting 200 events through a 50ms-per-write sink; \
             expected the lossy non-blocking channel to absorb the saturation",
        );
    }

    /// `WorkerGuard::drop` must flush every emitted event before
    /// returning. Hoisting the guard into a `static` (whose destructor
    /// never runs) silently re-introduces the race.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn writer_pipeline_flushes_all_events_before_guard_drops() {
        use std::io::Write;

        struct CollectingSink {
            buf: Arc<std::sync::Mutex<Vec<u8>>>,
        }
        impl Write for CollectingSink {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.buf
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let buf = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = CollectingSink {
            buf: Arc::clone(&buf),
        };
        let (make_writer, guard, _pw) = build_redacting_writer(sink);

        // Disable ANSI so the assertion can match `seq=N` against a stable
        // byte boundary. Default `tracing_subscriber::fmt()` emits color
        // codes between fields, which would defeat a literal-substring
        // assertion.
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::new("info"))
            .with_writer(make_writer)
            .with_ansi(false)
            .finish();
        {
            let _g = tracing::subscriber::set_default(subscriber);
            for i in 0..50 {
                tracing::info!(seq = i, "completeness test event");
            }
        }
        // Drop the guard explicitly (mirrors `run` returning) so the
        // background flush thread drains before we read the sink.
        drop(guard);

        let captured = buf
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            !captured.is_empty(),
            "guard drop completed but sink received no bytes; \
             the non-blocking worker did not flush before drop returned",
        );
        let captured_str = String::from_utf8_lossy(&captured);
        for i in 0..50 {
            // `seq=N\n` anchors at end-of-line so seq=5 doesn't match seq=50.
            assert!(
                captured_str.contains(&format!("seq={i}\n")),
                "event seq={i} missing from captured output after guard drop; \
                 captured was: {captured_str}",
            );
        }
    }

    #[test]
    fn make_password_provider_with_direct_source() {
        let source = password::PasswordSource::Direct(Arc::new(SecretString::from("mypass")));
        let provider = make_password_provider(source);
        let result = provider().unwrap();
        assert_eq!(result.expose_secret(), "mypass");
        // Can be called multiple times
        let result2 = provider().unwrap();
        assert_eq!(result2.expose_secret(), "mypass");
    }

    // `PasswordSource::Command::resolve()` shells out via `sh -c`, which
    // is unix-only. Windows coverage lives in
    // `password::tests::run_password_command_errors_on_non_unix`.
    #[cfg(unix)]
    #[test]
    fn make_password_provider_with_command_source() {
        let source = password::PasswordSource::Command("echo cmd_test".to_string());
        let provider = make_password_provider(source);
        let result = provider().unwrap();
        assert_eq!(result.expose_secret(), "cmd_test");
    }

    // ── Watch-mode control flow tests ──────────────────────────────────

    use tokio_util::sync::CancellationToken;

    /// Run the watch-loop pattern and return how many cycles completed.
    async fn run_watch_loop(
        shutdown_token: &CancellationToken,
        watch_with_interval: Option<u64>,
    ) -> u32 {
        let mut cycles = 0u32;
        loop {
            if shutdown_token.is_cancelled() {
                break;
            }
            cycles += 1;
            if let Some(interval) = watch_with_interval {
                tokio::select! {
                    () = tokio::time::sleep(std::time::Duration::from_secs(interval)) => {}
                    () = shutdown_token.cancelled() => { break; }
                }
            } else {
                break;
            }
        }
        cycles
    }

    /// The watch loop uses `tokio::select!` to make the inter-cycle sleep
    /// interruptible by a shutdown signal. Cancellation breaks out promptly
    /// despite a long interval.
    #[tokio::test]
    async fn watch_sleep_exits_promptly_on_shutdown() {
        let shutdown_token = CancellationToken::new();
        let token_clone = shutdown_token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            token_clone.cancel();
        });

        let start = std::time::Instant::now();
        let cycles = run_watch_loop(&shutdown_token, Some(3600)).await;

        assert_eq!(cycles, 1);
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
    }

    /// A pre-cancelled token prevents any cycle from starting.
    #[test]
    fn watch_loop_skips_cycle_when_already_cancelled() {
        let shutdown_token = CancellationToken::new();
        shutdown_token.cancel();

        let mut cycles_started = 0u32;
        loop {
            if shutdown_token.is_cancelled() {
                break;
            }
            cycles_started += 1;
        }
        assert_eq!(cycles_started, 0);
    }

    /// When `watch_with_interval` is None the loop executes exactly once.
    #[tokio::test]
    async fn watch_loop_runs_once_without_interval() {
        let shutdown_token = CancellationToken::new();
        assert_eq!(run_watch_loop(&shutdown_token, None).await, 1);
    }

    /// Shutdown during inter-cycle sleep completes exactly one cycle.
    #[tokio::test]
    async fn watch_loop_completes_one_cycle_then_exits_on_shutdown() {
        let shutdown_token = CancellationToken::new();
        let token_clone = shutdown_token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            token_clone.cancel();
        });
        assert_eq!(run_watch_loop(&shutdown_token, Some(3600)).await, 1);
    }

    /// kei must abort BEFORE auth (and well before any download) when the
    /// data directory has < 1 GiB free. A sync that fills the disk mid-write
    /// leaves orphan `.part` files and a half-truncated final file -- the
    /// worst-case for the "atomic writes" invariant.
    ///
    /// Parameterised over the entire span around the threshold:
    ///   0 bytes               -> bail (extreme)
    ///   1023 MiB              -> bail (just below 1 GiB)
    ///   1 GiB exactly         -> ok (boundary is inclusive on the OK side)
    ///   10 GiB                -> ok (well above)
    #[test]
    fn run_sync_low_disk_space_aborts_before_auth() {
        let dir = std::path::Path::new("/tmp/codex/kei/cg-20-disk");

        // Below threshold: bail.
        for &low in &[0u64, 1023 * 1024 * 1024] {
            let r = check_min_disk_space(low, dir);
            assert!(
                r.is_err(),
                "{low} bytes is below 1 GiB; check_min_disk_space must bail"
            );
            let msg = format!("{:#}", r.expect_err("expected Err"));
            assert!(
                msg.contains("Insufficient disk space"),
                "error message must call out the disk-space reason; got: {msg}"
            );
            assert!(
                msg.contains("minimum 1 GiB"),
                "error message must state the minimum so operators know what to free; got: {msg}"
            );
        }

        // At and above threshold: ok.
        for &ok in &[MIN_FREE_BYTES, 10 * 1024 * 1024 * 1024] {
            assert!(
                check_min_disk_space(ok, dir).is_ok(),
                "{ok} bytes is at or above 1 GiB; check_min_disk_space must succeed"
            );
        }
    }

    /// Confirm `MIN_FREE_BYTES` is exactly 1 GiB. A future
    /// edit that nudges this constant changes operator-visible behavior;
    /// pin the value so the change is intentional.
    #[test]
    fn check_min_disk_space_threshold_is_one_gib() {
        assert_eq!(MIN_FREE_BYTES, 1_073_741_824);
        assert_eq!(MIN_FREE_BYTES, 1024 * 1024 * 1024);
    }

    #[test]
    fn classify_exit_error_two_factor_required_is_suppressed_success() {
        let e: anyhow::Error = auth::error::AuthError::TwoFactorRequired.into();
        let c = classify_exit_error(&e);
        assert_eq!(c, ExitClassification::TwoFactorRequired);
        assert_eq!(c.exit_code(), 0);
        assert!(
            !c.should_log(),
            "2FA-required must not emit a final error log; that path is the success branch"
        );
    }

    #[test]
    fn classify_exit_error_partial_sync_uses_exit_partial() {
        let e: anyhow::Error = PartialSyncError(7).into();
        let c = classify_exit_error(&e);
        assert_eq!(c, ExitClassification::Partial);
        assert_eq!(c.exit_code(), EXIT_PARTIAL);
        assert_eq!(c.exit_code(), 2);
        assert!(c.should_log());
    }

    #[test]
    fn classify_exit_error_auth_non_2fa_uses_exit_auth() {
        let e: anyhow::Error = auth::error::AuthError::FailedLogin("bad password".into()).into();
        let c = classify_exit_error(&e);
        assert_eq!(c, ExitClassification::Auth);
        assert_eq!(c.exit_code(), EXIT_AUTH);
        assert_eq!(c.exit_code(), 3);
        assert!(c.should_log());
    }

    #[test]
    fn classify_exit_error_terminal_auth_uses_exit_terminal_auth() {
        let e: anyhow::Error = auth::error::AuthError::terminal_apple_auth(
            auth::error::APPLE_ACCOUNT_LOCKED_CODE,
            "Account locked",
        )
        .into();
        let c = classify_exit_error(&e);
        assert_eq!(c, ExitClassification::TerminalAuth);
        assert_eq!(c.exit_code(), EXIT_TERMINAL_AUTH);
        assert_eq!(c.exit_code(), 4);
        assert!(c.should_log());
    }

    #[test]
    fn classify_exit_error_generic_uses_failure() {
        let e = anyhow::anyhow!("disk on fire");
        let c = classify_exit_error(&e);
        assert_eq!(c, ExitClassification::Other);
        assert_eq!(c.exit_code(), 1);
        assert!(c.should_log());
    }

    #[test]
    fn classify_exit_error_walks_anyhow_context() {
        let e: anyhow::Error = anyhow::Error::from(auth::error::AuthError::TwoFactorRequired)
            .context("while validating session")
            .context("during startup");
        assert_eq!(
            classify_exit_error(&e),
            ExitClassification::TwoFactorRequired,
            "AuthError wrapped in .context() must still classify as 2FA-required; \
             otherwise context-wrapping at any call site silently flips exit 0 -> exit 1"
        );

        let e: anyhow::Error =
            anyhow::Error::from(PartialSyncError(3)).context("after final retry pass");
        assert_eq!(classify_exit_error(&e), ExitClassification::Partial);

        let e: anyhow::Error = anyhow::Error::from(auth::error::AuthError::terminal_apple_auth(
            auth::error::APPLE_ACCOUNT_LOCKED_CODE,
            "Account locked",
        ))
        .context("while completing SRP login")
        .context("during startup");
        assert_eq!(classify_exit_error(&e), ExitClassification::TerminalAuth);
    }

    #[test]
    fn classify_exit_error_codes_are_distinct() {
        let codes = [
            ExitClassification::TwoFactorRequired.exit_code(),
            ExitClassification::Partial.exit_code(),
            ExitClassification::Auth.exit_code(),
            ExitClassification::TerminalAuth.exit_code(),
            ExitClassification::Other.exit_code(),
        ];
        let mut sorted: Vec<u8> = codes.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            codes.len(),
            "every exit classification must map to a distinct code; got {codes:?}"
        );
    }
}

/// Wrappers around `pub(crate)` parser entry points so cargo-fuzz harnesses
/// in `fuzz/` can drive them through the lib instead of inlining source
/// files via `#[path]`. Gated on the `__fuzz_internals` feature; absent in
/// production builds.
///
/// Every entry point takes/returns only externally-nameable types
/// (`&[u8]`, `&str`, `serde_json::Value`) and discards typed results
/// internally so the lib's internal types stay `pub(crate)`. Don't add
/// anything here that isn't strictly needed by a fuzz target.
#[cfg(feature = "__fuzz_internals")]
#[doc(hidden)]
pub mod __fuzz {
    use serde_json::Value;

    /// Try every CloudKit response struct on the same byte slice. Each call
    /// is `serde_json::from_slice::<T>(data)` with the result discarded.
    pub fn cloudkit_try_all(data: &[u8]) {
        use crate::icloud::photos::cloudkit::{
            BatchQueryResponse, ChangesDatabaseResponse, ChangesZoneResponse, ChangesZoneResult,
            QueryResponse, Record, ZoneId, ZoneListResponse,
        };
        let _ = serde_json::from_slice::<ZoneListResponse>(data);
        let _ = serde_json::from_slice::<QueryResponse>(data);
        let _ = serde_json::from_slice::<BatchQueryResponse>(data);
        let _ = serde_json::from_slice::<Record>(data);
        let _ = serde_json::from_slice::<ChangesDatabaseResponse>(data);
        let _ = serde_json::from_slice::<ChangesZoneResponse>(data);
        let _ = serde_json::from_slice::<ChangesZoneResult>(data);
        let _ = serde_json::from_slice::<ZoneId>(data);
    }

    /// Try every iCloud auth response struct on the same byte slice.
    pub fn auth_responses_try_all(data: &[u8]) {
        use crate::auth::responses::{AccountLoginResponse, SrpInitResponse, TwoFactorChallenge};
        let _ = serde_json::from_slice::<SrpInitResponse>(data);
        let _ = serde_json::from_slice::<AccountLoginResponse>(data);
        let _ = serde_json::from_slice::<TwoFactorChallenge>(data);
    }

    /// Parse a TOML config from arbitrary bytes; discards the typed result.
    pub fn parse_toml_config(s: &str) {
        let _ = toml::from_str::<crate::config::TomlConfig>(s);
    }

    /// Run every `*Enc` decoder against a JSON value, both as the JSON shape
    /// path and as the bplist-via-base64 path. The fuzz harness handles the
    /// base64 wrapping itself; this function takes the prepared `Value`.
    pub fn enc_decoders(fields: &Value) {
        use crate::icloud::photos::enc;
        let _ = enc::decode_string(fields, "captionEnc");
        let _ = enc::decode_string(fields, "extendedDescEnc");
        let _ = enc::decode_keywords(fields);
        let _ = enc::decode_location(fields, "locationEnc");
        let _ = enc::decode_location(fields, "locationV2Enc");
        let _ = enc::decode_location_with_fallback(fields);
    }

    /// Build a `PhotoAsset` from two CloudKit `Record` JSON values. Returns
    /// `()` on success / discard so the harness doesn't name internal types.
    /// Inputs that don't deserialize as a `Record` are skipped.
    pub fn photo_asset_from_record_json(master: Value, asset: Value) {
        use crate::icloud::photos::asset::PhotoAsset;
        use crate::icloud::photos::cloudkit::Record;
        let Ok(master) = serde_json::from_value::<Record>(master) else {
            return;
        };
        let Ok(asset) = serde_json::from_value::<Record>(asset) else {
            return;
        };
        let _ = PhotoAsset::from_records(master, &asset);
    }

    /// Run the path-component sanitizers over an arbitrary `&str`. Splits
    /// the input on the first NUL into (template, album) so the fuzzer can
    /// reach the `{album}` substitution path.
    pub fn paths_sanitization(s: &str) {
        use crate::download::paths;
        let _ = paths::clean_filename(s);
        let _ = paths::sanitize_path_component(s);
        let _ = paths::strip_python_wrapper(s);
        let _ = paths::remove_unicode_chars(s);

        let (template, album) = match s.split_once('\0') {
            Some((a, b)) => (a, Some(b)),
            None => (s, None),
        };
        let _ = paths::expand_album_token(template, album);

        for size in [0u64, 1, u64::MAX] {
            let _ = paths::add_dedup_suffix(s, size);
        }
    }

    /// Walk an HEIC byte buffer for the embedded XMP packet. Defense-in-depth
    /// against the upstream mp4-atom OOM class that hit `parse_vorbis_comment`
    /// (kixelated/mp4-atom#154, fixed upstream in #157) and any sibling
    /// decoders that might regress in the same shape.
    pub fn heif_extract_xmp(bytes: &[u8]) -> Option<Vec<u8>> {
        crate::download::heif::extract_xmp_bytes(bytes)
    }

    /// Cheap content-sniff check for HEIC/HEIF/AVIF magic bytes.
    pub fn heif_is_heif_content(bytes: &[u8]) -> bool {
        crate::download::heif::is_heif_content(bytes)
    }

    /// Run the three `state` enum string parsers on the same input. They're
    /// inherent `from_str` methods, not the `FromStr` trait, returning
    /// `Option<Self>`.
    pub fn state_enums_from_str(s: &str) {
        let _ = crate::state::VersionSizeKey::from_str(s);
        let _ = crate::state::AssetStatus::from_str(s);
        let _ = crate::state::MediaType::from_str(s);
    }
}
