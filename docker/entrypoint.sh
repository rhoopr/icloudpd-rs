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
        sync|login|list|password|reset|config|status|verify|import-existing|reconcile|retry-failed)
            set -- kei "$@"
            ;;
        *)
            if ! command -v "$1" >/dev/null 2>&1; then
                set -- kei "$@"
            fi
            ;;
    esac
fi

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
