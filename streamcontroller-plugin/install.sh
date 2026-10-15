#!/usr/bin/env bash
# Copy the plugin into StreamController's plugin folder (Flatpak or native install).
# Restart StreamController afterwards.
set -euo pipefail
cd "$(dirname "$0")"
NAME=com_pwsoundboard_PWSoundboard
if [[ -d "$HOME/.var/app/com.core447.StreamController" ]]; then
    DEST="$HOME/.var/app/com.core447.StreamController/data/plugins"
else
    DEST="${XDG_DATA_HOME:-$HOME/.local/share}/StreamController/plugins"
fi
mkdir -p "$DEST"
rm -rf "${DEST:?}/$NAME"
cp -r "$NAME" "$DEST/$NAME"
find "$DEST/$NAME" -name __pycache__ -prune -exec rm -rf {} +
echo "Installed to $DEST/$NAME – restart StreamController."
