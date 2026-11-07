#!/bin/sh
# Remove ARPAuto Macro. Settings and profiles are kept unless --purge is given.
set -eu
PREFIX="${PREFIX:-$HOME/.local}"
rm -f "$PREFIX/bin/arpauto-macro" \
      "$PREFIX/share/applications/arpauto-macro.desktop" \
      "$PREFIX/share/icons/hicolor/scalable/apps/arpauto-macro.svg" \
      "$PREFIX/share/icons/hicolor/256x256/apps/arpauto-macro.png"
if [ "${1:-}" = "--purge" ]; then
    rm -rf "${XDG_CONFIG_HOME:-$HOME/.config}/arpauto-macro"
    echo "Removed ARPAuto Macro and its settings."
else
    echo "Removed ARPAuto Macro. Settings remain in ${XDG_CONFIG_HOME:-$HOME/.config}/arpauto-macro (use --purge to delete)."
fi
