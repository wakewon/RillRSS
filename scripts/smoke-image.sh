#!/usr/bin/env bash
# Run on a native GitHub runner; never uses application data from the checkout.
set -Eeuo pipefail

IMAGE=${1:?Usage: smoke-image.sh IMAGE VERSION}
VERSION=${2:?Usage: smoke-image.sh IMAGE VERSION}
[[ $(docker run --rm "$IMAGE" --version) == "rillrss $VERSION" ]]
container=$(docker run -d -p 127.0.0.1::8080 -e RILL_PASSWORD=release-smoke-password "$IMAGE")
cleanup() {
    local status=$?
    if ((status != 0)); then docker logs "$container" >&2 || true; fi
    docker rm -fv "$container" >/dev/null
}
trap cleanup EXIT

healthy=false
for ((attempt = 0; attempt < 60; attempt++)); do
    if docker exec "$container" rillrss healthcheck; then
        healthy=true
        break
    fi
    sleep 2
done
[[ $healthy == true ]]
port=$(docker port "$container" 8080/tcp)
curl --fail --silent --show-error "http://$port/healthz"
docker exec "$container" entrypoint.sh check-db | grep -x ok
docker exec "$container" entrypoint.sh backup /data/ci-backup.db
docker exec "$container" test -s /data/ci-backup.db
