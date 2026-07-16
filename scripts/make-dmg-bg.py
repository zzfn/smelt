#!/usr/bin/env python3
"""生成 dmg 挂载窗口的背景图（@1x 640×400 + @2x 1280×800）。

深色渐变底 + 中间蓝色箭头（app → 应用程序）+ 标题，风格与 app 图标一致。

这里只画底图，两件事刻意不画：
- **图标标签**：由 Finder 自己渲染（软链直接命名成「应用程序」，见
  scripts/dmg-settings.py）。这里再画一份就会与 Finder 的标签错开叠成重影。
- **首次打开提示**：未签名 app 的放行方式随系统变（macOS 15 起 Apple 取消了
  Control-click 绕过，只能走「系统设置 → 隐私与安全性 → 仍要打开」）。背景图是
  烧死的，改一次得重新发版，这类会过时的信息放 GitHub Release 说明里
  （见 .github/workflows/release.yml）。

图标摆位定义在 dmg-settings.py：app 中心 (160, 210)，应用程序 (480, 210)。
另注：用户若在 Finder 开了标签页栏，它会吃掉约 28px 内容区高度、把底图往下挤，
所以别把元素贴着下沿画。

用法： make-dmg-bg.py <out_1x.png> <out_2x.png>
依赖： python3 + Pillow；中文用系统黑体。
"""
import sys
from PIL import Image, ImageDraw, ImageFont

W, H = 640, 400
# 字体只能选 Pillow(FreeType) 能按文件路径打开的。PingFang（macOS 默认 UI 中文字体）
# 用不了：它是系统私有字体，不以文件形式存在于 /System/Library/Fonts——实测 mdfind
# 全系统只有一份 .fontinfo 元数据，FreeType 拿不到字体文件，truetype() 直接抛
# cannot open resource。要用它只能走 CoreText，不值得为此拖进 PyObjC。
# Hiragino Sans GB 是系统自带的**简体中文**黑体（Hiragino Sans W0~W9 是日文字体，
# 汉字会出日文字形，别混用），CI runner 上同样存在。
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

    # 图标下方不画标签：Finder 会渲染真实文件名（Smelt / 应用程序），这里再画一份
    # 就会错开叠成重影。

    # 箭头：app → 应用程序（两图标之间）
    accent = (0x5A, 0x9C, 0xF0)
    ay = 205 * scale
    x0, x1 = 252 * scale, 392 * scale
    d.line([(x0, ay), (x1, ay)], fill=accent, width=max(2, 3 * scale))
    s = 13 * scale
    d.polygon([(x1, ay - s), (x1 + int(s * 1.5), ay), (x1, ay + s)], fill=accent)

    # 底部不画「首次打开」提示：放行方式随系统变（macOS 15 起 Control-click 绕过已
    # 被取消），背景图烧死改不了；且 Finder 开了标签页栏时底部会被挤出窗口。这类
    # 信息改放 GitHub Release 说明。

    img.save(out)
    print(f"   saved {out} ({w}×{h})")


if __name__ == "__main__":
    if len(sys.argv) != 3:
        print("用法： make-dmg-bg.py <out_1x.png> <out_2x.png>", file=sys.stderr)
        sys.exit(1)
    render(1, sys.argv[1])
    render(2, sys.argv[2])
