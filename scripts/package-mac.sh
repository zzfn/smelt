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
BIN_NAME="smelt"              # cargo 产物名
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

# 远程 H5：手机端 CLI 面板（Preact + Tailwind + xterm）。
# 必须打进 App Resources；否则 smeltd 找不到 SPA，会回退旧 HTML，移动端样式全乱。
REMOTE_WEB_DIST="$ROOT/remote-web/dist"
if [[ ! -f "$REMOTE_WEB_DIST/index.html" ]]; then
  if command -v npm >/dev/null 2>&1; then
    echo "▶ 构建 remote-web（npm run build）…"
    (cd "$ROOT/remote-web" && npm install && npm run build)
  else
    echo "✗ 缺少 remote-web/dist，且本机无 npm。请先：cd remote-web && npm install && npm run build" >&2
    exit 1
  fi
fi
if [[ ! -f "$REMOTE_WEB_DIST/index.html" ]]; then
  echo "✗ remote-web 构建失败：没有 $REMOTE_WEB_DIST/index.html" >&2
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

# 远程 H5 → Contents/Resources/remote-web（smeltd 运行时按 current_exe 解析）
echo "▶ 拷贝 remote-web → Resources …"
rm -rf "$RES/remote-web"
mkdir -p "$RES/remote-web"
# dist 内容（index.html + assets/）直接落在 remote-web/ 下
cp -R "$REMOTE_WEB_DIST"/. "$RES/remote-web/"
if [[ ! -f "$RES/remote-web/index.html" ]]; then
  echo "✗ 拷贝后缺少 $RES/remote-web/index.html" >&2
  exit 1
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
# 挂载后是一个固定尺寸、带背景箭头的窗口，把 app 拖到「应用程序」即完成安装。
#
# 这些定制（窗口尺寸 / 背景 / 图标坐标）最终只落在卷根目录的 .DS_Store 一个文件里。
# dmgbuild 靠 ds_store + mac_alias 直接把它写出来，全程不碰 Finder，因此本地与 CI
# （headless、没有 Finder 自动化授权）走的是同一条路径——本地打出来什么样，Release
# 就是什么样。旧版靠 AppleScript 指挥 Finder 定制，CI 上做不了只能整段跳过，发出去的
# 一直是没有背景和图标摆位的朴素 dmg。
#
# 换掉 AppleScript 顺带消掉两个旧坑：
#   - 旧脚本认死卷名做定制，撞上同名残留卷（上次没 detach 干净）会把新盘挤成
#     "Smelt 1"、定制悄悄写到旧盘上；dmgbuild 读 hdiutil 返回的真实挂载点，天然没
#     这个问题，那段「清理残留卷」也就不必要了（它还会误弹同名外置盘）。
#   - .DS_Store 由 Finder 异步写盘，旧脚本得轮询等它大小稳定才敢 detach；现在是同步
#     写文件，等待逻辑一并删掉。

