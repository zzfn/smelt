//! 内嵌终端后端：portable-pty 起 shell 子进程 + alacritty_terminal 做终端状态机。
//!
//! 数据流：后台线程读 PTY 输出 → vte 解析器 advance → 更新共享的 Term 网格；
//! UI 线程定时对网格做快照并重绘（见 main.rs 的定时 spawn）。

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::Processor;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};

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
        if let Some(dir) = cwd {
            cmd.cwd(dir);
        }
        cmd.env("TERM", "xterm-256color");
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

        Ok(Self { term, writer, size })
    }

    /// 向 shell 写入字节（键盘输入用）。
    pub fn send_input(&mut self, bytes: &[u8]) {
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
    }

    /// 快照当前可视网格为文本行（先只取字符，颜色留到下一步）。
    pub fn snapshot_lines(&self) -> Vec<String> {
        let term = match self.term.lock() {
            Ok(t) => t,
            Err(_) => return Vec::new(),
        };
        let cols = self.size.cols;
        let mut lines: Vec<String> = Vec::with_capacity(self.size.rows);
        let mut current = String::with_capacity(cols);
        let mut count = 0usize;
        // display_iter 按可视区行主序逐格给出 Indexed<&Cell>
        for indexed in term.grid().display_iter() {
            current.push(indexed.cell.c);
            count += 1;
            if count % cols == 0 {
                lines.push(std::mem::take(&mut current));
            }
        }
        if !current.is_empty() {
            lines.push(current);
        }
        lines
    }
}
