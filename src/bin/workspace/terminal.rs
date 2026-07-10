//! 内嵌终端前端：连接 smeltd 守护进程拿字节流 + alacritty_terminal 做终端状态机。
//!
//! PTY 与 shell 活在 smeltd 里（GUI 退出不杀会话，重开按 id 重连并重放恢复画面，
//! 类 tmux；协议见 src/bin/smeltd.rs 头注释）。数据流：后台线程读守护 socket →
//! vte 解析器 advance → 更新共享的 Term 网格；UI 线程定时对网格做快照并重绘。

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermDamage, TermMode};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor, Rgb};

/// 深浅色模式：进程内只有一套主题（设置页全局切换），用一个原子量足够，不必给
/// 每个 Terminal/EventProxy 各传一份——见 `set_dark_mode`（main.rs 在
/// Appearance.theme_mode 变化时同步调用）与下面 `default_fg`/`default_bg`/`palette`。
static DARK_MODE: AtomicBool = AtomicBool::new(true);

/// 切换终端配色跟随的深浅色模式。
pub fn set_dark_mode(dark: bool) {
    DARK_MODE.store(dark, Ordering::Relaxed);
}

pub fn is_dark() -> bool {
    DARK_MODE.load(Ordering::Relaxed)
}

/// 默认前景 / 背景色：深色取 iTerm2 风格灰白正文，浅色取近白底 + 深灰正文。
const DEFAULT_FG_DARK: u32 = 0x00d8_d8d8;
const DEFAULT_BG_DARK: u32 = 0x001a_1b26;
const DEFAULT_FG_LIGHT: u32 = 0x0024_292e;
const DEFAULT_BG_LIGHT: u32 = 0x00f6_f8fa;

pub fn default_fg() -> u32 {
    if is_dark() { DEFAULT_FG_DARK } else { DEFAULT_FG_LIGHT }
}

pub fn default_bg() -> u32 {
    if is_dark() { DEFAULT_BG_DARK } else { DEFAULT_BG_LIGHT }
}

/// 16 色 ANSI 调色板：深色沿用 Tokyo Night（白/亮白改为灰白/纯白，iTerm2 风格）；
/// 浅色是同色相压深/加饱和的对应版本，保证在浅底上仍有足够对比度。
const PALETTE_DARK: [u32; 16] = [
    0x0015_161e, 0x00f7_768e, 0x009e_ce6a, 0x00e0_af68, 0x007a_a2f7, 0x00bb_9af7, 0x007d_cfff,
    0x00c7_c7c7, 0x002c_3149, 0x00f7_768e, 0x009e_ce6a, 0x00e0_af68, 0x007a_a2f7, 0x00bb_9af7,
    0x007d_cfff, 0x00ff_ffff,
];
const PALETTE_LIGHT: [u32; 16] = [
    0x0024_283b, 0x00c0_324a, 0x004e_8a2f, 0x00a1_690f, 0x0037_60bf, 0x0078_47bd, 0x000f_7b9e,
    0x004a_4a4a, 0x006b_7089, 0x00d7_495f, 0x005f_ae3f, 0x00c4_8511, 0x002e_6fe0, 0x0091_61d9,
    0x0010_93c2, 0x001a_1b26,
];

fn palette() -> &'static [u32; 16] {
    if is_dark() { &PALETTE_DARK } else { &PALETTE_LIGHT }
}

/// 一个渲染用的终端单元：字符 + 前景/背景 rgb + 粗体/下划线。
pub struct Cell {
    pub ch: char,
    pub fg: u32,
    pub bg: u32,
    pub bold: bool,
    pub underline: bool,
}

/// 一帧终端快照：网格行 + 光标位置（行, 列）。cursor 为 None 表示已上滚离开可视区。
pub struct Frame {
    pub rows: Vec<Vec<Cell>>,
    pub cursor: Option<(usize, usize)>,
}

/// 是否东方全角文字（等宽字体里字形精确占两格）：CJK / 假名 / 谚文 / 全角符号等。
/// 用于决定宽字符占位格跳过（这些）还是保留空格（emoji / 其它符号）。
fn is_wide_cjk(c: char) -> bool {
    matches!(
        c as u32,
        0x1100..=0x115F      // 谚文字母
        | 0x2E80..=0x303E    // CJK 部首 / 康熙 / 符号
        | 0x3041..=0x33FF    // 假名 / 注音 / CJK 兼容
        | 0x3400..=0x4DBF    // CJK 扩展 A
        | 0x4E00..=0x9FFF    // CJK 统一表意
        | 0xA000..=0xA4CF    // 彝文
        | 0xAC00..=0xD7A3    // 谚文音节
        | 0xF900..=0xFAFF    // CJK 兼容表意
        | 0xFE30..=0xFE4F    // CJK 兼容形式
        | 0xFF00..=0xFF60    // 全角 ASCII
        | 0xFFE0..=0xFFE6    // 全角符号
        | 0x2_0000..=0x3_FFFD // CJK 扩展 B+
    )
}

/// 把 alacritty 的 Color 解析成 0xRRGGBB。is_fg 决定「默认色」取前景还是背景。
fn resolve(color: Color, is_fg: bool) -> u32 {
    match color {
        Color::Spec(rgb) => ((rgb.r as u32) << 16) | ((rgb.g as u32) << 8) | rgb.b as u32,
        Color::Indexed(i) => indexed_rgb(i),
        Color::Named(n) => named_rgb(n, is_fg),
    }
}

