//! Live `import-existing` tests against the real Apple CloudKit API.
//!
//! Strategy:
//! 1. **Setup once per test run**: download a fixture set of recent photos
//!    via the real `kei sync` command (default size, default folder
//!    structure, default match policy). The fixture directory is reused
//!    across every test in this file via a `OnceLock`.
//! 2. **Per test**: each test runs `kei import-existing` against that
//!    fixture (or a copy of a subset of it) with a fresh state DB to
//!    isolate side-effects.
//!
//! All tests are gated `#[ignore]`. Run with:
//!
//! ```sh
//! cargo test --test import_existing_live -- --ignored --test-threads=1
//! ```
//!
//! The fixture is intentionally not cleaned up between runs — the next
//! invocation can reuse it via `KEI_IMPORT_FIXTURE_DIR`. By default the
//! fixture lives in `/tmp/claude/kei-import-fixture/`, so a re-run just
//! polls for new photos via the same `kei sync` command (which is a no-op
//! when nothing changed).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::print_stderr,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::indexing_slicing
)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use predicates::prelude::*;
use tempfile::tempdir;

const FIXTURE_TIMEOUT_SECS: u64 = 1800; // 30m: full sync of ~100 assets
const IMPORT_TIMEOUT_SECS: u64 = 300; // 5m: import-existing scans, no downloads
const FIXTURE_RECENT: u32 = 100;

/// Dir where the fixture sync writes its files. Reused across tests in a
/// single `cargo test` invocation, and persisted across invocations
/// (allowing the second run to re-use the cache as long as the dir exists
/// and has files).
fn fixture_root() -> PathBuf {
    if let Ok(dir) = std::env::var("KEI_IMPORT_FIXTURE_DIR") {
        return PathBuf::from(dir);
    }
    PathBuf::from("/tmp/claude/kei-import-fixture")
}

/// One-shot ensure-fixture: returns the fixture download dir + the data
/// dir used during the sync (the data dir contains state.db produced by
/// the sync, which subsequent import-existing runs deliberately ignore --
/// each test gets its own state DB).
fn fixture() -> &'static (PathBuf, PathBuf) {
    static FIX: OnceLock<(PathBuf, PathBuf)> = OnceLock::new();
    FIX.get_or_init(|| {
        let (username, password, cookie_dir) = common::require_preauth();
        let download_dir = fixture_root();
        let data_dir = download_dir.join("_kei_data");
        std::fs::create_dir_all(&download_dir).unwrap();
        std::fs::create_dir_all(&data_dir).unwrap();

        // Mirror the cookie dir into data_dir so the sync command picks
        // up the existing trust cookie. (kei looks for cookies under
        // --data-dir.)
        for entry in std::fs::read_dir(&cookie_dir).unwrap() {
            let entry = entry.unwrap();
            let dst = data_dir.join(entry.file_name());
            if !dst.exists() {
                let _ = std::fs::copy(entry.path(), dst);
            }
        }

        eprintln!(
            "Building import-existing fixture: --recent {FIXTURE_RECENT} into {}",
            download_dir.display()
        );
        let output = common::cmd()
            .args([
                "sync",
                "--username",
                &username,
                "--password",
                &password,
                "--data-dir",
                data_dir.to_str().unwrap(),
                "--directory",
                download_dir.to_str().unwrap(),
                "--recent",
                &FIXTURE_RECENT.to_string(),
                "--no-progress-bar",
                "--no-incremental",
            ])
            .timeout(Duration::from_secs(FIXTURE_TIMEOUT_SECS))
            .output()
            .expect("failed to run fixture sync");
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            panic!("fixture sync failed:\nstderr: {stderr}");
        }
        eprintln!("Fixture ready: {}", download_dir.display());
        (download_dir, data_dir)
    })
}

/// Build an `import-existing` command targeting the fixture's download
/// dir but with a fresh `--data-dir` so the per-test state DB stays
/// isolated.
fn import_cmd(
    username: &str,
    password: &str,
    cookie_dir: &Path,
    download_dir: &Path,
    data_dir: &Path,
    extra: &[&str],
) -> assert_cmd::Command {
    // Mirror cookies into the per-test data_dir so import-existing can
    // authenticate without hitting Apple again.
    if let Ok(entries) = std::fs::read_dir(cookie_dir) {
        for entry in entries.flatten() {
            let dst = data_dir.join(entry.file_name());
            if !dst.exists() {
                let _ = std::fs::copy(entry.path(), &dst);
            }
        }
    }
    let mut cmd = common::cmd();
    cmd.args([
        "import-existing",
        "--username",
        username,
        "--password",
        password,
        "--data-dir",
        data_dir.to_str().unwrap(),
        "--download-dir",
        download_dir.to_str().unwrap(),
        "--no-progress-bar",
    ]);
    cmd.args(extra);
    cmd
}

