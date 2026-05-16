//! Linux-specific integration tests for `kei install` / `kei uninstall`.
//!
//! Exercises the unit-file rendering pipeline end-to-end via `--dry-run`,
//! which writes the systemd unit file but skips the `systemctl` /
//! `loginctl` side effects. Faithful coverage of the daemon-reload /
//! enable / linger steps requires an active user session, so those land
//! in PR 8's per-platform smoke matrix instead of here.

#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]

mod common;

use predicates::prelude::*;
use std::time::Duration;
use tempfile::TempDir;

const TIMEOUT: Duration = Duration::from_secs(20);

fn cmd_with_home(home: &TempDir) -> assert_cmd::Command {
    let mut cmd = common::cmd();
    cmd.timeout(TIMEOUT);
    // Scrub inherited environment so the test can't reach the developer's
    // real systemd user session: XDG_RUNTIME_DIR + DBUS_SESSION_BUS_ADDRESS
    // are how `systemctl --user` finds the live user-bus, so removing them
    // makes the (best-effort) disable / daemon-reload calls fail fast in
    // the tempdir instead of touching the host's running services.
    cmd.env_remove("XDG_CONFIG_HOME");
    cmd.env_remove("XDG_RUNTIME_DIR");
    cmd.env_remove("DBUS_SESSION_BUS_ADDRESS");
    cmd.env_remove("SUDO_USER");
    cmd.env("HOME", home.path());
    cmd.env("XDG_CONFIG_HOME", home.path().join(".config"));
    cmd
}

fn user_unit_path(home: &TempDir) -> std::path::PathBuf {
    home.path().join(".config/systemd/user/kei.service")
}

#[test]
fn dry_run_install_user_writes_unit_file_with_expected_keys() {
    let home = TempDir::new().unwrap();
    cmd_with_home(&home)
        .args(["install", "--user", "--dry-run"])
        .assert()
        .success();

    let unit = user_unit_path(&home);
    assert!(unit.exists(), "expected unit at {}", unit.display());
    let body = std::fs::read_to_string(&unit).unwrap();

    // Spot-check the load-bearing keys; the renderer's full shape is
    // covered by unit tests in src/service/linux.rs.
    assert!(body.contains("[Unit]"), "missing [Unit]:\n{body}");
    assert!(body.contains("[Service]"), "missing [Service]:\n{body}");
    assert!(body.contains("[Install]"), "missing [Install]:\n{body}");
    assert!(body.contains("Type=notify"), "missing Type=notify:\n{body}");
    assert!(
        body.contains("Description=kei Media Sync Engine"),
        "missing Description:\n{body}"
    );
    assert!(
        body.contains("WantedBy=default.target"),
        "missing WantedBy:\n{body}"
    );

    // ExecStart must point at an absolute path (the actual kei binary
    // canonicalized by current_executable). Verifying the prefix is
    // enough — the path itself is environment-dependent.
    assert!(
        body.contains("ExecStart=/") && body.contains(" service run --config "),
        "ExecStart not absolute or missing flags:\n{body}"
    );
}

#[test]
fn dry_run_install_user_is_idempotent() {
    let home = TempDir::new().unwrap();
    for _ in 0..2 {
        cmd_with_home(&home)
            .args(["install", "--user", "--dry-run"])
            .assert()
            .success();
    }
    assert!(user_unit_path(&home).exists());
}

#[test]
fn dry_run_install_user_writes_to_xdg_config_home_override() {
    // Ensures we honor XDG_CONFIG_HOME rather than always landing under
    // $HOME/.config — the systemd convention is XDG-first, and a user
    // who relocates their config would expect kei to follow.
    let home = TempDir::new().unwrap();
    let xdg = TempDir::new().unwrap();
    let mut cmd = common::cmd();
    cmd.timeout(TIMEOUT);
    cmd.env_remove("XDG_RUNTIME_DIR");
    cmd.env_remove("DBUS_SESSION_BUS_ADDRESS");
    cmd.env_remove("SUDO_USER");
    cmd.env("HOME", home.path());
    cmd.env("XDG_CONFIG_HOME", xdg.path());

    cmd.args(["install", "--user", "--dry-run"])
        .assert()
        .success();

    let xdg_unit = xdg.path().join("systemd/user/kei.service");
    let home_unit = home.path().join(".config/systemd/user/kei.service");
    assert!(
        xdg_unit.exists(),
        "expected unit under XDG_CONFIG_HOME at {}",
        xdg_unit.display()
    );
    assert!(
        !home_unit.exists(),
        "must not fall through to $HOME/.config when XDG_CONFIG_HOME is set"
    );
}

