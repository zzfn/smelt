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

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Direction, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::search::{RegexIter, RegexSearch};
use alacritty_terminal::term::{
    point_to_viewport, viewport_to_point, Config, Term, TermDamage, TermMode, SEMANTIC_ESCAPE_CHARS,
};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Processor, Rgb};

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

/// 一个渲染用的终端单元：字符 + 前景/背景 rgb + 字形修饰 + 是否在选区内。
pub struct Cell {
    pub ch: char,
    pub fg: u32,
    pub bg: u32,
    pub bold: bool,
    /// SGR 3。bat / delta 的注释、agent 输出的强调文本都在用。
    pub italic: bool,
    /// SGR 2（faint）。CLI 里做视觉层级的主力——git 的次要信息、`ls` 的元数据、
    /// agent 的灰色提示行。渲染侧把前景色的 alpha 乘 0.7（跟 Zed / alacritty 一致）。
    pub dim: bool,
    /// 任意一种下划线（SGR 4 及其变体，alacritty 的 `ALL_UNDERLINES` 聚合位）。
    pub underline: bool,
    /// 下划线是波浪线（SGR 4:3）。编译器诊断、`rg --hyperlink`、TUI 的错误标注在用。
    pub undercurl: bool,
    /// SGR 9 删除线。
    pub strikeout: bool,
    /// 挂在这一格上的**零宽字符**（alacritty 的 `cell.zerowidth()`）：变体选择器
    /// （`⚠` + U+FE0F 才是彩色 emoji ⚠️）、组合变音符（`e` + U+0301 = é）、ZWJ 等。
    ///
    /// 它们必须跟着基字符一起交给排版器，但**不占格子**。丢掉的话：emoji 掉成黑白字形，
    /// 带声调的文字直接掉音标——而 alacritty 复制时是带上它们的，于是「看到的 ≠ 复制到的」。
    /// 绝大多数格子没有零宽字符，用 Option 免掉每格一次分配。
    pub zw: Option<Box<[char]>>,
    /// OSC 8 超链接（`ESC]8;;uri ST`）的目标 URI。`eza` / `gh` / `npm` / `cargo` 和各家
    /// agent 都在用它——**可见文本是标题、URL 藏在协议里**，所以光靠正则扫可见文本
    /// （见 find_urls）是找不到的。这是终端协议层的东西，不绑定任何一家 agent。
    pub link: Option<Arc<str>>,
    /// 这一格的底色是不是「终端默认底色」。**必须按颜色枚举判**（`Color::Named(Background)`，
    /// 跟 Zed 的 `is_default_background_color` 一致），不能拿解析出来的 RGB 去比：应用完全
    /// 可以显式设一个恰好等于默认底色的 RGB（`\e[48;2;…m`），那时它是「真的画了一块底色」，
    /// 而我们开着背景图 / 透明度时，默认底色的格子是**留空让背景透出来**的——判错就会在
    /// 本该是纯色块的地方漏出背景图。
    pub bg_default: bool,
    pub selected: bool,
}

/// 选区类型（对 terminal_view 屏蔽 alacritty 的 SelectionType）：
/// Simple=普通拖选，Word=双击选词（语义边界），Line=三击选整行。
#[derive(Clone, Copy)]
pub enum SelectionKind {
    Simple,
    Word,
    Line,
}

/// 光标形状（对 terminal_view 屏蔽 alacritty 的 CursorShape）。应用用 DECSCUSR
/// （`CSI Ps SP q`）切换——zsh 的 vi-mode 用它在插入态显竖线、普通态显方块。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CursorKind {
    /// 实心方块（默认）
    Block,
    /// 下划线
    Underline,
    /// 竖线
    Bar,
    /// 空心方块
    Hollow,
}

/// 一帧终端快照：网格行 + 光标。
pub struct Frame {
    pub rows: Vec<Vec<Cell>>,
    /// **可见**光标 (行, 列, 形状)。None = 已上滚离开可视区，或应用用 `CSI ?25l`
    /// 隐藏了光标——全屏 TUI（Cursor CLI / Claude Code 等）常隐藏真实光标、在自己的
    /// 输入框里画反色假光标，这时真实光标往往停在角落，照画会多出一个孤立色块。
    pub cursor: Option<(usize, usize, CursorKind)>,
    /// 光标**位置** (行, 列)，含被隐藏的情况；None 仅表示不在可视区内。
    /// IME 候选窗 / 预编辑串定位用——光标藏没藏，输入法都得知道往哪落。
    pub cursor_pos: Option<(usize, usize)>,
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

/// 网格行列 + 单元格像素尺寸，给 OSC/CSI 查询（TextAreaSizeRequest）和 PTY resize 用。
#[derive(Clone, Copy)]
struct TermMetrics {
    rows: u16,
    cols: u16,
    /// 单格宽/高（像素）。0 = 未知（首帧量字宽之前）。
    cell_w: u16,
    cell_h: u16,
}

/// 事件代理：alacritty 的 EventListener。终端响铃 Event::Bell → 写入一条默认通知；
/// PtyWrite / ColorRequest / Clipboard* / TextAreaSizeRequest → 写回 PTY 或系统剪贴板；
/// 其余事件仍忽略（重绘走 UI 定时快照）。
#[derive(Clone)]
struct EventProxy {
    notify: NotifySlot,
    /// 终端标题（OSC 0/2）——Claude Code 用它实时报告「在干嘛」（任务名 + 状态符号）。
    title: Arc<Mutex<Option<String>>>,
    /// 守护连接写端，跟 [`Terminal`] 自己发键盘输入共用同一把锁——两边都是往同一个
    /// socket 写帧，混着写会把帧头/帧长/payload 交叉打乱，必须靠这把锁串行。
    writer: Arc<Mutex<UnixStream>>,
    /// 当前网格/单元格尺寸（TextAreaSizeRequest 应答用）。
    metrics: Arc<Mutex<TermMetrics>>,
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
            // OSC 52：应用把文本写到系统剪贴板 / 从剪贴板读回。远程会话、嵌套
            // tmux、部分 CLI 复制都靠它。读写走系统工具（见 os_clipboard_*），不必
            // 绕到 UI 线程——EventProxy 跑在 PTY 读线程上。
            Event::ClipboardStore(_ty, data) => os_clipboard_write(&data),
            Event::ClipboardLoad(_ty, format) => {
                let text = os_clipboard_read();
                self.write_pty(format(&text).as_bytes());
            }
            Event::TextAreaSizeRequest(format) => {
                let m = self.metrics.lock().ok().map(|g| *g).unwrap_or(TermMetrics {
                    rows: 24,
                    cols: 80,
                    cell_w: 0,
                    cell_h: 0,
                });
                let ws = WindowSize {
                    num_lines: m.rows,
                    num_cols: m.cols,
                    cell_width: m.cell_w,
                    cell_height: m.cell_h,
                };
                self.write_pty(format(ws).as_bytes());
            }
            _ => {}
        }
    }
}

