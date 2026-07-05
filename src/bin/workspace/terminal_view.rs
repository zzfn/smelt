//! 单个终端视图：一个 Terminal + 焦点 + IME + 网格渲染 + 键盘/滚轮输入。
//! 多个 TerminalView 由 Workspace 以标签形式管理。

use std::cell::Cell as StdCell;
use std::ops::Range;
use std::rc::Rc;
use std::time::Duration;

use gpui::*;
use smol::Timer;

use crate::terminal::{self, Terminal};

/// 选区高亮背景色。
const SEL_BG: u32 = 0x0033_4a6a;

/// 悬停链接的高亮前景色。
const LINK_FG: u32 = 0x007d_cfff;

/// 终端字体：Nerd Font 的严格等宽变体（含图标/powerline 字形，且单格宽对齐）。
pub const FONT_FAMILY: &str = "JetBrainsMono Nerd Font Mono";

/// 终端网格刷新间隔（后台线程在更新，UI 定时快照重绘）。
const REFRESH: Duration = Duration::from_millis(30);

/// 终端字体与网格度量（渲染与行列计算共用，保持一致）。
pub const FONT_PX: f32 = 13.0;
pub const LINE_PX: f32 = 18.0;
/// 等宽字宽 ≈ 字号 × 该比例（用于从窗口宽度估算列数）。
const CELL_W_RATIO: f32 = 0.6;
/// 估算的边距 / 标签栏高度，用于从窗口尺寸推算终端可用网格区域。
const PAD_X: f32 = 16.0;
const PAD_Y: f32 = 16.0;
const CHROME_H: f32 = 44.0;
/// Shift+PageUp/Down 每次滚动的行数。
const PAGE_LINES: i32 = 20;

/// 一个内嵌终端视图。
pub struct TerminalView {
    terminal: Terminal,
    focus_handle: FocusHandle,
    did_focus: bool,
    /// 输入法合成中的预编辑文本（未提交），仅用于满足 IME 协议，不发给 PTY。
    marked_text: Option<String>,
    title: String,
    /// 初始工作目录（新建标签继承用）。
    cwd: Option<String>,
    /// 鼠标框选：(锚点, 当前端) 的 (行, 列)。
    sel: Option<((usize, usize), (usize, usize))>,
    /// 上次测得的等宽字符像素宽（鼠标坐标换算用）。
    cell_w: f32,
    /// 网格原点（含内边距）的窗口像素坐标，由 canvas 在 paint 时写入。
    grid_origin: Rc<StdCell<(f32, f32)>>,
    /// 当前 Cmd 悬停的链接范围 (行, 起列, 止列)，用于高亮 + 切换鼠标样式。
    hover_url: Option<(usize, usize, usize)>,
}

