#!/bin/sh
# Builds target/LOST.app (and LOST-VERSION-macos.zip) from a release build.
# With BIN unset it uses target/release/lost; the release workflow passes a
# universal (Intel + Apple silicon) binary made with lipo. Not signed:
# the first time, open it with right-click → Open.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -n 1)
BIN=${BIN:-$root/target/release/lost}
app="$root/target/LOST.app"

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$BIN" "$app/Contents/MacOS/lost"
cp "$root/dist/icons/lost.icns" "$app/Contents/Resources/lost.icns"
sed "s/@VERSION@/$version/g" "$root/dist/Info.plist" > "$app/Contents/Info.plist"

cd "$root/target"
rm -f "LOST-$version-macos.zip"
ditto -c -k --keepParent LOST.app "LOST-$version-macos.zip"
echo "target/LOST-$version-macos.zip"
