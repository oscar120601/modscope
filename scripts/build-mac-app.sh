#!/usr/bin/env bash
# 在 Mac 上編譯並打包成 ModScope.app（Apple Silicon / Intel 皆可）
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release
APP="target/ModScope.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/modscope "$APP/Contents/MacOS/ModScope"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>ModScope</string>
  <key>CFBundleDisplayName</key><string>ModScope</string>
  <key>CFBundleIdentifier</key><string>io.github.oscar120601.modscope</string>
  <key>CFBundleExecutable</key><string>ModScope</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
</dict></plist>
PLIST
codesign --force --deep -s - "$APP" 2>/dev/null || true   # ad-hoc 簽章，本機執行用
echo "完成：$APP （可拖進「應用程式」資料夾）"