impl TerminalView {
    pub fn new(cx: &mut Context<Self>, cwd: Option<String>) -> Self {
        let terminal = Terminal::spawn(24, 80, cwd.as_deref()).expect("启动内嵌终端失败");

        // 定时重绘：后台读线程更新 Term 网格，这里每 30ms 通知 UI 刷新。
        cx.spawn(async move |this, cx| loop {
            Timer::after(REFRESH).await;
            if this.update(cx, |_, cx| cx.notify()).is_err() {
                break; // 视图已销毁
            }
        })
        .detach();

        // 标签标题：取工作目录最后一段
        let title = cwd
            .as_deref()
            .and_then(|p| p.trim_end_matches('/').rsplit('/').next())
            .filter(|s| !s.is_empty())
            .unwrap_or("终端")
            .to_string();

        Self {
            terminal,
            focus_handle: cx.focus_handle(),
            did_focus: false,
            marked_text: None,
            title,
            cwd,
            sel: None,
            cell_w: 8.0,
            grid_origin: Rc::new(StdCell::new((0.0, 0.0))),
            hover_url: None,
        }
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn cwd(&self) -> Option<String> {
        self.cwd.clone()
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    /// 窗口像素坐标 → 网格单元 (行, 列)。
    fn pos_to_cell(&self, pos: Point<Pixels>) -> (usize, usize) {
        let (ox, oy) = self.grid_origin.get();
        let x = (f32::from(pos.x) - ox).max(0.0);
        let y = (f32::from(pos.y) - oy).max(0.0);
        let col = (x / self.cell_w.max(1.0)).floor() as usize;
        let row = (y / LINE_PX).floor() as usize;
        (row, col)
    }

    /// 提取当前选区文本（用于复制）。按 (行,列) 字典序规范化。
    fn selected_text(&self) -> Option<String> {
        let (a, b) = self.sel?;
        let (s, e) = if a <= b { (a, b) } else { (b, a) };
        let frame = self.terminal.snapshot();
        if frame.rows.is_empty() {
            return None;
        }
        let last_row = e.0.min(frame.rows.len() - 1);
        let mut out = String::new();
        for r in s.0..=last_row {
            let row = &frame.rows[r];
            if !row.is_empty() {
                let lo = if r == s.0 { s.1 } else { 0 };
                let hi = (if r == e.0 { e.1 } else { row.len() - 1 }).min(row.len() - 1);
                let mut line = String::new();
                if lo <= hi {
                    for c in lo..=hi {
                        line.push(row[c].ch);
                    }
                }
                out.push_str(line.trim_end());
            }
            if r != last_row {
                out.push('\n');
            }
        }
        if out.trim().is_empty() {
            None
        } else {
            Some(out)
        }
    }

    /// 双击选词：以点击单元为中心，向两侧扩展到空白为止。
    fn word_at(&self, (r, c): (usize, usize)) -> Option<((usize, usize), (usize, usize))> {
        let frame = self.terminal.snapshot();
        let row = frame.rows.get(r)?;
        if c >= row.len() || row[c].ch.is_whitespace() {
            return Some(((r, c), (r, c)));
        }
        let mut lo = c;
        while lo > 0 && !row[lo - 1].ch.is_whitespace() {
            lo -= 1;
        }
        let mut hi = c;
        while hi + 1 < row.len() && !row[hi + 1].ch.is_whitespace() {
            hi += 1;
        }
        Some(((r, lo), (r, hi)))
    }

    /// 三击选行：整行到最后一个非空白字符。
    fn line_at(&self, (r, _c): (usize, usize)) -> Option<((usize, usize), (usize, usize))> {
        let frame = self.terminal.snapshot();
        let row = frame.rows.get(r)?;
        let last = row
            .iter()
            .rposition(|cell| !cell.ch.is_whitespace())
            .unwrap_or(0);
        Some(((r, 0), (r, last)))
    }

    /// 点击单元处若落在某个 URL 上，返回该 URL。
    fn url_at(&self, (r, c): (usize, usize)) -> Option<String> {
        let frame = self.terminal.snapshot();
        let row = frame.rows.get(r)?;
        find_urls(row)
            .into_iter()
            .find(|&(a, b, _)| c >= a && c <= b)
            .map(|(_, _, url)| url)
    }

    /// 单元处链接的范围 (行, 起列, 止列)，用于悬停高亮。
    fn link_range_at(&self, (r, c): (usize, usize)) -> Option<(usize, usize, usize)> {
        let frame = self.terminal.snapshot();
        let row = frame.rows.get(r)?;
        find_urls(row)
            .into_iter()
            .find(|&(a, b, _)| c >= a && c <= b)
            .map(|(a, b, _)| (r, a, b))
    }
}

/// 输入法（IME）支持：中文等需要合成的输入走这里，最终提交的文字通过
/// replace_text_in_range 回调进来，写入 PTY。英文/可打印字符同样经此路径。
impl EntityInputHandler for TerminalView {
    fn text_for_range(
        &mut self,
        _range: Range<usize>,
        _adjusted: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        self.marked_text.clone()
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        None
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_text
            .as_ref()
            .map(|s| 0..s.encode_utf16().count())
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_text = None;
    }

    fn replace_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked_text = None;
        if !text.is_empty() {
            self.terminal.send_input(text.as_bytes());
        }
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked_text = if new_text.is_empty() {
            None
        } else {
            Some(new_text.to_string())
        };
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        Some(Bounds {
            origin: element_bounds.origin,
            size: size(px(2.0), px(LINE_PX)),
        })
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 首帧把焦点抢到终端上。
        if !self.did_focus {
            self.did_focus = true;
            window.focus(&self.focus_handle, cx);
        }

        // 依据窗口尺寸重算终端行列，并 resize 网格 + PTY（无变化则内部跳过）。
        {
            let vp = window.viewport_size();
            // 精确测量等宽字符宽度（量一个 'M'）；异常时回退到 0.6 估算。
            let run = TextRun {
                len: 1,
                font: font(FONT_FAMILY),
                color: hsla(0.0, 0.0, 1.0, 1.0),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let measured =
                f32::from(window.text_system().layout_line("M", px(FONT_PX), &[run], None).width);
            let cell_w = if measured > 1.0 {
                measured
            } else {
                FONT_PX * CELL_W_RATIO
            };
            self.cell_w = cell_w; // 供鼠标坐标换算
            let vw = f32::from(vp.width);
            let vh = f32::from(vp.height);
            let cols = (((vw - PAD_X) / cell_w).floor() as usize).max(20);
            let grid_rows = (((vh - CHROME_H - PAD_Y) / LINE_PX).floor() as usize).max(5);
            self.terminal.resize(grid_rows, cols);
        }

        let frame = self.terminal.snapshot();
        let cursor = frame.cursor;
        let sel = self.sel;
        let hover_url = self.hover_url;
        let has_hover = hover_url.is_some();
        let base_font = font(FONT_FAMILY);
        let fh = self.focus_handle.clone();
        let entity = cx.entity();
        let origin_cell = self.grid_origin.clone();

        div()
            .relative()
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(rgb(0x1a1b26))
            .text_color(rgb(0xc0caf5))
            .font_family(FONT_FAMILY)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _window, cx| {
                let ks = &ev.keystroke;
                let m = &ks.modifiers;
                // Cmd+C 复制选区
                if m.platform && ks.key == "c" {
                    if let Some(text) = this.selected_text() {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                    }
                    return;
                }
                // Cmd+V 粘贴：读剪贴板写入 PTY
                if m.platform && ks.key == "v" {
                    if let Some(text) = cx.read_from_clipboard().and_then(|it| it.text()) {
                        this.terminal.send_input(text.as_bytes());
                        cx.notify();
                    }
                    return;
                }
                // Shift+PageUp/Down 滚动历史缓冲
                if m.shift && (ks.key == "pageup" || ks.key == "pagedown") {
                    let delta = if ks.key == "pageup" {
                        PAGE_LINES
                    } else {
                        -PAGE_LINES
                    };
                    this.terminal.scroll(delta);
                    cx.notify();
                    return;
                }
                if let Some(bytes) = keystroke_to_bytes(ks) {
                    this.terminal.send_input(&bytes);
                    cx.notify();
                }
            }))
            .on_scroll_wheel(cx.listener(|this, ev: &ScrollWheelEvent, _window, cx| {
                let lines = match ev.delta {
                    ScrollDelta::Lines(p) => p.y as i32,
                    ScrollDelta::Pixels(p) => (f32::from(p.y) / LINE_PX) as i32,
                };
                if lines != 0 {
                    this.terminal.scroll(lines);
                    cx.notify();
                }
            }))
            // 鼠标框选
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                    window.focus(&this.focus_handle, cx);
                    let cell = this.pos_to_cell(ev.position);
                    // Cmd+点击打开链接
                    if ev.modifiers.platform {
                        if let Some(url) = this.url_at(cell) {
                            cx.open_url(&url);
                            return;
                        }
                    }
                    this.sel = match ev.click_count {
                        2 => this.word_at(cell),         // 双击选词
                        n if n >= 3 => this.line_at(cell), // 三击选行
                        _ => Some((cell, cell)),
                    };
                    cx.notify();
                }),
            )
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _window, cx| {
                if ev.pressed_button == Some(MouseButton::Left) {
                    if let Some((a, _)) = this.sel {
                        let head = this.pos_to_cell(ev.position);
                        this.sel = Some((a, head));
                        cx.notify();
                    }
                } else {
                    // 按住 Cmd 悬停链接：记录链接范围（用于高亮 + 手型）
                    let hl = if ev.modifiers.platform {
                        this.link_range_at(this.pos_to_cell(ev.position))
                    } else {
                        None
                    };
                    if hl != this.hover_url {
                        this.hover_url = hl;
                        cx.notify();
                    }
                }
            }))
            // 按/松 Cmd 时（鼠标不动也）即时更新链接高亮/手型
            .on_modifiers_changed(cx.listener(|this, ev: &ModifiersChangedEvent, window, cx| {
                let hl = if ev.modifiers.platform {
                    this.link_range_at(this.pos_to_cell(window.mouse_position()))
                } else {
                    None
                };
                if hl != this.hover_url {
                    this.hover_url = hl;
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _ev: &MouseUpEvent, _window, cx| {
                    // 单击（锚点==端点，未拖动）清除选区
                    if let Some((a, b)) = this.sel {
                        if a == b {
                            this.sel = None;
                            cx.notify();
                        }
                    }
                }),
            )
            // 终端主体：逐行渲染 alacritty 网格快照（带颜色 / 光标）
            .child(
                div()
                    .flex()
                    .flex_col()
                    .size_full()
                    .text_size(px(FONT_PX))
                    .line_height(px(LINE_PX))
                    .children(frame.rows.into_iter().enumerate().map(move |(r, row)| {
                        let cc = match cursor {
                            Some((cr, cc)) if cr == r => Some(cc),
                            _ => None,
                        };
                        let sr = sel_range_for_row(r, sel, row.len());
                        let hl = match hover_url {
                            Some((hr, a, b)) if hr == r => Some((a, b)),
                            _ => None,
                        };
                        render_row(row, cc, sr, &base_font, hl)
                    })),
            )
            // 透明覆盖层：paint 阶段注册 IME 输入处理器，并记录网格原点。
            .child(
                canvas(
                    // prepaint：建一个覆盖终端区的 hitbox（供设置鼠标样式用）
                    move |bounds, window, _cx| window.insert_hitbox(bounds, HitboxBehavior::Normal),
                    move |bounds, hitbox, window, cx| {
                        // 鼠标样式：悬停链接时手型，否则文本 I-beam
                        window.set_cursor_style(
                            if has_hover {
                                CursorStyle::PointingHand
                            } else {
                                CursorStyle::IBeam
                            },
                            &hitbox,
                        );
                        // 网格原点 = 覆盖层原点（终端主体已去内边距，直接对齐）
                        origin_cell.set((f32::from(bounds.origin.x), f32::from(bounds.origin.y)));
                        window.handle_input(&fh, ElementInputHandler::new(bounds, entity), cx);
                    },
                )
                .absolute()
                .inset_0(),
            )
    }
}

