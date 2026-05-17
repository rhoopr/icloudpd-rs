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
        "[download.retry].max_retries",
    ] {
        assert!(
            guide.contains(expected),
            "migration guide missing TOML-first replacement: {expected}"
        );
    }
}
