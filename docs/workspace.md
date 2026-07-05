# smelt workspace

基于 [GPUI](https://gpui.rs) 的个人工作台桌面应用：内嵌一个真正的终端，让你在自己的项目里跑
`claude code` 等交互式命令，后续叠加 smelt 的「数字分身」能力（instincts / brief / 会话监控）。

运行：

```bash
cargo run --bin workspace
```

> 注：GUI 是独立的 `workspace` 二进制，原有 `smelt` CLI 不受影响。

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
src/bin/workspace/
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

## 关键技术决策

- **runtime_shaders**：绕开完整 Xcode 依赖，是能在只装 CLT 的机器上构建的关键。
- **portable-pty + alacritty（仅状态机）**：与 GUI 集成比 alacritty 自带 event_loop 更干净，
  与 codux 的做法一致。
- **IME 走 `EntityInputHandler`**：中文等合成输入不经 `on_key_down`，必须实现输入处理器
  并在 paint 阶段用 `window.handle_input` 注册；同一 canvas 顺便记录网格原点供鼠标换算。
- **整行 `StyledText`**：解决「逐格定宽 span」带来的子像素抖动与截断，是终端网格渲染的正确姿势。

---

## 路线图

- [ ] **可点击链接**：识别 URL、下划线、`Cmd+点击`用浏览器打开（进行中）
- [ ] **每标签独立项目目录**：新建标签时可选不同项目
- [ ] **接 smelt 大脑**：
  - `~/.claude/projects/*.jsonl` 实时会话监控（token / 工具调用 / 进度）
  - instincts / brief 侧栏
- [ ] 更像 IDE：分屏、项目文件树、命令面板（`Cmd+K`）
