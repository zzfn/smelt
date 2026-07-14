#!/usr/bin/env bash
# 把 workspace GUI 打包成可分发的 Smelt.app（Apple Silicon / arm64，不签名）。
#
# 用法：
#   ./scripts/package-mac.sh            # 用已有 release 产物组装
#   ./scripts/package-mac.sh --build    # 先 cargo build --release 再组装
#
# 产物：
#   dist/Smelt.app     —— 可双击运行的应用
#   dist/Smelt.dmg     —— 分发件（定制拖拽安装窗口）
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
  echo "✗ 找不到 ${BIN}，先跑一次：cargo build --release --bin $BIN_NAME（或加 --build）" >&2
  exit 1
fi
if [[ ! -f "$DAEMON_BIN" ]]; then
  echo "✗ 找不到 ${DAEMON_BIN}（终端持久化守护），先：cargo build --release --bin smeltd" >&2
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
	<!-- 声明可打开文件夹：Dock 图标才接受拖入目录（触发 application:openURLs:）。
	     LSHandlerRank=Alternate 避免抢占系统默认的文件夹打开方式。 -->
	<key>CFBundleDocumentTypes</key>
	<array>
		<dict>
			<key>CFBundleTypeName</key>
			<string>Folder</string>
			<key>CFBundleTypeRole</key>
			<string>Viewer</string>
			<key>LSHandlerRank</key>
			<string>Alternate</string>
			<key>LSItemContentTypes</key>
			<array>
				<string>public.folder</string>
			</array>
		</dict>
	</array>
${ICON_LINE}
</dict>
</plist>
PLIST

# 去掉本机 quarantine，方便自测双击打开
xattr -cr "$APP" || true

# 签名：优先用 scripts/setup-codesign-identity.sh 生成的自签名身份——它的
# designated requirement 锚定在证书哈希上，重新编译/升级二进制内容变了也不影响，
# macOS TCC（完全磁盘访问权限等）授权能跨版本保留。找不到该身份（没跑过那个脚本
# 的贡献者机器、或没配置的 CI）就回退到 ad-hoc（免费但每次内容一变身份就变，权限
# 跟着失效）。CI 可通过 SMELT_CODESIGN_IDENTITY 环境变量指定身份名。
# 注意：不管哪种签名都过不了 Gatekeeper 公证（那个需要付费 Developer ID + 公证），
# 这里只解决权限持久化，自用/局域网分发够用。
IDENTITY="${SMELT_CODESIGN_IDENTITY:-Smelt Local Signing}"
if ! security find-certificate -c "$IDENTITY" >/dev/null 2>&1; then
  echo "⚠ 未找到签名身份「${IDENTITY}」，回退到 ad-hoc 签名（权限会在重新编译后失效）"
  echo "  一次性修复：./scripts/setup-codesign-identity.sh"
  IDENTITY="-"
fi
if [[ "$IDENTITY" == "-" ]]; then
  echo "▶ ad-hoc 签名（免费，仅用于稳定权限身份，不影响 Gatekeeper 警告）…"
else
  echo "▶ 签名（身份：${IDENTITY}）…"
fi
codesign --force --deep --sign "$IDENTITY" "$APP"
codesign --verify --deep --strict "$APP" && echo "  ✓ 签名校验通过"

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

# 上次若中途失败没 detach 干净，残留挂载会把这次的新盘挤成 "Smelt 1"，下面认死
# "$VOL" 这个名字的 AppleScript 会定制到旧盘上，新盘反而悄悄产出朴素无定制的 dmg——
# 这是「有时候定制没效果」的一个真实根因。开工前先清掉同名残留，保证这次 attach
# 出来的就是 "/Volumes/$VOL"。（极端情况下若你真有块外置盘恰好也叫 Smelt 会被一并
# 弹出，但这种命名巧合概率极低，权衡后可接受。）
if [[ -d "/Volumes/$VOL" ]]; then
  echo "  … 清理上次残留的挂载 /Volumes/$VOL"
  hdiutil detach "/Volumes/$VOL" -force >/dev/null 2>&1 || true
fi

