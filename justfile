# Local dev recipes. Bare `just` lists them. No one-shot aliases over
# raw cargo commands - recipes only exist when they compose, set up
# env, or dispatch on a mode.

set shell := ["bash", "-euo", "pipefail", "-c"]
set tempdir := "/tmp"

_default:
    @just --list

# Pre-push gate: fmt + clippy + offline tests + doc + audit + workflow hardening + typos +
# round-trip property gate. The round-trip gate fails when this branch
# adds/changes a serializer in src/ without a paired round-trip test;
# see scripts/check-roundtrip-gate.sh for the detector and override.
gate:
    cargo fmt --all --check
    cargo clippy --all-targets --all-features -- -D warnings
    cargo clippy --all-targets --no-default-features -- -D warnings
    cargo test --all-features
    cargo test --no-default-features
    RUSTDOCFLAGS="-Dwarnings" cargo doc --no-deps --all-features
    cargo fetch --locked
    cargo audit --deny warnings
    python3 .github/scripts/check_workflow_hardening.py
    typos
    bash scripts/check-roundtrip-gate.sh

# Pre-release battery with phase logs, metrics, Docker smokes, and live smokes.
# Stops on first failure. Keeps a /tmp/kei-full-test-*.log only when failing.
full-test:
    #!/usr/bin/env bash
    set -Eeuo pipefail
    log_path=$(mktemp "/tmp/kei-full-test-$(date +%Y%m%dT%H%M%S)-XXXXXX.log")
    failed_line=""
    failed_command=""
    record_failure() {
        failed_line="${BASH_LINENO[0]:-unknown}"
        failed_command="$BASH_COMMAND"
    }
    finish() {
        rc=$?
        if [ "$rc" -eq 0 ]; then
            rm -f "$log_path"
            return 0
        fi
        {
            echo ""
            echo "full-test: failed with exit code $rc"
            if [ -n "$failed_command" ]; then
                echo "full-test: failed command near line $failed_line: $failed_command"
            fi
            echo "full-test: log saved at $log_path"
        } >&2
        exit "$rc"
    }
    trap record_failure ERR
    trap finish EXIT
    exec > >(tee "$log_path") 2>&1
    echo "full-test: keeping failure log at $log_path if this run fails"
    scripts/full-test/run_all.sh

# Compact history table for previous `just full-test` runs.
full-test-history N="10":
    scripts/full-test/history.sh "{{N}}"

# Test dispatcher: offline | fast | live | concurrency | state | docker | PATTERN.
test MODE="":
    #!/usr/bin/env bash
    set -euo pipefail
    # Shared live-auth setup: sources .env if needed, applies the
    # maintainer's album default. Cookie dir falls through to the
    # harness default (./.test-cookies) when unset. Overridable via
    # the environment.
    _live_env() {
        source scripts/just/live-env.sh
    }
    case "{{MODE}}" in
        "")
            cargo test --all-features
            ;;
        fast)
            cargo test --lib -- --test-threads=1
            cargo test --test cli --test behavioral --test service_cli --test service_linux --test service_macos --test service_windows --test service_status
            ;;
        live)
            _live_env
            cargo test --test sync -- --ignored --test-threads=1
            cargo test --test state_auth -- --ignored --test-threads=1
            cargo test --test import_existing_live -- --ignored --test-threads=1
            ;;
        concurrency)
            _live_env
            ./tests/shell/concurrency.sh
            ;;
        state)
            _live_env
            ./tests/shell/state-machine.sh
            ;;
        docker)
            _live_env
            ./tests/shell/docker.sh
            ;;
        *)
            cargo test "{{MODE}}"
            ;;
    esac

