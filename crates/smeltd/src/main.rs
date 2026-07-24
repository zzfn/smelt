//! smeltd —— 终端持久化守护进程（tmux 的最小替身）。
//!
//! 所有 shell / PTY 活在这里而非 GUI 进程里：GUI 退出、崩溃，会话照常运行；
//! 重开 GUI 按会话 id 重连（attach）。
//!
//! ## 画面恢复（类 tmux，不是「字节磁带重放」）
//!
//! 每个会话在守护内常驻一份 `alacritty_terminal::Term`：PTY 输出一边转发给 client，
//! 一边 `parser.advance` 进这份网格。attach 时**不**依赖可能被环形缓冲腰斩的原始
//! 字节重放，而是把当前网格序列化成一段自洽的 ANSI「整屏快照」发给客户端——空 Term
//! 解析后即当前画面，避免长 detach 后 Ctrl+C 大重绘错位（见 docs/roadmap.md）。
//! 仍保留一小段原始字节环形缓冲，**只**给尚未 attach 的瞬间攒实时输出；
//! **绝不**用它在 upgrade 后重建 Term（环形缓冲会在 CSI 中间腰斩，feed 必花屏）。
//!
//! 协议（Unix socket ~/.smelt/smeltd.sock）——连接后客户端先发一行 JSON：
//!   {"op":"open","id":"..","cwd":"..","cols":120,"rows":30}  → 进入流模式（唯一 client，
//!                                                              同 id 第二次 open 顶掉前一个）
//!   {"op":"watch","id":".."}                                 → 进入**只读**流模式（旁观，见下）
//!   {"op":"list"}                                            → 回 {"sessions":[..]} 后关闭
//!   {"op":"kill","id":".."}                                  → 回 {"ok":true} 后关闭
//!   {"op":"version"}                                         → 回 {"version":"..","exe_mtime":123} 后关闭
//!   {"op":"shutdown"}                                        → 回 {"ok":true} 后进程退出（杀掉所有会话！）
//!   {"op":"upgrade"}                                         → 回 {"ok":true} 后 exec 磁盘上的新二进制，
//!                                                              PTY fd 原地交接，**所有会话不中断**（见下）
//!   {"op":"upgrade","exe":"/path/to/smeltd"}                 → 同上，但 exec 指定路径（装 DMG 时先
//!                                                              handoff 到暂存包，再替换 .app，避免
//!                                                              整包覆盖把旧守护 SIGKILL、会话全灭）
//!   {"op":"remote_start","bind":"..","port":0,"write":false}  → 回 {"ok":true,"token":"..","addr":"..","write":bool}，
//!                                                              见下「内嵌远程网关」（bind/port/write 都可省，
//!                                                              默认回环随机口 + 只读）
//!   {"op":"remote_stop"}                                     → 回 {"ok":true} 后关闭
//!   {"op":"remote_status"}                                   → 回 {"running":bool,"token":"..","addr":"..","write":bool} 后关闭
//!   {"op":"tunnel_start","write":false}                       → 回 {"ok":true,"url":"..","write":bool}，
//!                                                              spawn cloudflared 把远程网关暴露到公网
//!                                                              （见下「Cloudflare Tunnel」）
//!   {"op":"tunnel_stop"}                                     → 回 {"ok":true} 后关闭
//!   {"op":"tunnel_status"}                                   → 回 {"running":bool,"url":"..","write":bool} 后关闭
//!   {"op":"state","id":"..","phase":"..","question":".."}    → 回 {"ok":true} 后关闭，hook 直写（见下
//!                                                              「状态通道」），question 可省
//!   {"op":"action","id":"..","kind":"approve|deny|reply","text":".."}
//!                                                            → 回 {"ok":true}/{"ok":false,"err":".."} 后关闭，
//!                                                              见下「远程操控」（text 仅 reply 需要）
//!   {"op":"input","id":"..","data":".."}                     → 回 {"ok":true}/{"ok":false,"err":".."} 后关闭，
//!                                                              `data` 是 UTF-8 字符串（控制字符用 JSON
//!                                                              `\u00xx`），原样写入 PTY，**无 phase 门闩**
//!   {"op":"resize","id":"..","cols":N,"rows":M}              → 回 {"ok":true} 后关闭，改 PTY 窗口尺寸
//!                                                              （SIGWINCH，供手机端按视口重排 TUI）
//!
//! 流模式：
//!   守护 → 客户端：先发 JSON 尺寸行（含 replay_len=快照字节数）→ Codux 风格 keyframe
//!                   ANSI（模式前缀 + 按行 CUP + 绝对 SGR，见 snapshot_ansi）
//!                   → 再实时转发 PTY 输出
//!   客户端 → 守护：帧 [type:u8][len:u32 BE][payload]
//!     type 0 = 键盘输入字节；type 1 = resize
//!       payload 8 字节：cols u32 BE + rows u32 BE（兼容旧客户端，像素 = 0）
//!       payload 16 字节：cols + rows + cell_w + cell_h（各 u32 BE）→
//!         ws_xpixel = cols*cell_w，ws_ypixel = rows*cell_h
//! shell 退出 → 守护关闭该连接（客户端读到 EOF）。
//!
//! ## `watch`：只读旁观，不参与「同 id 唯一 client」的顶替
//!
//! 远程操作/观战席这类场景需要「GUI 开着的同时，另一路也能看画面」——但 `open` 的语义
//! 是「同 id 只允许一个 GUI」（第二次 open 会 shutdown 前一个连接），不能照搬。`watch`
//! 是独立的第二条路径：会话必须已存在（不会像 `open` 那样兜底新建）；进来后收一份和
//! `open` 一样的尺寸行 + ANSI 快照，但**不进入帧循环**——不认输入/resize，收到任何客户端
//! 发来的字节都当异常直接断开。多个 `watch` 连接可以并存，也不影响 `open` 的那个唯一
//! client；某个 watcher 断线只清自己，不影响其他 watcher 或 client。
//!
//! ## 无缝升级（"upgrade" op，nginx 风格 exec 交接）
//!
//! fd 属于进程而非二进制：`exec()` 换掉程序映像但 PID 与打开的 fd 都还在，只要
//! PTY master fd 不关，shell 就活着。流程：
//! 1. 短暂持一下 sessions 锁，只做「克隆一份 Arc 列表」这一步就放开——不长期占着，
//!    避免这期间 open/list/kill/version 全部卡死；随后拿 SPAWN_GATE 独占锁挡住新
//!    shell 的 fork（防止 fork 意外继承正被清 CLOEXEC 的 fd，见 SPAWN_GATE 注释）；
//! 2. 逐会话拿 ctl/out 锁做快照（master fd / shell pid / 尺寸 / **Term 可视区 keyframe**）
//!    ——out 锁在 handle_open 里配了写超时（CLIENT_WRITE_TIMEOUT），泵线程不会无限期攥着；
//! 3. 给 master fd 和监听 socket fd 清掉 CLOEXEC，快照写入交接文件（fd 号 + grid ANSI，
//!    0600）；**画面恢复只认 grid**，与 shell/TUI/agent 无关，同一条路径；
//! 4. 回 {"ok":true} 后 `exec()` 磁盘上的 smeltd（同路径新内容），带 SMELTD_HANDOFF 环境变量；
//! 5. 新进程：认领 fd → 空 Term → **只 feed grid keyframe** → 开泵（jolt=true）。
//!    环形 `buf` 若写在交接文件里也**不** feed（历史字段，兼容旧 handoff 文件）。
//! exec 失败则回滚（恢复 CLOEXEC、删交接文件、继续服务，释放 SPAWN_GATE）。客户端连接
//! 是 CLOEXEC 的，随 exec 断开，GUI 按会话 id 重连即恢复——跟 GUI 自己重启走的是同一条
//! reattach 路。shell 子进程的父进程关系不受 exec 影响（同 PID），收尸的 waitpid 照常
//! 工作。交接文件读不出/解析失败（极端情况）时新进程走全新启动兜底：**不**做「能连上
//! 说明已有守护」这条单实例检查——此时我们可能还继承着旧监听 fd，检查会连上自己而
//! 误判、直接自杀，见 main() 里的 came_from_handoff 分支。
//!
//! ## 内嵌远程网关（`remote_start`/`remote_stop`/`remote_status`）
//!
//! 路由/handler 全在 `remote_gateway.rs`（跟独立进程版 `gateway.rs` 共用一份，见那边
//! 的模块注释）——这里只是按需把它跑起来。守护本身是同步/阻塞线程模型，**不**把
//! `main()` 整个改成 async；`remote_start` 只是另起一条 OS 线程，在那条线程里私自建
//! 一个 tokio runtime 跑 axum server，跟守护主循环完全隔离，互不影响。
//!
//! 幂等：已经开着时 `remote_start` 直接回现有的 token/addr，不重启、不换 token；
//! 想要新 token 得先 `remote_stop` 再 `remote_start`。**不**参与无缝升级交接——
//! `upgrade` 之后如果之前开着远程网关，会随旧进程退出而关闭，新进程里默认是关的
//! （GUI 那边在 upgrade 完成后按需重新 `remote_start`）。安全默认跟 `watch` 一致：
//! 默认关闭、绑回环，见 collaboration.md 的安全底线。
//!
//! ## Cloudflare Tunnel（`tunnel_start`/`tunnel_stop`/`tunnel_status`）
//!
//! 解决"内嵌远程网关默认绑回环，手机切到蜂窝网络就连不上"这个问题（见
//! docs/remote-ops-roadmap.md Phase 3）。`tunnel_start` 会先确保内嵌远程网关已经
//! 开着（没开就用默认参数开一个），再 spawn `cloudflared tunnel --url` 子进程把它
//! 暴露到一个 `*.trycloudflare.com` 公网地址——**不是自建信令 + WebRTC**，是走
//! Cloudflare 的隧道中转，权衡理由见 roadmap 文档。
//!
//! `cloudflared` 是外部二进制，不 vendor 进仓库；没装时 `tunnel_start` 会明确报错
//! （提示 `brew install cloudflared`），不是静默失败。查找**不只靠 PATH**：从 Dock
//! 启动的 GUI 继承的 PATH 通常没有 Homebrew（`/opt/homebrew/bin`），开发时终端里
//! 有、打成 DMG 后却报「没找到」就是这个原因。见 `resolve_cloudflared`。
//! 子进程的 stdout/stderr 全程有专门线程持续读干净（不只是为了扒事件，也是为了
//! 不让管道写满反过来卡住 cloudflared）。同样不参与无缝升级交接，同样默认关闭。
//!
//! **强制 `--protocol http2`**（实测踩过的坑）：quick tunnel 默认先试 QUIC，网络挡
//! UDP/QUIC 时会反复重试好几轮才退化到 http2；而且 cloudflared 打印"隧道已创建"
//! 的 URL 早于连接真正建好——只看到 URL 就上报成功，实测会先给出一个访问 530 的
//! 死链接。`start_tunnel` 因此额外等一条 `Registered tunnel connection` 日志才算数。
//!
//! ## 远程操控（`action` + `input` op）
//!
//! Phase 6：远程端是 PC 工作的**延续**——能力上要能往 PTY 写任意字节，交互上再
//! 用操作台按钮减负。两条 op 分工：
//!
//! **`input`**：原始字节写入 PTY，和本机键盘同权。**没有 phase 门闩**——用户可能
//! 随时要 Ctrl+C、在 agent 思考时补一句、或在 TUI 里方向键导航。`data` 是 UTF-8
//! 字符串（控制字符走 JSON `\u00xx`，xterm onData 出来的串 `JSON.stringify` 即可）；
//! 空串拒绝。
//!
//! **`action`**：approve/deny/reply 映射成固定按键序列，是高频快捷方式，**不是**
//! 能力上限。门闩（`phase` 必须是 `AwaitingApproval`/`WaitingForUser`）是**正确性**
//! 保护，防止误点「批准」时 agent 其实在跑别的——不排队，直接拒绝：
//! - `approve` → `\r`（回车，接受当前高亮的默认项）
//! - `deny` → `\x1b`（Esc，不管菜单形状直接取消/拒绝）
//! - `reply` → 文本 + `\r`（便捷回复；自由输入更推荐走 `input`）
//!
//! 授权模型：链接本身就是授权；写权限（action + input）由生成链接时的开关决定
//! （GUI 的"允许写入"），网关侧 `write_enabled` 把关，smeltd 的 action 门闩只管
//! 时机、不管权限。

use smelt_core::remote_gateway;
// 只需要 spinner 判定；OSC 扫描整包留给 workspace（smelt-core 的 osc 模块）。
use smelt_core::title_spinner;
// 权限菜单解析与网格取文本：与 GUI 共用 smelt-core 里的同一份。手机端不再自己解析——
// 它拉 `menu` op 拿这里的结果。两份实现（Rust/TS）曾实测漂移过，别再走回头路。
use smelt_core::{permission_menu, term_text};

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::thread;
use std::time::Duration;

use alacritty_terminal::event::{Event, EventListener};
#[cfg(test)]
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config as TermConfig, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Processor};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};

/// 常驻 Term 的 scrollback 行数（状态机 history-limit）。
const TERM_HISTORY: usize = 10_000;
/// attach 快照最多带上的历史行数（含可视区）；避免超大会话一次吐爆客户端。
const SNAPSHOT_MAX_LINES: usize = 10_000;

/// attach 客户端 socket 的写超时：泵线程/attach 初始重放都会往客户端 write，客户端
/// 冻结（GUI 被挂起/调试暂停）时不能让这一个 write 无限期占着 Out 锁——handle_upgrade
/// 快照时也要挨个拿这把锁，泵线程如果永久攥着，会把整个 upgrade 拖成全局死锁。
const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_secs(3);

/// 挡住「spawn 新 shell 的 fork」与「upgrade 清 CLOEXEC 准备 exec」并发的门闩：不挡会
/// 有极小窗口——CLOEXEC 刚被清、我们自己还没 exec 时，恰好 fork 出一个新 shell，会把
/// 当时暴露出去的全部 fd（其它会话的 PTY master、监听 socket）一并带给这个新 shell。
/// spawn 拿共享锁（多个新会话可以互相并发起），upgrade 拿独占锁（跟所有 spawn 互斥）。
static SPAWN_GATE: RwLock<()> = RwLock::new(());

fn sock_path() -> std::path::PathBuf {
    let dir = dirs::home_dir().unwrap_or_else(|| "/tmp".into()).join(".smelt");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("smeltd.sock")
}

/// 追加一行到 ~/.smelt/daemon.log。只给「守护无声死亡」的几条路径留痕用——
/// 守护被 SIGKILL（例：装新版时用 cp 覆盖了已签名二进制，upgrade 的 exec 会被
/// macOS 内核直接杀掉，无输出无崩溃报告）或静默 return 时，这份日志是唯一线索：
/// 日志停在「即将 exec」而没有下一行「交接完成」，就是 exec 被杀。
fn dlog(msg: &str) {
    use std::io::Write;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(sock_path().with_file_name("daemon.log"))
    {
        let _ = writeln!(f, "[{ts}] pid={} {msg}", std::process::id());
    }
}

/// 本进程可执行文件的 mtime（unix 秒）：作为「版本身份」上报给 GUI。GUI 拿磁盘上
/// smeltd 二进制的当前 mtime 一比，就知道正在跑的守护是不是重装/重编译前的旧进程。
fn exe_mtime_secs() -> u64 {
    std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::metadata(p).ok())
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 会话控制端：PTY 输入 / resize / 杀进程。
///
/// 持有的是 PTY master 的**裸 fd**（File 包装）而非 portable_pty 的类型：无缝升级要把
/// fd 原样带过 exec，portable_pty 的 MasterPty/Child 无法从裸 fd 重建。spawn 仍用
/// portable_pty（openpty + 环境 + 会话组等脏活），起完就把 fd dup 出来自己管。
struct Ctl {
    /// PTY master：写输入 + ioctl(TIOCSWINSZ) resize；泵线程的读端是它的 try_clone。
    master: std::fs::File,
    /// shell 进程 pid：kill 会话 / shell 退出后收尸（waitpid）。
    pid: i32,
    /// reattach 后首个 resize 强制「抖动」（先 rows+1 再回正）：即使尺寸与断开前相同也
    /// 制造 SIGWINCH，让备用屏 TUI（Claude Code 等）重绘整屏，避免重连花屏。
    jolt: bool,
    /// PTY 当前行列。attach 时回报给客户端：重放字节按此宽度生成，GUI 必须把本地
    /// 终端建成同尺寸再解析，否则行宽错位（zsh 行尾 % 盖不掉、TUI 布局撕裂）。
    cols: u16,
    rows: u16,
    /// spawn 时的静态目录（作战地图要）。**不**跟随 shell 的 `cd`——真实 cwd 要
    /// OSC 7，这里只是「这个会话是从哪打开的」，见 SessionState.cwd 用法。
    cwd: Option<String>,
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 本进程启动时刻（unix 秒），`version` op 回给 GUI 展示「守护跑了多久」。
/// 必须在 main 最开头取一次，否则记的是「首次有人问」的时间。
///
/// 无缝升级（exec 交接）后是全新进程，这个值会重置，而会话照旧活着——设置页因此
/// 会显示「守护刚起、会话仍在」，那是如实反映，不是 bug。
fn started_at() -> u64 {
    static STARTED_AT: OnceLock<u64> = OnceLock::new();
    *STARTED_AT.get_or_init(now_unix)
}

/// 会话状态通道（见 docs/state-channel-plan.md）。三个信源按可信度覆盖：
/// hook 直写（`state` op，协议事实，最高）> OSC 9/777（终端协议，中）>
/// OSC 0/2 标题的 spinner 猜测（最低，纯猜）。schema 定死，字段不够用再加，
/// 不删不改类型——远程端/GUI 都按这份 schema 解码。
#[derive(Clone, Default, serde::Serialize)]
struct SessionState {
    id: String,
    cwd: Option<String>,
    /// claude / codex / copilot（来自 spawn 时的 launch 命令）。
    launch: Option<String>,
    /// OSC 0/2 标题，GUI 现在也读这个显示 tab 名。
    title: Option<String>,
    phase: Phase,
    /// unix 秒。「空转多久了」靠它算——作战地图用。
    phase_since: u64,
    /// 在问什么——远程遥控的命脉，Phase 6 的 action 门闩靠它判断能不能安全写入。
    pending_question: Option<String>,
    /// 累计花费口径（各轮 cache_read 都加了），**不是**上下文占用，不能当余量分母。
    /// 见 session_history::SessionSummary.total_tokens；目前没接，先占位。
    tokens_used: Option<u64>,
    /// 撞车预警要用；目前没接（见 git_panel.rs 的 GitStatusData），先占位。
    branch: Option<String>,
    dirty_files: Vec<String>,
    updated_at: u64,
}

#[derive(Clone, Copy, PartialEq, Debug, Default, serde::Serialize, serde::Deserialize)]
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

/// `subscribe` 连接的全局池——状态订阅是「一条连接看全部会话」，不像 `watch`
/// 那样挂在单个 Session 底下（见 docs/state-channel-plan.md 的 subscribe 设计）。
type Subscribers = Arc<Mutex<Vec<UnixStream>>>;

/// 状态变化推给所有订阅者；写失败（已断线）的连接直接摘掉，跟 watchers 的
/// 惰性清理是同一个模式。
fn broadcast_state(subscribers: &Subscribers, state: &SessionState) {
    let payload = serde_json::json!({ "session": state }).to_string();
    subscribers.lock().unwrap().retain_mut(|s| writeln!(s, "{payload}").is_ok());
}

/// 常驻 Term 的事件监听：接住 alacritty 解析出的 `Event::Title`/`Event::Bell`，
/// 写进共享的 `SessionState`，顺带广播给所有 `subscribe` 连接。**只在这里猜
/// phase**（OSC 0/2 标题 spinner，最低可信度信源）——`state` op 是协议事实，
/// 可信度最高。spinner 只允许在 `Idle`/`Thinking` 上升为 `Thinking`；绝不能盖掉
/// hook 已写好的 `AwaitingApproval`/`WaitingForUser`/`ExecutingTool`/`Dead`（否则
/// 远程 action 门闩会误拒、操作台按钮错态）。标题不是 spinner 时**不**反过来猜
/// 别的 phase（缺乏证据不代表 idle）。
#[derive(Clone)]
struct StateListener {
    state: Arc<Mutex<SessionState>>,
    subscribers: Subscribers,
}

impl EventListener for StateListener {
    fn send_event(&self, event: Event) {
        let snapshot = {
            let Ok(mut st) = self.state.lock() else { return };
            match event {
                Event::Title(t) => {
                    if title_spinner::title_starts_with_spinner(t.trim_start())
                        && matches!(st.phase, Phase::Idle | Phase::Thinking)
                    {
                        // 只在「进入」Thinking 那一刻记起点。agent 思考时 spinner 每秒
                        // 换一帧（⠋→⠙→⠹…），帧帧都是一次 Title 事件；已经在 Thinking
                        // 里还刷起点的话，「已思考 N 秒」会永远在 0~1 之间跳。
                        if st.phase != Phase::Thinking {
                            st.phase = Phase::Thinking;
                            st.phase_since = now_unix();
                        }
                    }
                    st.title = Some(t);
                    st.updated_at = now_unix();
                }
                Event::Bell => {
                    st.updated_at = now_unix();
                }
                _ => return,
            }
            st.clone()
        };
        broadcast_state(&self.subscribers, &snapshot);
    }
}

/// 按行列 + 可选像素尺寸 resize PTY（TIOCSWINSZ）。
/// `xpixel`/`ypixel` 是**整窗**像素（cols×cell_w / rows×cell_h），不是单格。
fn resize_fd(fd: RawFd, rows: u16, cols: u16, xpixel: u16, ypixel: u16) {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: xpixel,
        ws_ypixel: ypixel,
    };
    unsafe {
        libc::ioctl(fd, libc::TIOCSWINSZ, &ws);
    }
}

/// 会话 resize：PTY ioctl + 常驻 Term 同步 + 可选 jolt 抖动。
/// 手机远程与 GUI open 帧共用，避免两套尺寸逻辑漂移。
fn resize_session(sess: &Session, cols: u16, rows: u16, cell_w: u16, cell_h: u16) {
    let cols = cols.max(1);
    let rows = rows.max(1);
    let xpixel = cols.saturating_mul(cell_w);
    let ypixel = rows.saturating_mul(cell_h);
    let mut ctl = sess.ctl.lock().unwrap();
    let fd = ctl.master.as_raw_fd();
    if ctl.jolt {
        ctl.jolt = false;
        resize_fd(fd, rows.saturating_add(1), cols, xpixel, ypixel);
    }
    resize_fd(fd, rows, cols, xpixel, ypixel);
    ctl.cols = cols;
    ctl.rows = rows;
    drop(ctl);
    if let Ok(mut term) = sess.term.lock() {
        term.resize(DaemonTermSize {
            rows: rows as usize,
            cols: cols as usize,
        });
    }
}

/// 开/关 fd 的 CLOEXEC 标志。平时所有 fd 都应带 CLOEXEC（不泄漏给 spawn 出的 shell）；
/// 仅在 exec 交接前对要带过去的 fd 关掉。
fn set_cloexec(fd: RawFd, on: bool) {
    unsafe {
        let cur = libc::fcntl(fd, libc::F_GETFD);
        if cur >= 0 {
            let new = if on { cur | libc::FD_CLOEXEC } else { cur & !libc::FD_CLOEXEC };
            libc::fcntl(fd, libc::F_SETFD, new);
        }
    }
}

/// dup 一个 fd 并包成 File。dup 出的新 fd 默认**不带** CLOEXEC，这里立即补上——
/// 否则它会泄漏进之后 spawn 的每个 shell（占着 PTY master 不放，会话杀不干净）。
fn dup_file(fd: RawFd) -> anyhow::Result<std::fs::File> {
    let d = unsafe { libc::dup(fd) };
    anyhow::ensure!(d >= 0, "dup({fd}) 失败");
    set_cloexec(d, true);
    Ok(unsafe { std::fs::File::from_raw_fd(d) })
}

/// 会话输出端：当前 attach 的客户端 + watch 旁观者。
/// 「快照→接管」与实时转发共用这把锁，严格串行。
/// 画面恢复只靠常驻 Term 的 keyframe，**不再**维护环形字节缓冲。
struct Out {
    client: Option<UnixStream>,
    /// `watch` 连接：只读旁观，不参与 client 的顶替逻辑，可多个并存。
    watchers: Vec<UnixStream>,
}

/// 守护侧常驻终端状态机尺寸（实现 alacritty Dimensions）。
#[derive(Clone, Copy)]
struct DaemonTermSize {
    rows: usize,
    cols: usize,
}

impl Dimensions for DaemonTermSize {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

fn daemon_term_config() -> TermConfig {
    TermConfig {
        scrolling_history: TERM_HISTORY,
        ..TermConfig::default()
    }
}

fn new_daemon_term<T: EventListener>(rows: u16, cols: u16, listener: T) -> Term<T> {
    let size = DaemonTermSize {
        rows: rows.max(1) as usize,
        cols: cols.max(1) as usize,
    };
    Term::new(daemon_term_config(), &size, listener)
}

struct Session {
    ctl: Mutex<Ctl>,
    out: Mutex<Out>,
    /// 常驻网格：PTY 输出持续 advance；attach 时序列化成 ANSI 快照。挂的是
    /// `StateListener`（不再是 `VoidListener`）——守护自己也要看得见 Title/Bell。
    term: Mutex<Term<StateListener>>,
    /// 结构化状态（见 SessionState）。跟 `term` 的监听器共用同一个 Arc，
    /// `state` op（hook 直写）和 `subscribe` 的转发都读/改这一份。
    state: Arc<Mutex<SessionState>>,
}

type Sessions = Arc<Mutex<HashMap<String, Arc<Session>>>>;

/// 内嵌远程网关开着时的状态：token、绑定地址、写权限、喊停用的信号。见文件头
/// 「内嵌远程网关」一节——这条不参与无缝升级交接，`upgrade` 后新进程里永远是 None。
struct RemoteGateway {
    token: String,
    addr: std::net::SocketAddr,
    write: bool,
    shutdown_tx: tokio::sync::oneshot::Sender<()>,
}

type RemoteState = Arc<Mutex<Option<RemoteGateway>>>;

/// 幂等：已经开着直接回现有 token/addr/write，不重启、不换 token——包括 `write`
/// 参数：想改写权限得先 `remote_stop` 再 `remote_start`，不支持热切换（跟其余
/// 参数如 bind/port 一样，改配置就是重开一次，这个项目里没有"热更新"这个概念）。
/// bind 非法 / 端口绑不上 / 服务线程起不来都走 Err，调用方原样透传给客户端。
///
/// **先等 serve 就绪再写 `RemoteState`**：以前 spawn 后立刻标 running，子线程
/// `Runtime::new`/`from_std` 失败时状态假活，幂等路径永远回死 token。
fn start_remote_gateway(
    state: &RemoteState,
    bind: &str,
    port: u16,
    write: bool,
) -> Result<(String, std::net::SocketAddr, bool), String> {
    let mut guard = state.lock().unwrap();
    if let Some(g) = guard.as_ref() {
        return Ok((g.token.clone(), g.addr, g.write));
    }

    let ip: std::net::IpAddr = bind.parse().map_err(|e| format!("非法绑定地址 {bind}：{e}"))?;
    let std_listener = std::net::TcpListener::bind((ip, port))
        .map_err(|e| format!("绑定 {bind}:{port} 失败：{e}"))?;
    std_listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let addr = std_listener.local_addr().map_err(|e| e.to_string())?;

    let token = uuid::Uuid::new_v4().simple().to_string();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    // 子线程认领 listener / 建 runtime 成功才算 ready；失败则本函数 Err 且不写 state。
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();

    let token_for_thread = token.clone();
    thread::spawn(move || {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(e) => {
                let msg = format!("远程网关起不了 tokio runtime：{e}");
                eprintln!("{msg}");
                let _ = ready_tx.send(Err(msg));
                return;
            }
        };
        rt.block_on(async move {
            let listener = match tokio::net::TcpListener::from_std(std_listener) {
                Ok(l) => l,
                Err(e) => {
                    let msg = format!("远程网关认领监听 fd 失败：{e}");
                    eprintln!("{msg}");
                    let _ = ready_tx.send(Err(msg));
                    return;
                }
            };
            // listener 已就绪，即将 serve——此时可以对外报 running。
            let _ = ready_tx.send(Ok(()));
            let app = remote_gateway::build_router(token_for_thread, write);
            let serve = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                });
            if let Err(e) = serve.await {
                eprintln!("远程网关退出：{e}");
            }
        });
    });

    match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => {
            *guard = Some(RemoteGateway {
                token: token.clone(),
                addr,
                write,
                shutdown_tx,
            });
            Ok((token, addr, write))
        }
        Ok(Err(e)) => Err(e),
        Err(_) => Err("远程网关启动超时（5s）".into()),
    }
}

