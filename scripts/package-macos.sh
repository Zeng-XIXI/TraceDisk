#!/bin/sh
set -eu

SCRIPT_DIRECTORY=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
PROJECT_DIRECTORY=$(CDPATH= cd -- "$SCRIPT_DIRECTORY/.." && pwd)
PACKAGE_JSON="$PROJECT_DIRECTORY/apps/desktop/package.json"
TAURI_CONFIG="$PROJECT_DIRECTORY/apps/desktop/src-tauri/tauri.conf.json"
CARGO_MANIFEST="$PROJECT_DIRECTORY/Cargo.toml"

PACKAGE_VERSION=$(/usr/bin/plutil -extract version raw "$PACKAGE_JSON")
TAURI_VERSION=$(/usr/bin/plutil -extract version raw "$TAURI_CONFIG")
CARGO_VERSION=$(/usr/bin/sed -n 's/^version = "\([^"]*\)"/\1/p' "$CARGO_MANIFEST" | /usr/bin/head -n 1)
VERSION=${1:-$PACKAGE_VERSION}

case "$VERSION" in
    ''|*[!0-9.]*|.*|*.)
        echo "版本号必须采用数字点分格式，例如 0.1.0" >&2
        exit 1
        ;;
esac

if [ "$PACKAGE_VERSION" != "$VERSION" ] || [ "$TAURI_VERSION" != "$VERSION" ] || [ "$CARGO_VERSION" != "$VERSION" ]; then
    echo "版本号不一致：Cargo=$CARGO_VERSION, package=$PACKAGE_VERSION, Tauri=$TAURI_VERSION, requested=$VERSION" >&2
    exit 1
fi

ARCHITECTURE=$(/usr/bin/uname -m)
case "$ARCHITECTURE" in
    arm64|x86_64) ;;
    *)
        echo "不支持的 macOS 架构：$ARCHITECTURE" >&2
        exit 1
        ;;
esac

RELEASE_DIRECTORY="$PROJECT_DIRECTORY/release/v$VERSION"
APP_PATH="$PROJECT_DIRECTORY/target/release/bundle/macos/TraceDisk.app"
ARTIFACT_PREFIX="TraceDisk-v$VERSION-macos-$ARCHITECTURE"
ZIP_PATH="$RELEASE_DIRECTORY/$ARTIFACT_PREFIX.zip"
DMG_PATH="$RELEASE_DIRECTORY/$ARTIFACT_PREFIX.dmg"
CHECKSUM_PATH="$RELEASE_DIRECTORY/SHA256SUMS.txt"
RELEASE_NOTES_SOURCE="$PROJECT_DIRECTORY/docs/releases/v$VERSION.md"

if [ ! -f "$RELEASE_NOTES_SOURCE" ]; then
    echo "缺少版本说明：$RELEASE_NOTES_SOURCE" >&2
    exit 1
fi

/bin/mkdir -p "$RELEASE_DIRECTORY"
cd "$PROJECT_DIRECTORY/apps/desktop"
npm run bundle:mac

/usr/bin/codesign --verify --deep --strict --verbose=2 "$APP_PATH"
/usr/bin/ditto -c -k --sequesterRsrc --keepParent "$APP_PATH" "$ZIP_PATH"
/usr/bin/hdiutil create -volname "TraceDisk v$VERSION" -srcfolder "$APP_PATH" -ov -format UDZO "$DMG_PATH"
/bin/cp "$RELEASE_NOTES_SOURCE" "$RELEASE_DIRECTORY/RELEASE_NOTES.md"
/usr/bin/shasum -a 256 "$ZIP_PATH" "$DMG_PATH" > "$CHECKSUM_PATH"

echo "TraceDisk v$VERSION macOS 发布包已生成："
echo "$RELEASE_DIRECTORY"