fn named_rgb(n: NamedColor, is_fg: bool) -> u32 {
    use NamedColor::*;
    let p = palette();
    match n {
        Black => p[0],
        Red => p[1],
        Green => p[2],
        Yellow => p[3],
        Blue => p[4],
        Magenta => p[5],
        Cyan => p[6],
        White => p[7],
        BrightBlack => p[8],
        BrightRed => p[9],
        BrightGreen => p[10],
        BrightYellow => p[11],
        BrightBlue => p[12],
        BrightMagenta => p[13],
        BrightCyan => p[14],
        BrightWhite => p[15],
        Background => default_bg(),
        // Foreground / Cursor / Dim* / 未来新增变体统一回落到默认色
        _ => {
            if is_fg {
                default_fg()
            } else {
                default_bg()
            }
        }
    }
}

/// xterm 256 色索引 → rgb：0-15 用调色板，16-231 为 6×6×6 色立方，232-255 为灰阶。
fn indexed_rgb(i: u8) -> u32 {
    match i {
        0..=15 => palette()[i as usize],
        16..=231 => {
            let i = i - 16;
            let step = |v: u8| -> u32 {
                if v == 0 {
                    0
                } else {
                    55 + v as u32 * 40
                }
            };
            (step(i / 36) << 16) | (step((i % 36) / 6) << 8) | step(i % 6)
        }
        232..=255 => {
            let v = 8 + (i as u32 - 232) * 10;
            (v << 16) | (v << 8) | v
        }
    }
}

/// 终端尺寸，实现 alacritty 的 Dimensions（先固定行列，resize 留到下一步）。
#[derive(Clone, Copy)]
pub struct TermSize {
    pub rows: usize,
    pub cols: usize,
}

impl Dimensions for TermSize {
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

/// 通知消息的共享槽：EventProxy（响铃 Bell）与 reader 线程（OSC 9/777）都往这写，
/// UI 侧轮询 take_notification 取走，用作「agent 需要注意」提示。
type NotifySlot = Arc<Mutex<Option<String>>>;

/// 事件代理：alacritty 的 EventListener。终端响铃 Event::Bell → 写入一条默认通知；
/// PtyWrite / ColorRequest → 把 alacritty 算好的回应字节写回 PTY（见 write_pty）；
/// 其余事件仍忽略（重绘走 UI 定时快照）。
#[derive(Clone)]
struct EventProxy {
    notify: NotifySlot,
    /// 终端标题（OSC 0/2）——Claude Code 用它实时报告「在干嘛」（任务名 + 状态符号）。
    title: Arc<Mutex<Option<String>>>,
    /// 守护连接写端，跟 [`Terminal`] 自己发键盘输入共用同一把锁——两边都是往同一个
    /// socket 写帧，混着写会把帧头/帧长/payload 交叉打乱，必须靠这把锁串行。
    writer: Arc<Mutex<UnixStream>>,
}

impl EventProxy {
    /// 把响应字节当作「PTY 输入」帧写回守护——对 shell/CLI 来说，终端主动应答的
    /// 查询（光标位置、颜色）和用户敲键盘没有区别，都是它 stdin 收到的字节。
    fn write_pty(&self, bytes: &[u8]) {
        if let Ok(mut w) = self.writer.lock() {
            write_frame(&mut w, 0, bytes);
        }
    }

    /// alacritty 自己不记「当前实际渲染色」，查询颜色时要由我们把 RGB 值喂回去。
    /// smelt 没有运行时改色的路径（无 OSC 4/10/11 set-color 场景），直接用当前主题的
    /// 默认前景 / 背景 / 16 色板作答，覆盖 CLI 常见的「查一下背景色决定用什么灰」——
    /// 取的是 `palette()`/`default_fg`/`default_bg`，跟着 `set_dark_mode` 一起切换。
    fn resolve_color(index: usize) -> Rgb {
        let to_rgb = |hex: u32| Rgb {
            r: ((hex >> 16) & 0xff) as u8,
            g: ((hex >> 8) & 0xff) as u8,
            b: (hex & 0xff) as u8,
        };
        let p = palette();
        if index < p.len() {
            to_rgb(p[index])
        } else if index == NamedColor::Background as usize {
            to_rgb(default_bg())
        } else {
            to_rgb(default_fg())
        }
    }
}

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        match event {
            Event::Bell => {
                if let Ok(mut g) = self.notify.lock() {
                    *g = Some("🔔 响铃".to_string());
                }
            }
            Event::Title(t) => {
                if let Ok(mut g) = self.title.lock() {
                    *g = Some(t);
                }
            }
            // 光标位置 / 设备属性等查询-应答协议：不回应会让依赖精确光标位置渲染
            // 的 TUI（如 Claude Code 的输入框 ghost-text 补全）拿不到定位信息。
            Event::PtyWrite(text) => self.write_pty(text.as_bytes()),
            Event::ColorRequest(index, format) => {
                self.write_pty(format(Self::resolve_color(index)).as_bytes())
            }
            _ => {}
        }
    }
}

/// OSC 9 / 777 通知扫描：alacritty 不解析这两个序列，我们在 reader 线程自己扫字节流
/// 提取 `ESC ] 9 ; 消息 (BEL|ST)`（跟 cmux 同协议），跨 read 边界保持状态。
#[derive(Default)]
struct OscScan {
    prev_esc: bool,
    in_osc: bool,
    buf: Vec<u8>,
}

impl OscScan {
    fn feed(&mut self, b: u8, notify: &Mutex<Option<String>>) {
        if self.in_osc {
            if b == 0x07 {
                self.finish(notify); // BEL 结束
            } else if self.prev_esc && b == 0x5c {
                self.buf.pop(); // 去掉刚推入的 ESC，ST（ESC \）结束
                self.finish(notify);
            } else {
                self.buf.push(b);
                self.prev_esc = b == 0x1b;
                if self.buf.len() > 4096 {
                    self.reset(); // 异常超长，丢弃
                }
            }
        } else if self.prev_esc && b == 0x5d {
            self.in_osc = true; // ESC ] 进入 OSC
            self.buf.clear();
            self.prev_esc = false;
        } else {
            self.prev_esc = b == 0x1b;
        }
    }