fn stop_remote_gateway(state: &RemoteState) {
    if let Some(g) = state.lock().unwrap().take() {
        let _ = g.shutdown_tx.send(());
    }
}

/// Cloudflare Tunnel（Phase 3，见 docs/remote-ops-roadmap.md）：spawn `cloudflared`
/// 子进程把本机远程网关暴露到公网，不是 P2P，是走 Cloudflare 中转——见 roadmap 里
/// 放弃自建信令+WebRTC 的理由。持有子进程句柄，`tunnel_stop` 时负责杀干净。
///
/// `Starting` 占位：挡住并发 `tunnel_start` 在「已确认 None → 等 cloudflared 30s」
/// 窗口里各起一个子进程、后写覆盖导致前者泄漏的竞态。
enum TunnelSlot {
    Starting,
    Up {
        child: std::process::Child,
        url: String,
    },
}

type TunnelState = Arc<Mutex<Option<TunnelSlot>>>;

/// 进程退出 / upgrade exec 前清理远程网关与 tunnel。菜单栏 quit 与 accept 线程
/// 不同线程，靠这份 OnceLock 共享 Arc（main 启动时 register）。
static LIFECYCLE: std::sync::OnceLock<(RemoteState, TunnelState)> = std::sync::OnceLock::new();

fn register_lifecycle(remote: RemoteState, tunnel: TunnelState) {
    let _ = LIFECYCLE.set((remote, tunnel));
}

/// 杀 cloudflared、关内嵌网关。exit/exec 前必须调——否则子进程孤儿化或
/// exec 后 PID 仍在但 `TunnelState` 已丢句柄。
fn cleanup_sidecar_services() {
    if let Some((remote, tunnel)) = LIFECYCLE.get() {
        stop_tunnel(tunnel);
        stop_remote_gateway(remote);
    }
}

/// 从 cloudflared 的一行日志里认出公网 URL（形如
/// `https://xxx-xxx-xxx.trycloudflare.com`，混在 box-drawing 字符和时间戳里）。
fn extract_tunnel_url(line: &str) -> Option<String> {
    line.split_whitespace()
        .find(|tok| tok.starts_with("https://") && tok.contains(".trycloudflare.com"))
        .map(|s| s.trim_matches('|').to_string())
}

/// URL 打印出来 ≠ 隧道真的能用——cloudflared 注册完 hostname 就先打 URL，实际到
/// Cloudflare 边缘的连接（尤其网络挡了 QUIC、要退化到 http2 时）可能还要再等一会。
/// 必须等到这条"已建好连接"的日志才算数（实测：只看 URL 会拿到一个暂时 530 的死链接）。
enum TunnelEvent {
    Url(String),
    Connected,
    /// cloudflared 明确失败（如 `failed to request quick Tunnel: ...`），应立刻失败
    /// 而不是傻等到 30s 超时。
    Failed(String),
}

const TUNNEL_CONNECTED_MARKER: &str = "Registered tunnel connection";

/// 从 cloudflared 日志里摘「致命错误」摘要，方便 GUI 展示（换网络后常见
/// `api.trycloudflare.com` 被墙/劫持/断连）。
fn extract_tunnel_failure(line: &str) -> Option<String> {
    let lower = line.to_ascii_lowercase();
    // 官方 quick tunnel 申请失败
    if lower.contains("failed to request quick tunnel")
        || lower.contains("failed to create tunnel")
        || lower.contains("unable to reach the origin service")
        || lower.contains("context deadline exceeded")
        || lower.contains("connection refused")
        || lower.contains("i/o timeout")
        || lower.contains("no such host")
        || lower.contains("certificate")
        || (lower.contains("err ") && lower.contains("tunnel"))
    {
        // 去掉时间戳前缀，只留可读部分
        let msg = line
            .split_once("ERR ")
            .map(|(_, rest)| rest)
            .or_else(|| line.split_once("INF ").map(|(_, rest)| rest))
            .unwrap_or(line)
            .trim();
        if msg.is_empty() {
            return None;
        }
        return Some(msg.to_string());
    }
    // 非 ERR 级别但明确失败的整行
    if line.contains("failed to request quick Tunnel") {
        return Some(line.trim().to_string());
    }
    None
}

/// 持续把 cloudflared 的一路输出（stdout 或 stderr）读干净，**贯穿整个子进程生命
/// 周期**，不是只读到握手成功为止：不只是为了扒事件（不读干净会把管道缓冲区写满，
/// 反过来卡住 cloudflared），也是因为断线重连之后 cloudflared 理论上可能重新申请
/// 一个新域名（官方对 quick tunnel 只保证"进程存活期间"这一件事，没有更强的承诺）。
/// 一旦扫到新的 URL，直接更新 `tunnel_state` 里存的那份——不这样做的话，一旦真的
/// 发生重新分配，GUI 会一直显示一条已经失效的旧链接，且没有任何信号能让它自己发现。
fn spawn_tunnel_output_scanner(
    reader: impl Read + Send + 'static,
    tx: std::sync::mpsc::Sender<TunnelEvent>,
    tunnel_state: TunnelState,
) {
    thread::spawn(move || {
        let buf = BufReader::new(reader);
        for line in buf.lines().map_while(Result::ok) {
            if let Some(url) = extract_tunnel_url(&line) {
                let _ = tx.send(TunnelEvent::Url(url.clone()));
                // 握手阶段是 Starting/None，交给 start_tunnel 的 rx 处理首次握手；
                // 这行只对「握手完成之后又冒出新 URL」更新已上线的槽位。
                if let Some(TunnelSlot::Up { url: slot_url, .. }) =
                    tunnel_state.lock().unwrap().as_mut()
                {
                    *slot_url = url;
                }
            }
            if line.contains(TUNNEL_CONNECTED_MARKER) {
                let _ = tx.send(TunnelEvent::Connected);
            }
            if let Some(err) = extract_tunnel_failure(&line) {
                let _ = tx.send(TunnelEvent::Failed(err));
            }
        }
    });
}

/// 保证本机远程网关按 `write` 开着（隧道启动前要先有可转发的网关）。
///
/// - 未开 → 用回环 + 随机端口 + `write` 开一个
/// - 已开且 `write` 一致 → 复用（不换 token）
/// - 已开但 `write` 不同 → 先停再开（权限烤进 router，不能热切换；旧链接随之失效）
///
/// 这是 `start_tunnel` 的入口，不能直接调幂等的 `start_remote_gateway`：后者在已开时
/// **忽略**传入的 `write`，会把「隧道要可写」静默落成只读（Phase 6 修过的坑）。
fn ensure_remote_gateway_with_write(
    state: &RemoteState,
    write: bool,
) -> Result<(String, std::net::SocketAddr, bool), String> {
    {
        let guard = state.lock().unwrap();
        if let Some(g) = guard.as_ref() {
            if g.write == write {
                return Ok((g.token.clone(), g.addr, g.write));
            }
        }
    }
    stop_remote_gateway(state);
    start_remote_gateway(state, "127.0.0.1", 0, write)
}

#[cfg(test)]
mod ensure_remote_gateway_write_tests {
    use super::*;

    #[test]
    fn starts_with_requested_write_when_down() {
        let state: RemoteState = Arc::new(Mutex::new(None));
        let (token, _addr, write) = ensure_remote_gateway_with_write(&state, true).expect("start");
        assert!(write, "应烤进 write=true");
        assert!(!token.is_empty());
        // 现状一致：再要一次可写必须复用同一 token，不能偷偷再起一个
        let (token2, _, write2) = ensure_remote_gateway_with_write(&state, true).expect("reuse");
        assert_eq!(token, token2);
        assert!(write2);
        stop_remote_gateway(&state);
    }

    #[test]
    fn restarts_and_rotates_token_when_write_changes() {
        let state: RemoteState = Arc::new(Mutex::new(None));
        let (token_ro, _, write_ro) =
            ensure_remote_gateway_with_write(&state, false).expect("start ro");
        assert!(!write_ro);

        // 关键回归：幂等的 start_remote_gateway 在已开时会忽略传入 write=true，
        // ensure 必须先停再开，否则隧道路径会静默保持只读。
        let (token_rw, _, write_rw) =
            ensure_remote_gateway_with_write(&state, true).expect("upgrade to rw");
        assert!(write_rw, "write 切换后必须变成可写");
        assert_ne!(token_ro, token_rw, "写权限变了必须换新 token，旧链接失效");

        let (token_ro2, _, write_ro2) =
            ensure_remote_gateway_with_write(&state, false).expect("downgrade to ro");
        assert!(!write_ro2);
        assert_ne!(token_rw, token_ro2);
        stop_remote_gateway(&state);
    }

    #[test]
    fn plain_start_remote_gateway_is_still_idempotent_on_write() {
        // 对照：裸 start_remote_gateway 的旧语义还在——已开时忽略 write 参数。
        // ensure 才是"按 write 对齐"的入口；别把两个行为搞混。
        let state: RemoteState = Arc::new(Mutex::new(None));
        let (t1, _, w1) = start_remote_gateway(&state, "127.0.0.1", 0, false).expect("ro");
        assert!(!w1);
        let (t2, _, w2) = start_remote_gateway(&state, "127.0.0.1", 0, true).expect("idempotent");
        assert_eq!(t1, t2);
        assert!(!w2, "幂等路径必须继续忽略传入的 write=true");
        stop_remote_gateway(&state);
    }
}

/// 定位本机 `cloudflared` 可执行文件。
///
/// 不能只写 `Command::new("cloudflared")`：从 Dock / Finder 打开的 `.app` 拿到的
/// PATH 往往是 `/usr/bin:/bin:/usr/sbin:/sbin`，**没有** Homebrew 的
/// `/opt/homebrew/bin` 或 `/usr/local/bin`。终端里 `cargo run` 正常、装 DMG 后
/// 却提示「没找到 cloudflared」就是这个差异。
fn resolve_cloudflared() -> Result<std::path::PathBuf, String> {
    use std::path::PathBuf;

    if let Ok(p) = std::env::var("SMELT_CLOUDFLARED") {
        let p = PathBuf::from(p.trim());
        if p.is_file() {
            return Ok(p);
        }
        return Err(format!(
            "SMELT_CLOUDFLARED={p:?} 不是可执行文件"
        ));
    }

    let mut candidates: Vec<PathBuf> = Vec::new();
    // Homebrew / 常见前缀（Apple Silicon 与 Intel）
    candidates.push(PathBuf::from("/opt/homebrew/bin/cloudflared"));
    candidates.push(PathBuf::from("/usr/local/bin/cloudflared"));
    candidates.push(PathBuf::from("/usr/bin/cloudflared"));
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".local/bin/cloudflared"));
        candidates.push(home.join("bin/cloudflared"));
        // 部分用户用 brew --prefix 装在自定义路径；仍可通过下面 PATH / login shell 兜底
    }
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            if dir.is_empty() {
                continue;
            }
            candidates.push(PathBuf::from(dir).join("cloudflared"));
        }
    }

    for c in &candidates {
        if c.is_file() {
            return Ok(c.clone());
        }
    }

    // 最后手段：登录 shell 的 PATH（GUI 进程 PATH 太瘦时，zsh -lc 往往还能 which 到）
    #[cfg(target_os = "macos")]
    {
        for shell in ["/bin/zsh", "/bin/bash"] {
            let Ok(out) = std::process::Command::new(shell)
                .args(["-lc", "command -v cloudflared"])
                .output()
            else {
                continue;
            };
            if !out.status.success() {
                continue;
            }
            let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if p.is_empty() {
                continue;
            }
            let p = PathBuf::from(p);
            if p.is_file() {
                return Ok(p);
            }
        }
    }

    Err(
        "没找到 cloudflared（Dock 启动的 App 读不到 brew 的 PATH 是正常的）。\
         请确认已安装：brew install cloudflared；或设置环境变量 SMELT_CLOUDFLARED=绝对路径"
            .into(),
    )
}

/// 幂等：已经开着直接回现有 URL。会先确保本机远程网关已经按 `write` 开着（隧道
/// 要转发给它），没开或写权限对不上会顺带用默认参数（回环 + 随机端口）开/重开一个。
///
/// 强制 `--protocol http2`：quick tunnel 默认先试 QUIC，网络挡 UDP/QUIC 时（不少
/// 企业网/部分云环境如此）要退化重试好几轮才会换协议，直接指定 http2 跳过这段
/// 摸索，换一点 QUIC 本可能带来的延迟优势，换更快、更可预期的建连。
fn start_tunnel(
    tunnel_state: &TunnelState,
    remote_state: &RemoteState,
    write: bool,
) -> Result<(String, bool), String> {
    {
        let mut guard = tunnel_state.lock().unwrap();
        match guard.as_ref() {
            Some(TunnelSlot::Up { url, .. }) => {
                // 幂等：已开就不重启。write 以网关现状为准；想改权限得先 stop 再开。
                let effective_write =
                    remote_state.lock().unwrap().as_ref().map(|g| g.write).unwrap_or(write);
                return Ok((url.clone(), effective_write));
            }
            Some(TunnelSlot::Starting) => {
                return Err("隧道正在启动，请稍后再试".into());
            }
            None => {
                // 占位：挡住并发 start 在放锁后各起一个 cloudflared 的竞态。
                *guard = Some(TunnelSlot::Starting);
            }
        }
    }

    let start_result = (|| {
        let (_, addr, effective_write) = ensure_remote_gateway_with_write(remote_state, write)?;

        use std::process::{Command, Stdio};
        let cloudflared = resolve_cloudflared()?;
        let mut child = Command::new(&cloudflared)
            .arg("tunnel")
            .arg("--protocol")
            .arg("http2")
            .arg("--url")
            .arg(format!("http://{addr}"))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    format!(
                        "无法执行 {}（文件在但启动失败？）：{e}",
                        cloudflared.display()
                    )
                } else {
                    format!("启动 cloudflared（{}）失败：{e}", cloudflared.display())
                }
            })?;

        let (tx, rx) = std::sync::mpsc::channel::<TunnelEvent>();
        if let Some(out) = child.stdout.take() {
            spawn_tunnel_output_scanner(out, tx.clone(), Arc::clone(tunnel_state));
        }
        if let Some(err) = child.stderr.take() {
            spawn_tunnel_output_scanner(err, tx, Arc::clone(tunnel_state));
        }

        // 45s 内必须同时等到 URL 和"已连接"确认；若 cloudflared 已打出明确错误则立刻失败。
        let deadline = std::time::Instant::now() + Duration::from_secs(45);
        let mut url: Option<String> = None;
        let mut connected = false;
        let mut last_fail: Option<String> = None;
        let url = loop {
            if connected {
                if let Some(u) = url {
                    break u;
                }
            }
            // 进程已退出且还没成功 → 用已抓到的失败信息
            if matches!(child.try_wait(), Ok(Some(_))) {
                let _ = child.wait();
                return Err(format_tunnel_timeout_err(last_fail.as_deref(), true));
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format_tunnel_timeout_err(last_fail.as_deref(), false));
            }
            match rx.recv_timeout(remaining) {
                Ok(TunnelEvent::Url(u)) => url = Some(u),
                Ok(TunnelEvent::Connected) => connected = true,
                Ok(TunnelEvent::Failed(msg)) => {
                    last_fail = Some(msg.clone());
                    // 申请 quick tunnel 失败通常进程会很快退出；先记下来，下一轮 try_wait
                    // 或再次 Failed 再收尾。若已是明确 "failed to request" 则立刻失败。
                    let lower = msg.to_ascii_lowercase();
                    if lower.contains("failed to request")
                        || lower.contains("failed to create")
                        || lower.contains("no such host")
                    {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(format_tunnel_timeout_err(Some(&msg), true));
                    }
                }
                Err(_) => {
                    // recv 超时：可能是 deadline 到了，回到 loop 顶检查
                }
            }
        };

        Ok((child, url, effective_write))
    })();

    match start_result {
        Ok((child, url, effective_write)) => {
            *tunnel_state.lock().unwrap() = Some(TunnelSlot::Up {
                child,
                url: url.clone(),
            });
            Ok((url, effective_write))
        }
        Err(e) => {
            // 清掉 Starting 占位，允许下次重试；若中途被 stop_tunnel 清过则保持 None。
            let mut guard = tunnel_state.lock().unwrap();
            if matches!(guard.as_ref(), Some(TunnelSlot::Starting)) {
                *guard = None;
            }
            Err(e)
        }
    }
}

fn stop_tunnel(state: &TunnelState) {
    match state.lock().unwrap().take() {
        Some(TunnelSlot::Up { mut child, .. }) => {
            let _ = child.kill();
            let _ = child.wait(); // 收尸，避免僵尸进程
        }
        Some(TunnelSlot::Starting) | None => {
            // Starting：start_tunnel 失败路径会自己清；这里 take 掉占位可打断并发等待方的假设
        }
    }
}

/// 把超时/进程退出收成用户可读错误。`last_fail` 来自 cloudflared 日志（若有）。
fn format_tunnel_timeout_err(last_fail: Option<&str>, process_exited: bool) -> String {
    let hint = "当前网络可能访问不了 Cloudflare Quick Tunnel（api.trycloudflare.com）。\
                可换网络 / 开代理后再试，或仅用本机/局域网链接。";
    match (last_fail, process_exited) {
        (Some(msg), true) => format!("cloudflared 建隧道失败：{msg}。{hint}"),
        (Some(msg), false) => format!("cloudflared 建隧道超时（45s）：{msg}。{hint}"),
        (None, true) => format!("cloudflared 已退出，未拿到公网链接。{hint}"),
        (None, false) => format!("等 cloudflared 建好隧道超时（45s）。{hint}"),
    }
}

/// 顺带自愈：cloudflared 意外退出时 `try_wait` 清状态，不让 GUI 一直显示死链。
fn tunnel_status(state: &TunnelState) -> Option<String> {
    let mut guard = state.lock().unwrap();
    match guard.as_mut() {
        Some(TunnelSlot::Up { child, url }) => {
            if matches!(child.try_wait(), Ok(Some(_))) {
                *guard = None;
                None
            } else {
                Some(url.clone())
            }
        }
        Some(TunnelSlot::Starting) => None, // 还没 URL，对外等同未运行
        None => None,
    }
}

/// macOS 顶部状态栏常驻图标（accessory 模式：**没有 Dock 图标、不进 ⌘Tab**，只在菜单栏
/// 留一枚图标）。smeltd 本是无 UI 的守护，但被 GUI 拉起时继承了登录会话、连得上
/// WindowServer，于是在这里挂个图标当常驻入口——即便 workspace 主窗口关了、图标仍在。
/// 跟 `workspace/status_item.rs` 同一路数（绕开框架直接摸 AppKit），但更简单：菜单是
/// 静态两项，不随会话状态重建。仅在 `SMELT_MENUBAR` 存在（即由 GUI 拉起）时才被调用。
#[cfg(target_os = "macos")]
mod menubar {
    use objc::declare::ClassDecl;
    use objc::runtime::{Class, Object, Sel};
    use objc::{class, msg_send, sel, sel_impl};
    use std::sync::OnceLock;

    /// 应用图标母图，编进二进制当菜单栏图标（跟 workspace 用的是同一张）。
    const APP_ICON_PNG: &[u8] = include_bytes!("../../../assets/icon-1024.png");

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct NSSize {
        width: f64,
        height: f64,
    }

    /// 点「打开 smelt」：拉起同目录的 GUI（dev 的 target 目录和 app 包内都叫 smelt）。
    /// 已在跑的话，由 GUI 自己的单实例逻辑负责前置窗口，这里只管发起。
    extern "C" fn on_open(_this: &Object, _cmd: Sel, _sender: *mut Object) {
        if let Ok(exe) = std::env::current_exe() {
            use std::process::Stdio;
            let gui = exe.with_file_name("smelt");
            let _ = std::process::Command::new(gui)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
        }
    }

    /// 点「退出 smelt」：整个守护进程退出。注意这会关掉所有 PTY——所有会话（含正在
    /// 跑的 agent）随之结束。后果已写进菜单项文案里。先清 tunnel/远程网关，避免
    /// cloudflared 孤儿化。
    extern "C" fn on_quit(_this: &Object, _cmd: Sel, _sender: *mut Object) {
        super::cleanup_sidecar_services();
        std::process::exit(0);
    }

    /// 注册（仅一次）点击靶子类：AppKit 菜单项只认 target-action，不认 Rust 闭包，
    /// 得声明一个最小的 `NSObject` 子类当靶子（同 status_item.rs 的做法）。
    fn target_class() -> Result<&'static Class, String> {
        static CLASS: OnceLock<&'static Class> = OnceLock::new();
        if let Some(c) = CLASS.get() {
            return Ok(*c);
        }
        // 已注册过则直接取，避免 ClassDecl::new 返回 None 再 expect 崩掉守护。
        if let Some(existing) = Class::get("SmeltdMenubarTarget") {
            let _ = CLASS.set(existing);
            return Ok(existing);
        }
        let mut decl = ClassDecl::new("SmeltdMenubarTarget", class!(NSObject))
            .ok_or_else(|| "无法声明 SmeltdMenubarTarget".to_string())?;
        unsafe {
            decl.add_method(
                sel!(smeltdOpen:),
                on_open as extern "C" fn(&Object, Sel, *mut Object),
            );
            decl.add_method(
                sel!(smeltdQuit:),
                on_quit as extern "C" fn(&Object, Sel, *mut Object),
            );
        }
        let cls = decl.register();
        let _ = CLASS.set(cls);
        Ok(cls)
    }

    /// `&str` → 临时 `NSString*`（autorelease，仅供本次调用当参数用）。
    unsafe fn nsstring(s: &str) -> *mut Object {
        let c = std::ffi::CString::new(s).unwrap_or_default();
        msg_send![class!(NSString), stringWithUTF8String: c.as_ptr()]
    }

    /// 建菜单栏图标 + 静态菜单，然后跑 AppKit runloop（阻塞到进程退出）。
    /// **必须在主线程调用。** 图标、菜单、靶子实例都常驻到进程退出，故意不释放。
    ///
    /// AppKit 类拿不到时（cargo 直接跑 / 无 GUI 会话 / 框架未加载）返回 Err——
    /// **绝不能 panic**：accept 在别的线程上，主线程 panic 会把整个守护带走，
    /// 留下僵尸 sock，GUI 所有新建会话全失败（表现为「加项目没反应」）。
    pub fn run_event_loop() -> Result<(), String> {
        // class! 宏在类不存在时直接 panic；先用 Class::get 探测。
        if Class::get("NSApplication").is_none() {
            return Err("NSApplication 不可用（AppKit 未加载）".into());
        }
        unsafe {
            let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];
            // accessory：不占 Dock、不进 ⌘Tab，只在菜单栏留一枚图标。
            // NSApplicationActivationPolicyAccessory == 1。
            let _: bool = msg_send![app, setActivationPolicy: 1i64];

            let bar: *mut Object = msg_send![class!(NSStatusBar), systemStatusBar];
            // NSVariableStatusItemLength == -1.0，按内容自适应宽度。
            let item: *mut Object = msg_send![bar, statusItemWithLength: -1.0f64];
            let _: () = msg_send![item, retain]; // 常驻单例，自己按住

            let button: *mut Object = msg_send![item, button];
            let data: *mut Object = msg_send![
                class!(NSData),
                dataWithBytes: APP_ICON_PNG.as_ptr() as *const std::ffi::c_void
                length: APP_ICON_PNG.len()
            ];
            let image: *mut Object = msg_send![class!(NSImage), alloc];
            let image: *mut Object = msg_send![image, initWithData: data];
            if !image.is_null() {
                // 母图 1024×1024，菜单栏按 18pt 显示（跟系统自带图标观感对齐）。
                let _: () = msg_send![image, setSize: NSSize { width: 18.0, height: 18.0 }];
                let _: () = msg_send![button, setImage: image];
            } else {
                let _: () = msg_send![button, setTitle: nsstring("smelt")];
            }

            let target_cls = target_class()?;
            let target: *mut Object = msg_send![target_cls, new]; // +1，永不 release
            let menu: *mut Object = msg_send![class!(NSMenu), new]; // +1，永不 release

            let open_item: *mut Object = msg_send![class!(NSMenuItem), alloc];
            let open_item: *mut Object = msg_send![open_item,
                initWithTitle: nsstring("打开 smelt")
                action: sel!(smeltdOpen:)
                keyEquivalent: nsstring("")];
            let _: () = msg_send![open_item, setTarget: target];
            let _: () = msg_send![menu, addItem: open_item];
            let _: () = msg_send![open_item, release];

            let sep: *mut Object = msg_send![class!(NSMenuItem), separatorItem];
            let _: () = msg_send![menu, addItem: sep];

            let quit_item: *mut Object = msg_send![class!(NSMenuItem), alloc];
            let quit_item: *mut Object = msg_send![quit_item,
                initWithTitle: nsstring("退出 smelt（结束所有会话）")
                action: sel!(smeltdQuit:)
                keyEquivalent: nsstring("")];
            let _: () = msg_send![quit_item, setTarget: target];
            let _: () = msg_send![menu, addItem: quit_item];
            let _: () = msg_send![quit_item, release];

            let _: () = msg_send![item, setMenu: menu];

            // 阻塞跑 runloop：菜单点击的 target-action 全靠它派发。
            let _: () = msg_send![app, run];
        }
        Ok(())
    }
}


