#!/usr/bin/env bash
# Offline v0.20 regression smoke.
#
# This is the quick patch-release gate for the May 27, 2026 regression set:
# token reuse, retry-state fallback, on-disk pending adoption, tolerated
# pagination shortfalls, permissive valid-media validation, inactive pass
# templates, and dotted config paths.

set -euo pipefail

repo_root=$(git rev-parse --show-toplevel 2>/dev/null) || {
  echo "run_release_regression_smoke: not in a git repo" >&2
  exit 1
}
cd "$repo_root"

echo "--- stored token decisions stay incremental ---"
cargo test --lib sync_cycle::tests::determine_sync_mode_two_normal_syncs_reuse_stored_token

echo "--- failed rows force full enumeration before normal sync ---"
cargo test --lib download::tests::incremental_with_failed_rows_falls_back_to_full_enumeration

echo "--- pending rows with existing files are adopted ---"
cargo test --lib download::pipeline::tests::producer_adopts_pending_on_disk_skip_as_downloaded

echo "--- tolerated pagination shortfalls remain warnings ---"
cargo test --lib download::tests::classify_pagination_shortfall_issue_498_fixture_is_tolerated
cargo test --lib download::tests::classify_pagination_shortfall_billimek_sharedsync_fixture_is_tolerated

echo "--- valid JPEG bytes under .PNG are saved ---"
cargo test --lib download::file::tests::attempt_download_promotes_valid_jpeg_with_png_extension_issue_507

echo "--- inactive album and smart-folder templates do not force full enumeration ---"
cargo test --lib download::tests::unfiled_only_incremental_ignores_inactive_album_path_templates

echo "--- dotted config path handling ---"
cargo test --lib config::tests::test_expand_tilde_with_injected_home_uses_path_join
host=$(rustc -vV | awk '/^host:/ { print $2 }')
if [[ "$host" == *windows* ]]; then
  cargo test --lib config::tests::test_expand_tilde_windows_home_keeps_separator_before_dot_config
else
  echo "skipping cfg(windows) dotted path smoke on host $host"
fi

echo "release regression smoke passed"
