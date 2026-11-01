#!/usr/bin/env bash
# Install or upgrade a RillRSS release in a fixed directory (default /opt/rillrss).
# The directory keeps .env, data/ and an optional compose.override.yml across
# releases; a release only replaces compose.yml, the image and the helper files.
set -Eeuo pipefail
umask 077

INSTALL_DIR=/opt/rillrss
MIGRATE_FROM=
KEEP_BACKUPS=3
HEALTH_TIMEOUT=300

usage() {
    cat <<'EOF'
Usage:
  sudo ./install.sh [--install-dir PATH] [--migrate-from OLD_DIR]

Fresh install (default directory /opt/rillrss): copies the release files and
creates .env from .env.example, then stops so you can edit .env.

Upgrade (the directory already has .env): backs up the database while the old
container still runs, loads the new image, replaces compose.yml (the old one is
kept as compose.yml.previous), restarts and waits for the health check.

--migrate-from OLD_DIR: one-time move from a deployment that was started inside
an unpacked release directory. The new image is loaded and the configuration
checked first; then the old deployment is stopped, its .env and data/ are
copied (OLD_DIR itself is left untouched for rollback) and the service is
started from the install directory.
EOF
}

fail() {
    printf 'Error: %s\n' "$*" >&2
    exit 1
}

info() {
    printf '==> %s\n' "$*"
}

# Printed if the script fails after it has started changing the deployment.
ROLLBACK_HINT=
on_exit() {
    local rc=$?
    if ((rc != 0)) && [[ -n $ROLLBACK_HINT ]]; then
        printf '\n%s\n' "$ROLLBACK_HINT" >&2
    fi
}
trap on_exit EXIT