fn main() {
    // 钉住启动时刻：晚一步取到的就是「首次有人问 version」的时间，不是启动时间。
    started_at();
    // 无缝升级交接：上一代进程 exec 本二进制前写好交接文件并把路径放在环境变量里。
    // 立即摘掉环境变量：它只对"本次 exec 交接"有意义，不能传染给之后 spawn 的 shell。
    let handoff = std::env::var("SMELTD_HANDOFF").ok();
    // Edition 2024：`remove_var` 标为 unsafe（多线程改 env 非同步）。
    // 此处在 main 最开头、尚未 spawn 任何线程，单线程访问安全。
    unsafe { std::env::remove_var("SMELTD_HANDOFF") };
    let came_from_handoff = handoff.is_some();

    let path = sock_path();
    // 不参与无缝升级交接：每次进程启动（含 upgrade 后的新进程）都是全新的空列表——
    // subscribe 连接是网络层面的东西，跟 out.client/watchers 一样没必要假装还在。
    // 建在 resume_handoff 之前：交接恢复的会话也需要一份 Subscribers 去广播状态。
    let subscribers: Subscribers = Arc::new(Mutex::new(Vec::new()));
    let (listener, sessions, acp_sessions) = match handoff.and_then(|p| resume_handoff(&p, &subscribers)) {
        Some(x) => {
            let acp_n = x.2.lock().map(|s| s.len()).unwrap_or(0);
            dlog(&format!(
                "upgrade: 交接完成，恢复 {} 个终端会话 + {acp_n} 个 ACP 会话",
                x.1.lock().map(|s| s.len()).unwrap_or(0)
            ));
            x
        }
        None => {
            if came_from_handoff {
                dlog("upgrade: 交接文件恢复失败，走全新启动（会话丢失但守护存活）");
            }
            // 单实例检查只在「不是从交接来的」这条路径上做：能连上说明已有活守护，
            // 直接退出。若 came_from_handoff 为真，说明本进程就是刚从上一代 exec
            // 过来的替身——这种情况下绝不能做这个检查：上一代把监听 fd 的 CLOEXEC
            // 清掉了，我们已经继承着它，此时 connect 这个 path 会连上我们自己继承
            // 的那份监听 fd（进 backlog 即成功），于是把「自己」误判成「已有别的
            // 守护」而直接 return 退出——刚交接过来的进程当场自杀，所有会话陪葬。
            // 交接失败时唯一正确的动作是：忽略那份不可追溯的旧监听 fd（它会作为
            // 一个泄漏的 fd 留在本进程里，无害但也无法优雅关闭——resume_handoff
            // 失败通常发生在 JSON 都解析不出来的极端情况，代价可接受），把 socket
            // 文件净空重 bind，保证守护本身不能倒。
            if !came_from_handoff && UnixStream::connect(&path).is_ok() {
                return;
            }
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(handoff_path()); // 清掉可能残留的上次交接文件
            let listener = match UnixListener::bind(&path) {
                Ok(l) => l,
                Err(e) => {
                    // 曾经是静默 return：守护无声消失、sock 残留，外面完全查不到
                    // 死因（排障时被坑过——必须留痕）。
                    dlog(&format!("bind {} 失败，守护退出：{e}", path.display()));
                    return;
                }
            };
            // socket 仅本用户可读写。
            let _ = std::fs::set_permissions(
                &path,
                std::os::unix::fs::PermissionsExt::from_mode(0o600),
            );
            (listener, Arc::new(Mutex::new(HashMap::new())), Arc::new(Mutex::new(HashMap::new())))
        }
    };

    let listen_fd = listener.as_raw_fd();
    let exe_mtime = exe_mtime_secs();
    // 不参与无缝升级交接：每次进程启动（含 upgrade 后的新进程）都是全新的 None，
    // 见 RemoteGateway / Tunnel 定义处注释。
    let remote_state: RemoteState = Arc::new(Mutex::new(None));
    let tunnel_state: TunnelState = Arc::new(Mutex::new(None));
    // acp_sessions 现在参与无缝升级交接了（见上面 resume_handoff 的返回值）：
    // 正常冷启动时是空表，upgrade 交接恢复时带着接过来的会话。
    // 菜单栏 quit / 任何路径 cleanup 都要够得着这两份状态。
    register_lifecycle(Arc::clone(&remote_state), Arc::clone(&tunnel_state));

    // thread-per-connection 的 accept 主循环。抽成闭包，好让主线程在 macOS 上腾出来
    // 跑菜单栏 runloop——AppKit 铁律：NSApplication/NSStatusItem 只能在主线程摸。
    let accept_loop = move || {
        for conn in listener.incoming() {
            let Ok(conn) = conn else { continue };
            let sessions = Arc::clone(&sessions);
            let acp_sessions = Arc::clone(&acp_sessions);
            let remote_state = Arc::clone(&remote_state);
            let tunnel_state = Arc::clone(&tunnel_state);
            let subscribers = Arc::clone(&subscribers);
            thread::spawn(move || {
                handle_conn(
                    conn,
                    sessions,
                    acp_sessions,
                    exe_mtime,
                    listen_fd,
                    remote_state,
                    tunnel_state,
                    subscribers,
                )
            });
        }
    };

    // 只有被 GUI 拉起时（SMELT_MENUBAR=1，说明继承了登录会话、连得上 WindowServer）
    // 才在顶部状态栏挂图标；命令行 / 无 GUI 会话下老老实实 headless 跑，绝不让「图标」
    // 这个锦上添花的东西把守护本身拖垮。
    //
    // 菜单栏失败时必须继续 accept：历史上 SMELT_MENUBAR 路径在 NSApplication 缺失时
    // panic，整个守护带走、只剩僵尸 sock → GUI 所有「打开项目 / 拖入 / +」全失败。
    #[cfg(target_os = "macos")]
    if std::env::var_os("SMELT_MENUBAR").is_some() {
        let daemon = thread::spawn(accept_loop);
        match menubar::run_event_loop() {
            Ok(()) => {
                // runloop 正常结束（菜单「退出」走 process::exit，一般到不了这里）
                let _ = daemon.join();
            }
            Err(e) => {
                dlog(&format!("menubar 不可用，守护继续 headless：{e}"));
                // accept 在后台线程，主线程 join 撑住进程，效果等同 headless accept_loop
                let _ = daemon.join();
            }
        }
        return;
    }

    accept_loop();
}

/// 交接文件路径（跟 socket 同目录）。
fn handoff_path() -> std::path::PathBuf {
    sock_path().with_file_name("handoff.json")
}

/// 从交接文件恢复：认领监听 socket 和各会话的 PTY master fd，重建会话表 + 泵线程。
/// 任何全局性错误（文件读不到/解析失败/监听 fd 无效）返回 None 走全新启动——会话
/// 保不住但守护必须活着；单个会话的 fd 坏了只跳过那一个。
fn resume_handoff(
    path: &str,
    subscribers: &Subscribers,
) -> Option<(UnixListener, Sessions, AcpSessions)> {
    let data = std::fs::read_to_string(path).ok()?;
    let _ = std::fs::remove_file(path); // 读到手就删，避免残留被下次启动误认
    let v: serde_json::Value = serde_json::from_str(&data).ok()?;

    let listen_fd = v["listen_fd"].as_i64()? as RawFd;
    // 校验这个 fd 真的有效（exec 前若忘了清 CLOEXEC，这里会拿到无效 fd）。
    if unsafe { libc::fcntl(listen_fd, libc::F_GETFD) } < 0 {
        return None;
    }
    set_cloexec(listen_fd, true);
    let listener = unsafe { UnixListener::from_raw_fd(listen_fd) };

    let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
    for item in v["sessions"].as_array().map(|a| a.as_slice()).unwrap_or_default() {
        let Some(id) = item["id"].as_str() else { continue };
        let fd = item["fd"].as_i64().unwrap_or(-1) as RawFd;
        let pid = item["pid"].as_i64().unwrap_or(0) as i32;
        if fd < 0 || unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
            continue; // fd 本身缺失/已失效，没有可恢复的东西
        }
        if pid <= 0 {
            // fd 有效但 pid 信息坏了：没法按 pid 去 waitpid/kill 这个孤儿 shell，
            // 干脆关掉 master fd——PTY 挂断会让前台进程组收到 SIGHUP，大概率跟着
            // 退出；不关的话这个 fd 就白白泄漏在新进程里，永远够不着。
            unsafe {
                libc::close(fd);
            }
            continue;
        }
        set_cloexec(fd, true);
        let master = unsafe { std::fs::File::from_raw_fd(fd) };
        let Ok(reader) = master.try_clone() else {
            // master 已被 from_raw_fd 接管，这里 drop 会关掉 fd（PTY 挂断，shell
            // 大概率收到 SIGHUP 退出）；但没有泵线程去 waitpid，起一个一次性收尸
            // 线程，避免它在进程表里挂成永久僵尸。
            drop(master);
            thread::spawn(move || unsafe {
                libc::waitpid(pid, std::ptr::null_mut(), 0);
            });
            continue;
        };
        let cols = item["cols"].as_u64().unwrap_or(80) as u16;
        let rows = item["rows"].as_u64().unwrap_or(24) as u16;
        let cwd = item["cwd"].as_str().map(String::from);
        let launch = item["launch"].as_str().map(String::from);
        let alt_flag = item["alt_screen"].as_bool().unwrap_or(false);
        // 旧 handoff 文件可能仍带 "buf"（环形原始字节）——**忽略，永不 feed**。
        // 状态通道不参与交接：新进程里全新一份 SessionState（launch 会写回，便于
        // snapshot 识别 agent）。hook/OSC 很快会补 phase/title。
        let state = Arc::new(Mutex::new(SessionState {
            id: id.to_string(),
            cwd: cwd.clone(),
            launch: launch.clone(),
            ..Default::default()
        }));
        // —— 画面恢复：全会话同一条路径，不按 shell/TUI/agent 分支 ——
        //
        // 唯一信源：upgrade 时从常驻 Term 导出的 viewport keyframe（`grid`）。
        // 环形字节可能在 CSI 中间腰斩，**永远不 feed**（按类型特判 ring = 拆东墙补西墙）。
        //
        // 无 grid（极老交接文件）：若交接前在备用屏，只注 1049h 模式位；其余空白 + jolt。
        let listener =
            StateListener { state: Arc::clone(&state), subscribers: Arc::clone(subscribers) };
        let mut term = new_daemon_term(rows, cols, listener);
        let grid = item["grid"].as_str().and_then(hex_decode).unwrap_or_default();
        let was_alt = alt_flag || buf_looks_like_alt_screen(&grid);
        if !grid.is_empty() {
            feed_term(&mut term, &grid);
            dlog(&format!(
                "handoff: 恢复会话 id={id} rows={rows} cols={cols} alt={was_alt} launch={:?} grid_len={} (feed keyframe)",
                launch,
                grid.len()
            ));
        } else if was_alt || alt_flag {
            feed_term(&mut term, b"\x1b[?1049h");
            dlog(&format!(
                "handoff: 恢复会话 id={id} rows={rows} cols={cols} alt=true launch={:?} (无 grid，仅 1049h + jolt)",
                launch
            ));
        } else {
            dlog(&format!(
                "handoff: 恢复会话 id={id} rows={rows} cols={cols} alt=false launch={:?} (无 grid，空 Term + jolt)",
                launch
            ));
        }
        let sess = Arc::new(Session {
            ctl: Mutex::new(Ctl {
                master,
                pid,
                // 一律 jolt：有 grid 时对齐真 cell 尺寸；无 grid 时逼进程自绘。
                jolt: true,
                cols,
                rows,
                cwd,
            }),
            out: Mutex::new(Out {
                client: None,
                watchers: Vec::new(),
            }),
            term: Mutex::new(term),
            state,
        });
        sessions.lock().unwrap().insert(id.to_string(), Arc::clone(&sess));
        start_pty_pump(sess, Box::new(reader), id.to_string(), Arc::clone(&sessions));
    }

    // ACP 会话：fd 裸传跟终端同一招，多一步"回放 pending_raw_line 再接上
    // 实时字节"（见 acp_conn::resume_acp_from_fds），把交接过来的快照数据
    // 重建成活体状态。
    let acp_sessions: AcpSessions = Arc::new(Mutex::new(HashMap::new()));
    for item in v["acp_sessions"].as_array().map(|a| a.as_slice()).unwrap_or_default() {
        let Some(id) = item["id"].as_str() else { continue };
        let stdin_fd = item["stdin_fd"].as_i64().unwrap_or(-1) as RawFd;
        let stdout_fd = item["stdout_fd"].as_i64().unwrap_or(-1) as RawFd;
        let pid = item["pid"].as_i64().unwrap_or(0) as i32;
        if stdin_fd < 0
            || stdout_fd < 0
            || unsafe { libc::fcntl(stdin_fd, libc::F_GETFD) } < 0
            || unsafe { libc::fcntl(stdout_fd, libc::F_GETFD) } < 0
        {
            continue; // fd 缺失/已失效，没有可恢复的东西
        }
        if pid <= 0 {
            // pid 坏了没法在需要时 kill 这个孤儿 agent；关掉两个 fd（agent 读到
            // stdin EOF 大概率自己退出，同终端那边同款兜底思路）。
            unsafe {
                libc::close(stdin_fd);
                libc::close(stdout_fd);
            }
            continue;
        }
        let Some(snapshot_v) = item.get("snapshot") else { continue };
        let Ok(snapshot) =
            serde_json::from_value::<smelt_core::acp_session::AcpSnapshot>(snapshot_v.clone())
        else {
            continue;
        };
        let Some(acp_session_id) = snapshot.acp_session_id.clone() else {
            // 没有 agent 侧 session id 就没法直接 attach_session——理论上不该
            // 发生（能撑到升级这一刻的会话早就握手成功过一次）。防御性地跟
            // pid 坏了同样处理：关 fd + 杀子进程，不留孤儿。
            unsafe {
                libc::close(stdin_fd);
                libc::close(stdout_fd);
                libc::kill(pid, libc::SIGKILL);
            }
            continue;
        };
        set_cloexec(stdin_fd, true);
        set_cloexec(stdout_fd, true);

        let cwd = item["cwd"].as_str().map(String::from);
        let cmd = item["cmd"].as_str().unwrap_or_default().to_string();
        let agent_needs_transcript_check =
            item["agent_needs_transcript_check"].as_bool().unwrap_or(false);
        let pending_raw_line = item["pending_raw_line"].as_str().map(String::from);
        let supports_image = snapshot.supports_image;
        let reduced = smelt_core::acp_session::AcpSessionState::from_snapshot(snapshot);

        let state = Arc::new(Mutex::new(SessionState {
            id: id.to_string(),
            cwd: cwd.clone(),
            launch: Some(cmd),
            ..Default::default()
        }));
        let sess = Arc::new(AcpSession {
            reduced: Mutex::new(reduced),
            handle: Mutex::new(None),
            cwd,
            agent_needs_transcript_check,
            state,
            out: Mutex::new(AcpOut { client: None, watchers: Vec::new() }),
        });

        let handle = smelt_core::acp_conn::resume_acp_from_fds(
            id.to_string(),
            stdin_fd,
            stdout_fd,
            pid,
            acp_session_id,
            supports_image,
            pending_raw_line,
        );
        let event_rx = handle.event_rx.clone();
        *sess.handle.lock().unwrap() = Some(handle);
        acp_sessions.lock().unwrap().insert(id.to_string(), Arc::clone(&sess));
        // 落地就有一份现成快照，不用等下一次协议事件才让 subscribe 订阅者
        // 看到这条会话——跟终端那边"resume 完成靠后续 PTY 输出自然触发广播"
        // 不同，ACP 没有"泵线程闲着也吐字节"这回事。
        update_acp_daemon_state(&sess, subscribers);
        start_acp_event_drain(sess, event_rx, subscribers.clone());
    }
    Some((listener, sessions, acp_sessions))
}

/// 把字节喂进常驻 Term；panic 时吞掉，避免畸形序列拖死整个守护。
fn feed_term<T: EventListener>(term: &mut Term<T>, bytes: &[u8]) {
    let mut parser: Processor = Processor::new();
    let _ = catch_unwind(AssertUnwindSafe(|| {
        parser.advance(term, bytes);
    }));
}

fn buf_looks_like_alt_screen(buf: &[u8]) -> bool {
    buf.windows(8).any(|w| w == b"\x1b[?1049h")
}

/// keyframe / 交接 payload 的二进制字段编码（hex，无额外依赖）。
fn hex_encode(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for byte in b {
        s.push_str(&format!("{byte:02x}"));
    }
    s
}

/// 按字节解码 hex——交接文件是外部数据（可能损坏/被篡改）；全程字节级 match，
/// 不用 `&s[i..i+2]`，避免非字符边界 panic（resume 时 panic = 全会话陪葬）。
fn hex_decode(s: &str) -> Option<Vec<u8>> {
    fn nibble(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }
    let b = s.as_bytes();
    if b.len() % 2 != 0 {
        return None;
    }
    (0..b.len()).step_by(2).map(|i| Some((nibble(b[i])? << 4) | nibble(b[i + 1])?)).collect()
}

/// `resume_handoff` 的行为——这是「无缝升级」的落地点，也是全文件最该被守住的一段：
/// 它一旦出错，用户正在跑的 agent 会话会在升级瞬间集体消失，且没有任何补救。
/// 此前这里**一个测试都没有**，几条要命的不变量全靠注释。
#[cfg(test)]
mod resume_handoff_tests {
    use super::*;

    fn no_subs() -> Subscribers {
        Arc::new(Mutex::new(Vec::new()))
    }

