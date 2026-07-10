<div align="center">

<img src="assets/icon-1024.png" alt="smelt" width="128">

# smelt

**Mac 上的 AI coding 驾驶舱 —— 一个专为「同时指挥多个 CLI coding agent 干活」设计的桌面工作台。**

基于 [GPUI](https://gpui.rs) 的原生应用，内嵌真终端，多项目 × 多标签。
Claude Code、Codex、Gemini CLI……凡是跑在终端里的 agent，都能在这里并排看住。

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Release](https://img.shields.io/github/v/release/zzfn/smelt)](https://github.com/zzfn/smelt/releases)
[![Platform](https://img.shields.io/badge/platform-macOS%20(Apple%20Silicon)-lightgrey)](https://github.com/zzfn/smelt/releases)

> **状态**：working prototype，持续迭代中。

</div>

---

## 为什么

AI 插件让编辑器更聪明，但人还是那个敲键盘的苦力。当 agent 能独立跑完读代码、改代码、跑测试、
提交的整条链路，人的角色就该从「打字的人」变成「看导航、下指令的人」。

这时候需要的不是一个更聪明的编辑器，而是一个能同时看住好几个正在跑的 agent 的**驾驶舱**。
smelt 把终端——agent 真正干活的地方——变成主战场。

## 安装

从 [Releases](https://github.com/zzfn/smelt/releases) 下载 `Smelt.dmg`，拖进 Applications 即可。
应用内置在线更新，后续版本会自动检查并静默下载。

> 目前仅支持 **macOS（Apple Silicon）**。

## 功能

**工作台**（与具体 agent 无关）
- 多项目 × 多标签内嵌真终端，`claude`、`codex`、`gemini`、`vim`、`htop` 等交互式程序与全屏 TUI 都能跑
- 分屏：竖切 / 横切，一个会话里并排看多个 agent
- 命令面板（`Cmd+K`）、可折叠侧栏
- 「需要关注」角标同时出现在 Dock 图标和菜单栏常驻图标上，切走 smelt 也能瞥见

**深度集成**（目前仅 Claude Code）

下面三项靠解析 Claude Code 写在本地的 transcript（`~/.claude/projects/**/*.jsonl`）实现，
其它 agent 暂时只能当普通终端用：

- 会话状态监控：标出哪个会话在跑、哪个在等你输入
- 用量统计：按工具 / 模型 / 项目拆分 token 用量，含今日走势与活动热力图
- 历史会话浏览：翻看完整对话（只读）

**读写代码**
- 文件树 + 文件名/内容搜索，内置编辑器（tree-sitter 语法高亮、行号、`Cmd+S` 保存）
- Git diff 视图，字符级行内高亮
- 代码热力图：从 `git log` 提炼改动热点（改得越勤、越近，分数越高）

**其它**
- 终端会话持久化：GUI 退出或崩溃不影响 shell 存活，重开自动 reattach
- 桌面宠物：透明置顶浮窗，可选接 LLM 大脑（OpenAI 兼容协议）

终端本身支持完整 ANSI / xterm 256 色 / 24-bit 真彩、Nerd Font、中文输入法（IME）、
框选与双击选词、10000 行滚动回看、`Cmd+点击` 打开链接。

## 快捷键

| 快捷键 | 作用 |
|---|---|
| `Cmd+K` | 命令面板 |
| `Cmd+B` | 切换侧栏 |
| `Cmd+[` / `Cmd+]` | 上一个 / 下一个会话 |
| `Cmd+D` / `Cmd+Shift+D` | 竖切 / 横切分屏 |
| `Cmd+W` | 关闭当前 pane |
| `Cmd+S` | 保存文件（文件树页） |
| `Cmd+,` | 设置 |
| `Cmd+C` / `Cmd+V` | 复制选区 / 粘贴（终端内） |
| `Shift+PageUp` / `Shift+PageDown` | 翻滚历史缓冲 |

## 从源码构建

需要 Rust stable 与 macOS。**无需安装完整 Xcode**——项目通过 `gpui_platform` 的
`runtime_shaders` feature 把 Metal 着色器改到运行时编译，只装 Command Line Tools 即可。

```sh
cargo run --bin workspace   # 开发模式直接跑 GUI
make dist-build             # 编译 release 并打包出 dist/Smelt.dmg
make help                   # 查看全部构建目标
```

跑测试与类型检查：

```sh
cargo check --all-targets
cargo test
```

## 架构

仓库有两个二进制：

| 二进制 | 作用 |
|---|---|
| `workspace` | GUI 主程序（`src/bin/workspace/`） |
| `smeltd` | 终端持久化守护进程，类 tmux（`src/bin/smeltd.rs`） |

`smeltd` 由 GUI 按需自动拉起并托管（独立进程组，GUI 退出不波及），**不需要手动运行**。
它以字节流 + 重放 + 尺寸协商的方式为每个终端会话保活，重开 GUI 时按会话 id reattach。

详细架构与已实现功能清单见 [`docs/workspace.md`](docs/workspace.md)，
待做点子见 [`docs/roadmap.md`](docs/roadmap.md)。

## 技术栈

Rust 2021 · [GPUI](https://github.com/zed-industries/zed) + [gpui-component](https://github.com/longbridge/gpui-component)
· portable-pty（PTY）· alacritty_terminal（ANSI 状态机）· tokio · smol · similar（diff）
· notify（文件监听）· reqwest · anyhow

配置放在 `~/.smelt/`。

## 贡献

欢迎 issue 与 PR。提交前请确保 `cargo check --all-targets` 与 `cargo test` 通过，
commit message 遵循 [Conventional Commits](https://www.conventionalcommits.org/)。

## License

[MIT](LICENSE)
