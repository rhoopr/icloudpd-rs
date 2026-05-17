#!/usr/bin/env bash
# Phase 5 -- live binary smokes against the production release binary.
# Each smoke wraps a single CLI subcommand through time_phase.sh --live so
# the rate-limit / prereq-skip flags are honored automatically.
#
# Required env when live phases are not being skipped:
#   ICLOUD_USERNAME  iCloud account (sourced from .env by the orchestrator)
#
# Optional env:
#   KEI_TEST_DATA_DIR  cookie / db dir (default .test-cookies under repo)
#   KEI_TEST_ALBUM     album name for sync dry-run (default icloudpd-test)
#   KEI_TEST_DOWNLOAD_DIR  scratch dir for sync/import dry-run (default /tmp/codex/photos-test)
#
# Adding a new CLI subcommand: add a smoke line below. Don't add destructive
# (reset) or interactive (login) commands -- they're covered elsewhere.

set -euo pipefail

repo_root=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
script_dir="$(cd "$(dirname "$0")" && pwd)"
time_phase="$script_dir/time_phase.sh"
binary="$repo_root/target/release/kei"

if [[ ! -x "$binary" ]]; then
  echo "run_live_smokes: missing release binary at $binary (run Phase 1 first)" >&2
  exit 1
fi

USR="${ICLOUD_USERNAME:-missing@example.invalid}"
DD="${KEI_TEST_DATA_DIR:-$repo_root/.test-cookies}"
ALBUM="${KEI_TEST_ALBUM:-icloudpd-test}"
DOWNLOAD_DIR="${KEI_TEST_DOWNLOAD_DIR:-/tmp/codex/photos-test}"
mkdir -p "$DOWNLOAD_DIR"

run() {
  local phase="$1"; shift
  "$time_phase" --live "$phase" -- "$@"
}

# Wrappers for commands that need shell composition (rc check, etc.).
verify_wrapper() {
  set +e
  "$binary" verify --username "$USR" --data-dir "$DD"
  rc=$?
  set -e
  # rc=2 is clap parse error; everything else (including non-zero data
  # mismatches) means the command at least reached the binary correctly.
  [[ $rc -ne 2 ]]
}
export -f verify_wrapper
export USR DD binary

run live_status            "$binary" status                       --username "$USR" --data-dir "$DD"
run live_libraries         "$binary" list libraries               --username "$USR" --data-dir "$DD"
run live_albums            "$binary" list albums                  --username "$USR" --data-dir "$DD"
run live_dryrun            "$binary" sync --dry-run --recent 5 -a "$ALBUM" --download-dir "$DOWNLOAD_DIR" --username "$USR" --data-dir "$DD"
run live_config_show       "$binary" config show                  --username "$USR" --data-dir "$DD"
run live_verify            bash -c verify_wrapper
run live_reconcile_dryrun  "$binary" reconcile --dry-run          --username "$USR" --data-dir "$DD"
run live_password_backend  "$binary" password backend             --username "$USR" --data-dir "$DD"
run live_import_dryrun     "$binary" import-existing --dry-run --recent 5 --download-dir "$DOWNLOAD_DIR" --username "$USR" --data-dir "$DD"
