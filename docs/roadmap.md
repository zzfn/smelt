# smelt Roadmap / Backlog

待做点子的存档，做完了就从这里挪走或标 ✅。

## 系统感知能力（OS perception）

宠物 / 会话监控可以更「懂」用户当下在干嘛。按「获取难度 + 权限成本」分档：

### ✅ 已做：只读、零权限
直接用 `objc` 发消息，不需任何系统权限：
- `[NSEvent mouseLocation]` —— 全屏鼠标位置（宠物眼睛 / 身体跟随、AFK 检测）
- `[NSWorkspace frontmostApplication].localizedName` —— 当前前台 app 名（宠物切 app 评论 + 喂给大脑当上下文）

**未用任何 `enigo` / `tfc`（输入模拟）或 `xcap` / `scrap`（截屏）库**；`cocoa` / `core-graphics` 只是 gpui 的间接依赖，自有代码没碰。

### 🔲 输入模拟（enigo / tfc）
让 smelt / 宠物能**替用户操作**：自动点按钮、宠物真的用爪子拖窗口、快捷宏、一键把某段文本敲进当前输入框。
- 成本：加一个 crate（enigo 跨平台），无需特殊权限（但受辅助功能限制的某些操作除外）
- 风险：误触、抢焦点；要非常克制，最好用户显式触发
- 适合：给宠物加「叼着东西放到你光标处」这类**用户主动发起**的趣味动作

### 🔲 屏幕捕获（xcap / scrap）
让宠物 / agent **看见屏幕**：
- 会话卡片实时缩略图（现在总览是文字预览，可换成真·画面缩略）
- 宠物「瞄一眼」你在干嘛（截当前窗口喂多模态模型）
- ⚠️ 需要**屏幕录制权限**（首次弹窗授权），比读 app 名重得多；耗性能，要节流
- 建议：真要做先做「会话缩略图」这个明确有用的，别泛泛截全屏

### 🔲 更深的上下文（需辅助功能权限）
拿「浏览器具体在看哪个网址」「文档标题」——要开**辅助功能 (Accessibility)** 权限读 AX 树。收益不确定、隐私敏感，暂不碰。

---

## 会话持久化增强（smeltd）

### ✅ 根治 Ctrl+C 重连错位：daemon 侧常驻 Term，attach 时吐网格快照

**已落地（完整快照）**（`src/bin/smeltd.rs`）：
1. 每会话常驻 `alacritty_terminal::Term`（history 1 万行），PTY 泵 `parser.advance`（`catch_unwind`）。
2. attach 时 `snapshot_ansi`：**scrollback（上限 1 万行）+ 可视区**自洽 ANSI；备用屏、SGR、
   软换行、OSC 8、光标形状/显隐、bracketed paste / app cursor / 鼠标 / focus 等模式恢复。
3. resize 同步常驻 Term；GUI 协议不变（`replay_len` + 字节流）。
4. 原始缓冲 256KB 仅供 upgrade 交接 best-effort 重建 + jolt。

**仍可增强**：
- 实机验证：长 detach + Claude Code Ctrl+C。
- 守护进程崩溃后的落盘恢复（与 reattach 正确性无关）。

可借鉴 codex 的做法落盘会话：
- 会话历史写 JSONL（`~/.smelt/sessions/<date>/<uuid>.jsonl`，每行带时间戳的 typed 事件）
- SQLite 做索引，支持会话列表分页 / 全文搜索（`rusqlite` 依赖已随 instincts 蒸馏链路一起
  移除，真要做这条得重新加回来）
- 好处：机器 / 守护重启后可恢复完整历史、总览页能列历史会话
- 注：**不需要** PostgreSQL/TimescaleDB —— 本项目栈是 SQLite，本地单机 IPC 场景 SQLite 足够

## 其他 codex 借鉴项（详见调研）
- smeltd 加 JSON-RPC 结构化控制通道：会话状态从「解析字节流猜」升级为「协议事实」
- Claude Code hook → `smelt-notify` 小工具直写 smeltd socket（比解析 OSC 更可靠的第二信源）
- 会话卡片「运行了多久」计时、token 用量 / 上下文余量展示

## AgentHub 借鉴项（同类 macOS app，jamesrochabrun/AgentHub）

### 🔲 会话监控：用 watcher 而非轮询，五态状态机
验证过的实现路径，跟 smelt 已有的「agent 状态五态细分」（`1dd36fe`）思路一致，做「会话监控层」
（解析 `~/.claude/projects/*.jsonl`）时可直接抄：
- 文件系统 watcher 驱动，不轮询
- 状态划分：`Thinking / Executing Tool / Awaiting Approval / Waiting for User / Idle`

