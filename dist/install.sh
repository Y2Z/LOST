#!/bin/sh
# Installs LOST: the program, its desktop entry and icons, the systemd unit
# for the web server (not started), and the example /etc/lost/lost.toml.
#
#   sudo sh dist/install.sh                 # into /usr/local
#   PREFIX=/usr sudo -E sh dist/install.sh  # elsewhere
#   DESTDIR=/tmp/pkg sh dist/install.sh     # staged, for packagers
#   sh dist/install.sh --uninstall          # removes what it installed
#
# Run from a release tarball (the program is next to this folder) or from a
# source checkout after `cargo build --release`.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
root=$(dirname "$here")
PREFIX=${PREFIX:-/usr/local}
DESTDIR=${DESTDIR:-}
if [ -z "${BIN:-}" ]; then
    if [ -f "$root/lost" ]; then BIN="$root/lost"; else BIN="$root/target/release/lost"; fi
fi

bin="$DESTDIR$PREFIX/bin/lost"
apps="$DESTDIR$PREFIX/share/applications"
icons="$DESTDIR$PREFIX/share/icons/hicolor"
unit="$DESTDIR$PREFIX/lib/systemd/system/lost.service"
config="$DESTDIR/etc/lost/lost.toml"
sizes="16 24 32 48 64 128 256 512"

if [ "${1:-}" = "--uninstall" ]; then
    rm -f "$bin" "$apps/lost.desktop" "$icons/scalable/apps/lost.svg" "$unit"
    for size in $sizes; do rm -f "$icons/${size}x${size}/apps/lost.png"; done
    echo "Removed LOST from $PREFIX. Your data (~/.lost) and /etc/lost were left alone."
    exit 0
fi

if [ ! -f "$BIN" ]; then
    echo "No program at $BIN: run 'cargo build --release' first." >&2
    exit 1
fi

install -Dm755 "$BIN" "$bin"
install -Dm644 "$here/lost.desktop" "$apps/lost.desktop"
install -Dm644 "$root/res/images/lost.svg" "$icons/scalable/apps/lost.svg"
for size in $sizes; do
    install -Dm644 "$here/icons/lost-$size.png" "$icons/${size}x${size}/apps/lost.png"
done
# The unit runs the program where it was just installed.
mkdir -p "$(dirname "$unit")"
sed "s|/usr/bin/lost|$PREFIX/bin/lost|" "$here/lost.service" > "$unit"
chmod 644 "$unit"
# Never overwrite a configuration that's already there.
if [ ! -e "$config" ]; then
    install -Dm644 "$here/lost.toml" "$config"
fi

if [ -z "$DESTDIR" ]; then
    command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database -q "$PREFIX/share/applications" || true
    command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -q -t "$PREFIX/share/icons/hicolor" || true
    command -v systemctl >/dev/null 2>&1 && systemctl daemon-reload 2>/dev/null || true
fi
echo "Installed LOST in $PREFIX. Run 'lost' (or find LOST in your applications)."
echo "Web server as a service: sudo systemctl enable --now lost"
