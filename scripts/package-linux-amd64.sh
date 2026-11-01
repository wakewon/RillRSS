#!/usr/bin/env bash
# Backwards-compatible entry point; releases normally build in GitHub Actions.
set -Eeuo pipefail
if (($# < 1 || $# > 3)); then
    printf 'Usage: %s VERSION [IMAGE_TAR] [OUTPUT_DIR]\n' "$0" >&2
    exit 2
fi
SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
exec "$SCRIPT_DIR/package-linux.sh" "$1" amd64 "${2:-}" "${3:-$SCRIPT_DIR/../dist}"
