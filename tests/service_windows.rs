//! Windows-only integration tests for `kei install` / `kei uninstall` /
//! `kei service status`. The full SCM round-trip lands in PR 8's smoke
//! matrix (which runs on a real elevated Windows runner). What this file
//! covers is the surface the smoke matrix can't easily probe: dry-run
//! preview output, `--system` rejection, friendly error when running
//! non-elevated, and `kei uninstall` on a host with no kei service.
//!
//! Gated to Windows so the file is a no-op on linux/macOS hosts. The
//! cross-platform CLI parsing is covered separately in `service_cli.rs`.

#![cfg(target_os = "windows")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]

mod common;

use predicates::prelude::*;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(30);

fn cmd() -> assert_cmd::Command {
    let mut cmd = common::cmd();
    cmd.timeout(TIMEOUT);
    cmd
}

#[test]
fn install_system_is_rejected_with_pointer_to_user_install() {
    cmd()
        .args(["install", "--system"])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("not supported on Windows")
                .and(predicate::str::contains("per-user")),
        );
}

#[test]
fn dry_run_install_emits_full_preview_without_touching_scm() {
    // `--dry-run` must not require elevation: it does not call SCM, so
    // it must succeed inside this test process even though the test
    // runner may not be elevated. The preview must list every field
    // SCM would have configured so an operator can eyeball the install.
    let assert = cmd().args(["install", "--user", "--dry-run"]).assert();
    let output = assert.get_output().clone();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "expected --dry-run install to succeed without elevation; \
         stdout=\n{stdout}\nstderr=\n{stderr}"
    );
    for needle in [
        "Service name        : com.rhoopr.kei",
        "Display name        : kei Media Sync Engine",
        "Account             : .\\",
        "Service type        : OWN_PROCESS",
        "Start type          : AUTO_START",
        "Failure actions     : restart x3",
    ] {
        assert!(
            stdout.contains(needle),
            "expected preview to contain {needle:?}; got:\n{stdout}",
        );
    }
}

#[test]
fn status_on_host_without_kei_service_reports_not_installed_or_scm_unavailable() {
    // CI windows-latest runs as Administrator so SCM is reachable; the
    // status output for a host with no kei registered is "not
    // installed". Local non-elevated runs see "SCM unavailable" -- both
    // are valid for this test, since the assertion is "kei service
    // status returns 0 and emits a Service: line", not which verdict.
    cmd()
        .args(["service", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Service:"));
}

#[test]
fn uninstall_on_host_without_kei_service_is_a_clean_no_op() {
    // Mirrors the linux/macOS contract: `kei uninstall` on a host with
    // no kei registered must succeed (with a "nothing to remove" log
    // line) rather than error out. Without --purge the call cannot
    // touch the user's state directory.
    cmd().args(["uninstall"]).assert().success();
}
