#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIST_DIR="$ROOT_DIR/dist"
APP_PATH="$DIST_DIR/Lime Player.app"
VERSION="$(awk -F '"' '/^version =/ { print $2; exit }' "$ROOT_DIR/Cargo.toml")"
ARCH="$(uname -m)"

if [[ -z "$VERSION" ]]; then
    echo "Could not read package version from Cargo.toml" >&2
    exit 1
fi

NAME="Lime-Player-$VERSION-$ARCH"
DMG_PATH="$DIST_DIR/$NAME.dmg"
ZIP_PATH="$DIST_DIR/$NAME.zip"

# Build the ad-hoc-signed app bundle (release build).
rm -rf "$APP_PATH"
"$ROOT_DIR/scripts/package-macos-app.sh" "$APP_PATH"

# Stage the DMG contents: the app plus an Applications symlink for drag-to-install.
STAGING="$(mktemp -d)"
trap 'rm -rf "$STAGING"' EXIT
ditto "$APP_PATH" "$STAGING/Lime Player.app"
ln -s /Applications "$STAGING/Applications"

# hdiutil sometimes fails with "Resource busy" on CI runners, so retry.
rm -f "$DMG_PATH"
for attempt in 1 2 3 4 5; do
    if hdiutil create -volname "Lime Player" -srcfolder "$STAGING" \
        -fs HFS+ -format UDZO -ov "$DMG_PATH"; then
        break
    fi
    if [[ "$attempt" -eq 5 ]]; then
        echo "hdiutil create failed after $attempt attempts" >&2
        exit 1
    fi
    echo "hdiutil create failed (attempt $attempt), retrying..." >&2
    sleep $((attempt * 3))
done

# Alternative download: the zipped app bundle.
rm -f "$ZIP_PATH"
ditto -c -k --keepParent "$APP_PATH" "$ZIP_PATH"

echo "Created DMG installer: $DMG_PATH"
echo "Created ZIP archive:   $ZIP_PATH"
