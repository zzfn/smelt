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
//! 仍保留一小段原始字节缓冲，仅供无缝 upgrade 交接后尽量重建 Term 状态。
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
//!   {"op":"remote_start","bind":"..","port":0}               → 回 {"ok":true,"token":"..","addr":".."}，
//!                                                              见下「内嵌远程网关」（bind/port 可省，默认回环随机口）
//!   {"op":"remote_stop"}                                     → 回 {"ok":true} 后关闭
//!   {"op":"remote_status"}                                   → 回 {"running":bool,"token":"..","addr":".."} 后关闭
//!
//! 流模式：
//!   守护 → 客户端：先发 JSON 尺寸行（含 replay_len=快照字节数）→ ANSI 网格快照
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
//! 2. 逐会话拿 ctl/out 锁做快照（master fd / shell pid / 尺寸 / 重放缓冲）——out 锁
//!    在 handle_open 里配了写超时（CLIENT_WRITE_TIMEOUT），泵线程不会无限期攥着；
//! 3. 给 master fd 和监听 socket fd 清掉 CLOEXEC，快照写入交接文件（fd 号 + 元数据，
//!    0600 权限——里面是全部会话的回放缓冲明文）；
//! 4. 回 {"ok":true} 后 `exec()` 磁盘上的 smeltd（同路径新内容），带 SMELTD_HANDOFF 环境变量；
//! 5. 新进程启动时发现交接文件：from_raw_fd 认领监听 socket 和各会话的 master fd，
//!    重建会话表并重启泵线程（jolt=true，GUI 重连后首个 resize 触发 SIGWINCH 全屏重绘）。
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

#[path = "../remote_gateway.rs"]
mod remote_gateway;

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::Duration;

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config as TermConfig, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Processor};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};

/// 每会话原始字节缓冲上限（**仅**供 upgrade 交接后尽量重建 Term，不再作为 attach 主路径）。
/// 主路径是常驻 Term 的网格快照。容量小于旧 2MB，降低交接文件体积。
const BUF_CAP: usize = 256 * 1024;

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

/// 会话输出端：原始字节旁路缓冲 + 当前 attach 的客户端。
/// 「快照→接管」与实时转发共用这把锁，严格串行。
struct Out {
    /// upgrade 交接用；attach 主路径已改走网格快照。
    buf: Vec<u8>,
    client: Option<UnixStream>,
    /// `watch` 连接：只读旁观，不参与 client 的顶替逻辑，可多个并存。
    /// 复用 `out` 这把已有的锁（不新增锁），锁序与顶替逻辑因此天然保持一致。
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

fn new_daemon_term(rows: u16, cols: u16) -> Term<VoidListener> {
    let size = DaemonTermSize {
        rows: rows.max(1) as usize,
        cols: cols.max(1) as usize,
    };
    Term::new(daemon_term_config(), &size, VoidListener)
}

struct Session {
    ctl: Mutex<Ctl>,
    out: Mutex<Out>,
    /// 常驻网格：PTY 输出持续 advance；attach 时序列化成 ANSI 快照。
    term: Mutex<Term<VoidListener>>,
}

type Sessions = Arc<Mutex<HashMap<String, Arc<Session>>>>;

/// 内嵌远程网关开着时的状态：token、绑定地址、喊停用的信号。见文件头「内嵌远程
/// 网关」一节——这条不参与无缝升级交接，`upgrade` 后新进程里永远是 None。
struct RemoteGateway {
    token: String,
    addr: std::net::SocketAddr,
    shutdown_tx: tokio::sync::oneshot::Sender<()>,
}

type RemoteState = Arc<Mutex<Option<RemoteGateway>>>;

/// 幂等：已经开着直接回现有 token/addr，不重启、不换 token。
/// bind 非法 / 端口绑不上都走 Err，调用方原样透传给客户端。
fn start_remote_gateway(state: &RemoteState, bind: &str, port: u16) -> Result<(String, std::net::SocketAddr), String> {
    let mut guard = state.lock().unwrap();
    if let Some(g) = guard.as_ref() {
        return Ok((g.token.clone(), g.addr));
    }

    let ip: std::net::IpAddr = bind.parse().map_err(|e| format!("非法绑定地址 {bind}：{e}"))?;
    let std_listener = std::net::TcpListener::bind((ip, port))
        .map_err(|e| format!("绑定 {bind}:{port} 失败：{e}"))?;
    std_listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let addr = std_listener.local_addr().map_err(|e| e.to_string())?;

    let token = uuid::Uuid::new_v4().simple().to_string();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    let token_for_thread = token.clone();
    thread::spawn(move || {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("远程网关起不了 tokio runtime：{e}");
                return;
            }
        };
        rt.block_on(async move {
            let listener = match tokio::net::TcpListener::from_std(std_listener) {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("远程网关认领监听 fd 失败：{e}");
                    return;
                }
            };
            let app = remote_gateway::build_router(token_for_thread);
            let serve = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                });
            if let Err(e) = serve.await {
                eprintln!("远程网关退出：{e}");
            }
        });
    });

    *guard = Some(RemoteGateway { token: token.clone(), addr, shutdown_tx });
    Ok((token, addr))
}

