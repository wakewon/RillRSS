#!/usr/bin/env bash
# Build the x86-64 server release: dist/rillrss-VERSION-linux-amd64.tar.gz + .sha256.
#   scripts/package-linux-amd64.sh 0.1.2              # builds and saves the image
#   scripts/package-linux-amd64.sh 0.1.2 image.tar    # uses an existing docker save
set -Eeuo pipefail
umask 022

usage() {
    printf 'Usage: %s VERSION [IMAGE_TAR] [OUTPUT_DIR]\n' "$0" >&2
}

if (($# < 1 || $# > 3)); then
    usage
    exit 2
fi

VERSION=$1
IMAGE_TAR=${2:-}
SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
OUTPUT_DIR=${3:-"$REPO_ROOT/dist"}
TAG="rillrss:${VERSION}-amd64"

fail() {
    printf '%s\n' "$*" >&2
    exit 1
}

[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+([.-][[:alnum:]][[:alnum:].-]*)?$ ]] || fail "Invalid release version: $VERSION"
cargo_version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$REPO_ROOT/Cargo.toml" | head -n 1)
[[ "$cargo_version" == "$VERSION" ]] || fail "Cargo.toml says $cargo_version, not $VERSION"
for file in compose.yml .env.example nginx-rillrss.conf DEPLOY.zh-CN.md install.sh update.sh; do
    [[ -f "$REPO_ROOT/deploy/$file" ]] || fail "Missing deployment file: deploy/$file"
done
command -v sha256sum >/dev/null || fail "sha256sum is required."
command -v tar >/dev/null || fail "tar is required."

mkdir -p "$OUTPUT_DIR"
OUTPUT_DIR=$(cd "$OUTPUT_DIR" && pwd)
PACKAGE_NAME="rillrss-${VERSION}-linux-amd64"
PACKAGE_DIR="$OUTPUT_DIR/$PACKAGE_NAME"
ARCHIVE="$OUTPUT_DIR/$PACKAGE_NAME.tar.gz"
CHECKSUM="$ARCHIVE.sha256"
STAGE=$(mktemp -d "$OUTPUT_DIR/.${PACKAGE_NAME}.XXXXXX")
trap 'rm -rf "$STAGE"' EXIT
STAGED_PACKAGE="$STAGE/$PACKAGE_NAME"
mkdir -p "$STAGED_PACKAGE"

if [[ -z "$IMAGE_TAR" ]]; then
    docker buildx build --platform linux/amd64 -t "$TAG" --load "$REPO_ROOT"
    docker save --platform linux/amd64 -o "$STAGED_PACKAGE/image.tar" "$TAG"
else
    [[ -f "$IMAGE_TAR" ]] || fail "Image archive not found: $IMAGE_TAR"
    cp "$IMAGE_TAR" "$STAGED_PACKAGE/image.tar"
fi
tar -xOf "$STAGED_PACKAGE/image.tar" manifest.json | grep -Fq "\"$TAG\"" \
    || fail "Image archive does not contain tag $TAG."

cp "$REPO_ROOT/deploy/.env.example" "$STAGED_PACKAGE/.env.example"
cp "$REPO_ROOT/deploy/nginx-rillrss.conf" "$STAGED_PACKAGE/nginx-rillrss.conf"
sed "s/^VERSION=.*/VERSION=${VERSION}/" "$REPO_ROOT/deploy/DEPLOY.zh-CN.md" > "$STAGED_PACKAGE/DEPLOY.zh-CN.md"
cp "$REPO_ROOT/deploy/install.sh" "$STAGED_PACKAGE/install.sh"
cp "$REPO_ROOT/deploy/update.sh" "$STAGED_PACKAGE/update.sh"
sed -E "s|^    image: rillrss:[^[:space:]]+|    image: ${TAG}|" \
    "$REPO_ROOT/deploy/compose.yml" > "$STAGED_PACKAGE/compose.yml"
grep -Fq "image: $TAG" "$STAGED_PACKAGE/compose.yml" || fail "compose.yml has no rillrss image line to update."
printf '%s\n' "$VERSION" > "$STAGED_PACKAGE/VERSION"
chmod 755 "$STAGED_PACKAGE" "$STAGED_PACKAGE/install.sh" "$STAGED_PACKAGE/update.sh"
chmod 644 "$STAGED_PACKAGE"/{image.tar,compose.yml,.env.example,nginx-rillrss.conf,DEPLOY.zh-CN.md,VERSION}

(cd "$STAGED_PACKAGE" && sha256sum image.tar compose.yml .env.example nginx-rillrss.conf DEPLOY.zh-CN.md VERSION install.sh update.sh > SHA256SUMS)
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
