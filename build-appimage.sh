#!/usr/bin/env bash
# Build dist/PW_Soundboard-<version>-<arch>.AppImage
#
#   ./build-appimage.sh            # build release binary + AppImage
#   APPIMAGETOOL=/path/to/appimagetool ./build-appimage.sh
#
# Requirements (Arch): sudo pacman -S --needed rust pipewire clang pkgconf
set -euo pipefail
cd "$(dirname "$0")"

APP=pw-soundboard
VERSION="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"
ARCH="$(uname -m)"
APPDIR=build/AppDir
OUT="dist/PW_Soundboard-${VERSION}-${ARCH}.AppImage"

need() { command -v "$1" >/dev/null 2>&1 || { echo "error: '$1' not found. $2" >&2; exit 1; }; }
need cargo      "Install Rust: sudo pacman -S rust (or rustup)"
need pkg-config "sudo pacman -S pkgconf"
need clang      "Needed to generate PipeWire bindings: sudo pacman -S clang"
pkg-config --exists libpipewire-0.3 || { echo "error: PipeWire dev files missing: sudo pacman -S pipewire" >&2; exit 1; }

echo "==> Building release binary"
# Keep local paths (home directory, checkout location) out of the binary's panic messages.
# The last matching prefix wins, so the more specific ones come after $HOME.
export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$HOME=/home/user \
--remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/cargo --remap-path-prefix=$PWD=/src"
cargo build --release

echo "==> Assembling AppDir"
rm -rf "$APPDIR"
install -Dm755 "target/release/$APP"            "$APPDIR/usr/bin/$APP"
install -Dm644 "packaging/$APP.desktop"         "$APPDIR/usr/share/applications/$APP.desktop"
install -Dm644 "assets/$APP.png"                "$APPDIR/usr/share/icons/hicolor/512x512/apps/$APP.png"
install -Dm755 packaging/AppRun                 "$APPDIR/AppRun"
cp "packaging/$APP.desktop" "$APPDIR/$APP.desktop"
cp "assets/$APP.png"        "$APPDIR/$APP.png"
ln -sf "$APP.png" "$APPDIR/.DirIcon"

echo "==> Locating appimagetool"
TOOL="${APPIMAGETOOL:-}"
if [[ -z "$TOOL" ]]; then
    if command -v appimagetool >/dev/null 2>&1; then
        TOOL="$(command -v appimagetool)"
    else
        TOOL="build/appimagetool-${ARCH}.AppImage"
        if [[ ! -x "$TOOL" ]]; then
            echo "    downloading appimagetool"
            curl -fL --retry 3 -o "$TOOL" \
                "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-${ARCH}.AppImage"
            chmod +x "$TOOL"
        fi
    fi
fi

echo "==> Packing $OUT"
mkdir -p dist
# Extract-and-run lets this work in containers/CI without FUSE.
APPIMAGE_EXTRACT_AND_RUN=1 ARCH="$ARCH" "$TOOL" --no-appstream "$APPDIR" "$OUT"
chmod +x "$OUT"
echo
echo "Done: $OUT"