    /// 每个用例一个独立文件名：测试是多线程并行跑的，共用路径会互相踩。
    fn tmp_handoff(name: &str) -> String {
        std::env::temp_dir()
            .join(format!("smelt-test-handoff-{name}-{}.json", std::process::id()))
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn missing_file_returns_none() {
        let p = tmp_handoff("missing");
        let _ = std::fs::remove_file(&p);
        assert!(resume_handoff(&p, &no_subs()).is_none());
    }

    #[test]
    fn malformed_json_returns_none() {
        let p = tmp_handoff("malformed");
        std::fs::write(&p, "{ this is not json").unwrap();
        assert!(
            resume_handoff(&p, &no_subs()).is_none(),
            "解析失败必须走全新启动，而不是 panic 把守护带走"
        );
        let _ = std::fs::remove_file(&p);
    }

    /// 读到手就删：文件残留下来会被下次启动误认成「有交接要恢复」，
    /// 那时里面的 fd 早已属于别的东西。失败路径也必须删。
    #[test]
    fn consumes_handoff_file_even_when_parse_fails() {
        let p = tmp_handoff("consume");
        std::fs::write(&p, "{ not json").unwrap();
        let _ = resume_handoff(&p, &no_subs());
        assert!(
            !std::path::Path::new(&p).exists(),
            "handoff 文件读完必须删掉，无论恢复成功与否"
        );
    }

    #[test]
    fn missing_listen_fd_returns_none() {
        let p = tmp_handoff("no-listen-fd");
        std::fs::write(&p, r#"{"sessions":[]}"#).unwrap();
        assert!(resume_handoff(&p, &no_subs()).is_none());
        let _ = std::fs::remove_file(&p);
    }

    /// exec 前忘了清 CLOEXEC 的话，这里拿到的就是无效 fd——必须识别出来走全新启动，
    /// 而不是把一个野 fd 当监听 socket 用。
    /// 一个绝不会被分配到的 fd 号：远超 ulimit -n，fcntl 必然 EBADF。
    ///
    /// 不能用「open 一个再 close，拿它的号当无效 fd」——测试是多线程并行跑的，
    /// 号一释放就会被别的用例的 pipe() 拿去，于是「无效 fd」其实是别人的活 fd，
    /// resume_handoff 接管后 close 掉，对面就 double close：
    /// `IO Safety violation: owned file descriptor already closed`。这里踩过。
    const NEVER_VALID_FD: RawFd = 1_000_000;

    /// exec 前忘了清 CLOEXEC 的话，这里拿到的就是无效 fd——必须识别出来走全新启动，
    /// 而不是把一个野 fd 当监听 socket 用。
    #[test]
    fn invalid_listen_fd_returns_none() {
        let p = tmp_handoff("bad-listen-fd");
        std::fs::write(
            &p,
            format!(r#"{{"listen_fd":{NEVER_VALID_FD},"sessions":[]}}"#),
        )
        .unwrap();
        assert!(resume_handoff(&p, &no_subs()).is_none());
        let _ = std::fs::remove_file(&p);
    }

    /// 造一个能被 resume_handoff 认领的监听 fd。
    fn make_listen_fd(name: &str) -> RawFd {
        let sock = std::env::temp_dir()
            .join(format!("smelt-test-{name}-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let l = UnixListener::bind(&sock).unwrap();
        let _ = std::fs::remove_file(&sock); // 已 bind，文件可以立刻删
        std::os::unix::io::IntoRawFd::into_raw_fd(l)
    }

    /// 造一个「PTY master」替身：用管道写端即可——resume_handoff 只是接管 fd、
    /// try_clone 给泵线程，测试不需要真的跑一个 shell。
    /// 返回 (master_fd, 读端保管者, pid)。
    fn make_fake_pty() -> (RawFd, std::fs::File, i32) {
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe() 失败");
        let read_end = unsafe { std::fs::File::from_raw_fd(fds[0]) };
        // 用一个真实存在过的 pid：让泵线程结束时的 waitpid 有合法目标，不借用 -1
        let child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id() as i32;
        drop(child); // 留成 zombie，交给泵收尸
        (fds[1], read_end, pid)
    }

    fn term_text_of(sess: &Arc<Session>) -> String {
        let term = sess.term.lock().unwrap();
        term_text::text_lines(&term).join("\n")
    }

    /// **永不 feed ring**——本文件头号不变量，此前只有注释在守。
    ///
    /// 旧版 handoff 文件会带 `"buf"`（每会话的环形原始字节）。环形缓冲是按容量截断的，
    /// 截断点可能正落在一条 CSI 序列中间，feed 进去必然花屏。所以画面只认从常驻 Term
    /// 导出的 `grid` keyframe，`buf` 即便存在也必须被忽略。
    ///
    /// 这条一旦被「顺手优化」掉（比如有人觉得「没 grid 时用 buf 兜底也行」），
    /// 症状是升级后终端花屏，且只在带旧交接文件的机器上出现——极难复现。
    #[test]
    fn never_feeds_legacy_ring_buffer_even_when_present() {
        let p = tmp_handoff("no-feed-ring");
        let listen_fd = make_listen_fd("no-feed-ring");
        let (master_fd, _read_end, pid) = make_fake_pty();

        let handoff = serde_json::json!({
            "listen_fd": listen_fd,
            "sessions": [{
                "id": "s1",
                "fd": master_fd,
                "pid": pid,
                "cols": 80,
                "rows": 24,
                // 旧字段：必须被忽略
                "buf": hex_encode(b"RINGBUF-MUST-NOT-RENDER"),
                // 唯一信源
                "grid": hex_encode(b"GRIDKEYFRAME-OK"),
            }]
        });
        std::fs::write(&p, handoff.to_string()).unwrap();

        let (_listener, sessions, _acp) = resume_handoff(&p, &no_subs()).expect("应能恢复");
        let sess = sessions.lock().unwrap().get("s1").cloned().expect("会话 s1 应存在");
        let text = term_text_of(&sess);

        assert!(text.contains("GRIDKEYFRAME-OK"), "grid keyframe 应被 feed：{text:?}");
        assert!(
            !text.contains("RINGBUF"),
            "buf（环形原始字节）绝不能被 feed——它可能在 CSI 中间腰斩，feed 必花屏：{text:?}"
        );
    }

    /// **没有 grid、只有 buf 时，仍然不许 feed buf**——这才是「永不 feed ring」真正
    /// 会被破坏的地方：老版交接文件就是只有 buf 没有 grid，一旦有人觉得
    /// 「没 grid 时拿 buf 兜一下也行」，花屏就回来了。
    ///
    /// 上面那条 `never_feeds_legacy_ring_buffer_even_when_present` 挡不住这种改法
    /// （它的用例里 grid 存在，走不到兜底分支）——变异测试实测漏过。两条都要有。
    #[test]
    fn ignores_buf_when_grid_absent() {
        let p = tmp_handoff("buf-no-grid");
        let listen_fd = make_listen_fd("buf-no-grid");
        let (master_fd, _read_end, pid) = make_fake_pty();

        let handoff = serde_json::json!({
            "listen_fd": listen_fd,
            "sessions": [{
                "id": "s1", "fd": master_fd, "pid": pid, "cols": 80, "rows": 24,
                // 老版交接文件的形态：只有 buf，没有 grid
                "buf": hex_encode(b"LEGACYRING-MUST-NOT-RENDER"),
            }]
        });
        std::fs::write(&p, handoff.to_string()).unwrap();

        let (_l, sessions, _acp) = resume_handoff(&p, &no_subs()).expect("应能恢复");
        let sess = sessions.lock().unwrap().get("s1").cloned().unwrap();
        let text = term_text_of(&sess);
        assert!(
            !text.contains("LEGACYRING"),
            "没有 grid 时也不能拿 buf 兜底——环形字节可能在 CSI 中间腰斩，feed 必花屏。\
             宁可空屏 + jolt 让进程自绘：{text:?}"
        );
    }

    /// fd 已失效的会话只跳过它自己，不能拖垮整次恢复——其余会话必须照常回来。
    #[test]
    fn skips_session_with_dead_fd_but_keeps_the_rest() {
        let p = tmp_handoff("dead-fd");
        let listen_fd = make_listen_fd("dead-fd");
        let (good_fd, _read_end, pid) = make_fake_pty();

        let handoff = serde_json::json!({
            "listen_fd": listen_fd,
            "sessions": [
                { "id": "dead", "fd": NEVER_VALID_FD, "pid": pid, "cols": 80, "rows": 24 },
                { "id": "good", "fd": good_fd, "pid": pid, "cols": 80, "rows": 24,
                  "grid": hex_encode(b"ALIVE") },
            ]
        });
        std::fs::write(&p, handoff.to_string()).unwrap();

        let (_l, sessions, _acp) = resume_handoff(&p, &no_subs()).expect("应能恢复");
        let map = sessions.lock().unwrap();
        assert!(!map.contains_key("dead"), "fd 失效的会话应被跳过");
        assert!(map.contains_key("good"), "其余会话必须照常恢复，不能被坏的那个拖垮");
    }

    /// 无 grid、且交接前在备用屏：注 1049h 让 TUI 自己重画，
    /// 而不是把它留在主屏上（那样 agent 的界面会叠在 shell 历史上）。
    #[test]
    fn without_grid_alt_screen_flag_enters_alt_mode() {
        let p = tmp_handoff("alt-no-grid");
        let listen_fd = make_listen_fd("alt-no-grid");
        let (master_fd, _read_end, pid) = make_fake_pty();

        let handoff = serde_json::json!({
            "listen_fd": listen_fd,
            "sessions": [{
                "id": "s1", "fd": master_fd, "pid": pid, "cols": 80, "rows": 24,
                "alt_screen": true,
            }]
        });
        std::fs::write(&p, handoff.to_string()).unwrap();

        let (_l, sessions, _acp) = resume_handoff(&p, &no_subs()).expect("应能恢复");
        let sess = sessions.lock().unwrap().get("s1").cloned().unwrap();
        let term = sess.term.lock().unwrap();
        assert!(
            term.mode().contains(TermMode::ALT_SCREEN),
            "交接前在备用屏、又没有 grid 时，应只注 1049h 把 Term 切回备用屏"
        );
    }

    /// 恢复的会话一律挂 jolt：有 grid 时用于对齐真实 cell 尺寸，无 grid 时逼进程自绘。
    #[test]
    fn restored_session_is_marked_for_jolt() {
        let p = tmp_handoff("jolt");
        let listen_fd = make_listen_fd("jolt");
        let (master_fd, _read_end, pid) = make_fake_pty();

        let handoff = serde_json::json!({
            "listen_fd": listen_fd,
            "sessions": [{
                "id": "s1", "fd": master_fd, "pid": pid, "cols": 80, "rows": 24,
                "grid": hex_encode(b"X"),
            }]
        });
        std::fs::write(&p, handoff.to_string()).unwrap();

        let (_l, sessions, _acp) = resume_handoff(&p, &no_subs()).expect("应能恢复");
        let sess = sessions.lock().unwrap().get("s1").cloned().unwrap();
        assert!(sess.ctl.lock().unwrap().jolt, "恢复的会话必须挂 jolt");
    }

    /// 造一对能被 resume_handoff 接管的假 stdin/stdout fd（管道即可，不需要
    /// 真的能跑 JSON-RPC——resume_acp_from_fds 只是起个线程去读它，本测试不
    /// 关心那条线程后续读到什么，只关心 resume_handoff 这一步的解析/建表
    /// 逻辑对不对）。返回 (stdin_fd, stdout_fd, 两端读写口保管者, pid)。
    fn make_fake_acp_stdio() -> (RawFd, RawFd, (std::fs::File, std::fs::File), i32) {
        let mut in_fds = [0i32; 2];
        let mut out_fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(in_fds.as_mut_ptr()) }, 0);
        assert_eq!(unsafe { libc::pipe(out_fds.as_mut_ptr()) }, 0);
        let child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id() as i32;
        drop(child); // 留成 zombie，交给 resume_acp_from_fds 内部的 KillProcessGroupOnDrop 收尾
        // 写端/读端各自保管一份，避免管道另一头因为「没人拿着」直接 EOF。
        let stdin_fd = in_fds[1]; // 交给 resume_handoff 接管（当作"守护写向 agent"）
        let stdout_fd = out_fds[0]; // 交给 resume_handoff 接管（当作"从 agent 读"）
        let keep_alive = unsafe {
            (std::fs::File::from_raw_fd(in_fds[0]), std::fs::File::from_raw_fd(out_fds[1]))
        };
        (stdin_fd, stdout_fd, keep_alive, pid)
    }

    fn sample_snapshot(acp_session_id: &str) -> smelt_core::acp_session::AcpSnapshot {
        smelt_core::acp_session::AcpSessionState::placeholder(
            vec![smelt_core::acp_chat::AcpEntry::User("hi".into())],
            Some(acp_session_id.to_string()),
            String::new(),
        )
        .to_snapshot(false)
    }

    #[test]
    fn acp_session_with_valid_fds_and_session_id_is_recovered() {
        let p = tmp_handoff("acp-ok");
        let listen_fd = make_listen_fd("acp-ok");
        let (stdin_fd, stdout_fd, _keep_alive, pid) = make_fake_acp_stdio();

        let handoff = serde_json::json!({
            "listen_fd": listen_fd,
            "sessions": [],
            "acp_sessions": [{
                "id": "acp-1",
                "stdin_fd": stdin_fd,
                "stdout_fd": stdout_fd,
                "pid": pid,
                "cwd": "/tmp/proj",
                "cmd": "claude --dangerously-skip-permissions",
                "agent_needs_transcript_check": true,
                "snapshot": sample_snapshot("sid-1"),
                "pending_raw_line": null,
            }],
        });
        std::fs::write(&p, handoff.to_string()).unwrap();

        let (_l, _sessions, acp) = resume_handoff(&p, &no_subs()).expect("应能恢复");
        let sess = acp.lock().unwrap().get("acp-1").cloned().expect("acp-1 应被恢复");
        assert_eq!(sess.cwd.as_deref(), Some("/tmp/proj"));
        assert!(sess.agent_needs_transcript_check);
        assert!(sess.handle.lock().unwrap().is_some(), "应该已经起了 resume 连接");
        let reduced = sess.reduced.lock().unwrap();
        assert_eq!(reduced.entries.len(), 1);
        assert_eq!(reduced.acp_session_id.as_deref(), Some("sid-1"));
    }

    /// 没有 agent 侧 session id 就没法 attach_session——理论上不该发生，但
    /// 交接文件是外部数据，得防御性地跳过而不是 panic 或者留一条永远连不上
    /// 的死会话。
    #[test]
    fn acp_session_without_session_id_is_skipped() {
        let p = tmp_handoff("acp-no-sid");
        let listen_fd = make_listen_fd("acp-no-sid");
        let (stdin_fd, stdout_fd, _keep_alive, pid) = make_fake_acp_stdio();

        let mut snapshot = sample_snapshot("whatever");
        snapshot.acp_session_id = None;
        let handoff = serde_json::json!({
            "listen_fd": listen_fd,
            "sessions": [],
            "acp_sessions": [{
                "id": "acp-2",
                "stdin_fd": stdin_fd,
                "stdout_fd": stdout_fd,
                "pid": pid,
                "cwd": null,
                "cmd": "claude",
                "agent_needs_transcript_check": true,
                "snapshot": snapshot,
                "pending_raw_line": null,
            }],
        });
        std::fs::write(&p, handoff.to_string()).unwrap();

        let (_l, _sessions, acp) = resume_handoff(&p, &no_subs()).expect("应能恢复");
        assert!(acp.lock().unwrap().get("acp-2").is_none());
    }

    /// fd 号本身失效（exec 前忘了清 CLOEXEC 之类）：必须跳过，不能把野 fd
    /// 当成活的 stdin/stdout 去用。
    #[test]
    fn acp_session_with_invalid_fd_is_skipped() {
        let p = tmp_handoff("acp-bad-fd");
        let listen_fd = make_listen_fd("acp-bad-fd");

        let handoff = serde_json::json!({
            "listen_fd": listen_fd,
            "sessions": [],
            "acp_sessions": [{
                "id": "acp-3",
                "stdin_fd": NEVER_VALID_FD,
                "stdout_fd": NEVER_VALID_FD,
                "pid": 1,
                "cwd": null,
                "cmd": "claude",
                "agent_needs_transcript_check": true,
                "snapshot": sample_snapshot("sid-3"),
                "pending_raw_line": null,
            }],
        });
        std::fs::write(&p, handoff.to_string()).unwrap();

        let (_l, _sessions, acp) = resume_handoff(&p, &no_subs()).expect("应能恢复");
        assert!(acp.lock().unwrap().get("acp-3").is_none());
    }

    /// 正卡着一张审批卡片时，pending_raw_line 应该原样透传进 resume_handoff
    /// （具体会不会被正确回放是 acp_conn::resume_acp_from_fds 的职责，这里
    /// 只验证 resume_handoff 这一层没有把它弄丢/挡在门外）。
    #[test]
    fn acp_session_with_pending_raw_line_still_recovers() {
        let p = tmp_handoff("acp-pending");
        let listen_fd = make_listen_fd("acp-pending");
        let (stdin_fd, stdout_fd, _keep_alive, pid) = make_fake_acp_stdio();

        let handoff = serde_json::json!({
            "listen_fd": listen_fd,
            "sessions": [],
            "acp_sessions": [{
                "id": "acp-4",
                "stdin_fd": stdin_fd,
                "stdout_fd": stdout_fd,
                "pid": pid,
                "cwd": null,
                "cmd": "claude",
                "agent_needs_transcript_check": true,
                "snapshot": sample_snapshot("sid-4"),
                "pending_raw_line": r#"{"jsonrpc":"2.0","id":7,"method":"session/request_permission","params":{}}"#,
            }],
        });
        std::fs::write(&p, handoff.to_string()).unwrap();

        let (_l, _sessions, acp) = resume_handoff(&p, &no_subs()).expect("应能恢复");
        assert!(acp.lock().unwrap().get("acp-4").is_some());
    }
}

#[cfg(test)]
mod handoff_tests {
    use super::*;

    /// grid keyframe 的 hex 字段必须逐字节还原。
    #[test]
    fn hex_roundtrip() {
        let data: Vec<u8> = (0..=255u8).cycle().take(4096).collect();
        assert_eq!(hex_decode(&hex_encode(&data)).as_deref(), Some(data.as_slice()));
        assert_eq!(hex_decode("").as_deref(), Some(&[][..]));
        assert_eq!(hex_decode("abc"), None, "奇数长度应判非法");
        assert_eq!(hex_decode("zz"), None, "非 hex 字符应判非法");
    }

    /// 损坏的 hex 字段（多字节 UTF-8）只判非法、绝不 panic。
    #[test]
    fn hex_decode_never_panics_on_multibyte_utf8() {
        assert_eq!(hex_decode("中文"), None); // 6 字节，偶数，非 hex 字符
        assert_eq!(hex_decode("a中"), None); // 1 + 3 字节，奇偶交叉
        assert_eq!(hex_decode("ab中c"), None);
    }

    #[test]
    fn buf_detects_alt_screen() {
        assert!(buf_looks_like_alt_screen(b"\x1b[?1049hTUI"));
        assert!(!buf_looks_like_alt_screen(b"plain shell"));
    }
}

#[cfg(test)]
mod tunnel_tests {
    use super::*;

    /// 真实抓过的 cloudflared 输出（`cloudflared tunnel --url` 本地实测）：URL 混在
    /// box-drawing 字符、时间戳、日志级别里，前后有一堆空格垫着对齐画框。
    #[test]
    fn extract_tunnel_url_from_real_cloudflared_log_line() {
        let line = "2026-07-15T08:22:32Z INF |  https://loved-ran-principles-mailto.trycloudflare.com                                     |";
        assert_eq!(
            extract_tunnel_url(line).as_deref(),
            Some("https://loved-ran-principles-mailto.trycloudflare.com")
        );
    }

    #[test]
    fn extract_tunnel_url_ignores_unrelated_lines() {
        assert_eq!(extract_tunnel_url("2026-07-15T08:22:22Z INF Requesting new quick Tunnel on trycloudflare.com..."), None);
        assert_eq!(extract_tunnel_url("2026-07-15T08:25:01Z ERR Failed to dial a quic connection"), None);
        assert_eq!(extract_tunnel_url(""), None);
    }

    #[test]
    fn extract_tunnel_failure_from_quick_tunnel_eof() {
        let line = r#"failed to request quick Tunnel: Post "https://api.trycloudflare.com/tunnel": EOF"#;
        let msg = extract_tunnel_failure(line).expect("应识别 quick tunnel 申请失败");
        assert!(msg.to_ascii_lowercase().contains("failed to request"));
    }

    /// 真实抓过的"已连接"确认行（`--protocol http2` 实测）：URL 早就打印过了，
    /// 但真正能访问是等到这一行才确认——只看 URL 那次拿到的是暂时 530 的死链接，
    /// 这条测试锁死这个 marker 字符串跟真实日志一致，不能悄悄改错。
    #[test]
    fn connected_marker_matches_real_cloudflared_log_line() {
        let line = "2026-07-15T08:26:46Z INF Registered tunnel connection connIndex=0 connection=0cc8452d-281b-43d2-892a-a60480f845d9 event=0 ip=198.18.20.145 location=lax07 protocol=http2";
        assert!(line.contains(TUNNEL_CONNECTED_MARKER));
    }

    /// GUI 瘦 PATH 下也要能扫到 brew 安装路径（本机装了才会过；CI 无 cloudflared 则 skip）。
    #[test]
    fn resolve_cloudflared_finds_homebrew_path_when_installed() {
        let brew = std::path::Path::new("/opt/homebrew/bin/cloudflared");
        let brew_intel = std::path::Path::new("/usr/local/bin/cloudflared");
        if !brew.is_file() && !brew_intel.is_file() {
            return;
        }
        // 清掉 PATH，模拟 Dock 启动的 .app
        let old = std::env::var_os("PATH");
        // Safety: 单测进程内临时改 PATH；串行跑即可
        unsafe { std::env::set_var("PATH", "/usr/bin:/bin") };
        let found = resolve_cloudflared();
        if let Some(p) = old {
            unsafe { std::env::set_var("PATH", p) };
        } else {
            unsafe { std::env::remove_var("PATH") };
        }
        let p = found.expect("PATH 很瘦时仍应扫到 Homebrew 路径");
        assert!(p.is_file(), "{p:?}");
        assert!(
            p.ends_with("cloudflared"),
            "应是 cloudflared 本体，得到 {p:?}"
        );
    }
}

#[cfg(test)]
mod state_listener_tests {
    use super::*;

    fn no_subscribers() -> Subscribers {
        Arc::new(Mutex::new(Vec::new()))
    }

    /// 标题以 spinner 开头 → 认定 Thinking，且更新 phase_since；标题本身也要存。
    #[test]
    fn title_with_spinner_sets_thinking_phase() {
        let state = Arc::new(Mutex::new(SessionState::default()));
        let listener = StateListener { state: Arc::clone(&state), subscribers: no_subscribers() };
        listener.send_event(Event::Title("⠋ doing work".to_string()));

        let st = state.lock().unwrap();
        assert_eq!(st.phase, Phase::Thinking);
        assert_eq!(st.title.as_deref(), Some("⠋ doing work"));
        assert!(st.phase_since > 0);
    }

    /// 标题不是 spinner 时**不猜**别的 phase——缺乏证据不等于 idle，避免把更
    /// 可信的信源（hook state op）刚写好的值带偏。标题本身还是要照存。
    #[test]
    fn title_without_spinner_does_not_touch_phase() {
        let state = Arc::new(Mutex::new(SessionState {
            phase: Phase::AwaitingApproval,
            ..Default::default()
        }));
        let listener = StateListener { state: Arc::clone(&state), subscribers: no_subscribers() };
        listener.send_event(Event::Title("zsh %".to_string()));

        let st = state.lock().unwrap();
        assert_eq!(st.phase, Phase::AwaitingApproval, "不该被标题猜测覆盖");
        assert_eq!(st.title.as_deref(), Some("zsh %"));
    }

    /// spinner 是最低可信度信源：不得盖掉 hook 已写入的等待/审批/执行态，
    /// 否则远程 action 门闩会误判「agent 不在等你」。
    #[test]
    fn spinner_title_does_not_override_awaiting_approval() {
        let state = Arc::new(Mutex::new(SessionState {
            phase: Phase::AwaitingApproval,
            phase_since: 1,
            ..Default::default()
        }));
        let listener = StateListener { state: Arc::clone(&state), subscribers: no_subscribers() };
        listener.send_event(Event::Title("⠋ waiting for permission".to_string()));

        let st = state.lock().unwrap();
        assert_eq!(st.phase, Phase::AwaitingApproval, "spinner 不得覆盖 AwaitingApproval");
        assert_eq!(st.phase_since, 1, "phase_since 也不该被 spinner 刷新");
        assert_eq!(st.title.as_deref(), Some("⠋ waiting for permission"));
    }

    /// 已在 Thinking 中时，spinner 换帧不得重置 phase_since。agent 思考时 spinner
    /// 每秒换一帧（⠋→⠙→⠹…），帧帧都是一次 Title 事件；若每帧都把起点推到 now，
    /// 「已思考 N 秒」就永远在 0~1 之间跳——上面那条 AwaitingApproval 用例覆盖不到
    /// 这条路径（它压根进不了 Idle|Thinking 分支）。
    #[test]
    fn spinner_frame_does_not_refresh_phase_since_while_thinking() {
        let state = Arc::new(Mutex::new(SessionState {
            phase: Phase::Thinking,
            phase_since: 1,
            ..Default::default()
        }));
        let listener = StateListener { state: Arc::clone(&state), subscribers: no_subscribers() };
        listener.send_event(Event::Title("⠙ still thinking".to_string()));

        let st = state.lock().unwrap();
        assert_eq!(st.phase, Phase::Thinking);
        assert_eq!(st.phase_since, 1, "spinner 换帧不该把思考计时起点推到 now");
        assert_eq!(st.title.as_deref(), Some("⠙ still thinking"));
    }

    #[test]
    fn spinner_title_does_not_override_executing_tool_or_dead() {
        for phase in [Phase::ExecutingTool, Phase::WaitingForUser, Phase::Dead] {
            let state = Arc::new(Mutex::new(SessionState {
                phase,
                ..Default::default()
            }));
            let listener =
                StateListener { state: Arc::clone(&state), subscribers: no_subscribers() };
            listener.send_event(Event::Title("⠋ busy".to_string()));
            assert_eq!(state.lock().unwrap().phase, phase, "{phase:?} 不得被 spinner 覆盖");
        }
    }

    /// Bell 只更新时间戳，不改 phase——单独响铃太不可靠，只能当辅助信号。
    #[test]
    fn bell_touches_timestamp_without_changing_phase() {
        let state = Arc::new(Mutex::new(SessionState { phase: Phase::Idle, ..Default::default() }));
        let listener = StateListener { state: Arc::clone(&state), subscribers: no_subscribers() };
        listener.send_event(Event::Bell);

        let st = state.lock().unwrap();
        assert_eq!(st.phase, Phase::Idle);
        assert!(st.updated_at > 0);
    }

    /// 广播：state 变化后，所有订阅者都该收到一行 `{"session": ...}`。
    #[test]
    fn send_event_broadcasts_to_subscribers() {
        let (a, mut a_client) = UnixStream::pair().unwrap();
        let subscribers: Subscribers = Arc::new(Mutex::new(vec![a]));
        let state = Arc::new(Mutex::new(SessionState { id: "t".into(), ..Default::default() }));
        let listener = StateListener { state, subscribers };
        listener.send_event(Event::Title("⠋ working".to_string()));

        let mut line = String::new();
        BufReader::new(&mut a_client).read_line(&mut line).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["session"]["id"], "t");
        assert_eq!(v["session"]["phase"], "thinking");
    }
}

/// `input` op 的载荷解析：取 `data` 字段的 UTF-8 字节。空串 / 缺字段 → `None`。
/// 不在这里做 phase 门闩——那是 `action` 的事。
fn input_payload(v: &serde_json::Value) -> Option<Vec<u8>> {
    let s = v["data"].as_str()?;
    if s.is_empty() {
        return None;
    }
    Some(s.as_bytes().to_vec())
}

/// `action` op 的 kind → PTY 字节映射。`text` 只有 `reply` 用得上。
/// `Err` 是给客户端看的错误文案——未知 kind / 空 reply 都走这里，不是默认行为。
fn action_payload(kind: Option<&str>, text: Option<&str>) -> Result<Vec<u8>, &'static str> {
    match kind {
        Some("approve") => Ok(b"\r".to_vec()),
        Some("deny") => Ok(b"\x1b".to_vec()),
        Some("reply") => {
            // 空 reply 若退化成单独 `\r` 就和 approve 一样——误点会当成批准。
            let t = text.unwrap_or("");
            if t.is_empty() {
                return Err("需要非空 text");
            }
            let mut bytes = t.as_bytes().to_vec();
            bytes.push(b'\r');
            Ok(bytes)
        }
        _ => Err("未知 kind"),
    }
}

#[cfg(test)]
mod action_tests {
    use super::*;

    #[test]
    fn approve_is_bare_enter() {
        assert_eq!(action_payload(Some("approve"), None), Ok(b"\r".to_vec()));
    }

    #[test]
    fn deny_is_bare_escape_not_arrow_navigation() {
        // 故意不测"按几次下方向键"——这条路本身就不成立（菜单选项数量不是常数，
        // 见模块注释「远程操控」一节）。Esc 不依赖菜单结构。
        assert_eq!(action_payload(Some("deny"), None), Ok(b"\x1b".to_vec()));
    }

    #[test]
    fn reply_appends_enter_after_text() {
        assert_eq!(
            action_payload(Some("reply"), Some("不用了，换个方式")),
            Ok("不用了，换个方式\r".as_bytes().to_vec())
        );
    }

    #[test]
    fn reply_without_text_is_rejected() {
        assert_eq!(action_payload(Some("reply"), None), Err("需要非空 text"));
        assert_eq!(action_payload(Some("reply"), Some("")), Err("需要非空 text"));
    }

    #[test]
    fn unknown_kind_returns_err() {
        assert_eq!(action_payload(Some("do_a_barrel_roll"), None), Err("未知 kind"));
        assert_eq!(action_payload(None, None), Err("未知 kind"));
    }
}

#[cfg(test)]
mod input_payload_tests {
    use super::*;

    #[test]
    fn data_string_becomes_utf8_bytes() {
        let v = serde_json::json!({ "data": "hello" });
        assert_eq!(input_payload(&v), Some(b"hello".to_vec()));
    }

    #[test]
    fn control_chars_in_json_string_work() {
        // Ctrl+C = \u0003；xterm onData + JSON.stringify 就是这条路
        let v = serde_json::json!({ "data": "" });
        assert_eq!(input_payload(&v), Some(vec![0x03]));
    }

    #[test]
    fn empty_or_missing_data_is_none() {
        assert_eq!(input_payload(&serde_json::json!({ "data": "" })), None);
        assert_eq!(input_payload(&serde_json::json!({})), None);
        assert_eq!(input_payload(&serde_json::json!({ "data": null })), None);
    }
}

/// 端到端走真实的 `handle_conn` 分发，而不是只测 action_payload 这个纯函数——
/// 门闩逻辑（phase 不对就拒绝、不实际写入）本身也得有测试盯着，不能只信任
/// action_payload 测过就够了。
#[cfg(test)]
mod action_integration_tests {
    use super::*;

    /// `Ctl.master` 用一根真管道（不是 /dev/null）：这样能从另一头读回真正写
    /// 进去的字节，验证 action 落地的到底是不是预期的按键序列，不是只看 `ok`。
    fn make_pipe_session(rows: u16, cols: u16, phase: Phase) -> (Arc<Session>, std::fs::File) {
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe() 失败");
        let read_end = unsafe { std::fs::File::from_raw_fd(fds[0]) };
        let master = unsafe { std::fs::File::from_raw_fd(fds[1]) };

        let child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id() as i32;
        drop(child); // 留成 zombie，这个测试不需要真的收尸

        let state = Arc::new(Mutex::new(SessionState { phase, ..Default::default() }));
        let subscribers: Subscribers = Arc::new(Mutex::new(Vec::new()));
        let listener = StateListener { state: Arc::clone(&state), subscribers };
        let sess = Arc::new(Session {
            ctl: Mutex::new(Ctl { master, pid, jolt: false, cols, rows, cwd: None }),
            out: Mutex::new(Out { client: None, watchers: Vec::new() }),
            term: Mutex::new(new_daemon_term(rows, cols, listener)),
            state,
        });
        (sess, read_end)
    }

    /// 直接走 handle_conn 的真实分发（不是绕过去调内部函数）：action 是一次性
    /// 请求-响应，不像 watch/open 那样要开线程陪它跑一辈子。
    fn call_action(sessions: &Sessions, id: &str, kind: &str) -> serde_json::Value {
        let (server, client) = UnixStream::pair().unwrap();
        let remote_state: RemoteState = Arc::new(Mutex::new(None));
        let tunnel_state: TunnelState = Arc::new(Mutex::new(None));
        let subscribers: Subscribers = Arc::new(Mutex::new(Vec::new()));
        let mut client = client;
        writeln!(client, "{}", serde_json::json!({ "op": "action", "id": id, "kind": kind }))
            .unwrap();
        handle_conn(server, Arc::clone(sessions), Arc::new(Mutex::new(HashMap::new())), 0, -1, remote_state, tunnel_state, subscribers);
        let mut resp = String::new();
        BufReader::new(client).read_line(&mut resp).unwrap();
        serde_json::from_str(&resp).unwrap()
    }

    #[test]
    fn approve_writes_bare_enter_when_awaiting_approval() {
        let (sess, mut read_end) = make_pipe_session(24, 80, Phase::AwaitingApproval);
        let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
        sessions.lock().unwrap().insert("a".to_string(), sess);

        let resp = call_action(&sessions, "a", "approve");
        assert_eq!(resp["ok"], true, "resp={resp}");

        let mut buf = [0u8; 8];
        let n = read_end.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"\r");
    }