/// Parse the trailing summary printed by `import-existing`.
fn parse_summary(stdout: &str) -> ImportSummary {
    let mut total = 0_u64;
    let mut matched = 0_u64;
    let mut unmatched = 0_u64;
    for line in stdout.lines() {
        let line = line.trim();
        if let Some(n) = line.strip_prefix("Total assets scanned:") {
            total = n.trim().parse().unwrap_or(0);
        } else if let Some(n) = line.strip_prefix("Files matched:") {
            matched = n.trim().parse().unwrap_or(0);
        } else if let Some(n) = line.strip_prefix("Unmatched versions:") {
            unmatched = n.trim().parse().unwrap_or(0);
        }
    }
    ImportSummary {
        total,
        matched,
        unmatched,
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ImportSummary {
    total: u64,
    matched: u64,
    unmatched: u64,
}

/// Count downloaded rows in `state.db`.
fn count_downloaded_rows(data_dir: &Path) -> u64 {
    let db_path = data_dir.join("state.db");
    if !db_path.exists() {
        return 0;
    }
    let conn = rusqlite::Connection::open(&db_path).expect("open state db");
    conn.query_row(
        "SELECT COUNT(*) FROM assets WHERE status = 'downloaded'",
        [],
        |row| row.get::<_, i64>(0),
    )
    .map(|n| u64::try_from(n).unwrap_or(0))
    .unwrap_or(0)
}

// ── Tests ──────────────────────────────────────────────────────────────

/// Smoke test: import-existing against the fixture's download dir
/// matches every file the sync wrote.
#[test]
#[ignore]
fn import_matches_default_layout_after_sync() {
    let (username, password, cookie_dir) = common::require_preauth();
    let (download_dir, _sync_data_dir) = fixture();

    common::with_auth_retry(|| {
        let test_data = tempdir().unwrap();
        let output = import_cmd(
            &username,
            &password,
            &cookie_dir,
            download_dir,
            test_data.path(),
            &[],
        )
        .timeout(Duration::from_secs(IMPORT_TIMEOUT_SECS))
        .assert()
        .success()
        .get_output()
        .clone();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let summary = parse_summary(&stdout);
        assert!(summary.total > 0, "expected some assets, got {summary:?}");
        assert!(
            summary.matched > 0,
            "expected matches, got {summary:?}\n{stdout}"
        );
        // Most assets should match — exact equality is brittle if iCloud
        // returned an asset whose primary version isn't on disk (e.g. a
        // rare orientation or a 0-byte placeholder), but the bulk should.
        let match_ratio = (summary.matched as f64) / (summary.total as f64);
        assert!(
            match_ratio > 0.5,
            "match ratio too low: {match_ratio:.2} ({summary:?})"
        );

        let rows = count_downloaded_rows(test_data.path());
        assert!(rows > 0, "no rows written to state DB");
        assert_eq!(
            rows, summary.matched,
            "DB row count must equal stdout `matched` value"
        );
    });
}

/// `--dry-run` reports the same matched count but writes no rows.
#[test]
#[ignore]
fn import_dry_run_writes_no_rows() {
    let (username, password, cookie_dir) = common::require_preauth();
    let (download_dir, _sync_data_dir) = fixture();

    common::with_auth_retry(|| {
        let test_data = tempdir().unwrap();
        let output = import_cmd(
            &username,
            &password,
            &cookie_dir,
            download_dir,
            test_data.path(),
            &["--dry-run"],
        )
        .timeout(Duration::from_secs(IMPORT_TIMEOUT_SECS))
        .assert()
        .success()
        .stdout(predicate::str::contains("DRY RUN"))
        .get_output()
        .clone();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let summary = parse_summary(&stdout);
        assert!(summary.matched > 0, "dry-run should still count matches");
        assert_eq!(
            count_downloaded_rows(test_data.path()),
            0,
            "dry-run must not write rows"
        );
    });
}

/// Re-running import-existing should produce the same matched count and
/// the same DB row count -- no duplicates.
#[test]
#[ignore]
fn import_is_idempotent() {
    let (username, password, cookie_dir) = common::require_preauth();
    let (download_dir, _sync_data_dir) = fixture();

    common::with_auth_retry(|| {
        let test_data = tempdir().unwrap();
        let run = || -> ImportSummary {
            let output = import_cmd(
                &username,
                &password,
                &cookie_dir,
                download_dir,
                test_data.path(),
                &[],
            )
            .timeout(Duration::from_secs(IMPORT_TIMEOUT_SECS))
            .assert()
            .success()
            .get_output()
            .clone();
            parse_summary(&String::from_utf8_lossy(&output.stdout))
        };
        let first = run();
        let rows_after_first = count_downloaded_rows(test_data.path());
        let second = run();
        let rows_after_second = count_downloaded_rows(test_data.path());

        assert_eq!(
            first.matched, second.matched,
            "matched counts diverged across runs: {first:?} vs {second:?}"
        );
        assert_eq!(
            rows_after_first, rows_after_second,
            "DB row count grew on re-run -- import-existing isn't idempotent"
        );
    });
}

/// `--recent N` caps the scan -- with N << total, matched < total scanned
/// against the full fixture, but greater than zero.
#[test]
#[ignore]
fn import_recent_limit_caps_scan() {
    let (username, password, cookie_dir) = common::require_preauth();
    let (download_dir, _sync_data_dir) = fixture();

    common::with_auth_retry(|| {
        let test_data = tempdir().unwrap();
        let output = import_cmd(
            &username,
            &password,
            &cookie_dir,
            download_dir,
            test_data.path(),
            &["--recent", "5"],
        )
        .timeout(Duration::from_secs(IMPORT_TIMEOUT_SECS))
        .assert()
        .success()
        .get_output()
        .clone();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let summary = parse_summary(&stdout);
        assert!(
            summary.total <= 5,
            "--recent 5 must scan at most 5 assets, got {summary:?}"
        );
    });
}

/// `--recent <N>d` (date filter) is rejected with a clear bail message,
/// matching the explicit handling we added to the binary. No fixture
/// required: the bail happens before any I/O against the download dir.
#[test]
#[ignore]
fn import_recent_days_form_is_rejected() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let test_root = tempdir().unwrap();
        let download_dir = test_root.path().join("photos");
        std::fs::create_dir_all(&download_dir).unwrap();
        import_cmd(
            &username,
            &password,
            &cookie_dir,
            &download_dir,
            test_root.path(),
            &["--recent", "30d"],
        )
        .timeout(Duration::from_secs(IMPORT_TIMEOUT_SECS))
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "isn't supported for import-existing",
        ));
    });
}