/// 渲染一行：整行作为一个 StyledText，逐段用 TextRun 上色（前景+背景+粗体+下划线）。
/// 整行只整形一次 —— 拖选拆分不抖、宽度精确不截断。光标单元反色、选区单元高亮。
fn render_row(
    row: Vec<terminal::Cell>,
    cursor_col: Option<usize>,
    sel: Option<(usize, usize)>,
    base_font: &Font,
    hover_link: Option<(usize, usize)>,
) -> Div {
    let is_sel = |i: usize| sel.map_or(false, |(lo, hi)| i >= lo && i <= hi);
    let is_link = |i: usize| hover_link.map_or(false, |(a, b)| i >= a && i <= b);
    // 悬停链接：高亮色 + 下划线；再叠加光标反色 / 选区背景。
    let style_of = |i: usize| -> (u32, u32, bool, bool) {
        let c = &row[i];
        let mut fg = c.fg;
        let mut bg = c.bg;
        let mut underline = c.underline;
        if is_link(i) {
            fg = LINK_FG;
            underline = true;
        }
        if Some(i) == cursor_col {
            std::mem::swap(&mut fg, &mut bg);
        } else if is_sel(i) {
            bg = SEL_BG;
        }
        (fg, bg, c.bold, underline)
    };

    let mut line = String::new();
    let mut runs: Vec<TextRun> = Vec::new();
    let mut i = 0;
    while i < row.len() {
        let style = style_of(i);
        let (fg, bg, bold, underline) = style;
        let mut seg_len = 0usize;
        while i < row.len() && style_of(i) == style {
            let ch = row[i].ch;
            line.push(ch);
            seg_len += ch.len_utf8();
            i += 1;
        }
        let mut fnt = base_font.clone();
        if bold {
            fnt.weight = FontWeight::BOLD;
        }
        runs.push(TextRun {
            len: seg_len,
            font: fnt,
            color: Hsla::from(rgb(fg)),
            background_color: Some(Hsla::from(rgb(bg))),
            underline: underline.then(|| UnderlineStyle {
                thickness: px(1.0),
                color: Some(Hsla::from(rgb(fg))),
                wavy: false,
            }),
            strikethrough: None,
        });
    }

    div()
        .h(px(LINE_PX))
        .child(StyledText::new(line).with_runs(runs))
}