# Coverage: (none) | html | live | patch [BASE]. `live` merges sync + state_auth into the offline baseline.
cov MODE="" BASE="main":
    #!/usr/bin/env bash
    set -euo pipefail
    _live_env() {
        source scripts/just/live-env.sh
    }
    case "{{MODE}}" in
        "")
            cargo llvm-cov --all-features
            ;;
        html)
            cargo llvm-cov --all-features --html
            echo "Report: target/llvm-cov/html/index.html"
            ;;
        live)
            _live_env
            # --no-report accumulates coverage across multiple test
            # binary invocations so we can run the live suites under the
            # same profile data as the offline ones. The final `report`
            # call prints the merged summary.
            cargo llvm-cov clean --workspace
            cargo llvm-cov --no-report --lib
            cargo llvm-cov --no-report --test cli
            cargo llvm-cov --no-report --test behavioral
            cargo llvm-cov --no-report --test sync -- --include-ignored --test-threads=1
            cargo llvm-cov --no-report --test state_auth -- --include-ignored --test-threads=1
            cargo llvm-cov --no-report --test import_existing_live -- --include-ignored --test-threads=1
            cargo llvm-cov report --summary-only
            ;;
        patch)
            cargo llvm-cov --all-features --lcov --output-path head.lcov
            # --detach so the base worktree doesn't collide with any
            # existing checkout of the base branch (e.g. the main repo
            # already sitting on main).
            git worktree add --detach ../.kei-cov-base "{{BASE}}" >/dev/null
            (cd ../.kei-cov-base && cargo llvm-cov --all-features --lcov --output-path "$OLDPWD/base.lcov")
            git worktree remove ../.kei-cov-base >/dev/null
            python3 .github/scripts/patch_coverage.py \
                --lcov head.lcov \
                --base-lcov base.lcov \
                --base "{{BASE}}"
            rm -f head.lcov base.lcov
            ;;
        *)
            echo "Unknown mode: {{MODE}}" >&2
            echo "Modes: (none) | html | live | patch [BASE]" >&2
            exit 1
            ;;
    esac

# Run any kei subcommand under cargo run with .env + scratch data/photos dirs pre-applied.
dev CMD *ARGS:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ -f .env ]; then
        set -a; source .env; set +a
    fi
    export KEI_DATA_DIR="${KEI_DEV_DATA_DIR:-$HOME/.config/kei}"
    cfg="$(mktemp)"
    trap 'rm -f "$cfg"' EXIT
    printf 'data_dir = "%s"\n[download]\ndirectory = "%s"\n' "$KEI_DATA_DIR" "${KEI_DEV_PHOTOS_DIR:-/tmp/kei-dev-photos}" > "$cfg"
    cargo run -- {{CMD}} --config "$cfg" {{ARGS}}

# Docker: build | multiarch | run | shell | health.
docker MODE:
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{MODE}}" in
        build)
            docker build -t kei:dev .
            ;;
        multiarch)
            # The default `docker` driver can't build multiple platforms;
            # bootstrap (or reuse) a `docker-container` driver builder
            # named `kei-multiarch` for this invocation only.
            if ! docker buildx inspect kei-multiarch >/dev/null 2>&1; then
                docker buildx create --name kei-multiarch --driver docker-container >/dev/null
            fi
            docker buildx build --builder kei-multiarch \
                --platform linux/amd64,linux/arm64 \
                -t kei:dev .
            ;;
        run)
            docker compose up
            ;;
        shell)
            docker run --rm -it --entrypoint bash kei:dev
            ;;
        health)
            container=$(docker ps --filter ancestor=kei:dev --quiet | head -1)
            if [ -z "$container" ]; then
                echo "No running kei:dev container found." >&2
                exit 1
            fi
            docker exec "$container" cat /config/health.json
            ;;
        *)
            echo "Unknown mode: {{MODE}}" >&2
            echo "Modes: build | multiarch | run | shell | health" >&2
            exit 1
            ;;
    esac

# Reproduce release.yml's build + archive locally for TARGET (default host).
release TARGET="":
    #!/usr/bin/env bash
    set -euo pipefail
    target="{{TARGET}}"
    if [ -z "$target" ]; then
        target=$(rustc -vV | awk '/^host:/ {print $2}')
    fi
    case "$target" in
        aarch64-unknown-linux-gnu)
            export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
            export CXX_aarch64_unknown_linux_gnu=aarch64-linux-gnu-g++
            export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_CXX=aarch64-linux-gnu-g++
            export PKG_CONFIG_ALLOW_CROSS=1
            export PKG_CONFIG_PATH=/usr/lib/aarch64-linux-gnu/pkgconfig
            ;;
    esac
    cargo build --locked --release --target "$target"
    mkdir -p dist
    case "$target" in
        *-windows-*)
            archive="dist/kei-$target.zip"
            (cd "target/$target/release" && zip "../../../$archive" kei.exe)
            ;;
        *)
            archive="dist/kei-$target.tar.gz"
            tar -C "target/$target/release" -czf "$archive" kei
            ;;
    esac
    checksum_file="dist/SHA256SUMS.txt"
    archive_name=$(basename "$archive")
    checksum=$(cd dist && sha256sum "$archive_name")
    tmp=$(mktemp "dist/SHA256SUMS.XXXXXX")
    if [ -f "$checksum_file" ]; then
        awk -v name="$archive_name" '$2 != name { print }' "$checksum_file" > "$tmp"
    fi
    printf '%s\n' "$checksum" >> "$tmp"
    mv "$tmp" "$checksum_file"
    echo ""
    echo "Archive: $archive"
    echo "Checksum written to dist/SHA256SUMS.txt"
    echo ""
    version=$(awk -F'"' '/^version = "/ {print $2; exit}' Cargo.toml)
    echo "=== CHANGELOG [$version] ==="
    awk -v ver="$version" '
        /^## \[/ { in_section = ($0 ~ "^## \\[" ver "\\]"); next }
        in_section { print }
    ' CHANGELOG.md | sed '/./,$!d' | awk 'NR==1 && /^$/ {next} {print}'

