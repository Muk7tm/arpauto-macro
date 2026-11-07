#!/bin/sh
# Install ARPAuto Macro for the current user (default prefix: ~/.local).
#   PREFIX=/usr/local sudo -E ./packaging/install.sh   for a system-wide install
set -eu
cd "$(dirname "$0")/.."
PREFIX="${PREFIX:-$HOME/.local}"
BINDIR="$PREFIX/bin"

if pgrep -x arpauto-macro >/dev/null 2>&1; then
    echo "Note: $(basename "$0" .sh): arpauto-macro is running. Close it after installing so the new version starts."
fi

if [ -x dist/arpauto-macro ] && [ "${REBUILD:-0}" != "1" ]; then
    BIN=dist/arpauto-macro
else
    cargo build --release --locked
    BIN=target/release/arpauto-macro
fi

install -Dm755 "$BIN" "$BINDIR/arpauto-macro"
install -Dm644 assets/icon.svg "$PREFIX/share/icons/hicolor/scalable/apps/arpauto-macro.svg"
install -Dm644 assets/icon.png "$PREFIX/share/icons/hicolor/256x256/apps/arpauto-macro.png"
mkdir -p "$PREFIX/share/applications"
sed "s|@BINDIR@|$BINDIR|g" packaging/arpauto-macro.desktop > "$PREFIX/share/applications/arpauto-macro.desktop"
chmod 644 "$PREFIX/share/applications/arpauto-macro.desktop"
command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database -q "$PREFIX/share/applications" || true

echo "Installed $("$BINDIR/arpauto-macro" --version) to $BINDIR/arpauto-macro"
case ":$PATH:" in *":$BINDIR:"*) ;; *) echo "Note: $BINDIR is not in PATH; the desktop entry uses the full path." ;; esac