fn stop_remote_gateway(state: &RemoteState) {
    if let Some(g) = state.lock().unwrap().take() {
        let _ = g.shutdown_tx.send(());
    }
}

fn main() {
    // 无缝升级交接：上一代进程 exec 本二进制前写好交接文件并把路径放在环境变量里。
    // 立即摘掉环境变量：它只对"本次 exec 交接"有意义，不能传染给之后 spawn 的 shell。
    let handoff = std::env::var("SMELTD_HANDOFF").ok();
    // Edition 2024：`remove_var` 标为 unsafe（多线程改 env 非同步）。
    // 此处在 main 最开头、尚未 spawn 任何线程，单线程访问安全。
    unsafe { std::env::remove_var("SMELTD_HANDOFF") };
    let came_from_handoff = handoff.is_some();

    let path = sock_path();
    let (listener, sessions) = match handoff.and_then(|p| resume_handoff(&p)) {
        Some(x) => x,
        None => {
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
            let Ok(listener) = UnixListener::bind(&path) else { return };
            // socket 仅本用户可读写。
            let _ = std::fs::set_permissions(
                &path,
                std::os::unix::fs::PermissionsExt::from_mode(0o600),
            );
            (listener, Arc::new(Mutex::new(HashMap::new())))
        }
    };

    let listen_fd = listener.as_raw_fd();
    let exe_mtime = exe_mtime_secs();
    // 不参与无缝升级交接：每次进程启动（含 upgrade 后的新进程）都是全新的 None，
    // 见 RemoteGateway 定义处注释。
    let remote_state: RemoteState = Arc::new(Mutex::new(None));
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let sessions = Arc::clone(&sessions);
        let remote_state = Arc::clone(&remote_state);
        thread::spawn(move || handle_conn(conn, sessions, exe_mtime, listen_fd, remote_state));
    }
}

/// 交接文件路径（跟 socket 同目录）。
fn handoff_path() -> std::path::PathBuf {
    sock_path().with_file_name("handoff.json")
}

/// 从交接文件恢复：认领监听 socket 和各会话的 PTY master fd，重建会话表 + 泵线程。
/// 任何全局性错误（文件读不到/解析失败/监听 fd 无效）返回 None 走全新启动——会话
/// 保不住但守护必须活着；单个会话的 fd 坏了只跳过那一个。
fn resume_handoff(path: &str) -> Option<(UnixListener, Sessions)> {
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
        let buf = item["buf"].as_str().and_then(hex_decode).unwrap_or_default();
        let cols = item["cols"].as_u64().unwrap_or(80) as u16;
        let rows = item["rows"].as_u64().unwrap_or(24) as u16;
        // 尽量用交接带来的字节重建 Term（可能腰斩，仅 best-effort）；jolt 会让 TUI 再刷。
        let mut term = new_daemon_term(rows, cols);
        if !buf.is_empty() {
            feed_term(&mut term, &buf);
        }
        let sess = Arc::new(Session {
            ctl: Mutex::new(Ctl {
                master,
                pid,
                // 交接后 GUI 会重连，首个 resize 抖动出 SIGWINCH 让 TUI 全屏重绘，
                // 顺带盖掉交接窗口内可能没进缓冲/Term 的零星输出。
                jolt: true,
                cols,
                rows,
            }),
            out: Mutex::new(Out {
                buf,
                client: None,
                watchers: Vec::new(),
            }),
            term: Mutex::new(term),
        });
        sessions.lock().unwrap().insert(id.to_string(), Arc::clone(&sess));
        start_pty_pump(sess, Box::new(reader), id.to_string(), Arc::clone(&sessions));
    }
    Some((listener, sessions))
}