# Fuzz: list | build | run TARGET [SECONDS] | all [SECONDS]. Requires nightly + cargo-fuzz; install with `rustup install nightly && cargo install cargo-fuzz`.
fuzz MODE="list" *ARGS="":
    #!/usr/bin/env bash
    set -euo pipefail
    if ! command -v cargo-fuzz >/dev/null 2>&1; then
        echo "cargo-fuzz not installed. Run: cargo install cargo-fuzz" >&2
        exit 1
    fi
    if ! rustup toolchain list | grep -q '^nightly'; then
        echo "nightly toolchain not installed. Run: rustup install nightly" >&2
        exit 1
    fi
    case "{{MODE}}" in
        list)
            cargo +nightly fuzz list
            ;;
        build)
            cargo +nightly fuzz build
            ;;
        all)
            args=({{ARGS}})
            seconds="${args[0]:-60}"
            mapfile -t targets < <(cargo +nightly fuzz list)
            if [ "${#targets[@]}" -eq 0 ]; then
                echo "no fuzz targets found" >&2
                exit 1
            fi
            for target in "${targets[@]}"; do
                echo ""
                echo "=== fuzz $target (${seconds}s) ==="
                mkdir -p "fuzz/corpus/$target"
                extra=()
                if [ -d "fuzz/seeds/$target" ]; then
                    extra+=("fuzz/seeds/$target")
                fi
                cargo +nightly fuzz run "$target" "fuzz/corpus/$target" "${extra[@]}" -- -max_total_time="$seconds"
            done
            ;;
        run)
            args=({{ARGS}})
            target="${args[0]:-}"
            seconds="${args[1]:-60}"
            if [ -z "$target" ]; then
                echo "usage: just fuzz run TARGET [SECONDS]" >&2
                cargo +nightly fuzz list
                exit 2
            fi
            # libfuzzer treats the FIRST corpus dir as input/output (it
            # writes new coverage-improving inputs there) and any later dirs
            # as read-only auxiliary corpora. Pass fuzz/corpus/<target>
            # first so libfuzzer's auto-saved finds land there, and
            # fuzz/seeds/<target> second so checked-in regression inputs
            # replay every run without getting clobbered by autogen entries.
            mkdir -p "fuzz/corpus/$target"
            extra=()
            if [ -d "fuzz/seeds/$target" ]; then
                extra+=("fuzz/seeds/$target")
            fi
            cargo +nightly fuzz run "$target" "fuzz/corpus/$target" "${extra[@]}" -- -max_total_time="$seconds"
            ;;
        *)
            echo "Unknown mode: {{MODE}}" >&2
            echo "Modes: list | build | run TARGET [SECONDS] | all [SECONDS]" >&2
            exit 1
            ;;
    esac

