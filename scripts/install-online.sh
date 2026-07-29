#!/usr/bin/env sh
set -eu

REPOSITORY=${1:-Divitmehta123/divits-opensource}
PLATFORM=${2:-linux-x64}
API_URL="https://api.github.com/repos/$REPOSITORY/releases/latest"
RELEASE_JSON=$(curl -fsSL -H 'User-Agent: Divits-OpenSource-Installer' "$API_URL")
ASSET="divits-opensource-$PLATFORM.tar.gz"
ARCHIVE_URL=$(printf '%s' "$RELEASE_JSON" | sed -n "s#.*\"browser_download_url\": \"\([^\"]*/$ASSET\)\".*#\1#p" | head -n 1)
CHECKSUM_URL=$(printf '%s' "$RELEASE_JSON" | sed -n "s#.*\"browser_download_url\": \"\([^\"]*/$ASSET.sha256\)\".*#\1#p" | head -n 1)
[ -n "$ARCHIVE_URL" ] && [ -n "$CHECKSUM_URL" ] || {
  echo "Latest release does not contain $ASSET and its SHA-256 checksum." >&2
  exit 1
}

TEMPORARY=$(mktemp -d)
trap 'rm -rf "$TEMPORARY"' EXIT
curl -fsSL "$ARCHIVE_URL" -o "$TEMPORARY/$ASSET"
curl -fsSL "$CHECKSUM_URL" -o "$TEMPORARY/$ASSET.sha256"
(cd "$TEMPORARY" && sha256sum -c "$ASSET.sha256")
tar -xzf "$TEMPORARY/$ASSET" -C "$TEMPORARY"
"$TEMPORARY/install.sh"
