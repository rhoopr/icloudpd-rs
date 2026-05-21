#!/usr/bin/env bash
# Phase 1.5 - host release archive smoke.
#
# Packages the already-built host release binary into a throwaway archive,
# extracts it, then smokes the extracted binary. This catches packaging drift
# without writing to dist/ or requiring a release tag.
#
# Optional env:
#   KEI_FULL_TEST_EXPECT_VERSION  exact package version expected by this run

set -euo pipefail

repo_root=$(git rev-parse --show-toplevel 2>/dev/null) || {
  echo "run_release_archive_smoke: not in a git repo" >&2
  exit 1
}
cd "$repo_root"

target=$(rustc -vV | awk '/^host:/ { print $2 }')
version=$(awk -F'"' '/^version = "/ { print $2; exit }' Cargo.toml)
binary="$repo_root/target/release/kei"

if [[ ! -x "$binary" ]]; then
  echo "run_release_archive_smoke: missing release binary at $binary" >&2
  echo "run build_release before this phase" >&2
  exit 1
fi

if [[ -n "${KEI_FULL_TEST_EXPECT_VERSION:-}" && "$version" != "$KEI_FULL_TEST_EXPECT_VERSION" ]]; then
  echo "run_release_archive_smoke: Cargo.toml version $version does not match KEI_FULL_TEST_EXPECT_VERSION=$KEI_FULL_TEST_EXPECT_VERSION" >&2
  exit 1
fi

work="${TMPDIR:-/tmp/codex/kei/full-test/tmp}/release-archive-smoke"
rm -rf "$work"
mkdir -p "$work/extract" "$work/data"

archive="$work/kei-$target.tar.gz"
tar -C "$repo_root/target/release" -czf "$archive" kei
sha256sum "$archive" > "$work/SHA256SUMS.txt"

tar -C "$work/extract" -xzf "$archive"
extracted="$work/extract/kei"

version_out=$("$extracted" --version)
expected="kei $version"
if [[ "$version_out" != "$expected" ]]; then
  echo "run_release_archive_smoke: expected '$expected', got '$version_out'" >&2
  exit 1
fi

"$extracted" --help >/dev/null

env \
  ICLOUD_USERNAME=release-smoke@example.invalid \
  KEI_PASSWORD=release-smoke-password \
  KEI_DATA_DIR="$work/data" \
  "$extracted" config show --config "$repo_root/example.config.toml" >/dev/null

echo "release archive smoke passed: $archive"
