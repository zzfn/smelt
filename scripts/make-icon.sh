#!/usr/bin/env bash
# 从 assets/icon.svg 渲染前景 + 深石墨渐变圆角方块底，合成 macOS app 图标。
# 产出 assets/icon-1024.png（母图）和 assets/AppIcon.icns（打包用）。
# 依赖：rsvg-convert（brew install librsvg）、python3 + Pillow、系统 sips / iconutil。
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ASSETS="$ROOT/assets"
SVG="$ASSETS/icon.svg"
mkdir -p "$ASSETS"

command -v rsvg-convert >/dev/null 2>&1 || { echo "✗ 缺 rsvg-convert： brew install librsvg" >&2; exit 1; }
[ -f "$SVG" ] || { echo "✗ 找不到源图： $SVG" >&2; exit 1; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# 前景内容区大小（占 1024 的比例，越大越饱满）。
FG=720
echo "▶ 渲染前景 ${FG}×${FG} …"
rsvg-convert -w "$FG" -h "$FG" "$SVG" -o "$TMP/fg.png"

echo "▶ 合成圆角方块底 → 母图 1024 …"
python3 - "$TMP/fg.png" "$ASSETS/icon-1024.png" <<'PY'
import sys
from PIL import Image, ImageDraw
S = 1024
fg = Image.open(sys.argv[1]).convert("RGBA")
img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
pad = int(S * 0.09)
box = [pad, pad, S - pad, S - pad]
radius = int((S - 2 * pad) * 0.235)  # squircle 近似圆角

def lerp(a, b, t):
    return tuple(round(a[i] + (b[i] - a[i]) * t) for i in range(3))

# 竖直渐变底：顶部石墨 → 底部近黑
grad = Image.new("RGB", (1, S))
top, bot = (0x33, 0x34, 0x3d), (0x16, 0x16, 0x1b)
for y in range(S):
    grad.putpixel((0, y), lerp(top, bot, y / (S - 1)))
grad = grad.resize((S, S))

mask = Image.new("L", (S, S), 0)
ImageDraw.Draw(mask).rounded_rectangle(box, radius=radius, fill=255)
img.paste(grad, (0, 0), mask)

# 前景居中
fw, fh = fg.size
img.alpha_composite(fg, ((S - fw) // 2, (S - fh) // 2))
img.save(sys.argv[2])
print("   saved", sys.argv[2])
PY

echo "▶ 多尺寸 iconset → icns …"
ICONSET="$ASSETS/AppIcon.iconset"
rm -rf "$ICONSET"; mkdir -p "$ICONSET"
SRC="$ASSETS/icon-1024.png"
for sz in 16 32 128 256 512; do
  sips -z $sz $sz             "$SRC" --out "$ICONSET/icon_${sz}x${sz}.png"     >/dev/null
  sips -z $((sz*2)) $((sz*2)) "$SRC" --out "$ICONSET/icon_${sz}x${sz}@2x.png" >/dev/null
done
cp "$SRC" "$ICONSET/icon_512x512@2x.png"
iconutil -c icns "$ICONSET" -o "$ASSETS/AppIcon.icns"
rm -rf "$ICONSET"

echo "✅ 图标已生成：$ASSETS/AppIcon.icns（重新 make dist 即会打进包）"