    #[test]
    fn deny_writes_bare_escape_when_waiting_for_user() {
        let (sess, mut read_end) = make_pipe_session(24, 80, Phase::WaitingForUser);
        let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
        sessions.lock().unwrap().insert("b".to_string(), sess);

        let resp = call_action(&sessions, "b", "deny");
        assert_eq!(resp["ok"], true, "resp={resp}");

        let mut buf = [0u8; 8];
        let n = read_end.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"\x1b");
    }

    /// 门闩：phase 是 Thinking（agent 正忙）时，action 必须被拒绝，且**真的没有
    /// 写入任何字节**——不能只是回错误但底下偷偷写了。
    #[test]
    fn action_rejected_and_no_bytes_written_when_agent_busy() {
        let (sess, mut read_end) = make_pipe_session(24, 80, Phase::Thinking);
        let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
        sessions.lock().unwrap().insert("c".to_string(), sess);

        let resp = call_action(&sessions, "c", "approve");
        assert_eq!(resp["ok"], false);
        assert!(resp["err"].as_str().unwrap().contains("不是在等你"), "resp={resp}");

        // 管道写端没收到任何字节：把它设成非阻塞读一下，读不到东西才对。
        use std::os::fd::AsRawFd;
        unsafe {
            let flags = libc::fcntl(read_end.as_raw_fd(), libc::F_GETFL);
            libc::fcntl(read_end.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        let mut buf = [0u8; 8];
        let result = read_end.read(&mut buf);
        assert!(result.is_err(), "门闩失效：agent 忙的时候还是写进去了字节");
    }

    #[test]
    fn action_on_unknown_session_is_rejected() {
        let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
        let resp = call_action(&sessions, "does-not-exist", "approve");
        assert_eq!(resp["ok"], false);
    }
}

/// `input` 端到端：无 phase 门闩——agent 忙也能写（跟 action 最关键的差异）；
/// 这是「远程 = 工作延续」的契约，回归测试必须盯死。
#[cfg(test)]
mod input_integration_tests {
    use super::*;

    fn make_pipe_session(rows: u16, cols: u16, phase: Phase) -> (Arc<Session>, std::fs::File) {
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe() 失败");
        let read_end = unsafe { std::fs::File::from_raw_fd(fds[0]) };
        let master = unsafe { std::fs::File::from_raw_fd(fds[1]) };
        let child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id() as i32;
        drop(child);
        let state = Arc::new(Mutex::new(SessionState { phase, ..Default::default() }));
        let subscribers: Subscribers = Arc::new(Mutex::new(Vec::new()));
        let listener = StateListener { state: Arc::clone(&state), subscribers };
        let sess = Arc::new(Session {
            ctl: Mutex::new(Ctl { master, pid, jolt: false, cols, rows, cwd: None }),
            out: Mutex::new(Out { client: None, watchers: Vec::new() }),
            term: Mutex::new(new_daemon_term(rows, cols, listener)),
            state,
        });
        (sess, read_end)
    }

    fn call_input(sessions: &Sessions, id: &str, data: &str) -> serde_json::Value {
        let (server, client) = UnixStream::pair().unwrap();
        let remote_state: RemoteState = Arc::new(Mutex::new(None));
        let tunnel_state: TunnelState = Arc::new(Mutex::new(None));
        let subscribers: Subscribers = Arc::new(Mutex::new(Vec::new()));
        let mut client = client;
        writeln!(
            client,
            "{}",
            serde_json::json!({ "op": "input", "id": id, "data": data })
        )
        .unwrap();
        handle_conn(server, Arc::clone(sessions), Arc::new(Mutex::new(HashMap::new())), 0, -1, remote_state, tunnel_state, subscribers);
        let mut resp = String::new();
        BufReader::new(client).read_line(&mut resp).unwrap();
        serde_json::from_str(&resp).unwrap()
    }

    #[test]
    fn input_writes_even_when_agent_is_thinking() {
        let (sess, mut read_end) = make_pipe_session(24, 80, Phase::Thinking);
        let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
        sessions.lock().unwrap().insert("busy".to_string(), sess);

        // Ctrl+C（0x03）：json! 直接嵌 char，serde 编进 JSON 字符串
        let ctrl_c = "\u{0003}";
        let resp = call_input(&sessions, "busy", ctrl_c);
        assert_eq!(resp["ok"], true, "resp={resp}");

        let mut buf = [0u8; 8];
        let n = read_end.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"\x03");
    }

    #[test]
    fn input_writes_text_while_idle() {
        let (sess, mut read_end) = make_pipe_session(24, 80, Phase::Idle);
        let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
        sessions.lock().unwrap().insert("idle".to_string(), sess);

        let resp = call_input(&sessions, "idle", "ls -la\r");
        assert_eq!(resp["ok"], true, "resp={resp}");

        let mut buf = [0u8; 64];
        let n = read_end.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"ls -la\r");
    }

    #[test]
    fn empty_input_is_rejected() {
        let (sess, mut read_end) = make_pipe_session(24, 80, Phase::Idle);
        let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
        sessions.lock().unwrap().insert("e".to_string(), sess);

        let resp = call_input(&sessions, "e", "");
        assert_eq!(resp["ok"], false);
        assert!(resp["err"].as_str().unwrap().contains("data"), "resp={resp}");

        use std::os::fd::AsRawFd;
        unsafe {
            let flags = libc::fcntl(read_end.as_raw_fd(), libc::F_GETFL);
            libc::fcntl(read_end.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        let mut buf = [0u8; 8];
        assert!(read_end.read(&mut buf).is_err(), "空 input 不该写字节");
    }

    #[test]
    fn input_on_unknown_session_is_rejected() {
        let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
        let resp = call_input(&sessions, "nope", "x");
        assert_eq!(resp["ok"], false);
    }
}

fn handle_conn(
    conn: UnixStream,
    sessions: Sessions,
    acp_sessions: AcpSessions,
    exe_mtime: u64,
    listen_fd: RawFd,
    remote_state: RemoteState,
    tunnel_state: TunnelState,
    subscribers: Subscribers,
) {
    // 头一行 JSON。之后的帧字节可能已被 BufReader 预读，故帧循环必须复用同一个 reader。
    let Ok(rc) = conn.try_clone() else { return };
    let mut reader = BufReader::new(rc);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { return };

    match v["op"].as_str() {
        Some("open") => handle_open(conn, reader, &v, sessions, Arc::clone(&subscribers)),
        Some("watch") => handle_watch(conn, reader, &v, sessions),
        Some("subscribe") => handle_subscribe(conn, &sessions, &acp_sessions, &subscribers),
        Some("acp_open") => handle_acp_open(conn, reader, &v, acp_sessions, subscribers),
        Some("acp_watch") => handle_acp_watch(conn, reader, &v, acp_sessions),
        Some("acp_kill") => handle_acp_kill(conn, &v, &acp_sessions),
        // 扫当前可视区里的权限菜单，解析结果原样回给调用方（网关 → 手机端）。
        // 只读、无副作用，所以不要写权限：看得见菜单 ≠ 能点它，点是走 input/action。
        //
        // 为什么是「拉」而不是随 state 广播：state 广播由 hook 驱动（phase 变化），
        // 没接 hook 的 agent 永远不广播，菜单就永远到不了手机。而画面变化只有客户端
        // 最清楚——它在渲染 xterm，debounce 之后拉一次即可，服务端不必在 PTY 泵那条
        // 每字节都过的热路径上挂解析。
        Some("menu") => {
            let id = v["id"].as_str().unwrap_or_default();
            let sess = sessions.lock().unwrap().get(id).cloned();
            let menu = sess.and_then(|s| {
                let term = s.term.lock().ok()?;
                permission_menu::parse_permission_prompt(&term_text::last_lines(&term, 28))
            });
            let mut c = conn;
            let _ = writeln!(c, "{}", serde_json::json!({ "ok": true, "menu": menu }));
        }
        Some("list") => {
            let (mut ids, mut states): (Vec<String>, Vec<SessionState>) = sessions
                .lock()
                .unwrap()
                .iter()
                .map(|(id, s)| (id.clone(), s.state.lock().unwrap().clone()))
                .unzip();
            let (acp_ids, acp_states): (Vec<String>, Vec<SessionState>) = acp_sessions
                .lock()
                .unwrap()
                .iter()
                .map(|(id, s)| (id.clone(), s.state.lock().unwrap().clone()))
                .unzip();
            ids.extend(acp_ids);
            states.extend(acp_states);
            let mut c = conn;
            let _ = writeln!(c, "{}", serde_json::json!({ "sessions": ids, "states": states }));
        }
        Some("kill") => {
            let id = v["id"].as_str().unwrap_or_default();
            let s = sessions.lock().unwrap().remove(id);
            if let Some(s) = s {
                unsafe {
                    libc::kill(s.ctl.lock().unwrap().pid, libc::SIGKILL);
                }
                let mut out = s.out.lock().unwrap();
                if let Some(c) = out.client.take() {
                    let _ = c.shutdown(Shutdown::Both);
                }
                for w in out.watchers.drain(..) {
                    let _ = w.shutdown(Shutdown::Both);
                }
            }
            let mut c = conn;
            let _ = writeln!(c, "{}", serde_json::json!({ "ok": true }));
        }
        Some("upgrade") => handle_upgrade(conn, &v, &sessions, &acp_sessions, listen_fd),
        Some("version") => {
            // pid/started_at/session_count/exe 是后加的：旧 GUI 只读 version/exe_mtime，
            // 多出来的字段它直接忽略，协议向后兼容。
            // `exe`：GUI 用来判断守护是否仍住在 .app 内（装 DMG 会被 SIGKILL）。
            let session_count = sessions.lock().map(|s| s.len()).unwrap_or(0);
            let exe_path = std::env::current_exe()
                .ok()
                .map(|p| p.to_string_lossy().into_owned());
            let mut c = conn;
            let _ = writeln!(
                c,
                "{}",
                serde_json::json!({
                    "version": env!("CARGO_PKG_VERSION"),
                    "exe_mtime": exe_mtime,
                    "exe": exe_path,
                    "pid": std::process::id(),
                    "started_at": started_at(),
                    "session_count": session_count,
                })
            );
        }
        Some("shutdown") => {
            let mut c = conn;
            let _ = writeln!(c, "{}", serde_json::json!({ "ok": true }));
            let _ = c.shutdown(Shutdown::Both);
            // 先收尸 cloudflared / 远程网关，再 exit——否则隧道子进程会孤儿化并继续
            // 转发到已死端口。PTY 随本进程死、shell 收 SIGHUP，这是「重启守护」的代价。
            cleanup_sidecar_services();
            std::process::exit(0);
        }
        Some("remote_start") => {
            let bind = v["bind"].as_str().unwrap_or("127.0.0.1").to_string();
            let port = v["port"].as_u64().unwrap_or(0) as u16;
            let write = v["write"].as_bool().unwrap_or(false);
            let mut c = conn;
            match start_remote_gateway(&remote_state, &bind, port, write) {
                Ok((token, addr, write)) => {
                    let _ = writeln!(
                        c,
                        "{}",
                        serde_json::json!({
                            "ok": true, "token": token, "addr": addr.to_string(), "write": write
                        })
                    );
                }
                Err(e) => {
                    let _ = writeln!(c, "{}", serde_json::json!({ "ok": false, "err": e }));
                }
            }
        }
        Some("remote_stop") => {
            stop_remote_gateway(&remote_state);
            let mut c = conn;
            let _ = writeln!(c, "{}", serde_json::json!({ "ok": true }));
        }
        Some("remote_status") => {
            let mut c = conn;
            let body = match remote_state.lock().unwrap().as_ref() {
                Some(g) => serde_json::json!({
                    "running": true, "token": g.token, "addr": g.addr.to_string(), "write": g.write
                }),
                None => serde_json::json!({ "running": false }),
            };
            let _ = writeln!(c, "{}", body);
        }
        Some("tunnel_start") => {
            let write = v["write"].as_bool().unwrap_or(false);
            let mut c = conn;
            match start_tunnel(&tunnel_state, &remote_state, write) {
                Ok((url, write)) => {
                    let _ = writeln!(c, "{}", serde_json::json!({ "ok": true, "url": url, "write": write }));
                }
                Err(e) => {
                    let _ = writeln!(c, "{}", serde_json::json!({ "ok": false, "err": e }));
                }
            }
        }
        Some("tunnel_stop") => {
            stop_tunnel(&tunnel_state);
            let mut c = conn;
            let _ = writeln!(c, "{}", serde_json::json!({ "ok": true }));
        }
        Some("tunnel_status") => {
            let mut c = conn;
            let body = match tunnel_status(&tunnel_state) {
                Some(url) => {
                    let write = remote_state.lock().unwrap().as_ref().map(|g| g.write).unwrap_or(false);
                    serde_json::json!({ "running": true, "url": url, "write": write })
                }
                None => serde_json::json!({ "running": false }),
            };
            let _ = writeln!(c, "{}", body);
        }
        Some("state") => {
            // hook 直写，协议事实——三个信源里可信度最高，无条件覆盖 StateListener
            // 猜出来的值（见 SessionState 定义处注释）。会话不存在（hook 跑得比
            // spawn 还快，或者会话已经被 kill）就静默丢弃，不算错误。
            let id = v["id"].as_str().unwrap_or_default();
            // 先把会话取出来、当场放掉 sessions 锁，再往下走：下面的 broadcast_state
            // 要拿 subscribers，而 handle_subscribe 是「持 subscribers 求 sessions」——
            // 反向持有就是 ABBA 死锁。sessions 一旦锁死，open/list/kill/version/upgrade
            // 全部卡住，PTY 还活着但守护已废，用户只能 pkill，正在跑的会话全灭。
            //
            // 绝不能写回 `if let Some(sess) = sessions.lock().unwrap()...`：if-let 的
            // scrutinee 临时量（那把 guard）活到整个 body 结束，Rust 2024 的 if-let
            // rescope 只改 else 分支、救不了这里。旁边 action/input/resize 用 let-else
            // 正是为此（guard 在语句末即释放）。
            let sess = sessions.lock().unwrap().get(id).cloned();
            if let Some(sess) = sess {
                if let Ok(phase) = serde_json::from_value::<Phase>(v["phase"].clone()) {
                    let snapshot = {
                        let mut st = sess.state.lock().unwrap();
                        st.phase = phase;
                        st.phase_since = now_unix();
                        st.pending_question = v["question"].as_str().map(String::from);
                        st.updated_at = now_unix();
                        st.clone()
                    };
                    broadcast_state(&subscribers, &snapshot);
                }
            }
            let mut c = conn;
            let _ = writeln!(c, "{}", serde_json::json!({ "ok": true }));
        }
        Some("action") => {
            let id = v["id"].as_str().unwrap_or_default();
            let mut c = conn;
            let Some(sess) = sessions.lock().unwrap().get(id).cloned() else {
                let _ = writeln!(c, "{}", serde_json::json!({ "ok": false, "err": "会话不存在" }));
                return;
            };

            // 门闩：只有 agent 真的在等你的时候才允许写入——不然这几个字节会被当成
            // agent 当前正在做的别的事情的输入，把会话搞乱（见 collaboration.md
            // 「联机 review」一节的坑）。不排队，直接拒绝：Phase 5 的操作台本来就是
            // 状态驱动渲染按钮，正常点击时机不该落到这个分支。
            let phase = sess.state.lock().unwrap().phase;
            if !matches!(phase, Phase::AwaitingApproval | Phase::WaitingForUser) {
                let _ = writeln!(
                    c,
                    "{}",
                    serde_json::json!({ "ok": false, "err": "agent 现在不是在等你，稍后再试" })
                );
                return;
            }

            let payload = match action_payload(v["kind"].as_str(), v["text"].as_str()) {
                Ok(p) => p,
                Err(err) => {
                    let _ = writeln!(c, "{}", serde_json::json!({ "ok": false, "err": err }));
                    return;
                }
            };

            let write_result = {
                let ctl = sess.ctl.lock().unwrap();
                (&ctl.master).write_all(&payload)
            };
            match write_result {
                Ok(()) => {
                    let _ = writeln!(c, "{}", serde_json::json!({ "ok": true }));
                }
                Err(e) => {
                    let _ = writeln!(c, "{}", serde_json::json!({ "ok": false, "err": e.to_string() }));
                }
            }
        }
        Some("input") => {
            // 原始输入：工作延续，无 phase 门闩。权限在网关 write_enabled，这里只做
            // 「会话在不在 + 载荷非空 + 写进 master」。
            let id = v["id"].as_str().unwrap_or_default();
            let mut c = conn;
            let Some(sess) = sessions.lock().unwrap().get(id).cloned() else {
                let _ = writeln!(c, "{}", serde_json::json!({ "ok": false, "err": "会话不存在" }));
                return;
            };

            let Some(payload) = input_payload(&v) else {
                let _ = writeln!(
                    c,
                    "{}",
                    serde_json::json!({ "ok": false, "err": "需要非空 data" })
                );
                return;
            };

            let write_result = {
                let ctl = sess.ctl.lock().unwrap();
                (&ctl.master).write_all(&payload)
            };
            match write_result {
                Ok(()) => {
                    let _ = writeln!(c, "{}", serde_json::json!({ "ok": true }));
                }
                Err(e) => {
                    let _ = writeln!(c, "{}", serde_json::json!({ "ok": false, "err": e.to_string() }));
                }
            }
        }
        Some("resize") => {
            // 手机端按视口改 PTY 尺寸，让 Claude 等 TUI SIGWINCH 重排，
            // 避免「镜像桌面大窗口 → 底部空一大截」。
            let id = v["id"].as_str().unwrap_or_default();
            let cols = v["cols"].as_u64().unwrap_or(0) as u16;
            let rows = v["rows"].as_u64().unwrap_or(0) as u16;
            let cell_w = v["cell_w"].as_u64().unwrap_or(0) as u16;
            let cell_h = v["cell_h"].as_u64().unwrap_or(0) as u16;
            let mut c = conn;
            if cols == 0 || rows == 0 {
                let _ = writeln!(
                    c,
                    "{}",
                    serde_json::json!({ "ok": false, "err": "cols/rows 必须 > 0" })
                );
                return;
            }
            let Some(sess) = sessions.lock().unwrap().get(id).cloned() else {
                let _ = writeln!(c, "{}", serde_json::json!({ "ok": false, "err": "会话不存在" }));
                return;
            };
            // jolt：确保即使尺寸碰巧与当前相同也发出 SIGWINCH，逼 TUI 全量重绘
            sess.ctl.lock().unwrap().jolt = true;
            resize_session(&sess, cols, rows, cell_w, cell_h);
            let _ = writeln!(
                c,
                "{}",
                serde_json::json!({ "ok": true, "cols": cols, "rows": rows })
            );
        }
        _ => {}
    }
}

fn handle_open(
    conn: UnixStream,
    mut reader: BufReader<UnixStream>,
    v: &serde_json::Value,
    sessions: Sessions,
    subscribers: Subscribers,
) {
    let id = v["id"].as_str().unwrap_or_default().to_string();
    if id.is_empty() {
        return;
    }
    let cols = v["cols"].as_u64().unwrap_or(80) as u16;
    let rows = v["rows"].as_u64().unwrap_or(24) as u16;
    let cwd = v["cwd"].as_str().map(String::from);
    // 只在新建会话时生效（reattach 到已存在的会话没有"起始命令"这回事）。
    let launch = v["launch"].as_str().map(String::from);

    // 取既有会话（reattach）或新建。
    let existing = sessions.lock().unwrap().get(&id).cloned();
    let reattach = existing.is_some();
    let sess = match existing {
        Some(s) => {
            // reattach：等客户端首帧 resize（含真实 cell 像素）再 jolt，避免在错误
            // 尺寸下 SIGWINCH → Claude「显示不全」。见下方 delayed jolt 注释。
            s.ctl.lock().unwrap().jolt = true;
            s
        }
        None => {
            let Ok((sess, pty_reader)) =
                spawn_session(&id, rows, cols, cwd.as_deref(), launch.as_deref(), &subscribers)
            else {
                return;
            };
            let sess = Arc::new(sess);
            sessions.lock().unwrap().insert(id.clone(), Arc::clone(&sess));
            start_pty_pump(Arc::clone(&sess), pty_reader, id.clone(), Arc::clone(&sessions));
            sess
        }
    };

    // attach：回报 PTY 当前尺寸 → 网格 ANSI 快照 → 接管转发。
    //
    // 锁序必须与泵一致（term → out），且 snapshot 与装上 client 之间不能放掉 out：
    // 若先 snapshot 再另抢 out，间隙里泵可能 advance(D) 后发现还没 client 而丢弃 D，
    // 新客户端拿到的网格就永久缺字节（正是「吐快照」要避免的 reattach 错位）。
    // 正确做法：持 term 时抢到 out → 再出快照 → 放 term → 写 socket 期间只持 out
    // （泵 advance 后堵在 out，client 装上后再把缺口字节转发给新客户端）。
    let (cur_cols, cur_rows) = {
        let ctl = sess.ctl.lock().unwrap();
        (ctl.cols, ctl.rows)
    };
    let launch_for_snap = sess.state.lock().unwrap().launch.clone();
    let attached_fd = {
        let Ok(mut c) = conn.try_clone() else { return };
        let fd = c.as_raw_fd();
        // 写超时：客户端冻结时不能无限期占着 out 锁（见 CLIENT_WRITE_TIMEOUT）。
        let _ = c.set_write_timeout(Some(CLIENT_WRITE_TIMEOUT));

        let (snapshot, mut out) = {
            let term = sess.term.lock().unwrap();
            let out = sess.out.lock().unwrap();
            // launch 参与判定：Grok 等未必进 1049 备用屏，但仍是 TUI，灌网格会顶行乱码。
            let snapshot = snapshot_ansi(&term, launch_for_snap.as_deref());
            drop(term);
            (snapshot, out)
        };

        if let Some(old) = out.client.take() {
            let _ = old.shutdown(Shutdown::Both); // 顶掉旧连接（同 id 只允许一个 GUI）
        }
        // replay_len = 快照字节数：客户端仍用它划「历史/实时」边界，跳过快照里的
        // 历史 OSC 9（网格快照本身不含旧通知序列，但边界语义保留兼容）。
        let replay_len = snapshot.len();
        if writeln!(
            c,
            "{}",
            serde_json::json!({ "cols": cur_cols, "rows": cur_rows, "replay_len": replay_len })
        )
        .is_err()
        {
            return;
        }
        if replay_len > 0 && c.write_all(&snapshot).is_err() {
            return;
        }
        out.client = Some(c);
        fd
    };

    // reattach jolt 策略（修 Claude 显示不全 / Grok 半残）：
    // **不要**在 attach 当下立刻 SIGWINCH——此时 GUI 往往还是守护旧 cols/rows、cell=0，
    // TUI 按错误尺寸整屏重画 → 显示不全。正确顺序：等客户端 force_resize（真 cell
    // 像素）走 type-1 帧，resize_session 里 jolt 才触发。
    // 兜底：350ms 内若 jolt 仍 true（客户端没发 resize），再强制抖一次。
    if reattach {
        sess.ctl.lock().unwrap().jolt = true;
        let sess2 = Arc::clone(&sess);
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(350));
            let (c, r, still) = {
                let Ok(ctl) = sess2.ctl.lock() else { return };
                (ctl.cols, ctl.rows, ctl.jolt)
            };
            if still {
                if let Ok(mut ctl) = sess2.ctl.lock() {
                    ctl.jolt = true;
                }
                resize_session(&sess2, c, r, 0, 0);
            }
            // 再补一枪：部分 TUI（Claude）第一次 SIGWINCH 只重排半屏
            thread::sleep(Duration::from_millis(200));
            if let Ok(mut ctl) = sess2.ctl.lock() {
                ctl.jolt = true;
                let c = ctl.cols;
                let r = ctl.rows;
                drop(ctl);
                resize_session(&sess2, c, r, 0, 0);
            }
        });
    }

    // 帧循环：输入 / resize，直到客户端断开。
    loop {
        let mut hdr = [0u8; 5];
        if reader.read_exact(&mut hdr).is_err() {
            break;
        }
        let len = u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize;
        if len > (1 << 20) {
            break; // 异常长度，掐断
        }
        let mut payload = vec![0u8; len];
        if reader.read_exact(&mut payload).is_err() {
            break;
        }
        match hdr[0] {
            0 => {
                let ctl = sess.ctl.lock().unwrap();
                let _ = (&ctl.master).write_all(&payload);
            }
            1 if len == 8 || len == 16 => {
                let cols = u32::from_be_bytes(payload[0..4].try_into().unwrap()) as u16;
                let rows = u32::from_be_bytes(payload[4..8].try_into().unwrap()) as u16;
                // 可选：单元格像素（新客户端 16 字节帧）；整窗像素 = 行列 × 格像素。
                let (cell_w, cell_h) = if len == 16 {
                    let cw = u32::from_be_bytes(payload[8..12].try_into().unwrap()) as u16;
                    let ch = u32::from_be_bytes(payload[12..16].try_into().unwrap()) as u16;
                    (cw, ch)
                } else {
                    (0, 0)
                };
                resize_session(&sess, cols, rows, cell_w, cell_h);
            }
            _ => break,
        }
    }

    // 断开：仅当 client 还是本连接时才清（可能已被新 GUI 顶掉）。
    let mut out = sess.out.lock().unwrap();
    if out.client.as_ref().map(|c| c.as_raw_fd()) == Some(attached_fd) {
        out.client = None;
    }
}

/// 只读旁观：观战席/远程查看这类场景用。跟 `handle_open` 的核心区别——
/// 1. 不兜底 spawn：会话必须已存在，旁观一个不存在的会话没有意义；
/// 2. 不顶替 `out.client`，也不顶替其它 watcher——`push` 进去，多个旁观者可并存；
/// 3. 没有帧循环：旁观连接只读，收到客户端发来的任何字节都当异常直接断开清理。
fn handle_watch(
    conn: UnixStream,
    mut reader: BufReader<UnixStream>,
    v: &serde_json::Value,
    sessions: Sessions,
) {
    let id = v["id"].as_str().unwrap_or_default().to_string();
    if id.is_empty() {
        return;
    }
    let Some(sess) = sessions.lock().unwrap().get(&id).cloned() else {
        return;
    };

    let (cur_cols, cur_rows) = {
        let ctl = sess.ctl.lock().unwrap();
        (ctl.cols, ctl.rows)
    };

    // 锁序、snapshot-与-挂载之间不放锁的道理跟 handle_open 完全一致（见其注释）：
    // 用 out 锁本身当「挂载点」，snapshot 拼好、watcher push 进 Vec 一步做完，
    // 中间不放 out 锁，泵线程就不会在这个间隙 advance 出一段没人接住的字节。
    let attached_fd = {
        let Ok(mut c) = conn.try_clone() else { return };
        let fd = c.as_raw_fd();
        let _ = c.set_write_timeout(Some(CLIENT_WRITE_TIMEOUT));

        let term = sess.term.lock().unwrap();
        let mut out = sess.out.lock().unwrap();
        let launch = sess.state.lock().unwrap().launch.clone();
        let snapshot = snapshot_ansi(&term, launch.as_deref());
        drop(term);

        let replay_len = snapshot.len();
        if writeln!(
            c,
            "{}",
            serde_json::json!({ "cols": cur_cols, "rows": cur_rows, "replay_len": replay_len })
        )
        .is_err()
        {
            return;
        }
        if replay_len > 0 && c.write_all(&snapshot).is_err() {
            return;
        }
        out.watchers.push(c);
        fd
    };

    // 只读：不认帧协议，读到任何东西（含 EOF/出错）都收尾——旁观者本就不该往这条连接写字节。
    let mut scratch = [0u8; 64];
    let _ = reader.read(&mut scratch);

    let mut out = sess.out.lock().unwrap();
    out.watchers.retain(|w| w.as_raw_fd() != attached_fd);
}

/// 状态订阅：跟 `watch` 是同一种只读连接模式，但订阅面是**全部会话**，不是单个
/// session（见 Subscribers 类型定义处注释）。首帧全量快照，之后每次任何会话的
/// state 变化都会推一行——广播逻辑在 broadcast_state / StateListener::send_event /
/// `state` op 里，这里只管连接的注册与清理。快照汇总终端 + ACP 两张表——四色
/// 状态两边共用同一个 `SessionState`/`Phase`（见下方「ACP 会话托管」一节），GUI
/// 那条既有的 subscribe 监听代码完全不用为 ACP 改一行。
fn handle_subscribe(
    conn: UnixStream,
    sessions: &Sessions,
    acp_sessions: &AcpSessions,
    subscribers: &Subscribers,
) {
    let Ok(mut c) = conn.try_clone() else { return };
    let fd = c.as_raw_fd();
    // 与 open/watch 一致：冻结订阅者不能无限期占着 broadcast_state 的 subscribers 锁。
    let _ = c.set_write_timeout(Some(CLIENT_WRITE_TIMEOUT));

    // snapshot 写出去、push 进订阅列表之间不能放 subscribers 锁：否则中间这个空隙里
    // 一次 broadcast_state 可能两头都漏掉——快照里没有它（早于快照），也没收到广播
    // （晚于注册），这次状态变化对这个订阅者来说凭空消失。跟 handle_watch 的
    // snapshot-与-挂载不放锁是同一个道理。
    let mut subs = subscribers.lock().unwrap();
    let mut snapshot: Vec<SessionState> = {
        let sessions = sessions.lock().unwrap();
        sessions.values().map(|s| s.state.lock().unwrap().clone()).collect()
    };
    snapshot.extend(
        acp_sessions.lock().unwrap().values().map(|s| s.state.lock().unwrap().clone()),
    );
    if writeln!(c, "{}", serde_json::json!({ "sessions": snapshot })).is_err() {
        return;
    }
    subs.push(c);
    drop(subs);

    // 只读：不认帧协议，读到任何东西（含 EOF/出错）都收尾，跟 handle_watch 一致。
    let mut reader = BufReader::new(conn);
    let mut scratch = [0u8; 64];
    let _ = reader.read(&mut scratch);

    subscribers.lock().unwrap().retain(|s| s.as_raw_fd() != fd);
}

// ===================== ACP 会话托管 =====================
//
// 跟终端会话是两条平行的托管逻辑：没有 PTY/网格，「画面」就是
// `smelt_core::acp_session::AcpSnapshot`（entries + phase + 待办卡片），由
// `smelt_core::acp_session::apply_event` 把子进程 agent 发来的协议事件归约
// 出来——归约逻辑本身跟 GPUI 无关，谁接手连接谁跑，见该模块文件头注释（核心
// 原因：`AcpEvent::Permission`/`Elicitation` 带的 responder 绑在连接线程上，
// 没法跨进程传，只能是 smeltd 亲自跑完整个事件循环）。
//
// 四色状态复用终端会话已有的 `SessionState`/`Phase`/`broadcast_state`/
// `subscribe` 机制：`AcpSession.state` 就是一份跟终端会话同类型的
// `Arc<Mutex<SessionState>>`，list/subscribe 汇总时两边的 Vec 拼在一起即可。
// ACP 会话 id 沿用 GUI 现有的 `acp-` 前缀约定，GUI 靠这个前缀判断该走
// open/watch 还是 acp_open/acp_watch。
//
// 协议：
//   {"op":"acp_open","id":"acp-..","cwd":"..","cmd":"..","agent":"claude",
//    "resume_id":".."}
//     → 已存在且还活着（有 handle）就直接接上；已存在但 Ended（没有 handle）
//       就用请求带的 cmd + 已知的旧 session id（没有才退回请求带的 resume_id）
//       重新 spawn（「重新开始」）；都不存在就全新建。回一份
//       `{"snapshot": AcpSnapshot}`，之后每次归约有实质变化再推一份同形状的
//       行。同 id 只允许一个控制连接，第二次 open 顶掉前一个。
//   {"op":"acp_watch","id":".."} → 只读镜像，会话必须已存在，可多个并存。
//   {"op":"acp_kill","id":".."} → 回 {"ok":true}，杀子进程、从表里摘掉、
//     踢掉所有 client/watcher。
//
// acp_open 连接内不是终端那套二进制帧，是纯 JSON 行、双向：
//   客户端 → 守护：一行 `AcpUserAction` 的 JSON
//   守护 → 客户端：一行 `{"snapshot": AcpSnapshot}`
// 断开 acp_open 连接（切标签/关标签/App 退出）只摘连接，不杀会话——这正是
// 这层要解决的问题（GUI 退出不该带走 ACP 对话）。真要杀走 acp_kill。
//
// 「无缝升级」交接：agent 子进程的 stdin/stdout fd 跟 PTY master fd 同一招
// 裸传过 exec()，快照数据（entries/phase/model 等）随交接文件走。真正的
// 难点不在 fd 本身（管道 fd 天然能存活 exec()），而在 JSON-RPC 是有状态协议
// ——升级那一刻如果正卡着一张权限/选择题卡片，那个请求的 Rust responder
// 对象没法序列化过 exec()。解法：`smelt_core::acp_conn` 的 `with_debug`
// 捕获每条请求的原始 JSON-RPC 行文本一起交接，新进程接上继承来的 fd 后先
// 把这行「回放」一遍，让 SDK 重新解析出绑定同一个原始请求 id 的等价
// responder，见 `resume_acp_from_fds`。没连上/已经 Ended 的会话没有 fd 可
// 传，交接不了的会在升级前主动关掉，不静默留孤儿子进程。

struct AcpOut {
    client: Option<UnixStream>,
    watchers: Vec<UnixStream>,
}

struct AcpSession {
    reduced: Mutex<smelt_core::acp_session::AcpSessionState>,
    handle: Mutex<Option<smelt_core::acp_conn::AcpHandle>>,
    cwd: Option<String>,
    /// 只有 Claude 才该为 true，见 `AcpLaunch::resume_needs_transcript_check`。
    agent_needs_transcript_check: bool,
    /// 四色状态，跟终端会话共用同一个类型/同一套广播机制。
    state: Arc<Mutex<SessionState>>,
    out: Mutex<AcpOut>,
}

