#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
dmg_path="$1"
expected_arch="$2"
version="$3"
work="$(mktemp -d)"
mount="$work/mount"
mkdir "$mount"
cleanup() {
  hdiutil detach "$mount" >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT
hdiutil verify "$dmg_path"
hdiutil attach -readonly -nobrowse -mountpoint "$mount" "$dmg_path"
app="$mount/MultiGateway.app"
binary="$app/Contents/MacOS/MultiGateway"
test -x "$binary"
test "$(readlink "$mount/Applications")" = /Applications
test "$(lipo -archs "$binary")" = "$expected_arch"
test "$(plutil -extract CFBundleShortVersionString raw "$app/Contents/Info.plist")" = "$version"
test "$(plutil -extract CFBundleVersion raw "$app/Contents/Info.plist")" = "$version"
test "$(plutil -extract CFBundleIdentifier raw "$app/Contents/Info.plist")" = com.multigateway.desktop
test -s "$app/Contents/Resources/AppIcon.icns"
codesign --verify --deep --strict --verbose=2 "$app"
ditto "$app" "$work/installed/MultiGateway.app"
cmp "$binary" "$work/installed/MultiGateway.app/Contents/MacOS/MultiGateway"
python3 "$root/scripts/smoke-package.py" "$work/installed/MultiGateway.app/Contents/MacOS/MultiGateway" "$version"
echo "DMG verified: $expected_arch $version"
