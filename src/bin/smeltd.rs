//! smeltd —— 终端持久化守护进程（tmux 的最小替身）。
//!
//! 所有 shell / PTY 活在这里而非 GUI 进程里：GUI 退出、崩溃，会话照常运行；
//! 重开 GUI 按会话 id 重连（attach），守护重放输出缓冲恢复画面。
//!
//! 协议（Unix socket ~/.smelt/smeltd.sock）——连接后客户端先发一行 JSON：
//!   {"op":"open","id":"..","cwd":"..","cols":120,"rows":30}  → 进入流模式
//!   {"op":"list"}                                            → 回 {"sessions":[..]} 后关闭
//!   {"op":"kill","id":".."}                                  → 回 {"ok":true} 后关闭
//!   {"op":"version"}                                         → 回 {"version":"..","exe_mtime":123} 后关闭
//!   {"op":"shutdown"}                                        → 回 {"ok":true} 后进程退出（杀掉所有会话！）
//!   {"op":"upgrade"}                                         → 回 {"ok":true} 后 exec 磁盘上的新二进制，
//!                                                              PTY fd 原地交接，**所有会话不中断**（见下）
//!
//! 流模式：
//!   守护 → 客户端：原始 PTY 输出字节（attach 先重放缓冲，再实时转发）
//!   客户端 → 守护：帧 [type:u8][len:u32 BE][payload]
//!     type 0 = 键盘输入字节；type 1 = resize（payload 8 字节：cols u32 BE + rows u32 BE）
//! shell 退出 → 守护关闭该连接（客户端读到 EOF）。
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

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::Duration;

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

/// 每会话输出回放缓冲上限。从中部截断可能留半截转义序列，alacritty 解析器可容错跳过。
const BUF_CAP: usize = 2 * 1024 * 1024;

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

/// 按行列 resize PTY（TIOCSWINSZ 直接打在 master fd 上）。
fn resize_fd(fd: RawFd, rows: u16, cols: u16) {
    let ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
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

/// 会话输出端：回放缓冲 + 当前 attach 的客户端。「重放→接管」与实时转发共用这把锁，
/// 严格串行，重连时不会出现新输出插到重放内容前面的乱序。
struct Out {
    buf: Vec<u8>,
    client: Option<UnixStream>,
}

struct Session {
    ctl: Mutex<Ctl>,
    out: Mutex<Out>,
}

type Sessions = Arc<Mutex<HashMap<String, Arc<Session>>>>;

fn main() {
    // 无缝升级交接：上一代进程 exec 本二进制前写好交接文件并把路径放在环境变量里。
    // 立即摘掉环境变量：它只对"本次 exec 交接"有意义，不能传染给之后 spawn 的 shell。
    let handoff = std::env::var("SMELTD_HANDOFF").ok();
    std::env::remove_var("SMELTD_HANDOFF");
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
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let sessions = Arc::clone(&sessions);
        thread::spawn(move || handle_conn(conn, sessions, exe_mtime, listen_fd));
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
        let sess = Arc::new(Session {
            ctl: Mutex::new(Ctl {
                master,
                pid,
                // 交接后 GUI 会重连，首个 resize 抖动出 SIGWINCH 让 TUI 全屏重绘，
                // 顺带盖掉交接窗口内可能没进重放缓冲的零星输出。
                jolt: true,
                cols: item["cols"].as_u64().unwrap_or(80) as u16,
                rows: item["rows"].as_u64().unwrap_or(24) as u16,
            }),
            out: Mutex::new(Out { buf, client: None }),
        });
        sessions.lock().unwrap().insert(id.to_string(), Arc::clone(&sess));
        start_pty_pump(sess, Box::new(reader), id.to_string(), Arc::clone(&sessions));
    }
    Some((listener, sessions))
}

/// 重放缓冲的交接编码：hex 简单无依赖，2MB 上限的缓冲编成 4MB 文本，一次性开销可接受。
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

