#!/bin/sh
# Builds target/LOST-VERSION-ARCH.AppImage from target/release/lost.
# Needs appimagetool (https://github.com/AppImage/appimagetool): on PATH, or
# set APPIMAGETOOL. LOST needs only the C library (X11, Wayland and OpenGL
# are loaded at run time from the system), so nothing else goes in.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -n 1)
arch=$(uname -m)
app="$root/target/LOST.AppDir"
tool=${APPIMAGETOOL:-appimagetool}

rm -rf "$app"
install -Dm755 "$root/target/release/lost" "$app/usr/bin/lost"
install -Dm644 "$root/dist/lost.desktop" "$app/usr/share/applications/lost.desktop"
install -Dm644 "$root/res/images/lost.svg" "$app/usr/share/icons/hicolor/scalable/apps/lost.svg"
install -Dm644 "$root/dist/icons/lost-256.png" "$app/usr/share/icons/hicolor/256x256/apps/lost.png"
# appimagetool wants these at the top of the AppDir.
cp "$root/dist/lost.desktop" "$app/lost.desktop"
cp "$root/dist/icons/lost-256.png" "$app/lost.png"
cat > "$app/AppRun" <<'RUN'
#!/bin/sh
here=$(dirname "$(readlink -f "$0")")
exec "$here/usr/bin/lost" "$@"
RUN
chmod 755 "$app/AppRun"

ARCH="$arch" "$tool" "$app" "$root/target/LOST-$version-$arch.AppImage"
echo "target/LOST-$version-$arch.AppImage"