while (($#)); do
    case "$1" in
        --install-dir)
            (($# >= 2)) || fail "--install-dir needs a path"
            INSTALL_DIR=$2
            shift 2
            ;;
        --migrate-from)
            (($# >= 2)) || fail "--migrate-from needs a path"
            MIGRATE_FROM=$2
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            usage >&2
            fail "unknown argument: $1"
            ;;
    esac
done

[[ ${EUID:-$(id -u)} -eq 0 ]] || fail "run this script with sudo or as root"
[[ $(uname -s) == Linux ]] || fail "this release requires a Linux server"
command -v docker >/dev/null || fail "Docker is not installed"
command -v sha256sum >/dev/null || fail "sha256sum is required"
docker compose version >/dev/null 2>&1 || fail "Docker Compose v2 is required"

PACKAGE_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
for file in image.tar compose.yml .env.example nginx-rillrss.conf DEPLOY.zh-CN.md VERSION SHA256SUMS update.sh; do
    [[ -f "$PACKAGE_DIR/$file" ]] || fail "package file is missing: $file"
done
(cd "$PACKAGE_DIR" && sha256sum -c SHA256SUMS >/dev/null) || fail "package checksum mismatch; download the release again"
NEW_VERSION=$(<"$PACKAGE_DIR/VERSION")
# Packages made before multi-architecture releases did not have an ARCH file.
PACKAGE_ARCH=amd64
if [[ -f "$PACKAGE_DIR/ARCH" ]]; then PACKAGE_ARCH=$(<"$PACKAGE_DIR/ARCH"); fi
case "$(uname -m)" in
    x86_64|amd64) HOST_ARCH=amd64 ;;
    aarch64|arm64) HOST_ARCH=arm64 ;;
    *) fail "unsupported server architecture: $(uname -m)" ;;
esac
[[ $PACKAGE_ARCH == "$HOST_ARCH" ]] || fail "this package is for $PACKAGE_ARCH, but this server is $HOST_ARCH"

mkdir -p "$INSTALL_DIR"
INSTALL_DIR=$(cd "$INSTALL_DIR" && pwd)

# Run Compose from the install directory so compose.override.yml is honoured.
compose() {
    (cd "$INSTALL_DIR" && docker compose "$@")
}

# Id of the running rillrss container of the Compose project in $1, or nothing.
running_container() {
    local id
    id=$(cd "$1" && docker compose ps -q rillrss 2>/dev/null) || return 0
    if [[ -n $id && $(docker inspect -f '{{.State.Running}}' "$id" 2>/dev/null) == true ]]; then
        printf '%s' "$id"
    fi
}

# Version from an architecture-specific image tag.
container_version() {
    local image
    image=$(docker inspect -f '{{.Config.Image}}' "$1" 2>/dev/null) || image=
    image=${image##*:}
    image=${image%-amd64}
    printf '%s' "${image%-arm64}"
}

load_image() {
    info "Loading image rillrss:$NEW_VERSION-$PACKAGE_ARCH"
    docker load -i "$PACKAGE_DIR/image.tar" >/dev/null
}

install_release_files() {
    if [[ -f "$INSTALL_DIR/compose.yml" ]]; then
        cp -p "$INSTALL_DIR/compose.yml" "$INSTALL_DIR/compose.yml.previous"
    fi
    install -m 644 "$PACKAGE_DIR/compose.yml" "$INSTALL_DIR/compose.yml"
    install -m 644 "$PACKAGE_DIR/.env.example" "$INSTALL_DIR/.env.example"
    install -m 644 "$PACKAGE_DIR/nginx-rillrss.conf" "$INSTALL_DIR/nginx-rillrss.conf.example"
    install -m 644 "$PACKAGE_DIR/DEPLOY.zh-CN.md" "$INSTALL_DIR/DEPLOY.zh-CN.md"
    install -m 644 "$PACKAGE_DIR/VERSION" "$INSTALL_DIR/VERSION"
    if [[ -f "$PACKAGE_DIR/ARCH" ]]; then
        install -m 644 "$PACKAGE_DIR/ARCH" "$INSTALL_DIR/ARCH"
    fi
    install -m 755 "$PACKAGE_DIR/update.sh" "$INSTALL_DIR/update.sh"
}

start_and_wait() {
    compose config --quiet
    info "Starting RillRSS $NEW_VERSION from $INSTALL_DIR"
    compose up -d
    local deadline=$((SECONDS + HEALTH_TIMEOUT))
    until compose exec -T rillrss rillrss healthcheck >/dev/null 2>&1; do
        if ((SECONDS >= deadline)); then
            compose ps >&2 || true
            printf '\nRillRSS did not become healthy within %ss. Recent logs:\n' "$HEALTH_TIMEOUT" >&2
            compose logs --tail=60 rillrss >&2 || true
            exit 1
        fi
        sleep 2
    done
    compose ps
}

# ---------------------------------------------------------------------------
# One-time migration from a deployment inside an unpacked release directory.
# ---------------------------------------------------------------------------
if [[ -n $MIGRATE_FROM ]]; then
    [[ -d $MIGRATE_FROM ]] || fail "old deployment directory does not exist: $MIGRATE_FROM"
    MIGRATE_FROM=$(cd "$MIGRATE_FROM" && pwd)
    [[ $MIGRATE_FROM != "$INSTALL_DIR" ]] || fail "--migrate-from and --install-dir must differ"
    [[ -f $MIGRATE_FROM/compose.yml ]] || fail "no compose.yml in $MIGRATE_FROM"
    [[ -f $MIGRATE_FROM/.env ]] || fail "no .env in $MIGRATE_FROM"
    [[ -d $MIGRATE_FROM/data ]] || fail "no data/ directory in $MIGRATE_FROM"
    [[ ! -e $INSTALL_DIR/.env ]] || fail "$INSTALL_DIR/.env already exists; refusing to overwrite it"
    [[ ! -e $INSTALL_DIR/data ]] || fail "$INSTALL_DIR/data already exists; refusing to overwrite it"

    old_id=$(running_container "$MIGRATE_FROM")
    old_port=
    if [[ -n $old_id ]]; then
        # Copy the directory the running container really uses.
        old_src=$(docker inspect -f '{{range .Mounts}}{{if eq .Destination "/data"}}{{.Source}}{{end}}{{end}}' "$old_id")
        [[ -n $old_src && $(realpath "$old_src") == $(realpath "$MIGRATE_FROM/data") ]] \
            || fail "the running container keeps /data in '${old_src:-a volume}', not $MIGRATE_FROM/data; nothing was changed"
        old_port=$(docker port "$old_id" 8080/tcp 2>/dev/null | head -n 1) || old_port=
    fi

    need_kb=$(du -sk "$MIGRATE_FROM/data" | cut -f1)
    avail_kb=$(df -Pk "$INSTALL_DIR" | awk 'NR == 2 { print $4 }')
    ((avail_kb > need_kb + 204800)) \
        || fail "not enough free space in $INSTALL_DIR: data/ needs $((need_kb / 1024)) MB plus 200 MB headroom"

    # Everything that can fail without downtime happens before the old service stops.
    load_image
    docker compose --project-directory "$PACKAGE_DIR" --env-file "$MIGRATE_FROM/.env" \
        -f "$PACKAGE_DIR/compose.yml" config --quiet \
        || fail "the old .env does not work with the new compose.yml; nothing was changed"

    ROLLBACK_HINT="Migration did not finish; $MIGRATE_FROM is unchanged. To go back to it:
  cd $INSTALL_DIR && docker compose down; cd $MIGRATE_FROM && docker compose up -d
To retry the migration afterwards, first remove $INSTALL_DIR/.env and $INSTALL_DIR/data."
    info "Stopping the old deployment in $MIGRATE_FROM"
    (cd "$MIGRATE_FROM" && docker compose down)

    info "Copying .env and data/ ($((need_kb / 1024)) MB) to $INSTALL_DIR"
    install -m 600 "$MIGRATE_FROM/.env" "$INSTALL_DIR/.env"
    if [[ -f $MIGRATE_FROM/compose.override.yml && ! -e $INSTALL_DIR/compose.override.yml ]]; then
        install -m 644 "$MIGRATE_FROM/compose.override.yml" "$INSTALL_DIR/compose.override.yml"
    fi
    cp -a "$MIGRATE_FROM/data" "$INSTALL_DIR/data"

    install_release_files
    start_and_wait
    ROLLBACK_HINT=
    new_port=$(compose port rillrss 8080 2>/dev/null | head -n 1) || new_port=
    if [[ -n $old_port && $new_port != "$old_port" ]]; then
        printf '\nWarning: the old container listened on %s, the new one on %s.\n' "$old_port" "${new_port:-nothing}" >&2
        printf 'Set RILL_PORT in %s/.env (or point Nginx at the new port).\n' "$INSTALL_DIR" >&2
    fi
    cat <<EOF

RillRSS $NEW_VERSION is running from $INSTALL_DIR and is healthy.
$MIGRATE_FROM was not changed; delete it once everything checks out.
To go back instead:
  cd $INSTALL_DIR && docker compose down; cd $MIGRATE_FROM && docker compose up -d
EOF
    exit 0
fi

# ---------------------------------------------------------------------------
# Fresh install: copy files and stop so the operator can edit .env.
# ---------------------------------------------------------------------------
if [[ ! -f $INSTALL_DIR/.env ]]; then
    install -m 600 "$PACKAGE_DIR/.env.example" "$INSTALL_DIR/.env"
    mkdir -p "$INSTALL_DIR/data"
    install_release_files
    load_image
    cat <<EOF

Files are installed in $INSTALL_DIR. The service was not started: edit
  $INSTALL_DIR/.env
and set at least RILL_PASSWORD and RILL_BASE_URL (and RILL_PORT if 8080 is
taken). Then start it with:
  cd $INSTALL_DIR && docker compose up -d

Later releases install with:
  sudo $INSTALL_DIR/update.sh /path/to/rillrss-VERSION-linux-amd64.tar.gz
EOF
    exit 0
fi

# ---------------------------------------------------------------------------
# Upgrade in place.
# ---------------------------------------------------------------------------
mkdir -p "$INSTALL_DIR/data"
cur_id=$(running_container "$INSTALL_DIR")
OLD_VERSION=unknown
backup_note="No database backup was made because the service was not running."
if [[ -n $cur_id ]]; then
    OLD_VERSION=$(container_version "$cur_id")
    # Online backup with the old binary, before anything changes.
    backup="rillrss-${OLD_VERSION:-unknown}-$(date +%Y%m%d-%H%M%S).db"
    info "Backing up the database to data/backups/$backup"
    # shellcheck disable=SC2016 # $PUID/$PGID expand inside the container
    if ! compose exec -T rillrss sh -c 'mkdir -p /data/backups && chown "$PUID:$PGID" /data/backups' \
        || ! compose exec -T rillrss entrypoint.sh backup "/data/backups/$backup" >/dev/null; then
        fail "database backup failed; nothing was changed"
    fi
    backup_note="The database was backed up to $INSTALL_DIR/data/backups/$backup."
    # Keep only the newest few pre-upgrade backups (names contain no spaces).
    # shellcheck disable=SC2012
    ls -1t "$INSTALL_DIR"/data/backups/rillrss-*.db 2>/dev/null \
        | tail -n +$((KEEP_BACKUPS + 1)) | xargs -r rm -f -- || true
fi

load_image
install_release_files
ROLLBACK_HINT="Upgrade to $NEW_VERSION did not finish. $backup_note
To run the previous release again:
  cd $INSTALL_DIR && cp compose.yml.previous compose.yml && docker compose up -d"
start_and_wait
ROLLBACK_HINT=
printf '\nRillRSS %s is running and healthy (previously %s). %s\n' "$NEW_VERSION" "$OLD_VERSION" "$backup_note"
