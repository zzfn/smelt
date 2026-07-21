# smelt

Mac 上的 AI coding 驾驶舱：一个专为「同时指挥多个 CLI coding agent 干活」设计的桌面
工作台。基于 GPUI，内嵌真终端，多项目 × 多标签。

分层（改代码时别弄混）：
- **通用终端层**：外壳、分屏、快捷启动（Claude Code / Codex / Copilot 都有入口）。
- **状态感知层**：靠终端标题（OSC 0/2）+ OSC 9/777 通知 + 响铃，是终端协议而非某家私有
  格式，任何遵守约定的 agent 都能被识别。别在这层写死 Claude Code 假设。
- **Claude Code 专属**：用量统计、历史会话浏览、记忆浏览，读 `~/.claude/projects/**`
  下的 `*.jsonl`（会话）和 `memory/*.md`（长期记忆）。只有 `usage_stats.rs` /
  `session_history.rs` / `claude_memory.rs` 该碰这份数据；项目路径→目录名的编码规则
  只有 `session_history.rs` 一份，别再复制。

## 二进制
- `smelt`（`crates/smelt/`）—— GUI 主程序，`cargo run --bin smelt`
- `smeltd`（`crates/smeltd/src/main.rs`）—— 终端持久化守护进程（类 tmux）：GUI 退出/
  崩溃不影响 shell 存活，重开 GUI 按会话 id reattach

## 技术栈
- Rust 2021，tokio async
- GPUI + gpui-component（桌面 UI，GPU 渲染）
- portable-pty + alacritty_terminal（内嵌终端：PTY + ANSI 状态机）
- reqwest（宠物 LLM 大脑等场景调用模型 API）
- anyhow（错误处理）

## 目录
- `crates/smelt-core/` —— GUI 与守护共用的无 UI 逻辑（终端文本提取、OSC 扫描、
  权限菜单解析、远程网关）。**这个 crate 不许引 GPUI**，守护侧编译速度靠这条底线
- `crates/smelt/` —— GUI：多标签终端、文件树、git diff 视图、桌面宠物
- `crates/smeltd/` —— 守护侧三个无 GUI 二进制（smeltd / gateway / smelt-notify），
  依赖树不含 GPUI，改守护不用等 GUI 依赖编译
- `docs/workspace.md` —— GUI 已实现功能与架构
- `docs/roadmap.md` —— 待做点子存档

## 原则
- 每步 cargo check 通过再继续
- 配置放 ~/.smelt/config.toml