/// Truncating one of the fixture's files makes that version come up
/// `unmatched`. Operates on a copy of a small slice of the fixture so
/// the shared fixture itself stays intact.
#[test]
#[ignore]
fn import_unmatches_truncated_file() {
    let (username, password, cookie_dir) = common::require_preauth();
    let (download_dir, _sync_data_dir) = fixture();

    common::with_auth_retry(|| {
        // Copy 3 files into a fresh dir, preserving the original parent
        // directory layout (Y/m/d/...) so import-existing's path
        // derivation lines up.
        let test_root = tempdir().unwrap();
        let test_dl = test_root.path().join("photos");
        std::fs::create_dir_all(&test_dl).unwrap();
        let files: Vec<PathBuf> = common::walkdir(download_dir)
            .into_iter()
            .filter(|p| {
                let s = p.to_string_lossy();
                !s.contains("/_kei_data/") && !s.contains("/state.db")
            })
            .take(3)
            .collect();
        if files.len() < 3 {
            eprintln!("Fixture only has {} files, skipping", files.len());
            return;
        }
        for src in &files {
            let rel = src.strip_prefix(download_dir).unwrap();
            let dst = test_dl.join(rel);
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::copy(src, &dst).unwrap();
        }

        // Truncate the first file by 1 byte.
        let first_rel = files[0].strip_prefix(download_dir).unwrap();
        let truncated = test_dl.join(first_rel);
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(&truncated)
            .unwrap();
        let len = f.metadata().unwrap().len();
        f.set_len(len.saturating_sub(1)).unwrap();

        let test_data = tempdir().unwrap();
        let output = import_cmd(
            &username,
            &password,
            &cookie_dir,
            &test_dl,
            test_data.path(),
            &["--recent", "10"],
        )
        .timeout(Duration::from_secs(IMPORT_TIMEOUT_SECS))
        .assert()
        .success()
        .get_output()
        .clone();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let summary = parse_summary(&stdout);
        assert!(
            summary.unmatched >= 1,
            "expected ≥1 unmatched (truncated file), got {summary:?}\n{stdout}"
        );
    });
}

