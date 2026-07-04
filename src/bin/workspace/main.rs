//! smelt 工作台 —— 基于 gpui-component 的桌面窗口。
//!
//! 本轮：内嵌真终端（portable-pty + alacritty_terminal），起你的 shell、
//! 实时渲染输出、并支持键盘输入。颜色 / 多标签 / resize 留到后续增量。
//!
//! 运行： cargo run --bin workspace

mod terminal;

use std::time::Duration;

use gpui::*;
use gpui_component::*;
use smol::Timer;
use terminal::Terminal;

/// 终端网格刷新间隔（后台线程在更新，UI 定时快照重绘）。
const REFRESH: Duration = Duration::from_millis(30);

/// 工作台根视图：一个内嵌终端。
struct Workspace {
    terminal: Terminal,
    focus_handle: FocusHandle,
    did_focus: bool,
}

impl Workspace {
    fn new(cx: &mut Context<Self>) -> Self {
        let cwd = std::env::current_dir()
            .ok()
            .and_then(|p| p.to_str().map(String::from));
        let terminal = Terminal::spawn(24, 80, cwd.as_deref()).expect("启动内嵌终端失败");

        // 定时重绘：后台读线程更新 Term 网格，这里每 30ms 通知 UI 刷新。
        cx.spawn(async move |this, cx| loop {
            Timer::after(REFRESH).await;
            if this.update(cx, |_, cx| cx.notify()).is_err() {
                break; // 视图已销毁
            }
        })
        .detach();

        Self {
            terminal,
            focus_handle: cx.focus_handle(),
            did_focus: false,
        }
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 首帧把焦点抢到终端上，之后不再干预（用户可自由切换焦点）。
        if !self.did_focus {
            self.did_focus = true;
            window.focus(&self.focus_handle, cx);
        }

        let lines = self.terminal.snapshot_lines();

        div()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _window, cx| {
                if let Some(bytes) = keystroke_to_bytes(&ev.keystroke) {
                    this.terminal.send_input(&bytes);
                    cx.notify();
                }
            }))
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(0x1a1b26))
            .text_color(rgb(0xc0caf5))
            .font_family("monospace")
            .text_sm()
            // 顶部标题栏：mac 三色圆点 + 标题
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_4()
                    .py_2()
                    .bg(rgb(0x16161e))
                    .child(dot(0xff5f56))
                    .child(dot(0xffbd2e))
                    .child(dot(0x27c93f))
                    .child(
                        div()
                            .ml_2()
                            .text_color(rgb(0x7aa2f7))
                            .child("smelt workspace · terminal"),
                    ),
            )
            // 终端主体：逐行渲染 alacritty 网格快照
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .p_2()
                    .children(lines.into_iter().map(|line| div().child(line))),
            )
    }
}

/// 把一次按键转成写给 PTY 的字节。返回 None 表示不发送（如 Cmd 快捷键）。
fn keystroke_to_bytes(ks: &Keystroke) -> Option<Vec<u8>> {
    let m = &ks.modifiers;

    // Cmd（platform）组合留给应用级快捷键，不进终端。
    if m.platform {
        return None;
    }

    // 特殊键 → 终端控制序列
    let named: Option<&[u8]> = match ks.key.as_str() {
        "enter" => Some(b"\r"),
        "backspace" => Some(b"\x7f"),
        "tab" => Some(b"\t"),
        "escape" => Some(b"\x1b"),
        "space" => Some(b" "),
        "left" => Some(b"\x1b[D"),
        "right" => Some(b"\x1b[C"),
        "up" => Some(b"\x1b[A"),
        "down" => Some(b"\x1b[B"),
        "home" => Some(b"\x1b[H"),
        "end" => Some(b"\x1b[F"),
        "delete" => Some(b"\x1b[3~"),
        "pageup" => Some(b"\x1b[5~"),
        "pagedown" => Some(b"\x1b[6~"),
        _ => None,
    };
    if let Some(bytes) = named {
        return Some(bytes.to_vec());
    }

    // Ctrl + 字母 → 控制字节（ctrl-c=0x03, ctrl-d=0x04 ...）
    if m.control && ks.key.len() == 1 {
        let c = ks.key.as_bytes()[0];
        if c.is_ascii_alphabetic() {
            return Some(vec![c.to_ascii_lowercase() - b'a' + 1]);
        }
    }

    // 可打印字符（含 shift / unicode）：用 key_char，Alt 作 Meta 前缀。
    if let Some(kc) = &ks.key_char {
        if !kc.is_empty() {
            let mut bytes = Vec::new();
            if m.alt {
                bytes.push(0x1b);
            }
            bytes.extend_from_slice(kc.as_bytes());
            return Some(bytes);
        }
    }

    None
}

/// 标题栏上的一个 mac 风格圆点。
fn dot(color: u32) -> impl IntoElement {
    div().w(px(12.)).h(px(12.)).rounded_full().bg(rgb(color))
}

fn main() {
    gpui_platform::application().run(move |cx| {
        // 用任何 gpui-component 功能前必须先初始化。
        gpui_component::init(cx);

        cx.spawn(async move |cx| {
            cx.open_window(WindowOptions::default(), |window, cx| {
                let view = cx.new(|cx| Workspace::new(cx));
                // 顶层视图必须包一层 Root（组件库的主题/遮罩系统要求）。
                cx.new(|cx| Root::new(view, window, cx))
            })
            .expect("打开窗口失败");
        })
        .detach();
    });
}