# Check local tools needed by gate/full-test/cov/fuzz/docker recipes.
doctor:
    #!/usr/bin/env bash
    set -euo pipefail
    status=0
    ok() { printf '[ok]   %s\n' "$1"; }
    fail() { printf '[miss] %s\n' "$1"; status=1; }
    note() { printf '[note] %s\n' "$1"; }
    have_cmd() {
        local cmd="$1"
        local label="${2:-$1}"
        local version=""
        if command -v "$cmd" >/dev/null 2>&1; then
            version=$("$cmd" --version 2>/dev/null | head -1 || true)
            ok "$label${version:+: $version}"
        else
            fail "$label not found"
        fi
    }
    have_cargo_subcommand() {
        local subcmd="$1"
        local label="${2:-cargo $subcmd}"
        local version=""
        if cargo "$subcmd" --version >/dev/null 2>&1; then
            version=$(cargo "$subcmd" --version 2>/dev/null | head -1 || true)
            ok "$label${version:+: $version}"
        else
            fail "$label not available"
        fi
    }
    have_cmd just
    have_cmd cargo
    have_cmd rustup
    if cargo +nightly --version >/dev/null 2>&1; then
        ok "nightly toolchain: $(cargo +nightly --version | head -1)"
    else
        fail "nightly toolchain not installed"
    fi
    have_cmd cargo-fuzz
    have_cargo_subcommand udeps "cargo-udeps"
    have_cargo_subcommand audit "cargo-audit"
    have_cargo_subcommand llvm-cov "cargo-llvm-cov"
    have_cmd typos
    have_cmd docker
    if docker buildx version >/dev/null 2>&1; then
        ok "docker buildx: $(docker buildx version | head -1)"
    else
        fail "docker buildx not available"
    fi
    if command -v systemd-analyze >/dev/null 2>&1 || command -v plutil >/dev/null 2>&1; then
        ok "service-smoke verifier available"
    else
        note "service-smoke verifier not found; service-smoke is Linux/macOS only"
    fi
    if [ -f .env ]; then
        ok ".env present for live recipes"
    else
        note ".env missing; live recipes need ICLOUD_USERNAME/ICLOUD_PASSWORD in environment"
    fi
    if [ -d .test-cookies ]; then
        ok ".test-cookies present"
    else
        note ".test-cookies missing; live auth may need a fresh login"
    fi
    exit "$status"

# Service-install smoke: builds release, dry-run install, validates artifact, uninstall, asserts clean.
# Mirrors .github/workflows/service-smoke.yml. Linux + macOS only.
service-smoke:
    #!/usr/bin/env bash
    set -euxo pipefail
    cargo build --release
    KEI="$PWD/target/release/kei"
    # `kei status` derives its state-DB path from the username; pin a
    # placeholder so the smoke does not depend on the operator's .env
    # or saved config.
    export ICLOUD_USERNAME="service-smoke@example.invalid"
    assert_service_not_installed() {
        local status
        status=$("$KEI" status)
        printf '%s\n' "$status" | grep -q '^Service: not installed'
    }
    case "$(uname -s)" in
        Linux)
            UNIT="$HOME/.config/systemd/user/kei.service"
            UNIT_PREVIEW="${RUNNER_TEMP:-${TMPDIR:-/tmp}}/kei.service"
            # Clean any leftover from a previous failed run so the
            # pre-state assertion is meaningful.
            rm -f "$UNIT" "$UNIT_PREVIEW"
            test ! -e "$UNIT"
            assert_service_not_installed
            "$KEI" install --user --dry-run > "$UNIT_PREVIEW"
            test ! -e "$UNIT"
            systemd-analyze --user verify "$UNIT_PREVIEW"
            "$KEI" uninstall
            test ! -e "$UNIT"
            assert_service_not_installed
            ;;
        Darwin)
            PLIST="$HOME/Library/LaunchAgents/com.rhoopr.kei.plist"
            PLIST_PREVIEW="${RUNNER_TEMP:-${TMPDIR:-/tmp}}/com.rhoopr.kei.plist"
            rm -f "$PLIST" "$PLIST_PREVIEW"
            test ! -e "$PLIST"
            assert_service_not_installed
            "$KEI" install --dry-run > "$PLIST_PREVIEW"
            test ! -e "$PLIST"
            plutil -lint "$PLIST_PREVIEW"
            "$KEI" uninstall
            test ! -e "$PLIST"
            assert_service_not_installed
            ;;
        *)
            echo "service-smoke is Linux/macOS only; Windows runs in CI" >&2
            exit 2
            ;;
    esac

# Disabled: not part of the active workflow.
# Create branch NAME off a freshly fetched origin/main (AGENTS.md branch-from-fresh-main rule).
# branch NAME:
#     #!/usr/bin/env bash
#     set -euo pipefail
#     git fetch origin main
#     if git show-ref --verify --quiet "refs/heads/{{NAME}}"; then
#         echo "branch '{{NAME}}' already exists locally" >&2
#         exit 1
#     fi
#     git switch -c "{{NAME}}" origin/main
