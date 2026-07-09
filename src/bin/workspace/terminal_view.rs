//! 单个终端视图：一个 Terminal + 焦点 + IME + 网格渲染 + 键盘/滚轮输入。
//! 多个 TerminalView 由 Workspace 以标签形式管理。

use std::cell::Cell as StdCell;
use std::ops::Range;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::*;
use smol::Timer;

use crate::terminal::{self, Terminal};

/// 选区高亮背景色。
const SEL_BG: u32 = 0x0033_4a6a;

/// 悬停链接的高亮前景色。
const LINK_FG: u32 = 0x007d_cfff;

/// 终端字体：Nerd Font 的严格等宽变体（含图标/powerline 字形，且单格宽对齐）。
pub const FONT_FAMILY: &str = "JetBrainsMono Nerd Font Mono";

/// 图标 fallback：内嵌的 Nerd Font Symbols-only 字体（见 main.rs 里的 add_fonts）。
/// FONT_FAMILY 本身查不到的码位（比如装的是非 Nerd Font 版本、或某些较新图标）会落到这里，
/// 不必强求用户机器上装的必须是打了 Nerd Font 补丁的那个变体。
const SYMBOLS_FALLBACK_FONT: &str = "Symbols Nerd Font Mono";

/// 终端字体（主字体 + 图标 fallback）。渲染和测量都应该用这个，保持字形来源一致——
/// 否则测量用的字体和实际渲染用的字体对某个字符的 fallback 结果不一样，会导致
/// 列宽计算和实际显示对不上（拖选/鼠标定位跑偏）。
fn terminal_font() -> Font {
    Font {
        fallbacks: Some(FontFallbacks::from_fonts(vec![SYMBOLS_FALLBACK_FONT.to_string()])),
        ..font(FONT_FAMILY)
    }
}

/// 终端网格刷新间隔（后台线程在更新，UI 定时快照重绘）。
const REFRESH: Duration = Duration::from_millis(30);

/// 终端字体与网格度量（渲染与行列计算共用，保持一致）。
pub const FONT_PX: f32 = 13.0;
pub const LINE_PX: f32 = 18.0;
/// 等宽字宽 ≈ 字号 × 该比例（用于从窗口宽度估算列数）。
const CELL_W_RATIO: f32 = 0.6;
/// 终端内容的每侧内边距（避免文字贴边被裁）。canvas 覆盖层保持满尺寸，
/// 只把网格原点按此偏移，故鼠标/IME 坐标一致，网格可用区 = 尺寸 − 2×PAD。
const PAD_X: f32 = 12.0;
const PAD_Y: f32 = 8.0;
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
    /// 终端自身像素尺寸 (宽, 高)，由 canvas 写入；按卡片大小算行列（网格 Hub 用）。
    grid_size: Rc<StdCell<(f32, f32)>>,
    /// 当前 Cmd 悬停的链接范围 (行, 起列, 止列)，用于高亮 + 切换鼠标样式。
    hover_url: Option<(usize, usize, usize)>,
    /// 最近一帧的光标位置 (行, 列)，供 IME 定位候选窗（bounds_for_range）。
    cursor: Option<(usize, usize)>,
    /// 「需要注意」通知消息：响铃 / OSC 9 上报且尚未被查看（供侧栏蓝点 / pane 蓝环 /
    /// 通知面板）；None = 无待处理通知。
    notification: Option<String>,
    /// 最近收到通知的时刻（总览页显示「N 分钟前」）。
    notified_at: Option<Instant>,
    /// 上一帧该终端是否在「运行中」（标题以 braille spinner 开头）；用于检测完成边沿。
    was_running: bool,
    /// 已连续运行的帧数（REFRESH 为单位）；超阈值判为「卡住」，提醒一次。
    running_frames: u32,
    /// 是否已就「卡住」提醒过（同一段运行只提醒一次）。
    stuck_notified: bool,
    /// 守护里的会话 id（持久化到 workspace.json；重开 GUI 按它 reattach）。
    session_id: String,
    /// 「任务完成未读」：Running→Idle 边沿置位，用户输入回应后清。
    /// 与 notification 分开：完成 ≠ 需要处理，只是提示「有结果可以看了」。
    completed_unread: bool,
    /// 最近一次系统通知的 (文本, 时刻)：同文本 60s 内不重发（Claude Code 会反复
    /// 上报 waiting for input，不去重就是通知轰炸）。
    last_notified: Option<(String, Instant)>,
    /// 最近一次比较过的外观设置：定时刷新时用于判断"背景色/图/透明度/模糊"是否被
    /// 改过（这些跟 PTY 内容无关，Terminal::take_damage 感知不到）。
    /// None = 还没比较过，首次一律当作"变了"以确保能显示当前外观。
    last_appearance: Option<crate::Appearance>,
    /// 触控板滚轮的像素余数：触控板每帧只送几像素的增量，若逐事件独立按
    /// LINE_PX 取整会把大部分小增量截断成 0（滚了但没反应），造成"很不跟手"
    /// 的卡顿感。改为跨事件累加像素，攒够一整行再吐出、余数留到下次。
    scroll_accum: f32,
}

