#!/usr/bin/env bash
# Master orchestrator for `just full-test`. Runs every phase in deterministic
# order with prereq checks, rate-limit handling, per-phase logs, run records,
# and a final diff_runs.sh report.
#
# Phases run in this order:
#   begin_run    -- staging + concurrency guard, clears flag files
#   prereqs      -- check_prereqs.sh; sets .live-skipped if .env / cookies bad
#   0  gate            offline lib/cli/behavioral, fmt, clippy, audit, doc
#   0  drift           any tests/*.rs not covered by gate's --test list
#   0.5 nodefault      clippy + test with --no-default-features (xmp off)
#   0.75 fuzz_build    cargo +nightly fuzz build (skip if missing)
#   0.8  udeps         cargo +nightly udeps --all-targets
#   0.9  offline_all   cargo test --all-features
#   0.95 scope_matrix  matrix test for library/album/smart-folder/unfiled scope
#   1    build_release cargo build --release
#   1.5  release_archive_smoke
#   2    docker_build      just docker build
#   2    docker_multiarch  just docker multiarch
#   2    docker_puid_smoke
#   2    docker_version, docker_help, docker_default_cmd
#   3  test_live       live cargo (just test live)        --live
#   4  test_shell_*    auto-discovered shell suites       --live
#   5  live_*          binary smokes (run_live_smokes.sh) --live
#   5.5  live_import_rehearsal                            --live
#   6  service_smoke   just service-smoke when supported
#   opt  real_service_lifecycle when KEI_FULL_TEST_REAL_SERVICE=1 --live
#   finalize_run + diff_runs on success
#
# Live-tagged phases honor .live-skipped (prereq fail) and .rate-limited
# (Apple 503 detected) -- both clear at begin_run, both auto-skip downstream
# live phases without LLM intervention.
#
# Exit code:
#   0   no failed phases (skips + rate-limited are not failures)
#   1   first failed phase exits non-zero immediately
#   65  another /full-test run is in progress (begin_run refused)

set -euo pipefail

repo_root=$(git rev-parse --show-toplevel 2>/dev/null) || {
  echo "run_all: not in a git repo" >&2
  exit 1
}
script_dir="$(cd "$(dirname "$0")" && pwd)"
cd "$repo_root"
runs_dir="${KEI_FULL_TEST_RUNS_DIR:-/tmp/codex/kei/full-test/test-runs}"
run_started=0
current_phase="setup"
summary_path="$runs_dir/.current.jsonl"

cleanup_failed_run() {
  rc=$?
  if [[ $rc -ne 0 && $run_started -eq 1 ]]; then
    "$script_dir/render_summary.py" "$summary_path" \
      --result fail \
      --fallback-failure "$current_phase" "run stopped before this phase completed" \
      || true
    rm -f "$runs_dir/.run-marker"
  fi
  exit "$rc"
}
trap cleanup_failed_run EXIT

# --- Begin -----------------------------------------------------------------
run_id=$("$script_dir/begin_run.sh") || exit $?
run_started=1
echo "begin: run_id=$run_id (repo: $repo_root)" >&2

# --- Source .env so live phases see ICLOUD_USERNAME etc. ------------------
if [[ -f .env ]]; then
  set -a
  # shellcheck disable=SC1091
  source .env
  set +a
fi

current_phase="prereqs"
"$script_dir/check_prereqs.sh"

# Live env exports (lib.sh + just both consult these)
export ICLOUD_TEST_COOKIE_DIR="$repo_root/.test-cookies"
export KEI_TEST_ALBUM="${KEI_TEST_ALBUM:-kei-test}"
export KEI_DOCKER_IMAGE="${KEI_DOCKER_IMAGE:-kei:dev}"

# Keep child tempdirs out of the repo checkout. These are Codex working files,
# not retained .scratch documents.
full_tmp_dir="${KEI_FULL_TEST_TMPDIR:-/tmp/codex/kei/full-test/tmp}"
mkdir -p "$full_tmp_dir"
export TMPDIR="$full_tmp_dir"
export TEMP="$full_tmp_dir"
export TMP="$full_tmp_dir"

tp() { "$script_dir/time_phase.sh" "$@"; }
run_phase() {
  current_phase="$1"
  tp "$@"
}
run_live_phase() {
  current_phase="$1"
  tp --live "$@"
}

# --- Phase 0: gate --------------------------------------------------------
run_phase gate -- just gate

