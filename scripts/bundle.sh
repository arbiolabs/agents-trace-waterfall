#!/bin/sh
# Builds target/release/Trace Waterfall.app: the desktop app as a proper macOS bundle with its icon.
set -eu
root="$(cd "$(dirname "$0")/.." && pwd)"
cargo build --release -p trace-app --manifest-path "$root/Cargo.toml"
app="$root/target/release/Trace Waterfall.app"
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$root/target/release/trace-waterfall" "$app/Contents/MacOS/trace-waterfall"
cp "$root/crates/trace-app/assets/app-icon/AppIcon.icns" "$app/Contents/Resources/AppIcon.icns"
cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Trace Waterfall</string>
  <key>CFBundleDisplayName</key><string>Trace Waterfall</string>
  <key>CFBundleIdentifier</key><string>dev.trace-waterfall.app</string>
  <key>CFBundleExecutable</key><string>trace-waterfall</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
</dict>
</plist>
PLIST
echo "$app"
