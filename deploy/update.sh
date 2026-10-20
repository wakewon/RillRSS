#!/usr/bin/env bash
# Upgrade the RillRSS installation this script lives in to a downloaded release:
#   sudo /opt/rillrss/update.sh /path/to/rillrss-VERSION-linux-amd64.tar.gz
# The release's own install.sh does the work (backup, image load, restart).
set -Eeuo pipefail
umask 077

fail() {
    printf 'Error: %s\n' "$*" >&2
    exit 1
}

if (($# != 1)) || [[ ! -f $1 ]]; then
    printf 'Usage: sudo %s /path/to/rillrss-VERSION-linux-amd64.tar.gz\n' "$0" >&2
    exit 2
fi
[[ ${EUID:-$(id -u)} -eq 0 ]] || fail "run this script with sudo or as root"
command -v tar >/dev/null || fail "tar is required"
command -v sha256sum >/dev/null || fail "sha256sum is required"

INSTALL_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
ARCHIVE_DIR=$(cd -- "$(dirname -- "$1")" && pwd)
ARCHIVE_NAME=$(basename -- "$1")

# Verify the download when its .sha256 file sits next to it.
if [[ -f "$ARCHIVE_DIR/$ARCHIVE_NAME.sha256" ]]; then
    (cd "$ARCHIVE_DIR" && sha256sum -c "$ARCHIVE_NAME.sha256" >/dev/null) \
        || fail "$ARCHIVE_NAME does not match $ARCHIVE_NAME.sha256; upload it again"
fi

STAGE_DIR=$(mktemp -d)
trap 'rm -rf "$STAGE_DIR"' EXIT
tar -xzf "$ARCHIVE_DIR/$ARCHIVE_NAME" -C "$STAGE_DIR"
mapfile -t INSTALLERS < <(find "$STAGE_DIR" -mindepth 2 -maxdepth 2 -type f -name install.sh)
((${#INSTALLERS[@]} == 1)) || fail "expected exactly one install.sh in $ARCHIVE_NAME"

"${INSTALLERS[0]}" --install-dir "$INSTALL_DIR"
