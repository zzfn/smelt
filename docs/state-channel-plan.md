# 状态通道落地方案（联机的第一块砖）

配套 [collaboration.md](collaboration.md) 的设计讨论。那份讲「做什么」，这份讲「改哪些函数」。

**为什么先做这个**：观战席 / 远程遥控 / 作战地图 / 联机 review / 任务分发——五个功能里
四个不需要画面通道，但**五个全都需要状态通道**。而画面通道（`snapshot_ansi` + 字节流）
已经有了，状态通道恰恰是缺的那个。

它**单机就能验证**：做完之后会话总览立刻从「靠 OSC 猜」变成「读事实」，哪怕联机一行不写
也不亏。

---

## 现状：三个卡点（都是读代码确认过的事实，不是推测）

### 卡点 1：守护对 agent 状态一无所知

```rust
// smeltd.rs:64
use alacritty_terminal::event::VoidListener;
// smeltd.rs:209
term: Mutex<Term<VoidListener>>,
```

守护侧常驻的 `Term` 挂的是 **`VoidListener`——所有 alacritty 事件被直接丢弃**，包括
`Event::Title`（OSC 0/2）和 `Event::Bell`。守护现在只是一根哑字节管道 + 网格快照器。

所有状态推断都压在 GUI 侧（`terminal.rs:283` 的 `EventProxy` 才是真 listener）。GUI 一关，
状态就没人算了——**这与「会话脱离 GUI 存活」的核心设计直接矛盾**。

### 卡点 2：守护 → GUI 是裸字节流，没有帧

- **GUI → 守护**：有帧。`write_frame(w, ty, payload)`（`terminal.rs:632`），
  `type 0 = 键盘输入`，`type 1 = resize`（协议见 `smeltd.rs:26-30`）
- **守护 → GUI**：**无帧**。一行 JSON 尺寸头 + ANSI 快照 + 之后**全是裸 PTY 字节**
  （`smeltd.rs:776` 直接 `c.write_all(chunk)`）

所以状态事件**没有地方塞**。两条出路：

| 方案 | 改动 | 风险 |
|---|---|---|
| A. 读方向加帧类型 | 要改 open 流模式 + 协议版本协商 | 动了三条不变量，破坏兼容 |
| **B. 新增 `subscribe` op，单开一条状态连接** | **完全不碰 open 流模式** | **零** |

**选 B。** 作战地图 / 远程遥控 / 收件箱根本不需要字节流，只订状态；观战席需要两者时，
开两条连接即可。不动 `handle_open`，那三条不变量（id 立即落盘 / attach 先回报尺寸 /
同锁串行）一条都不用碰。

### 卡点 3：hook 无法知道自己属于哪个会话 ← 整条 hook 链路的地基

```rust
// smeltd.rs:680
fn spawn_session(rows: u16, cols: u16, cwd: Option<&str>, launch: Option<&str>)
```

`spawn_session` **连 session id 都没收到**，更没有往 shell 环境里注入。于是 Claude Code
的 hook 脚本跑起来时，**没有任何办法知道自己在哪个 smelt 会话里**，也就无从上报。

必须加 `cmd.env("SMELT_SESSION_ID", id)`。**这一行是「协议事实」这条路的入口**，没有它，
后面全都是空中楼阁。

---

## Schema（定死，三个信源都往里灌）

```rust
#[derive(Clone, Default, serde::Serialize)]
struct SessionState {
    id: String,
    cwd: Option<String>,          // ⚠️ Ctl 现在没存，要补（handshake 收到过就丢了）
    launch: Option<String>,       // claude / codex / copilot（LaunchKind 的来源）
    title: Option<String>,        // OSC 0/2，Term 事件填
    phase: Phase,
    phase_since: u64,             // unix 秒——「空转半小时」靠它
    pending_question: Option<String>,  // 远程遥控的命脉
    tokens_used: Option<u64>,
    branch: Option<String>,
    dirty_files: Vec<String>,     // 撞车预警靠它
    updated_at: u64,
}

#[derive(Clone, Copy, Default, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Thinking,
    ExecutingTool,
    AwaitingApproval,
    WaitingForUser,
    #[default]
    Idle,
    Dead,
}
```

对照 GUI 现有的 `AgentStatus`（`main.rs:203`，五态 `WaitingApproval / NeedsAttention /
Running / Done / Idle`）：那是个**渲染用的派生值**（每帧现算、按优先级折叠多个 pane），
不是会话的原子状态。两者不是一回事，**保留 `AgentStatus` 作为 UI 层的派生**，让它改从
`Phase` 算，而不是从 OSC 猜。

### 三个信源，按可信度覆盖

