#!/bin/sh
# Run RillRSS as an unprivileged user (PUID/PGID, default 1000). When started as root,
# make sure the data directory belongs to that user first (bind mounts created by
# Docker are owned by root).
set -e
PUID="${PUID:-1000}"
PGID="${PGID:-1000}"
DATA="${RILL_DATA_DIR:-/data}"

if [ "$(id -u)" = "0" ]; then
    mkdir -p "$DATA"
    if [ "$(stat -c %u "$DATA")" != "$PUID" ] || [ "$(stat -c %g "$DATA")" != "$PGID" ]; then
        chown -R "$PUID:$PGID" "$DATA"
    fi
    if [ "$PUID" != "0" ]; then
        exec setpriv --reuid="$PUID" --regid="$PGID" --clear-groups \
            env HOME="$DATA" /usr/local/bin/rillrss "$@"
    fi
fi
exec /usr/local/bin/rillrss "$@"
