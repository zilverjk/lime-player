#!/usr/bin/env bash
# Rebuilds the macOS app bundle (package-macos-app.sh) and relaunches it so the running app
# shows the latest changes.
#
#   ./scripts/relaunch-macos-app.sh               build + relaunch unconditionally
#   ./scripts/relaunch-macos-app.sh --if-changed  only when a source file changed since the last
#                                                 attempt (used by the Claude Code Stop hook in
#                                                 .claude/settings.local.json)
#
# Exits 2 on a build failure, with the tail of the build log on stderr, so an `asyncRewake` hook
# can report it. A failed build is not retried until a source file changes again.
set -uo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_PATH="$ROOT_DIR/dist/Lime Player.app"
APP_BINARY="$APP_PATH/Contents/MacOS/lime-player"
STATE_DIR="$ROOT_DIR/target"
STAMP="$STATE_DIR/relaunch.stamp"
LOCK_DIR="$STATE_DIR/relaunch.lock"
LOG="$STATE_DIR/relaunch.log"

mkdir -p "$STATE_DIR"

# Only files that change the built app; docs, notes and .gitignored paths never trigger a rebuild.
sources_changed_since() {
    local reference="$1"
    [[ -e "$reference" ]] || return 0
    local since
    since="$(stat -f %m "$reference")"
    [[ -n "$(fd --type f --changed-within "@$since" --max-results 1 \
        -e rs -e slint -e svg -e toml -e lock -e icns -e png -e jpg \
        . "$ROOT_DIR" 2>/dev/null)" ]]
}

if [[ "${1:-}" == "--if-changed" ]]; then
    # A multi-step change in progress (e.g. a background agent workflow) holds rebuilds off until
    # it removes this file, so a half-edited tree is never built and relaunched.
    [[ -e "$STATE_DIR/relaunch.hold" ]] && exit 0
    reference="$STAMP"
    [[ -e "$reference" ]] || reference="$APP_BINARY"
    sources_changed_since "$reference" || exit 0
fi

# One run at a time; a lock left behind by a dead run is taken over.
if ! mkdir "$LOCK_DIR" 2>/dev/null; then
    # No pid yet means another run is just starting.
    if [[ ! -e "$LOCK_DIR/pid" ]] || kill -0 "$(<"$LOCK_DIR/pid")" 2>/dev/null; then
        exit 0
    fi
    rm -rf "$LOCK_DIR"
    mkdir "$LOCK_DIR" || exit 0
fi
echo $$ >"$LOCK_DIR/pid"
trap 'rm -rf "$LOCK_DIR"' EXIT

# Marked before building, so an edit made while the build runs still counts as a change next time.
touch "$STAMP.next"

if ! "$ROOT_DIR/scripts/package-macos-app.sh" >"$LOG" 2>&1; then
    mv "$STAMP.next" "$STAMP"
    osascript -e 'display notification "Build failed — see target/relaunch.log" with title "Lime Player"' 2>/dev/null
    {
        echo "Lime Player rebuild failed (full log: $LOG):"
        tail -n 25 "$LOG"
    } >&2
    exit 2
fi
mv "$STAMP.next" "$STAMP"

pkill -f "$APP_BINARY"
for _ in {1..50}; do
    pgrep -f "$APP_BINARY" >/dev/null || break
    sleep 0.1
done
open "$APP_PATH"
echo "Lime Player rebuilt and relaunched."
