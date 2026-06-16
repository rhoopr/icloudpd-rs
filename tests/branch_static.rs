//! Offline branch-coherence checks for packaging and migration docs.
//!
//! These intentionally inspect repository files instead of spawning Docker or
//! contacting iCloud. The live shell suites still own runtime behavior; this
//! file pins the risky static contracts that made this branch easy to regress.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn repo_file(path: &str) -> String {
    let mut full = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    full.push(path);
    std::fs::read_to_string(&full)
        .unwrap_or_else(|e| panic!("read {}: {e}", full.display()))
        .replace("\r\n", "\n")
        .replace('\r', "\n")
}

fn repo_path(path: &str) -> PathBuf {
    let mut full = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    full.push(path);
    full
}

fn rust_files_under(path: &Path, out: &mut Vec<PathBuf>) {
    for entry in
        std::fs::read_dir(path).unwrap_or_else(|e| panic!("read dir {}: {e}", path.display()))
    {
        let entry = entry.unwrap_or_else(|e| panic!("read dir entry {}: {e}", path.display()));
        let path = entry.path();
        if path.is_dir() {
            rust_files_under(&path, out);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

fn production_source(source: &str) -> &str {
    let source = source
        .split_once("\n#[cfg(test)]\nmod tests")
        .map_or(source, |(prod, _)| prod);
    source
        .split_once("\n#[cfg(test)]\nmod wiremock_tests")
        .map_or(source, |(prod, _)| prod)
}

fn function_name(line: &str) -> Option<String> {
    let line = line.trim_start();
    let (_, rest) = line.split_once("fn ")?;
    let name: String = rest
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
        .collect();
    (!name.is_empty()).then_some(name)
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
        "don't auto-copy files from the old `icloudpd-rs` paths",
        "cp ~/.config/icloudpd-rs/config.toml ~/.config/kei/config.toml",
        "cp ~/.icloudpd-rs/* ~/.config/kei/cookies/",
    ] {
        assert!(
            guide.contains(expected),
            "migration guide missing TOML-first replacement: {expected}"
        );
    }
}

#[test]
fn notification_script_docs_pin_legacy_env_plus_report_json() {
    let changelog = repo_file("CHANGELOG.md");
    assert!(
        changelog.contains(
            "Notification scripts keep the existing `KEI_ICLOUD_USERNAME` and per-cycle `KEI_*` stat variables"
        ),
        "changelog must pin the legacy notification-script env contract"
    );
    assert!(
        changelog.contains("now also receive `KEI_REPORT_JSON` when `[report].json` is configured"),
        "changelog must call out report JSON as an addition"
    );

    let guide = repo_file("docs/migration-from-icloudpd.md");
    assert!(
        guide.contains("kei sends `KEI_EVENT`, `KEI_MESSAGE`, `KEI_ICLOUD_USERNAME`"),
        "migration guide must keep the legacy notification-script username env var"
    );
    assert!(
        guide.contains(
            "per-cycle `KEI_*` stats, and `KEI_REPORT_JSON` when `[report].json` is configured"
        ),
        "migration guide must document legacy stats plus report JSON"
    );

    let example_config = repo_file("example.config.toml");
    assert!(
        example_config.contains(
            "receives KEI_EVENT, KEI_MESSAGE, KEI_ICLOUD_USERNAME, per-cycle KEI_* stats, and KEI_REPORT_JSON when [report].json is configured"
        ),
        "example config must describe the current notification-script env surface"
    );
}

#[test]
fn full_test_routes_child_tempdirs_to_tmp_codex() {
    let run_all = repo_file("scripts/full-test/run_all.sh");
    let tmp_assignment = "full_tmp_dir=\"${KEI_FULL_TEST_TMPDIR:-/tmp/codex/kei/full-test/tmp}\"";
    let tmp_export = "export TMPDIR=\"$full_tmp_dir\"";
    let temp_export = "export TEMP=\"$full_tmp_dir\"";
    let tmp_windows_export = "export TMP=\"$full_tmp_dir\"";
    let shell_scratch_export =
        "export KEI_TEST_SCRATCH_DIR=\"${KEI_TEST_SCRATCH_DIR:-$full_tmp_dir/shell}\"";

    for expected in [
        tmp_assignment,
        "mkdir -p \"$full_tmp_dir\"",
        tmp_export,
        temp_export,
        tmp_windows_export,
        shell_scratch_export,
        "mkdir -p \"$KEI_TEST_SCRATCH_DIR\"",
    ] {
        assert!(
            run_all.contains(expected),
            "full-test orchestrator missing /tmp/codex tempdir setup: {expected}"
        );
    }

    let export_pos = run_all
        .find(tmp_export)
        .expect("full-test must export TMPDIR before live tests");
    let shell_export_pos = run_all
        .find(shell_scratch_export)
        .expect("full-test must export KEI_TEST_SCRATCH_DIR before shell tests");
    let live_pos = run_all
        .find("run_live_phase test_live")
        .expect("full-test live phase must still exist");
    let shell_pos = run_all
        .find("run_shell_suites.sh")
        .expect("full-test shell phase must still exist");

    assert!(
        export_pos < live_pos && export_pos < shell_pos && shell_export_pos < shell_pos,
        "TMPDIR and KEI_TEST_SCRATCH_DIR must be set before live cargo and shell phases allocate tempdirs"
    );
}

#[test]
fn full_test_run_start_metadata_is_stable_until_finalize() {
    let begin = repo_file("scripts/full-test/begin_run.sh");
    let finalize = repo_file("scripts/full-test/finalize_run.sh");

    for expected in [
        "start_file=\"$runs_dir/.run-started-at\"",
        "lockfile=\"$runs_dir/.lock\"",
        "flock 9",
        "if [[ $marker_age -lt 3600 ]]; then",
        "staging: $current (no records yet)",
        "date +%Y-%m-%dT%H:%M:%S > \"$start_file\"",
    ] {
        assert!(
            begin.contains(expected),
            "begin_run must atomically record stable run-start metadata: {expected}"
        );
    }

    for expected in [
        "start_file=\"$runs_dir/.run-started-at\"",
        "if [[ -s \"$start_file\" ]]; then",
        "started_at=$(head -n 1 \"$start_file\")",
        "rm -f \"$current\" \"$runs_dir/.run-marker\" \"$start_file\"",
    ] {
        assert!(
            finalize.contains(expected),
            "finalize_run must use and clean the stable run-start metadata: {expected}"
        );
    }
}

#[test]
fn full_test_reports_include_newer_phase_metadata() {
    let render = repo_file("scripts/full-test/render_summary.py");
    let diff = repo_file("scripts/full-test/diff_runs.sh");

    for phase in [
        "release_archive_smoke",
        "docker_puid_smoke",
        "live_import_rehearsal",
        "real_service_lifecycle",
    ] {
        assert!(
            render.contains(phase),
            "render_summary.py must sort and display newer full-test phase {phase}"
        );
        assert!(
            diff.contains(phase),
            "diff_runs.sh must assign phase number/test metadata for {phase}"
        );
    }
}

#[test]
fn full_test_prereqs_report_script_tooling_gaps() {
    let prereqs = repo_file("scripts/full-test/check_prereqs.sh");

    for expected in [
        "optional-missing $cmd not found",
        "report_tool shellcheck shellcheck 1",
        "report_tool shfmt shfmt 1",
        "report_tool ruff ruff 1",
        "report_tool actionlint actionlint 1",
        "report_tool cargo-bloat cargo-bloat 1",
    ] {
        assert!(
            prereqs.contains(expected),
            "full-test prereqs must report script/tooling availability: {expected}"
        );
    }
}

#[test]
fn full_test_checks_gnu_linux_userland_before_begin_run() {
    let run_all = repo_file("scripts/full-test/run_all.sh");
    let check = repo_file("scripts/full-test/check_userland.sh");

    assert!(
        run_all.contains(r#""$script_dir/check_userland.sh""#),
        "full-test must check local userland before mutating run state"
    );
    let check_pos = run_all
        .find(r#""$script_dir/check_userland.sh""#)
        .expect("userland check must be present");
    let begin_pos = run_all
        .find(r#"run_id=$("$script_dir/begin_run.sh")"#)
        .expect("begin_run call must still exist");
    assert!(
        check_pos < begin_pos,
        "userland check must run before begin_run writes markers"
    );

    for expected in [
        "find does not support GNU -printf",
        "stat does not support GNU -c",
        "timeout command is present but failed a basic smoke test",
        "full-test: unsupported local userland",
        "GNU/Linux userland tools",
    ] {
        assert!(
            check.contains(expected),
            "userland check must explain unsupported local tooling: {expected}"
        );
    }
}

#[test]
fn full_test_docker_smokes_quote_configured_image() {
    let run_all = repo_file("scripts/full-test/run_all.sh");
    let shell_suites = repo_file("scripts/full-test/run_shell_suites.sh");
    let docker_puid = repo_file("scripts/full-test/run_docker_puid_smoke.sh");
    let shell_lib = repo_file("tests/shell/lib.sh");

    for expected in [
        r#"export KEI_DOCKER_IMAGE="${KEI_DOCKER_IMAGE:-kei:dev}""#,
        r#"docker run --rm "$KEI_DOCKER_IMAGE" --version"#,
        r#"docker run --rm "$KEI_DOCKER_IMAGE" --help"#,
        r#"bash -c 'timeout 8 docker run --rm -e ICLOUD_USERNAME=dummy@example.com "$KEI_DOCKER_IMAGE"; rc=$?; [[ $rc -ne 2 ]]'"#,
    ] {
        assert!(
            run_all.contains(expected),
            "full-test docker smokes must use the exported, quoted docker image: {expected}"
        );
    }

    assert!(
        !run_all.contains(" $KEI_DOCKER_IMAGE; rc=\\$?"),
        "full-test docker_default_cmd must not interpolate the image into the bash -c body unquoted"
    );

    for expected in [
        r#"image="${KEI_DOCKER_IMAGE:-kei:dev}""#,
        r#"KEI_DOCKER_IMAGE="$image""#,
    ] {
        assert!(
            shell_suites.contains(expected),
            "shell suite runner must pass the selected docker image through: {expected}"
        );
    }
    assert!(
        docker_puid.contains(r#"image="${KEI_DOCKER_IMAGE:-kei:dev}""#),
        "docker PUID smoke must use the same KEI_DOCKER_IMAGE default as full-test"
    );
    assert!(
        shell_lib.contains(r#"printf '%s' "${KEI_DOCKER_IMAGE:-kei:latest}""#),
        "standalone shell tests must keep their documented local default"
    );
}

#[test]
fn local_gate_includes_script_and_workflow_lint_recipes() {
    let justfile = repo_file("justfile");
    let ci = repo_file(".github/workflows/ci.yml");

    for expected in [
        "lint-workflows:",
        "python3 .github/scripts/check_workflow_hardening.py",
        "PYTHONPYCACHEPREFIX=\"$pycache_dir\" python3 -m py_compile .github/scripts/*.py",
        "actionlint .github/workflows/*.yml",
        "lint-scripts:",
        "bash -n \"${shell_files[@]}\"",
        "PYTHONPYCACHEPREFIX=\"$pycache_dir\" python3 -m py_compile \"${python_files[@]}\"",
        "shellcheck \"${shell_files[@]}\"",
        "shfmt -d \"${shell_files[@]}\"",
        "ruff check \"${python_files[@]}\"",
    ] {
        assert!(
            justfile.contains(expected),
            "justfile must keep script/workflow lint coverage: {expected}"
        );
    }

    let gate = justfile
        .split_once("gate:\n")
        .map(|(_, tail)| tail)
        .and_then(|tail| tail.split_once("\n\n").map(|(gate, _)| gate))
        .expect("justfile must keep gate recipe");
    for expected in ["just lint-workflows", "just lint-scripts"] {
        assert!(gate.contains(expected), "just gate must run {expected}");
    }

    assert!(
        ci.contains("  script-lint:\n"),
        "CI workflow must keep the script-lint job"
    );
    assert!(
        ci.contains("PYTHONPYCACHEPREFIX=/tmp/codex/kei/pycache python3 -m py_compile"),
        "CI script lint must route generated Python bytecode outside the repo tree"
    );
    let aggregate = ci
        .split_once("  ci:\n")
        .map(|(_, tail)| tail)
        .expect("CI aggregate job must exist");
    assert!(
        aggregate.contains("      - script-lint\n"),
        "aggregate CI job must require script-lint"
    );
}

#[test]
fn aggregate_ci_depends_on_no_default_feature_gate() {
    let ci = repo_file(".github/workflows/ci.yml");
    assert!(
        ci.contains("  test_no_default:\n"),
        "CI workflow must keep the no-default-features job"
    );

    let aggregate = ci
        .split_once("  ci:\n")
        .map(|(_, tail)| tail)
        .expect("CI aggregate job must exist");
    assert!(
        aggregate.contains("      - test_no_default\n"),
        "aggregate CI job must require test_no_default so branch protection sees no-default failures"
    );
}

#[test]
fn rust_ci_runs_on_main_push_without_pr_only_coverage() {
    let ci = repo_file(".github/workflows/ci.yml");
    let hardening = repo_file(".github/scripts/check_workflow_hardening.py");

    for expected in [
        "  push:\n    branches: [main]",
        "if [[ \"$EVENT_NAME\" != \"pull_request\" ]]; then",
        "github.event_name == 'pull_request' && needs.detect.outputs.code == 'true'",
    ] {
        assert!(
            ci.contains(expected),
            "CI must run on main pushes while keeping coverage PR-only: {expected}"
        );
    }

    for expected in [
        "push:\\n    branches: [main]",
        "if [[ \"$EVENT_NAME\" != \"pull_request\" ]]; then",
        "github.event_name == 'pull_request' && needs.detect.outputs.code == 'true'",
    ] {
        assert!(
            hardening.contains(expected),
            "workflow hardening must pin the CI push/coverage guard: {expected}"
        );
    }
}

#[test]
fn release_homebrew_downloads_fail_fast_and_verify_checksums() {
    let release = repo_file(".github/workflows/release.yml");
    let hardening = repo_file(".github/scripts/check_workflow_hardening.py");

    for expected in [
        r#"curl -fsSL "$BASE/SHA256SUMS.txt" -o /tmp/SHA256SUMS.txt"#,
        r#"curl -fsSL "$BASE/$file" -o "/tmp/$file""#,
        r#"expected_sha=$(awk -v file="$file" '$2 == file { print $1 }' /tmp/SHA256SUMS.txt)"#,
        r#"if [ "$actual_sha" != "$expected_sha" ]; then"#,
        r#"SHAS[$key]="$actual_sha""#,
    ] {
        assert!(
            release.contains(expected),
            "release Homebrew update must fail fast and verify checksums: {expected}"
        );
    }

    for expected in [
        r#"curl -fsSL "$BASE/SHA256SUMS.txt""#,
        r#"curl -fsSL "$BASE/$file""#,
        r#"expected_sha=$(awk -v file="$file""#,
        r#"if [ "$actual_sha" != "$expected_sha" ]; then"#,
        r#"SHAS[$key]="$actual_sha""#,
    ] {
        assert!(
            hardening.contains(expected),
            "workflow hardening script must pin release invariant: {expected}"
        );
    }
}

#[test]
fn service_smoke_path_filters_cover_shared_dispatch() {
    let service_smoke = repo_file(".github/workflows/service-smoke.yml");
    let hardening = repo_file(".github/scripts/check_workflow_hardening.py");

    for path in [
        "src/service/**",
        "src/commands/service.rs",
        "src/cli.rs",
        "src/config.rs",
        "src/lib.rs",
        "src/commands/status.rs",
    ] {
        let expected = format!("- '{path}'");
        assert!(
            service_smoke.contains(&expected),
            "service-smoke path filter must include {path}"
        );
        assert!(
            hardening.contains(path),
            "workflow hardening script must enforce service-smoke path filter for {path}"
        );
    }
}

#[test]
fn contributor_docs_match_current_gate() {
    let contributing = repo_file("CONTRIBUTING.md");
    let pr_template = repo_file(".github/pull_request_template.md");

    for expected in [
        "cargo fmt --all --check",
        "cargo clippy --all-targets --all-features -- -D warnings",
        "cargo clippy --all-targets --no-default-features -- -D warnings",
        "cargo test --all-features",
        "cargo test --no-default-features",
        "RUSTDOCFLAGS=\"-Dwarnings\" cargo doc --no-deps --all-features",
        "cargo audit --deny warnings",
        "python3 .github/scripts/check_workflow_hardening.py",
        "PYTHONPYCACHEPREFIX=/tmp/codex/kei/pycache python3 -m py_compile",
        "bash -n scripts/check-contracts scripts/check-roundtrip-gate.sh",
        "scripts/check-contracts",
        "bash scripts/check-roundtrip-gate.sh",
    ] {
        assert!(
            contributing.contains(expected),
            "CONTRIBUTING.md must document current gate command: {expected}"
        );
    }

    assert!(
        pr_template.contains("`just gate` passes"),
        "PR template should ask reviewers for the current local gate"
    );
    assert!(
        !pr_template.contains("cargo test --bin kei --test cli --test behavioral"),
        "PR template must not keep stale partial test command"
    );
}

#[test]
fn roundtrip_gate_documents_heuristic_limits_and_bypass_rationale() {
    let gate = repo_file("scripts/check-roundtrip-gate.sh");

    for expected in [
        "Heuristic diff guard for serializer changes",
        "intentionally heuristic",
        "false-positive",
        "false-negative",
        "review prompt, not proof that the code is wrong",
        "written reviewer rationale",
        "heuristic serializer change detected without a round-trip test edit",
    ] {
        assert!(
            gate.contains(expected),
            "roundtrip gate must document heuristic behavior and bypass rationale: {expected}"
        );
    }
}

#[test]
fn bug_report_template_requires_web_access_and_redaction() {
    let bug = repo_file(".github/ISSUE_TEMPLATE/bug_report.yml");

    for expected in [
        "I have confirmed that ADP is disabled",
        "required: true",
        "Redact Apple IDs, passwords, session cookies, bearer tokens, webhook URLs",
    ] {
        assert!(
            bug.contains(expected),
            "bug report template must keep triage and redaction guidance: {expected}"
        );
    }
}

#[test]
fn loopback_bound_tests_keep_explicit_skip_gate() {
    let helper = repo_file("src/test_helpers.rs");
    let metrics = repo_file("src/metrics.rs");
    let readme = repo_file("tests/README.md");

    for expected in [
        "loopback bind is not permitted on this host",
        "pub(crate) fn skip_if_loopback_bind_blocked",
        "pub(crate) async fn start_wiremock_or_skip",
        "macro_rules! start_wiremock_or_skip",
        "None => return",
    ] {
        assert!(
            helper.contains(expected),
            "loopback test helper must keep explicit skip support: {expected}"
        );
    }

    for expected in [
        "spawn_server_with_staleness_threshold_does_not_panic_inside_runtime",
        "spawn_server_serves_metrics_and_healthz_over_http",
        "skip_if_loopback_bind_blocked",
    ] {
        assert!(
            metrics.contains(expected),
            "metrics HTTP tests must remain covered by the loopback skip gate: {expected}"
        );
    }

    for expected in [
        "Some offline unit tests bind `127.0.0.1`",
        "Normal CI hosts still run the",
        "tests strictly; restricted sandboxes",
        "explicit skip line instead of a",
        "false bind failure",
    ] {
        assert!(
            readme.contains(expected),
            "tests/README.md must document loopback skip semantics: {expected}"
        );
    }
}

#[test]
fn audit_ignores_carry_removal_triggers() {
    let audit = repo_file(".cargo/audit.toml");

    for expected in [
        "Remove this ignore once little_exif drops paste",
        "Remove this ignore once reqwest's QUIC stack no longer pulls rand",
    ] {
        assert!(
            audit.contains(expected),
            "audit ignore must document removal trigger: {expected}"
        );
    }
}

#[test]
fn funding_file_contains_only_configured_sponsor_platforms() {
    let funding = repo_file(".github/FUNDING.yml");

    assert_eq!(
        funding.trim(),
        "ko_fi: rhoopr",
        "FUNDING.yml should not keep unconfigured GitHub template placeholders"
    );
}

#[test]
fn typed_error_downcasts_stay_in_named_classifier_boundaries() {
    let allowed = [
        "classify_api_error",
        "classify_auth_flow_error",
        "classify_auth_retry_error",
        "classify_cli_parse_exit",
        "classify_download_task_error",
        "classify_exit_error",
        "classify_incremental_error",
        "classify_srp_post_error",
        "classify_sync_auth_error",
        "is_session_error",
        "map_library_init_error",
    ]
    .into_iter()
    .map(String::from)
    .collect::<BTreeSet<_>>();
    let mut files = Vec::new();
    rust_files_under(&repo_path("src"), &mut files);

    let mut observed = BTreeSet::new();
    let mut violations = Vec::new();
    for path in files {
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
            .replace("\r\n", "\n")
            .replace('\r', "\n");
        let prod = production_source(&source);
        let mut current_fn: Option<String> = None;

        for (index, line) in prod.lines().enumerate() {
            if let Some(name) = function_name(line) {
                current_fn = Some(name);
            }
            if line.contains("downcast_ref::<") || line.contains(".downcast::<") {
                let Some(name) = current_fn.as_deref() else {
                    violations.push(format!(
                        "{}:{} downcast outside a function: {}",
                        path.strip_prefix(env!("CARGO_MANIFEST_DIR"))
                            .unwrap_or(path.as_path())
                            .display(),
                        index + 1,
                        line.trim()
                    ));
                    continue;
                };
                if allowed.contains(name) {
                    observed.insert(name.to_string());
                } else {
                    violations.push(format!(
                        "{}:{} downcast in {name}: {}",
                        path.strip_prefix(env!("CARGO_MANIFEST_DIR"))
                            .unwrap_or(path.as_path())
                            .display(),
                        index + 1,
                        line.trim()
                    ));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "production typed-error downcasts must stay in named classifiers or documented owner boundaries:\n{}",
        violations.join("\n")
    );
    assert_eq!(
        observed, allowed,
        "classifier inventory changed; update the #587 boundary list deliberately"
    );
}

#[test]
fn live_test_recipe_forces_all_features_after_nodefault_phase() {
    let justfile = repo_file("justfile");
    let live_case = justfile
        .split("live)")
        .nth(1)
        .and_then(|tail| tail.split(";;").next())
        .expect("justfile must have a live test recipe case");

    for suite in ["sync", "state_auth", "import_existing_live"] {
        let expected = format!("cargo test --all-features --test {suite}");
        assert!(
            live_case.contains(&expected),
            "`just test live` must rebuild {suite}'s child binary with XMP after full-test's no-default phase"
        );
    }
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
        smokes.contains(r#"${TMPDIR:-/tmp/codex/kei/full-test/tmp}/photos-test"#),
        "live smoke download scratch should follow full-test's TMPDIR"
    );
}

#[test]
fn live_import_rehearsal_seeds_album_with_per_filter_recent_scope() {
    let rehearsal = repo_file("scripts/full-test/run_live_import_rehearsal.sh");

    assert!(
        rehearsal.contains("sync --recent 10 --recent-scope per-filter --no-progress-bar"),
        "live import rehearsal must seed from the selected album's recent window, not the global library frontier"
    );
    assert!(
        rehearsal.contains("set +e\n  \"$@\" >\"$out\" 2>\"$err\"\n  local rc=$?\n  set -e"),
        "live import rehearsal must print command tails before propagating a failed command"
    );
    assert!(
        rehearsal.contains("import-existing --dry-run --recent 10 --force-empty --no-progress-bar"),
        "live import rehearsal dry-run should keep import-existing bounded to the same recent count"
    );
    assert!(
        rehearsal.contains("import-existing --recent 10 --force-empty --no-progress-bar"),
        "live import rehearsal real import should keep import-existing bounded to the same recent count"
    );
}

#[test]
fn full_test_cross_zone_album_phase_is_opt_in_and_checks_source_zone() {
    let run_all = repo_file("scripts/full-test/run_all.sh");
    let script = repo_file("scripts/full-test/run_cross_zone_album_hydration.sh");
    let readme = repo_file("tests/README.md");

    assert!(
        run_all.contains(r#"if [[ -n "${KEI_FULL_TEST_CROSS_ZONE_ALBUM:-}" ]]; then"#),
        "cross-zone live full-test phase must stay opt-in"
    );
    assert!(
        run_all.contains(
            r#"run_live_phase live_cross_zone_album -- "$script_dir/run_cross_zone_album_hydration.sh""#
        ),
        "cross-zone album phase must use live phase wrapping for prereq and rate-limit handling"
    );
    assert!(
        script.contains("libraries = [\"all\"]"),
        "cross-zone fixture sync must include all visible libraries"
    );
    assert!(
        script.contains("a.library <> 'PrimarySync'"),
        "cross-zone fixture assertion must prove a non-primary source zone"
    );
    assert!(
        script.contains("JOIN asset_albums aa"),
        "cross-zone fixture assertion must be tied to the selected album membership"
    );
    assert!(
        readme.contains("KEI_FULL_TEST_CROSS_ZONE_ALBUM"),
        "tests README must document the opt-in cross-zone fixture"
    );
}
