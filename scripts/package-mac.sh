#!/usr/bin/env bash
# 把 workspace GUI 打包成可分发的 Smelt.app（Apple Silicon / arm64，不签名）。
#
# 用法：
#   ./scripts/package-mac.sh            # 用已有 release 产物组装
#   ./scripts/package-mac.sh --build    # 先 cargo build --release 再组装
#
# 产物：
#   dist/Smelt.app     —— 可双击运行的应用
#   dist/Smelt.zip     —— 发给同事的分发件（含 app + 安装说明）
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

if [[ "${1:-}" == "--build" ]]; then
  echo "▶ 编译 release …"
  cargo build --release --bin "$BIN_NAME"
fi

if [[ ! -f "$BIN" ]]; then
  echo "✗ 找不到 $BIN，先跑一次：cargo build --release --bin $BIN_NAME（或加 --build）" >&2
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

echo "▶ 生成安装说明 …"
cat > "$DIST/INSTALL.txt" <<'TXT'
Smelt 安装说明（Apple Silicon Mac）
====================================

这个 app 没有做苹果签名，首次打开会提示「无法验证开发者」，属正常。
二选一放行：

方式 A（推荐，最快）：
  1. 把 Smelt.app 拖进「应用程序」文件夹
  2. 打开「终端」，粘贴执行：
       xattr -dr com.apple.quarantine /Applications/Smelt.app
  3. 之后双击正常打开

方式 B（不想用终端）：
  1. 右键点 Smelt.app → 选「打开」
  2. 弹窗里再点一次「打开」
  （只需这样做一次，之后可正常双击）
TXT

echo "▶ 打 zip …"
( cd "$DIST" && rm -f "$APP_NAME.zip" && zip -q -r -X "$APP_NAME.zip" "$APP_NAME.app" "INSTALL.txt" )

echo "▶ 打 dmg …"
# staging 里放 app + 一个指向 /Applications 的软链接：挂载后同事把 app
# 图标拖到 Applications 文件夹即完成安装（经典 Mac 拖拽安装体验）。
STAGE="$DIST/.dmg_stage"
rm -rf "$STAGE"; mkdir -p "$STAGE"
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
cp "$DIST/INSTALL.txt" "$STAGE/"
rm -f "$DIST/$APP_NAME.dmg"
hdiutil create -volname "$APP_NAME" -srcfolder "$STAGE" -ov -format UDZO "$DIST/$APP_NAME.dmg" >/dev/null
rm -rf "$STAGE"

echo ""
echo "✅ 完成"
echo "   应用：   $APP"
echo "   分发件： $DIST/$APP_NAME.dmg  （发这个给同事，双击挂载后拖进 Applications）"
echo "            $DIST/$APP_NAME.zip  （zip 备选，解压后自行拖入 Applications）"