fn handle_conn(conn: UnixStream, sessions: Sessions, exe_mtime: u64, listen_fd: RawFd) {
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
                if let Some(c) = s.out.lock().unwrap().client.take() {
                    let _ = c.shutdown(Shutdown::Both);
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

    // attach：回报 PTY 当前尺寸 → 重放缓冲 → 接管转发（尺寸行 + 重放在同一锁内先于
    // 实时转发，客户端先按正确宽度建终端再解析重放字节，行宽才对得上）。
    let (cur_cols, cur_rows) = {
        let ctl = sess.ctl.lock().unwrap();
        (ctl.cols, ctl.rows)
    };
    let attached_fd = {
        let Ok(mut c) = conn.try_clone() else { return };
        let fd = c.as_raw_fd();
        // 写超时：客户端冻结（GUI 被挂起/调试暂停）时，泵线程/这里的初始重放不能
        // 无限期占着下面这把 out 锁——handle_upgrade 快照时也要挨个拿它，泵线程
        // 一旦永久攥着，会把整个 upgrade 拖成全局死锁（见 CLIENT_WRITE_TIMEOUT）。
        let _ = c.set_write_timeout(Some(CLIENT_WRITE_TIMEOUT));
        let mut out = sess.out.lock().unwrap();
        if let Some(old) = out.client.take() {
            let _ = old.shutdown(Shutdown::Both); // 顶掉旧连接（同 id 只允许一个 GUI）
        }
        // replay_len 告诉客户端接下来这段字节是重放的历史，不是刚发生的：客户端拿它
        // 划一条边界，重放范围内扫到的 OSC 9/777 通知（可能是几天前就已经处理过的
        // 权限确认之类）不会被当成新事件重新弹出来，见 terminal.rs::spawn 里的用法。
        let replay_len = out.buf.len();
        if writeln!(
            c,
            "{}",
            serde_json::json!({ "cols": cur_cols, "rows": cur_rows, "replay_len": replay_len })
        )
        .is_err()
        {
            return;
        }
        if replay_len > 0 && c.write_all(&out.buf).is_err() {
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
            1 if len == 8 => {
                let cols = u32::from_be_bytes(payload[0..4].try_into().unwrap()) as u16;
                let rows = u32::from_be_bytes(payload[4..8].try_into().unwrap()) as u16;
                let mut ctl = sess.ctl.lock().unwrap();
                let fd = ctl.master.as_raw_fd();
                if ctl.jolt {
                    ctl.jolt = false;
                    resize_fd(fd, rows.saturating_add(1), cols);
                }
                resize_fd(fd, rows, cols);
                ctl.cols = cols;
                ctl.rows = rows;
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
        ctl: Mutex::new(Ctl { master, pid, jolt: false, cols, rows }),
        out: Mutex::new(Out { buf: Vec::new(), client: None }),
    };
    Ok((sess, Box::new(pty_reader)))
}

/// PTY 输出泵线程：读 PTY → 追加回放缓冲（截断到上限）→ 转发当前客户端。
/// shell 退出（EOF）：移除会话、断开客户端、收割子进程。
fn start_pty_pump(
    sess: Arc<Session>,
    mut pty_reader: Box<dyn Read + Send>,
    id: String,
    sessions: Sessions,
) {
    thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match pty_reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut out = sess.out.lock().unwrap();
                    out.buf.extend_from_slice(&buf[..n]);
                    if out.buf.len() > BUF_CAP {
                        let cut = out.buf.len() - BUF_CAP;
                        out.buf.drain(..cut);
                    }
                    if let Some(c) = out.client.as_mut() {
                        if c.write_all(&buf[..n]).is_err() {
                            out.client = None; // 客户端已断，会话继续养着
                        }
                    }
                }
            }
        }
        sessions.lock().unwrap().remove(&id);
        let mut out = sess.out.lock().unwrap();
        if let Some(c) = out.client.take() {
            let _ = c.shutdown(Shutdown::Both); // GUI 读到 EOF 即知 shell 退出
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