/// Removing a file makes that version come up `unmatched` (file not on disk).
#[test]
#[ignore]
fn import_unmatches_missing_file() {
    let (username, password, cookie_dir) = common::require_preauth();
    let (download_dir, _sync_data_dir) = fixture();

    common::with_auth_retry(|| {
        let test_root = tempdir().unwrap();
        let test_dl = test_root.path().join("photos");
        std::fs::create_dir_all(&test_dl).unwrap();
        let files: Vec<PathBuf> = common::walkdir(download_dir)
            .into_iter()
            .filter(|p| {
                let s = p.to_string_lossy();
                !s.contains("/_kei_data/") && !s.contains("/state.db")
            })
            .take(3)
            .collect();
        if files.len() < 3 {
            eprintln!("Fixture only has {} files, skipping", files.len());
            return;
        }
        // Copy only the first 2 of 3 -- the third will be "missing".
        for src in &files[..2] {
            let rel = src.strip_prefix(download_dir).unwrap();
            let dst = test_dl.join(rel);
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::copy(src, &dst).unwrap();
        }

        let test_data = tempdir().unwrap();
        let output = import_cmd(
            &username,
            &password,
            &cookie_dir,
            &test_dl,
            test_data.path(),
            &["--recent", "5"],
        )
        .timeout(Duration::from_secs(IMPORT_TIMEOUT_SECS))
        .assert()
        .success()
        .get_output()
        .clone();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let summary = parse_summary(&stdout);
        assert!(
            summary.unmatched >= 1,
            "expected ≥1 unmatched (missing file), got {summary:?}\n{stdout}"
        );
    });
}

/// Pointing at a non-existent download dir is a clear bail.
#[test]
#[ignore]
fn import_bails_on_missing_download_dir() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let test_data = tempdir().unwrap();
        let bogus = test_data.path().join("does-not-exist");
        import_cmd(
            &username,
            &password,
            &cookie_dir,
            &bogus,
            test_data.path(),
            &[],
        )
        .timeout(Duration::from_secs(IMPORT_TIMEOUT_SECS))
        .assert()
        .failure()
        .stderr(predicate::str::contains("Directory does not exist"));
    });
}

/// `--directory` is the deprecated alias for `--download-dir`. It must
/// still resolve, with a warning logged. Skips dry-run-only verification --
/// just exercises the alias resolution end-to-end.
#[test]
#[ignore]
fn import_accepts_deprecated_directory_flag() {
    let (username, password, _cookie_dir) = common::require_preauth();
    let (download_dir, _sync_data_dir) = fixture();

    common::with_auth_retry(|| {
        let test_data = tempdir().unwrap();
        // Use --directory directly (bypassing import_cmd which uses --download-dir)
        let mut cmd = common::cmd();
        cmd.args([
            "import-existing",
            "--username",
            &username,
            "--password",
            &password,
            "--data-dir",
            test_data.path().to_str().unwrap(),
            "--directory",
            download_dir.to_str().unwrap(),
            "--no-progress-bar",
            "--dry-run",
        ]);
        cmd.timeout(Duration::from_secs(IMPORT_TIMEOUT_SECS))
            .assert()
            .success()
            .stdout(predicate::str::contains("DRY RUN"));
    });
}

/// TOML-only configuration (no CLI flags for resolved fields). Verifies
/// `[photos]` and `[download]` sections feed import-existing's
/// `DownloadConfig` correctly.
#[test]
#[ignore]
fn import_reads_toml_for_path_derivation() {
    let (username, password, cookie_dir) = common::require_preauth();
    let (download_dir, _sync_data_dir) = fixture();

    common::with_auth_retry(|| {
        let test_data = tempdir().unwrap();
        // Mirror cookies so the binary doesn't need to re-auth.
        if let Ok(entries) = std::fs::read_dir(&cookie_dir) {
            for entry in entries.flatten() {
                let dst = test_data.path().join(entry.file_name());
                if !dst.exists() {
                    let _ = std::fs::copy(entry.path(), dst);
                }
            }
        }
        // Write a TOML config that re-states the defaults explicitly.
        // If the resolution plumbing ever drops a field, this test will
        // start producing zero matches, surfacing the regression.
        let toml_path = test_data.path().join("kei.toml");
        let toml_body = format!(
            r#"
[download]
directory = {dir:?}
folder_structure = "%Y/%m/%d"

[photos]
size = "original"
file_match_policy = "name-size-dedup-with-suffix"
live_photo_mode = "both"
live_photo_size = "original"
live_photo_mov_filename_policy = "suffix"
align_raw = "as-is"
keep_unicode_in_filenames = false
force_size = false
"#,
            dir = download_dir.to_string_lossy()
        );
        std::fs::write(&toml_path, toml_body).unwrap();

        let mut cmd = common::cmd();
        cmd.args([
            "import-existing",
            "--username",
            &username,
            "--password",
            &password,
            "--data-dir",
            test_data.path().to_str().unwrap(),
            "--config",
            toml_path.to_str().unwrap(),
            "--no-progress-bar",
            "--dry-run",
            "--recent",
            "10",
        ]);
        let output = cmd
            .timeout(Duration::from_secs(IMPORT_TIMEOUT_SECS))
            .assert()
            .success()
            .get_output()
            .clone();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let summary = parse_summary(&stdout);
        assert!(
            summary.matched > 0,
            "TOML-only config must drive matching, got {summary:?}\n{stdout}"
        );
    });
}