type AcpSessions = Arc<Mutex<HashMap<String, Arc<AcpSession>>>>;

/// ACP 相位 → 四色 Phase。`Running` 还要看 entries 里有没有进行中的工具调用，
/// 细分成「执行工具」/「思考中」——跟旧版 GUI `sync_daemon_state` 的判断一致。
fn compute_acp_daemon_phase(reduced: &smelt_core::acp_session::AcpSessionState) -> Phase {
    use smelt_core::acp_chat::{AcpEntry, ToolCallStatus};
    use smelt_core::acp_session::AcpPhase;
    match &reduced.phase {
        AcpPhase::Starting | AcpPhase::Idle => Phase::Idle,
        AcpPhase::Running => {
            let executing = reduced.entries.iter().any(|e| {
                matches!(
                    e,
                    AcpEntry::ToolCall {
                        status: ToolCallStatus::InProgress | ToolCallStatus::Pending,
                        ..
                    }
                )
            });
            if executing { Phase::ExecutingTool } else { Phase::Thinking }
        }
        AcpPhase::AwaitingApproval => Phase::AwaitingApproval,
        AcpPhase::AwaitingChoice => Phase::WaitingForUser,
        AcpPhase::Ended(_) => Phase::Dead,
    }
}

fn acp_pending_question(reduced: &smelt_core::acp_session::AcpSessionState) -> Option<String> {
    reduced
        .permission
        .as_ref()
        .map(|p| p.question.clone())
        .or_else(|| reduced.elicitation.as_ref().map(|e| e.message.clone()))
}

/// 把归约状态里的相位/待办问句同步进四色 `SessionState` 并广播。跟旧版 GUI
/// `AcpView::sync_daemon_state` 是同一件事，只是现在算在 smeltd 侧。
fn update_acp_daemon_state(sess: &AcpSession, subscribers: &Subscribers) {
    let (phase, pending_question) = {
        let reduced = sess.reduced.lock().unwrap();
        (compute_acp_daemon_phase(&reduced), acp_pending_question(&reduced))
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let snapshot = {
        let mut st = sess.state.lock().unwrap();
        if st.phase != phase {
            st.phase_since = now;
        }
        st.phase = phase;
        st.pending_question = pending_question;
        st.updated_at = now;
        st.clone()
    };
    broadcast_state(subscribers, &snapshot);
}

/// 推一份最新快照给控制连接 + 全部旁观者，写失败的直接摘掉（对齐终端
/// `out.watchers.retain_mut`/PTY 泵的写失败清理策略）。`should_persist` 是
/// "这次变化是怎么发生的"这个上下文，调用方按场景传：事件驱动的走
/// `ApplyOutcome::should_persist`；用户动作（发 prompt/选权限）驱动的固定
/// false——跟旧版行为一致，用户主动发起的变化不单独触发落盘，等下一次
/// 协议事件（通常是 TurnEnded）时一并存。
fn push_acp_snapshot(sess: &AcpSession, should_persist: bool) {
    let snap = sess.reduced.lock().unwrap().to_snapshot(should_persist);
    let payload = serde_json::json!({ "snapshot": snap }).to_string();
    let mut out = sess.out.lock().unwrap();
    if let Some(c) = &mut out.client {
        if writeln!(c, "{payload}").is_err() {
            out.client = None;
        }
    }
    out.watchers.retain_mut(|w| writeln!(w, "{payload}").is_ok());
}

/// 事件 drain：整个会话生命周期只有这一条线程在改 `reduced`（`apply_acp_user_action`
/// 里权限/选择题相关的写也在这条线程外发生，但两边改的是不相交的字段/走
/// 互斥锁，不会踩踏）。通道关闭（连接线程收尾）就退出；如果退出时相位还不是
/// `Ended`（没收到 `Fatal` 就断，比如连接线程 panic），兜底补一个 Ended，不让
/// GUI 永远卡在「运行中」。
fn start_acp_event_drain(
    sess: Arc<AcpSession>,
    event_rx: smol::channel::Receiver<smelt_core::acp_conn::AcpEvent>,
    subscribers: Subscribers,
) {
    thread::spawn(move || {
        smol::block_on(async {
            while let Ok(ev) = event_rx.recv().await {
                let outcome = {
                    let mut st = sess.reduced.lock().unwrap();
                    smelt_core::acp_session::apply_event(&mut st, ev)
                };
                push_acp_snapshot(&sess, outcome.should_persist);
                update_acp_daemon_state(&sess, &subscribers);
            }
        });
        sess.handle.lock().unwrap().take(); // drop：ChildGuard 收尸子进程组
        let already_ended =
            matches!(sess.reduced.lock().unwrap().phase, smelt_core::acp_session::AcpPhase::Ended(_));
        if !already_ended {
            sess.reduced.lock().unwrap().phase =
                smelt_core::acp_session::AcpPhase::Ended("连接意外中断".to_string());
            push_acp_snapshot(&sess, true);
        }
        update_acp_daemon_state(&sess, &subscribers);
    });
}

/// spawn 一次连接（首次建会话 / 「重新开始」共用）：先按旧版 GUI `restart()`
/// 的规则重置回合态字段，再起连接线程、挂事件 drain。
fn acp_relaunch(
    sess: &Arc<AcpSession>,
    id: &str,
    cmd: String,
    resume_id: Option<String>,
    subscribers: &Subscribers,
) {
    smelt_core::acp_session::reset_for_restart(&mut sess.reduced.lock().unwrap());
    let needs_check = sess.agent_needs_transcript_check;
    let handle = smelt_core::acp_conn::spawn_acp(smelt_core::acp_conn::AcpLaunch {
        cmd: cmd.clone(),
        cwd: sess.cwd.clone(),
        sid: id.to_string(),
        resume_session_id: resume_id.map(agent_client_protocol::schema::v1::SessionId::new),
        resume_needs_transcript_check: needs_check,
    });
    let event_rx = handle.event_rx.clone();
    *sess.handle.lock().unwrap() = Some(handle);
    sess.state.lock().unwrap().launch = Some(cmd);
    push_acp_snapshot(sess, false); // 刚 spawn，还没有新内容，不用触发落盘
    update_acp_daemon_state(sess, subscribers);
    start_acp_event_drain(Arc::clone(sess), event_rx, subscribers.clone());
}

fn acp_spawn(
    id: &str,
    cwd: Option<String>,
    cmd: String,
    agent_needs_transcript_check: bool,
    resume_id: Option<String>,
    acp_sessions: &AcpSessions,
    subscribers: &Subscribers,
) -> Arc<AcpSession> {
    let sess = Arc::new(AcpSession {
        reduced: Mutex::new(smelt_core::acp_session::AcpSessionState::default()),
        handle: Mutex::new(None),
        cwd: cwd.clone(),
        agent_needs_transcript_check,
        state: Arc::new(Mutex::new(SessionState {
            id: id.to_string(),
            cwd,
            launch: None,
            title: None,
            phase: Phase::Idle,
            phase_since: 0,
            pending_question: None,
            tokens_used: None,
            branch: None,
            dirty_files: Vec::new(),
            updated_at: 0,
        })),
        out: Mutex::new(AcpOut { client: None, watchers: Vec::new() }),
    });
    acp_relaunch(&sess, id, cmd, resume_id, subscribers);
    // 必须登记进共享表：不然 watch/list/kill 都找不到这条会话，升级时的 fd
    // 收集循环（handle_upgrade）也看不到它，会话会在下一次无缝升级时静默丢失。
    acp_sessions.lock().unwrap().insert(id.to_string(), Arc::clone(&sess));
    sess
}

fn apply_acp_user_action(
    sess: &AcpSession,
    action: smelt_core::acp_session::AcpUserAction,
    subscribers: &Subscribers,
) {
    use smelt_core::acp_conn::AcpCommand;
    use smelt_core::acp_session::AcpUserAction;
    match action {
        AcpUserAction::Prompt { text, images } => {
            let cmd_tx = sess.handle.lock().unwrap().as_ref().map(|h| h.cmd_tx.clone());
            let Some(cmd_tx) = cmd_tx else { return };
            let img_count = images.len();
            // 展示文案跟旧版 GUI `send_prompt` 一致：base64 图片体积大，历史里
            // 只留「带了几张图」的标记。
            let shown = match (text.is_empty(), img_count) {
                (_, 0) => text.clone(),
                (true, n) => format!("[{n} 张图片]"),
                (false, n) => format!("{text}\n[{n} 张图片]"),
            };
            if cmd_tx.try_send(AcpCommand::Prompt { text, images }).is_ok() {
                smelt_core::acp_session::note_prompt_sent(&mut sess.reduced.lock().unwrap(), shown);
                push_acp_snapshot(sess, false);
                update_acp_daemon_state(sess, subscribers);
            }
        }
        AcpUserAction::Cancel => {
            if let Some(h) = sess.handle.lock().unwrap().as_ref() {
                let _ = h.cmd_tx.try_send(AcpCommand::Cancel);
            }
        }
        AcpUserAction::SetModel(model_id) => {
            if let Some(h) = sess.handle.lock().unwrap().as_ref() {
                let _ = h.cmd_tx.try_send(AcpCommand::SetModel(model_id));
            }
        }
        AcpUserAction::PermissionSelect { option_id } => {
            smelt_core::acp_session::select_permission(&mut sess.reduced.lock().unwrap(), &option_id);
            push_acp_snapshot(sess, false);
            update_acp_daemon_state(sess, subscribers);
        }
        AcpUserAction::ElicitationChoose { field_ix, opt_ix } => {
            let auto_submit = smelt_core::acp_session::choose_elicitation(
                &mut sess.reduced.lock().unwrap(),
                field_ix,
                opt_ix,
            );
            if auto_submit {
                smelt_core::acp_session::submit_elicitation(&mut sess.reduced.lock().unwrap());
            }
            push_acp_snapshot(sess, false);
            update_acp_daemon_state(sess, subscribers);
        }
        AcpUserAction::ElicitationSubmit => {
            smelt_core::acp_session::submit_elicitation(&mut sess.reduced.lock().unwrap());
            push_acp_snapshot(sess, false);
            update_acp_daemon_state(sess, subscribers);
        }
        AcpUserAction::ElicitationDismiss => {
            smelt_core::acp_session::dismiss_elicitation(&mut sess.reduced.lock().unwrap());
            push_acp_snapshot(sess, false);
            update_acp_daemon_state(sess, subscribers);
        }
    }
}

fn handle_acp_open(
    conn: UnixStream,
    mut reader: BufReader<UnixStream>,
    v: &serde_json::Value,
    acp_sessions: AcpSessions,
    subscribers: Subscribers,
) {
    let id = v["id"].as_str().unwrap_or_default().to_string();
    if id.is_empty() {
        return;
    }
    let cwd = v["cwd"].as_str().map(String::from);
    let cmd = v["cmd"].as_str().unwrap_or_default().to_string();
    let needs_check = v["agent"].as_str().unwrap_or("claude") == "claude";
    let req_resume_id = v["resume_id"].as_str().map(String::from);

    let existing = acp_sessions.lock().unwrap().get(&id).cloned();
    let sess = match existing {
        Some(s) => {
            let alive = s.handle.lock().unwrap().is_some();
            if !alive && !cmd.is_empty() {
                // 已经 Ended（或还没真正连接过）：这次 open 等于「重新开始」。
                // 优先用已知的旧 agent session id 真续接，没有才退回请求带的
                // （比如历史会话页第一次点「继续」，本地还没有 acp_session_id）。
                let known = s.reduced.lock().unwrap().acp_session_id.clone();
                acp_relaunch(&s, &id, cmd, known.or(req_resume_id), &subscribers);
            }
            s
        }
        None => acp_spawn(&id, cwd, cmd, needs_check, req_resume_id, &acp_sessions, &subscribers),
    };

    let attached_fd = {
        let Ok(mut c) = conn.try_clone() else { return };
        let fd = c.as_raw_fd();
        let _ = c.set_write_timeout(Some(CLIENT_WRITE_TIMEOUT));
        let snapshot = sess.reduced.lock().unwrap().to_snapshot(false);
        if writeln!(c, "{}", serde_json::json!({ "snapshot": snapshot })).is_err() {
            return;
        }
        let mut out = sess.out.lock().unwrap();
        if let Some(old) = out.client.take() {
            let _ = old.shutdown(Shutdown::Both); // 顶掉旧连接（同 id 只允许一个控制连接）
        }
        out.client = Some(c);
        fd
    };

    // 动作循环：一行一个 AcpUserAction 的 JSON，直到客户端断开。
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let Ok(action) = serde_json::from_str(line.trim()) else {
            continue; // 认不出的行跳过，别让一条坏数据掐断整条连接
        };
        apply_acp_user_action(&sess, action, &subscribers);
    }

    let mut out = sess.out.lock().unwrap();
    if out.client.as_ref().map(|c| c.as_raw_fd()) == Some(attached_fd) {
        out.client = None;
    }
}

/// 只读旁观：会话必须已存在（没有 ACP 版本的「不存在就兜底 spawn」——旁观一个
/// 没人开过的会话没有意义），不参与 client 顶替，可多个并存。
fn handle_acp_watch(
    conn: UnixStream,
    mut reader: BufReader<UnixStream>,
    v: &serde_json::Value,
    acp_sessions: AcpSessions,
) {
    let id = v["id"].as_str().unwrap_or_default().to_string();
    if id.is_empty() {
        return;
    }
    let Some(sess) = acp_sessions.lock().unwrap().get(&id).cloned() else { return };
    let attached_fd = {
        let Ok(mut c) = conn.try_clone() else { return };
        let fd = c.as_raw_fd();
        let _ = c.set_write_timeout(Some(CLIENT_WRITE_TIMEOUT));
        let snapshot = sess.reduced.lock().unwrap().to_snapshot(false);
        if writeln!(c, "{}", serde_json::json!({ "snapshot": snapshot })).is_err() {
            return;
        }
        sess.out.lock().unwrap().watchers.push(c);
        fd
    };
    let mut scratch = [0u8; 64];
    let _ = reader.read(&mut scratch);
    sess.out.lock().unwrap().watchers.retain(|w| w.as_raw_fd() != attached_fd);
}

/// 杀会话：子进程、连接、旁观者全部收尾，从表里摘掉。跟终端 `kill` 是同一种
/// 「立即生效、不等收尾」的语气。
fn handle_acp_kill(conn: UnixStream, v: &serde_json::Value, acp_sessions: &AcpSessions) {
    let id = v["id"].as_str().unwrap_or_default();
    if let Some(s) = acp_sessions.lock().unwrap().remove(id) {
        if let Some(h) = s.handle.lock().unwrap().take() {
            let _ = h.cmd_tx.try_send(smelt_core::acp_conn::AcpCommand::Shutdown);
        }
        let mut out = s.out.lock().unwrap();
        if let Some(c) = out.client.take() {
            let _ = c.shutdown(Shutdown::Both);
        }
        for w in out.watchers.drain(..) {
            let _ = w.shutdown(Shutdown::Both);
        }
    }
    let mut c = conn;
    let _ = writeln!(c, "{}", serde_json::json!({ "ok": true }));
}

/// 无缝升级：快照会话表 → 写交接文件 → exec 磁盘上的新二进制（流程见文件头注释）。
///
/// 锁策略：只短暂持 sessions 锁拿一份 Arc 列表就放掉——不像早期版本那样一直攥到
/// exec，那样会让 open/list/kill/version 在升级期间全部卡在 sessions 锁上。逐会话
/// 再去拿 out 锁时，靠 handle_open 里给客户端 socket 设的 CLIENT_WRITE_TIMEOUT
/// 兜底：就算某个客户端冻结导致泵线程握着 out 锁在 write_all 里卡住，最多卡
/// CLIENT_WRITE_TIMEOUT 那么久也会因写超时放手，不会无限期挂死。
/// （极小残余窗口：某泵线程恰好已 read 出 ≤8KB 还没拿到锁，这部分随 exec 丢失。
/// 丢的只是"显示字节"不是输入；重连后的 jolt 全屏重绘会盖掉，可接受。）
fn handle_upgrade(
    conn: UnixStream,
    req: &serde_json::Value,
    sessions: &Sessions,
    acp_sessions: &AcpSessions,
    listen_fd: RawFd,
) {
    let mut c = conn;
    // 可选 `"exe":"/path/to/smeltd"`：装 DMG 时先 exec 暂存目录里的新二进制，
    // 再替换 .app，避免「整包覆盖把旧 smeltd SIGKILL、会话全灭再新建」。
    // 未传则 exec current_exe（同路径更新）。
    let exe = if let Some(p) = req["exe"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
        let path = std::path::PathBuf::from(p);
        if !path.is_file() {
            let _ = writeln!(
                c,
                "{}",
                serde_json::json!({ "ok": false, "err": format!("exe 不存在：{}", path.display()) })
            );
            return;
        }
        path
    } else {
        match std::env::current_exe() {
            Ok(p) => p,
            Err(_) => {
                let _ =
                    writeln!(c, "{}", serde_json::json!({ "ok": false, "err": "current_exe 失败" }));
                return;
            }
        }
    };

    // ACP 会话的 fd 裸传：跟 PTY master fd 同一招（dup + 清 CLOEXEC 活过
    // exec()），另外还带上快照数据（entries/phase/model 等，纯数据，序列化
    // 没有问题）和"如果正卡着一张审批/选择题卡片，那条原始请求的原文"——
    // 新进程接上继承来的 fd 后，见 resume_acp_from_fds，先回放这行原文再
    // 接实时字节，SDK 会重新解析出一个绑定同一个原始请求 id 的等价
    // responder，不会丢这张卡，见 acp_conn.rs 里那条注释。
    //
    // 只有还活着（有 handle 且已经拿到 pid/fd——刚发起 spawn、还没跑到那一步
    // 的极窄窗口除外）的会话才能参与；已经 Ended 的没有 fd 可传，交接后就是
    // "这个 id 在新进程里不存在了"，GUI 侧本来就有 AcpSaved 兜底（按
    // resume_session_id 重新走 session/load），不算回归。
    let acp_session_list: Vec<(String, Arc<AcpSession>)> =
        acp_sessions.lock().unwrap().iter().map(|(k, v)| (k.clone(), Arc::clone(v))).collect();
    let mut acp_items = Vec::new();
    let mut acp_fds = Vec::new();
    for (id, sess) in &acp_session_list {
        let stdio = sess.handle.lock().unwrap().as_ref().and_then(|h| *h.stdio.lock().unwrap());
        let Some(stdio) = stdio else { continue }; // Ended，或还没连上——交接不了，留给 GUI 侧兜底
        let cmd = sess.state.lock().unwrap().launch.clone().unwrap_or_default();
        let (snapshot, pending_raw_line) = {
            let reduced = sess.reduced.lock().unwrap();
            (reduced.to_snapshot(false), reduced.pending_raw_request_line().map(str::to_string))
        };
        acp_items.push(serde_json::json!({
            "id": id,
            "stdin_fd": stdio.stdin_fd,
            "stdout_fd": stdio.stdout_fd,
            "pid": stdio.pid,
            "cwd": sess.cwd,
            "cmd": cmd,
            "agent_needs_transcript_check": sess.agent_needs_transcript_check,
            "snapshot": snapshot,
            "pending_raw_line": pending_raw_line,
        }));
        acp_fds.push(stdio.stdin_fd);
        acp_fds.push(stdio.stdout_fd);
    }
    // 交接不了的（没连上/已经 Ended）主动关掉，不留孤儿。
    let handed_off_ids: std::collections::HashSet<&str> =
        acp_items.iter().filter_map(|v| v["id"].as_str()).collect();
    for (id, sess) in &acp_session_list {
        if handed_off_ids.contains(id.as_str()) {
            continue;
        }
        if let Some(h) = sess.handle.lock().unwrap().take() {
            let _ = h.cmd_tx.try_send(smelt_core::acp_conn::AcpCommand::Shutdown);
        }
    }

    let session_list: Vec<(String, Arc<Session>)> =
        sessions.lock().unwrap().iter().map(|(k, v)| (k.clone(), Arc::clone(v))).collect();

    // 挡住并发 spawn：跟 spawn_session 共用 SPAWN_GATE，独占锁一直拿到 exec（或本函数
    // 提前失败返回）为止，防止清 CLOEXEC 的窗口里恰好 fork 出新 shell，把这些 fd
    // 也带过去（见 SPAWN_GATE 定义处注释）。
    let _spawn_gate = SPAWN_GATE.write().unwrap();

    let mut out_guards = Vec::new(); // 持有到 exec，挡住泵线程
    let mut items = Vec::new();
    let mut fds = vec![listen_fd];
    fds.extend(&acp_fds);
    for (id, sess) in &session_list {
        // 锁序 term → out，与泵线程一致，避免死锁。
        let ctl = sess.ctl.lock().unwrap();
        let term = sess.term.lock().unwrap();
        let out = sess.out.lock().unwrap();
        let fd = ctl.master.as_raw_fd();
        let launch = sess.state.lock().unwrap().launch.clone();
        let alt_screen = term.mode().contains(TermMode::ALT_SCREEN);
        // 全会话同一套：可视区 keyframe（可再 feed 进同尺寸 Term，round-trip 安全）。
        // 不写 ring：resume 侧永不 feed ring，写进去只会误导后人再加特判。
        let grid = snapshot_ansi_for_handoff(&term, launch.as_deref());
        items.push(serde_json::json!({
            "id": id,
            "fd": fd,
            "pid": ctl.pid,
            "cols": ctl.cols,
            "rows": ctl.rows,
            "cwd": ctl.cwd,
            "launch": launch,
            "alt_screen": alt_screen,
            "grid": hex_encode(&grid),
        }));
        fds.push(fd);
        drop(term);
        drop(ctl);
        out_guards.push(out);
    }

    // 交接的 fd 全部清 CLOEXEC，让它们活过 exec。
    for &fd in &fds {
        set_cloexec(fd, false);
    }
    let payload = serde_json::json!({
        "listen_fd": listen_fd,
        "sessions": items,
        "acp_sessions": acp_items,
    })
    .to_string();
    let hp = handoff_path();
    if std::fs::write(&hp, payload).is_err() {
        for &fd in &fds {
            set_cloexec(fd, true);
        }
        let _ = writeln!(c, "{}", serde_json::json!({ "ok": false, "err": "写交接文件失败" }));
        return;
    }
    // 含会话 keyframe（屏幕内容），仅本用户可读写；resume 读到即删。
    let _ =
        std::fs::set_permissions(&hp, std::os::unix::fs::PermissionsExt::from_mode(0o600));

    // 先回执再 exec：客户端连接是 CLOEXEC 的，exec 后立即断开，回执必须赶在前面。
    // exec 失败的情况客户端会看到 ok:true 但轮询版本发现没变，按"升级未生效"处理。
    let _ = writeln!(c, "{}", serde_json::json!({ "ok": true }));

    // tunnel/远程网关不参与交接：exec 后新进程状态是空的，必须先杀 cloudflared，
    // 否则旧子进程仍挂在同一 PID 下却无人 stop，且转发的本机端口已随旧线程消失。
    cleanup_sidecar_services();

    // 死前留痕：exec 可能不返回也不失败——被 macOS 内核 SIGKILL（新二进制以
    // cp 覆盖方式安装、同 inode 改写破坏签名时）。日志停在这一行而没有后续的
    // 「交接完成」，就是这种死法。
    dlog(&format!("upgrade: 即将 exec {}（{} 个会话交接）", exe.display(), fds.len()));

    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(&exe).env("SMELTD_HANDOFF", &hp).exec();

    // 走到这里说明 exec 失败（新二进制没法执行）：回滚，守护继续用旧版本服务。
    // 注意：sidecar 已停，调用方若仍需要远程/隧道得再 remote_start/tunnel_start。
    let _ = std::fs::remove_file(&hp);
    for &fd in &fds {
        set_cloexec(fd, true);
    }
    dlog(&format!("upgrade: exec 失败已回滚，继续用旧版服务：{err}"));
    eprintln!("smeltd 无缝升级 exec 失败: {err}");
}

/// 开 PTY + 起 shell（环境设置与 GUI 内嵌版完全一致，见 workspace/terminal.rs 的注释）。
/// `launch`：项目「+」悬浮菜单的 Claude Code / Codex 快捷入口——把要跑的命令直接编进
/// 启动命令行（`-c '<launch>; exec <shell> -l'`），而不是等 shell 起来后再补发按键。
/// 这样从根上没有"shell 是否已经在读 stdin"的时序问题，命令跑完会 exec 回一个
/// 正常交互 login shell，之后就是一个普通会话。
fn spawn_session(
    id: &str,
    rows: u16,
    cols: u16,
    cwd: Option<&str>,
    launch: Option<&str>,
    subscribers: &Subscribers,
) -> anyhow::Result<(Session, Box<dyn Read + Send>)> {
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })?;

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
    let mut cmd = CommandBuilder::new(shell.clone());
    // login shell：拿完整 PATH（.app 双击启动时系统 PATH 很精简）。
    cmd.arg("-l");
    if let Some(launch) = launch {
        cmd.arg("-c");
        cmd.arg(format!("{launch}; exec {shell} -l"));
    }
    if let Some(dir) = cwd {
        cmd.cwd(dir);
    }
    cmd.env("TERM", "xterm-256color");
    // 少数 CLI 只认 COLORTERM 才开 24-bit 真彩（Zed 也会设）。
    cmd.env("COLORTERM", "truecolor");
    // 伪装 iTerm2：让 Claude Code 自动发 OSC 9 通知（GUI 侧捕获），见 terminal.rs 注释。
    cmd.env("TERM_PROGRAM", "iTerm.app");
    cmd.env("TERM_PROGRAM_VERSION", "3.5.0");
    // UTF-8 locale 兜底（无 LANG 时 zsh 落 C locale 会把 UTF-8 续字节转成乱码）。
    if std::env::var("LANG").is_err() {
        cmd.env("LANG", "en_US.UTF-8");
    }
    // 整条 hook 链路的地基：没有它，smelt-notify 没法知道自己在哪个会话里，
    // 后面的 state op 全是空中楼阁（见 docs/state-channel-plan.md）。
    cmd.env("SMELT_SESSION_ID", id);
    cmd.env("SMELT_SOCK", sock_path());
    // 共享锁：多个新会话可以互相并发 spawn，但跟 handle_upgrade 的独占锁互斥——
    // 挡住「fork 出的子进程意外继承 upgrade 正在清 CLOEXEC 的其它会话 fd」（见
    // SPAWN_GATE 定义处注释）。
    let child = {
        let _gate = SPAWN_GATE.read().unwrap();
        pair.slave.spawn_command(cmd)?
    };
    let pid = child
        .process_id()
        .map(|p| p as i32)
        .ok_or_else(|| anyhow::anyhow!("拿不到 shell pid"))?;

    // 把 master fd dup 成自己持有的 File（写端 + 读端各一份），portable_pty 的 pair
    // 在函数结尾 drop、关掉它自己那份 fd——PTY 只要还有 fd 开着就活着。child 句柄
    // 一并丢弃：kill/收尸都用 pid 直接做（portable_pty 的 Child drop 不杀进程）。
    let raw = pair
        .master
        .as_raw_fd()
        .ok_or_else(|| anyhow::anyhow!("拿不到 PTY master fd"))?;
    let master = dup_file(raw)?;
    let pty_reader = master.try_clone()?;
    let state = Arc::new(Mutex::new(SessionState {
        id: id.to_string(),
        cwd: cwd.map(String::from),
        launch: launch.map(String::from),
        ..Default::default()
    }));
    let sess = Session {
        ctl: Mutex::new(Ctl {
            master,
            pid,
            jolt: false,
            cols,
            rows,
            cwd: cwd.map(String::from),
        }),
        out: Mutex::new(Out {
            client: None,
            watchers: Vec::new(),
        }),
        term: Mutex::new(new_daemon_term(
            rows,
            cols,
            StateListener { state: Arc::clone(&state), subscribers: Arc::clone(subscribers) },
        )),
        state,
    };
    Ok((sess, Box::new(pty_reader)))
}

