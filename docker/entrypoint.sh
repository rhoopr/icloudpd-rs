#!/bin/sh
# kei container entrypoint.
#
# When PUID and PGID are set, drops to that UID:GID via gosu after
# fixing ownership of /config and /photos. Without them, runs as root.
#
# `kei` subcommand list is hard-coded to avoid colliding with debian
# binaries: /usr/bin/{sync,login,reset} would otherwise hijack the
# default CMD. Update when adding a kei subcommand.

set -e

if [ "$#" -eq 0 ]; then
    set -- kei
elif [ "${1#-}" != "$1" ]; then
    set -- kei "$@"
else
    case "$1" in
        sync|login|list|password|reset|config|status|doctor|manifest|verify|reconcile|import-existing|install|uninstall|service|help)
            set -- kei "$@"
            ;;
        *)
            if ! command -v "$1" >/dev/null 2>&1; then
                set -- kei "$@"
            fi
            ;;
    esac
fi

collect_removed_sync_env_names() {
    found=""
    for name in \
        KEI_DOWNLOAD_DIR \
        KEI_DIRECTORY \
        KEI_FOLDER_STRUCTURE \
        KEI_FOLDER_STRUCTURE_ALBUMS \
        KEI_FOLDER_STRUCTURE_SMART_FOLDERS \
        KEI_ALBUM \
        KEI_EXCLUDE_ALBUM \
        KEI_LIBRARY \
        KEI_SKIP_VIDEOS \
        KEI_SKIP_PHOTOS \
        KEI_THREADS \
        KEI_THREADS_NUM \
        KEI_BANDWIDTH_LIMIT \
        KEI_TEMP_SUFFIX \
        KEI_MAX_RETRIES \
        KEI_MAX_DOWNLOAD_ATTEMPTS \
        KEI_WATCH_WITH_INTERVAL \
        KEI_NOTIFY_SYSTEMD \
        KEI_PID_FILE \
        KEI_RECONCILE_EVERY_N_CYCLES \
        KEI_NOTIFICATION_SCRIPT \
        KEI_REPORT_JSON \
        KEI_METRICS_PORT
    do
        if env | grep -q "^${name}="; then
            if [ -n "$found" ]; then
                found="$found, $name"
            else
                found="$name"
            fi
        fi
    done
    printf '%s' "$found"
}

has_help_or_version_flag() {
    for arg in "$@"; do
        case "$arg" in
            -h|--help|-V|--version)
                return 0
                ;;
        esac
    done
    return 1
}

is_sync_like_command() {
    if has_help_or_version_flag "$@"; then
        return 1
    fi

    if [ "${1:-}" = "kei" ]; then
        shift
    fi

    if [ "$#" -eq 0 ]; then
        return 0
    fi

    case "${1:-}" in
        sync|import-existing)
            return 0
            ;;
        service)
            [ "${2:-}" = "run" ]
            return
            ;;
        *)
            return 1
            ;;
    esac
}

has_explicit_nondefault_config() {
    if [ "${1:-}" = "kei" ]; then
        shift
    fi

    while [ "$#" -gt 0 ]; do
        case "$1" in
            --config)
                cfg="${2:-}"
                [ "$cfg" != "/config/config.toml" ]
                return
                ;;
            --config=*)
                cfg="${1#--config=}"
                [ "$cfg" != "/config/config.toml" ]
                return
                ;;
        esac
        shift
    done

    return 1
}

docker_upgrade_preflight() {
    [ -f /config/config.toml ] && return 0
    is_sync_like_command "$@" || return 0
    has_explicit_nondefault_config "$@" && return 0

    removed="$(collect_removed_sync_env_names)"
    [ -n "$removed" ] || return 0

    echo "kei: /config/config.toml is required for v0.20 Docker sync settings." >&2
    echo "kei: found removed env config: $removed" >&2
    echo "kei: move durable settings into /config/config.toml; keep env for secrets and runtime glue." >&2
    echo "kei: see https://github.com/rhoopr/kei/blob/main/docs/v0.20-migration.md" >&2
    exit 1
}

docker_upgrade_preflight "$@"

if [ -z "${PUID:-}" ] && [ -z "${PGID:-}" ]; then
    exec "$@"
fi

if [ -z "${PUID:-}" ] || [ -z "${PGID:-}" ]; then
    echo "kei: PUID and PGID must be set together (got PUID='${PUID:-}' PGID='${PGID:-}')" >&2
    exit 1
fi

case "$PUID$PGID" in
    *[!0-9]*)
        echo "kei: PUID/PGID must be numeric (got PUID=$PUID PGID=$PGID)" >&2
        exit 1
        ;;
esac

# Touch only mismatched inodes. A blind `chown -R` on a multi-TB
# Synology library would take hours; `find -not -uid` is O(stragglers)
# and a no-op on steady-state restarts. Read-only mounts produce a
# warning but don't fail; the user may have mounted them deliberately.
for d in /config /photos; do
    [ -d "$d" ] || continue
    find "$d" \! -uid "$PUID" -print0 2>/dev/null \
        | xargs -0 -r chown "$PUID:$PGID" 2>/dev/null \
        || echo "kei: warning: chown $d failed (read-only mount?)" >&2
done

# gosu accepts numeric uid:gid and runs without an /etc/passwd entry.
exec gosu "$PUID:$PGID" "$@"
