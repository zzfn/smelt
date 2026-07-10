# smelt workspace

基于 [GPUI](https://gpui.rs) 的个人工作台桌面应用：内嵌一个真正的终端，让你在自己的项目里跑
`claude` / `codex` / `gemini` 等交互式 agent，定位是「AI coding 驾驶舱」——多项目 × 多终端的外壳。

外壳本身与具体 agent 无关；在此之上叠加的 agent 会话监控 / 用量统计 / 历史会话浏览
（读 `~/.claude/projects/**/*.jsonl`）目前仅支持 Claude Code。

运行：

```bash
cargo run --bin workspace
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

## 快捷键

### 全局（挂在 `Workspace` 根节点 `on_key_down`，见 `main.rs`）

| 快捷键 | 作用 |
|---|---|
| `Cmd+K` | 命令面板 |
| `Cmd+B` | 切换侧栏 |
| `Cmd+[` / `Cmd+]` | 切换上/下一个会话 |
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

- [ ] **每标签独立项目目录**：新建标签时可选不同项目
- [ ] **会话监控层**：`~/.claude/projects/*.jsonl` 实时解析各终端 agent 状态
  （thinking / 等待输入 / 完成、token 用量、最近工具调用）
- [ ] 更像 IDE：分屏