/// 计算某行落在选区内的列范围（按 (行,列) 字典序规范化）。
fn sel_range_for_row(
    r: usize,
    sel: Option<((usize, usize), (usize, usize))>,
    row_len: usize,
) -> Option<(usize, usize)> {
    let (a, b) = sel?;
    let (s, e) = if a <= b { (a, b) } else { (b, a) };
    if r < s.0 || r > e.0 || row_len == 0 {
        return None;
    }
    let lo = if r == s.0 { s.1 } else { 0 };
    let hi = (if r == e.0 { e.1 } else { row_len - 1 }).min(row_len - 1);
    if lo > hi {
        None
    } else {
        Some((lo, hi))
    }
}

/// 在一行里找出所有 URL，返回 (起列, 止列含, url)。
fn find_urls(row: &[terminal::Cell]) -> Vec<(usize, usize, String)> {
    let n = row.len();
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        if starts_scheme(row, i) {
            let mut j = i;
            while j < n && is_url_char(row[j].ch) {
                j += 1;
            }
            // 去掉结尾的标点
            let mut end = j;
            while end > i
                && matches!(
                    row[end - 1].ch,
                    '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '"' | '\''
                )
            {
                end -= 1;
            }
            if end - i >= 10 {
                let url: String = (i..end).map(|k| row[k].ch).collect();
                out.push((i, end - 1, url));
            }
            i = end.max(i + 1);
        } else {
            i += 1;
        }
    }
    out
}

