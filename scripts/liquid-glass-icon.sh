#!/usr/bin/env bash
# Compile the Liquid Glass app icon (macOS 26 and later) into app/src-tauri/macos/Assets.car.
#
# scripts/app-icon.py writes the Icon Composer document assets/logo/Varve.icon; this script runs it,
# then compiles the document with actool. Tauri cannot compile .icon documents itself, so the
# compiled Assets.car is committed and bundle.macOS.files in tauri.conf.json copies it into
# Contents/Resources. app/src-tauri/Info.plist sets CFBundleIconName to Varve so macOS 26 picks the
# icon from Assets.car; older macOS keeps using icon.icns through CFBundleIconFile.
#
# Needs Xcode 26 or later (actool reads .icon documents from Xcode 26 on).
#
# Usage: scripts/liquid-glass-icon.sh [preview-dir]
#   With a preview directory, it also renders every appearance to PNG there with Icon Composer's
#   ictool, to look at before committing.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
icon="$root/assets/logo/Varve.icon"
out="$root/app/src-tauri/macos"
# The deployment target of the app: Tauri's default bundle.macOS.minimumSystemVersion.
min_macos="10.13"

python3 -I -B "$root/scripts/app-icon.py"

# actool also writes Varve.icns, a pre-26 fallback rendered from the glass document. The app keeps
# its own icon.icns for that, so actool compiles into a temporary folder (left for the system to
# clean up) and only Assets.car is copied out.
build="$(mktemp -d -t varve-icon)"
partial_plist="$build/partial-Info.plist"

xcrun actool "$icon" \
  --compile "$build" \
  --app-icon Varve \
  --platform macosx \
  --target-device mac \
  --minimum-deployment-target "$min_macos" \
  --output-partial-info-plist "$partial_plist" \
  --output-format human-readable-text \
  --notices --warnings --errors

mkdir -p "$out"
cp "$build/Assets.car" "$out/Assets.car"

# app/src-tauri/Info.plist takes CFBundleIconName from this. Not CFBundleIconFile: Tauri sets that
# to icon.icns, which stays the icon before macOS 26.
echo "partial Info.plist from actool:"
cat "$partial_plist"

echo "icon assets in $out/Assets.car (count, name, appearance):"
xcrun assetutil --info "$out/Assets.car" \
  | python3 -I -B -c '
import collections, json, sys
seen = collections.Counter((e.get("Name"), e.get("Appearance", "-")) for e in json.load(sys.stdin) if e.get("Name"))
for (name, appearance), n in sorted(seen.items()):
    print(f"  {n:3}  {name}  {appearance}")
'

if [[ $# -ge 1 ]]; then
  preview="$1"
  mkdir -p "$preview"
  ictool="$(xcode-select -p)/../Applications/Icon Composer.app/Contents/Executables/ictool"
  for rendition in Default Dark ClearLight ClearDark TintedLight TintedDark; do
    "$ictool" "$icon" --export-image --output-file "$preview/glass-$rendition.png" \
      --platform macOS --rendition "$rendition" --width 512 --height 512 --scale 1 >/dev/null
  done
  echo "previews in $preview"
fi
