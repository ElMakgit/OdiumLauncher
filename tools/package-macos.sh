#!/usr/bin/env bash
# Builds a universal (Apple Silicon + Intel) Odium Launcher bundle and DMG.
#
# Runs on macOS with Rust (rustup) and Xcode command line tools installed.
# From the repository root:
#
#     ./tools/package-macos.sh
#
# The result is dist/OdiumLauncher-macos-universal.dmg.
set -euo pipefail

cd "$(dirname "$0")/.."

APP_NAME="Odium Launcher"
BIN_NAME="OdiumLauncher"
BUNDLE_ID="com.elmakgit.odiumlauncher"
VERSION="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n1)"
VERSION="${VERSION:-0.1.0}"
DIST="dist"
APP="${DIST}/${BIN_NAME}.app"

echo "==> Building ${BIN_NAME} ${VERSION} for both macOS architectures"
rustup target add aarch64-apple-darwin x86_64-apple-darwin
cargo build --release --target aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin

echo "==> Creating the universal binary"
rm -rf "${APP}"
mkdir -p "${APP}/Contents/MacOS" "${APP}/Contents/Resources"
lipo -create \
    "target/aarch64-apple-darwin/release/${BIN_NAME}" \
    "target/x86_64-apple-darwin/release/${BIN_NAME}" \
    -output "${APP}/Contents/MacOS/${BIN_NAME}"
chmod +x "${APP}/Contents/MacOS/${BIN_NAME}"

echo "==> Writing Info.plist"
cat > "${APP}/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleExecutable</key>
    <string>${BIN_NAME}</string>
    <key>CFBundleIdentifier</key>
    <string>${BUNDLE_ID}</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>${APP_NAME}</string>
    <key>CFBundleDisplayName</key>
    <string>${APP_NAME}</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>${VERSION}</string>
    <key>CFBundleVersion</key>
    <string>${VERSION}</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
    <key>LSMinimumSystemVersion</key>
    <string>11.0</string>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.games</string>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
PLIST

echo "==> Rendering the app icon"
ICONSET="target/AppIcon.iconset"
rm -rf "${ICONSET}"
mkdir -p "${ICONSET}"
SOURCE_ICON="crates/pml-ui/assets/dog_face.png"
for size in 16 32 64 128 256 512; do
    sips -z "${size}" "${size}" "${SOURCE_ICON}" \
        --out "${ICONSET}/icon_${size}x${size}.png" >/dev/null
    double=$((size * 2))
    sips -z "${double}" "${double}" "${SOURCE_ICON}" \
        --out "${ICONSET}/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "${ICONSET}" -o "${APP}/Contents/Resources/AppIcon.icns"

echo "==> Signing the bundle (ad-hoc)"
# Arm64 binaries must carry at least an ad-hoc signature to start. Without an
# Apple Developer certificate the app is not notarised: the first launch on
# another Mac needs right-click -> Open (or `xattr -dr com.apple.quarantine`).
codesign --force --deep --sign - "${APP}"
codesign --verify --deep --strict "${APP}"

echo "==> Creating the DMG"
DMG_ROOT="target/dmg-root"
DMG="${DIST}/${BIN_NAME}-macos-universal.dmg"
rm -rf "${DMG_ROOT}" "${DMG}"
mkdir -p "${DMG_ROOT}"
cp -R "${APP}" "${DMG_ROOT}/"
ln -s /Applications "${DMG_ROOT}/Applications"
hdiutil create -volname "${APP_NAME}" -srcfolder "${DMG_ROOT}" \
    -ov -format UDZO "${DMG}"

echo "==> Done: ${DMG}"