### 🔲 Git worktree 集成到 UI
比「每标签独立项目目录」这条更进一步，不只是选目录：
- 在 UI 里建 / 删 sibling worktree、在新分支上直接开会话
- **Remix**：把当前会话 remix 到一个隔离 worktree 继续跑，可切换 provider（Claude ↔ Codex），
  原会话的完整 transcript 作为上下文传给新会话——同一个任务想换个 provider 或分支试错，
  不用从头给上下文

### 🔲 交互式 diff（不只是只读 diff 视图）
smelt 现在的 git diff 视图是看的；AgentHub 能在 diff 里选中改动、写行内评论，批量发回给
agent 会话继续处理。把「审查」和「指挥」打通，符合 smelt「驾驶舱」定位——人看导航、下指令，
不是自己去改代码。落地大概分两步：
- diff 视图支持行内选区 + 评论输入（UI 层，在现有 `similar` 字符级 diff 基础上加交互）
- 评论批量打包，通过 smeltd 写回对应会话的 PTY（或未来的结构化控制通道）

### 不抄的一点
AgentHub 卡片移除时会杀掉 shell 进程树防孤儿进程——跟 smeltd「GUI 退出/崩溃不影响 shell
存活」的核心设计哲学相反，不借鉴。MCP Apps / iOS Simulator / Storybook 等偏 Swift 移动端
生态的功能，跟 smelt 定位不搭，跳过。
- macOS Seatbelt 沙箱（`sandbox-exec` + SBPL 模板）跑 agent 命令

## 宠物
- 鼠标很近时凑上来 / 划过身体害羞挤压（距离 / 接触反应）
- Stage 3：打字跟宠物多轮对话（输入框 + 对话历史）

---

## 终端渲染性能：P0 已解决空闲开销，调试 HUD 的帧率是结构性成本

**✅ 已做（P0）**：`TerminalView` 的 30ms 定时器以前无条件 `cx.notify()`，导致哪怕 shell
完全空闲也以 33 次/秒的频率重画。改用 alacritty 自带的 `Term::damage()` 判断内容是否
真变化，没变就跳过。实测：空闲终端 5 秒内 `render()` 从理论上的 165 次降到 8 次。
过程中隔离测试抓到一个真 bug——`damage_cursor()` 无条件标记光标格，必须排除"仅光标
那一格"的脏区才算数，见 `terminal.rs` 的 `damage_gate_tests`。

**🔍 调试 HUD（Cmd+Shift+F）测出的 30-40 FPS 不会因为 P0 变化，原因已查清**：
- HUD 用 `window.request_animation_frame()` 强制 Workspace 每帧重画；GPUI 的
  `window.refreshing` 机制会让同一帧里所有被摸到的实体（包括 TerminalView）跟着
  强制重画，绕开各自的 dirty 判断——这是 GPUI `view.rs` 里 prepaint 复用逻辑的一部分
  （`!dirty_views.contains && !refreshing` 才复用），不是 P0 没生效
- 实测过"啥都不动"和"`yes` 持续刷屏"两种场景下 HUD 帧率相近，证实这个数字取决于
  **屏幕上有多少行要重新布局+绘制**，跟内容是否变化无关
- 已用真实分段计时排除了 `snapshot()`（重负载下 median 0.8ms，可忽略）；GPUI 自带
  `LineLayoutCache` 大概率已经在做"内容没变就不重新整形文字"（按 text+font+runs 内容
  跨帧复用，见 `text_system/line_layout.rs`）——原计划的"按行内容哈希缓存"（codux 同款
  方案）大概率是重复造轮子，故未实施
- 剩下的成本只能是 **Taffy 给 ~50 行 Div 做布局 + GPU 绘制提交**，这是当前"每行一个
  独立 Div 交给 Taffy 排布"这种渲染方式的结构性成本，测不到更细（裸调 GPUI 元素构造
  函数脱离真实 App/Window 上下文会直接让 rustc 编译崩溃，这条路不安全）

**🔲 若以后要继续优化**：只有真在日常使用（非 HUD 极端测试）里明显感觉到"疯狂刷屏时卡"
才值得——需要绕开 `Div`/`Taffy`，自己写一个手动整形+绘制的终端元素（类似 codux 自己
写的 `TerminalElement`），是比 P0 大得多的重写，收益未知，先不做。