| 信源 | 可信度 | 现状 |
|---|---|---|
| Claude Code hook → 直写 socket | **事实**（agent 亲口说的） | ❌ 不存在，本方案新增 |
| OSC 9/777 + BEL | 中（终端协议，通用） | ✅ GUI 侧有（`terminal.rs:364` `OscScan`），守护侧没有 |
| OSC 0/2 标题（Braille spinner 前缀） | 低（纯猜） | ✅ GUI 侧有（`terminal_view.rs:238`） |

---

## 改动清单（精确到函数）

### 第 1 步：守护自己能看见状态

**1.1 `VoidListener` → `StateListener`**（`smeltd.rs`）

```rust
#[derive(Clone)]
struct StateListener {
    state: Arc<Mutex<SessionState>>,
}

impl EventListener for StateListener {
    fn send_event(&self, event: Event) {
        let Ok(mut st) = self.state.lock() else { return };
        match event {
            Event::Title(t) => {
                // Braille spinner 前缀 = 正在跑（规则目前在 terminal_view.rs:238，
                // 别再复制第四份，抽成共享函数）
                st.title = Some(t);
                st.touch();
            }
            Event::Bell => st.mark_attention("🔔 响铃"),
            _ => {}
        }
    }
}
```

**连带改动**（`Term<VoidListener>` 是全文类型）：
- `smeltd.rs:197` `new_daemon_term() -> Term<StateListener>`（要多收一个 state 参数）
- `smeltd.rs:209` `Session.term: Mutex<Term<StateListener>>`
- `smeltd.rs:340` `feed_term()` 签名
- `smeltd.rs:842` `snapshot_ansi(term: &Term<VoidListener>)` → **泛型化**
  `fn snapshot_ansi<T: EventListener>(term: &Term<T>)`，这样测试里继续用 `VoidListener`，
  15 个快照测试一行不用改
- `resume_handoff`（`smeltd.rs:313`）里建 Term 的地方

**1.2 `Session` 加两个字段**（`smeltd.rs:205`）

```rust
struct Session {
    ctl: Mutex<Ctl>,
    out: Mutex<Out>,
    term: Mutex<Term<StateListener>>,
    state: Arc<Mutex<SessionState>>,       // 新
    subscribers: Mutex<Vec<UnixStream>>,   // 新：订阅状态的连接
}
```

**锁序**（现有是 `term → out`，必须守住）：`state` 与 `subscribers` 都是**叶子锁**——
持有它们时不得再去拿 `term`/`out`/`sessions`。广播时先把待发 JSON 在 state 锁内拼好、
放掉 state 锁，再拿 subscribers 锁写 socket。

**1.3 `Ctl` 补 `cwd`**（`smeltd.rs:114`）

`handle_open` 在 `smeltd.rs:465` 收到过 `cwd`，spawn 完就丢了。作战地图要它，补一个字段
存下来即可（**注意：这是 spawn 时的静态目录，不跟随 shell 的 `cd`**——真实 cwd 要 OSC 7，
GUI 的 `OscScan` 也没解析它，另做）。

**1.4 OSC 9/777 扫描搬进守护**

GUI 的 `OscScan`（`terminal.rs:364-416`）逻辑要在守护侧也有一份——但**不要复制**
（CLAUDE.md 明令）。抽到共享模块（新建 `src/osc.rs`，两个 bin 都 `mod` 进去），
在 `start_pty_pump`（`smeltd.rs:750`）的泵循环里逐字节喂。

守护侧不需要 GUI 那套 `replay_len` 边界保护（那是为了避免 reattach 时重弹历史通知，
守护是**产生**方，没有重放问题）。

### 第 2 步：hook 链路（「协议事实」）

**2.1 注入环境变量**（`smeltd.rs:680` `spawn_session`）

```rust
fn spawn_session(id: &str, rows: u16, cols: u16, cwd: Option<&str>, launch: Option<&str>)
//               ^^^^^^^^ 新增参数（调用点 smeltd.rs:478）
{
    ...
    cmd.env("SMELT_SESSION_ID", id);          // ← 整条 hook 链路的地基
    cmd.env("SMELT_SOCK", sock_path());
}
```

**2.2 新 bin：`smelt-notify`**（约 80 行）

```
stdin: Claude Code 的 hook JSON（含 hook_event_name / tool_name / message）
env:   $SMELT_SESSION_ID, $SMELT_SOCK
出参:  连 socket 发一行 {"op":"state","id":"...","phase":"...","question":"..."}
```

映射：

| hook 事件 | → Phase |
|---|---|
| `UserPromptSubmit` | `Thinking` |
| `PreToolUse` | `ExecutingTool`（带 tool_name） |
| `Notification`（含 permission/权限） | `AwaitingApproval` + `pending_question` |
| `Stop` | `WaitingForUser` |
| `SessionEnd` | `Dead` |

**2.3 新 op：`state`**（`smeltd.rs:411` 的 `match v["op"]`）