/// 判断第 i 列起是否是 http:// 或 https://。
fn starts_scheme(row: &[terminal::Cell], i: usize) -> bool {
    let at = |pat: &str| {
        let pc: Vec<char> = pat.chars().collect();
        i + pc.len() <= row.len() && (0..pc.len()).all(|k| row[i + k].ch == pc[k])
    };
    at("http://") || at("https://")
}

fn is_url_char(c: char) -> bool {
    !c.is_whitespace() && !matches!(c, '<' | '>' | '"' | '`' | '|' | '{' | '}' | '^')
}

/// 把一次「非文本按键」转成写给 PTY 的字节：特殊键和 Ctrl 组合。
/// 可打印字符与空格走 IME 的 replace_text_in_range，不在这里处理。
fn keystroke_to_bytes(ks: &Keystroke) -> Option<Vec<u8>> {
    let m = &ks.modifiers;

    if m.platform {
        return None;
    }

    let named: Option<&[u8]> = match ks.key.as_str() {
        "enter" => Some(b"\r"),
        "backspace" => Some(b"\x7f"),
        "tab" => Some(b"\t"),
        "escape" => Some(b"\x1b"),
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

    if m.control && ks.key.len() == 1 {
        let c = ks.key.as_bytes()[0];
        if c.is_ascii_alphabetic() {
            return Some(vec![c.to_ascii_lowercase() - b'a' + 1]);
        }
    }

    None
}