# 打包工具链装进独立 venv：不污染系统 python，也绕开 PEP 668 externally-managed
# 限制（CI runner 的 python 多半是 Homebrew 装的，直接 pip install 会被拒）。
# 已经装好就复用（dist/ 在 .gitignore 里，不入库）。
#
# PyPI 在国内/部分网络会 ReadTimeout（卡在 mac-alias 等依赖）。支持：
#   PIP_INDEX_URL=https://pypi.tuna.tsinghua.edu.cn/simple make dist-build
# 未设置时：先官方，失败再自动换清华镜像。
VENV="$DIST/.dmgvenv"
if [[ ! -x "$VENV/bin/dmgbuild" ]]; then
  echo "  … 准备打包工具链（dmgbuild + Pillow）"
  rm -rf "$VENV"
  python3 -m venv "$VENV"
  # 拉长超时，避免默认 15s 被掐断
  export PIP_DEFAULT_TIMEOUT="${PIP_DEFAULT_TIMEOUT:-120}"
  pip_base=( "$VENV/bin/pip" install --upgrade )
  # 用户指定镜像则只走一条；否则官方 → 清华
  if [[ -n "${PIP_INDEX_URL:-}" ]]; then
    echo "  … pip 使用 PIP_INDEX_URL=$PIP_INDEX_URL"
    "${pip_base[@]}" pip
    "${pip_base[@]}" "dmgbuild==1.6.7" "Pillow>=10"
  else
    if ! "${pip_base[@]}" --quiet pip \
      || ! "${pip_base[@]}" "dmgbuild==1.6.7" "Pillow>=10"; then
      echo "  ⚠ 官方 PyPI 失败，改用清华镜像重试 …"
      MIRROR="https://pypi.tuna.tsinghua.edu.cn/simple"
      "${pip_base[@]}" -i "$MIRROR" --trusted-host pypi.tuna.tsinghua.edu.cn pip
      "${pip_base[@]}" -i "$MIRROR" --trusted-host pypi.tuna.tsinghua.edu.cn \
        "dmgbuild==1.6.7" "Pillow>=10"
    fi
  fi
  [[ -x "$VENV/bin/dmgbuild" ]] || {
    echo "✗ 安装 dmgbuild 失败。可手动：" >&2
    echo "  PIP_INDEX_URL=https://pypi.tuna.tsinghua.edu.cn/simple make dist-build" >&2
    exit 1
  }
  echo "  ✓ dmgbuild 已就绪"
fi

# 背景图：@1x + @2x 合成 retina 多分辨率 tiff，retina 屏上才不糊。
# Pillow 就在上面那个 venv 里，所以这里不再容错——过去是「缺 Pillow 就静默退化成
# 无背景」，而 CI runner 恰恰没有 Pillow，等于永远没背景还不报错。现在直接报错。
BG1="$DIST/.dmgbg.png"; BG2="$DIST/.dmgbg@2x.png"; BG_TIFF="$DIST/.dmgbg.tiff"
rm -f "$BG_TIFF"
"$VENV/bin/python" "$ROOT/scripts/make-dmg-bg.py" "$BG1" "$BG2" >/dev/null
tiffutil -cathidpicheck "$BG1" "$BG2" -out "$BG_TIFF" >/dev/null
rm -f "$BG1" "$BG2"
[[ -f "$BG_TIFF" ]] || { echo "✗ 背景图生成失败，没产出 $BG_TIFF" >&2; exit 1; }

VOL="$APP_NAME"
rm -f "$DIST/$APP_NAME.dmg"

# dmgbuild 内部仍要 attach 一个可写映像来放文件，hdiutil 的挂载/卸载在 runner 上会
# 偶发撞车——v0.4.5 两次发布分别挂在 `create failed - Resource busy` 和 `convert
# failed - Resource temporarily unavailable`，失败在不同步骤，是典型的资源竞争而非
# 固定 bug。竞争面消不掉，用重试兜住。
export SMELT_APP="$APP"
export SMELT_DMG_BG="$BG_TIFF"
export SMELT_VOL_ICON="$ROOT/assets/AppIcon.icns"
built=0
for attempt in 1 2 3; do
  if "$VENV/bin/dmgbuild" -s "$ROOT/scripts/dmg-settings.py" "$VOL" "$DIST/$APP_NAME.dmg"; then
    built=1
    break
  fi
  echo "  ⚠ 第 ${attempt}/3 次打 dmg 失败（多半是 hdiutil 挂载竞争），3s 后重试 …"
  hdiutil detach "/Volumes/$VOL" -force >/dev/null 2>&1 || true
  sleep 3
done
rm -f "$BG_TIFF"
[[ "$built" == 1 ]] || { echo "✗ dmg 打包连续 3 次失败" >&2; exit 1; }

echo ""
echo "✅ 完成"
echo "   应用：   $APP"
echo "   分发件： $DIST/$APP_NAME.dmg"
