"""dmgbuild 配置：Smelt.dmg 挂载窗口的外观（窗口尺寸 / 背景 / 图标摆位）。

由 scripts/package-mac.sh 调用。dmgbuild 用 exec() 执行本文件，这里拿不到 __file__，
所以路径一律走环境变量传入：
  SMELT_APP      —— 要打进 dmg 的 Smelt.app 路径
  SMELT_DMG_BG   —— 背景图（retina tiff）
  SMELT_VOL_ICON —— 卷图标（.icns）

图标坐标必须与 scripts/make-dmg-bg.py 画的箭头对齐：箭头从 app 指向应用程序，
两边坐标对不上箭头就指偏。
"""

import os

application = os.environ["SMELT_APP"]
appname = os.path.basename(application)

# 软链名直接用中文：Finder 会把它当文件名渲染在图标下方，于是「应用程序」这个中文
# 标签是真标签。别再让背景图画一份——两份错开叠着就是重影（v0.5.5 的实际毛病）。
APPS_LABEL = "应用程序"

# 只读压缩映像，zlib 最高压缩
format = "UDZO"
compression_level = 9

files = [application]
symlinks = {APPS_LABEL: "/Applications"}

background = os.environ["SMELT_DMG_BG"]

# 卷图标：不设的话挂载后 Finder 侧边栏/桌面是个通用白磁盘。
icon = os.environ["SMELT_VOL_ICON"]

# 640×428：内容区正好容下 640×400 的背景图，余下 28 是标题栏。
window_rect = ((200, 120), (640, 428))
icon_size = 128
# 13：与背景图标题的字号层级配套；dmgbuild 默认 16 在 128px 图标下偏重。
text_size = 13.0
icon_locations = {
    appname: (160, 210),
    APPS_LABEL: (480, 210),
}
