#!/usr/bin/env bash
# Regenerates assets/AppIcon.icns from assets/icon.svg. Requires resvg (`brew install resvg`)
# to rasterize with a transparent background; iconutil ships with macOS.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SVG="$ROOT/assets/icon.svg"
OUT="$ROOT/assets/AppIcon.icns"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

command -v resvg >/dev/null || { echo "resvg is required: brew install resvg" >&2; exit 1; }
MASTER="$WORK/icon-1024.png"
resvg -w 1024 -h 1024 "$SVG" "$MASTER"

ICONSET="$WORK/AppIcon.iconset"
mkdir -p "$ICONSET"
for size in 16 32 128 256 512; do
  double=$((size * 2))
  resvg -w "$size" -h "$size" "$SVG" "$ICONSET/icon_${size}x${size}.png"
  resvg -w "$double" -h "$double" "$SVG" "$ICONSET/icon_${size}x${size}@2x.png"
done
iconutil -c icns "$ICONSET" -o "$OUT"
echo "$OUT"
