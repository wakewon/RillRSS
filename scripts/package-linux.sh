#!/usr/bin/env bash
# Package a Linux Docker release (normally built on a native GitHub runner).
#   scripts/package-linux.sh 0.1.4 arm64 image.tar
# Omit IMAGE_TAR to explicitly opt into a local Docker build.
set -Eeuo pipefail
umask 022

usage() {
    printf 'Usage: %s VERSION {amd64|arm64} [IMAGE_TAR] [OUTPUT_DIR]\n' "$0" >&2
}

if (($# < 2 || $# > 4)); then
    usage
    exit 2
fi

VERSION=$1
ARCH=$2
IMAGE_TAR=${3:-}
SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
OUTPUT_DIR=${4:-"$REPO_ROOT/dist"}
TAG="rillrss:${VERSION}-${ARCH}"

fail() {
    printf '%s\n' "$*" >&2
    exit 1
}

[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+([.-][[:alnum:]][[:alnum:].-]*)?$ ]] || fail "Invalid release version: $VERSION"
case "$ARCH" in amd64|arm64) ;; *) fail "Unsupported architecture: $ARCH" ;; esac
cargo_version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$REPO_ROOT/Cargo.toml" | head -n 1)
[[ "$cargo_version" == "$VERSION" ]] || fail "Cargo.toml says $cargo_version, not $VERSION"
for file in compose.yml .env.example nginx-rillrss.conf DEPLOY.zh-CN.md install.sh update.sh; do
    [[ -f "$REPO_ROOT/deploy/$file" ]] || fail "Missing deployment file: deploy/$file"
done
command -v sha256sum >/dev/null || fail "sha256sum is required."
command -v tar >/dev/null || fail "tar is required."
command -v python3 >/dev/null || fail "python3 is required to validate the image archive."

mkdir -p "$OUTPUT_DIR"
OUTPUT_DIR=$(cd "$OUTPUT_DIR" && pwd)
PACKAGE_NAME="rillrss-${VERSION}-linux-${ARCH}"
PACKAGE_DIR="$OUTPUT_DIR/$PACKAGE_NAME"
ARCHIVE="$OUTPUT_DIR/$PACKAGE_NAME.tar.gz"
CHECKSUM="$ARCHIVE.sha256"
STAGE=$(mktemp -d "$OUTPUT_DIR/.${PACKAGE_NAME}.XXXXXX")
trap 'rm -rf "$STAGE"' EXIT
STAGED_PACKAGE="$STAGE/$PACKAGE_NAME"
mkdir -p "$STAGED_PACKAGE"

if [[ -z "$IMAGE_TAR" ]]; then
    docker buildx build --platform "linux/$ARCH" -t "$TAG" --load "$REPO_ROOT"
    docker save -o "$STAGED_PACKAGE/image.tar" "$TAG"
else
    [[ -f "$IMAGE_TAR" ]] || fail "Image archive not found: $IMAGE_TAR"
    cp "$IMAGE_TAR" "$STAGED_PACKAGE/image.tar"
fi
python3 - "$STAGED_PACKAGE/image.tar" "$TAG" "$ARCH" <<'PY'
import json, sys, tarfile

archive, tag, arch = sys.argv[1:]
with tarfile.open(archive) as image:
    manifest = json.load(image.extractfile('manifest.json'))
    entries = [entry for entry in manifest if tag in (entry.get('RepoTags') or [])]
    if len(entries) != 1:
        sys.exit(f'Image archive must contain exactly one image with tag {tag}')
    config = json.load(image.extractfile(entries[0]['Config']))
    if config.get('architecture') != arch or config.get('os') != 'linux':
        sys.exit(f'Image archive is not linux/{arch}')
PY

cp "$REPO_ROOT/deploy/.env.example" "$STAGED_PACKAGE/.env.example"
cp "$REPO_ROOT/deploy/nginx-rillrss.conf" "$STAGED_PACKAGE/nginx-rillrss.conf"
sed "s/^VERSION=.*/VERSION=${VERSION}/" "$REPO_ROOT/deploy/DEPLOY.zh-CN.md" > "$STAGED_PACKAGE/DEPLOY.zh-CN.md"
cp "$REPO_ROOT/deploy/install.sh" "$STAGED_PACKAGE/install.sh"
cp "$REPO_ROOT/deploy/update.sh" "$STAGED_PACKAGE/update.sh"
sed -E -e "s|^    image: rillrss:[^[:space:]]+|    image: ${TAG}|" \
    -e "s|^    platform: .*|    platform: linux/${ARCH}|" \
    "$REPO_ROOT/deploy/compose.yml" > "$STAGED_PACKAGE/compose.yml"
grep -Fq "image: $TAG" "$STAGED_PACKAGE/compose.yml" || fail "compose.yml has no rillrss image line to update."
printf '%s\n' "$VERSION" > "$STAGED_PACKAGE/VERSION"
printf '%s\n' "$ARCH" > "$STAGED_PACKAGE/ARCH"
chmod 755 "$STAGED_PACKAGE" "$STAGED_PACKAGE/install.sh" "$STAGED_PACKAGE/update.sh"
chmod 644 "$STAGED_PACKAGE"/{image.tar,compose.yml,.env.example,nginx-rillrss.conf,DEPLOY.zh-CN.md,VERSION,ARCH}

(cd "$STAGED_PACKAGE" && sha256sum image.tar compose.yml .env.example nginx-rillrss.conf DEPLOY.zh-CN.md VERSION ARCH install.sh update.sh > SHA256SUMS)
chmod 644 "$STAGED_PACKAGE/SHA256SUMS"

# Plain archive: no macOS ._* files or xattr headers, files owned by root.
if tar --version 2>/dev/null | grep -q bsdtar; then
    COPYFILE_DISABLE=1 tar --no-xattrs --no-mac-metadata --uid 0 --gid 0 --uname root --gname root \
        -czf "$STAGE/$PACKAGE_NAME.tar.gz" -C "$STAGE" "$PACKAGE_NAME"
else
    tar --no-xattrs --owner=0 --group=0 --numeric-owner \
        -czf "$STAGE/$PACKAGE_NAME.tar.gz" -C "$STAGE" "$PACKAGE_NAME"
fi
(cd "$STAGE" && sha256sum "$PACKAGE_NAME.tar.gz" > "$PACKAGE_NAME.tar.gz.sha256")

rm -rf "$PACKAGE_DIR"
mv "$STAGED_PACKAGE" "$PACKAGE_DIR"
mv "$STAGE/$PACKAGE_NAME.tar.gz" "$ARCHIVE"
mv "$STAGE/$PACKAGE_NAME.tar.gz.sha256" "$CHECKSUM"
printf 'Created %s\nCreated %s\n' "$ARCHIVE" "$CHECKSUM"
