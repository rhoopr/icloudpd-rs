#!/usr/bin/env bash
# Phase 2.5 - Docker entrypoint PUID/PGID smoke.
#
# This is offline. It verifies the NAS-facing entrypoint behavior without
# touching iCloud: numeric PUID/PGID drop, volume chown, default root mode,
# and clear rejection for invalid env combinations.

set -euo pipefail

repo_root=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
cd "$repo_root"

image="${KEI_DOCKER_IMAGE:-kei:dev}"
work="${TMPDIR:-/tmp/codex/kei/full-test/tmp}/docker-puid-smoke"
rm -rf "$work"
mkdir -p "$work/config" "$work/photos" "$work/sub-config" "$work/sub-photos"

test_puid="${KEI_DOCKER_TEST_PUID:-4321}"
test_pgid="${KEI_DOCKER_TEST_PGID:-4322}"

cleanup() {
  docker run --rm \
    -v "$work/config:/c" \
    -v "$work/photos:/p" \
    -v "$work/sub-config:/sc" \
    -v "$work/sub-photos:/sp" \
    "$image" \
    chown -R "$(id -u):$(id -g)" /c /p /sc /sp >/dev/null 2>&1 || true
  rm -rf "$work" 2>/dev/null || true
}
trap cleanup EXIT

echo "--- PUID/PGID drop and chown ---"
puid_out=$(docker run --rm \
  -e PUID="$test_puid" \
  -e PGID="$test_pgid" \
  -v "$work/config:/config" \
  -v "$work/photos:/photos" \
  --entrypoint /usr/local/bin/entrypoint.sh \
  "$image" \
  sh -c 'id -u; id -g; stat -c %u /config; stat -c %u /photos' 2>&1)
printf '%s\n' "$puid_out"
expected=$(printf '%s\n%s\n%s\n%s' "$test_puid" "$test_pgid" "$test_puid" "$test_puid")
if [[ "$puid_out" != "$expected" ]]; then
  echo "run_docker_puid_smoke: PUID/PGID output mismatch" >&2
  exit 1
fi

echo "--- default root mode ---"
root_out=$(docker run --rm \
  --entrypoint /usr/local/bin/entrypoint.sh \
  "$image" id -u 2>&1)
if [[ "$root_out" != "0" ]]; then
  echo "run_docker_puid_smoke: expected default uid 0, got $root_out" >&2
  exit 1
fi

echo "--- invalid PUID rejected ---"
bad_out=$(docker run --rm \
  -e PUID=notanumber \
  -e PGID="$test_pgid" \
  --entrypoint /usr/local/bin/entrypoint.sh \
  "$image" id 2>&1 || true)
printf '%s\n' "$bad_out"
echo "$bad_out" | grep -q "PUID/PGID must be numeric"

echo "--- partial PUID/PGID rejected ---"
partial_out=$(docker run --rm \
  -e PUID="$test_puid" \
  --entrypoint /usr/local/bin/entrypoint.sh \
  "$image" id 2>&1 || true)
printf '%s\n' "$partial_out"
echo "$partial_out" | grep -q "must be set together"

echo "--- kei subcommand under dropped uid ---"
sub_out=$(docker run --rm \
  -e ICLOUD_USERNAME=docker-puid@example.invalid \
  -e KEI_DATA_DIR=/config \
  -e PUID="$test_puid" \
  -e PGID="$test_pgid" \
  -v "$work/sub-config:/config" \
  -v "$work/sub-photos:/photos" \
  "$image" status --downloaded 2>&1)
printf '%s\n' "$sub_out" | tail -5
echo "$sub_out" | grep -q "No state database found"

sub_owner=$(stat -c %u "$work/sub-config" 2>/dev/null || echo "")
if [[ "$sub_owner" != "$test_puid" ]]; then
  echo "run_docker_puid_smoke: expected /config owner $test_puid, got $sub_owner" >&2
  exit 1
fi

echo "docker PUID smoke passed"