# CI 里没有 Finder/GUI session，下面那套 AppleScript 窗口定制本来就做不了（只会走到
# 「⚠ Finder 定制失败」那条容错分支），产出的必然是朴素 dmg。既然定制不可能生效，就
# 没必要为它走「建可写映像 → attach → 定制 → detach → convert」这条挂载链：runner 上
# hdiutil 的挂载/卸载会偶发撞车——v0.4.5 两次发布分别挂在 `create failed - Resource
# busy` 和 `convert failed - Resource temporarily unavailable`，失败在不同步骤，是典型
# 的资源竞争而非固定 bug。CI 直接一步压出只读 dmg：不 attach、无中间可写映像，把竞争
# 面砍掉。本地打包不受影响，仍走下面的定制流程。
if [[ -n "${CI:-}" || -n "${GITHUB_ACTIONS:-}" ]]; then
  echo "  … CI 环境：跳过 Finder 窗口定制（无 GUI session，本来也不生效），直接压制只读 dmg"
  rm -f "$DIST/$APP_NAME.dmg"
  hdiutil create -volname "$VOL" -srcfolder "$STAGE" -fs HFS+ -format UDZO \
    -imagekey zlib-level=9 -ov "$DIST/$APP_NAME.dmg" >/dev/null
  rm -rf "$STAGE"
  echo ""
  echo "✅ 完成（CI 朴素 dmg）"
  echo "   应用：   $APP"
  echo "   分发件： $DIST/$APP_NAME.dmg"
  exit 0
fi

rm -f "$RW"
hdiutil create -volname "$VOL" -srcfolder "$STAGE" -fs HFS+ -format UDRW -ov "$RW" >/dev/null

DEV="$(hdiutil attach -readwrite -noverify -noautoopen "$RW" | grep -E '^/dev/' | head -1 | awk '{print $1}')"
MOUNT="/Volumes/$VOL"

# attach 到 Finder 认出这个卷中间有个空档，固定 sleep 1 机器一忙就可能不够——
# 轮询等挂载点真的出现，比赌一个固定时长靠谱。
for _ in $(seq 1 20); do
  [[ -d "$MOUNT" ]] && break
  sleep 0.5
done
if [[ ! -d "$MOUNT" ]]; then
  echo "✗ 挂载卷 $MOUNT 迟迟没出现，打包终止" >&2
  hdiutil detach "$DEV" -force >/dev/null 2>&1 || true
  exit 1
fi

# 有背景 tiff 才设背景，否则只做布局定制。
if [[ -f "$STAGE/.background/bg.tiff" ]]; then
  BG_CMD='set background picture of theViewOptions to file ".background:bg.tiff"'
else
  BG_CMD=''
fi

# AppleScript 定制挂载窗口：隐藏工具栏/状态栏、固定尺寸、图标摆位（app 左 / 应用程序 右）。
# 用 2>&1 >/dev/null 只留错误文本（osascript 正常路径没有 stdout 输出可看），
# 失败原因直接打出来，别再让"可能需要授权"这种猜测式提示背锅。
if ! OSA_ERR="$(osascript <<EOA 2>&1 >/dev/null
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
)"; then
  echo "  ⚠ Finder 定制失败，dmg 仍会正常产出，只是没有背景/图标摆位。原因：${OSA_ERR:-未知}"
  echo "    （常见原因：首次运行需在「系统设置→隐私与安全性→自动化」允许终端控制 Finder）"
fi

# .DS_Store（背景/布局/图标坐标全存这里）是 Finder 异步写盘的，AppleScript 的
# close 只是发了指令，不代表已经落盘。轮询等它出现、且连续两次读到的大小不变
# 再 sync + detach，否则赶上机器忙，写一半就被卸载，定制悄悄丢失且没有任何报错。
DS_STORE="$MOUNT/.DS_Store"
prev_size=-1
stable=0
for _ in $(seq 1 20); do
  if [[ -f "$DS_STORE" ]]; then
    cur_size="$(stat -f%z "$DS_STORE" 2>/dev/null || echo -1)"
    if [[ "$cur_size" == "$prev_size" && "$cur_size" -gt 0 ]]; then
      stable=$((stable + 1))
      [[ $stable -ge 2 ]] && break
    else
      stable=0
    fi
    prev_size="$cur_size"
  fi
  sleep 0.3
done
sync

hdiutil detach "$DEV" >/dev/null 2>&1 || { sleep 2; hdiutil detach "$DEV" -force >/dev/null 2>&1 || true; }

rm -f "$DIST/$APP_NAME.dmg"
hdiutil convert "$RW" -format UDZO -imagekey zlib-level=9 -o "$DIST/$APP_NAME.dmg" >/dev/null
rm -f "$RW"; rm -rf "$STAGE"

echo ""
echo "✅ 完成"
echo "   应用：   $APP"
echo "   分发件： $DIST/$APP_NAME.dmg"
