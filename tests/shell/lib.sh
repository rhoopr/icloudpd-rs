# Shared helpers for kei's shell-native test scripts.
#
# Source this file after setting PROJECT_DIR (usually
# `PROJECT_DIR=$(cd "$(dirname "$0")/../.." && pwd)` in the caller).
# Loads .env for credentials and exposes helpers the three shell suites
# share: release-binary resolution, session preflight, scratch-dir
# allocation, a PASS/FAIL counter, and scoped lock cleanup.
#
# Environment variables (all optional unless noted):
#   ICLOUD_USERNAME             (required) Apple ID email
#   ICLOUD_PASSWORD             (required) Apple ID password
#   ICLOUD_TEST_COOKIE_DIR      pre-authenticated session dir (default: $PROJECT_DIR/.test-cookies)
#   KEI_TEST_ALBUM              test album name in iCloud (default: kei-test)
#   KEI_DOCKER_IMAGE            docker image to test (default: kei:latest)
#   KEI_TEST_SCRATCH_DIR        base dir for per-suite scratch (default: /tmp/kei-tests-$USER)

: "${PROJECT_DIR:?PROJECT_DIR must be set by the caller}"

# Load .env for credentials if the caller hasn't already.
if [ -z "${ICLOUD_USERNAME:-}" ] && [ -f "$PROJECT_DIR/.env" ]; then
    # shellcheck disable=SC1091
    source "$PROJECT_DIR/.env"
fi

kei_require_env() {
    if [ -z "${ICLOUD_USERNAME:-}" ] || [ -z "${ICLOUD_PASSWORD:-}" ]; then
        echo "ABORT: ICLOUD_USERNAME and ICLOUD_PASSWORD must be set (via .env or environment)."
        exit 1
    fi
}

# Strip non-alphanumeric characters, matching kei's Session::sanitized_filename().
kei_user_slug() {
    printf '%s' "$ICLOUD_USERNAME" | tr -cd '[:alnum:]'
}

kei_cookie_dir() {
    if [ -n "${ICLOUD_TEST_COOKIE_DIR:-}" ]; then
        case "$ICLOUD_TEST_COOKIE_DIR" in
            "~/"*) printf '%s/%s' "$HOME" "${ICLOUD_TEST_COOKIE_DIR#~/}" ;;
            *)     printf '%s' "$ICLOUD_TEST_COOKIE_DIR" ;;
        esac
    else
        printf '%s/.test-cookies' "$PROJECT_DIR"
    fi
}

kei_db_path() {
    printf '%s/%s.db' "$(kei_cookie_dir)" "$(kei_user_slug)"
}

kei_album() {
    printf '%s' "${KEI_TEST_ALBUM:-kei-test}"
}

kei_docker_image() {
    printf '%s' "${KEI_DOCKER_IMAGE:-kei:latest}"
}

# Base dir for per-suite scratch output. Under /tmp so the worktree stays
# clean and parallel suites in different worktrees don't collide through
# repo-local paths.
kei_scratch_base() {
    if [ -n "${KEI_TEST_SCRATCH_DIR:-}" ]; then
        printf '%s' "$KEI_TEST_SCRATCH_DIR"
    else
        printf '/tmp/kei-tests-%s' "${USER:-$(id -un)}"
    fi
}

# Allocate a suite-specific scratch directory. Usage:
#   DIR=$(kei_scratch_dir concurrency/resume)
#   # ... use $DIR ...
#   rm -rf "$DIR"
kei_scratch_dir() {
    local suite="${1:?kei_scratch_dir: suite name required}"
    local dir
    dir="$(kei_scratch_base)/$suite-$$"
    mkdir -p "$dir"
    printf '%s' "$dir"
}

# Path to the built release binary. Shell suites invoke this directly so
# sync latency reflects prod.
kei_release_bin() {
    printf '%s/target/release/kei' "$PROJECT_DIR"
}