```rust
Some("state") => {
    // hook 直写，最高可信度，覆盖 OSC 猜出来的值
    // 更新 SessionState → 广播给 subscribers
}
```

### 第 3 步：状态订阅（GUI 从「猜」改成「读」）

**3.1 新 op：`subscribe`**（长连接，newline-delimited JSON）

```
客户端 → {"op":"subscribe"}
守护   → {"sessions":[SessionState, ...]}        # 首帧全量
守护   → {"session": SessionState}               # 之后每次变化推一行
```

**完全不碰 `handle_open`。** 这是它相对「读方向加帧」的全部价值。

**3.2 扩展 `list`**（`smeltd.rs:413`，向后兼容）

现在只回 `{"sessions": ["id1", "id2"]}`（**纯 id 列表，零元数据**）。加一个字段：

```rust
serde_json::json!({ "sessions": ids, "states": states })  // ids 保留，老客户端不受影响
```

**3.3 GUI 侧：开一条全局订阅连接**

不要每个 pane 一条。在 `Workspace`（`main.rs:712`）上挂：

```rust
daemon_states: HashMap<String, SessionState>,   // key = smeltd session id
```

由一条常驻 subscribe 连接填。然后：

- `Session::status()`（`main.rs:342`）和 `pane_status()`（`main.rs:444`）改成**读
  `daemon_states`**，OSC 推断降级为 fallback（守护是老版本时）
- **顺带收掉一个技术债**：Braille spinner 判定（`\u{2801}..=\u{28FF}`）现在被复制了
  **三份**（`terminal_view.rs:241` / `main.rs:364` / `main.rs:453`），收进守护后 GUI 侧
  一份都不用留

⚠️ **id 的对应关系别搞错**：smeltd 的 session id 是**每个 pane 一个**
（`TerminalView.session_id`，`terminal_view.rs:181`），不是 GUI `Session`（会话）一个——
一个 GUI 会话分屏后有多个 smeltd 会话。

---

## 后续字段的来源（做作战地图时才需要）

- **`dirty_files` / `branch`**：`git_panel.rs` 已有
  （`GitStatusData.files: Vec<(String, String)>`，`run_git` 带 `GIT_OPTIONAL_LOCKS=0`
  已根治 index.lock 争用）。**坑：git status 只在 Git/Files 页 render 时才刷新**
  （`main.rs:2648` / `:2697`），待在终端页时根本不刷。要常驻上报得新加驱动点。
  另外 `GitStatusData` 除 `files` 外字段全私有，要开 getter。
- **`tokens_used`**：`session_history::SessionSummary.total_tokens`（单份 transcript =
  单会话）。⚠️ 它是**累计花费**口径（把每轮 `cache_read_input_tokens` 都加了），
  **不是上下文占用**，不能拿来当「余量」的分母。
- **上下文余量**：**全仓不存在**，要做得新算。
- **`project_dir` 编码规则**（`session_history.rs:14`）是**私有 fn**，要用得开
  `pub(crate)`，**别在 smeltd 侧重抄一遍**（CLAUDE.md 明令只此一份）。

## 配置放哪

CLAUDE.md 写的是「配置放 `~/.smelt/config.toml`」，但**这是一句尚未兑现的原则**：仓库里
**没有 `toml` 依赖**，唯一读 config.toml 的地方是 `agent.rs:59` 手写行扫描，只认一个
`DEEPSEEK_API_KEY`。

实际配置全是 JSON，收口在 `json_store.rs`（`load_json` / `save_json`），已有 5 个：
`appearance.json` / `launch.json` / `llm.json` / `pet.json` / `workspace.json`。

**联机配置沿现有惯例走 `json_store` 加 `~/.smelt/collab.json`**，零新依赖。真想兑现
config.toml 那条原则，是另一件事，别混在这个改动里。

---

## 落地顺序

1. **守护看得见状态**（StateListener + SessionState + Ctl 存 cwd + OSC 扫描共享）
   → 守护第一次知道自己在跑什么
2. **hook 链路**（`SMELT_SESSION_ID` 注入 + `smelt-notify` + `state` op）
   → 状态从「猜」变成「事实」
3. **订阅**（`subscribe` op + GUI 全局订阅 + `list` 扩展）
   → GUI 改成读事实；**顺带收掉 spinner 判定的三份复制**
4. —— 到这里单机已经全部受益，联机一行没写 ——
5. **网络暴露**（token + WebSocket，复用 attach 协议）→ 观战席

## 安全底线（第 5 步才相关，但现在就定死）

把 PTY 暴露到网络 = 开远程 shell。默认关闭；token 强随机、可过期；**可写模式必须主人当面
点头**，不能靠 URL 携带就生效；绑回环或 Tailscale 接口，**不是 `0.0.0.0`**。
