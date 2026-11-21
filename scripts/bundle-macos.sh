#!/bin/sh
# Builds dist/termy.app and dist/termy-<version>-macos-<arch>.dmg.
#
#   scripts/bundle-macos.sh            # native arch (Apple Silicon → arm64)
#   UNIVERSAL=1 scripts/bundle-macos.sh  # arm64 + x86_64 universal binary
#
# The app is ad-hoc signed: it runs on the Mac that built it. Distributing it
# to other Macs without Gatekeeper warnings needs a Developer ID certificate
# and notarization (see the end of this script).
set -eu
cd "$(dirname "$0")/.."
# System tools first: a stray PATH entry (e.g. XAMPP's Intel-only head/clear) must not break the build.
export PATH="$HOME/.cargo/bin:/usr/bin:/bin:/usr/sbin:/sbin:$PATH"

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
DIST=dist
APP="$DIST/termy.app"

if [ "${UNIVERSAL:-0}" = 1 ]; then
    ARCH=universal
    rustup target add aarch64-apple-darwin x86_64-apple-darwin >/dev/null
    cargo build --release --target aarch64-apple-darwin
    cargo build --release --target x86_64-apple-darwin
    BIN="$DIST/termy-universal"
    mkdir -p "$DIST"
    lipo -create -output "$BIN" \
        target/aarch64-apple-darwin/release/termy target/x86_64-apple-darwin/release/termy
else
    ARCH=$(uname -m)
    cargo build --release
    BIN=target/release/termy
fi

echo "==> Assembling $APP ($VERSION, $ARCH)"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/termy"
sed "s/__VERSION__/$VERSION/g" assets/macos/Info.plist > "$APP/Contents/Info.plist"
printf 'APPL????' > "$APP/Contents/PkgInfo"

# Icon: regenerate the PNG if missing, then build the .icns.
[ -f assets/macos/icon-1024.png ] || python3 scripts/make-icon.py assets/macos/icon-1024.png
ICONSET=$(mktemp -d)/termy.iconset
mkdir -p "$ICONSET"
for size in 16 32 128 256 512; do
    sips -z $size $size assets/macos/icon-1024.png --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
    double=$((size * 2))
    sips -z $double $double assets/macos/icon-1024.png --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/termy.icns"
rm -rf "$(dirname "$ICONSET")"

echo "==> Signing (ad-hoc)"
codesign --force --deep --options runtime --sign - "$APP"
codesign --verify --strict "$APP"

echo "==> Creating DMG"
DMG="$DIST/termy-$VERSION-macos-$ARCH.dmg"
STAGE=$(mktemp -d)
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
rm -f "$DMG"
hdiutil create -volname "termy $VERSION" -srcfolder "$STAGE" -fs HFS+ -format UDZO -quiet "$DMG"
rm -rf "$STAGE"

echo "==> Done"
du -sh "$APP" "$DMG"
# Distribution to other Macs (requires an Apple Developer account):
#   codesign --force --deep --options runtime --sign "Developer ID Application: …" dist/termy.app
#   xcrun notarytool submit dist/termy-*.dmg --keychain-profile <profile> --wait
#   xcrun stapler staple dist/termy-*.dmg
