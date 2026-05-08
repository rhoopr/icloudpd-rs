//! Cross-platform CLI parsing tests for `kei install`, `kei uninstall`,
//! and `kei service {run,status}`.
//!
//! Tests in this file must hold on every target. Platform-specific
//! behavior — actually writing a unit file or invoking systemctl — lives
//! in `tests/service_linux.rs` (and analogous suites for macOS / Windows
//! once PRs 4-5 land). The "not yet implemented" assertions for macOS /
//! Windows are gated to those targets so PR 3's Linux backend doesn't
//! make them lie.

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
fn uninstall_accepts_purge_flag_via_clap() {
    // Cross-platform check: clap parses `--purge` as a known flag (no
    // exit-2 parse error). The actual uninstall behavior is asserted
    // per-platform; here we just confirm the surface accepts the flag
    // without flagging it as unknown.
    cmd()
        .args(["uninstall", "--purge", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--purge"));
}

#[test]
fn install_accepts_dry_run_flag_via_clap() {
    // Same shape as --purge: assert clap recognizes `--dry-run` rather
    // than executing the install. The Linux backend exercises the
    // dry-run end-to-end in tests/service_linux.rs.
    cmd()
        .args(["install", "--dry-run", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--dry-run"));
}

// ── Stub error contract (non-Linux only) ────────────────────────────────
//
// PR 3 lands the Linux backend, so on Linux these no longer return a
// "not yet implemented" error. PRs 4 (macOS) and 5 (Windows) replace
// the stubs on those targets too — the cfg gate auto-deactivates each
// test as its platform's backend lands.

#[cfg(not(target_os = "linux"))]
#[test]
fn install_returns_clean_not_implemented_error() {
    cmd()
        .arg("install")
        .assert()
        .failure()
        .stderr(predicate::str::contains("not yet implemented"));
}

#[cfg(not(target_os = "linux"))]
#[test]
fn uninstall_returns_clean_not_implemented_error() {
    cmd()
        .arg("uninstall")
        .assert()
        .failure()
        .stderr(predicate::str::contains("not yet implemented"));
}

#[cfg(not(target_os = "linux"))]
#[test]
fn service_status_returns_clean_not_implemented_error() {
    cmd()
        .args(["service", "status"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not yet implemented"));
}