    fn finish(&mut self, notify: &Mutex<Option<String>>) {
        if let Ok(s) = std::str::from_utf8(&self.buf) {
            if let Some((ps, pt)) = s.split_once(';') {
                if ps == "9" || ps == "777" {
                    // OSC 777 常见格式 `777;notify;title;body`，取最后一段作正文。
                    let msg = pt.rsplit(';').next().unwrap_or(pt).trim().to_string();
                    if !msg.is_empty() {
                        if let Ok(mut g) = notify.lock() {
                            *g = Some(msg);
                        }
                    }
                }
            }
        }
        self.reset();
    }

    fn reset(&mut self) {
        self.in_osc = false;
        self.prev_esc = false;
        self.buf.clear();
    }
}

// ===================== smeltd 守护连接层 =====================

fn sock_path() -> std::path::PathBuf {
    let dir = dirs::home_dir().unwrap_or_else(|| "/tmp".into()).join(".smelt");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("smeltd.sock")
}

/// 连接守护；连不上就拉起同目录的 smeltd（独立进程组，GUI / 终端退出都不波及）再重试。
fn connect_daemon() -> std::io::Result<UnixStream> {
    let path = sock_path();
    if let Ok(s) = UnixStream::connect(&path) {
        return Ok(s);
    }
    let exe = std::env::current_exe()?;
    let daemon = exe.with_file_name("smeltd");
    {
        use std::os::unix::process::CommandExt;
        use std::process::Stdio;
        let _ = std::process::Command::new(&daemon)
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
    // 守护 bind 很快，通常首轮就连上；给足 5s 兜底。
    for _ in 0..50 {
        thread::sleep(Duration::from_millis(100));
        if let Ok(s) = UnixStream::connect(&path) {
            return Ok(s);
        }
    }
    Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "smeltd 未就绪"))
}

/// 探测正在跑的守护：连不上（守护没起，`connect_daemon` 会自己拉起磁盘上最新的）
/// 判 `NotRunning`；连上了但读不出合法的 "version" 响应——老到连这个探测本身都不
/// 认识——判 `Unresponsive`，这种必然过期；否则拿到对方自报的可执行文件 mtime
/// （见 smeltd.rs::exe_mtime_secs）。
enum DaemonProbe {
    NotRunning,
    Unresponsive,
    ExeMtime(u64),
}

fn probe_daemon() -> DaemonProbe {
    let Ok(mut s) = UnixStream::connect(sock_path()) else {
        return DaemonProbe::NotRunning;
    };
    let mtime = (|| -> Option<u64> {
        writeln!(s, "{}", serde_json::json!({ "op": "version" })).ok()?;
        let mut resp = String::new();
        BufReader::new(s).read_line(&mut resp).ok()?;
        let v: serde_json::Value = serde_json::from_str(&resp).ok()?;
        v["exe_mtime"].as_u64()
    })();
    match mtime {
        Some(m) => DaemonProbe::ExeMtime(m),
        None => DaemonProbe::Unresponsive,
    }
}

/// 磁盘上 smeltd 二进制（GUI 同目录）的当前 mtime（秒）。
fn disk_smeltd_mtime() -> Option<u64> {
    let exe = std::env::current_exe().ok()?;
    let daemon = exe.with_file_name("smeltd");
    let modified = std::fs::metadata(daemon).ok()?.modified().ok()?;
    modified.duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_secs())
}

/// 守护是否落后于磁盘上的 smeltd 二进制（重装/重编译后常见：旧守护不会自动重启，
/// 新代码要等手动重启守护才生效）。守护没起 → false（没什么可重启的，交给
/// `connect_daemon` 按需拉起最新的）；守护活着但连 "version" op 都不认识 → true
/// （老到必然过期）；查得到 mtime 但磁盘那份查不到 → false（避免误报打扰用户）。
pub fn daemon_outdated() -> bool {
    match probe_daemon() {
        DaemonProbe::NotRunning => false,
        DaemonProbe::Unresponsive => true,
        DaemonProbe::ExeMtime(running) => disk_smeltd_mtime().is_some_and(|disk| disk > running),
    }
}

/// 无缝升级的结果。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum UpgradeOutcome {
    /// 交接完成，会话全部保留；或守护本来没跑、直接拉起了最新版。
    Upgraded,
    /// 正在跑的守护太旧，不认识 "upgrade" op（静默断连），只能走硬重启。
    Unsupported,
    /// 守护接了单但升级没生效（exec 失败等），版本还是旧的。
    Failed,
}

