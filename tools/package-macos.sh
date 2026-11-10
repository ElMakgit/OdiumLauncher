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

# The embedded offline NeoForge archives make each release binary large, and the
# GitHub runner has little free disk. Object files are useless once a target is
# linked, so each architecture tree is dropped before the next one is built;
# otherwise two target trees plus the universal binary do not fit and `hdiutil`
# fails with "No space left on device".
free_target_objects() {
    rm -rf "target/$1/release/deps" \
           "target/$1/release/build" \
           "target/$1/release/incremental" \
           "target/$1/release/.fingerprint"
}

cargo build --release --target aarch64-apple-darwin
free_target_objects aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin
free_target_objects x86_64-apple-darwin
df -h . | tail -n1

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

echo "==> Copying the license"
cp LICENSE "${APP}/Contents/Resources/LICENSE"
cp THIRD-PARTY-NOTICES.md "${APP}/Contents/Resources/THIRD-PARTY-NOTICES.md"

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
RW_DMG="target/${BIN_NAME}-rw.dmg"
MOUNT="target/dmg-mount"
rm -rf "${DMG_ROOT}" "${DMG}" "${RW_DMG}" "${MOUNT}"
mkdir -p "${DMG_ROOT}"
cp -R "${APP}" "${DMG_ROOT}/"
ln -s /Applications "${DMG_ROOT}/Applications"

# hdiutil's automatic estimate is too tight for a ~100 MB universal binary with
# the embedded archive: the read-write image fills up mid-copy and hdiutil
# reports "No space left on device" even on a disk with plenty of room. Size the
# image explicitly with a wide margin instead.
size_mb=$(($(du -sm "${DMG_ROOT}" | cut -f1) * 3 / 2 + 64))
hdiutil create -size "${size_mb}m" -fs HFS+ -volname "${APP_NAME}" "${RW_DMG}"
mkdir -p "${MOUNT}"
hdiutil attach "${RW_DMG}" -mountpoint "${MOUNT}" -nobrowse -readwrite
cp -R "${DMG_ROOT}/." "${MOUNT}/"
sync
hdiutil detach "${MOUNT}" || hdiutil detach "${MOUNT}" -force
hdiutil convert "${RW_DMG}" -format UDZO -imagekey zlib-level=9 -ov -o "${DMG}"
rm -f "${RW_DMG}"

echo "==> Writing the SHA-256 checksum"
shasum -a 256 "${DMG}" | awk '{print $1}' > "${DMG}.sha256"
echo "==> Done: ${DMG} (+ .sha256)"
