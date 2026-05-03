#!/bin/sh
# kei container entrypoint.
#
# When PUID and/or PGID are set, drop privileges to that UID:GID before
# exec'ing the command. The volumes /config and /photos are chown'd to
# that UID on first use so kei can write to them and the host user owns
# the resulting files.
#
# When neither is set, runs as root (preserves prior default).
#
# Args dispatch (matches the convention used by postgres/mysql/redis
# official images):
#   - First arg starts with `-`           -> treat as kei flag, prepend `kei`
#   - First arg isn't an executable on $PATH -> treat as kei subcommand, prepend `kei`
#   - First arg IS an executable          -> run it directly (e.g. `sh`, `id`)
# So `docker run kei sync` runs kei sync, and `docker run kei sh` opens
# a shell for debugging without --entrypoint.
#
# Typical use case: NAS deployments (Synology Container Manager, Unraid,
# TrueNAS Scale) where files written into mounted volumes need to belong
# to the user that owns the host directory, otherwise downstream
# indexers (e.g. Synology Photos) can't read them.

set -e

# Dispatch: prepend `kei` for kei flags and known kei subcommands (some
# of which collide with system binaries: `sync`, `login`, `reset` are
# all in /usr/bin on debian-slim). Otherwise treat the first arg as a
# binary and exec it directly so `docker run kei sh` and `docker run
# kei id` work for debugging.
if [ "$#" -eq 0 ]; then
    set -- kei
elif [ "$1" = "kei" ]; then
    : # already kei + args
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

if [ -n "${PUID:-}" ] || [ -n "${PGID:-}" ]; then
    PUID="${PUID:-1000}"
    PGID="${PGID:-1000}"

    case "$PUID$PGID" in
        *[!0-9]*)
            echo "kei: PUID/PGID must be numeric (got PUID=$PUID PGID=$PGID)" >&2
            exit 1
            ;;
    esac

    if ! getent group "$PGID" >/dev/null 2>&1; then
        groupadd -g "$PGID" kei
    fi

    if ! getent passwd "$PUID" >/dev/null 2>&1; then
        useradd -u "$PUID" -g "$PGID" -M -d /config -s /bin/sh kei
    fi

    # Recursive chown only when the top-level dir's UID doesn't already
    # match. Skips the cost on subsequent restarts and avoids touching a
    # large /photos volume every time. A read-only mount produces a
    # warning but doesn't fail the container; the user may have mounted
    # /config or /photos read-only deliberately.
    for d in /config /photos; do
        if [ ! -d "$d" ]; then
            continue
        fi
        current_uid="$(stat -c '%u' "$d" 2>/dev/null || echo "")"
        if [ "$current_uid" != "$PUID" ]; then
            chown -R "$PUID:$PGID" "$d" 2>/dev/null \
                || echo "kei: warning: chown $d failed (read-only mount?)" >&2
        fi
    done

    exec gosu "$PUID:$PGID" "$@"
fi

exec "$@"