/// 无缝升级守护：发 "upgrade" op，守护 exec 磁盘上的新二进制、PTY fd 原地交接，
/// **所有会话不中断**（协议与流程见 smeltd.rs 头注释）。调用方在成功后应对每个
/// 终端调 reconnect()——会话 id 都还在，走的是正常 reattach + 重放恢复。
///
/// `read_line` 前设了读超时：守护万一卡住（比如某个 out 锁被冻结客户端占住），不能
/// 让这次调用永久挂起——上层 `daemon_upgrading` 标志会跟着卡死，整个功能失效。
pub fn upgrade_daemon() -> UpgradeOutcome {
    let Ok(mut s) = UnixStream::connect(sock_path()) else {
        // 守护没跑：拉起磁盘上最新的等于升级完成，但要探测确认它真的起来了再报
        // 成功——ensure_daemon_running 的失败是静默的，不确认就报 Upgraded 会让
        // UI 显示"已升级"而守护其实没起来。
        ensure_daemon_running();
        return if matches!(probe_daemon(), DaemonProbe::ExeMtime(_)) {
            UpgradeOutcome::Upgraded
        } else {
            UpgradeOutcome::Failed
        };
    };
    if writeln!(s, "{}", serde_json::json!({ "op": "upgrade" })).is_err() {
        return UpgradeOutcome::Failed;
    }
    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
    // 三种读结果分开判断，不能混为一谈：
    // - Ok(0)（EOF，没读到任何字节）＝老守护完全不认识这个 op，直接断连 → Unsupported；
    // - Err（超时/IO 错误）＝守护接了但迟迟不回（可能卡住），不代表版本问题 → Failed；
    // - Ok(n>0) 但解析不出 JSON，或解析出来 ok!=true（比如 current_exe/写交接文件
    //   失败的显式回执）＝守护是新版本、只是这次没成功，同样是 Failed，不能引导
    //   用户去做「版本过旧只能硬重启」这种更破坏性的操作。
    let mut resp = String::new();
    match BufReader::new(s).read_line(&mut resp) {
        Ok(0) => return UpgradeOutcome::Unsupported,
        Ok(_) => {}
        Err(_) => return UpgradeOutcome::Failed,
    }
    let acked = serde_json::from_str::<serde_json::Value>(resp.trim())
        .is_ok_and(|v| v["ok"].as_bool() == Some(true));
    if !acked {
        return UpgradeOutcome::Failed;
    }
    // exec + 交接在百毫秒量级；轮询到新进程的 exe_mtime 追平磁盘为止。
    let disk = disk_smeltd_mtime();
    for _ in 0..25 {
        thread::sleep(Duration::from_millis(200));
        if let DaemonProbe::ExeMtime(running) = probe_daemon() {
            if disk.is_none_or(|d| running >= d) {
                return UpgradeOutcome::Upgraded;
            }
        }
    }
    UpgradeOutcome::Failed
}

