//! Personality layer: friendly UX wrapping for CLI output.
//!
//! Two modes: `Friendly` adds verb-cycling spinners, ETA wording, summary
//! cards, and curated phase narration. `Off` keeps v0.13 behaviour byte-for-byte
//! for journals, pipes, JSON consumers, and explicit `--log-level` users.
//!
//! The gate is a single function (`resolve_mode`) so every consumer sees the
//! same answer for a given environment. New surfaces should call
//! `resolve_mode` once at startup and pass the resulting `Mode` down rather
//! than re-detecting at each call site.

pub mod active_bar;
pub mod bar_render;
pub mod pace;
pub mod sparkline;
pub mod theme;
pub mod tracing;
// `verbs` lands here so delight-B can wire its pools into the existing bar
// surface without a second round-trip through review. The bar's verb-cycling
// hook is a `set_message` driver that belongs alongside greeting / phase
// narration in the lifecycle PR.
#[allow(dead_code, reason = "consumed by delight-B lifecycle work")]
pub mod verbs;

use std::env;
use std::io::IsTerminal;

/// Friendly UX mode resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Verb cycling, summary card, sign-off, curated phase lines.
    Friendly,
    /// v0.13 behaviour: structured tracing with target+timestamp, plain bars.
    #[default]
    Off,
}

impl Mode {
    pub fn is_friendly(self) -> bool {
        matches!(self, Mode::Friendly)
    }
}

/// Inputs that can force friendly mode off regardless of user preference.
///
/// Detection happens once at startup. Honouring CLI flags, env vars, and
/// the running container/service context in one place keeps the rules
/// auditable and lets us unit-test every disabling input.
#[derive(Debug, Clone)]
pub struct Context {
    /// `stderr` is a terminal (where tracing writes).
    pub stderr_is_tty: bool,
    /// `stdout` is a terminal (where personality lines and the bar print).
    pub stdout_is_tty: bool,
    /// `TERM=dumb` (forces off; dumb terminals can't render any of this).
    pub term_dumb: bool,
    /// User passed `--no-progress-bar`.
    pub no_progress_bar: bool,
    /// User passed `--only-print-filenames`.
    pub only_print_filenames: bool,
    /// User passed `--report-json` or another machine-output flag.
    pub report_json: bool,
    /// User explicitly set `--log-level` (not the default).
    pub log_level_explicit: bool,
    /// `RUST_LOG` env var is set (user wants verbose tracing).
    pub rust_log_set: bool,
    /// Running under `kei service run` (journal-bound output).
    pub service_run: bool,
    /// Running inside a container (`/.dockerenv` exists or cgroup hint).
    pub in_container: bool,
    /// Running under systemd (`INVOCATION_ID` env var is set).
    pub under_systemd: bool,
}

impl Context {
    /// Detect context from the current process. Should be called once at
    /// startup, before any tool messes with stdio.
    #[must_use]
    pub fn detect(
        no_progress_bar: bool,
        only_print_filenames: bool,
        report_json: bool,
        log_level_explicit: bool,
        service_run: bool,
    ) -> Self {
        Self {
            stderr_is_tty: std::io::stderr().is_terminal(),
            stdout_is_tty: std::io::stdout().is_terminal(),
            term_dumb: env::var_os("TERM").is_some_and(|t| t == "dumb"),
            no_progress_bar,
            only_print_filenames,
            report_json,
            log_level_explicit,
            rust_log_set: env::var_os("RUST_LOG").is_some(),
            service_run,
            in_container: std::path::Path::new("/.dockerenv").exists(),
            under_systemd: env::var_os("INVOCATION_ID").is_some(),
        }
    }

    /// Test helper: build a context with all gates open (friendly will resolve on).
    #[cfg(test)]
    fn permissive() -> Self {
        Self {
            stderr_is_tty: true,
            stdout_is_tty: true,
            term_dumb: false,
            no_progress_bar: false,
            only_print_filenames: false,
            report_json: false,
            log_level_explicit: false,
            rust_log_set: false,
            service_run: false,
            in_container: false,
            under_systemd: false,
        }
    }
}

