#!/usr/bin/env bash
# 把 workspace GUI 打包成可分发的 Smelt.app（Apple Silicon / arm64，不签名）。
#
# 用法：
#   ./scripts/package-mac.sh            # 用已有 release 产物组装
#   ./scripts/package-mac.sh --build    # 先 cargo build --release 再组装
#
# 产物：
#   dist/Smelt.app     —— 可双击运行的应用
#   dist/Smelt.dmg     —— 发给同事的分发件（定制拖拽安装窗口）
set -euo pipefail

APP_NAME="Smelt"
BIN_NAME="workspace"          # cargo 产物名
EXEC_NAME="smelt"             # .app 内可执行文件名
BUNDLE_ID="com.zzfn.smelt"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | sed -E 's/.*"(.*)".*/\1/')"
DIST="$ROOT/dist"
APP="$DIST/$APP_NAME.app"
MACOS="$APP/Contents/MacOS"
RES="$APP/Contents/Resources"
BIN="$ROOT/target/release/$BIN_NAME"
DAEMON_BIN="$ROOT/target/release/smeltd"   # 终端持久化守护（GUI 按同目录寻址拉起）

if [[ "${1:-}" == "--build" ]]; then
  echo "▶ 编译 release …"
  cargo build --release --bin "$BIN_NAME" --bin smeltd
fi

if [[ ! -f "$BIN" ]]; then
  echo "✗ 找不到 $BIN，先跑一次：cargo build --release --bin $BIN_NAME（或加 --build）" >&2
  exit 1
fi
if [[ ! -f "$DAEMON_BIN" ]]; then
  echo "✗ 找不到 $DAEMON_BIN（终端持久化守护），先：cargo build --release --bin smeltd" >&2
  exit 1
fi

# 校验是 arm64，避免误把 Intel 产物发给 Apple Silicon 同事
if ! file "$BIN" | grep -q "arm64"; then
  echo "✗ $BIN 不是 arm64，同事的 Apple Silicon Mac 会闪退。请在 Apple Silicon 上编译。" >&2
  exit 1
fi

echo "▶ 组装 $APP_NAME.app (v$VERSION) …"
rm -rf "$APP"
mkdir -p "$MACOS" "$RES"
cp "$BIN" "$MACOS/$EXEC_NAME"
chmod +x "$MACOS/$EXEC_NAME"
# 守护与 GUI 同目录（GUI 用 current_exe().with_file_name("smeltd") 寻址拉起）。
cp "$DAEMON_BIN" "$MACOS/smeltd"
chmod +x "$MACOS/smeltd"

# 图标（可选）：存在 assets/AppIcon.icns 就带上
ICON_LINE=""
if [[ -f "$ROOT/assets/AppIcon.icns" ]]; then
  cp "$ROOT/assets/AppIcon.icns" "$RES/AppIcon.icns"
  ICON_LINE=$'\t<key>CFBundleIconFile</key>\n\t<string>AppIcon</string>'
fi

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleName</key>
	<string>${APP_NAME}</string>
	<key>CFBundleDisplayName</key>
	<string>${APP_NAME}</string>
	<key>CFBundleIdentifier</key>
	<string>${BUNDLE_ID}</string>
	<key>CFBundleExecutable</key>
	<string>${EXEC_NAME}</string>
	<key>CFBundleVersion</key>
	<string>${VERSION}</string>
	<key>CFBundleShortVersionString</key>
	<string>${VERSION}</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>LSMinimumSystemVersion</key>
	<string>11.0</string>
	<key>NSHighResolutionCapable</key>
	<true/>
${ICON_LINE}
</dict>
</plist>
PLIST

# 去掉本机 quarantine，方便自测双击打开
xattr -cr "$APP" || true

echo "▶ 打 dmg（定制安装窗口）…"
# staging：app + 指向 /Applications 的软链 + 隐藏背景图。挂载后是一个固定尺寸、
# 带背景箭头的窗口，把 app 拖到「应用程序」即完成安装（精致拖拽安装体验）。
STAGE="$DIST/.dmg_stage"
rm -rf "$STAGE"; mkdir -p "$STAGE/.background"
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"

# 背景图（@1x + @2x 合成 retina 多分辨率 tiff）；缺 Pillow 则退化为无背景。
BG1="$DIST/.dmgbg.png"; BG2="$DIST/.dmgbg@2x.png"
if python3 "$ROOT/scripts/make-dmg-bg.py" "$BG1" "$BG2" >/dev/null 2>&1; then
  tiffutil -cathidpicheck "$BG1" "$BG2" -out "$STAGE/.background/bg.tiff" >/dev/null 2>&1 || true
fi
rm -f "$BG1" "$BG2"

VOL="$APP_NAME"
RW="$DIST/.rw.dmg"
rm -f "$RW"
hdiutil create -volname "$VOL" -srcfolder "$STAGE" -fs HFS+ -format UDRW -ov "$RW" >/dev/null

DEV="$(hdiutil attach -readwrite -noverify -noautoopen "$RW" | grep -E '^/dev/' | head -1 | awk '{print $1}')"
sleep 1

# 有背景 tiff 才设背景，否则只做布局定制。
if [[ -f "$STAGE/.background/bg.tiff" ]]; then
  BG_CMD='set background picture of theViewOptions to file ".background:bg.tiff"'
else
  BG_CMD=''
fi

# AppleScript 定制挂载窗口：隐藏工具栏/状态栏、固定尺寸、图标摆位（app 左 / 应用程序 右）。
osascript <<EOA || echo "  ⚠ Finder 定制被跳过（首次可能需在「系统设置→隐私与安全性→自动化」允许终端控制 Finder）；dmg 仍可用"
tell application "Finder"
  tell disk "$VOL"
    open
    set current view of container window to icon view
    set toolbar visible of container window to false
    set statusbar visible of container window to false
    set the bounds of container window to {200, 120, 840, 548}
    set theViewOptions to the icon view options of container window
    set arrangement of theViewOptions to not arranged
    set icon size of theViewOptions to 128
    $BG_CMD
    set position of item "$APP_NAME.app" of container window to {160, 210}
    set position of item "Applications" of container window to {480, 210}
    update without registering applications
    delay 1
    close
  end tell
end tell
EOA
sync

hdiutil detach "$DEV" >/dev/null 2>&1 || { sleep 2; hdiutil detach "$DEV" -force >/dev/null 2>&1 || true; }

rm -f "$DIST/$APP_NAME.dmg"
hdiutil convert "$RW" -format UDZO -imagekey zlib-level=9 -o "$DIST/$APP_NAME.dmg" >/dev/null
rm -f "$RW"; rm -rf "$STAGE"

echo ""
echo "✅ 完成"
echo "   应用：   $APP"
echo "   分发件： $DIST/$APP_NAME.dmg  （发这个给同事，双击挂载后拖进 Applications）"