/// 把字节喂进常驻 Term；panic 时吞掉，避免畸形序列拖死整个守护。
fn feed_term(term: &mut Term<VoidListener>, bytes: &[u8]) {
    let mut parser: Processor = Processor::new();
    let _ = catch_unwind(AssertUnwindSafe(|| {
        parser.advance(term, bytes);
    }));
}

/// 重放缓冲的交接编码：hex 简单无依赖，BUF_CAP 上限的缓冲编成 2× 文本，一次性开销可接受。
fn hex_encode(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for byte in b {
        s.push_str(&format!("{byte:02x}"));
    }
    s
}

/// 按字节而非按 `str` 下标解码——交接文件是外部数据（可能损坏/被篡改），`buf` 字段
/// 一旦混入非 ASCII 内容，`&s[i..i+2]` 这种字符串切片会在非字符边界 panic（此函数跑
/// 在 resume_handoff/主线程，此时上一代进程已经 exec 没了，panic 就是全会话陪葬）。
/// 全程只做字节级 match，不触碰任何字符串切片 API，天然不可能因编码问题 panic。
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

#[cfg(test)]
mod handoff_tests {
    use super::*;

    /// 重放缓冲跨交接必须逐字节还原——含 0x00/0xff/转义序列这类非文本字节。
    #[test]
    fn hex_roundtrip() {
        let data: Vec<u8> = (0..=255u8).cycle().take(4096).collect();
        assert_eq!(hex_decode(&hex_encode(&data)).as_deref(), Some(data.as_slice()));
        assert_eq!(hex_decode("").as_deref(), Some(&[][..]));
        assert_eq!(hex_decode("abc"), None, "奇数长度应判非法");
        assert_eq!(hex_decode("zz"), None, "非 hex 字符应判非法");
    }

    /// 交接文件被损坏/篡改后 buf 字段混入多字节 UTF-8 字符（如中文，恰好偶数字节，
    /// 能通过 len%2 检查）：曾经按 &s[i..i+2] 字符串切片会在非字符边界 panic，
    /// 此时上一代进程已 exec 没了，panic 即全会话陪葬。必须只判非法、绝不 panic。
    #[test]
    fn hex_decode_never_panics_on_multibyte_utf8() {
        assert_eq!(hex_decode("中文"), None); // 6 字节，偶数，非 hex 字符
        assert_eq!(hex_decode("a中"), None); // 1 + 3 字节，奇偶交叉
        assert_eq!(hex_decode("ab中c"), None);
    }
}

