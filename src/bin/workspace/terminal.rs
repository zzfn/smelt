//! 内嵌终端前端：连接 smeltd 守护进程拿字节流 + alacritty_terminal 做终端状态机。
//!
//! PTY 与 shell 活在 smeltd 里（GUI 退出不杀会话，重开按 id 重连并重放恢复画面，
//! 类 tmux；协议见 src/bin/smeltd.rs 头注释）。数据流：后台线程读守护 socket →
//! vte 解析器 advance → 更新共享的 Term 网格；UI 线程定时对网格做快照并重绘。

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermDamage, TermMode};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor};

/// 默认前景 / 背景色（前景取 iTerm2 风格灰白，正文不再偏紫）。
pub const DEFAULT_FG: u32 = 0x00d8_d8d8;
pub const DEFAULT_BG: u32 = 0x001a_1b26;

/// 16 色 ANSI 调色板。彩色沿用 Tokyo Night，白/亮白改为灰白/纯白（iTerm2 风格）。
const PALETTE: [u32; 16] = [
    0x0015_161e, 0x00f7_768e, 0x009e_ce6a, 0x00e0_af68, 0x007a_a2f7, 0x00bb_9af7, 0x007d_cfff,
    0x00c7_c7c7, 0x0041_4868, 0x00f7_768e, 0x009e_ce6a, 0x00e0_af68, 0x007a_a2f7, 0x00bb_9af7,
    0x007d_cfff, 0x00ff_ffff,
];

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
    match n {
        Black => PALETTE[0],
        Red => PALETTE[1],
        Green => PALETTE[2],
        Yellow => PALETTE[3],
        Blue => PALETTE[4],
        Magenta => PALETTE[5],
        Cyan => PALETTE[6],
        White => PALETTE[7],
        BrightBlack => PALETTE[8],
        BrightRed => PALETTE[9],
        BrightGreen => PALETTE[10],
        BrightYellow => PALETTE[11],
        BrightBlue => PALETTE[12],
        BrightMagenta => PALETTE[13],
        BrightCyan => PALETTE[14],
        BrightWhite => PALETTE[15],
        Background => DEFAULT_BG,
        // Foreground / Cursor / Dim* / 未来新增变体统一回落到默认色
        _ => {
            if is_fg {
                DEFAULT_FG
            } else {
                DEFAULT_BG
            }
        }
    }
}

/// xterm 256 色索引 → rgb：0-15 用调色板，16-231 为 6×6×6 色立方，232-255 为灰阶。
fn indexed_rgb(i: u8) -> u32 {
    match i {
        0..=15 => PALETTE[i as usize],
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
/// 其余事件暂忽略（重绘走 UI 定时快照）。
#[derive(Clone)]
struct EventProxy {
    notify: NotifySlot,
    /// 终端标题（OSC 0/2）——Claude Code 用它实时报告「在干嘛」（任务名 + 状态符号）。
    title: Arc<Mutex<Option<String>>>,
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
    writer: UnixStream,
    size: TermSize,
    /// 通知消息槽（响铃 / OSC 9 写入，UI 轮询 take_notification 取走）。
    notify: NotifySlot,
    /// 终端标题（agent 实时状态；UI 读 current_title 用于通知 / 总览）。
    title: Arc<Mutex<Option<String>>>,
}

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
        // 1) 连守护（不在则自动拉起）并声明要打开的会话
        let mut writer = connect_daemon()?;
        writeln!(
            writer,
            "{}",
            serde_json::json!({ "op": "open", "id": id, "cwd": cwd, "cols": cols, "rows": rows, "launch": launch })
        )?;

        // 守护先回报 PTY 当前尺寸（reattach 时是断开前的实际尺寸）：本地终端必须建成
        // 同尺寸再解析重放字节，否则行宽错位（zsh 行尾 % 盖不掉、Claude Code 布局撕裂）。
        // 之后 GUI 布局就绪会正常 resize 到新尺寸，alacritty reflow 会重排。
        // 注意：重放字节可能已被 BufReader 预读，读线程必须复用它，不能再从裸流读。
        let mut buffered = BufReader::new(writer.try_clone()?);
        let mut line = String::new();
        buffered.read_line(&mut line)?;
        let v: serde_json::Value = serde_json::from_str(line.trim())?;
        let size = TermSize {
            rows: v["rows"].as_u64().unwrap_or(rows as u64) as usize,
            cols: v["cols"].as_u64().unwrap_or(cols as u64) as usize,
        };

        // 2) alacritty 终端状态机（EventProxy 把响铃 / 标题写入共享槽）
        let notify: NotifySlot = Arc::new(Mutex::new(None));
        let title: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let term = Term::new(
            Config::default(),
            &size,
            EventProxy { notify: notify.clone(), title: title.clone() },
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
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break, // EOF：shell 退出
                    Ok(n) => {
                        // OSC 9/777 通知：alacritty 不解析，自己扫字节提取
                        for &b in &buf[..n] {
                            osc.feed(b, &notify_reader);
                        }
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
        write_frame(&mut self.writer, 1, &payload);
    }

    /// 向 shell 写入字节（键盘输入用）：帧转发给守护。
    pub fn send_input(&mut self, bytes: &[u8]) {
        write_frame(&mut self.writer, 0, bytes);
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
