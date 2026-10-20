#!/bin/sh
# Sync a RillRSS instance with Newsboat through both of its remote backends
# (FreshRSS/Google Reader API and TT-RSS API) and print what each one sees.
#
#   scripts/verify_newsboat.sh https://rss.example.com admin 'api-password'
#
# Requires newsboat (e.g. `brew install newsboat` or `apt install newsboat`).
# Uses a temporary directory; nothing is written to your newsboat profile.
set -eu
BASE="${1:?usage: $0 <base-url> <user> <password>}"
USER_NAME="${2:?user}"
PASS="${3:?password}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

run() {
    name="$1"; shift
    mkdir -p "$WORK/$name"; : > "$WORK/$name/urls"
    printf '%s\n' "$@" > "$WORK/$name/config"
    echo "== $name"
    (cd "$WORK/$name" && newsboat -C config -u urls -c cache.db -x reload print-unread)
    if command -v sqlite3 >/dev/null 2>&1; then
        sqlite3 "$WORK/$name/cache.db" "SELECT COUNT(*) || ' articles, ' || COALESCE(SUM(unread), 0) || ' unread' FROM rss_item;"
    fi
}

run freshrss \
    'urls-source "freshrss"' \
    "freshrss-url \"$BASE/api/greader.php\"" \
    "freshrss-login \"$USER_NAME\"" \
    "freshrss-password \"$PASS\"" \
    'freshrss-min-items 100'

run ttrss \
    'urls-source "ttrss"' \
    "ttrss-url \"$BASE\"" \
    "ttrss-login \"$USER_NAME\"" \
    "ttrss-password \"$PASS\"" \
    'ttrss-mode "multi"'
