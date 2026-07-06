#!/usr/bin/env python3
"""生成 dmg 挂载窗口的背景图（@1x 640×400 + @2x 1280×800）。

深色渐变底 + 中间蓝色箭头（app → 应用程序）+ 引导文字，风格与 app 图标一致。
图标摆位由 package-mac.sh 的 AppleScript 决定，需与这里的标签位置对齐：
  app 图标中心 (160, 210)，应用程序 (480, 210)，标签画在其正下方。

用法： make-dmg-bg.py <out_1x.png> <out_2x.png>
依赖： python3 + Pillow；中文用系统 PingFang 字体。
"""
import sys
from PIL import Image, ImageDraw, ImageFont

W, H = 640, 400
# PingFang.ttc 在 Pillow 下打不开（会 fallback 成方框），改用同样内置的黑体。
CN_FONT = "/System/Library/Fonts/Hiragino Sans GB.ttc"


def lerp(a, b, t):
    return tuple(round(a[i] + (b[i] - a[i]) * t) for i in range(3))


def font(size, scale):
    try:
        return ImageFont.truetype(CN_FONT, size * scale)
    except Exception:
        return ImageFont.load_default()


def render(scale, out):
    w, h = W * scale, H * scale
    # 竖直渐变（1px 宽拉伸，避免逐像素慢）
    top, bot = (0x22, 0x23, 0x2B), (0x14, 0x14, 0x19)
    col = Image.new("RGB", (1, h))
    for y in range(h):
        col.putpixel((0, y), lerp(top, bot, y / (h - 1)))
    img = col.resize((w, h))
    d = ImageDraw.Draw(img)

    # 标题
    title = "拖到「应用程序」即可安装"
    ft = font(20, scale)
    tw = d.textlength(title, font=ft)
    d.text(((w - tw) / 2, 46 * scale), title, font=ft, fill=(0xE6, 0xE6, 0xEA))

    # 图标下方标签（与 AppleScript 图标中心 x 对齐）
    fl = font(13, scale)
    for cx, label in ((160, "Smelt"), (480, "应用程序")):
        lw = d.textlength(label, font=fl)
        d.text((cx * scale - lw / 2, 298 * scale), label, font=fl, fill=(0x9A, 0x9A, 0xA4))

    # 箭头：app → 应用程序（两图标之间）
    accent = (0x5A, 0x9C, 0xF0)
    ay = 205 * scale
    x0, x1 = 252 * scale, 392 * scale
    d.line([(x0, ay), (x1, ay)], fill=accent, width=max(2, 3 * scale))
    s = 13 * scale
    d.polygon([(x1, ay - s), (x1 + int(s * 1.5), ay), (x1, ay + s)], fill=accent)

    # 底部放行提示（未签名 app）
    tip = "未签名 · 首次打开请右键点 Smelt → 选「打开」"
    fp = font(11, scale)
    pw = d.textlength(tip, font=fp)
    d.text(((w - pw) / 2, 360 * scale), tip, font=fp, fill=(0x60, 0x60, 0x6A))

    img.save(out)
    print(f"   saved {out} ({w}×{h})")


if __name__ == "__main__":
    if len(sys.argv) != 3:
        print("用法： make-dmg-bg.py <out_1x.png> <out_2x.png>", file=sys.stderr)
        sys.exit(1)
    render(1, sys.argv[1])
    render(2, sys.argv[2])
