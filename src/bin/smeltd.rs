//! smeltd —— 终端持久化守护进程（tmux 的最小替身）。
//!
//! 所有 shell / PTY 活在这里而非 GUI 进程里：GUI 退出、崩溃，会话照常运行；
//! 重开 GUI 按会话 id 重连（attach），守护重放输出缓冲恢复画面。
//!
//! 协议（Unix socket ~/.smelt/smeltd.sock）——连接后客户端先发一行 JSON：
//!   {"op":"open","id":"..","cwd":"..","cols":120,"rows":30}  → 进入流模式
//!   {"op":"list"}                                            → 回 {"sessions":[..]} 后关闭
//!   {"op":"kill","id":".."}                                  → 回 {"ok":true} 后关闭
//!
//! 流模式：
//!   守护 → 客户端：原始 PTY 输出字节（attach 先重放缓冲，再实时转发）
//!   客户端 → 守护：帧 [type:u8][len:u32 BE][payload]
//!     type 0 = 键盘输入字节；type 1 = resize（payload 8 字节：cols u32 BE + rows u32 BE）
//! shell 退出 → 守护关闭该连接（客户端读到 EOF）。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};
use std::thread;

use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};

/// 每会话输出回放缓冲上限。从中部截断可能留半截转义序列，alacritty 解析器可容错跳过。
const BUF_CAP: usize = 2 * 1024 * 1024;

fn sock_path() -> std::path::PathBuf {
    let dir = dirs::home_dir().unwrap_or_else(|| "/tmp".into()).join(".smelt");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("smeltd.sock")
}

/// 会话控制端：PTY 输入 / resize / 杀进程。
struct Ctl {
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    /// reattach 后首个 resize 强制「抖动」（先 rows+1 再回正）：即使尺寸与断开前相同也
    /// 制造 SIGWINCH，让备用屏 TUI（Claude Code 等）重绘整屏，避免重连花屏。
    jolt: bool,
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
    let path = sock_path();
    // 单实例：能连上说明已有活守护，直接退出；连不上则清掉残留 socket 文件再 bind。
    if UnixStream::connect(&path).is_ok() {
        return;
    }
    let _ = std::fs::remove_file(&path);
    let Ok(listener) = UnixListener::bind(&path) else { return };
    // socket 仅本用户可读写。
    let _ = std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600));

    let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let sessions = Arc::clone(&sessions);
        thread::spawn(move || handle_conn(conn, sessions));
    }
}

fn handle_conn(conn: UnixStream, sessions: Sessions) {
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
                let _ = s.ctl.lock().unwrap().child.kill();
                if let Some(c) = s.out.lock().unwrap().client.take() {
                    let _ = c.shutdown(Shutdown::Both);
                }
            }
            let mut c = conn;
            let _ = writeln!(c, "{}", serde_json::json!({ "ok": true }));
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

    // 取既有会话（reattach）或新建。
    let existing = sessions.lock().unwrap().get(&id).cloned();
    let sess = match existing {
        Some(s) => {
            // reattach：下个 resize 抖动触发 SIGWINCH 重绘。
            s.ctl.lock().unwrap().jolt = true;
            s
        }
        None => {
            let Ok((sess, pty_reader)) = spawn_session(rows, cols, cwd.as_deref()) else {
                return;
            };
            let sess = Arc::new(sess);
            sessions.lock().unwrap().insert(id.clone(), Arc::clone(&sess));
            start_pty_pump(Arc::clone(&sess), pty_reader, id.clone(), Arc::clone(&sessions));
            sess
        }
    };

    // attach：重放缓冲 + 接管转发（同一锁内，保证与实时输出串行无乱序）。
    let attached_fd = {
        let Ok(mut c) = conn.try_clone() else { return };
        let fd = c.as_raw_fd();
        let mut out = sess.out.lock().unwrap();
        if let Some(old) = out.client.take() {
            let _ = old.shutdown(Shutdown::Both); // 顶掉旧连接（同 id 只允许一个 GUI）
        }
        if !out.buf.is_empty() && c.write_all(&out.buf).is_err() {
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
                let mut ctl = sess.ctl.lock().unwrap();
                let _ = ctl.writer.write_all(&payload);
                let _ = ctl.writer.flush();
            }
            1 if len == 8 => {
                let cols = u32::from_be_bytes(payload[0..4].try_into().unwrap()) as u16;
                let rows = u32::from_be_bytes(payload[4..8].try_into().unwrap()) as u16;
                let size = PtySize { rows, cols, pixel_width: 0, pixel_height: 0 };
                let mut ctl = sess.ctl.lock().unwrap();
                if ctl.jolt {
                    ctl.jolt = false;
                    let _ = ctl.master.resize(PtySize { rows: rows.saturating_add(1), ..size });
                }
                let _ = ctl.master.resize(size);
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

/// 开 PTY + 起 shell（环境设置与 GUI 内嵌版完全一致，见 workspace/terminal.rs 的注释）。
fn spawn_session(
    rows: u16,
    cols: u16,
    cwd: Option<&str>,
) -> anyhow::Result<(Session, Box<dyn Read + Send>)> {
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })?;

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
    let mut cmd = CommandBuilder::new(shell);
    // login shell：拿完整 PATH（.app 双击启动时系统 PATH 很精简）。
    cmd.arg("-l");
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
    let child = pair.slave.spawn_command(cmd)?;

    let pty_reader = pair.master.try_clone_reader()?;
    let writer = pair.master.take_writer()?;
    let sess = Session {
        ctl: Mutex::new(Ctl { writer, master: pair.master, child, jolt: false }),
        out: Mutex::new(Out { buf: Vec::new(), client: None }),
    };
    Ok((sess, pty_reader))
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
        let _ = sess.ctl.lock().unwrap().child.wait(); // 收尸避免僵尸进程
    });
}
