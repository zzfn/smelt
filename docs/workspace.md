# smelt workspace

基于 [GPUI](https://gpui.rs) 的个人工作台桌面应用：内嵌一个真正的终端，让你在自己的项目里跑
`claude` / `codex` / `gemini` 等交互式 agent，定位是「AI coding 驾驶舱」——多项目 × 多终端的外壳。

外壳与状态感知都与具体 agent 无关——状态靠终端标题（OSC 0/2）+ OSC 9/99/777 通知 + 响铃
读出来，是终端协议而非私有格式。只有用量统计与历史会话浏览要读
`~/.claude/projects/**/*.jsonl`，这两项仅支持 Claude Code。

运行：

```bash
cargo run --bin smelt
```

> 注：`smelt` 原有的 instincts 蒸馏 CLI（`observe`/`digest`/`merge` 等）已整体移除；
> 本仓库现在只有 `workspace`（GUI）和 `smeltd`（终端持久化守护）两个二进制。

---

## 技术栈

| 层 | 选型 | 说明 |
|----|------|------|
| GUI 框架 | **GPUI**（zed-industries/zed，锁定 commit `1d217ee`） | GPU 加速、Metal 渲染 |
| 组件库 | **gpui-component**（longbridge，Apache-2.0） | Dock/主题等，构建于同一 gpui |
| 终端后端 | **portable-pty** `0.9` | 起 `$SHELL` 子进程并拿到 PTY |
| 终端状态机 | **alacritty_terminal** `0.26` | ANSI 解析 + 网格模型 + 滚动缓冲 |
| 定时器 | **smol** `2` | 驱动网格定时快照重绘 |

**免完整 Xcode**：通过 `gpui_platform` 的 `runtime_shaders` feature 让 Metal 着色器改到
运行时编译，编译期不调 `xcrun metal`，只装 Command Line Tools 即可构建。

---

## 目录结构

```
crates/smelt/src/
├── main.rs           # Workspace：多标签管理器 + 应用入口
├── terminal_view.rs  # TerminalView：单个终端视图（渲染/输入/IME/选区/滚动/resize）
└── terminal.rs       # Terminal：终端后端（PTY + alacritty 状态机 + 颜色解析）
```

数据流：后台线程读 PTY 输出 → `vte` 解析器 → 更新共享的 alacritty `Term` 网格；
UI 线程每 30ms 对网格做快照并重绘。

---

## 已实现功能

### 工作台外壳
- **多标签 / 多终端**：每个标签是一个独立的 `TerminalView`（各自的 PTY、历史、焦点、状态）
- **标签栏**：点击切换、`+` 新建、`×` 关闭（至少保留一个）

### 历史会话页（Claude Code 专属）
页内有「会话 / 记忆」两个子页，共用「左列表 + 右详情」骨架，数据都在
`~/.claude/projects/<编码后的项目路径>/` 下：
- **会话**：`*.jsonl`，还原完整对话（只读浏览，不支持 resume），见 `session_history.rs`
- **记忆**：`memory/*.md`，Claude Code 攒下的长期记忆——每个 md 是一条（YAML
  frontmatter 的 `name`/`description` + markdown 正文），左列表显示标题和一句话描述，
  右侧渲染全文。见 `claude_memory.rs`。目录里的 `MEMORY.md` 是索引（内容 = 各条
  description 汇总），列出来纯属重复，故跳过。

### 终端能力
- **内嵌真终端**：能跑交互式程序与全屏 TUI（`claude`、`htop`、`vim` 等）
- **随窗口 resize**：网格行列跟随窗口大小，同步 alacritty 与 PTY（`SIGWINCH`）
- **精确字宽**：用 `text_system.layout_line` 量等宽字符实宽，列对齐准确
- **滚动回看**：10000 行历史缓冲，鼠标滚轮 / `Shift+PageUp`/`Shift+PageDown` 翻看
- **光标**：块状光标（反色），基于 `renderable_content`

### 颜色与渲染
- **完整配色**：Tokyo Night 16 色 ANSI + xterm 256 色 + 24-bit RGB
- **文本属性**：粗体、下划线、反色（INVERSE）
- **Nerd Font**：`JetBrainsMono Nerd Font Mono`，图标 / powerline / git 字形正常显示
- **整行渲染**：每行作为单个 `StyledText` + 多 `TextRun` 上色，整行只整形一次 ——
  拖选拆分不抖动、宽度精确不截断，且比逐格 span 更高效

### 输入
- **键盘输入**：特殊键（回车/退格/方向键/Home/End/Delete/Page）、Ctrl 组合（如 `Ctrl+C` = SIGINT）
- **中文输入（IME）**：实现 `EntityInputHandler` + `canvas` 注册；中文合成、可打印字符、
  空格统一走 IME 提交路径写入 PTY，`on_key_down` 只处理特殊键与 Ctrl 组合
- **复制粘贴**：`Cmd+C` 复制选区、`Cmd+V` 粘贴剪贴板

### 鼠标
- **框选**：拖拽选择，选区高亮
- **双击选词 / 三击选行**
- 坐标换算基于 `canvas` 记录的网格原点（`absolute inset_0`）+ 实测字宽

---

## 快捷键

### 全局（挂在 `Workspace` 根节点 `on_key_down`，见 `main.rs`）

| 快捷键 | 作用 |
|---|---|
| `Cmd+K` | 命令面板 |
| `Cmd+B` | 切换侧栏 |
| `Cmd+[` / `Cmd+]` | 循环切换当前会话内的 pane（分屏） |
| `Cmd+1` ~ `Cmd+9` | 跳到侧栏第 N 个会话 |
| `Cmd+D` | 竖切分屏（右侧并排） |
| `Cmd+Shift+D` | 横切分屏（下方堆叠） |
| `Cmd+W` | 关闭当前 pane（会话只剩一个 pane 时关掉整个会话） |
| `Cmd+S` | 保存文件（仅文件树页生效） |
| `Cmd+Shift+F` | 切换调试 HUD（右上角帧率 + 帧耗时） |
| `Cmd+Q` | 退出（`cx.bind_keys`，也在应用菜单） |
| `Cmd+,` | 打开设置窗口 |

### 终端内（`TerminalView` 聚焦时，见 `terminal_view.rs`）

| 快捷键 | 作用 |
|---|---|
| `Cmd+C` | 复制选区 |
| `Cmd+V` | 粘贴剪贴板 |
| `Shift+PageUp` / `Shift+PageDown` | 翻滚历史缓冲 |
| `Cmd+点击` | 打开光标处识别到的链接 |
| `Shift+Enter` | 在开了 kitty keyboard protocol 的 TUI 里换行而非提交（见下） |

---

## 关键技术决策

- **终端渲染必须按网格定位，不能交给文本排版器自由流**：终端是字符网格，第 N 列就得画在
  `N × cell_w`；字体的字形宽度只决定「字长什么样」，不决定「它在哪」。早先整行拼成一个
  `StyledText` 让排版器自由流，位置就由字体 advance 说了算——主字体 JetBrains Mono
  （advance 0.6em）没有中文字形，中文 fallback 到 PingFang（1.0em），而两格 = 1.2em，
  **每个中文字亏 0.2em**，误差沿行累积：Claude Code 输出的表格里含中文的行比纯 `─` 横线
  短，`│` 一路往左漂。（当时的对策是让鼠标命中测试反过来「复现这个歪几何」，治标不治本。）
  现改为照 Zed 终端（同 GPUI 栈）的做法：`render_row` 按「样式相同 + 列号连续」把一行切成
  若干批，每批绝对定位在 `起始列 × cell_w`。宽字符第二格是 `'\0'` 占位，跳过它但列号照常
  前进，后面的字符便「对不上列号」而断批。
  **关键性质：宽字符永远落在批尾**（它后面必有占位格 → 下一个字符必断批），所以批内除
  末位外全是 advance == cell_w 的窄字符，位置天然正确；宽字符那点亏空后面再无字符可推。
  顺带收掉了三个补丁：`pos_to_cell` 的「重新整形反查列号」workaround（现在 `col = x/cell_w`
  直接成立）、IME 候选框与渲染互相矛盾的定位、以及「截断行尾防自动折行」。
- **Shift+Enter 与 kitty keyboard protocol**：遗留终端编码里 Enter 只有 `\r` 一种字节，
  Shift/Alt/Ctrl+Enter 全塌缩成同一个 `\r`，修饰键信息在协议层就丢了——所以「Shift+Enter
  换行、Enter 提交」不是 UI 能自己决定的事。kitty keyboard protocol 用 CSI u 编码补回修饰键
  （Shift+Enter → `ESC[13;2u`），TUI 进入时发 `CSI > 1 u` 开启，alacritty_terminal 会解析并置
  `TermMode::DISAMBIGUATE_ESC_CODES`。`keystroke_to_bytes` 据此分流：**开了才发 CSI u，没开
  一律发 `\r`**。这个 gate 不能省——bash/zsh 不认 CSI u，硬发会被 readline 当文本吐出 `[13;2u`。
  Claude Code 从 v2.1 起会主动开；老版本或不开协议的 TUI 还有 Alt+Enter（回退到 meta 前缀
  `ESC`+`CR`）这条传统通道。
  两个坑：① alacritty 的 `Config::kitty_keyboard` **默认 false**，关着时它会把 `CSI > 1 u`
  在 `push_keyboard_mode` 里静默 return 掉，mode 位永远置不上——建 `Term` 时必须显式开。
  ② 发送侧目前只给 Enter 编了 CSI u，其余键（Escape、Ctrl+字母、带修饰的方向键）在协议开着时
  仍发遗留编码，不是完整的 level-1 实现。够用是因为 Claude Code 本来就得兼容 iTerm2 那种
  「只把 Shift+Enter 映射成 CSI u、其余照旧」的手工配置；真要补全得照搬 alacritty 的
  `build_key_sequence`。
- **runtime_shaders**：绕开完整 Xcode 依赖，是能在只装 CLT 的机器上构建的关键。
- **portable-pty + alacritty（仅状态机）**：与 GUI 集成比 alacritty 自带 event_loop 更干净，
  与 codux 的做法一致。
- **IME 走 `EntityInputHandler`**：中文等合成输入不经 `on_key_down`，必须实现输入处理器
  并在 paint 阶段用 `window.handle_input` 注册；同一 canvas 顺便记录网格原点供鼠标换算。
- **整行 `StyledText`**：解决「逐格定宽 span」带来的子像素抖动与截断，是终端网格渲染的正确姿势。
- **桌面宠物窗口层级用 floating，不用 popup**：`WindowKind::PopUp` 默认落在
  `NSPopUpWindowLevel`（系统弹出菜单/提示条的层级，全局数一数二高）。宠物这种非激活、
  常驻可见的面板留在这个层级，会被 AppKit 当成「进程里层级最高的窗口」去参与激活判断，
  导致切到主窗口时闪一下又被切回去。手动把 level 降到 `NSFloatingWindowLevel` 才不会
  截胡主窗口的正常激活流程（见 `pet.rs` 的 `strip_native_chrome`）。
- **IME 的 `selectedRange` 必须返回有效折叠区间，不能恒为 `None`**：一直返回 `None` 会
  桥接成 AppKit 的 `{NSNotFound, 0}`，等于告诉系统「这里没有文字光标」——系统切换输入法
  时的提示气泡就不会出现（候选窗本身走 `hasMarkedText`/`setMarkedText`，不受影响）。

---

## 路线图

- [ ] **每标签独立项目目录**：新建标签时可选不同项目
- [ ] **会话监控层**：`~/.claude/projects/*.jsonl` 实时解析各终端 agent 状态
  （thinking / 等待输入 / 完成、token 用量、最近工具调用）
- [ ] 更像 IDE：分屏
