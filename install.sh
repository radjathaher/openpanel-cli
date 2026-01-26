#!/usr/bin/env bash
set -euo pipefail

REPO="radjathaher/openpanel-cli"
ASSET="openpanel-macos-arm64"

install_dir="${INSTALL_DIR:-/usr/local/bin}"
if [ ! -w "$install_dir" ]; then
  install_dir="$HOME/.local/bin"
fi
mkdir -p "$install_dir"

tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT

release_json="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest")"
asset_url="$(python3 - <<PY
import json, sys
name = "$ASSET"
release = json.load(sys.stdin)
for asset in release.get("assets", []):
    if asset.get("name") == name:
        print(asset.get("browser_download_url", ""))
        break
PY
<<<"$release_json")"

if [ -z "$asset_url" ]; then
  echo "error: asset $ASSET not found in latest release" >&2
  exit 1
fi

curl -fsSL "$asset_url" -o "$tmpdir/openpanel"
chmod +x "$tmpdir/openpanel"

mv "$tmpdir/openpanel" "$install_dir/openpanel"

echo "Installed openpanel to $install_dir/openpanel"