# Ensure $PROJECT_DIR/target/release/kei exists, building it if missing
# or older than Cargo.toml/Cargo.lock. A version bump or dependency change
# therefore can't leave shell suites running a stale binary.
kei_require_release_binary() {
    local bin
    bin="$(kei_release_bin)"
    local needs_build=0
    if [ ! -x "$bin" ]; then
        needs_build=1
    elif [ "$PROJECT_DIR/Cargo.toml" -nt "$bin" ] || [ "$PROJECT_DIR/Cargo.lock" -nt "$bin" ]; then
        needs_build=1
    fi
    if [ "$needs_build" -eq 1 ]; then
        echo "Building release binary (required by shell suites)..."
        ( cd "$PROJECT_DIR" && cargo build --release ) || {
            echo "ABORT: cargo build --release failed"
            exit 1
        }
    fi
}

# Confirm the pre-authenticated session still works. Emits a PASS line
# and returns 0 on success; aborts the script on a bad session so we
# don't cascade failures through a dozen subsequent sync invocations.
kei_preflight_session() {
    local bin cookies out
    bin="$(kei_release_bin)"
    cookies="$(kei_cookie_dir)"
    out=$("$bin" login \
        --username "$ICLOUD_USERNAME" \
        --password "$ICLOUD_PASSWORD" \
        --data-dir "$cookies" 2>&1)
    if echo "$out" | grep -q "Authentication completed\|Session OK\|already authenticated"; then
        echo "  OK: session valid"
        return 0
    fi
    echo "  ABORT: session invalid or rate-limited"
    echo "$out" | tail -3
    echo "  Re-authenticate: cargo run --release -- login --data-dir $cookies"
    exit 1
}

# PASS/FAIL/SKIP counters. Callers init once, emit per check, summarize
# at the end.
kei_check_init() {
    _KEI_PASS=0
    _KEI_FAIL=0
    _KEI_SKIP=0
}

# Usage: kei_check "<label>" [<result>]
# Result defaults to $? so `foo && bar; kei_check "baz"` works.
kei_check() {
    local label="$1"
    local result="${2:-$?}"
    if [ "$result" -eq 0 ]; then
        echo "  PASS: $label"
        _KEI_PASS=$((_KEI_PASS + 1))
    else
        echo "  FAIL: $label"
        _KEI_FAIL=$((_KEI_FAIL + 1))
    fi
}

kei_skip() {
    echo "  SKIP: $1"
    _KEI_SKIP=$((_KEI_SKIP + 1))
}

# Usage: kei_check_summary "<title>"; exit $?
kei_check_summary() {
    local title="${1:-RESULTS}"
    echo ""
    echo "=================================================="
    if [ "$_KEI_SKIP" -gt 0 ]; then
        echo "  $title: $_KEI_PASS pass, $_KEI_FAIL fail, $_KEI_SKIP skip"
    else
        echo "  $title: $_KEI_PASS pass, $_KEI_FAIL fail"
    fi
    echo "=================================================="
    [ "$_KEI_FAIL" -eq 0 ]
}

# Format elapsed time as `Xm YYs`. Usage: start=$(date +%s); ...; kei_elapsed "$start"
kei_elapsed() {
    local start="$1"
    local now
    now=$(date +%s)
    local delta=$((now - start))
    printf '%dm %02ds' $((delta / 60)) $((delta % 60))
}

# Remove only the lock file scoped to the current test's username. The
# older `rm -f "$COOKIES"/*.lock` pattern would stomp on any other
# running kei process sharing the cookie dir.
kei_clear_stale_lock() {
    local cookies slug
    cookies="$(kei_cookie_dir)"
    slug="$(kei_user_slug)"
    rm -f "$cookies/$slug.lock"
}

# Print a banner with the suite name and timestamp. Used at the top of
# each shell suite so output is self-describing when piped to a log.
kei_suite_banner() {
    local title="${1:?kei_suite_banner: title required}"
    echo "=================================================="
    echo "  $title"
    echo "  $(date '+%Y-%m-%d %H:%M:%S')"
    echo "=================================================="
}