/// 外观设置里跟终端渲染相关的字段是否发生变化（bg_color/bg_image/opacity/blur）。
/// Appearance 未 derive PartialEq，故手动比较这几个字段。
fn appearance_changed(a: &crate::Appearance, b: &crate::Appearance) -> bool {
    a.bg_color != b.bg_color || a.bg_image != b.bg_image || a.opacity != b.opacity || a.blur != b.blur
}

/// 「需要注意」的细分：等审批（红，最高优先）> 其他需要处理（等输入 / 响铃等，橙）。
/// 借鉴 codex 的 ThreadActiveFlag 设计——审批和一般等待是不同等级的行动召唤。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AttentionKind {
    /// Claude 等你批准某个操作（文本含 permission / approv / 权限 / 批准）。
    Approval,
    /// 其他需要处理（等输入、响铃、自定义通知）。
    Attention,
}

/// 从通知文本推断注意力等级。
fn classify_attention(msg: &str) -> AttentionKind {
    let m = msg.to_lowercase();
    if m.contains("permission") || m.contains("approv") || m.contains("权限") || m.contains("批准")
    {
        AttentionKind::Approval
    } else {
        AttentionKind::Attention
    }
}

/// 同一终端同文本的系统通知最小间隔。
const NOTIFY_DEDUP: Duration = Duration::from_secs(60);

/// 标题是否以 braille spinner（U+2801–U+28FF）开头 —— 与 Session::status 的 Running 判定一致。
fn title_is_running(title: Option<String>) -> bool {
    title
        .and_then(|t| t.chars().next())
        .map(|c| ('\u{2801}'..='\u{28FF}').contains(&c))
        .unwrap_or(false)
}

/// 「卡住」阈值：REFRESH≈30ms → ~33fps，约 8 分钟。
const STUCK_FRAMES: u32 = 8 * 60 * 1000 / 30;