#[test]
fn install_system_without_root_fails_clearly() {
    // CI runs as a non-root user, which is exactly the condition this
    // test wants to exercise. Skipping when EUID==0 (rare local-dev
    // case) keeps the assertion meaningful without false negatives.
    // SAFETY: stateless POSIX FFI call, no memory-safety preconditions.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipping non-root assertion: running as root");
        return;
    }

    let home = TempDir::new().unwrap();
    cmd_with_home(&home)
        .args(["install", "--system", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("must be run as root"));
}

#[test]
fn uninstall_removes_unit_file_after_dry_run_install() {
    let home = TempDir::new().unwrap();
    cmd_with_home(&home)
        .args(["install", "--user", "--dry-run"])
        .assert()
        .success();
    let unit = user_unit_path(&home);
    assert!(unit.exists(), "precondition: unit file written");

    // Bare `uninstall` (no --purge) should remove the unit file. It
    // also tries `systemctl --user disable --now`, which is a no-op /
    // failure in a tempdir-only environment — the function swallows
    // that and proceeds to delete the file.
    cmd_with_home(&home).arg("uninstall").assert().success();
    assert!(!unit.exists(), "uninstall should remove {}", unit.display());
}

#[test]
fn uninstall_with_no_unit_files_is_a_clean_no_op() {
    let home = TempDir::new().unwrap();
    cmd_with_home(&home).arg("uninstall").assert().success();
    assert!(!user_unit_path(&home).exists());
}

#[test]
fn uninstall_purge_removes_kei_state_dir_when_present() {
    let home = TempDir::new().unwrap();
    let kei_dir = home.path().join(".config/kei");
    std::fs::create_dir_all(&kei_dir).unwrap();
    std::fs::write(kei_dir.join("config.toml"), "[auth]\n").unwrap();
    std::fs::write(kei_dir.join("state.db"), b"\x00").unwrap();

    // Install a unit so uninstall has something to do too — but the
    // assertion is on the purge path.
    cmd_with_home(&home)
        .args(["install", "--user", "--dry-run"])
        .assert()
        .success();

    cmd_with_home(&home)
        .args(["uninstall", "--purge"])
        .assert()
        .success();

    assert!(
        !kei_dir.exists(),
        "--purge should remove ~/.config/kei (was at {})",
        kei_dir.display()
    );
}

#[test]
fn uninstall_purge_clears_encrypted_credential_file() {
    // --purge must wipe stored credentials, not just the on-disk state.
    // The encrypted-file backend is the only one we can exercise
    // hermetically: the keyring backend would otherwise dispatch through
    // libsecret to the dev's real OS keyring. cmd_with_home scrubs
    // DBUS_SESSION_BUS_ADDRESS specifically so that backend can't reach
    // anything live, leaving CredentialStore::delete to land its
    // file-side cleanup inside the tempdir.
    let home = TempDir::new().unwrap();
    let kei_dir = home.path().join(".config/kei");
    std::fs::create_dir_all(&kei_dir).unwrap();
    std::fs::write(
        kei_dir.join("config.toml"),
        "[auth]\nusername = \"kei-purge-test@example.invalid\"\n",
    )
    .unwrap();
    let cred_file = kei_dir.join("credentials.enc");
    std::fs::write(&cred_file, b"opaque-ciphertext-bytes").unwrap();

    cmd_with_home(&home)
        .args(["uninstall", "--purge"])
        .assert()
        .success();

    assert!(
        !cred_file.exists(),
        "--purge should remove the encrypted credential file"
    );
    assert!(
        !kei_dir.exists(),
        "--purge should also remove the kei state directory"
    );
}

#[test]
fn service_status_with_no_unit_reports_not_installed() {
    let home = TempDir::new().unwrap();
    cmd_with_home(&home)
        .args(["service", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Service: not installed"));
}
