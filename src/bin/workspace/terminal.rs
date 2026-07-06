//! 内嵌终端后端：portable-pty 起 shell 子进程 + alacritty_terminal 做终端状态机。
//!
//! 数据流：后台线程读 PTY 输出 → vte 解析器 advance → 更新共享的 Term 网格；
//! UI 线程定时对网格做快照并重绘（见 main.rs 的定时 spawn）。

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};

/// 默认前景 / 背景色（与窗口底色一致，Tokyo Night 风格）。
const DEFAULT_FG: u32 = 0x00c0_caf5;
pub const DEFAULT_BG: u32 = 0x001a_1b26;

/// 16 色 ANSI 调色板（Tokyo Night）。索引 0-7 常规、8-15 明亮。
const PALETTE: [u32; 16] = [
    0x0015_161e, 0x00f7_768e, 0x009e_ce6a, 0x00e0_af68, 0x007a_a2f7, 0x00bb_9af7, 0x007d_cfff,
    0x00a9_b1d6, 0x0041_4868, 0x00f7_768e, 0x009e_ce6a, 0x00e0_af68, 0x007a_a2f7, 0x00bb_9af7,
    0x007d_cfff, 0x00c0_caf5,
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

/// 一个内嵌终端：alacritty 的 Term（后台线程写、UI 线程读）+ PTY 写端。
pub struct Terminal {
    term: Arc<Mutex<Term<EventProxy>>>,
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    size: TermSize,
    /// 通知消息槽（响铃 / OSC 9 写入，UI 轮询 take_notification 取走）。
    notify: NotifySlot,
    /// 终端标题（agent 实时状态；UI 读 current_title 用于通知 / 总览）。
    title: Arc<Mutex<Option<String>>>,
}

impl Terminal {
    /// 起一个 shell（$SHELL，默认 /bin/zsh），工作目录 cwd，网格尺寸 rows×cols。
    pub fn spawn(rows: usize, cols: usize, cwd: Option<&str>) -> anyhow::Result<Self> {
        let size = TermSize { rows, cols };

        // 1) 开 PTY 并起 shell 子进程
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows: rows as u16,
            cols: cols as u16,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
        let mut cmd = CommandBuilder::new(shell);
        // login shell（-l）：读 ~/.zprofile 拿到完整 PATH。打包成 .app 双击启动时
        // 系统给的 PATH 很精简，不走 login 就找不到 homebrew 里的命令（如 starship）。
        cmd.arg("-l");
        if let Some(dir) = cwd {
            cmd.cwd(dir);
        }
        cmd.env("TERM", "xterm-256color");
        // 伪装成 iTerm2：Claude Code 的 auto 通知渠道靠 TERM_PROGRAM 识别终端，认出
        // iTerm2 就自动发 OSC 9 通知（我们已支持捕获）→ 用户零配置即可收到「agent 需要
        // 注意」。选 iTerm2 而非 Ghostty/Kitty：它不用 kitty 键盘协议（CSI u），不干扰
        // 按键输入，副作用最小。TERM 仍保持 xterm-256color，不启用 iTerm 私有 terminfo。
        cmd.env("TERM_PROGRAM", "iTerm.app");
        cmd.env("TERM_PROGRAM_VERSION", "3.5.0");
        // UTF-8 locale（对齐 Zed terminal 的做法）：双击 .app 启动时系统环境极简、
        // 没有 LANG，zsh 会落到 C/POSIX locale，把 starship 输出的多字节 UTF-8 续字节
        // 当成一个个 C1 控制符转义成 <009a> 之类，满屏乱码。父环境没设 LANG 才补
        // en_US.UTF-8（尊重用户已设的 locale），保证 .app 双击也能正常显示中文/图标。
        if std::env::var("LANG").is_err() {
            cmd.env("LANG", "en_US.UTF-8");
        }
        let _child = pair.slave.spawn_command(cmd)?;

        // 2) alacritty 终端状态机（EventProxy 把响铃 / 标题写入共享槽）
        let notify: NotifySlot = Arc::new(Mutex::new(None));
        let title: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let term = Term::new(
            Config::default(),
            &size,
            EventProxy { notify: notify.clone(), title: title.clone() },
        );
        let term = Arc::new(Mutex::new(term));

        // 3) 后台读线程：PTY 输出 → vte 解析更新 Term 网格 + 自己扫 OSC 9/777 通知
        let mut reader = pair.master.try_clone_reader()?;
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

        let writer = pair.master.take_writer()?;
        let master = pair.master; // 保留 master 用于 resize

        Ok(Self {
            term,
            writer,
            master,
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

    /// 按新行列 resize：同步 alacritty 网格与底层 PTY。无变化则跳过。
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
        let _ = self.master.resize(PtySize {
            rows: rows as u16,
            cols: cols as u16,
            pixel_width: 0,
            pixel_height: 0,
        });
    }

    /// 向 shell 写入字节（键盘输入用）。
    pub fn send_input(&mut self, bytes: &[u8]) {
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
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
