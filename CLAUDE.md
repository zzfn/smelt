# smelt

Mac 上的 AI coding 驾驶舱：一个专为「同时指挥多个 CLI coding agent 干活」设计的桌面
工作台。基于 GPUI，内嵌真终端，多项目 × 多标签。

终端外壳与具体 agent 无关（`claude` / `codex` / `gemini` 都能跑）；但会话状态监控、
用量统计、历史会话浏览三项靠解析 `~/.claude/projects/**/*.jsonl`，**目前仅支持
Claude Code**。改这三处时注意别把 Claude Code 专属假设泄漏到通用终端层。

## 二进制
- `workspace`（`src/bin/workspace/`）—— GUI 主程序，`cargo run --bin workspace`
- `smeltd`（`src/bin/smeltd.rs`）—— 终端持久化守护进程（类 tmux）：GUI 退出/崩溃
  不影响 shell 存活，重开 GUI 按会话 id reattach

## 技术栈
- Rust 2021，tokio async
- GPUI + gpui-component（桌面 UI，GPU 渲染）
- portable-pty + alacritty_terminal（内嵌终端：PTY + ANSI 状态机）
- reqwest（宠物 LLM 大脑等场景调用模型 API）
- anyhow（错误处理）

## 目录
- `src/bin/workspace/` —— GUI：多标签终端、文件树、git diff 视图、桌面宠物
- `src/bin/smeltd.rs` —— 终端持久化守护
- `docs/workspace.md` —— GUI 已实现功能与架构
- `docs/roadmap.md` —— 待做点子存档

## 原则
- 每步 cargo check 通过再继续
- 配置放 ~/.smelt/config.toml
