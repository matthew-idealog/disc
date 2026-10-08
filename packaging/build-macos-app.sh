#!/bin/bash
# Usage: packaging/build-macos-app.sh <path-to-disc-binary> <version> <output-dir>
# Assembles DISC.app around an already-built binary and zips it.
set -euo pipefail

BINARY="$1"
VERSION="$2"
OUT="$3"
HERE="$(cd "$(dirname "$0")" && pwd)"
APP="$OUT/DISC.app"

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
sed "s/__VERSION__/$VERSION/g" "$HERE/macos/Info.plist" > "$APP/Contents/Info.plist"
cp "$HERE/macos/launcher.sh" "$APP/Contents/MacOS/launcher"
cp "$BINARY" "$APP/Contents/MacOS/disc"
chmod +x "$APP/Contents/MacOS/launcher" "$APP/Contents/MacOS/disc"

# Ad-hoc signature: required for Apple Silicon; does not avoid the Gatekeeper prompt.
codesign --force --deep --sign - "$APP"

(cd "$OUT" && ditto -c -k --keepParent DISC.app DISC-macos.zip)