/// Resolve friendly mode given the user's preference and environmental gates.
///
/// `want_friendly` is the user-facing toggle (`--friendly` / TOML / future
/// default). Even when true, environmental gates can force off.
#[must_use]
pub fn resolve_mode(want_friendly: bool, ctx: &Context) -> Mode {
    if !want_friendly {
        return Mode::Off;
    }
    // Hard-off contexts: no override possible.
    if ctx.service_run || ctx.in_container || ctx.under_systemd {
        return Mode::Off;
    }
    if ctx.term_dumb {
        return Mode::Off;
    }
    if !ctx.stderr_is_tty || !ctx.stdout_is_tty {
        return Mode::Off;
    }
    if ctx.no_progress_bar || ctx.only_print_filenames {
        return Mode::Off;
    }
    if ctx.report_json {
        return Mode::Off;
    }
    if ctx.log_level_explicit || ctx.rust_log_set {
        return Mode::Off;
    }
    Mode::Friendly
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_when_user_disables() {
        let ctx = Context::permissive();
        assert_eq!(resolve_mode(false, &ctx), Mode::Off);
    }

    #[test]
    fn on_when_permissive() {
        let ctx = Context::permissive();
        assert_eq!(resolve_mode(true, &ctx), Mode::Friendly);
    }

    #[test]
    fn off_in_service_run() {
        let mut ctx = Context::permissive();
        ctx.service_run = true;
        assert_eq!(resolve_mode(true, &ctx), Mode::Off);
    }

    #[test]
    fn off_in_container() {
        let mut ctx = Context::permissive();
        ctx.in_container = true;
        assert_eq!(resolve_mode(true, &ctx), Mode::Off);
    }

    #[test]
    fn off_under_systemd() {
        let mut ctx = Context::permissive();
        ctx.under_systemd = true;
        assert_eq!(resolve_mode(true, &ctx), Mode::Off);
    }

    #[test]
    fn off_when_term_dumb() {
        let mut ctx = Context::permissive();
        ctx.term_dumb = true;
        assert_eq!(resolve_mode(true, &ctx), Mode::Off);
    }

    #[test]
    fn off_when_stderr_not_tty() {
        let mut ctx = Context::permissive();
        ctx.stderr_is_tty = false;
        assert_eq!(resolve_mode(true, &ctx), Mode::Off);
    }

    #[test]
    fn off_when_stdout_not_tty() {
        let mut ctx = Context::permissive();
        ctx.stdout_is_tty = false;
        assert_eq!(resolve_mode(true, &ctx), Mode::Off);
    }

    #[test]
    fn off_with_no_progress_bar() {
        let mut ctx = Context::permissive();
        ctx.no_progress_bar = true;
        assert_eq!(resolve_mode(true, &ctx), Mode::Off);
    }

    #[test]
    fn off_with_only_print_filenames() {
        let mut ctx = Context::permissive();
        ctx.only_print_filenames = true;
        assert_eq!(resolve_mode(true, &ctx), Mode::Off);
    }

    #[test]
    fn off_with_report_json() {
        let mut ctx = Context::permissive();
        ctx.report_json = true;
        assert_eq!(resolve_mode(true, &ctx), Mode::Off);
    }

    #[test]
    fn off_with_explicit_log_level() {
        let mut ctx = Context::permissive();
        ctx.log_level_explicit = true;
        assert_eq!(resolve_mode(true, &ctx), Mode::Off);
    }

    #[test]
    fn off_with_rust_log_env() {
        let mut ctx = Context::permissive();
        ctx.rust_log_set = true;
        assert_eq!(resolve_mode(true, &ctx), Mode::Off);
    }

    #[test]
    fn mode_is_friendly_helper() {
        assert!(Mode::Friendly.is_friendly());
        assert!(!Mode::Off.is_friendly());
    }
}