impl TerminalView {
    pub fn new(
        cx: &mut Context<Self>,
        cwd: Option<String>,
        session_id: String,
        launch: Option<&str>,
    ) -> Self {
        let terminal = Terminal::spawn(24, 80, cwd.as_deref(), &session_id, launch)
            .expect("启动内嵌终端失败");

        // 定时重绘：后台读线程更新 Term 网格，这里每 30ms 通知 UI 刷新。
        // 顺便检查响铃：非活动会话也在跑此循环，故能在后台标记「需要注意」。
        cx.spawn(async move |this, cx| loop {
            Timer::after(REFRESH).await;
            let r = this.update(cx, |this, cx| {
                if let Some(msg) = this.terminal.take_notification() {
                    let task = this.terminal.current_title();
                    // 焦点感知（借鉴 codex）：app 在前台时不弹系统通知——你自己看得见
                    // 蓝点/徽章，弹了是打扰；切走了才提醒。cx.active_window() 在 app
                    // 失活时为 None（宠物窗是 NonactivatingPanel，不参与）。
                    // 同文本 60s 去重：Claude Code 会反复上报同一条 waiting。
                    let now = Instant::now();
                    let dup = this
                        .last_notified
                        .as_ref()
                        .is_some_and(|(m, t)| *m == msg && now.duration_since(*t) < NOTIFY_DEDUP);
                    if cx.active_window().is_none() && !dup {
                        system_notify(&this.title, task.as_deref(), &msg);
                        this.last_notified = Some((msg.clone(), now));
                    }
                    // 宠物播报照常（应用内的轻提示，不算系统级打扰；宠物自己有气泡节流）。
                    let line = match task.as_deref() {
                        Some(t) if !t.is_empty() => format!("「{t}」{msg}"),
                        _ => msg.clone(),
                    };
                    crate::pet::push_pet_message(cx, line);
                    this.notification = Some(msg);
                    this.notified_at = Some(Instant::now());
                }

                // 运行状态边沿检测（标题 spinner）：完成提醒 + 卡住提醒。
                let running = title_is_running(this.terminal.current_title());
                let name = this.title.clone();
                if this.was_running && !running {
                    // Running → Idle：该会话的 agent 干完了 → 标「完成未读」（总览绿标）。
                    this.completed_unread = true;
                    crate::pet::push_pet_message(cx, format!("「{name}」任务完成啦，来看看结果吧"));
                }
                if running {
                    this.running_frames += 1;
                    if this.running_frames == STUCK_FRAMES && !this.stuck_notified {
                        this.stuck_notified = true;
                        crate::pet::push_pet_message(
                            cx,
                            format!("「{name}」已经跑了好久，要不去瞅一眼？"),
                        );
                    }
                } else {
                    this.running_frames = 0;
                    this.stuck_notified = false;
                }
                this.was_running = running;

                // P0 性能修复：这句 notify() 以前无条件调用，导致哪怕 shell 完全空闲
                // 也在以 33 次/秒的频率触发 render() 里"整个网格快照 + 每行重新整形
                // 文字"的重活。现在先问 alacritty 自带的 damage tracking——终端内容
                // （字符/颜色/光标/翻滚/进出备用屏幕/resize）没有真的变化，就跳过。
                // 外观设置（背景色/图/透明度/模糊）单独比较，因为这些跟 PTY 内容无关、
                // damage tracking 感知不到。拖选高亮 / Cmd 悬停链接不受影响：它们各自
                // 的鼠标事件处理里已经各自调用过 cx.notify()，跟这里无关。
                let content_changed = this.terminal.take_damage();
                let ap_now = cx.global::<crate::Appearance>().clone();
                let ap_changed = match &this.last_appearance {
                    Some(prev) => appearance_changed(prev, &ap_now),
                    None => true,
                };
                if ap_changed {
                    this.last_appearance = Some(ap_now);
                }
                if content_changed || ap_changed {
                    cx.notify();
                }
            });
            if r.is_err() {
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
            grid_size: Rc::new(StdCell::new((0.0, 0.0))),
            hover_url: None,
            cursor: None,
            notification: None,
            notified_at: None,
            was_running: false,
            running_frames: 0,
            stuck_notified: false,
            session_id,
            completed_unread: false,
            last_notified: None,
            last_appearance: None,
            scroll_accum: 0.0,
        }
    }

    /// 当前注意力等级：有待处理通知时按文本分类（等审批 > 一般注意）。
    pub fn attention_kind(&self) -> Option<AttentionKind> {
        self.notification.as_deref().map(classify_attention)
    }

    /// 是否「任务完成未读」（Running→Idle 后用户还没回应过）。
    pub fn completed_unread(&self) -> bool {
        self.completed_unread
    }

    /// 守护里的会话 id（关 pane 时用它让守护真正杀掉 shell）。
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// 守护整个重启后（旧会话随守护进程一起死掉，见 `terminal::restart_daemon`），
    /// 换一个全新会话顶替冻结的旧连接——同 id 在全新守护里查无此会话，走 `handle_open`
    /// 的新建分支，等效于重开一个终端。旧网格尺寸不丢：grid_size 仍是上次量到的值，
    /// 下一帧 render() 会照常把新终端 resize 到位，用户侧只是内容被清空重开。
    /// 连不上守护就原地不动（仍是冻结的旧终端），不 panic。
    pub fn reconnect(&mut self, cx: &mut Context<Self>) {
        let Ok(terminal) = Terminal::spawn(24, 80, self.cwd.as_deref(), &self.session_id, None)
        else {
            return;
        };
        self.terminal = terminal;
        self.notification = None;
        self.notified_at = None;
        self.was_running = false;
        self.running_frames = 0;
        self.stuck_notified = false;
        self.completed_unread = false;
        self.last_notified = None;
        self.sel = None;
        self.cursor = None;
        cx.notify();
    }

    /// 最近通知时刻（总览页「N 分钟前」用）。
    pub fn notified_at(&self) -> Option<Instant> {
        self.notified_at
    }


    /// 终端末尾最多 n 行非空文本（总览页迷你预览用）。
    pub fn last_lines(&self, n: usize) -> Vec<String> {
        let frame = self.terminal.snapshot();
        let mut lines: Vec<String> = frame
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .filter(|c| c.ch != '\0')
                    .map(|c| c.ch)
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect();
        while lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        let start = lines.len().saturating_sub(n);
        lines[start..].to_vec()
    }

    /// 是否有待处理通知（agent 需要注意）。
    pub fn has_attention(&self) -> bool {
        self.notification.is_some()
    }

    /// 通知消息文本（供通知面板显示）。
    pub fn notification(&self) -> Option<&str> {
        self.notification.as_deref()
    }

    /// agent 报告的终端标题（含任务名 + 状态符号）；供侧栏 / 总览显示。
    pub fn agent_title(&self) -> Option<String> {
        self.terminal.current_title()
    }


    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn cwd(&self) -> Option<String> {
        self.cwd.clone()
    }

    /// 从外部写一段文本到 PTY（等价于粘贴），供 diff 视图「发到终端」等场景复用。
    pub fn send_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.terminal.send_input(text.as_bytes());
        self.notification = None;
        self.completed_unread = false;
        cx.notify();
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    /// 窗口像素坐标 → 网格单元 (行, 列)。
    /// 列号不能用均匀格宽 x/cell_w：中文等全角字符走系统回退字体（PingFang），
    /// 实际字宽 ≠ 2×cell_w，行内中文越多偏差越大 → 框选高亮落后鼠标一大截。
    /// 改为把该行文本按渲染同款方式整形，用 x 反查字符位置再映射回网格列。
    fn pos_to_cell(&self, pos: Point<Pixels>, window: &mut Window) -> (usize, usize) {
        let (ox, oy) = self.grid_origin.get();
        let x = (f32::from(pos.x) - ox).max(0.0);
        let y = (f32::from(pos.y) - oy).max(0.0);
        let row = (y / LINE_PX).floor() as usize;
        (row, self.col_for_x(row, x, window))
    }

    /// 视觉 x 偏移（相对网格原点）→ 该行的网格列。
    fn col_for_x(&self, row_ix: usize, x: f32, window: &mut Window) -> usize {
        let uniform = || (x / self.cell_w.max(1.0)).floor() as usize;
        let frame = self.terminal.snapshot();
        let Some(cells) = frame.rows.get(row_ix) else {
            return uniform();
        };
        // 与 render_row 同规则构造行文本（'\0' 占位跳过），记录 字节偏移 → 网格列。
        let mut line = String::new();
        let mut byte_to_col: Vec<(usize, usize)> = Vec::new();
        for (col, cell) in cells.iter().enumerate() {
            if cell.ch != '\0' {
                byte_to_col.push((line.len(), col));
                line.push(cell.ch);
            }
        }
        if line.is_empty() {
            return uniform();
        }
        let run = TextRun {
            len: line.len(),
            font: terminal_font(),
            color: Hsla::default(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let layout = window.text_system().layout_line(&line, px(FONT_PX), &[run], None);
        match layout.index_for_x(px(x)) {
            // x 落在某个字形内 → 反查其字节偏移对应的网格列。
            Some(ix) => match byte_to_col.binary_search_by_key(&ix, |&(b, _)| b) {
                Ok(i) => byte_to_col[i].1,
                Err(0) => 0,
                Err(i) => byte_to_col[i - 1].1,
            },
            // 超出行尾 → 从最后一列起按均匀格宽外推（拖过行尾继续选）。
            None => {
                let last_col = byte_to_col.last().map_or(0, |&(_, c)| c);
                let overflow = (x - f32::from(layout.width)).max(0.0);
                last_col + 1 + (overflow / self.cell_w.max(1.0)).floor() as usize
            }
        }
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
                        // 跳过宽字符占位（'\0'），避免复制出空字符。
                        if row[c].ch != '\0' {
                            line.push(row[c].ch);
                        }
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

    /// 点击单元处若落在某个 URL / 本地文件路径上，返回该目标（未做 file:// 转换，
    /// 打开前还要经 [`open_target`]）。
    fn url_at(&self, (r, c): (usize, usize)) -> Option<String> {
        let frame = self.terminal.snapshot();
        let row = frame.rows.get(r)?;
        find_links(row)
            .into_iter()
            .find(|&(a, b, _)| c >= a && c <= b)
            .map(|(_, _, url)| url)
    }

    /// 单元处链接（URL 或本地路径）的范围 (行, 起列, 止列)，用于悬停高亮。
    fn link_range_at(&self, (r, c): (usize, usize)) -> Option<(usize, usize, usize)> {
        let frame = self.terminal.snapshot();
        let row = frame.rows.get(r)?;
        find_links(row)
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
            self.notification = None; // 用户回应了该会话 → 视为已处理，清「需要注意」
            self.completed_unread = false;
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
        // 候选窗要摆在光标格子上：从网格原点按 列×字宽 / 行×行高 偏移。
        let (row, col) = self.cursor.unwrap_or((0, 0));
        let origin = element_bounds.origin
            + point(px(PAD_X + col as f32 * self.cell_w), px(PAD_Y + row as f32 * LINE_PX));
        Some(Bounds {
            origin,
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

        // 依据「本终端自身尺寸」重算行列（网格 Hub 里每个终端只占一格）。
        {
            let (w, h) = self.grid_size.get();
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
            // grid_size 未就绪（首帧为 0）时跳过 resize：保持 spawn 的默认 80 列，
            // 等 canvas 量到真实尺寸再调（避免 w=0 把终端缩成最小 4 列）。
            if w > 1.0 && h > 1.0 {
                // 可用网格区 = 自身尺寸减去左右 / 上下各一份内边距。
                let cols = (((w - 2.0 * PAD_X) / cell_w).floor() as usize).clamp(4, 1000);
                let grid_rows = (((h - 2.0 * PAD_Y) / LINE_PX).floor() as usize).clamp(2, 1000);
                self.terminal.resize(grid_rows, cols);
            }
        }

        let frame = self.terminal.snapshot();
        let cursor = frame.cursor;
        self.cursor = cursor; // 存下来供 IME 候选窗定位
        let sel = self.sel;
        let hover_url = self.hover_url;
        let has_hover = hover_url.is_some();
        let base_font = terminal_font();
        let fh = self.focus_handle.clone();
        let entity = cx.entity();
        let origin_cell = self.grid_origin.clone();
        let size_cell = self.grid_size.clone();

        // 背景层：底色（带透明度）+ 可选背景图，铺在终端内容之下。
        // 终端「默认底色」格子渲染时留空（见 render_row），故背景层能透出。
        let ap = cx.global::<crate::Appearance>().clone();
        let mut bg_layer = div().absolute().inset_0().bg(rgb(ap.bg_color));
        if let Some(path) = &ap.bg_image {
            bg_layer = bg_layer.child(
                img(std::path::PathBuf::from(path))
                    .absolute()
                    .inset_0()
                    .size_full()
                    .object_fit(ObjectFit::Cover),
            );
        }
        let bg_layer = bg_layer.opacity(ap.opacity);

        div()
            .relative()
            .track_focus(&self.focus_handle)
            .size_full()
            // 关键：裁剪溢出 + 允许收缩到 0，否则长行的 min-content 宽度会把
            // 容器越撑越宽，canvas 量到更大宽度 → 列数变多 → 行更长，形成放大循环。
            .overflow_hidden()
            .min_w_0()
            .min_h_0()
            .text_color(rgb(terminal::DEFAULT_FG))
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
                        this.notification = None; // 粘贴回应 → 清「需要注意」
                        this.completed_unread = false;
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
                    this.terminal.scroll_to_bottom(); // 敲键盘即回到最新输出，跟真实终端一致
                    this.notification = None; // 用户按键回应 → 清「需要注意」
                    this.completed_unread = false;
                    cx.notify();
                }
            }))
            .on_scroll_wheel(cx.listener(|this, ev: &ScrollWheelEvent, window, cx| {
                // 新的一次触控板手势开始时清空余数，避免上一次手势的残留跟这次叠加。
                if matches!(ev.touch_phase, TouchPhase::Started) {
                    this.scroll_accum = 0.0;
                }
                let delta_px = match ev.delta {
                    ScrollDelta::Lines(p) => p.y * LINE_PX,
                    ScrollDelta::Pixels(p) => f32::from(p.y),
                };
                this.scroll_accum += delta_px;
                let lines = (this.scroll_accum / LINE_PX).trunc();
                if lines != 0.0 {
                    this.scroll_accum -= lines * LINE_PX;
                    // 按终端模式分流：TUI（Claude Code）转成鼠标滚轮事件，普通 shell 滚历史。
                    let (row, col) = this.pos_to_cell(ev.position, window);
                    this.terminal.scroll_wheel(lines as i32, row, col);
                    cx.notify();
                }
            }))
            // 鼠标框选
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                    window.focus(&this.focus_handle, cx);
                    let cell = this.pos_to_cell(ev.position, window);
                    // Cmd+点击打开链接
                    if ev.modifiers.platform {
                        if let Some(url) = this.url_at(cell) {
                            cx.open_url(&open_target(&url));
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
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, window, cx| {
                if ev.pressed_button == Some(MouseButton::Left) {
                    if let Some((a, _)) = this.sel {
                        let head = this.pos_to_cell(ev.position, window);
                        this.sel = Some((a, head));
                        cx.notify();
                    }
                } else {
                    // 按住 Cmd 悬停链接：记录链接范围（用于高亮 + 手型）
                    let hl = if ev.modifiers.platform {
                        this.link_range_at(this.pos_to_cell(ev.position, window))
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
                    this.link_range_at(this.pos_to_cell(window.mouse_position(), window))
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
                cx.listener(|this, ev: &MouseUpEvent, window, cx| {
                    // 锚点 != 端点：真的拖出了选区，保留本地框选、不转发给应用——拖拽选字
                    // 的意图比应用的鼠标点击上报优先级更高，这样才跟真实终端行为一致
                    // （之前版本按下就转发，导致开了鼠标上报的应用里完全没法拖拽选字）。
                    if this.sel.is_some_and(|(a, b)| a != b) {
                        return;
                    }
                    // 未拖动 = 单纯点击。应用开了鼠标点击上报（比如 Claude Code 里可点的
                    // agent 行）就把这次点击（按下+松开）转发过去——按下时故意没转发，
                    // 就是为了先看这一下到底是点击还是拖拽。按住 Shift 强制走本地单击
                    // （绕开应用抢鼠标，通用终端约定）。
                    if !ev.modifiers.shift {
                        let cell = this.pos_to_cell(ev.position, window);
                        if this.terminal.mouse_button(true, cell.0, cell.1) {
                            this.terminal.mouse_button(false, cell.0, cell.1);
                        }
                    }
                    // 单击（未拖动）清除选区
                    this.sel = None;
                    cx.notify();
                }),
            )
            // 背景层（最底）：底色 / 背景图 / 透明度
            .child(bg_layer)
            // 终端主体：逐行渲染 alacritty 网格快照（带颜色 / 光标）
            .child(
                div()
                    .flex()
                    .flex_col()
                    .size_full()
                    .px(px(PAD_X))
                    .py(px(PAD_Y))
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
                        // 网格原点 = 覆盖层原点 + 内边距（终端主体带内边距，坐标相应右下偏移）
                        origin_cell.set((
                            f32::from(bounds.origin.x) + PAD_X,
                            f32::from(bounds.origin.y) + PAD_Y,
                        ));
                        // 记录自身尺寸，供按卡片大小算行列
                        size_cell.set((f32::from(bounds.size.width), f32::from(bounds.size.height)));
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
    mut row: Vec<terminal::Cell>,
    cursor_col: Option<usize>,
    sel: Option<(usize, usize)>,
    base_font: &Font,
    hover_link: Option<(usize, usize)>,
) -> Div {
    // 去掉行尾的填充空格 / 宽字符占位：终端每行都被补空格到满列宽（如 167 格），
    // 整行丢给 StyledText 会因字体自由排版累计宽度超容器而「自动折行」。只渲染到
    // 内容末尾（或光标处）即可，宽度远小于容器，不再折行。
    let mut end = row
        .iter()
        .rposition(|c| c.ch != ' ' && c.ch != '\0')
        .map_or(0, |i| i + 1);
    if let Some(cc) = cursor_col {
        end = end.max(cc + 1);
    }
    row.truncate(end.min(row.len()));

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
            // 宽字符占位（'\0'）不输出：让前一个全角字形自然占满两格。
            if ch != '\0' {
                line.push(ch);
                seg_len += ch.len_utf8();
            }
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
            // 默认底色格子留空（不画背景），让下面的背景层 / 图片 / 桌面透出。
            background_color: (bg != terminal::DEFAULT_BG).then(|| Hsla::from(rgb(bg))),
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

/// 弹一条 macOS 系统通知（osascript，无额外依赖）。title 固定 smelt、
/// 副标题为「会话名 · agent 任务名」、正文为通知消息（对齐 cmux 的信息量）。
/// 失败静默忽略（未签名 / 无权限时可能不显示）。
fn system_notify(session: &str, task: Option<&str>, body: &str) {
    let subtitle = match task {
        Some(t) if !t.trim().is_empty() => format!("{session} · {t}"),
        _ => session.to_string(),
    };
    // 只走原生通知：打包成 smelt.app（有 bundle id）时用 smelt 名字 + 图标显示；
    // 开发版（cargo run 无 bundle）自动静默不打扰，不再回落 osascript。
    #[cfg(target_os = "macos")]
    deliver_native_notification("smelt", &subtitle, body);
    #[cfg(not(target_os = "macos"))]
    let _ = (subtitle, body);
}

/// 原生 `NSUserNotification`：仅在已打包（有 bundle identifier）时投递，用宿主 .app 图标。
/// 未打包 / 不可用则直接返回（开发版静默）。
#[cfg(target_os = "macos")]
fn deliver_native_notification(title: &str, subtitle: &str, body: &str) {
    use objc::runtime::Object;
    use objc::{class, msg_send, sel, sel_impl};

    unsafe {
        // 无 bundle identifier（cargo run 直接跑）→ 原生通知不会投递，静默返回。
        let bundle: *mut Object = msg_send![class!(NSBundle), mainBundle];
        if bundle.is_null() {
            return;
        }
        let ident: *mut Object = msg_send![bundle, bundleIdentifier];
        if ident.is_null() {
            return;
        }
        let center: *mut Object =
            msg_send![class!(NSUserNotificationCenter), defaultUserNotificationCenter];
        if center.is_null() {
            return;
        }
        let nsstr = |s: &str| -> *mut Object {
            let obj: *mut Object = msg_send![class!(NSString), alloc];
            let ptr = s.as_ptr() as *const std::ffi::c_void;
            // encoding 4 = NSUTF8StringEncoding。
            msg_send![obj, initWithBytes: ptr length: s.len() encoding: 4usize]
        };
        let n: *mut Object = msg_send![class!(NSUserNotification), alloc];
        let n: *mut Object = msg_send![n, init];
        let _: () = msg_send![n, setTitle: nsstr(title)];
        let _: () = msg_send![n, setSubtitle: nsstr(subtitle)];
        let _: () = msg_send![n, setInformativeText: nsstr(body)];
        let _: () = msg_send![center, deliverNotification: n];
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

/// 合并 URL + 本地文件路径的可点链接，供 [`TerminalView::url_at`]/[`TerminalView::link_range_at`] 共用。
fn find_links(row: &[terminal::Cell]) -> Vec<(usize, usize, String)> {
    let mut out = find_urls(row);
    out.extend(find_paths(row));
    out
}

/// 在一行里找出所有本地文件路径（绝对路径 / `~/` 开头），返回 (起列, 止列含, 展开后的
/// 绝对路径)。跟 URL 一样按「连续非空白 token」扫描，但额外要求磁盘上真实存在——否则
/// 随便一段带斜杠的文本（命令参数、注释里的 a/b/c）都会被当成可点链接，误判太多。
fn find_paths(row: &[terminal::Cell]) -> Vec<(usize, usize, String)> {
    let n = row.len();
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        let starts = row[i].ch == '/' || (row[i].ch == '~' && i + 1 < n && row[i + 1].ch == '/');
        if starts {
            let mut j = i;
            while j < n && is_url_char(row[j].ch) {
                j += 1;
            }
            let mut end = j;
            while end > i
                && matches!(
                    row[end - 1].ch,
                    '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '"' | '\''
                )
            {
                end -= 1;
            }
            if end > i {
                let token: String = (i..end).map(|k| row[k].ch).collect();
                if let Some(path) = expand_existing_path(&token) {
                    out.push((i, end - 1, path));
                }
            }
            i = end.max(i + 1);
        } else {
            i += 1;
        }
    }
    out
}

/// `~` 展开成 home 目录，并确认路径在磁盘上真实存在（文件或目录）；不存在则不认为
/// 是可点路径，避免误判。
fn expand_existing_path(token: &str) -> Option<String> {
    let expanded = match token.strip_prefix('~') {
        Some(rest) => dirs::home_dir()?.join(rest.trim_start_matches('/')).to_string_lossy().into_owned(),
        None => token.to_string(),
    };
    std::path::Path::new(&expanded).exists().then_some(expanded)
}

/// 把 [`TerminalView::url_at`] 返回的目标转成 `cx.open_url` 能吃的字符串：http(s)
/// 链接原样返回；本地路径转成 `file://` URL 并 percent-encode 每个非常规字节——
/// `NSURL::initWithString:` 对未编码的 UTF-8（中文路径）很挑剔，不编码直接建不出 NSURL。
fn open_target(target: &str) -> String {
    if target.starts_with("http://") || target.starts_with("https://") {
        return target.to_string();
    }
    let mut out = String::from("file://");
    for b in target.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{:02X}", b)),
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