/// PTY 输出泵：读 PTY → advance 常驻 Term → 转发 client / watchers。
/// shell 退出（EOF）：移除会话、断开客户端、收割子进程。
fn start_pty_pump(
    sess: Arc<Session>,
    mut pty_reader: Box<dyn Read + Send>,
    id: String,
    sessions: Sessions,
) {
    thread::spawn(move || {
        let mut buf = [0u8; 8192];
        let mut parser: Processor = Processor::new();
        loop {
            match pty_reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let chunk = &buf[..n];
                    // 先更新网格（锁序 term → out，与 attach 一致）。
                    if let Ok(mut term) = sess.term.lock() {
                        let _ = catch_unwind(AssertUnwindSafe(|| {
                            parser.advance(&mut *term, chunk);
                        }));
                    }
                    let mut out = sess.out.lock().unwrap();
                    if let Some(c) = out.client.as_mut() {
                        if c.write_all(chunk).is_err() {
                            out.client = None; // 客户端已断，会话继续养着
                        }
                    }
                    // 旁观者逐个转发，写失败（已断线）就摘掉；跟 client 互不影响。
                    out.watchers.retain_mut(|w| w.write_all(chunk).is_ok());
                }
            }
        }
        sessions.lock().unwrap().remove(&id);
        let mut out = sess.out.lock().unwrap();
        if let Some(c) = out.client.take() {
            let _ = c.shutdown(Shutdown::Both); // GUI 读到 EOF 即知 shell 退出
        }
        for w in out.watchers.drain(..) {
            let _ = w.shutdown(Shutdown::Both); // 旁观者同样该收到 EOF
        }
        drop(out);
        // 收尸避免僵尸进程。shell 是本进程的子进程，且 exec 交接不改变父子关系
        // （同 PID），所以交接后 waitpid 照常有效。
        let pid = sess.ctl.lock().unwrap().pid;
        unsafe {
            libc::waitpid(pid, std::ptr::null_mut(), 0);
        }
    });
}

// ===================== 网格 → ANSI 快照（完整：history + 可视区 + 模式）=====================

use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Cell;

/// 快捷启动 / 命令行是否像 agent TUI（未必进 1049 备用屏，但灌网格会花屏）。
fn is_agent_tui_launch(launch: Option<&str>) -> bool {
    let Some(l) = launch.map(|s| s.to_ascii_lowercase()) else {
        return false;
    };
    [
        "claude", "grok", "codex", "gemini", "copilot", "aider", "opencode", "cursor agent",
    ]
    .iter()
    .any(|k| l.contains(k))
}

/// 是否按 TUI 处理（备用屏或 agent 启动命令）。
fn is_tui_session<T: EventListener>(term: &Term<T>, launch: Option<&str>) -> bool {
    term.mode().contains(TermMode::ALT_SCREEN) || is_agent_tui_launch(launch)
}

/// GUI 客户端 reattach 用：TUI 只画可视区；主屏 shell 带 scrollback history。
fn snapshot_ansi<T: EventListener>(term: &Term<T>, launch: Option<&str>) -> Vec<u8> {
    if is_tui_session(term, launch) {
        snapshot_viewport(term)
    } else {
        snapshot_with_history(term)
    }
}

/// 写入 handoff.json 的 grid：全会话统一**仅可视区**，可再 feed 进同尺寸空 Term。
fn snapshot_ansi_for_handoff<T: EventListener>(term: &Term<T>, _launch: Option<&str>) -> Vec<u8> {
    snapshot_viewport(term)
}

fn snapshot_viewport<T: EventListener>(term: &Term<T>) -> Vec<u8> {
    let mut out = snapshot_mode_prefix(term, /*clear_scrollback=*/ false);
    paint_viewport_keyframe(&mut out, term);
    snapshot_cursor_suffix(term, &mut out);
    out
}

fn snapshot_with_history<T: EventListener>(term: &Term<T>) -> Vec<u8> {
    let mut out = snapshot_mode_prefix(term, /*clear_scrollback=*/ true);
    paint_history_keyframe(&mut out, term);
    snapshot_cursor_suffix(term, &mut out);
    out
}

fn snapshot_mode_prefix<T: EventListener>(term: &Term<T>, clear_scrollback: bool) -> Vec<u8> {
    let mode = *term.mode();
    let cols = term.columns().max(1);
    let screen_lines = term.screen_lines().max(1);
    let mut out = Vec::with_capacity(cols.saturating_mul(screen_lines).saturating_mul(8));
    out.extend_from_slice(b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l");
    if mode.contains(TermMode::ALT_SCREEN) {
        out.extend_from_slice(b"\x1b[?1049h");
    } else {
        out.extend_from_slice(b"\x1b[?1049l");
    }
    if mode.contains(TermMode::LINE_WRAP) {
        out.extend_from_slice(b"\x1b[?7h");
    } else {
        out.extend_from_slice(b"\x1b[?7l");
    }
    append_mode_restores(&mut out, mode);
    out.extend_from_slice(b"\x1b[?25l\x1b[0m\x1b[H\x1b[2J");
    if clear_scrollback {
        out.extend_from_slice(b"\x1b[3J");
    }
    out
}

fn snapshot_cursor_suffix<T: EventListener>(term: &Term<T>, out: &mut Vec<u8>) {
    let cols = term.columns().max(1);
    let screen_lines = term.screen_lines().max(1);
    let content = term.renderable_content();
    let cursor = content.cursor.point;
    let display_offset = term.grid().display_offset();
    let cursor_row = cursor.line.0 + display_offset as i32;
    if cursor_row >= 0 && (cursor_row as usize) < screen_lines {
        let col = cursor.column.0.min(cols.saturating_sub(1));
        let _ = write!(out, "\x1b[{};{}H", cursor_row as usize + 1, col + 1);
        match content.cursor.shape {
            CursorShape::Hidden => out.extend_from_slice(b"\x1b[?25l"),
            CursorShape::Underline => out.extend_from_slice(b"\x1b[4 q\x1b[?25h"),
            CursorShape::Beam => out.extend_from_slice(b"\x1b[6 q\x1b[?25h"),
            CursorShape::HollowBlock => out.extend_from_slice(b"\x1b[0 q\x1b[?25h"),
            CursorShape::Block => out.extend_from_slice(b"\x1b[2 q\x1b[?25h"),
        }
    }
}

/// TUI 可视区 keyframe：按行 CUP + 绝对 SGR（Codux `terminal_snapshot_data` 同构）。
fn paint_viewport_keyframe<T: EventListener>(out: &mut Vec<u8>, term: &Term<T>) {
    let cols = term.columns().max(1);
    let rows = term.screen_lines().max(1);
    let display_offset = term.grid().display_offset();

    // row → (col → cell 引用通过复制字符+样式)
    let mut grid: Vec<Vec<Option<KeyframeCell>>> = vec![vec![None; cols]; rows];
    for indexed in term.renderable_content().display_iter {
        let row = indexed.point.line.0 + display_offset as i32;
        if row < 0 || row as usize >= rows {
            continue;
        }
        let col = indexed.point.column.0;
        if col >= cols {
            continue;
        }
        let cell = indexed.cell;
        if cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        {
            continue;
        }
        let mut text = String::new();
        if cell.c != '\0' && !cell.c.is_control() {
            text.push(cell.c);
        }
        if let Some(zw) = cell.zerowidth() {
            for &ch in zw {
                if !ch.is_control() {
                    text.push(ch);
                }
            }
        }
        let width = if cell.flags.contains(Flags::WIDE_CHAR) {
            2
        } else {
            1
        };
        // 空白且默认样式：跳过，让主题底透出（Codux 同策略）
        if text.trim().is_empty()
            && is_default_fg(cell.fg)
            && is_default_bg(cell.bg)
            && !cell_has_visuals(cell)
        {
            continue;
        }
        grid[row as usize][col] = Some(KeyframeCell {
            text,
            width,
            style: CellStyle::from_cell(cell),
        });
    }

    emit_keyframe_rows(out, &grid);
}

/// Shell：history + 可视区，按缓冲行顺序硬换行推进（绝对 SGR）。
fn paint_history_keyframe<T: EventListener>(out: &mut Vec<u8>, term: &Term<T>) {
    let cols = term.columns().max(1);
    let top = term.topmost_line();
    let bottom = term.bottommost_line();
    let span = (bottom.0 - top.0 + 1).max(0) as usize;
    let start = if span > SNAPSHOT_MAX_LINES {
        Line(bottom.0 - SNAPSHOT_MAX_LINES as i32 + 1)
    } else {
        top
    };

    let mut rows: Vec<Vec<Option<KeyframeCell>>> = Vec::new();
    let mut line = start;
    while line <= bottom {
        let row = &term.grid()[line];
        let mut cells = vec![None; cols];
        for col in 0..cols {
            let cell = &row[Column(col)];
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            let mut text = String::new();
            if cell.c != '\0' && !cell.c.is_control() {
                text.push(cell.c);
            }
            if let Some(zw) = cell.zerowidth() {
                for &ch in zw {
                    if !ch.is_control() {
                        text.push(ch);
                    }
                }
            }
            let width = if cell.flags.contains(Flags::WIDE_CHAR) {
                2
            } else {
                1
            };
            if text.trim().is_empty()
                && is_default_fg(cell.fg)
                && is_default_bg(cell.bg)
                && !cell_has_visuals(cell)
            {
                continue;
            }
            cells[col] = Some(KeyframeCell {
                text,
                width,
                style: CellStyle::from_cell(cell),
            });
        }
        rows.push(cells);
        line += 1;
    }
    emit_keyframe_rows(out, &rows);
}

fn cell_has_visuals(cell: &Cell) -> bool {
    let f = cell.flags;
    f.intersects(
        Flags::BOLD
            | Flags::DIM
            | Flags::ITALIC
            | Flags::UNDERLINE
            | Flags::DOUBLE_UNDERLINE
            | Flags::UNDERCURL
            | Flags::DOTTED_UNDERLINE
            | Flags::DASHED_UNDERLINE
            | Flags::INVERSE
            | Flags::HIDDEN
            | Flags::STRIKEOUT
            | Flags::BOLD_ITALIC
            | Flags::DIM_BOLD,
    ) || cell.hyperlink().is_some()
}

#[derive(Clone, PartialEq)]
struct CellStyle {
    fg: Color,
    bg: Color,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: u8,
    inverse: bool,
    hidden: bool,
    strike: bool,
    link: Option<String>,
}

impl CellStyle {
    fn default_style() -> Self {
        Self {
            fg: Color::Named(NamedColor::Foreground),
            bg: Color::Named(NamedColor::Background),
            bold: false,
            dim: false,
            italic: false,
            underline: 0,
            inverse: false,
            hidden: false,
            strike: false,
            link: None,
        }
    }

    fn from_cell(cell: &Cell) -> Self {
        let f = cell.flags;
        Self {
            fg: cell.fg,
            bg: cell.bg,
            bold: f.contains(Flags::BOLD) || f.contains(Flags::BOLD_ITALIC),
            dim: f.contains(Flags::DIM) || f.contains(Flags::DIM_BOLD),
            italic: f.contains(Flags::ITALIC) || f.contains(Flags::BOLD_ITALIC),
            underline: underline_kind(f),
            inverse: f.contains(Flags::INVERSE),
            hidden: f.contains(Flags::HIDDEN),
            strike: f.contains(Flags::STRIKEOUT),
            link: cell.hyperlink().map(|h| h.uri().to_string()),
        }
    }
}

#[derive(Clone)]
struct KeyframeCell {
    text: String,
    width: usize,
    style: CellStyle,
}

/// 按行 `\x1b[row;1H` + 绝对 SGR 吐出（Codux `terminal_snapshot_data`）。
/// 每行画完后 `\x1b[K`（EL）清掉行尾残留，避免长输出软换行后 prompt 盖不干净。
fn emit_keyframe_rows(out: &mut Vec<u8>, rows: &[Vec<Option<KeyframeCell>>]) {
    let mut current = CellStyle::default_style();
    for (row_index, row_cells) in rows.iter().enumerate() {
        let Some(last_col) = row_cells.iter().rposition(|c| {
            c.as_ref().is_some_and(|cell| {
                !cell.text.trim().is_empty() || cell.style != CellStyle::default_style()
            })
        }) else {
            // 空行也 CUP + EL，清掉可能残留的旧内容
            let _ = write!(out, "\x1b[{};1H\x1b[K", row_index + 1);
            continue;
        };
        let _ = write!(out, "\x1b[{};1H", row_index + 1);
        let mut col = 0;
        while col <= last_col {
            match &row_cells[col] {
                Some(cell) => {
                    if cell.style != current {
                        if cell.style.link != current.link {
                            emit_link_osc(out, cell.style.link.as_deref());
                        }
                        emit_absolute_sgr(out, &cell.style);
                        current = cell.style.clone();
                    }
                    if cell.text.is_empty() {
                        for _ in 0..cell.width.max(1) {
                            out.push(b' ');
                        }
                    } else {
                        for ch in cell.text.chars() {
                            push_char(out, ch);
                        }
                    }
                    col += cell.width.max(1);
                }
                None => {
                    if current != CellStyle::default_style() {
                        if current.link.is_some() {
                            emit_link_osc(out, None);
                        }
                        out.extend_from_slice(b"\x1b[0m");
                        current = CellStyle::default_style();
                    }
                    out.push(b' ');
                    col += 1;
                }
            }
        }
        // 行尾 EL：抹掉该行 last_col 之后的旧字符（长 cargo 行糊进 prompt 的主因）
        if current != CellStyle::default_style() {
            if current.link.is_some() {
                emit_link_osc(out, None);
            }
            out.extend_from_slice(b"\x1b[0m");
            current = CellStyle::default_style();
        }
        out.extend_from_slice(b"\x1b[K");
    }
    if current != CellStyle::default_style() {
        if current.link.is_some() {
            emit_link_osc(out, None);
        }
        out.extend_from_slice(b"\x1b[0m");
    }
}

fn emit_link_osc(out: &mut Vec<u8>, uri: Option<&str>) {
    out.extend_from_slice(b"\x1b]8;;");
    if let Some(u) = uri {
        out.extend_from_slice(u.as_bytes());
    }
    out.extend_from_slice(b"\x1b\\");
}

/// 绝对 SGR：始终以 `0` 开头（Codux `snapshot_style_sgr`），杜绝差分状态机半截泄漏。
fn emit_absolute_sgr(out: &mut Vec<u8>, style: &CellStyle) {
    let mut params = Vec::with_capacity(32);
    params.push(b'0');
    let push = |params: &mut Vec<u8>, code: u8| {
        params.push(b';');
        push_u8(params, code);
    };
    if style.bold {
        push(&mut params, 1);
    }
    if style.dim {
        push(&mut params, 2);
    }
    if style.italic {
        push(&mut params, 3);
    }
    if style.underline != 0 {
        params.push(b';');
        match style.underline {
            1 => params.extend_from_slice(b"4"),
            2 => params.extend_from_slice(b"4:2"),
            3 => params.extend_from_slice(b"4:3"),
            4 => params.extend_from_slice(b"4:4"),
            5 => params.extend_from_slice(b"4:5"),
            _ => params.extend_from_slice(b"4"),
        }
    }
    if style.inverse {
        push(&mut params, 7);
    }
    if style.hidden {
        push(&mut params, 8);
    }
    if style.strike {
        push(&mut params, 9);
    }
    // 颜色：绝对模式下总是写上（含默认 39/49），与 Codux 一致
    append_color_params_abs(&mut params, true, style.fg);
    append_color_params_abs(&mut params, false, style.bg);

    out.extend_from_slice(b"\x1b[");
    out.extend_from_slice(&params);
    out.push(b'm');
}

fn append_color_params_abs(params: &mut Vec<u8>, is_fg: bool, color: Color) {
    params.push(b';');
    match color {
        Color::Named(n) => {
            push_u8(params, named_sgr_code(n, is_fg));
        }
        Color::Indexed(i) => {
            push_u8(params, if is_fg { 38 } else { 48 });
            params.extend_from_slice(b";5;");
            push_u8(params, i);
        }
        Color::Spec(rgb) => {
            push_u8(params, if is_fg { 38 } else { 48 });
            params.extend_from_slice(b";2;");
            push_u8(params, rgb.r);
            params.push(b';');
            push_u8(params, rgb.g);
            params.push(b';');
            push_u8(params, rgb.b);
        }
    }
}

fn push_char(out: &mut Vec<u8>, ch: char) {
    let mut buf = [0u8; 4];
    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
}

fn append_mode_restores(out: &mut Vec<u8>, mode: TermMode) {
    if mode.contains(TermMode::APP_CURSOR) {
        out.extend_from_slice(b"\x1b[?1h");
    }
    if mode.contains(TermMode::BRACKETED_PASTE) {
        out.extend_from_slice(b"\x1b[?2004h");
    }
    // 鼠标：按实际打开的子模式恢复（SGR 优先）
    if mode.intersects(TermMode::MOUSE_MODE) {
        if mode.contains(TermMode::SGR_MOUSE) {
            out.extend_from_slice(b"\x1b[?1006h");
        }
        if mode.contains(TermMode::MOUSE_REPORT_CLICK) {
            out.extend_from_slice(b"\x1b[?1000h");
        }
        if mode.contains(TermMode::MOUSE_DRAG) {
            out.extend_from_slice(b"\x1b[?1002h");
        }
        if mode.contains(TermMode::MOUSE_MOTION) {
            out.extend_from_slice(b"\x1b[?1003h");
        }
    }
    if mode.contains(TermMode::FOCUS_IN_OUT) {
        out.extend_from_slice(b"\x1b[?1004h");
    }
}

fn underline_kind(flags: Flags) -> u8 {
    if flags.contains(Flags::UNDERCURL) {
        3
    } else if flags.contains(Flags::DOUBLE_UNDERLINE) {
        2
    } else if flags.contains(Flags::DOTTED_UNDERLINE) {
        4
    } else if flags.contains(Flags::DASHED_UNDERLINE) {
        5
    } else if flags.contains(Flags::UNDERLINE) {
        1
    } else {
        0
    }
}

fn push_u8(params: &mut Vec<u8>, n: u8) {
    if n >= 100 {
        params.push(b'0' + n / 100);
        params.push(b'0' + (n / 10) % 10);
        params.push(b'0' + n % 10);
    } else if n >= 10 {
        params.push(b'0' + n / 10);
        params.push(b'0' + n % 10);
    } else {
        params.push(b'0' + n);
    }
}

fn is_default_fg(c: Color) -> bool {
    matches!(c, Color::Named(NamedColor::Foreground))
}
fn is_default_bg(c: Color) -> bool {
    matches!(c, Color::Named(NamedColor::Background))
}

fn named_sgr_code(n: NamedColor, is_fg: bool) -> u8 {
    use NamedColor::*;
    match (n, is_fg) {
        (Black, true) => 30,
        (Red, true) => 31,
        (Green, true) => 32,
        (Yellow, true) => 33,
        (Blue, true) => 34,
        (Magenta, true) => 35,
        (Cyan, true) => 36,
        (White, true) => 37,
        (Foreground, true) => 39,
        (BrightBlack, true) => 90,
        (BrightRed, true) => 91,
        (BrightGreen, true) => 92,
        (BrightYellow, true) => 93,
        (BrightBlue, true) => 94,
        (BrightMagenta, true) => 95,
        (BrightCyan, true) => 96,
        (BrightWhite, true) => 97,
        (Black, false) => 40,
        (Red, false) => 41,
        (Green, false) => 42,
        (Yellow, false) => 43,
        (Blue, false) => 44,
        (Magenta, false) => 45,
        (Cyan, false) => 46,
        (White, false) => 47,
        (Background, false) => 49,
        (BrightBlack, false) => 100,
        (BrightRed, false) => 101,
        (BrightGreen, false) => 102,
        (BrightYellow, false) => 103,
        (BrightBlue, false) => 104,
        (BrightMagenta, false) => 105,
        (BrightCyan, false) => 106,
        (BrightWhite, false) => 107,
        (_, true) => 39,
        (_, false) => 49,
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;
    use alacritty_terminal::vte::ansi::Processor;

    fn visible_text(term: &Term<VoidListener>) -> String {
        term.renderable_content()
            .display_iter
            .map(|i| i.cell.c)
            .filter(|c| *c != '\0')
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// 把整个网格（含 history）逐格 dump 成文本行——`\0`（宽字符占位格）画成 `·`，
    /// 好让 assert 失败时能一眼看出「哪一列开始错位」。
    fn grid_dump(term: &Term<VoidListener>) -> Vec<String> {
        let mut rows = Vec::new();
        let mut line = term.topmost_line();
        let bottom = term.bottommost_line();
        while line <= bottom {
            let mut s = String::new();
            for col in 0..term.columns() {
                let c = term.grid()[line][Column(col)].c;
                s.push(if c == '\0' { '·' } else { c });
            }
            rows.push(s);
            line += 1;
        }
        rows
    }

    /// 逐格 dump **颜色与属性**——`grid_dump` 只比字符，颜色错了它一无所知（真实的
    /// reattach bug 正是「字符都在、前景色被恢复成不可见」，字符级对比全绿）。
    /// 只 dump 非空格单元，输出紧凑，assert 失败时能直接看出哪个格子的 fg/bg 变了。
    fn attr_dump(term: &Term<VoidListener>) -> Vec<String> {
        let mut out = Vec::new();
        let mut line = term.topmost_line();
        let bottom = term.bottommost_line();
        while line <= bottom {
            for col in 0..term.columns() {
                let cell = &term.grid()[line][Column(col)];
                if cell.c == ' ' || cell.c == '\0' {
                    continue; // 空白格的前景色无所谓
                }
                out.push(format!(
                    "({},{}) {:?} fg={:?} bg={:?} flags={:?}",
                    line.0, col, cell.c, cell.fg, cell.bg, cell.flags
                ));
            }
            line += 1;
        }
        out
    }

    /// 快照的根本契约：**重放后的网格必须和原网格逐格相同**。
    /// 比「快照里含某段文本」强得多——丢格、列错位、行粘连都能抓到。
    fn assert_roundtrip(rows: usize, cols: usize, input: &str, what: &str) {
        let size = DaemonTermSize { rows, cols };
        let mut a = Term::new(daemon_term_config(), &size, VoidListener);
        let mut pa: Processor = Processor::new();
        pa.advance(&mut a, input.as_bytes());

        let snap = snapshot_ansi(&a, None);

        let mut b = Term::new(daemon_term_config(), &size, VoidListener);
        let mut pb: Processor = Processor::new();
        pb.advance(&mut b, &snap);

        // 颜色/属性必须也一致——真实 bug 就藏在这里，字符级对比看不见。
        let (want_attr, got_attr) = (attr_dump(&a), attr_dump(&b));
        assert_eq!(
            want_attr,
            got_attr,
            "\n{what}：快照重放后**颜色/属性**错了（字符可能都还在）\n快照字节: {:?}\n",
            String::from_utf8_lossy(&snap)
        );

        let (want, got) = (grid_dump(&a), grid_dump(&b));
        assert_eq!(
            want,
            got,
            "\n{what}：快照重放后网格错位\n原始:\n{}\n重放:\n{}\n快照字节: {:?}",
            want.join("\n"),
            got.join("\n"),
            String::from_utf8_lossy(&snap)
        );
    }

    /// 行尾放不下宽字符：alacritty 在最后一列填 LEADING_WIDE_CHAR_SPACER，宽字符挪到下一行。
    /// 快照 `continue` 跳过这个占位格 → 该行只吐 cols-1 个字符 → 不触发自动折行。
    #[test]
    fn roundtrip_wide_char_at_line_end() {
        assert_roundtrip(4, 8, "abcdefg中x", "行尾宽字符占位格");
    }

    /// 类 Claude Code 底部状态栏：整行背景色铺满 + 中文 + 边框字形（重启后错位的就是这片）。
    #[test]
    fn roundtrip_status_bar_like() {
        assert_roundtrip(
            6,
            40,
            "\x1b[44m current  6%  5:30am │ weekly  48% \x1b[0m\r\n\
             \x1b[2m ctx:18% │ cache:100% │ 检查当前模型 \x1b[0m\r\n> ",
            "状态栏（背景色 + 中文 + 竖线）",
        );
    }

    /// 满行（写满最后一列）后跟硬换行：pending-wrap 状态处理错就会多吞/多吐一行。
    #[test]
    fn roundtrip_full_width_row_then_newline() {
        assert_roundtrip(4, 6, "abcdef\r\nxy", "满行 + 硬换行");
    }

    /// 中文占满整行（每字 2 列，正好铺满）。
    #[test]
    fn roundtrip_cjk_fills_row() {
        assert_roundtrip(4, 6, "中文字\r\nab", "中文铺满行");
    }

    /// SGR 2（DIM）——Claude Code 状态栏的灰字大量用它。怀疑对象 #1。
    #[test]
    fn roundtrip_sgr_dim() {
        assert_roundtrip(3, 20, "\x1b[2mdim gray\x1b[0m ok", "DIM 灰字");
    }

    /// DIM + 前景色组合（暗绿等）。
    #[test]
    fn roundtrip_sgr_dim_with_color() {
        assert_roundtrip(3, 20, "\x1b[2;32mdimgreen\x1b[0m ok", "DIM + 绿");
    }

    /// bright black（90）——另一种常见灰。
    #[test]
    fn roundtrip_sgr_bright_black() {
        assert_roundtrip(3, 20, "\x1b[90mgray\x1b[0m ok", "bright black 灰");
    }

    /// 256 色前景（38;5;244 = 中灰）。
    #[test]
    fn roundtrip_sgr_256color() {
        assert_roundtrip(3, 20, "\x1b[38;5;244mgray\x1b[0m ok", "256 色灰");
    }

    /// 24-bit 真彩前景。
    #[test]
    fn roundtrip_sgr_truecolor() {
        assert_roundtrip(3, 20, "\x1b[38;2;136;136;136mgray\x1b[0m ok", "真彩灰");
    }

    /// 状态栏全家桶：灰边框 + DIM + 绿数字 + 中文，一行内多次切色。
    #[test]
    fn roundtrip_sgr_status_bar_mix() {
        assert_roundtrip(
            4,
            60,
            "\x1b[2m────\x1b[0m\r\n\
             \x1b[2m ctx:\x1b[0m\x1b[32m18%\x1b[0m \x1b[2m│ cache:\x1b[0m\x1b[32m100%\x1b[0m\r\n\
             \x1b[90m current \x1b[0m\x1b[92m11%\x1b[0m \x1b[2m检查模型\x1b[0m",
            "状态栏多色混排",
        );
    }

    #[test]
    fn snapshot_roundtrip_preserves_visible_text() {
        let size = DaemonTermSize { rows: 5, cols: 20 };
        let mut term = Term::new(daemon_term_config(), &size, VoidListener);
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, b"\x1b[31mhello\x1b[0m\r\nworld");

        let snap = snapshot_ansi(&term, None);
        assert!(snap.windows(5).any(|w| w == b"hello"));
        assert!(snap.windows(5).any(|w| w == b"world"));

        let mut term2 = Term::new(daemon_term_config(), &size, VoidListener);
        let mut parser2: Processor = Processor::new();
        parser2.advance(&mut term2, &snap);
        let text = visible_text(&term2);
        assert!(text.contains("hello"), "got {text:?}");
        assert!(text.contains("world"), "got {text:?}");
    }

    #[test]
    fn snapshot_enters_alt_screen_when_active() {
        let size = DaemonTermSize { rows: 4, cols: 10 };
        let mut term = Term::new(daemon_term_config(), &size, VoidListener);
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, b"\x1b[?1049hTUI");
        let snap = snapshot_ansi(&term, None);
        assert!(snap.windows(8).any(|w| w == b"\x1b[?1049h"));
        // Codux 风格 keyframe：备用屏也画可视区内容
        assert!(
            snap.windows(3).any(|w| w == b"TUI"),
            "TUI keyframe 应含可视区文字, got {}",
            String::from_utf8_lossy(&snap)
        );
        assert!(snap.windows(4).any(|w| w == b"\x1b[2J"), "应清屏");
        // 绝对 SGR：每个样式序列以 ESC[0 开头
        assert!(
            snap.windows(4).any(|w| w == b"\x1b[0m") || snap.windows(4).any(|w| w == b"\x1b[0;"),
            "应有绝对 SGR"
        );
    }

    /// agent launch 走 TUI keyframe（可视区），不是空骨架。
    #[test]
    fn snapshot_agent_launch_paints_viewport_keyframe() {
        let size = DaemonTermSize { rows: 4, cols: 20 };
        let mut term = Term::new(daemon_term_config(), &size, VoidListener);
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, b"hello-grok-grid");
        let snap = snapshot_ansi(&term, Some("grok"));
        assert!(
            snap.windows(10).any(|w| w == b"hello-grok"),
            "agent keyframe 应含可视区: {}",
            String::from_utf8_lossy(&snap)
        );
        // 按行 CUP
        assert!(snap.windows(4).any(|w| w == b"\x1b[1;"), "应按行 CUP 定位");
    }

    /// 真彩 SGR 必须以完整 `\x1b[0;…48;2;…m` 形式出现（Codux 绝对 SGR）。
    #[test]
    fn snapshot_truecolor_sgr_always_has_esc_prefix() {
        let size = DaemonTermSize { rows: 3, cols: 20 };
        let mut term = Term::new(daemon_term_config(), &size, VoidListener);
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, b"\x1b[48;2;20;20;20mX\x1b[0m");
        let snap = snapshot_ansi(&term, None);
        let s = String::from_utf8_lossy(&snap);
        // 实际形如 \x1b[0;39;48;2;20;20;20m
        assert!(
            s.contains("\u{1b}[0;39;48;2;20;20;20m") || s.contains("\u{1b}[0;") && s.contains("48;2;20;20;20m"),
            "绝对 SGR 应含完整真彩序列: {s}"
        );
        // 重放后字符仍在
        let mut term2 = Term::new(daemon_term_config(), &size, VoidListener);
        let mut p2: Processor = Processor::new();
        p2.advance(&mut term2, &snap);
        assert!(visible_text(&term2).contains('X'));
    }

    #[test]
    fn is_agent_tui_launch_matches_common_agents() {
        assert!(is_agent_tui_launch(Some("grok")));
        assert!(is_agent_tui_launch(Some("claude --dangerously-skip-permissions")));
        assert!(is_agent_tui_launch(Some("codex")));
        assert!(!is_agent_tui_launch(Some("zsh")));
        assert!(!is_agent_tui_launch(None));
    }

    #[test]
    fn snapshot_includes_scrollback_history() {
        // 3 行屏高，灌 10 行 → 前几行进 history
        let size = DaemonTermSize { rows: 3, cols: 40 };
        let mut term = Term::new(daemon_term_config(), &size, VoidListener);
        let mut parser: Processor = Processor::new();
        for i in 0..10 {
            parser.advance(&mut term, format!("line-{i:02}\r\n").as_bytes());
        }
        // 可视区只有最后几行；快照必须仍带上更早的 line-00
        let snap = snapshot_ansi(&term, None);
        assert!(
            snap.windows(7).any(|w| w == b"line-00"),
            "完整快照应含 scrollback 里的 line-00，实际: {}",
            String::from_utf8_lossy(&snap)
        );
        assert!(snap.windows(7).any(|w| w == b"line-09"));

        // 重放到更大屏，history 内容应可在网格里找到
        let size2 = DaemonTermSize { rows: 20, cols: 40 };
        let mut term2 = Term::new(daemon_term_config(), &size2, VoidListener);
        let mut parser2: Processor = Processor::new();
        parser2.advance(&mut term2, &snap);
        // 扫整个 grid（含 history）
        let mut all = String::new();
        let top = term2.topmost_line();
        let bottom = term2.bottommost_line();
        let mut line = top;
        while line <= bottom {
            for col in 0..term2.columns() {
                all.push(term2.grid()[line][Column(col)].c);
            }
            all.push('\n');
            line += 1;
        }
        assert!(all.contains("line-00"), "重放后 grid 应含 line-00，got {all:?}");
        assert!(all.contains("line-09"), "重放后 grid 应含 line-09");
    }

    #[test]
    fn snapshot_restores_bracketed_paste_mode() {
        let size = DaemonTermSize { rows: 3, cols: 10 };
        let mut term = Term::new(daemon_term_config(), &size, VoidListener);
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, b"\x1b[?2004hhi");
        let snap = snapshot_ansi(&term, None);
        assert!(
            snap.windows(8).any(|w| w == b"\x1b[?2004h"),
            "开了 bracketed paste 的会话快照应恢复该模式"
        );
    }

    #[test]
    fn snapshot_preserves_osc8_hyperlink() {
        let size = DaemonTermSize { rows: 3, cols: 40 };
        let mut term = Term::new(daemon_term_config(), &size, VoidListener);
        let mut parser: Processor = Processor::new();
        parser.advance(
            &mut term,
            b"\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\",
        );
        let snap = snapshot_ansi(&term, None);
        let s = String::from_utf8_lossy(&snap);
        assert!(
            s.contains("https://example.com"),
            "快照应含 OSC 8 URI，got {s}"
        );
        assert!(snap.windows(4).any(|w| w == b"link"));
    }
}

