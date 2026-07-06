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
use alacritty_terminal::term::{Config, Term};
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

/// 事件代理：alacritty 需要一个 EventListener；这里先忽略事件（重绘走 UI 定时快照）。
#[derive(Clone)]
struct EventProxy;

impl EventListener for EventProxy {
    fn send_event(&self, _event: Event) {}
}

/// 一个内嵌终端：alacritty 的 Term（后台线程写、UI 线程读）+ PTY 写端。
pub struct Terminal {
    term: Arc<Mutex<Term<EventProxy>>>,
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    size: TermSize,
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
        // UTF-8 locale（对齐 Zed terminal 的做法）：双击 .app 启动时系统环境极简、
        // 没有 LANG，zsh 会落到 C/POSIX locale，把 starship 输出的多字节 UTF-8 续字节
        // 当成一个个 C1 控制符转义成 <009a> 之类，满屏乱码。父环境没设 LANG 才补
        // en_US.UTF-8（尊重用户已设的 locale），保证 .app 双击也能正常显示中文/图标。
        if std::env::var("LANG").is_err() {
            cmd.env("LANG", "en_US.UTF-8");
        }
        let _child = pair.slave.spawn_command(cmd)?;

        // 2) alacritty 终端状态机
        let term = Term::new(Config::default(), &size, EventProxy);
        let term = Arc::new(Mutex::new(term));

        // 3) 后台读线程：PTY 输出 → vte 解析 → 更新 Term 网格
        let mut reader = pair.master.try_clone_reader()?;
        let term_reader = Arc::clone(&term);
        thread::spawn(move || {
            // Processor<T = StdSyncHandler>：默认类型参数不参与 ::new() 推断，需显式标注。
            let mut parser: Processor = Processor::new();
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break, // EOF：shell 退出
                    Ok(n) => {
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
        })
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

    /// 上下滚动历史缓冲：正数向上翻看历史，负数向下。
    pub fn scroll(&mut self, lines: i32) {
        if let Ok(mut term) = self.term.lock() {
            term.scroll_display(Scroll::Delta(lines));
        }
    }
}
