# smelt

Mac 上的 AI coding 驾驶舱：一个专为「同时指挥多个 CLI coding agent 干活」设计的桌面
工作台。基于 GPUI，内嵌真终端，多项目 × 多标签。

分层（改代码时别弄混）：
- **通用终端层**：外壳、分屏、快捷启动（Claude Code / Codex / Copilot 都有入口）。
- **状态感知层**：靠终端标题（OSC 0/2）+ OSC 9/777 通知 + 响铃，是终端协议而非某家私有
  格式，任何遵守约定的 agent 都能被识别。别在这层写死 Claude Code 假设。
- **Claude Code 专属**：用量统计、历史会话浏览，读 `~/.claude/projects/**/*.jsonl`
  （只有 `usage_stats.rs` / `session_history.rs` 该碰这份数据）。

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