/// Phase 0：`watch` 只读旁观必须能跟 `open` 独占连接并存，且互不干扰。
#[cfg(test)]
mod watch_tests {
    use super::*;

    /// 造一个不依赖真实 shell 的会话：`Ctl.master` 指向 `/dev/null`（测试不发输入帧，
    /// 用不上真正的 PTY 写端），`pid` 用一个已退出、还没被 reap 的真实子进程——
    /// 给 `start_pty_pump` 结束时的 `waitpid` 一个安全、真实存在的目标，不借用 -1
    /// 或随便一个不相关的 pid。
    pub(crate) fn make_dummy_session(rows: u16, cols: u16) -> Arc<Session> {
        let master = std::fs::OpenOptions::new().write(true).open("/dev/null").unwrap();
        let child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id() as i32;
        drop(child); // Child::drop 不 wait()，留成 zombie，交给 pump 收尾时的 waitpid

        let state = Arc::new(Mutex::new(SessionState::default()));
        let subscribers: Subscribers = Arc::new(Mutex::new(Vec::new()));
        let listener = StateListener { state: Arc::clone(&state), subscribers };
        Arc::new(Session {
            ctl: Mutex::new(Ctl { master, pid, jolt: false, cols, rows, cwd: None }),
            out: Mutex::new(Out { client: None, watchers: Vec::new() }),
            term: Mutex::new(new_daemon_term(rows, cols, listener)),
            state,
        })
    }

    /// 读一行 JSON 尺寸头 + `replay_len` 字节快照——跟真实客户端的 attach 协议一致。
    fn read_header_and_snapshot(br: &mut BufReader<UnixStream>) {
        let mut line = String::new();
        br.read_line(&mut line).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        let replay_len = v["replay_len"].as_u64().unwrap() as usize;
        let mut snap = vec![0u8; replay_len];
        br.read_exact(&mut snap).unwrap();
    }

    #[test]
    fn watch_coexists_with_open_and_survives_watcher_disconnect() {
        let sess = make_dummy_session(24, 80);
        let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
        sessions.lock().unwrap().insert("t".to_string(), Arc::clone(&sess));

        // 模拟 PTY：pump 从一端读，测试从另一端写，模拟"shell 产生了输出"。
        let (pty_reader_end, mut pty_writer_end) = UnixStream::pair().unwrap();
        start_pty_pump(Arc::clone(&sess), Box::new(pty_reader_end), "t".to_string(), Arc::clone(&sessions));

        // 第一路：open（同 id 唯一 client）。
        let (open_server, open_client) = UnixStream::pair().unwrap();
        let sessions_a = Arc::clone(&sessions);
        let subscribers_a: Subscribers = Arc::new(Mutex::new(Vec::new()));
        thread::spawn(move || {
            let reader = BufReader::new(open_server.try_clone().unwrap());
            handle_open(
                open_server,
                reader,
                &serde_json::json!({"id":"t","cols":80,"rows":24}),
                sessions_a,
                subscribers_a,
            );
        });
        let mut open_br = BufReader::new(open_client.try_clone().unwrap());
        read_header_and_snapshot(&mut open_br);

        // 第二路：watch（只读旁观）。这一步不该顶掉上面那个 open 连接。
        let (watch_server, watch_client) = UnixStream::pair().unwrap();
        let sessions_b = Arc::clone(&sessions);
        thread::spawn(move || {
            let reader = BufReader::new(watch_server.try_clone().unwrap());
            handle_watch(watch_server, reader, &serde_json::json!({"id":"t"}), sessions_b);
        });
        let mut watch_br = BufReader::new(watch_client.try_clone().unwrap());
        read_header_and_snapshot(&mut watch_br);

        // 模拟 shell 输出一行字节，open 和 watch 都该收到同一份转发。
        pty_writer_end.write_all(b"hello\r\n").unwrap();

        let mut open_buf = [0u8; 7];
        open_br.read_exact(&mut open_buf).unwrap();
        assert_eq!(&open_buf, b"hello\r\n", "open 没收到转发——watch 的接入可能把它顶掉了");

        let mut watch_buf = [0u8; 7];
        watch_br.read_exact(&mut watch_buf).unwrap();
        assert_eq!(&watch_buf, b"hello\r\n", "watch 没收到转发");

        // watcher 断开，不该影响 open 那一路继续收转发（惰性清理：写失败即摘除，
        // 不依赖 handle_watch 自己那个线程的清理时序）。
        drop(watch_br);
        drop(watch_client);

        pty_writer_end.write_all(b"world!\n").unwrap();
        let mut open_buf2 = [0u8; 7];
        open_br.read_exact(&mut open_buf2).unwrap();
        assert_eq!(&open_buf2, b"world!\n", "watcher 断线后不该影响 open 那一路的转发");

        // 收尾：关掉模拟 PTY 的写端，触发 pump 的退出清理（移除会话表项 + waitpid）。
        drop(pty_writer_end);
        let mut removed = false;
        for _ in 0..50 {
            if !sessions.lock().unwrap().contains_key("t") {
                removed = true;
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(removed, "pump 应在 PTY EOF 后把会话从表里摘掉");

        drop(open_br);
        drop(open_client);
    }

    /// subscribe：首帧全量快照，之后 state 变化推一行——跟真实 `state` op 走的是
    /// 同一条 broadcast_state 路径。
    #[test]
    fn subscribe_gets_snapshot_then_broadcast_on_state_change() {
        let sess = make_dummy_session(24, 80);
        sess.state.lock().unwrap().id = "sub-test".to_string();

        let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
        sessions.lock().unwrap().insert("sub-test".to_string(), Arc::clone(&sess));
        let subscribers: Subscribers = Arc::new(Mutex::new(Vec::new()));

        let (sub_server, sub_client) = UnixStream::pair().unwrap();
        let sessions_b = Arc::clone(&sessions);
        let acp_sessions_b: AcpSessions = Arc::new(Mutex::new(HashMap::new()));
        let subscribers_b = Arc::clone(&subscribers);
        thread::spawn(move || {
            handle_subscribe(sub_server, &sessions_b, &acp_sessions_b, &subscribers_b);
        });

        let mut br = BufReader::new(sub_client.try_clone().unwrap());
        let mut line = String::new();
        br.read_line(&mut line).unwrap();
        let first: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(first["sessions"].as_array().unwrap().len(), 1);
        assert_eq!(first["sessions"][0]["id"], "sub-test");

        let snapshot = {
            let mut st = sess.state.lock().unwrap();
            st.phase = Phase::AwaitingApproval;
            st.pending_question = Some("要不要继续".to_string());
            st.clone()
        };
        broadcast_state(&subscribers, &snapshot);

        let mut line2 = String::new();
        br.read_line(&mut line2).unwrap();
        let second: serde_json::Value = serde_json::from_str(&line2).unwrap();
        assert_eq!(second["session"]["phase"], "awaiting_approval");
        assert_eq!(second["session"]["pending_question"], "要不要继续");
    }

    /// `state` op 与 `subscribe` 并发时不得 ABBA 死锁——两边的锁序必须一致。
    ///
    /// 这两条路径在真实环境里天天并发：`state` 是 Claude hooks 每次状态变化都在打的，
    /// `subscribe` 是 GUI 状态通道常驻的。一旦成环，`sessions` 会被永久锁死，
    /// `open`/`list`/`kill`/`version`/`upgrade` 全部卡住——PTY 还活着，但守护废了，
    /// 用户只能 pkill，**正在跑的 agent 会话全灭**。CLIENT_WRITE_TIMEOUT 救不了：
    /// 卡在锁获取上，不是卡在 write。
    ///
    /// 易错点（本 bug 的成因）：`if let Some(x) = m.lock().unwrap().get(..).cloned() { .. }`
    /// 里那把 guard **活到整个 body 结束**（两个 edition 都如此，Rust 2024 的
    /// if-let rescope 只改 else 分支）。旁边的 action/input/resize 用 let-else，
    /// guard 在语句末即释放，所以只有 state 这一条路径会成环。
    #[test]
    fn state_op_and_subscribe_do_not_deadlock() {
        let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
        let sess = make_dummy_session(24, 80);
        sess.state.lock().unwrap().id = "dl".to_string();
        sessions.lock().unwrap().insert("dl".to_string(), sess);
        let subscribers: Subscribers = Arc::new(Mutex::new(Vec::new()));

        const ROUNDS: usize = 300;
        let (done_tx, done_rx) = std::sync::mpsc::channel::<&'static str>();

        // A：反复走 state op（真 handle_conn 分发）
        {
            let sessions = Arc::clone(&sessions);
            let subscribers = Arc::clone(&subscribers);
            let tx = done_tx.clone();
            thread::spawn(move || {
                for _ in 0..ROUNDS {
                    let (server, client) = UnixStream::pair().unwrap();
                    let mut client = client;
                    writeln!(
                        client,
                        "{}",
                        serde_json::json!({ "op": "state", "id": "dl", "phase": "thinking" })
                    )
                    .unwrap();
                    handle_conn(
                        server,
                        Arc::clone(&sessions),
                        Arc::new(Mutex::new(HashMap::new())),
                        0,
                        -1,
                        Arc::new(Mutex::new(None)),
                        Arc::new(Mutex::new(None)),
                        Arc::clone(&subscribers),
                    );
                }
                let _ = tx.send("state");
            });
        }

        // B：反复走 subscribe（持 subscribers 求 sessions，与 A 反向）。
        // handle_subscribe 注册完会阻塞在 read 上等客户端断开（长连接，同 handle_watch），
        // 所以必须另起线程跑它、由本线程 drop 掉 client 放它走——直接调用会把本线程
        // 当场焊死在 read 上（这里踩过一次，卡住的是测试自己，不是产品）。
        {
            let sessions = Arc::clone(&sessions);
            let subscribers = Arc::clone(&subscribers);
            let tx = done_tx.clone();
            thread::spawn(move || {
                for _ in 0..ROUNDS {
                    let (server, client) = UnixStream::pair().unwrap();
                    let s = Arc::clone(&sessions);
                    let acp_s: AcpSessions = Arc::new(Mutex::new(HashMap::new()));
                    let sub = Arc::clone(&subscribers);
                    let h = thread::spawn(move || handle_subscribe(server, &s, &acp_s, &sub));
                    // 立刻断开：read 拿到 EOF 就收尾退出。要抓的锁序（持 subscribers
                    // → 求 sessions）在注册阶段、早于 read，此时已经跑过了。
                    drop(client);
                    let _ = h.join();
                }
                let _ = tx.send("subscribe");
            });
        }
        drop(done_tx);

        for _ in 0..2 {
            if done_rx.recv_timeout(std::time::Duration::from_secs(20)).is_err() {
                panic!(
                    "state op 与 subscribe 并发死锁：20 秒内未跑完 {ROUNDS} 轮。\
                     state 持 sessions 求 subscribers，subscribe 持 subscribers 求 sessions"
                );
            }
        }
    }
}

/// ACP 会话托管：不 spawn 真实 agent 子进程（测试环境里也没有已登录的
/// claude/codex 可用），直接构造 `AcpSession`/`AcpSessionState` 驱动被测函数，
/// 只测「smeltd 这一层的管子接对了没」——归约本身（entries 合并/phase 机/
/// 回声去重等）已经在 smelt_core::acp_session 的单测里覆盖过，这里不重复。
#[cfg(test)]
mod acp_tests {
    use super::*;
    use smelt_core::acp_chat::{AcpEntry, ToolCallStatus, ToolKind};
    use smelt_core::acp_session::{AcpPhase, AcpSessionState};

    fn make_acp_session(id: &str, reduced: AcpSessionState) -> Arc<AcpSession> {
        Arc::new(AcpSession {
            reduced: Mutex::new(reduced),
            handle: Mutex::new(None),
            cwd: None,
            agent_needs_transcript_check: true,
            state: Arc::new(Mutex::new(SessionState { id: id.to_string(), ..Default::default() })),
            out: Mutex::new(AcpOut { client: None, watchers: Vec::new() }),
        })
    }

    #[test]
    fn watch_on_unknown_session_just_disconnects() {
        let acp_sessions: AcpSessions = Arc::new(Mutex::new(HashMap::new()));
        let (server, client) = UnixStream::pair().unwrap();
        let reader = BufReader::new(server.try_clone().unwrap());
        handle_acp_watch(server, reader, &serde_json::json!({"id": "acp-nope"}), acp_sessions);
        // 没有会话可接：函数直接 return，客户端读到 EOF（不是某行 JSON）。
        let mut buf = Vec::new();
        BufReader::new(client).read_to_end(&mut buf).unwrap();
        assert!(buf.is_empty());
    }

    #[test]
    fn watch_delivers_initial_snapshot_matching_to_snapshot() {
        let mut reduced = AcpSessionState::default();
        reduced.entries.push(AcpEntry::User("hi".into()));
        reduced.phase = AcpPhase::Idle;
        let expected = reduced.to_snapshot(false);

        let sess = make_acp_session("acp-1", reduced);
        let acp_sessions: AcpSessions = Arc::new(Mutex::new(HashMap::new()));
        acp_sessions.lock().unwrap().insert("acp-1".to_string(), Arc::clone(&sess));

        let (server, client) = UnixStream::pair().unwrap();
        let reader = BufReader::new(server.try_clone().unwrap());
        let h = thread::spawn(move || {
            handle_acp_watch(server, reader, &serde_json::json!({"id": "acp-1"}), acp_sessions);
        });

        let mut br = BufReader::new(client.try_clone().unwrap());
        let mut line = String::new();
        br.read_line(&mut line).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["snapshot"]["entries"].as_array().unwrap().len(), 1);
        assert_eq!(
            serde_json::to_value(&expected).unwrap()["entries"],
            v["snapshot"]["entries"]
        );

        // 全部克隆（`br` 内部那份 + 这个原始 `client`）都要丢，socket 才会真正
        // 关闭产生 EOF——只 drop 一份，另一份还开着，对端读不到 EOF 会一直卡住
        // （这个坑踩过一次，见 watch_tests 里同款收尾写法）。
        drop(br);
        drop(client);
        h.join().unwrap();
    }

    #[test]
    fn push_snapshot_reaches_control_client_and_watchers_and_drops_dead_ones() {
        let sess = make_acp_session("acp-2", AcpSessionState::default());

        let (c_server, c_client) = UnixStream::pair().unwrap();
        let (w_server, w_client) = UnixStream::pair().unwrap();
        {
            let mut out = sess.out.lock().unwrap();
            out.client = Some(c_server);
            out.watchers.push(w_server);
        }
        drop(c_client); // 控制连接对端已经断了：推送应该发现写失败并自己摘掉

        push_acp_snapshot(&sess, false);

        assert!(sess.out.lock().unwrap().client.is_none(), "写失败的 client 该被摘掉");
        assert_eq!(sess.out.lock().unwrap().watchers.len(), 1, "还活着的 watcher 不该被牵连摘掉");

        let mut line = String::new();
        BufReader::new(w_client).read_line(&mut line).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert!(v.get("snapshot").is_some());
    }

    #[test]
    fn kill_removes_session_and_closes_connections() {
        let sess = make_acp_session("acp-3", AcpSessionState::default());
        let acp_sessions: AcpSessions = Arc::new(Mutex::new(HashMap::new()));
        acp_sessions.lock().unwrap().insert("acp-3".to_string(), Arc::clone(&sess));

        let (c_server, c_client) = UnixStream::pair().unwrap();
        sess.out.lock().unwrap().client = Some(c_server);

        let (server, client) = UnixStream::pair().unwrap();
        handle_acp_kill(server, &serde_json::json!({"id": "acp-3"}), &acp_sessions);

        let mut resp = String::new();
        BufReader::new(client).read_line(&mut resp).unwrap();
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["ok"], true);
        assert!(!acp_sessions.lock().unwrap().contains_key("acp-3"));

        // 控制连接该被强制关掉：对端读到 EOF。
        let mut buf = Vec::new();
        BufReader::new(c_client).read_to_end(&mut buf).unwrap();
        assert!(buf.is_empty());
    }

    /// kill 一个不存在的 id：跟终端 `kill` 一样静默回 ok，不报错。
    #[test]
    fn kill_unknown_session_is_a_harmless_no_op() {
        let acp_sessions: AcpSessions = Arc::new(Mutex::new(HashMap::new()));
        let (server, client) = UnixStream::pair().unwrap();
        handle_acp_kill(server, &serde_json::json!({"id": "acp-ghost"}), &acp_sessions);
        let mut resp = String::new();
        BufReader::new(client).read_line(&mut resp).unwrap();
        assert_eq!(serde_json::from_str::<serde_json::Value>(&resp).unwrap()["ok"], true);
    }

    /// 帮手：走一遍 `handle_acp_open` 的完整流程（跟真实客户端一样连接→读首行
    /// 快照→断开），cmd 用一个必然不存在的路径——`spawn_acp` 保证不阻塞调用方
    /// （见文件头职责边界），子进程起不来只会异步产出 `AcpEvent::Fatal`，不
    /// 影响这里要测的「登记进表」这件事。
    fn open_acp_session_once(id: &str, acp_sessions: &AcpSessions, subscribers: &Subscribers) {
        let (server, client) = UnixStream::pair().unwrap();
        let reader = BufReader::new(server.try_clone().unwrap());
        let acp_sessions2 = Arc::clone(acp_sessions);
        let subscribers2 = Arc::clone(subscribers);
        let id_owned = id.to_string();
        let h = thread::spawn(move || {
            handle_acp_open(
                server,
                reader,
                &serde_json::json!({"id": id_owned, "cmd": "/definitely/not/a/real/binary-xyz"}),
                acp_sessions2,
                subscribers2,
            );
        });
        let mut br = BufReader::new(client.try_clone().unwrap());
        let mut line = String::new();
        br.read_line(&mut line).unwrap(); // 读到首行快照，说明 acp_spawn 已经跑完
        drop(br);
        drop(client); // 两份 clone 都要丢，读循环那头才会真正见到 EOF 退出
        h.join().unwrap();
    }

    /// 回归 code review 发现的高严重度 bug：`acp_spawn` 建了新会话却从没插进
    /// `acp_sessions` 表，导致 watch/list/kill 都找不到它，`handle_upgrade`
    /// 收集 fd 时也会漏掉它——无缝升级直接把这条会话弄丢。
    #[test]
    fn open_new_session_registers_it_in_acp_sessions_table() {
        let acp_sessions: AcpSessions = Arc::new(Mutex::new(HashMap::new()));
        let subscribers: Subscribers = Arc::new(Mutex::new(Vec::new()));

        open_acp_session_once("acp-new", &acp_sessions, &subscribers);

        assert!(
            acp_sessions.lock().unwrap().contains_key("acp-new"),
            "新建会话必须登记进 acp_sessions，不然 watch/list/kill 和无缝升级的 fd 收集都找不到它"
        );
    }

    /// 同一个 bug 的另一面：表里没有它，`handle_acp_open` 的「已存在就复用」
    /// 分支永远命中不了 —— 同一个 id 重开一次就会再走一遍 `acp_spawn`，多起
    /// 一个 agent 子进程，旧的那个泄漏在后台再也够不着。
    #[test]
    fn reopening_same_id_reuses_existing_session_instead_of_spawning_a_duplicate() {
        let acp_sessions: AcpSessions = Arc::new(Mutex::new(HashMap::new()));
        let subscribers: Subscribers = Arc::new(Mutex::new(Vec::new()));

        open_acp_session_once("acp-dup", &acp_sessions, &subscribers);
        let first = acp_sessions.lock().unwrap().get("acp-dup").cloned().expect("首次打开该已登记");

        open_acp_session_once("acp-dup", &acp_sessions, &subscribers);
        let second = acp_sessions.lock().unwrap().get("acp-dup").cloned().expect("重开该还在表里");

        assert_eq!(acp_sessions.lock().unwrap().len(), 1, "同一个 id 重开不该在表里多出一条");
        assert!(
            Arc::ptr_eq(&first, &second),
            "重开应该复用已登记的会话，不能是 acp_spawn 又建了一个新对象（否则旧 agent 进程/线程直接泄漏）"
        );
    }

    #[test]
    fn subscribe_snapshot_merges_terminal_and_acp_sessions() {
        let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
        let term_sess = watch_tests::make_dummy_session(24, 80);
        term_sess.state.lock().unwrap().id = "term-1".to_string();
        sessions.lock().unwrap().insert("term-1".to_string(), term_sess);

        let acp_sessions: AcpSessions = Arc::new(Mutex::new(HashMap::new()));
        acp_sessions
            .lock()
            .unwrap()
            .insert("acp-1".to_string(), make_acp_session("acp-1", AcpSessionState::default()));

        let subscribers: Subscribers = Arc::new(Mutex::new(Vec::new()));
        let (server, client) = UnixStream::pair().unwrap();
        let acp_sessions2 = Arc::clone(&acp_sessions);
        let h = thread::spawn(move || handle_subscribe(server, &sessions, &acp_sessions2, &subscribers));

        let mut line = String::new();
        BufReader::new(client.try_clone().unwrap()).read_line(&mut line).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        let ids: Vec<&str> = v["sessions"].as_array().unwrap().iter().map(|s| s["id"].as_str().unwrap()).collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"term-1"));
        assert!(ids.contains(&"acp-1"));

        drop(client);
        h.join().unwrap();
    }

    #[test]
    fn daemon_phase_distinguishes_executing_tool_from_thinking() {
        let mut running_with_tool = AcpSessionState::default();
        running_with_tool.phase = AcpPhase::Running;
        running_with_tool.entries.push(AcpEntry::ToolCall {
            id: "t1".into(),
            title: "Read".into(),
            kind: ToolKind::Read,
            status: ToolCallStatus::InProgress,
            output: Vec::new(),
        });
        assert_eq!(compute_acp_daemon_phase(&running_with_tool), Phase::ExecutingTool);

        let mut running_no_tool = AcpSessionState::default();
        running_no_tool.phase = AcpPhase::Running;
        assert_eq!(compute_acp_daemon_phase(&running_no_tool), Phase::Thinking);

        let mut ended = AcpSessionState::default();
        ended.phase = AcpPhase::Ended("boom".into());
        assert_eq!(compute_acp_daemon_phase(&ended), Phase::Dead);
    }

    #[test]
    fn pending_question_prefers_permission_over_elicitation() {
        use smelt_core::acp_session::LivePermission;

        let mut s = AcpSessionState::default();
        assert_eq!(acp_pending_question(&s), None);

        s.permission = Some(LivePermission {
            question: "要不要覆盖这个文件？".into(),
            tool_call_id: "t1".into(),
            options: Vec::new(),
            responder: None,
            raw_request_line: None,
        });
        assert_eq!(acp_pending_question(&s).as_deref(), Some("要不要覆盖这个文件？"));
    }
}