/// 让守护自己退出。**会杀掉它托管的所有 PTY 会话**（进程一死子进程 EOF/SIGHUP），
/// 调用前必须先让用户明确知情确认。退出后调 ensure_daemon_running() 拉起磁盘上
/// 最新的 smeltd 二进制。
///
/// "shutdown" op 本身也是新加的——老到连它都不认识的守护会照单全收地忽略这条消息，
/// 连接照旧开着，优雅关闭形同没发生。等一小段时间探测它是否真的死了，没死就按
/// 监听 socket 的进程直接 SIGKILL，这条路径不依赖守护认不认识任何协议。
pub fn restart_daemon() {
    let path = sock_path();
    if let Ok(mut s) = UnixStream::connect(&path) {
        let _ = writeln!(s, "{}", serde_json::json!({ "op": "shutdown" }));
        let mut resp = String::new();
        let _ = BufReader::new(s).read_line(&mut resp);
    }
    for _ in 0..10 {
        if UnixStream::connect(&path).is_err() {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
    force_kill_socket_owner(&path);
}

/// 兜底：找到正监听着 `path` 的进程并 SIGKILL，再清掉残留 socket 文件。用于优雅
/// shutdown 对老守护不生效的情况——`lsof -t` 直接按 socket 文件反查 pid，不经过
/// 应用层协议，多老的守护都杀得掉。
fn force_kill_socket_owner(path: &std::path::Path) {
    let Ok(out) = std::process::Command::new("lsof").arg("-t").arg(path).output() else {
        return;
    };
    for pid in String::from_utf8_lossy(&out.stdout).split_whitespace() {
        let _ = std::process::Command::new("kill").arg("-9").arg(pid).status();
    }
    let _ = std::fs::remove_file(path);
}

/// 确保守护活着，没有就拉起来（复用 connect_daemon 的探测+拉起+轮询逻辑）。
/// 调用方（重启守护后想立刻刷新状态）负责扔到后台线程，避免卡 UI（最坏等 5s）。
pub fn ensure_daemon_running() {
    let _ = connect_daemon();
}

/// 让守护杀掉某会话（用户主动关 pane 时调用；GUI 退出不调 → 会话持久活着）。
pub fn kill_remote(id: &str) {
    let Ok(mut s) = UnixStream::connect(sock_path()) else { return };
    let _ = writeln!(s, "{}", serde_json::json!({ "op": "kill", "id": id }));
    // 等守护回执，确保 kill 落地后再继续（避免关 pane 后立刻退出时丢命令）。
    let mut resp = String::new();
    let _ = BufReader::new(s).read_line(&mut resp);
}

/// 客户端 → 守护的帧：[type:u8][len:u32 BE][payload]。type 0=输入，1=resize。
fn write_frame(w: &mut UnixStream, ty: u8, payload: &[u8]) {
    let mut frame = Vec::with_capacity(5 + payload.len());
    frame.push(ty);
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(payload);
    let _ = w.write_all(&frame);
}

/// 一个内嵌终端：alacritty 的 Term（后台线程写、UI 线程读）+ 守护连接写端。
pub struct Terminal {
    term: Arc<Mutex<Term<EventProxy>>>,
    /// 写端加锁共享给 EventProxy（见其字段注释）：键盘输入和终端自动应答都从这
    /// 发出，必须串行，不能各拿一个裸 fd 各写各的。
    writer: Arc<Mutex<UnixStream>>,
    size: TermSize,
    /// 通知消息槽（响铃 / OSC 9 写入，UI 轮询 take_notification 取走）。
    notify: NotifySlot,
    /// 终端标题（agent 实时状态；UI 读 current_title 用于通知 / 总览）。
    title: Arc<Mutex<Option<String>>>,
}

/// 新建/reattach 握手失败时的重试次数与间隔：守护无缝升级 exec 交接的一次性抖动是
/// 百毫秒到 1 秒量级，这个预算（5 次 × 300ms ≈ 1.2s，含首次尝试共 5 次）足够盖过去。
const HANDSHAKE_RETRIES: u32 = 5;
const HANDSHAKE_RETRY_DELAY: Duration = Duration::from_millis(300);

impl Terminal {
    /// 打开（或重连）守护里 id 对应的会话：shell 环境由 smeltd 负责（-l / TERM /
    /// iTerm2 伪装 / LANG 兜底，见 smeltd.rs）。id 已存在 → attach，守护先重放输出
    /// 缓冲恢复画面，再实时转发。`launch`：新建会话时要先跑的命令（编进 shell 启动
    /// 命令行，见 smeltd.rs::spawn_session），只在新建时生效，reattach 会被忽略。
    pub fn spawn(
        rows: usize,
        cols: usize,
        cwd: Option<&str>,
        id: &str,
        launch: Option<&str>,
    ) -> anyhow::Result<Self> {
        // 1) 连守护（不在则自动拉起）并声明要打开的会话，握手失败带几次短重试。
        //
        // 守护无缝升级 exec 交接期间，恰好在这一瞬间新开的 pane 可能撞上这个连接
        // 被接受、但握手线程卡在守护内部的 SPAWN_GATE（跟 upgrade 互斥，见 smeltd.rs）
        // 上——exec 一发生，这条连接（普通客户端 fd 默认带 CLOEXEC）就被无声关闭，
        // 我们这边会读到 EOF/解析失败。整个交接是百毫秒到 1 秒量级的一次性抖动，
        // 短重试几次基本能把这个窗口盖掉，调用方不必为这种瞬时性错误崩溃整个 GUI
        // （调用方目前对失败仍是 `.expect()`，见 terminal_view.rs 的注释）。
        let (buffered, size, replay_len) = {
            let mut last_err = None;
            let mut result = None;
            for attempt in 0..HANDSHAKE_RETRIES {
                if attempt > 0 {
                    thread::sleep(HANDSHAKE_RETRY_DELAY);
                }
                match Self::handshake(rows, cols, cwd, id, launch) {
                    Ok(x) => {
                        result = Some(x);
                        break;
                    }
                    Err(e) => last_err = Some(e),
                }
            }
            match result {
                Some(x) => x,
                None => return Err(last_err.unwrap_or_else(|| anyhow::anyhow!("握手失败"))),
            }
        };
        let writer = buffered.get_ref().try_clone()?;

        // 2) alacritty 终端状态机（EventProxy 把响铃 / 标题写入共享槽，把 PTY 自动
        //    应答写回下面这个共享写端）
        let notify: NotifySlot = Arc::new(Mutex::new(None));
        let title: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let writer = Arc::new(Mutex::new(writer));
        let term = Term::new(
            Config::default(),
            &size,
            EventProxy { notify: notify.clone(), title: title.clone(), writer: writer.clone() },
        );
        let term = Arc::new(Mutex::new(term));

        // 3) 后台读线程：守护转发的 PTY 字节 → vte 解析更新 Term 网格 + 扫 OSC 9/777。
        //    EOF = shell 退出或守护离线（网格冻结，重开会话即恢复）。
        //    复用尺寸行的 BufReader：重放字节可能已在其内部缓冲里。
        let mut reader = buffered;
        let term_reader = Arc::clone(&term);
        let notify_reader = notify.clone();
        thread::spawn(move || {
            // Processor<T = StdSyncHandler>：默认类型参数不参与 ::new() 推断，需显式标注。
            let mut parser: Processor = Processor::new();
            let mut osc = OscScan::default();
            let mut buf = [0u8; 4096];
            let mut bytes_seen: usize = 0;
            // 重放缓冲里的历史字节可能藏着早就处理完的 OSC 9/777 通知（比如 Claude
            // 之前问过的权限确认，用户当时已经批准、任务也跑完了）——reattach 时如果
            // 原样喂给通知扫描，会把它们当成刚发生的事件重新弹出来，把明明已完成的
            // 会话错误标红（"重开 app 状态变红"那个 bug）。sink 接住落在 replay_len
            // 范围内关闭的 OSC 序列，只有真正在重放边界之后关闭的才写进 notify_reader；
            // 每个字节仍然逐一喂给 osc（状态机不断流），只是根据这个字节的绝对位置
            // 决定它触发的通知该进哪个槽，边界处不会解析错位。
            let sink: Mutex<Option<String>> = Mutex::new(None);
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break, // EOF：shell 退出
                    Ok(n) => {
                        // OSC 9/777 通知：alacritty 不解析，自己扫字节提取
                        for (i, &b) in buf[..n].iter().enumerate() {
                            let target =
                                if bytes_seen + i < replay_len { &sink } else { &notify_reader };
                            osc.feed(b, target);
                        }
                        bytes_seen += n;
                        if let Ok(mut term) = term_reader.lock() {
                            parser.advance(&mut *term, &buf[..n]);
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(Self {
            term,
            writer,
            size,
            notify,
            title,
        })
    }

    /// 一次性握手：连守护 + 声明会话 + 读首行尺寸 + 重放字节数，不重试（重试策略在
    /// `spawn` 里）。replay_len 是 reattach 时守护即将吐给我们的历史字节数（新建
    /// 会话是 0），供 spawn() 的读线程划一条"重放 / 实时"边界，见那边的用法。
    fn handshake(
        rows: usize,
        cols: usize,
        cwd: Option<&str>,
        id: &str,
        launch: Option<&str>,
    ) -> anyhow::Result<(BufReader<UnixStream>, TermSize, usize)> {
        let mut writer = connect_daemon()?;
        writeln!(
            writer,
            "{}",
            serde_json::json!({ "op": "open", "id": id, "cwd": cwd, "cols": cols, "rows": rows, "launch": launch })
        )?;
        let mut buffered = BufReader::new(writer);
        let mut line = String::new();
        buffered.read_line(&mut line)?;
        let v: serde_json::Value = serde_json::from_str(line.trim())?;
        let size = TermSize {
            rows: v["rows"].as_u64().unwrap_or(rows as u64) as usize,
            cols: v["cols"].as_u64().unwrap_or(cols as u64) as usize,
        };
        let replay_len = v["replay_len"].as_u64().unwrap_or(0) as usize;
        Ok((buffered, size, replay_len))
    }

    /// 取走最新通知消息（读并清）：响铃或 OSC 9/777 上报的「需要注意」文本。
    pub fn take_notification(&self) -> Option<String> {
        self.notify.lock().ok().and_then(|mut g| g.take())
    }

    /// 当前终端标题（agent 报告的任务名 + 状态符号）；未设置返回 None。
    pub fn current_title(&self) -> Option<String> {
        self.title.lock().ok().and_then(|g| g.clone())
    }

    /// 自上次调用以来，终端网格内容是否真的变化了（读并清 alacritty 自带的
    /// damage tracking）。涵盖：PTY 写入的字符/颜色、光标移动、翻滚历史、
    /// 进出备用屏幕（vim/less 等全屏 TUI）、resize 等——这些都由 alacritty 自动
    /// 判定。**不**涵盖：用户拖选（Selection）、Cmd 悬停链接高亮——这两个是
    /// TerminalView 自己维护的 UI 状态，跟 Term 无关，各自的鼠标事件处理里已经
    /// 各自调用 cx.notify()，不依赖这里。
    ///
    /// 只应由每个 TerminalView 自己的定时刷新循环调用（每个 Terminal 独占一个
    /// Term，不会有多个消费者互相"偷"对方读到的脏区）。
    pub fn take_damage(&self) -> bool {
        let Ok(mut term) = self.term.lock() else {
            // 拿不到锁（锁中毒）：保守起见当作有变化，避免画面从此卡死不再刷新。
            return true;
        };
        // 光标当前 (行, 列)：alacritty 的 damage_cursor() 每次调用都会无条件把光标
        // 所在这一格标脏（哪怕光标压根没动），这是它假设"渲染方每帧都要重画光标做
        // 闪烁动画"的设计——实测验证过：完全空闲的终端 take_damage() 会一直返回
        // true，就是这个原因（不是猜的，写了隔离测试复现过）。smelt 没有光标闪烁
        // 动画，不能把这个无条件标记当成"内容变了"，得从判定里精确减掉：只有脏区
        // 范围比"仅光标那一格"更宽，才算数（光标真的移动了、或该格所在行其它字符
        // 也变了，都会让范围变宽；纯粹静止不动时范围恰好等于光标那一格）。
        let cursor = term.grid().cursor.point;
        let (cursor_line, cursor_col) = (cursor.line.0 as usize, cursor.column.0);
        let damaged = match term.damage() {
            TermDamage::Full => true,
            TermDamage::Partial(it) => it.into_iter().any(|l| {
                l.line != cursor_line || l.left != cursor_col || l.right != cursor_col
            }),
        };
        term.reset_damage();
        damaged
    }

    /// 按新行列 resize：同步 alacritty 网格，并发帧让守护 resize 底层 PTY。无变化则跳过。
    pub fn resize(&mut self, rows: usize, cols: usize) {
        if rows == self.size.rows && cols == self.size.cols {
            return;
        }
        if rows == 0 || cols == 0 {
            return;
        }
        self.size = TermSize { rows, cols };
        if let Ok(mut term) = self.term.lock() {
            term.resize(self.size);
        }
        let mut payload = [0u8; 8];
        payload[0..4].copy_from_slice(&(cols as u32).to_be_bytes());
        payload[4..8].copy_from_slice(&(rows as u32).to_be_bytes());
        if let Ok(mut w) = self.writer.lock() {
            write_frame(&mut w, 1, &payload);
        }
    }

    /// 向 shell 写入字节（键盘输入用）：帧转发给守护。
    pub fn send_input(&mut self, bytes: &[u8]) {
        if let Ok(mut w) = self.writer.lock() {
            write_frame(&mut w, 0, bytes);
        }
    }

    /// 是否处于「应用光标键」模式（DECCKM）。像 Claude Code 里那种上下选列表的全屏
    /// TUI，进入时会开这个模式，把方向键约定成 SS3（`ESC O A/B/C/D`）而非默认的
    /// CSI（`ESC [ A/B/C/D`）——发错一种应用收不到方向键，见 keystroke_to_bytes。
    pub fn app_cursor_mode(&self) -> bool {
        match self.term.lock() {
            Ok(term) => term.mode().contains(TermMode::APP_CURSOR),
            Err(_) => false,
        }
    }

    /// 快照当前可视网格 + 光标。用 renderable_content：尊重滚动偏移、带光标，
    /// 并处理反色（INVERSE）/粗体/下划线属性。
    pub fn snapshot(&self) -> Frame {
        let term = match self.term.lock() {
            Ok(t) => t,
            Err(_) => {
                return Frame {
                    rows: Vec::new(),
                    cursor: None,
                }
            }
        };
        let content = term.renderable_content();
        let cursor_pt = content.cursor.point;
        let display_offset = content.display_offset;

        let cols = self.size.cols;
        let mut rows: Vec<Vec<Cell>> = Vec::with_capacity(self.size.rows);
        let mut row: Vec<Cell> = Vec::with_capacity(cols);
        let mut count = 0usize;
        for indexed in content.display_iter {
            let cell = indexed.cell;
            let flags = cell.flags;
            let mut fg = resolve(cell.fg, true);
            let mut bg = resolve(cell.bg, false);
            if flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            // 宽字符占两格：第二格是 WIDE_CHAR_SPACER 占位，怎么处理取决于前一个宽字符：
            // - 东方全角文字（CJK / 假名 / 谚文 / 全角）：等宽字体里字形精确占两格，
            //   占位格用 '\0' 跳过，否则多画一格空格 → 中文字距过大。
            // - emoji / 符号：走系统彩色字体 fallback，字形宽度不足两格，占位格保留
            //   空格补齐，否则少半格导致光标 / prompt 错位。
            let ch = if flags.contains(Flags::WIDE_CHAR_SPACER) {
                let prev = row.last().map(|c| c.ch).unwrap_or(' ');
                if is_wide_cjk(prev) {
                    '\0'
                } else {
                    ' '
                }
            } else {
                cell.c
            };
            row.push(Cell {
                ch,
                fg,
                bg,
                bold: flags.contains(Flags::BOLD),
                underline: flags.contains(Flags::UNDERLINE),
            });
            count += 1;
            if count % cols == 0 {
                rows.push(std::mem::take(&mut row));
            }
        }
        if !row.is_empty() {
            rows.push(row);
        }

        // 仅在未上滚（offset==0）且光标行在可视范围内时显示光标。
        let cursor = if display_offset == 0 {
            let r = cursor_pt.line.0;
            if r >= 0 && (r as usize) < rows.len() {
                Some((r as usize, cursor_pt.column.0))
            } else {
                None
            }
        } else {
            None
        };

        Frame { rows, cursor }
    }

    /// 上下滚动历史缓冲：正数向上翻看历史，负数向下。（Shift+PageUp 用，强制本地历史。）
    pub fn scroll(&mut self, lines: i32) {
        if let Ok(mut term) = self.term.lock() {
            term.scroll_display(Scroll::Delta(lines));
        }
    }

    /// 滚回底部：真实终端的通行做法——手滑滚了一下历史后忘了滚回去，键盘一敲就该
    /// 跟手回到最新输出，不然新内容（比如 Claude Code 退出时打的那行提示）默默追加
    /// 到当前视野之外，用户会误以为「没打印」。
    pub fn scroll_to_bottom(&mut self) {
        if let Ok(mut term) = self.term.lock() {
            term.scroll_display(Scroll::Bottom);
        }
    }

    /// 滚轮：按终端当前模式分流，`lines` 正数向上、负数向下，`(row,col)` 为 0 基单元格。
    ///
    /// - 应用开了鼠标上报（MOUSE_MODE）→ 把滚轮编码成鼠标滚轮事件发给应用。**Claude Code
    ///   等 TUI 就是靠这个滚动**（它们在备用屏、无本地 scrollback，等的是鼠标事件）。
    /// - 备用屏 + 备用滚动（ALT_SCREEN+ALTERNATE_SCROLL）→ 发方向键。
    /// - 否则（普通主屏）→ 滚本地历史缓冲。
    pub fn scroll_wheel(&mut self, lines: i32, row: usize, col: usize) {
        let mode = match self.term.lock() {
            Ok(term) => *term.mode(),
            Err(_) => return,
        };
        let count = (lines.unsigned_abs() as usize).clamp(1, 8);
        let up = lines > 0;

        // intersects 而非 contains：MOUSE_MODE 是 REPORT_CLICK|MOTION|DRAG 的组合，很多 TUI
        // （如 Claude Code）只开其中一位（MOUSE_MOTION），contains 会漏判，intersects 才对。
        if mode.intersects(TermMode::MOUSE_MODE) {
            // 鼠标滚轮「按下」事件：上=64 下=65；坐标 1 基。
            let cb: u8 = if up { 64 } else { 65 };
            let cx = col.saturating_add(1);
            let cy = row.saturating_add(1);
            let mut buf = Vec::new();
            for _ in 0..count {
                if mode.contains(TermMode::SGR_MOUSE) {
                    buf.extend_from_slice(format!("\x1b[<{cb};{cx};{cy}M").as_bytes());
                } else {
                    // 普通 X10 编码：各值偏移 32，坐标裁到 223。
                    let bx = 32u8.saturating_add(cx.min(223) as u8);
                    let by = 32u8.saturating_add(cy.min(223) as u8);
                    buf.extend_from_slice(&[0x1b, b'[', b'M', 32 + cb, bx, by]);
                }
            }
            self.send_input(&buf);
        } else if mode.contains(TermMode::ALT_SCREEN)
            && mode.contains(TermMode::ALTERNATE_SCROLL)
        {
            // 备用屏无本地历史：发方向键（应用光标模式用 SS3，否则 CSI）。
            let seq: &[u8] = match (up, mode.contains(TermMode::APP_CURSOR)) {
                (true, true) => b"\x1bOA",
                (true, false) => b"\x1b[A",
                (false, true) => b"\x1bOB",
                (false, false) => b"\x1b[B",
            };
            let mut buf = Vec::with_capacity(seq.len() * count);
            for _ in 0..count {
                buf.extend_from_slice(seq);
            }
            self.send_input(&buf);
        } else if let Ok(mut term) = self.term.lock() {
            term.scroll_display(Scroll::Delta(lines));
        }
    }

    /// 鼠标左键按下/松开上报：应用开了鼠标上报（MOUSE_MODE）时才编码转发，
    /// 否则原样返回 false 交给调用方走本地框选。**Claude Code TUI 里可点击的条目
    /// （比如 fork agent 那一行）就是靠收到这个鼠标事件来响应点击的**——iTerm2 等
    /// 真终端本就会转发，我们之前只转发了滚轮，点击全被本地框选吃掉了。
    /// `pressed` true=按下、false=松开；`(row,col)` 0 基单元格。
    pub fn mouse_button(&mut self, pressed: bool, row: usize, col: usize) -> bool {
        let mode = match self.term.lock() {
            Ok(term) => *term.mode(),
            Err(_) => return false,
        };
        if !mode.intersects(TermMode::MOUSE_MODE) {
            return false;
        }
        let cx = col.saturating_add(1);
        let cy = row.saturating_add(1);
        let buf = if mode.contains(TermMode::SGR_MOUSE) {
            format!("\x1b[<0;{cx};{cy}{}", if pressed { 'M' } else { 'm' }).into_bytes()
        } else {
            // X10 编码：按下按钮码 0，松开固定用 3（不区分具体按了哪个键）。
            let cb: u8 = if pressed { 0 } else { 3 };
            let bx = 32u8.saturating_add(cx.min(223) as u8);
            let by = 32u8.saturating_add(cy.min(223) as u8);
            vec![0x1b, b'[', b'M', 32 + cb, bx, by]
        };
        self.send_input(&buf);
        true
    }
}

#[cfg(test)]
mod damage_gate_tests {
    use super::*;

    /// P0 性能修复的验证：真空闲时 take_damage() 应稳定为 false（跳过重画），
    /// 写入字节后应变 true（真实变化不会被吞掉）。用全新一次性 session id +
    /// 空临时目录，不碰任何真实/持久化会话。
    #[test]
    fn idle_then_input_toggles_damage() {
        let dir = std::env::temp_dir().join(format!("smelt-damage-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let id = format!("damage-test-{}", uuid_like());

        let mut term = Terminal::spawn(24, 80, dir.to_str(), &id, None).expect("spawn 失败");

        // 让 shell 起步、打印 prompt 稳定下来（这部分输出算真实变化，先排掉）。
        thread::sleep(Duration::from_millis(800));
        let _ = term.take_damage(); // 清掉启动阶段的输出

        // 真空闲：什么都不做，多次采样应稳定为 false。
        let mut idle_true_count = 0;
        for _ in 0..10 {
            thread::sleep(Duration::from_millis(100));
            if term.take_damage() {
                idle_true_count += 1;
            }
        }
        assert_eq!(idle_true_count, 0, "真空闲时 take_damage() 不该返回 true（次数={idle_true_count}）");

        // 写入真实字节：应该被判定为变化。
        term.send_input(b"echo hi\n");
        thread::sleep(Duration::from_millis(300));
        assert!(term.take_damage(), "写入字节后 take_damage() 应返回 true");

        // 清理：让守护杀掉这个一次性测试会话。
        kill_remote(&id);
    }

    /// 不依赖 uuid crate（terminal.rs 本身不需要它），用 pid+时间戳拼一个够唯一的 id。
    fn uuid_like() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        format!("{}-{nanos}", std::process::id())
    }
}

/// 验证 EventProxy 会把 PtyWrite / ColorRequest 这类「终端该怎么回应」的事件真的
/// 写回去，不用起真实 shell/PTY——直接喂原始转义序列给 alacritty 的 Processor，
/// 用 UnixStream::pair() 在另一头当"假守护"读回应帧即可，快且不 flaky。
#[cfg(test)]
mod event_proxy_answers_tests {
    use super::*;

    fn make_proxy() -> (EventProxy, UnixStream) {
        let (probe, sock) = UnixStream::pair().expect("pair 失败");
        probe.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        let proxy = EventProxy {
            notify: Arc::new(Mutex::new(None)),
            title: Arc::new(Mutex::new(None)),
            writer: Arc::new(Mutex::new(sock)),
        };
        (proxy, probe)
    }

    /// 读一帧 [type:u8][len:u32 BE][payload] 并返回 (type, payload 字符串)。
    fn read_frame(probe: &mut UnixStream) -> (u8, String) {
        let mut header = [0u8; 5];
        probe.read_exact(&mut header).expect("应该收到回应帧，说明 PtyWrite 被丢了");
        let len = u32::from_be_bytes(header[1..5].try_into().unwrap()) as usize;
        let mut payload = vec![0u8; len];
        probe.read_exact(&mut payload).expect("帧头声明的长度和实际 payload 对不上");
        (header[0], String::from_utf8(payload).expect("回应应该是纯文本转义序列"))
    }

    /// `ESC[6n`（Cursor Position Report 查询）：alacritty 解析后应该通过
    /// Event::PtyWrite 吐出 `ESC[row;colR`，之前这个事件被 `_ => {}` 吞掉，
    /// Claude Code 输入框那类依赖精确光标定位的渲染（ghost-text 补全）就拿不到
    /// 位置信息。
    #[test]
    fn cursor_position_query_gets_answered() {
        let (proxy, mut probe) = make_proxy();
        let size = TermSize { rows: 24, cols: 80 };
        let mut term = Term::new(Config::default(), &size, proxy);
        let mut parser: Processor = Processor::new();

        parser.advance(&mut term, b"\x1b[6n");

        let (ty, resp) = read_frame(&mut probe);
        assert_eq!(ty, 0, "回应要走 type=0（PTY 输入）帧，跟键盘输入同一条路");
        assert!(
            resp.starts_with("\x1b[") && resp.ends_with('R'),
            "应为 ESC[row;colR 格式的光标位置回应，实际收到: {resp:?}"
        );
    }

    /// `OSC 11 ?`（查询当前背景色）：之前同样被吞掉，回应里应带上我们固定的
    /// DEFAULT_BG（`0x1a1b26`）而不是空/无回应。
    #[test]
    fn background_color_query_gets_answered() {
        set_dark_mode(true); // 全局态，跟其它测试共进程跑，显式定住深色断言的前提
        let (proxy, mut probe) = make_proxy();
        let size = TermSize { rows: 24, cols: 80 };
        let mut term = Term::new(Config::default(), &size, proxy);
        let mut parser: Processor = Processor::new();

        parser.advance(&mut term, b"\x1b]11;?\x07");

        let (ty, resp) = read_frame(&mut probe);
        assert_eq!(ty, 0);
        assert!(resp.contains("rgb:1a1a/1b1b/2626"), "应含 DEFAULT_BG 的 rgb 十六进制，实际: {resp:?}");
    }
}