/// OSC 52 写系统剪贴板。macOS 用 `pbcopy`（任意线程可调）；其它平台静默忽略。
fn os_clipboard_write(text: &str) {
    #[cfg(target_os = "macos")]
    {
        use std::io::Write;
        use std::process::{Command, Stdio};
        if let Ok(mut child) = Command::new("pbcopy").stdin(Stdio::piped()).spawn() {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            let _ = child.wait();
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = text;
    }
}

/// OSC 52 读系统剪贴板。macOS 用 `pbpaste`；失败 / 其它平台返回空串（format 仍会写出
/// 空应答，对端不至于卡死等回包）。
fn os_clipboard_read() -> String {
    #[cfg(target_os = "macos")]
    {
        if let Ok(out) = std::process::Command::new("pbpaste").output() {
            return String::from_utf8_lossy(&out.stdout).into_owned();
        }
    }
    String::new()
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
            // 由 GUI 拉起 → smeltd 继承登录会话、连得上 WindowServer，才允许它挂菜单栏
            // 图标（见 smeltd.rs::menubar）。命令行直接跑 smeltd 时没这个 env，纯 headless。
            .env("SMELT_MENUBAR", "1")
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

/// alacritty Term 的统一配置（生产 spawn 与测试共用，防两边漂移）：
/// - kitty_keyboard：默认 false 时 alacritty 会把 `CSI > 1 u` 静默丢掉
///   （push_keyboard_mode 里直接 return），DISAMBIGUATE_ESC_CODES 永远置不上，
///   Shift+Enter 也就永远退化成裸 Enter。见 kitty_keyboard_mode / keystroke_to_bytes。
/// - semantic_escape_chars：双击选词的断词字符。默认集合只有半角标点，中文场景下
///   全角标点也该断词（双击「数据层：字段」不该整段连选），追加常用全角标点。
fn term_config() -> Config {
    Config {
        kitty_keyboard: true,
        semantic_escape_chars: format!(
            "{SEMANTIC_ESCAPE_CHARS}：，。；！？、（）「」『』【】《》“”‘’"
        ),
        ..Config::default()
    }
}

/// 客户端 → 守护的帧：[type:u8][len:u32 BE][payload]。type 0=输入，1=resize。
fn write_frame(w: &mut UnixStream, ty: u8, payload: &[u8]) {
    let mut frame = Vec::with_capacity(5 + payload.len());
    frame.push(ty);
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(payload);
    let _ = w.write_all(&frame);
}

/// 编码一帧鼠标事件。`button`：0=左键、3=X10 松开、32=左键拖动等（xterm 约定）。
/// `pressed` 只对 SGR 有意义（`M` vs `m`）；X10 松开时调用方应传 button=3。
fn encode_mouse(mode: TermMode, button: u8, pressed: bool, row: usize, col: usize) -> Vec<u8> {
    let cx = col.saturating_add(1);
    let cy = row.saturating_add(1);
    if mode.contains(TermMode::SGR_MOUSE) {
        format!("\x1b[<{button};{cx};{cy}{}", if pressed { 'M' } else { 'm' }).into_bytes()
    } else {
        // X10：各值偏移 32，坐标裁到 223。
        let cb = button.min(223);
        let bx = 32u8.saturating_add(cx.min(223) as u8);
        let by = 32u8.saturating_add(cy.min(223) as u8);
        vec![0x1b, b'[', b'M', 32 + cb, bx, by]
    }
}

/// 把用户输入当成字面量塞进 RegexSearch：转义正则元字符，避免 `foo.bar` 误匹配。
pub(crate) fn escape_regex_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for c in s.chars() {
        if matches!(
            c,
            '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '^' | '$'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// 收集 buffer 内全部命中（上限 `SEARCH_MATCH_CAP`）。
fn collect_search_matches<T>(term: &Term<T>, query: &str) -> Vec<(Point, Point)> {
    let pattern = escape_regex_literal(query);
    let Ok(mut regex) = RegexSearch::new(&pattern) else {
        return Vec::new();
    };
    let start = Point::new(term.topmost_line(), Column(0));
    let end = Point::new(term.bottommost_line(), term.last_column());
    RegexIter::new(start, end, Direction::Right, term, &mut regex)
        .take(SEARCH_MATCH_CAP)
        .map(|m| (*m.start(), *m.end()))
        .collect()
}

/// 绝对坐标命中 → 可视区 SearchHit；不在可视区则 None。
pub(crate) fn match_to_viewport_hit(
    start: Point,
    end: Point,
    display_offset: usize,
    cols: usize,
    active: bool,
) -> Option<SearchHit> {
    let vp = point_to_viewport(display_offset, start)?;
    let col_start = vp.column.0;
    let col_end = if end.line == start.line {
        end.column.0
    } else {
        cols.saturating_sub(1)
    };
    Some(SearchHit {
        row: vp.line,
        col_start,
        col_end,
        active,
    })
}

/// 把剪贴板文本编码成写入 PTY 的字节（见 [`Terminal::paste`]）。
/// 抽成纯函数方便单测，不依赖真 PTY。
pub(crate) fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        // 剥 ESC：bracketed 内容里若夹着转义序列，会被应用当控制命令执行。
        let cleaned = text.replace('\x1b', "");
        let mut out = Vec::with_capacity(cleaned.len() + 12);
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(cleaned.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    }
}

/// 终端内搜索的一条命中（可视区坐标，0 基；跨行时只标首行起止列）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchHit {
    pub row: usize,
    pub col_start: usize,
    pub col_end: usize, // 含
    /// 是否为「当前」命中（下/上一个跳到的那条，画得更醒目）。
    pub active: bool,
}

/// 一次搜索操作的汇总：给搜索条显示「3/12」。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SearchStatus {
    /// 当前命中序号（1 基）；0 表示没有命中。
    pub current: usize,
    pub total: usize,
}

/// 滚动条用：`display_offset` 越大越往历史上看；0 = 贴底。
#[derive(Clone, Copy, Debug, Default)]
pub struct ScrollInfo {
    pub offset: usize,
    /// 最大可滚 offset（= history_size）。0 表示没有 scrollback，不必画条。
    pub max_offset: usize,
    pub viewport_rows: usize,
}

/// 单次搜索最多收集的命中数，避免超大缓冲卡顿。
const SEARCH_MATCH_CAP: usize = 2000;

/// 一个内嵌终端：alacritty 的 Term（后台线程写、UI 线程读）+ 守护连接写端。
pub struct Terminal {
    term: Arc<Mutex<Term<EventProxy>>>,
    /// 写端加锁共享给 EventProxy（见其字段注释）：键盘输入和终端自动应答都从这
    /// 发出，必须串行，不能各拿一个裸 fd 各写各的。
    writer: Arc<Mutex<UnixStream>>,
    size: TermSize,
    /// 与 EventProxy 共享的行列/单元格像素（resize 与 TextAreaSizeRequest 共用）。
    metrics: Arc<Mutex<TermMetrics>>,
    /// 通知消息槽（响铃 / OSC 9 写入，UI 轮询 take_notification 取走）。
    notify: NotifySlot,
    /// 终端标题（agent 实时状态；UI 读 current_title 用于通知 / 总览）。
    title: Arc<Mutex<Option<String>>>,
    /// `take_damage` 用来识别「光标真的动了」——alacritty 每帧都会把当前光标格标脏，
    /// 静止时要滤掉；但光标移动后若只标了新位置那一格，也得算变化（见 take_damage）。
    last_damage_cursor: Mutex<Option<(i32, usize)>>,
    /// 当前搜索查询串；变了就重建 `search_matches`。
    search_query: Mutex<String>,
    /// 全部命中（缓冲绝对坐标 start..=end），按阅读顺序。
    search_matches: Mutex<Vec<(Point, Point)>>,
    /// 当前命中在 `search_matches` 里的下标。
    search_index: Mutex<usize>,
    /// 重绘唤醒（Zed 式事件驱动）：读线程每喂完一批字节就 `try_send(())`，UI 侧
    /// 一个 `cx.spawn` 任务 `recv().await` 后 `cx.notify()`。这样「喂内容」与「触发
    /// 重绘」是同一个动作，不再依赖 30ms 轮询去 `take_damage()` 事后发现——reattach
    /// 后 agent 空闲、只有唯一一次输出时，轮询的时序/过滤一旦漏掉就永久停帧，正是
    /// 那个「底部画不出来、一敲键盘/框选才好」的 bug。`bounded(1)` 天然合并突发：
    /// 空闲无输出＝无唤醒（保住 P0 那条空闲不重绘的优化），有输出才唤醒。
    redraw_rx: smol::channel::Receiver<()>,
    /// 读线程 EOF/出错后置 true：无头任务 job 用来判定 oneshot 结束。
    finished: Arc<AtomicBool>,
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
        let metrics = Arc::new(Mutex::new(TermMetrics {
            rows: size.rows as u16,
            cols: size.cols as u16,
            cell_w: 0,
            cell_h: 0,
        }));
        let term = Term::new(
            term_config(),
            &size,
            EventProxy {
                notify: notify.clone(),
                title: title.clone(),
                writer: writer.clone(),
                metrics: metrics.clone(),
            },
        );
        let term = Arc::new(Mutex::new(term));

        // 3) 后台读线程：守护转发的 PTY 字节 → vte 解析更新 Term 网格 + 扫 OSC 9/777。
        //    EOF = shell 退出或守护离线（网格冻结，重开会话即恢复）。
        //    复用尺寸行的 BufReader：重放字节可能已在其内部缓冲里。
        // 重绘唤醒通道：读线程 → UI。bounded(1) 合并突发（已有待处理唤醒时后续 try_send
        // 直接丢弃，不堆积）。见 Terminal::redraw_rx 字段注释。
        let (redraw_tx, redraw_rx) = smol::channel::bounded::<()>(1);

        let mut reader = buffered;
        let term_reader = Arc::clone(&term);
        let notify_reader = notify.clone();
        let finished = Arc::new(AtomicBool::new(false));
        let finished_w = finished.clone();
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
                        // 喂完这批立刻请求一次重绘（Zed 式：内容生产者驱动重绘）。
                        // bounded(1) + try_send：已有待处理唤醒就丢弃，天然合并。
                        // 关键性质：最后一批喂完后必有一次待处理唤醒 → UI 必定再画一帧，
                        // 与 reattach 快照喂完这个场景精确对应。
                        let _ = redraw_tx.try_send(());
                    }
                    Err(_) => break,
                }
            }
            finished_w.store(true, Ordering::SeqCst);
            // 读线程退出（EOF/守护离线）：主动关掉发送端，让 UI 侧的 recv 任务收到
            // Err 而退出，不空转。
            drop(redraw_tx);
        });

        Ok(Self {
            term,
            writer,
            size,
            metrics,
            notify,
            title,
            last_damage_cursor: Mutex::new(None),
            search_query: Mutex::new(String::new()),
            search_matches: Mutex::new(Vec::new()),
            search_index: Mutex::new(0),
            redraw_rx,
            finished,
        })
    }

    /// 读线程是否已结束（shell 退出或守护断连）。
    #[allow(dead_code)]
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Relaxed)
    }

    /// 重绘唤醒的接收端（clone 一份给 UI 侧的 `cx.spawn` 任务 await）。见 `redraw_rx` 字段。
    pub fn redraw_channel(&self) -> smol::channel::Receiver<()> {
        self.redraw_rx.clone()
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
    /// 判定。**不**涵盖：用户拖选（Term.selection 的变化 alacritty 不计入 damage，
    /// 它认为选区高亮是渲染层的事）、Cmd 悬停链接高亮——这两个在 TerminalView
    /// 各自的鼠标事件处理里已经各自调用 cx.notify()，不依赖这里。
    ///
    /// 只应由每个 TerminalView 自己的定时刷新循环调用（每个 Terminal 独占一个
    /// Term，不会有多个消费者互相"偷"对方读到的脏区）。
    pub fn take_damage(&self) -> bool {
        let Ok(mut term) = self.term.lock() else {
            // 拿不到锁（锁中毒）：保守起见当作有变化，避免画面从此卡死不再刷新。
            return true;
        };
        // alacritty 的 damage_cursor() 每次都会无条件把**当前**光标格标脏（为闪烁动画
        // 设计）。smelt 没有闪烁，静止时必须滤掉「仅当前光标那一格」——否则空闲也 33fps。
        //
        // 但光标真的移动时，脏区可能**只有新位置那一格**（旧格不在 partial 里），若仍按
        // 「等于当前光标就忽略」会吞掉移动 → 光标不重画。所以再记一帧光标位置：动了就
        // 算有变化。
        let cursor = term.grid().cursor.point;
        let cur = (cursor.line.0, cursor.column.0);
        let cursor_moved = match self.last_damage_cursor.lock() {
            Ok(mut g) => {
                let moved = g.map(|prev| prev != cur).unwrap_or(true);
                *g = Some(cur);
                moved
            }
            Err(_) => true,
        };
        let cursor_line = cur.0 as usize;
        let cursor_col = cur.1;
        let damaged = match term.damage() {
            TermDamage::Full => true,
            TermDamage::Partial(it) => {
                let mut any = false;
                let mut only_idle_cursor = true;
                for l in it {
                    any = true;
                    if l.line != cursor_line || l.left != cursor_col || l.right != cursor_col {
                        only_idle_cursor = false;
                        break;
                    }
                }
                if !any {
                    false
                } else if only_idle_cursor {
                    // 脏区恰好是当前光标格：只有光标真的动了才算变化
                    cursor_moved
                } else {
                    true
                }
            }
        };
        term.reset_damage();
        damaged
    }

    /// 按新行列 + 单元格像素 resize：同步 alacritty 网格，并发帧让守护 ioctl
    /// TIOCSWINSZ（含 ws_xpixel/ws_ypixel）。`cell_w_px` / `cell_h_px` 为 0 时只更新
    /// 行列（兼容老路径）。无变化则跳过。
    pub fn resize(&mut self, rows: usize, cols: usize, cell_w_px: u16, cell_h_px: u16) {
        if rows == 0 || cols == 0 {
            return;
        }
        let same_grid = rows == self.size.rows && cols == self.size.cols;
        let same_cell = self
            .metrics
            .lock()
            .ok()
            .is_some_and(|m| m.cell_w == cell_w_px && m.cell_h == cell_h_px);
        if same_grid && same_cell {
            return;
        }
        self.size = TermSize { rows, cols };
        if let Ok(mut m) = self.metrics.lock() {
            m.rows = rows as u16;
            m.cols = cols as u16;
            if cell_w_px > 0 {
                m.cell_w = cell_w_px;
            }
            if cell_h_px > 0 {
                m.cell_h = cell_h_px;
            }
        }
        if !same_grid {
            if let Ok(mut term) = self.term.lock() {
                term.resize(self.size);
            }
        }
        // type 1 帧：cols + rows + cell_w + cell_h（各 u32 BE）。老 smeltd 只认 8 字节，
        // 新守护认 16 字节并把 cell 像素乘到 ws_xpixel/ws_ypixel。
        let (cw, ch) = self
            .metrics
            .lock()
            .ok()
            .map(|m| (m.cell_w, m.cell_h))
            .unwrap_or((cell_w_px, cell_h_px));
        let mut payload = [0u8; 16];
        payload[0..4].copy_from_slice(&(cols as u32).to_be_bytes());
        payload[4..8].copy_from_slice(&(rows as u32).to_be_bytes());
        payload[8..12].copy_from_slice(&(cw as u32).to_be_bytes());
        payload[12..16].copy_from_slice(&(ch as u32).to_be_bytes());
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

    /// 粘贴文本到 PTY。对端开了 bracketed paste（`CSI ?2004h`）时包
    /// `\x1b[200~…\x1b[201~`，并剥掉内容里的 ESC（防注入序列）；否则只把 `\r\n`/`\n`
    /// 规范成 `\r`——shell 行编辑器认的是 CR，原样喂 LF 会在 zsh/bash 里被当成提交
    /// 多次。跟 Zed `Terminal::paste` / iTerm 行为一致。
    pub fn paste(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let bracketed = match self.term.lock() {
            Ok(term) => term.mode().contains(TermMode::BRACKETED_PASTE),
            Err(_) => false,
        };
        self.send_input(&encode_paste(text, bracketed));
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

    /// 对端有没有开 kitty keyboard protocol 的「消歧」层（进入时发 `CSI > 1 u`）。
    /// 传统终端编码里 Shift+Enter 跟裸 Enter 撞车（都是 `\r`），修饰键信息丢了；开了这个
    /// 模式后带修饰键的按键改用 CSI u 编码上报，两者才能分开。Claude Code 从 v2.1 起
    /// 启动时会主动开——不开的程序（bash/zsh）就得继续收遗留编码，见 keystroke_to_bytes。
    pub fn kitty_keyboard_mode(&self) -> bool {
        match self.term.lock() {
            Ok(term) => term.mode().contains(TermMode::DISAMBIGUATE_ESC_CODES),
            Err(_) => false,
        }
    }

    /// 可视区的纯文本行（总览页迷你预览用）。**不走 snapshot**：那会把整个网格连同颜色、
    /// 属性、链接一起 clone 一遍，而这里只要字符——总览页每帧对每个终端都调一次，白白
    /// clone 几千个 Cell 太亏。零宽字符（变体选择器 / 音标）要带上，否则预览里 emoji 掉成
    /// 黑白、带声调的字掉音标。
    pub fn text_lines(&self) -> Vec<String> {
        let Ok(term) = self.term.lock() else {
            return Vec::new();
        };
        let cols = self.size.cols;
        let mut lines = Vec::with_capacity(self.size.rows);
        let mut cur = String::new();
        let mut count = 0usize;
        for indexed in term.renderable_content().display_iter {
            let cell = indexed.cell;
            if !cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                cur.push(cell.c);
                if let Some(zw) = cell.zerowidth() {
                    cur.extend(zw);
                }
            }
            count += 1;
            if count % cols == 0 {
                lines.push(std::mem::take(&mut cur).trim_end().to_string());
            }
        }
        if !cur.is_empty() {
            lines.push(cur.trim_end().to_string());
        }
        lines
    }

    /// 焦点变化上报（DEC 1004，`CSI ?1004h` 打开）：应用开了这个模式时，终端在获得 / 失去
    /// 焦点时要发 `ESC[I` / `ESC[O`。vim、部分 TUI 靠它决定要不要暂停动画、要不要重绘成
    /// 「未聚焦」的样子。没开这个模式的应用绝不能收到这两个序列——否则会被当成普通输入。
    pub fn report_focus(&mut self, focused: bool) {
        let enabled = match self.term.lock() {
            Ok(term) => term.mode().contains(TermMode::FOCUS_IN_OUT),
            Err(_) => false,
        };
        if enabled {
            self.send_input(if focused { b"\x1b[I" } else { b"\x1b[O" });
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
                    cursor_pos: None,
                }
            }
        };
        let content = term.renderable_content();
        let cursor_pt = content.cursor.point;
        let display_offset = content.display_offset;
        // 选区范围由 alacritty 维护（滚动跟随、新输出漂移、宽字符边界都是它处理），
        // 这里只做逐 cell 的 contains 判定——indexed.point 与 SelectionRange 坐标同源，直接比。
        let sel_range = content.selection;

        let cols = self.size.cols;
        let mut rows: Vec<Vec<Cell>> = Vec::with_capacity(self.size.rows);
        let mut row: Vec<Cell> = Vec::with_capacity(cols);
        let mut count = 0usize;
        for indexed in content.display_iter {
            let cell = indexed.cell;
            let selected = sel_range.as_ref().is_some_and(|r| r.contains(indexed.point));
            let flags = cell.flags;
            let inverse = flags.contains(Flags::INVERSE);
            let mut fg = resolve(cell.fg, true);
            let mut bg = resolve(cell.bg, false);
            // 反色（SGR 7）后真正当底色用的是**前景那个颜色**，默认底色的判定也得跟着换。
            let bg_color = if inverse { cell.fg } else { cell.bg };
            let bg_default = matches!(bg_color, Color::Named(NamedColor::Background));
            if inverse {
                std::mem::swap(&mut fg, &mut bg);
            }
            // 宽字符占两格，第二格是 WIDE_CHAR_SPACER 占位：一律记成 '\0'。
            // 渲染侧（render_row）据此跳过该格但让列号照常前进，于是宽字符后面的内容
            // 列号不再连续 → 自动断成新的一批、按 grid 列重新定位。字形本身宽窄不影响
            // 后续字符的位置，所以这里不必再区分「字形正好两格的 CJK」和「宽度不足的
            // emoji」——那个区分只在「靠字形宽度自然占位」的旧渲染下才有意义。
            let ch = if flags.contains(Flags::WIDE_CHAR_SPACER) { '\0' } else { cell.c };
            row.push(Cell {
                ch,
                fg,
                bg,
                bold: flags.contains(Flags::BOLD),
                italic: flags.contains(Flags::ITALIC),
                dim: flags.contains(Flags::DIM),
                // 下划线有 5 种（普通/双线/波浪/点/虚线），`ALL_UNDERLINES` 是它们的聚合位；
                // 只认 UNDERLINE 那一位的话，编译器诊断的波浪线之类会整个不显示。
                underline: flags.intersects(Flags::ALL_UNDERLINES),
                undercurl: flags.contains(Flags::UNDERCURL),
                strikeout: flags.contains(Flags::STRIKEOUT),
                zw: cell
                    .zerowidth()
                    .filter(|z| !z.is_empty())
                    .map(|z| z.to_vec().into_boxed_slice()),
                link: cell.hyperlink().map(|h| Arc::from(h.uri())),
                bg_default,
                selected,
            });
            count += 1;
            if count % cols == 0 {
                rows.push(std::mem::take(&mut row));
            }
        }
        if !row.is_empty() {
            rows.push(row);
        }

        // 光标位置：alacritty 的 cursor.point 是**活动区**坐标（不含滚动偏移），加上
        // display_offset 才是屏幕上的行——上滚 N 行看历史时，内容整体下移 N 行，光标也
        // 跟着往下走（iTerm2 行为）。只有滚到光标离开可视区才没有位置。
        // 之前是「一上滚就直接 None」：滚一行光标就消失，IME 候选窗也跟着跳回左上角。
        let cursor_pos = {
            let r = cursor_pt.line.0 + display_offset as i32;
            if r >= 0 && (r as usize) < rows.len() {
                Some((r as usize, cursor_pt.column.0))
            } else {
                None
            }
        };
        // 可见光标：应用没用 CSI ?25l 隐藏时才交给渲染层画（见 Frame 字段注释）。
        // 形状随 DECSCUSR 走（zsh vi-mode 会在插入/普通态之间切竖线和方块）。
        let cursor = match content.cursor.shape {
            CursorShape::Hidden => None,
            shape => cursor_pos.map(|(r, c)| {
                let kind = match shape {
                    CursorShape::Underline => CursorKind::Underline,
                    CursorShape::Beam => CursorKind::Bar,
                    CursorShape::HollowBlock => CursorKind::Hollow,
                    _ => CursorKind::Block,
                };
                (r, c, kind)
            }),
        };

        Frame { rows, cursor, cursor_pos }
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
        } else {
            // 不用 if-let 挂 lock()：Edition 2024 下 if-let 临时值 drop 更早，
            // 与 MutexGuard 同句时语义含糊；拆成块内绑定更清晰。
            let Ok(mut term) = self.term.lock() else { return };
            term.scroll_display(Scroll::Delta(lines));
        }
    }

    /// 应用是否开了任意鼠标上报（click / drag / motion 之一）。UI 用来在
    /// 「本地框选」和「转发给 TUI」之间分流；按住 Shift 时调用方应强制走本地选区
    /// （xterm 约定：Shift 旁路应用鼠标）。
    pub fn mouse_mode(&self) -> bool {
        match self.term.lock() {
            Ok(term) => term.mode().intersects(TermMode::MOUSE_MODE),
            Err(_) => false,
        }
    }

    /// 鼠标按下/松开上报。`button`：0=左、1=中、2=右（xterm 约定）。
    /// 应用开了 `MOUSE_MODE` 时才编码转发，否则返回 false。
    pub fn mouse_button(&mut self, button: u8, pressed: bool, row: usize, col: usize) -> bool {
        let mode = match self.term.lock() {
            Ok(term) => *term.mode(),
            Err(_) => return false,
        };
        if !mode.intersects(TermMode::MOUSE_MODE) {
            return false;
        }
        // SGR：button + pressed 决定 M/m；X10：松开固定 button 3。
        let code = if !pressed && !mode.contains(TermMode::SGR_MOUSE) {
            3
        } else {
            button.min(2)
        };
        self.send_input(&encode_mouse(mode, code, pressed, row, col));
        true
    }

    /// 按住某键拖动上报（button = 32+btn）。仅在 `MOUSE_DRAG` 或 `MOUSE_MOTION` 时转发。
    pub fn mouse_drag(&mut self, button: u8, row: usize, col: usize) -> bool {
        let mode = match self.term.lock() {
            Ok(term) => *term.mode(),
            Err(_) => return false,
        };
        if !mode.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION) {
            return false;
        }
        // 32 = motion 标志；+0/1/2 = 左/中/右。
        self.send_input(&encode_mouse(mode, 32 + button.min(2), true, row, col));
        true
    }

    /// 无按键悬停 motion（button = 35）。仅 `MOUSE_MOTION` 全开时 TUI 才关心。
    pub fn mouse_motion(&mut self, row: usize, col: usize) -> bool {
        let mode = match self.term.lock() {
            Ok(term) => *term.mode(),
            Err(_) => return false,
        };
        if !mode.contains(TermMode::MOUSE_MOTION) {
            return false;
        }
        self.send_input(&encode_mouse(mode, 35, true, row, col));
        true
    }

    /// 重建搜索命中列表（查询变了或内容大变时）。不滚动、不改当前序号（夹到合法范围）。
    pub fn set_search_query(&mut self, query: &str) -> SearchStatus {
        let q = query.trim().to_string();
        if q.is_empty() {
            self.clear_search();
            return SearchStatus::default();
        }
        let Ok(term) = self.term.lock() else {
            return SearchStatus::default();
        };
        let matches = collect_search_matches(&term, &q);
        let total = matches.len();
        if let Ok(mut g) = self.search_query.lock() {
            *g = q;
        }
        if let Ok(mut g) = self.search_matches.lock() {
            *g = matches;
        }
        if let Ok(mut g) = self.search_index.lock() {
            if total == 0 {
                *g = 0;
            } else {
                *g = (*g).min(total - 1);
            }
        }
        SearchStatus {
            current: if total == 0 { 0 } else { self.search_index.lock().map(|g| *g + 1).unwrap_or(1) },
            total,
        }
    }

    /// 跳到下一处 / 上一处命中并滚动到可视区。查询串变了会先重建列表。
    pub fn find_next(&mut self, query: &str, backward: bool) -> SearchStatus {
        let q = query.trim().to_string();
        if q.is_empty() {
            self.clear_search();
            return SearchStatus::default();
        }
        let query_changed = self
            .search_query
            .lock()
            .ok()
            .is_none_or(|g| *g != q);
        if query_changed {
            let _ = self.set_search_query(&q);
            // 新查询：后退从末条起，前进从首条起
            if let Ok(mut g) = self.search_index.lock() {
                let total = self.search_matches.lock().map(|m| m.len()).unwrap_or(0);
                *g = if total == 0 {
                    0
                } else if backward {
                    total - 1
                } else {
                    0
                };
            }
        } else {
            // 查询没变，但缓冲可能已滚动、新输出里也可能有新命中：旧坐标
            // 整体过期，步进前先按当前缓冲重建列表（重建会保住当前下标）。
            let _ = self.set_search_query(&q);
            let total = self.search_matches.lock().map(|m| m.len()).unwrap_or(0);
            if total == 0 {
                return SearchStatus::default();
            }
            if let Ok(mut g) = self.search_index.lock() {
                *g = if backward {
                    if *g == 0 { total - 1 } else { *g - 1 }
                } else {
                    (*g + 1) % total
                };
            }
        }
        self.scroll_to_active_match();
        self.search_status()
    }

    /// 当前搜索序号汇总。
    pub fn search_status(&self) -> SearchStatus {
        let total = self.search_matches.lock().map(|m| m.len()).unwrap_or(0);
        if total == 0 {
            return SearchStatus::default();
        }
        let current = self
            .search_index
            .lock()
            .map(|g| (*g + 1).min(total))
            .unwrap_or(1);
        SearchStatus { current, total }
    }

    /// 当前可视区内所有命中（含 active 标记），供 paint 高亮。
    ///
    /// 每次都按当前缓冲重新收集：缓存里的命中是收集那一刻的绝对坐标，终端
    /// 每滚一行它们就整体过期（全部 Line -1），照旧坐标画高亮会落在无关文本
    /// 上。本方法只在搜索条打开时被调用，命中数有 SEARCH_MATCH_CAP 兜底。
    pub fn viewport_search_hits(&self) -> Vec<SearchHit> {
        let Ok(term) = self.term.lock() else {
            return Vec::new();
        };
        let query = match self.search_query.lock() {
            Ok(g) => g.clone(),
            Err(_) => return Vec::new(),
        };
        if query.is_empty() {
            return Vec::new();
        }
        let fresh = collect_search_matches(&term, &query);
        let Ok(mut matches) = self.search_matches.lock() else {
            return Vec::new();
        };
        *matches = fresh;
        let total = matches.len();
        let active_idx = self
            .search_index
            .lock()
            .map(|mut g| {
                *g = if total == 0 { 0 } else { (*g).min(total - 1) };
                *g
            })
            .unwrap_or(0);
        let offset = term.grid().display_offset();
        let mut out = Vec::new();
        for (i, (start, end)) in matches.iter().enumerate() {
            if let Some(hit) = match_to_viewport_hit(*start, *end, offset, self.size.cols, i == active_idx)
            {
                out.push(hit);
            }
        }
        out
    }

    fn scroll_to_active_match(&mut self) {
        let Ok(mut term) = self.term.lock() else {
            return;
        };
        let Ok(matches) = self.search_matches.lock() else {
            return;
        };
        let idx = self.search_index.lock().map(|g| *g).unwrap_or(0);
        if let Some((start, _)) = matches.get(idx) {
            term.scroll_to_point(*start);
        }
    }

    /// 清空搜索状态（关搜索条时）。
    pub fn clear_search(&mut self) {
        if let Ok(mut g) = self.search_query.lock() {
            g.clear();
        }
        if let Ok(mut g) = self.search_matches.lock() {
            g.clear();
        }
        if let Ok(mut g) = self.search_index.lock() {
            *g = 0;
        }
    }

    /// 滚动条用的 offset / 上限 / 可视行数。
    pub fn scroll_info(&self) -> ScrollInfo {
        let Ok(term) = self.term.lock() else {
            return ScrollInfo {
                offset: 0,
                max_offset: 0,
                viewport_rows: self.size.rows,
            };
        };
        ScrollInfo {
            offset: term.grid().display_offset(),
            max_offset: term.history_size(),
            viewport_rows: self.size.rows,
        }
    }

    /// 把 display_offset 设到目标值（夹到 `[0, history_size]`）。
    pub fn set_scroll_offset(&mut self, offset: usize) {
        let Ok(mut term) = self.term.lock() else {
            return;
        };
        let max = term.history_size();
        let target = offset.min(max);
        let cur = term.grid().display_offset();
        let delta = target as i32 - cur as i32;
        if delta != 0 {
            // 正数 = 向上看历史（增大 offset）
            term.scroll_display(Scroll::Delta(delta));
        }
    }

    /// 可视区 (行, 列) → 缓冲区绝对坐标：行列先夹进可视范围，再按**当前**
    /// display_offset 换算。选区跟随滚动的关键就是每次都用当前偏移重算。
    fn grid_point(&self, term: &Term<EventProxy>, row: usize, col: usize) -> Point {
        let row = row.min(self.size.rows.saturating_sub(1));
        let col = col.min(self.size.cols.saturating_sub(1));
        viewport_to_point(term.grid().display_offset(), Point::new(row, Column(col)))
    }

    /// 开始一段选区。`left_side`：起点落在单元格左半还是右半（alacritty 用它决定
    /// 该格是否纳入选区——同格同侧的空 Simple 选区不产出内容，单击/微抖不会误选）。
    pub fn selection_start(&mut self, row: usize, col: usize, left_side: bool, kind: SelectionKind) {
        let Ok(mut term) = self.term.lock() else { return };
        let ty = match kind {
            SelectionKind::Simple => SelectionType::Simple,
            SelectionKind::Word => SelectionType::Semantic,
            SelectionKind::Line => SelectionType::Lines,
        };
        let point = self.grid_point(&term, row, col);
        let side = if left_side { Side::Left } else { Side::Right };
        term.selection = Some(Selection::new(ty, point, side));
    }

    /// 拖动更新选区活动端。坐标按当前 display_offset 重算，所以滚动后再拖、
    /// 或拖着不动光滚动（拖边缘自动滚动）都落在正确的缓冲区行上。
    pub fn selection_update(&mut self, row: usize, col: usize, left_side: bool) {
        let Ok(mut term) = self.term.lock() else { return };
        let point = self.grid_point(&term, row, col);
        let side = if left_side { Side::Left } else { Side::Right };
        if let Some(sel) = term.selection.as_mut() {
            sel.update(point, side);
        }
    }

    /// 清除选区。
    pub fn selection_clear(&mut self) {
        if let Ok(mut term) = self.term.lock() {
            term.selection = None;
        }
    }

    /// 当前选区文本：委托 alacritty 按缓冲区绝对行遍历（含已滚出可视区的
    /// scrollback），宽字符占位/软换行由它处理。空选区（单击未拖动）返回 None。
    pub fn selection_text(&self) -> Option<String> {
        let term = self.term.lock().ok()?;
        term.selection_to_string().filter(|s| !s.is_empty())
    }

}

