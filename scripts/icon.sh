#!/usr/bin/env bash
# Renders the app icon with scripts/icon.swift and packs it into
# assets/AppIcon.icns, which scripts/bundle.sh copies into the .app.
#
#   scripts/icon.sh
set -euo pipefail

cd "$(dirname "$0")/.."

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

swift scripts/icon.swift "$WORK/icon.png"

ICONSET="$WORK/AppIcon.iconset"
mkdir -p "$ICONSET"
for size in 16 32 128 256 512; do
  sips -z "$size" "$size" "$WORK/icon.png" --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
  double=$((size * 2))
  sips -z "$double" "$double" "$WORK/icon.png" --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
done

mkdir -p assets
iconutil -c icns "$ICONSET" -o assets/AppIcon.icns
echo "Escrito assets/AppIcon.icns"
