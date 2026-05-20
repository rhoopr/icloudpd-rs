//! Offline branch-coherence checks for packaging and migration docs.
//!
//! These intentionally inspect repository files instead of spawning Docker or
//! contacting iCloud. The live shell suites still own runtime behavior; this
//! file pins the risky static contracts that made this branch easy to regress.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::path::PathBuf;

fn repo_file(path: &str) -> String {
    let mut full = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    full.push(path);
    std::fs::read_to_string(&full)
        .unwrap_or_else(|e| panic!("read {}: {e}", full.display()))
        .replace("\r\n", "\n")
        .replace('\r', "\n")
}

#[test]
fn docker_packaging_defaults_to_service_run() {
    let dockerfile = repo_file("Dockerfile");
    assert!(
        dockerfile.contains("ENV KEI_DATA_DIR=/config"),
        "Docker image must keep session state and config under /config"
    );
    assert!(
        dockerfile.contains(r#"CMD ["service", "run", "--config", "/config/config.toml"]"#),
        "Docker default command must run service mode so the container stays alive"
    );
    assert!(
        !dockerfile.contains("KEI_WATCH_WITH_INTERVAL"),
        "removed watch env mirror must not return in the Dockerfile"
    );

    let entrypoint = repo_file("docker/entrypoint.sh");
    let service_index = entrypoint
        .find("|service)")
        .or_else(|| entrypoint.find("|service|"))
        .expect("entrypoint whitelist must include the kei service subcommand");
    let command_lookup = entrypoint
        .find("command -v")
        .expect("entrypoint should still fall back to command lookup");
    assert!(
        service_index < command_lookup,
        "`service` must be recognized as a kei subcommand before shell command lookup"
    );
}

#[test]
fn migration_guide_uses_toml_for_durable_sync_settings() {
    let guide = repo_file("docs/migration-from-icloudpd.md");
    let stale = [
        "kei sync --library",
        "kei sync --download-dir",
        "kei sync --album",
        "| `-p`, `--password` | Same",
        "| `-a`, `--album` | Same",
        "| `--watch-with-interval` | Same",
        "| `--notification-script` | Same flag",
        "| `--threads-num` | `--threads`",
        "| `--report-json`",
        "| `--http-bind`, `--http-port`",
    ];
    for needle in stale {
        assert!(
            !guide.contains(needle),
            "migration guide still advertises stale sync config surface: {needle}"
        );
    }

    for expected in [
        "kei sync --config ~/.config/kei/config.toml",
        "[download]\ndirectory = \"~/Photos/iCloud\"",
        "[filters]\nlibraries = [\"all\"]",
        "[watch].interval",
        "[notifications].script",
        "[download.retry].per_transfer",
    ] {
        assert!(
            guide.contains(expected),
            "migration guide missing TOML-first replacement: {expected}"
        );
    }
}

#[test]
fn full_test_routes_child_tempdirs_to_repo_scratch() {
    let run_all = repo_file("scripts/full-test/run_all.sh");
    let tmp_assignment =
        "full_tmp_dir=\"${KEI_FULL_TEST_TMPDIR:-$repo_root/.scratch/full-test-tmp}\"";
    let tmp_export = "export TMPDIR=\"$full_tmp_dir\"";
    let temp_export = "export TEMP=\"$full_tmp_dir\"";
    let tmp_windows_export = "export TMP=\"$full_tmp_dir\"";

    for expected in [
        tmp_assignment,
        "mkdir -p \"$full_tmp_dir\"",
        tmp_export,
        temp_export,
        tmp_windows_export,
    ] {
        assert!(
            run_all.contains(expected),
            "full-test orchestrator missing repo-local tempdir setup: {expected}"
        );
    }

    let export_pos = run_all
        .find(tmp_export)
        .expect("full-test must export TMPDIR before live tests");
    let live_pos = run_all
        .find("run_live_phase test_live")
        .expect("full-test live phase must still exist");
    let shell_pos = run_all
        .find("run_shell_suites.sh")
        .expect("full-test shell phase must still exist");

    assert!(
        export_pos < live_pos && export_pos < shell_pos,
        "TMPDIR must be set before live cargo and shell phases allocate tempdirs"
    );
}

#[test]
fn live_import_smoke_uses_toml_directory() {
    let smokes = repo_file("scripts/full-test/run_live_smokes.sh");

    assert!(
        smokes.contains("import-existing --dry-run --recent 5 --config \"$sync_config\""),
        "import-existing live smoke must pass the generated TOML config"
    );
    assert!(
        !smokes.contains("import-existing --dry-run --recent 5 --download-dir"),
        "import-existing live smoke must not use the removed --download-dir flag"
    );
    assert!(
        smokes.contains(r#"${TMPDIR:-$repo_root/.scratch/full-test-tmp}/photos-test"#),
        "live smoke download scratch should follow full-test's repo-local TMPDIR"
    );
}
