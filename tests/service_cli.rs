//! CLI parsing + container-guard tests for `kei install`, `kei uninstall`,
//! and `kei service {run,status}`.
//!
//! The per-platform install backends (launchd, systemd, Windows SCM) land
//! in PRs 3-5; until then `install` / `uninstall` / `service status`
//! return a clean "not yet implemented" error and the tests below assert
//! the contract: subcommand parsing, `--help` rendering, mutually
//! exclusive flags, and a friendly stub error rather than a panic.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]

mod common;

use predicates::prelude::*;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(10);

fn cmd() -> assert_cmd::Command {
    let mut cmd = common::cmd();
    cmd.timeout(TIMEOUT);
    cmd
}

// ── Help output ─────────────────────────────────────────────────────────

#[test]
fn install_help_lists_user_and_system_flags() {
    cmd()
        .args(["install", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--user").and(predicate::str::contains("--system")));
}

#[test]
fn uninstall_help_lists_purge_flag() {
    cmd()
        .args(["uninstall", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--purge"));
}

#[test]
fn service_help_lists_run_and_status() {
    cmd()
        .args(["service", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("run").and(predicate::str::contains("status")));
}

#[test]
fn service_run_help_inherits_sync_flags() {
    // `kei service run` shares SyncArgs, so its help must surface the
    // same flag vocabulary -- proves the delegation wiring is intact.
    cmd()
        .args(["service", "run", "--help"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("--watch-with-interval")
                .and(predicate::str::contains("--download-dir"))
                .and(predicate::str::contains("--threads")),
        );
}

#[test]
fn service_status_help_renders_without_panic() {
    // `Status` is a unit variant with no flags of its own. The assertion
    // is just "clap renders help and exits 0" -- defends against an
    // accidental enum-shape change that would break help generation.
    cmd()
        .args(["service", "status", "--help"])
        .assert()
        .success();
}

#[test]
fn top_level_help_lists_install_uninstall_service() {
    cmd().arg("--help").assert().success().stdout(
        predicate::str::contains("install")
            .and(predicate::str::contains("uninstall"))
            .and(predicate::str::contains("service")),
    );
}

// ── Argument parsing ────────────────────────────────────────────────────

#[test]
fn install_user_and_system_are_mutually_exclusive() {
    cmd()
        .args(["install", "--user", "--system"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be used with"));
}

#[test]
fn uninstall_accepts_purge_flag() {
    // Stub returns NotImplemented (exit 1); the relevant assertion is
    // that --purge parses cleanly, which the failure mode at exit 1
    // (vs. a clap parse error at exit 2) confirms.
    cmd()
        .args(["uninstall", "--purge"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not yet implemented"));
}

// ── Stub error contract ─────────────────────────────────────────────────

#[test]
fn install_returns_clean_not_implemented_error() {
    cmd()
        .arg("install")
        .assert()
        .failure()
        .stderr(predicate::str::contains("not yet implemented"));
}

#[test]
fn uninstall_returns_clean_not_implemented_error() {
    cmd()
        .arg("uninstall")
        .assert()
        .failure()
        .stderr(predicate::str::contains("not yet implemented"));
}

#[test]
fn service_status_returns_clean_not_implemented_error() {
    cmd()
        .args(["service", "status"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not yet implemented"));
}