# --- Phase 0: drift check (extra integration tests not in gate's list) ----
covered_re='^(cli|behavioral|service_cli|service_linux|service_macos|service_status|service_windows|sync|state_auth|import_existing_live)$'
while read -r test_file; do
  t="${test_file%.rs}"
  [[ -z "$t" ]] && continue
  [[ "$t" =~ $covered_re ]] && continue
  run_phase "test_$t" -- cargo test --test "$t"
done < <(find tests -maxdepth 1 -type f -name '*.rs' -printf '%f\n' | sort)

# --- Phase 0.5: nodefault --------------------------------------------------
run_phase nodefault -- bash -c 'cargo clippy --all-targets --no-default-features -- -D warnings && cargo test --no-default-features'

# --- Phase 0.75: fuzz_build (compile only) ---------------------------------
if rustup toolchain list 2>/dev/null | grep -q '^nightly' && command -v cargo-fuzz >/dev/null 2>&1; then
  run_phase fuzz_build -- cargo +nightly fuzz build
else
  "$script_dir/record_skip.sh" fuzz_build skipped "nightly toolchain or cargo-fuzz not installed"
fi

# --- Phase 0.8: unused dependencies ---------------------------------------
current_phase="udeps"
cargo +nightly --version >/dev/null 2>&1 || {
  echo "ERROR: nightly toolchain not installed. Run: rustup toolchain install nightly" >&2
  exit 1
}
cargo udeps --version >/dev/null 2>&1 || {
  echo "ERROR: cargo-udeps not installed. Run: cargo install cargo-udeps" >&2
  exit 1
}
run_phase udeps -- cargo +nightly udeps --all-targets

# --- Phase 0.9: full offline ----------------------------------------------
run_phase offline_all -- cargo test --all-features

# --- Phase 0.95: scope matrix ----------------------------------------------
run_phase scope_matrix -- cargo test scope_contract_matrix --lib

# --- Phase 1 + 2: release build + docker builds ---------------------------
run_phase build_release -- cargo build --release
run_phase release_archive_smoke -- "$script_dir/run_release_archive_smoke.sh"
run_phase docker_build -- just docker build
run_phase docker_puid_smoke -- "$script_dir/run_docker_puid_smoke.sh"
run_phase docker_multiarch -- just docker multiarch

# --- Phase 2 (cont.): docker smokes ---------------------------------------
run_phase docker_version       -- docker run --rm "$KEI_DOCKER_IMAGE" --version
run_phase docker_help          -- docker run --rm "$KEI_DOCKER_IMAGE" --help
run_phase docker_default_cmd   -- bash -c "timeout 8 docker run --rm -e ICLOUD_USERNAME=dummy@example.com $KEI_DOCKER_IMAGE; rc=\$?; [[ \$rc -ne 2 ]]"

# --- Phase 3: live cargo --------------------------------------------------
run_live_phase test_live -- env ICLOUD_TEST_COOKIE_DIR="$ICLOUD_TEST_COOKIE_DIR" just test live

# --- Phase 4: shell suites (auto-discovered) ------------------------------
current_phase="test_shell"
"$script_dir/run_shell_suites.sh"

# --- Phase 5: live binary smokes ------------------------------------------
current_phase="live_smokes"
"$script_dir/run_live_smokes.sh"
run_live_phase live_import_rehearsal -- "$script_dir/run_live_import_rehearsal.sh"

# --- Phase 6: service smoke ------------------------------------------------
if ! command -v systemd-analyze >/dev/null 2>&1 && ! command -v plutil >/dev/null 2>&1; then
  "$script_dir/record_skip.sh" service_smoke skipped "not Linux or macOS"
else
  run_phase service_smoke -- just service-smoke
fi

if [[ "${KEI_FULL_TEST_REAL_SERVICE:-0}" == "1" ]]; then
  run_live_phase real_service_lifecycle -- "$script_dir/run_real_service_lifecycle.sh"
fi

# --- Cleanup --------------------------------------------------------------
current_phase="cleanup"
rm -rf "$full_tmp_dir/photos-test" 2>/dev/null || true

# --- Finalize + report ----------------------------------------------------
current_phase="finalize"
record=$("$script_dir/finalize_run.sh")
summary_path="$record"
echo "finalized: $record" >&2
echo
"$script_dir/diff_runs.sh"
current_phase="summary"
"$script_dir/render_summary.py" "$record" --result pass