fn handle_conn(
    conn: UnixStream,
    sessions: Sessions,
    exe_mtime: u64,
    listen_fd: RawFd,
    remote_state: RemoteState,
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
        Some("open") => handle_open(conn, reader, &v, sessions),
        Some("watch") => handle_watch(conn, reader, &v, sessions),
        Some("list") => {
            let ids: Vec<String> = sessions.lock().unwrap().keys().cloned().collect();
            let mut c = conn;
            let _ = writeln!(c, "{}", serde_json::json!({ "sessions": ids }));
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
        Some("upgrade") => handle_upgrade(conn, &sessions, listen_fd),
        Some("version") => {
            let mut c = conn;
            let _ = writeln!(
                c,
                "{}",
                serde_json::json!({ "version": env!("CARGO_PKG_VERSION"), "exe_mtime": exe_mtime })
            );
        }
        Some("shutdown") => {
            let mut c = conn;
            let _ = writeln!(c, "{}", serde_json::json!({ "ok": true }));
            let _ = c.shutdown(Shutdown::Both);
            // 直接退出：PTY 全归子进程持有，本进程一死子进程读端 EOF/SIGHUP，
            // 所有会话随之终止 —— 这是「重启守护」明知故犯的代价，调用方必须先提示用户。
            std::process::exit(0);
        }
        Some("remote_start") => {
            let bind = v["bind"].as_str().unwrap_or("127.0.0.1").to_string();
            let port = v["port"].as_u64().unwrap_or(0) as u16;
            let mut c = conn;
            match start_remote_gateway(&remote_state, &bind, port) {
                Ok((token, addr)) => {
                    let _ = writeln!(
                        c,
                        "{}",
                        serde_json::json!({ "ok": true, "token": token, "addr": addr.to_string() })
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
                Some(g) => {
                    serde_json::json!({ "running": true, "token": g.token, "addr": g.addr.to_string() })
                }
                None => serde_json::json!({ "running": false }),
            };
            let _ = writeln!(c, "{}", body);
        }
        _ => {}
    }
}

fn handle_open(
    conn: UnixStream,
    mut reader: BufReader<UnixStream>,
    v: &serde_json::Value,
    sessions: Sessions,
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
    let sess = match existing {
        Some(s) => {
            // reattach：下个 resize 抖动触发 SIGWINCH 重绘。
            s.ctl.lock().unwrap().jolt = true;
            s
        }
        None => {
            let Ok((sess, pty_reader)) = spawn_session(rows, cols, cwd.as_deref(), launch.as_deref())
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
    let attached_fd = {
        let Ok(mut c) = conn.try_clone() else { return };
        let fd = c.as_raw_fd();
        // 写超时：客户端冻结时不能无限期占着 out 锁（见 CLIENT_WRITE_TIMEOUT）。
        let _ = c.set_write_timeout(Some(CLIENT_WRITE_TIMEOUT));

        let (snapshot, mut out) = {
            let term = sess.term.lock().unwrap();
            let out = sess.out.lock().unwrap();
            let snapshot = snapshot_ansi(&term);
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
                // 常驻 Term 与 PTY 同步行列，否则快照宽高和真实壳不一致。
                if let Ok(mut term) = sess.term.lock() {
                    term.resize(DaemonTermSize {
                        rows: rows.max(1) as usize,
                        cols: cols.max(1) as usize,
                    });
                }
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
        let snapshot = snapshot_ansi(&term);
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

/// 无缝升级：快照会话表 → 写交接文件 → exec 磁盘上的新二进制（流程见文件头注释）。
///
/// 锁策略：只短暂持 sessions 锁拿一份 Arc 列表就放掉——不像早期版本那样一直攥到
/// exec，那样会让 open/list/kill/version 在升级期间全部卡在 sessions 锁上。逐会话
/// 再去拿 out 锁时，靠 handle_open 里给客户端 socket 设的 CLIENT_WRITE_TIMEOUT
/// 兜底：就算某个客户端冻结导致泵线程握着 out 锁在 write_all 里卡住，最多卡
/// CLIENT_WRITE_TIMEOUT 那么久也会因写超时放手，不会无限期挂死。
/// （极小残余窗口：某泵线程恰好已 read 出 ≤8KB 还没拿到锁，这部分随 exec 丢失。
/// 丢的只是"显示字节"不是输入；重连后的 jolt 全屏重绘会盖掉，可接受。）
fn handle_upgrade(conn: UnixStream, sessions: &Sessions, listen_fd: RawFd) {
    let mut c = conn;
    let Ok(exe) = std::env::current_exe() else {
        let _ = writeln!(c, "{}", serde_json::json!({ "ok": false, "err": "current_exe 失败" }));
        return;
    };

    let session_list: Vec<(String, Arc<Session>)> =
        sessions.lock().unwrap().iter().map(|(k, v)| (k.clone(), Arc::clone(v))).collect();

    // 挡住并发 spawn：跟 spawn_session 共用 SPAWN_GATE，独占锁一直拿到 exec（或本函数
    // 提前失败返回）为止，防止清 CLOEXEC 的窗口里恰好 fork 出新 shell，把这些 fd
    // 也带过去（见 SPAWN_GATE 定义处注释）。
    let _spawn_gate = SPAWN_GATE.write().unwrap();

    let mut out_guards = Vec::new(); // 持有到 exec，挡住泵线程
    let mut items = Vec::new();
    let mut fds = vec![listen_fd];
    for (id, sess) in &session_list {
        let ctl = sess.ctl.lock().unwrap();
        let out = sess.out.lock().unwrap();
        let fd = ctl.master.as_raw_fd();
        items.push(serde_json::json!({
            "id": id,
            "fd": fd,
            "pid": ctl.pid,
            "cols": ctl.cols,
            "rows": ctl.rows,
            "buf": hex_encode(&out.buf),
        }));
        fds.push(fd);
        drop(ctl);
        out_guards.push(out);
    }

    // 交接的 fd 全部清 CLOEXEC，让它们活过 exec。
    for &fd in &fds {
        set_cloexec(fd, false);
    }
    let payload =
        serde_json::json!({ "listen_fd": listen_fd, "sessions": items }).to_string();
    let hp = handoff_path();
    if std::fs::write(&hp, payload).is_err() {
        for &fd in &fds {
            set_cloexec(fd, true);
        }
        let _ = writeln!(c, "{}", serde_json::json!({ "ok": false, "err": "写交接文件失败" }));
        return;
    }
    // 含全部会话的回放缓冲明文（可能有粘贴过的密钥/token），跟 socket 一样仅本用户可读写；
    // 正常路径下 resume_handoff 读到就删，这里只是缩小落盘期间的暴露窗口。
    let _ =
        std::fs::set_permissions(&hp, std::os::unix::fs::PermissionsExt::from_mode(0o600));

    // 先回执再 exec：客户端连接是 CLOEXEC 的，exec 后立即断开，回执必须赶在前面。
    // exec 失败的情况客户端会看到 ok:true 但轮询版本发现没变，按"升级未生效"处理。
    let _ = writeln!(c, "{}", serde_json::json!({ "ok": true }));

    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(&exe).env("SMELTD_HANDOFF", &hp).exec();

    // 走到这里说明 exec 失败（新二进制没法执行）：回滚，守护继续用旧版本服务。
    let _ = std::fs::remove_file(&hp);
    for &fd in &fds {
        set_cloexec(fd, true);
    }
    eprintln!("smeltd 无缝升级 exec 失败: {err}");
}

/// 开 PTY + 起 shell（环境设置与 GUI 内嵌版完全一致，见 workspace/terminal.rs 的注释）。
/// `launch`：项目「+」悬浮菜单的 Claude Code / Codex 快捷入口——把要跑的命令直接编进
/// 启动命令行（`-c '<launch>; exec <shell> -l'`），而不是等 shell 起来后再补发按键。
/// 这样从根上没有"shell 是否已经在读 stdin"的时序问题，命令跑完会 exec 回一个
/// 正常交互 login shell，之后就是一个普通会话。
fn spawn_session(
    rows: u16,
    cols: u16,
    cwd: Option<&str>,
    launch: Option<&str>,
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
    let sess = Session {
        ctl: Mutex::new(Ctl {
            master,
            pid,
            jolt: false,
            cols,
            rows,
        }),
        out: Mutex::new(Out {
            buf: Vec::new(),
            client: None,
            watchers: Vec::new(),
        }),
        term: Mutex::new(new_daemon_term(rows, cols)),
    };
    Ok((sess, Box::new(pty_reader)))
}

/// PTY 输出泵：读 PTY → advance 常驻 Term → 旁路缓冲 → 转发当前客户端。
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
                    out.buf.extend_from_slice(chunk);
                    if out.buf.len() > BUF_CAP {
                        let cut = out.buf.len() - BUF_CAP;
                        out.buf.drain(..cut);
                    }
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

/// 当前 SGR 状态（只发变化，压缩快照体积）。
#[derive(Clone, Copy, PartialEq, Eq)]
struct SgrState {
    fg: Color,
    bg: Color,
    bold: bool,
    dim: bool,
    italic: bool,
    /// 0=无，1=单线，2=双线，3=波浪，4=点，5=虚线
    underline: u8,
    strike: bool,
    inverse: bool,
    /// OSC 8 当前挂着的 URI（None = 未开链接）。
    link: Option<u64>, // 用指针地址当身份，避免克隆字符串比相等
}

impl Default for SgrState {
    fn default() -> Self {
        Self {
            fg: Color::Named(NamedColor::Foreground),
            bg: Color::Named(NamedColor::Background),
            bold: false,
            dim: false,
            italic: false,
            underline: 0,
            strike: false,
            inverse: false,
            link: None,
        }
    }
}

/// 把常驻 Term 编成自洽 ANSI 快照：
/// - **主缓冲**：有限 scrollback（`SNAPSHOT_MAX_LINES`）+ 可视区，客户端可上滚看历史
/// - **备用屏**：整屏重画（TUI 场景）
/// - 恢复光标形状/可见性、常用私有模式（bracketed paste / app cursor / 鼠标）
///
/// 不依赖可能被环形缓冲腰斩的原始字节流。
fn snapshot_ansi(term: &Term<VoidListener>) -> Vec<u8> {
    let grid = term.grid();
    let cols = term.columns().max(1);
    let screen_lines = term.screen_lines().max(1);
    let mode = *term.mode();

    let top = term.topmost_line();
    let bottom = term.bottommost_line();
    // 从 bottom 往上最多 SNAPSHOT_MAX_LINES 行
    let span = (bottom.0 - top.0 + 1).max(0) as usize;
    let start = if span > SNAPSHOT_MAX_LINES {
        Line(bottom.0 - SNAPSHOT_MAX_LINES as i32 + 1)
    } else {
        top
    };

    let lines_to_emit = (bottom.0 - start.0 + 1).max(0) as usize;
    let mut out = Vec::with_capacity(lines_to_emit.saturating_mul(cols).saturating_mul(4));

    // —— 缓冲 / 私有模式前缀 ——
    if mode.contains(TermMode::ALT_SCREEN) {
        out.extend_from_slice(b"\x1b[?1049h");
    }
    // 清屏 + 清客户端旧 scrollback，再由我们的 history 重新灌入
    out.extend_from_slice(b"\x1b[H\x1b[2J\x1b[3J\x1b[0m");

    // 自动换行：默认开着，显式对齐
    if mode.contains(TermMode::LINE_WRAP) {
        out.extend_from_slice(b"\x1b[?7h");
    } else {
        out.extend_from_slice(b"\x1b[?7l");
    }

    let mut sgr = SgrState::default();
    let mut line = start;
    while line <= bottom {
        let row = &grid[line];
        let wrap = row[Column(cols - 1)].flags.contains(Flags::WRAPLINE);

        for col in 0..cols {
            let cell = &row[Column(col)];
            let flags = cell.flags;

            if flags.contains(Flags::WIDE_CHAR_SPACER)
                || flags.contains(Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }

            // OSC 8 超链接开/关（按 URI 变）
            let link_id = cell.hyperlink().map(|h| {
                // 用 uri 指针稳定比较：同一 hyperlink Arc 地址相同
                h.uri().as_ptr() as u64 ^ (h.uri().len() as u64)
            });
            if link_id != sgr.link {
                // 先关旧
                if sgr.link.is_some() {
                    out.extend_from_slice(b"\x1b]8;;\x1b\\");
                }
                if let Some(h) = cell.hyperlink() {
                    out.extend_from_slice(b"\x1b]8;;");
                    out.extend_from_slice(h.uri().as_bytes());
                    out.extend_from_slice(b"\x1b\\");
                }
                sgr.link = link_id;
            }

            append_sgr(&mut out, &mut sgr, cell);

            if !flags.contains(Flags::HIDDEN) {
                let ch = cell.c;
                if ch != '\0' {
                    push_char(&mut out, ch);
                }
                if let Some(zw) = cell.zerowidth() {
                    for &z in zw {
                        push_char(&mut out, z);
                    }
                }
            }
        }

        // 行尾：软换行不插 \r\n（下一行是续行）；硬换行才换行。
        // 最后一行后也不要多余 \r\n，靠后面的 CUP 定位光标。
        if line < bottom && !wrap {
            // 换行前关 OSC 8，避免链接跨行粘连
            if sgr.link.is_some() {
                out.extend_from_slice(b"\x1b]8;;\x1b\\");
                sgr.link = None;
            }
            out.extend_from_slice(b"\r\n");
        }
        line += 1;
    }

    // 关闭可能仍开着的 OSC 8
    if sgr.link.is_some() {
        out.extend_from_slice(b"\x1b]8;;\x1b\\");
    }
    out.extend_from_slice(b"\x1b[0m");

    // —— 光标：吐完 history 后客户端应在底部；CUP 用可视区 1 基坐标 ——
    // alacritty：display_offset=0 时 cursor.line 为 0..screen_lines-1。
    let cursor = grid.cursor.point;
    let viewport_row = if cursor.line.0 >= 0 {
        cursor.line.0 as usize
    } else {
        0
    };
    let viewport_row = viewport_row.min(screen_lines.saturating_sub(1));
    let viewport_col = cursor.column.0.min(cols.saturating_sub(1));
    let _ = write!(
        out,
        "\x1b[{};{}H",
        viewport_row + 1,
        viewport_col + 1
    );

    // 光标形状（DECSCUSR）+ 显隐
    let content = term.renderable_content();
    match content.cursor.shape {
        CursorShape::Hidden => out.extend_from_slice(b"\x1b[?25l"),
        CursorShape::Underline => out.extend_from_slice(b"\x1b[4 q\x1b[?25h"),
        CursorShape::Beam => out.extend_from_slice(b"\x1b[6 q\x1b[?25h"),
        CursorShape::HollowBlock => out.extend_from_slice(b"\x1b[0 q\x1b[?25h"),
        CursorShape::Block => out.extend_from_slice(b"\x1b[2 q\x1b[?25h"),
    }

    // —— 常用模式：TUI 在 jolt 重绘前也能收到正确的输入约定 ——
    append_mode_restores(&mut out, mode);

    out
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

fn append_sgr(out: &mut Vec<u8>, state: &mut SgrState, cell: &Cell) {
    let flags = cell.flags;
    let bold = flags.contains(Flags::BOLD) || flags.contains(Flags::BOLD_ITALIC);
    let dim = flags.contains(Flags::DIM) || flags.contains(Flags::DIM_BOLD);
    let italic = flags.contains(Flags::ITALIC) || flags.contains(Flags::BOLD_ITALIC);
    let underline = underline_kind(flags);
    let strike = flags.contains(Flags::STRIKEOUT);
    let inverse = flags.contains(Flags::INVERSE);

    let next_attrs = (bold, dim, italic, underline, strike, inverse, cell.fg, cell.bg);
    let cur_attrs = (
        state.bold,
        state.dim,
        state.italic,
        state.underline,
        state.strike,
        state.inverse,
        state.fg,
        state.bg,
    );
    if next_attrs == cur_attrs {
        return;
    }

    let need_reset = state.bold && !bold
        || state.dim && !dim
        || state.italic && !italic
        || state.underline != 0 && underline == 0
        || state.underline != 0 && underline != 0 && state.underline != underline
        || state.strike && !strike
        || state.inverse && !inverse
        || (state.fg != cell.fg && is_default_fg(cell.fg))
        || (state.bg != cell.bg && is_default_bg(cell.bg));

    let mut params = Vec::new();
    let push_code = |params: &mut Vec<u8>, code: u8| {
        if !params.is_empty() {
            params.push(b';');
        }
        push_u8(params, code);
    };

    if need_reset {
        params.push(b'0');
        state.bold = false;
        state.dim = false;
        state.italic = false;
        state.underline = 0;
        state.strike = false;
        state.inverse = false;
        state.fg = Color::Named(NamedColor::Foreground);
        state.bg = Color::Named(NamedColor::Background);
    }

    if bold && !state.bold {
        push_code(&mut params, 1);
    }
    if dim && !state.dim {
        push_code(&mut params, 2);
    }
    if italic && !state.italic {
        push_code(&mut params, 3);
    }
    if underline != 0 && underline != state.underline {
        // SGR 4 / 4:2 / 4:3 / 4:4 / 4:5
        if !params.is_empty() {
            params.push(b';');
        }
        match underline {
            1 => params.extend_from_slice(b"4"),
            2 => params.extend_from_slice(b"4:2"),
            3 => params.extend_from_slice(b"4:3"),
            4 => params.extend_from_slice(b"4:4"),
            5 => params.extend_from_slice(b"4:5"),
            _ => params.extend_from_slice(b"4"),
        }
    }
    if inverse && !state.inverse {
        push_code(&mut params, 7);
    }
    if strike && !state.strike {
        push_code(&mut params, 9);
    }
    if cell.fg != state.fg {
        append_color_params(&mut params, true, cell.fg);
    }
    if cell.bg != state.bg {
        append_color_params(&mut params, false, cell.bg);
    }

    if !params.is_empty() {
        out.extend_from_slice(b"\x1b[");
        out.extend_from_slice(&params);
        out.push(b'm');
    }

    state.bold = bold;
    state.dim = dim;
    state.italic = italic;
    state.underline = underline;
    state.strike = strike;
    state.inverse = inverse;
    state.fg = cell.fg;
    state.bg = cell.bg;
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

fn append_color_params(params: &mut Vec<u8>, is_fg: bool, color: Color) {
    let sep = |params: &mut Vec<u8>| {
        if !params.is_empty() {
            params.push(b';');
        }
    };
    match color {
        Color::Named(n) => {
            sep(params);
            push_u8(params, named_sgr_code(n, is_fg));
        }
        Color::Indexed(i) => {
            sep(params);
            push_u8(params, if is_fg { 38 } else { 48 });
            params.push(b';');
            params.push(b'5');
            params.push(b';');
            push_u8(params, i);
        }
        Color::Spec(rgb) => {
            sep(params);
            push_u8(params, if is_fg { 38 } else { 48 });
            params.push(b';');
            params.push(b'2');
            params.push(b';');
            push_u8(params, rgb.r);
            params.push(b';');
            push_u8(params, rgb.g);
            params.push(b';');
            push_u8(params, rgb.b);
        }
    }
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

        let snap = snapshot_ansi(&a);

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

        let snap = snapshot_ansi(&term);
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
        let snap = snapshot_ansi(&term);
        assert!(snap.windows(8).any(|w| w == b"\x1b[?1049h"));
        assert!(snap.windows(3).any(|w| w == b"TUI"));
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
        let snap = snapshot_ansi(&term);
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
        let snap = snapshot_ansi(&term);
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
        let snap = snapshot_ansi(&term);
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
    fn make_dummy_session(rows: u16, cols: u16) -> Arc<Session> {
        let master = std::fs::OpenOptions::new().write(true).open("/dev/null").unwrap();
        let child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id() as i32;
        drop(child); // Child::drop 不 wait()，留成 zombie，交给 pump 收尾时的 waitpid

        Arc::new(Session {
            ctl: Mutex::new(Ctl { master, pid, jolt: false, cols, rows }),
            out: Mutex::new(Out { buf: Vec::new(), client: None, watchers: Vec::new() }),
            term: Mutex::new(new_daemon_term(rows, cols)),
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
        thread::spawn(move || {
            let reader = BufReader::new(open_server.try_clone().unwrap());
            handle_open(open_server, reader, &serde_json::json!({"id":"t","cols":80,"rows":24}), sessions_a);
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
}
