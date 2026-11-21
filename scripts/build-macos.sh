#!/bin/sh
# Build Redline for macOS and wrap it as dist/Redline.app (plus a zip of it).
#
#   scripts/build-macos.sh
#
# Needs Rust (rustup picks the version from redline/rust-toolchain.toml) and Xcode's swiftc.
# The app is signed ad hoc, not notarized: on another Mac, right-click it and choose Open the
# first time.
set -e
cd "$(dirname "$0")/.."
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' redline/Cargo.toml | head -1)
APP=dist/Redline.app

(cd redline && cargo build --release)
# The window-capture helper sits next to the redline binary, in the build folder and in the app.
swiftc -O redline/capture/main.swift -o redline/target/release/redline-capture

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources/mcp"
cp redline/target/release/redline redline/target/release/redline-capture "$APP/Contents/MacOS/"
cp redline/mcp/server.mjs "$APP/Contents/Resources/mcp/"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleName</key><string>Redline</string>
	<key>CFBundleDisplayName</key><string>Redline</string>
	<key>CFBundleIdentifier</key><string>dev.redliner.Redline</string>
	<key>CFBundleExecutable</key><string>redline</string>
	<key>CFBundlePackageType</key><string>APPL</string>
	<key>CFBundleShortVersionString</key><string>$VERSION</string>
	<key>CFBundleVersion</key><string>$VERSION</string>
	<key>LSMinimumSystemVersion</key><string>14.0</string>
	<key>LSUIElement</key><true/>
	<key>NSHighResolutionCapable</key><true/>
	<key>NSMicrophoneUsageDescription</key><string>Redline records what you say while the pen is on, and turns it into text on this Mac.</string>
</dict>
</plist>
PLIST
codesign --force --deep --sign - "$APP"
(cd dist && rm -f "Redline-$VERSION-macos-$(uname -m).zip" && ditto -c -k --keepParent Redline.app "Redline-$VERSION-macos-$(uname -m).zip")
echo "Built $APP and dist/Redline-$VERSION-macos-$(uname -m).zip"