#[cfg(test)]
mod damage_gate_tests {
    use super::*;

    /// P0 性能修复的验证：真空闲时 take_damage() 应稳定为 false（跳过重画），
    /// 写入字节后应变 true（真实变化不会被吞掉）。用全新一次性 session id +
    /// 空临时目录，不碰任何真实/持久化会话。
    ///
    /// 交互 shell 会间歇吐 PROMPT / OSC 标题，不能当「真空闲」基线。先跑 `cat`
    /// 堵住前台（不再画 prompt），再测门控；有输入时 cat 回显触发真实 damage。
    #[test]
    fn idle_then_input_toggles_damage() {
        let dir = std::env::temp_dir().join(format!("smelt-damage-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let id = format!("damage-test-{}", uuid_like());

        let mut term = Terminal::spawn(24, 80, dir.to_str(), &id, None).expect("spawn 失败");

        // 等 shell 起来后进入 cat：前台进程阻塞读，不再周期性画 prompt。
        thread::sleep(Duration::from_millis(500));
        term.send_input(b"cat\n");
        thread::sleep(Duration::from_millis(300));

        // 排掉 cat 启动 + 命令回显带来的 damage，直到连续安静。
        let mut quiet_streak = 0usize;
        for _ in 0..80 {
            thread::sleep(Duration::from_millis(50));
            if term.take_damage() {
                quiet_streak = 0;
            } else {
                quiet_streak += 1;
                if quiet_streak >= 10 {
                    break;
                }
            }
        }
        assert!(
            quiet_streak >= 10,
            "cat 阻塞后未能进入真空闲（quiet_streak={quiet_streak}），无法测 damage 门控"
        );

        // 真空闲：什么都不做，多次采样应稳定为 false。
        let mut idle_true_count = 0;
        for _ in 0..10 {
            thread::sleep(Duration::from_millis(100));
            if term.take_damage() {
                idle_true_count += 1;
            }
        }
        assert_eq!(idle_true_count, 0, "真空闲时 take_damage() 不该返回 true（次数={idle_true_count}）");

        // 写入真实字节：cat 回显，应被判定为变化。
        term.send_input(b"hi\n");
        thread::sleep(Duration::from_millis(300));
        assert!(term.take_damage(), "写入字节后 take_damage() 应返回 true");

        // 清理：让守护杀掉这个一次性测试会话。
        kill_remote(&id);
    }

    /// 走完整生产路径（Terminal::spawn 的真 PTY + 真 shell + alacritty 解析）验证
    /// kitty keyboard protocol 能被识别——上面 event_proxy 那个测试是手搭 Config 的，
    /// 万一 spawn 里忘了开 kitty_keyboard 它照样绿，这里才防得住。
    #[test]
    fn spawned_terminal_honors_kitty_keyboard_protocol() {
        let dir = std::env::temp_dir().join(format!("smelt-kitty-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let id = format!("kitty-test-{}", uuid_like());

        let term = Terminal::spawn(24, 80, dir.to_str(), &id, None).expect("spawn 失败");
        thread::sleep(Duration::from_millis(800)); // 等 shell 起来

        assert!(!term.kitty_keyboard_mode(), "shell 刚起来时不该开着");

        // 让 shell 真的把 `CSI > 1 u` 吐到 PTY 上（Claude Code v2.1+ 启动时干的事）。
        let mut term = term;
        term.send_input(b"printf '\\033[>1u'\n");
        thread::sleep(Duration::from_millis(500));
        assert!(
            term.kitty_keyboard_mode(),
            "真实 PTY 上收到 CSI > 1 u 后应置位——没置位说明 spawn 的 Config 没开 kitty_keyboard"
        );

        kill_remote(&id);
    }

    /// 全屏 TUI（Cursor CLI / Claude Code）用 `CSI ?25l` 藏真实光标、在输入框自画
    /// 假光标，真实光标常停在屏幕角落——snapshot 照画就会多出一个孤立反色块。
    /// 隐藏时 cursor（渲染用）必须为 None，cursor_pos（IME 定位用）必须保留。
    /// 直接往 Term 注入序列（不经 shell），避免时序 flaky；注入前等启动输出沉淀。
    #[test]
    fn hidden_cursor_not_rendered_but_position_kept() {
        let dir = std::env::temp_dir().join(format!("smelt-cursor-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let id = format!("cursor-test-{}", uuid_like());

        let term = Terminal::spawn(24, 80, dir.to_str(), &id, None).expect("spawn 失败");
        thread::sleep(Duration::from_millis(800)); // 等 shell 启动输出沉淀，避免并发 advance 干扰

        let frame = term.snapshot();
        let (row, col, kind) = frame.cursor.expect("shell 正常状态光标应可见");
        assert_eq!(Some((row, col)), frame.cursor_pos, "光标可见时位置应与 cursor_pos 一致");
        assert_eq!(kind, CursorKind::Block, "没发过 DECSCUSR 时是默认的实心块");

        let inject = |bytes: &[u8]| {
            let mut parser: Processor = Processor::new();
            let mut t = term.term.lock().unwrap();
            parser.advance(&mut *t, bytes);
        };

        inject(b"\x1b[?25l");
        let frame = term.snapshot();
        assert!(frame.cursor.is_none(), "CSI ?25l 隐藏后不该再交给渲染层画反色块");
        assert!(frame.cursor_pos.is_some(), "隐藏光标的位置（IME 定位用）不该丢");

        inject(b"\x1b[?25h");
        assert!(term.snapshot().cursor.is_some(), "CSI ?25h 后光标应恢复可见");

        // DECSCUSR：zsh vi-mode 靠它在插入态切竖线、普通态切回方块。形状必须带到渲染层，
        // 不能一律画成块。
        inject(b"\x1b[5 q"); // 5/6 = 竖线（闪烁/稳定）
        let (.., kind) = term.snapshot().cursor.expect("竖线光标仍是可见光标");
        assert_eq!(kind, CursorKind::Bar, "CSI 5 SP q 应切成竖线");

        inject(b"\x1b[3 q"); // 3/4 = 下划线
        let (.., kind) = term.snapshot().cursor.expect("下划线光标仍是可见光标");
        assert_eq!(kind, CursorKind::Underline, "CSI 3 SP q 应切成下划线");

        kill_remote(&id);
    }

    /// 用户报告：框选后滚动，选区高亮消失。选区跟随滚动是本次重构的核心目标——
    /// 选区存缓冲区绝对坐标，滚动只改 display_offset，snapshot 的逐 cell contains
    /// 判定应该继续命中。直接注入内容+滚动（不经 shell 时序），确定性复现。
    #[test]
    fn selection_survives_scrolling() {
        let dir = std::env::temp_dir().join(format!("smelt-selscroll-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let id = format!("selscroll-test-{}", uuid_like());

        let mut term = Terminal::spawn(24, 80, dir.to_str(), &id, None).expect("spawn 失败");
        thread::sleep(Duration::from_millis(800)); // 等 shell 启动输出沉淀

        // 注入 40 行，把内容顶进 scrollback（24 行屏高）。
        {
            let mut parser: Processor = Processor::new();
            let mut t = term.term.lock().unwrap();
            for i in 0..40 {
                parser.advance(&mut *t, format!("content-{i}\r\n").as_bytes());
            }
        }

        // 在可视区第 5 行选中前 9 列（"content-N" 长度 9）。
        term.selection_start(5, 0, true, SelectionKind::Simple);
        term.selection_update(5, 8, false);
        let text_before = term.selection_text().expect("建完选区应有文本");
        assert!(text_before.starts_with("content-"), "选到的应是注入的内容行，实际: {text_before:?}");
        let frame = term.snapshot();
        let sel_row_before = frame.rows.iter().position(|r| r.iter().any(|c| c.selected));
        assert_eq!(sel_row_before, Some(5), "选区高亮应画在第 5 行");

        // 向上滚 3 行：高亮应跟着内容下移到第 8 行，文本不变。
        term.scroll(3);
        let frame = term.snapshot();
        let sel_row_after = frame.rows.iter().position(|r| r.iter().any(|c| c.selected));
        assert_eq!(sel_row_after, Some(8), "滚动 3 行后选区高亮应跟随内容移到第 8 行");
        assert_eq!(term.selection_text().as_deref(), Some(text_before.as_str()), "滚动不该改变选区文本");

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
            metrics: Arc::new(Mutex::new(TermMetrics {
                rows: 24,
                cols: 80,
                cell_w: 8,
                cell_h: 16,
            })),
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

    /// TextAreaSizeRequest：应用查文本区尺寸时必须按当前 metrics 回应，不能吞掉。
    #[test]
    fn text_area_size_request_gets_answered() {
        let (proxy, mut probe) = make_proxy();
        // 直接发事件（不经 parser）：验证 EventProxy 分支真的写帧。
        use alacritty_terminal::event::WindowSize;
        proxy.send_event(Event::TextAreaSizeRequest(std::sync::Arc::new(|ws: WindowSize| {
            format!("{}x{}@{}x{}", ws.num_cols, ws.num_lines, ws.cell_width, ws.cell_height)
        })));
        let (ty, resp) = read_frame(&mut probe);
        assert_eq!(ty, 0);
        assert_eq!(resp, "80x24@8x16", "应回 metrics 里的 80×24 格、8×16 像素");
    }

    /// Shift+Enter 能不能换行，全押在这个位上：TUI 进入时发 `CSI > 1 u` 开 kitty keyboard
    /// protocol，alacritty 要把它解析成 TermMode::DISAMBIGUATE_ESC_CODES，keystroke_to_bytes
    /// 才会改发 CSI u 编码。退出时发 `CSI < u` 弹栈还原——还原不掉的话，TUI 退出后普通
    /// shell 里按 Shift+Enter 就会被吐出 `[13;2u` 乱码。
    #[test]
    fn kitty_keyboard_protocol_toggles_disambiguate_mode() {
        let (proxy, _probe) = make_proxy();
        let size = TermSize { rows: 24, cols: 80 };
        // 跟 Terminal::spawn 用同一份 config：kitty_keyboard 关着的话 alacritty 会把
        // CSI u 全静默丢掉，这个测试就成了摆设。
        let mut term = Term::new(term_config(), &size, proxy);
        let mut parser: Processor = Processor::new();

        assert!(
            !term.mode().contains(TermMode::DISAMBIGUATE_ESC_CODES),
            "默认不该开着——普通 shell 不认 CSI u"
        );

        parser.advance(&mut term, b"\x1b[>1u"); // Claude Code v2.1+ 启动时发这个
        assert!(
            term.mode().contains(TermMode::DISAMBIGUATE_ESC_CODES),
            "收到 CSI > 1 u 后应置位，否则 Shift+Enter 永远退化成裸 Enter"
        );

        parser.advance(&mut term, b"\x1b[<u"); // 退出时弹栈
        assert!(
            !term.mode().contains(TermMode::DISAMBIGUATE_ESC_CODES),
            "TUI 退出后应还原，否则 shell 里 Shift+Enter 会吐出 `[13;2u` 乱码"
        );
    }
}

#[cfg(test)]
mod paste_encode_tests {
    use super::encode_paste;

    #[test]
    fn plain_paste_normalizes_newlines_to_cr() {
        assert_eq!(encode_paste("a\nb\r\nc", false), b"a\rb\rc");
    }

    #[test]
    fn bracketed_paste_wraps_and_strips_esc() {
        let out = encode_paste("hi\x1b[31m", true);
        assert_eq!(out, b"\x1b[200~hi[31m\x1b[201~");
    }
}

#[cfg(test)]
mod search_resync_tests {
    use super::*;

    /// 搭一个不连守护的 Terminal：假 UnixStream 写端 + 直接构造的 Term。
    /// 专测搜索坐标——不 spawn shell，无时序依赖，不 flaky。
    fn make_terminal(rows: usize, cols: usize) -> Terminal {
        let (_probe, sock) = UnixStream::pair().expect("pair 失败");
        let metrics = Arc::new(Mutex::new(TermMetrics {
            rows: rows as u16,
            cols: cols as u16,
            cell_w: 8,
            cell_h: 16,
        }));
        let proxy = EventProxy {
            notify: Arc::new(Mutex::new(None)),
            title: Arc::new(Mutex::new(None)),
            writer: Arc::new(Mutex::new(sock.try_clone().expect("clone 失败"))),
            metrics: metrics.clone(),
        };
        let term = Term::new(term_config(), &TermSize { rows, cols }, proxy);
        Terminal {
            term: Arc::new(Mutex::new(term)),
            writer: Arc::new(Mutex::new(sock)),
            size: TermSize { rows, cols },
            metrics,
            notify: Arc::new(Mutex::new(None)),
            title: Arc::new(Mutex::new(None)),
            last_damage_cursor: Mutex::new(None),
            search_query: Mutex::new(String::new()),
            search_matches: Mutex::new(Vec::new()),
            search_index: Mutex::new(0),
            // 测试不驱动 UI 重绘，给一个即时关闭的通道占位即可。
            redraw_rx: {
                let (_, rx) = smol::channel::bounded::<()>(1);
                rx
            },
            finished: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 直接往 Term 喂字节（等价于读线程收到 PTY 输出）。
    fn feed(t: &Terminal, bytes: &[u8]) {
        let mut parser: Processor = Processor::new();
        if let Ok(mut term) = t.term.lock() {
            parser.advance(&mut *term, bytes);
        }
    }

    /// 取一条命中在当前视口网格上盖住的实际文本——高亮画在哪，就该是什么字。
    fn hit_text(t: &Terminal, hit: &SearchHit) -> String {
        let term = t.term.lock().unwrap();
        let offset = term.grid().display_offset();
        (hit.col_start..=hit.col_end)
            .map(|c| term.grid()[viewport_to_point(offset, Point::new(hit.row, Column(c)))].c)
            .collect()
    }

    /// 截图 bug 复现：搜索后日志继续滚，缓存的命中坐标整体过期（每滚一行
    /// 全部 Line -1），高亮落在无关文本上（搜 schwab 却高亮 HTTP/1）。
    /// 命中行滚出可视区后就不该再画任何高亮。
    #[test]
    fn stale_hits_vanish_after_scroll() {
        let mut t = make_terminal(4, 20);
        feed(&t, b"one needle here\r\nfill-a\r\nfill-b\r\nfill-c");
        let st = t.set_search_query("needle");
        assert_eq!((st.current, st.total), (1, 1));
        let hits = t.viewport_search_hits();
        assert_eq!(hits.len(), 1);
        assert_eq!(hit_text(&t, &hits[0]), "needle", "滚动前高亮就该在命中文本上");

        // 新输出滚 2 行：needle 行进 scrollback，贴底视口里已没有命中
        feed(&t, b"\r\nnew-1\r\nnew-2");
        let hits = t.viewport_search_hits();
        assert!(
            hits.is_empty(),
            "命中已滚出可视区，不该再画高亮（旧坐标落在 {:?}）",
            hits.first().map(|h| hit_text(&t, h))
        );
    }

    /// 回看历史时高亮必须跟着内容走：滚动后命中的视口位置变了，重算要对准。
    #[test]
    fn hits_track_content_into_scrollback() {
        let mut t = make_terminal(4, 20);
        feed(&t, b"one needle here\r\nfill-a\r\nfill-b\r\nfill-c");
        t.set_search_query("needle");
        feed(&t, b"\r\nnew-1\r\nnew-2");

        t.set_scroll_offset(2); // 回看到最初 4 行，needle 应在视口第 0 行
        let hits = t.viewport_search_hits();
        assert_eq!(hits.len(), 1, "回看后 needle 回到可视区");
        assert_eq!(hit_text(&t, &hits[0]), "needle", "高亮必须落在命中文本上");
        assert_eq!(hits[0].row, 0);
    }

    /// 同一查询按「下一个」：搜索之后新输出里的命中也要被看见。
    #[test]
    fn find_next_picks_up_new_matches() {
        let mut t = make_terminal(4, 20);
        feed(&t, b"one needle here\r\nfill-a");
        let st = t.find_next("needle", false);
        assert_eq!((st.current, st.total), (1, 1));

        feed(&t, b"\r\nsecond needle x");
        let st = t.find_next("needle", false);
        assert_eq!(st.total, 2, "新输出里的命中必须被看见");
        assert_eq!(st.current, 2);
    }
}

#[cfg(test)]
mod mouse_encode_tests {
    use super::encode_mouse;
    use alacritty_terminal::term::TermMode;

    #[test]
    fn sgr_press_release_and_drag() {
        let sgr = TermMode::SGR_MOUSE | TermMode::MOUSE_MODE;
        assert_eq!(encode_mouse(sgr, 0, true, 2, 4), b"\x1b[<0;5;3M");
        assert_eq!(encode_mouse(sgr, 0, false, 2, 4), b"\x1b[<0;5;3m");
        assert_eq!(encode_mouse(sgr, 32, true, 2, 4), b"\x1b[<32;5;3M");
    }
}

#[cfg(test)]
mod search_literal_tests {
    use super::{escape_regex_literal, match_to_viewport_hit, SearchHit};
    use alacritty_terminal::index::{Column, Line, Point};

    #[test]
    fn escapes_regex_metachars_for_literal_search() {
        assert_eq!(escape_regex_literal("a.b*c"), r"a\.b\*c");
        assert_eq!(escape_regex_literal("plain"), "plain");
    }

    #[test]
    fn match_maps_into_viewport_with_offset() {
        // 缓冲 line=-5，offset=5 → 可视第 0 行
        let start = Point::new(Line(-5), Column(3));
        let end = Point::new(Line(-5), Column(7));
        let hit = match_to_viewport_hit(start, end, 5, 80, true).unwrap();
        assert_eq!(
            hit,
            SearchHit {
                row: 0,
                col_start: 3,
                col_end: 7,
                active: true,
            }
        );
        // offset 不够 → 仍在历史上方，不可见
        assert!(match_to_viewport_hit(start, end, 2, 80, false).is_none());
    }
}
