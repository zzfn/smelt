//! 单个终端视图：一个 Terminal + 焦点 + IME + 网格渲染 + 键盘/滚轮输入。
//! 多个 TerminalView 由 Workspace 以标签形式管理。

use std::cell::Cell as StdCell;
use std::ops::Range;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::*;
use smol::Timer;

use crate::terminal::{self, Terminal};

/// 选区高亮背景色：跟终端主题一起切换（深色用暗蓝，浅色换成不刺眼的浅蓝，
/// 否则深色定死的暗蓝铺在浅底上，选中文字会糊在一起看不清）。
fn sel_bg() -> u32 {
    if terminal::is_dark() { 0x0033_4a6a } else { 0x00ad_d6ff }
}

/// 悬停链接的高亮前景色：同上，浅色主题换成对比度够的蓝。
fn link_fg() -> u32 {
    if terminal::is_dark() { 0x007d_cfff } else { 0x0009_69da }
}

/// 出厂默认终端字体：Nerd Font 的严格等宽变体（含图标/powerline 字形，且单格宽对齐）。
pub const DEFAULT_FONT_FAMILY: &str = "JetBrainsMono Nerd Font Mono";

/// 用户配置的终端字体族（设置页可改，持久化在 Appearance；跟 FONT_PX_ATOM 同一路数：
/// 单进程一套配置，全局量足够）。None/空 = 用出厂默认。
static FONT_FAMILY_CONF: std::sync::RwLock<Option<String>> = std::sync::RwLock::new(None);

/// 设置终端字体族（空白等同恢复默认）。
pub fn set_font_family(name: &str) {
    let name = name.trim();
    if let Ok(mut g) = FONT_FAMILY_CONF.write() {
        *g = if name.is_empty() { None } else { Some(name.to_string()) };
    }
}

/// 当前终端字体族。
pub fn font_family() -> String {
    FONT_FAMILY_CONF
        .read()
        .ok()
        .and_then(|g| g.clone())
        .unwrap_or_else(|| DEFAULT_FONT_FAMILY.to_string())
}

/// 兜底等宽字体：macOS 系统自带，必定存在。放在 fallback 链末尾做最后防线，
/// 保证 cell_w 与实际字形宽度同源——否则测量和渲染各自 fallback 到不同字体，
/// 列数按错误的字宽算出来，终端内容只占窗格的一个恒定比例（用户实测约一半宽）。
const MONO_FALLBACK_FONT: &str = "Menlo";

/// 终端字体（用户配置的主字体 + 内嵌默认字体 + 系统等宽兜底）。渲染和测量都必须
/// 用这个，保持字形来源一致——否则测量用的字体和实际渲染用的字体对某个字符的
/// fallback 结果不一样，会导致列宽计算和实际显示对不上（拖选/鼠标定位跑偏、内容
/// 占宽错误）。DEFAULT_FONT_FAMILY 已内嵌进二进制（见 main.rs 的 add_fonts）且
/// 自带全部 Nerd Font 图标码位：用户自选字体缺图标时落到它，不必单独嵌图标字体。
fn terminal_font() -> Font {
    Font {
        fallbacks: Some(FontFallbacks::from_fonts(vec![
            DEFAULT_FONT_FAMILY.to_string(),
            MONO_FALLBACK_FONT.to_string(),
        ])),
        ..font(font_family())
    }
}

/// 终端网格刷新间隔（后台线程在更新，UI 定时快照重绘）。
const REFRESH: Duration = Duration::from_millis(30);

// Tab / Shift-Tab 在终端聚焦时的专属动作。
//
// gpui-component 的 `Root` 全局把 "tab"/"shift-tab" 绑成了焦点跳转（`window.focus_next`），
// context 是 "Root"——而 GPUI 按键分发时，keymap 匹配到的 action 会在
// `on_key_down` 之类的原始按键监听器之前就被消费掉，根本轮不到终端自己处理，
// 导致 Tab 补全在终端里形同虚设。这里在 "Terminal" 这个更贴近焦点的 context 上
// 重新绑一份，深度更深的 context 按 GPUI 的 keymap 优先级规则会盖过 Root 那份，
// 从而把 Tab/Shift-Tab 交还给终端本身（见下方 render 里的 `.on_action`）。
gpui::actions!(smelt_terminal, [TerminalTab, TerminalBackTab]);

/// 终端字号（px）：跟随设置页「字体大小」全局切换，见 `set_font_px`。单进程只有
/// 一套终端字号，用全局原子量足够，不必给每处渲染/量measure 调用各传一份——
/// 跟 terminal.rs 的 DARK_MODE 是同一路数。
static FONT_PX_ATOM: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(13);
/// 字号可调范围：太小认不清字形，太大一屏放不下几列，都没意义。
pub const MIN_FONT_PX: u32 = 9;
pub const MAX_FONT_PX: u32 = 22;

/// 切换终端字号（px，自动夹到 [MIN_FONT_PX, MAX_FONT_PX]）。
pub fn set_font_px(px: u32) {
    FONT_PX_ATOM.store(px.clamp(MIN_FONT_PX, MAX_FONT_PX), std::sync::atomic::Ordering::Relaxed);
}

/// 当前终端字号（px）。
pub fn font_px() -> f32 {
    FONT_PX_ATOM.load(std::sync::atomic::Ordering::Relaxed) as f32
}

/// 行高：固定按原始 18/13 的比例跟字号一起缩放（原设计：13px 字对 18px 行高）。
fn line_px() -> f32 {
    font_px() * (18.0 / 13.0)
}

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
    /// 用户在侧栏子行右键改过的 pane 名；None = 用自动推导的标题。
    /// 跟着 `PaneState::Leaf` 一起持久化，重开 GUI 按 session_id reattach 时灌回来。
    custom_title: Option<String>,
    /// 初始工作目录（新建标签继承用）。
    cwd: Option<String>,
    /// 是否正在拖动框选（mouse_down 置位、mouse_up 清）。选区本体存在 alacritty 的
    /// Term.selection 里（缓冲区绝对坐标，滚动跟随/新输出漂移由它维护），这里只记
    /// 「拖没拖着」这个交互态。
    selecting: bool,
    /// 拖到可视区上/下边缘时的自动滚动方向：0 不滚，正=向上看历史，负=向下。
    drag_scroll: i32,
    /// 自动滚动期间选区活动端使用的列（沿用最后一次拖动事件的列）。
    drag_scroll_col: usize,
    /// 自动滚动定时器是否已在跑（防重复 spawn）。
    drag_scroll_running: bool,
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
    /// app 前台但用户没在看这个 pane 时的 toast 待发消息（组件 Notification）；
    /// Workspace::render 每帧来取，取走即清空。跟 last_notified 共用同一条
    /// 60s 同文本去重（见轮询循环），系统通知 / toast 二选一，不会重复弹。
    pending_toast: Option<String>,
    /// 最近一次比较过的外观设置：定时刷新时用于判断"背景色/图/透明度/模糊"是否被
    /// 改过（这些跟 PTY 内容无关，Terminal::take_damage 感知不到）。
    /// None = 还没比较过，首次一律当作"变了"以确保能显示当前外观。
    last_appearance: Option<crate::Appearance>,
    /// 触控板滚轮的像素余数：触控板每帧只送几像素的增量，若逐事件独立按
    /// LINE_PX 取整会把大部分小增量截断成 0（滚了但没反应），造成"很不跟手"
    /// 的卡顿感。改为跨事件累加像素，攒够一整行再吐出、余数留到下次。
    scroll_accum: f32,
    /// 建终端时的启动方式（侧栏行图标用，见 `LaunchKind`）。
    launch_kind: LaunchKind,
    /// 快捷启动项的显示名（设置里配的 label）。侧栏标题在 agent 还没上报任务名时
    /// 回退到它，而不是 cwd 末段——否则「+ → Claude Code」建出来却显示项目名。
    launch_label: Option<String>,
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

/// 建终端时用的启动方式，决定侧栏行图标——跟「+」下拉菜单里各项的图标对齐
/// （新建终端/Claude Code/Codex/Copilot 一一对应），一眼认出这一行是哪种会话。
/// 建好之后不变：daemon 重启触发的 `reconnect()` 只换底层连接，不重置这个。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LaunchKind {
    Terminal,
    Claude,
    Codex,
    Copilot,
}

/// 从 `launch` 命令行猜启动方式（见各「+」菜单项的 on_click：'claude'/'claude
/// --dangerously-skip-permissions'/'codex'/'copilot'），用前缀匹配以后加参数不失配。
fn classify_launch(launch: Option<&str>) -> LaunchKind {
    match launch.map(str::trim) {
        Some(l) if l.starts_with("claude") => LaunchKind::Claude,
        Some(l) if l.starts_with("codex") => LaunchKind::Codex,
        Some(l) if l.starts_with("copilot") => LaunchKind::Copilot,
        _ => LaunchKind::Terminal,
    }
}

impl TerminalView {
    pub fn new(
        cx: &mut Context<Self>,
        cwd: Option<String>,
        session_id: String,
        launch: Option<&str>,
        launch_label: Option<&str>,
    ) -> Self {
        let terminal = Terminal::spawn(24, 80, cwd.as_deref(), &session_id, launch)
            .expect("启动内嵌终端失败");
        let launch_kind = classify_launch(launch);
        let launch_label = launch_label
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

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
                    if !dup {
                        if cx.active_window().is_none() {
                            system_notify(&this.title, task.as_deref(), &msg);
                        } else {
                            // app 在前台：系统通知不弹，改交给 Workspace::render 判断——
                            // 只有「没在看这个 pane」才真弹 toast，正在看的直接吃掉。
                            this.pending_toast = Some(msg.clone());
                        }
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
            custom_title: None,
            cwd,
            selecting: false,
            drag_scroll: 0,
            drag_scroll_col: 0,
            drag_scroll_running: false,
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
            pending_toast: None,
            last_appearance: None,
            scroll_accum: 0.0,
            launch_kind,
            launch_label,
        }
    }

    /// 建终端时的启动方式（侧栏行图标对齐「+」菜单用）。
    pub fn launch_kind(&self) -> LaunchKind {
        self.launch_kind
    }

    /// 快捷启动项显示名（见字段注释）；普通「新建终端」为 None。
    pub fn launch_label(&self) -> Option<&str> {
        self.launch_label.as_deref()
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
        // 新 Terminal 自带空选区，只需重置本视图的拖选交互态。
        self.selecting = false;
        self.drag_scroll = 0;
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

    /// 通知消息文本（供通知面板显示）。
    pub fn notification(&self) -> Option<&str> {
        self.notification.as_deref()
    }

    /// 取走待发的 toast 消息（见 pending_toast）；Workspace::render 每帧调用一次。
    pub fn take_pending_toast(&mut self) -> Option<String> {
        self.pending_toast.take()
    }

    /// agent 报告的终端标题（含任务名 + 状态符号）；供侧栏 / 总览显示。
    pub fn agent_title(&self) -> Option<String> {
        self.terminal.current_title()
    }


    pub fn title(&self) -> &str {
        &self.title
    }

    /// 用户给这个 pane 起的名字；None = 还没改过名。
    pub fn custom_title(&self) -> Option<&str> {
        self.custom_title.as_deref()
    }

    /// 改名。传 None（或提交空串）= 清掉自定义名，回退到自动推导的标题。
    pub fn set_custom_title(&mut self, title: Option<String>) {
        self.custom_title = title.filter(|s| !s.trim().is_empty());
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
    ///
    /// 直接按均匀格宽换算即可：render_row 已经把每一批文本钉死在 col * cell_w 上，
    /// 画面就是标准网格。（早先渲染靠字体 advance 自由流，中文一多就整体左漂，这里不得不
    /// 「重新整形一遍该行、用 index_for_x 反查列号」去复现那个歪掉的几何——渲染掰正之后
    /// 那套 workaround 反而会让鼠标跟画面对不上，已随之删除。）
    fn pos_to_cell(&self, pos: Point<Pixels>, _window: &mut Window) -> (usize, usize) {
        let (ox, oy) = self.grid_origin.get();
        let x = (f32::from(pos.x) - ox).max(0.0);
        let y = (f32::from(pos.y) - oy).max(0.0);
        let row = (y / line_px()).floor() as usize;
        let col = (x / self.cell_w.max(1.0)).floor() as usize;
        (row, col)
    }

    /// 窗口像素 x 落在其网格单元的左半还是右半：选区端点的 Side。alacritty 用它
    /// 决定端点格是否纳入选区（同格同侧 = 空选区），于是单击/同格微抖不会误选出
    /// 一格——否则 mouse_up 会把这次点击当成拖选，不再转发给开了鼠标上报的 TUI。
    fn pos_in_left_half(&self, pos: Point<Pixels>) -> bool {
        let (ox, _) = self.grid_origin.get();
        let x = (f32::from(pos.x) - ox).max(0.0);
        (x / self.cell_w.max(1.0)).fract() < 0.5
    }

    /// 拖选拖出可视区上/下边缘后的自动滚动循环：每 60ms 按 drag_scroll 方向滚一行，
    /// 并把选区活动端钉在对应边缘行（行传 0 / usize::MAX，由 selection_update 夹回
    /// 可视区），一边滚一边扩选。松开鼠标或拖回区内即停，定时器自行退出。
    fn start_drag_scroll(&mut self, cx: &mut Context<Self>) {
        if self.drag_scroll_running {
            return;
        }
        self.drag_scroll_running = true;
        cx.spawn(async move |this, cx| loop {
            Timer::after(Duration::from_millis(60)).await;
            let go = this.update(cx, |this, cx| {
                if !this.selecting || this.drag_scroll == 0 {
                    this.drag_scroll_running = false;
                    return false;
                }
                let dir = this.drag_scroll;
                this.terminal.scroll(dir);
                // 向上滚活动端钉在首行（扩向更早内容），向下钉在末行；Side 取
                // 扩选方向的外侧，保证边缘行的端点格被选进来。
                let row = if dir > 0 { 0 } else { usize::MAX };
                this.terminal.selection_update(row, this.drag_scroll_col, dir > 0);
                cx.notify();
                true
            });
            if !matches!(go, Ok(true)) {
                break;
            }
        })
        .detach();
    }

    /// 某行 [a, b) 两个网格列之间要按几次左右方向键才能跨过去——不能直接拿列号
    /// 相减：宽字符（中/日/韩等）占两格但对 shell 的行编辑器来说只是一个字符，一次
    /// 方向键跨的是「一个字符」而不是「一格」。按列差算会在宽字符行里按过头（见
    /// Option+点击移动光标的调用处）。真正的字符数 = 该区间内非占位格（ch != '\0'）
    /// 的格子数——占位格是 terminal.rs 里宽字符后面那个跳过的空壳格，不代表独立字符。
    fn char_steps_between(&self, row: usize, a: usize, b: usize) -> usize {
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        let frame = self.terminal.snapshot();
        let Some(cells) = frame.rows.get(row) else {
            return hi - lo;
        };
        cells[lo..hi.min(cells.len())].iter().filter(|c| c.ch != '\0').count()
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
    /// 输入法拿这个接口取「文档里某一段文字」。我们的「文档」只有合成中的预编辑串
    /// （终端已提交的内容不属于可编辑文档），所以按 UTF-16 下标切片返回；越界就夹回
    /// 有效范围并通过 adjusted 告诉输入法。**不能不管问的是哪一段都把整串还回去**：
    /// 长度对不上会让输入法认为文档状态错乱，进而放弃合成、把拼音原文直接上屏。
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let text = self.marked_text.as_ref()?;
        let units: Vec<u16> = text.encode_utf16().collect();
        let start = range_utf16.start.min(units.len());
        let end = range_utf16.end.clamp(start, units.len());
        if start != range_utf16.start || end != range_utf16.end {
            *adjusted = Some(start..end);
        }
        String::from_utf16(&units[start..end]).ok()
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        // 一直报 None → macOS 侧的 selectedRange 变成 {NSNotFound, 0}，等于告诉系统
        // 「这里没有文字光标」。切换输入法时那个提示气泡靠 selectedRange 判断当前
        // 焦点是否有效文字输入位置，一直是 NSNotFound 会导致它不出现（IME 候选窗本身
        // 走 hasMarkedText/setMarkedText，不受这个影响，所以合成打字不受影响）。这里
        // 汇报一个折叠的光标位置：合成中就在 marked_text 末尾，否则在 0。
        let len = self.marked_text.as_ref().map(|s| s.encode_utf16().count()).unwrap_or(0);
        Some(UTF16Selection { range: len..len, reversed: false })
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
            + point(px(PAD_X + col as f32 * self.cell_w), px(PAD_Y + row as f32 * line_px()));
        Some(Bounds {
            origin,
            size: size(px(2.0), px(line_px())),
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
            // 必须用 terminal_font()（带 fallback 链）而非裸字体族：主字体没装时，
            // 裸字体和渲染各自 fallback 到不同字体，cell_w 就跟实际画出来的字宽脱节。
            let run = TextRun {
                len: 1,
                font: terminal_font(),
                color: hsla(0.0, 0.0, 1.0, 1.0),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let measured =
                f32::from(window.text_system().layout_line("M", px(font_px()), &[run], None).width);
            let cell_w = if measured > 1.0 {
                measured
            } else {
                font_px() * CELL_W_RATIO
            };
            self.cell_w = cell_w; // 供鼠标坐标换算
            // grid_size 未就绪（首帧为 0）时跳过 resize：保持 spawn 的默认 80 列，
            // 等 canvas 量到真实尺寸再调（避免 w=0 把终端缩成最小 4 列）。
            if w > 1.0 && h > 1.0 {
                // 可用网格区 = 自身尺寸减去左右 / 上下各一份内边距。
                let cols = (((w - 2.0 * PAD_X) / cell_w).floor() as usize).clamp(4, 1000);
                let grid_rows = (((h - 2.0 * PAD_Y) / line_px()).floor() as usize).clamp(2, 1000);
                self.terminal.resize(grid_rows, cols);
            }
        }

        let frame = self.terminal.snapshot();
        // 画反色块用可见光标（应用 CSI ?25l 藏光标时为 None）；IME 候选窗/预编辑
        // 定位、Option+点击移光标用**位置**（cursor_pos，含隐藏）——TUI 藏了光标
        // 输入法照样要知道往哪落。
        //
        // IME 合成中不画网格里的光标块：预编辑串自带光标（画在拼音末尾，跟 iTerm2 一致）。
        let cursor = if self.marked_text.is_some() { None } else { frame.cursor };
        self.cursor = frame.cursor_pos;
        let hover_url = self.hover_url;
        let has_hover = hover_url.is_some();
        let base_font = terminal_font();
        // 网格列宽：render_row 用它把每一批文本钉到 col * cell_w 上（见 render_row 头注）。
        let cell_w = self.cell_w;

        // IME 合成中的拼音预编辑串（marked text）：macOS 的分工是候选词浮窗由系统画
        // （bounds_for_range 只负责告诉它摆哪），**预编辑串由应用自己画**——不画的话
        // 打拼音就是盲打，只有候选窗没有输入回显。交给 render_row 画在光标所在行的行内，
        // 光标已上滚离开可视区（cursor_pos 为 None）时自然不画。
        let ime = self.marked_text.clone().zip(frame.cursor_pos);
        let fh = self.focus_handle.clone();
        let entity = cx.entity();
        let origin_cell = self.grid_origin.clone();
        let size_cell = self.grid_size.clone();

        // 背景层：底色（带透明度）+ 可选背景图，铺在终端内容之下。
        // 终端「默认底色」格子渲染时留空（见 render_row），故背景层能透出——所以这层
        // 的颜色必须跟 terminal::default_bg() 是同一套逻辑：用户没手动选过背景色时
        // 跟主题模式走，选过了就保留用户的选择（不因为切深浅色模式而被顶掉）。
        let ap = cx.global::<crate::Appearance>().clone();
        let bg_color = if ap.bg_color_is_default() { terminal::default_bg() } else { ap.bg_color };
        let mut bg_layer = div().absolute().inset_0().bg(rgb(bg_color));
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
            // 见 TerminalTab/TerminalBackTab 上的注释：让 Tab/Shift-Tab 在终端聚焦时
            // 归终端自己处理，别被 Root 的全局焦点跳转吃掉。
            .key_context("Terminal")
            .size_full()
            // 关键：裁剪溢出 + 允许收缩到 0，否则长行的 min-content 宽度会把
            // 容器越撑越宽，canvas 量到更大宽度 → 列数变多 → 行更长，形成放大循环。
            .overflow_hidden()
            .min_w_0()
            .min_h_0()
            .text_color(rgb(terminal::default_fg()))
            .font_family(font_family())
            .on_action(cx.listener(|this, _: &TerminalTab, _window, cx| {
                this.terminal.send_input(b"\t");
                this.terminal.scroll_to_bottom();
                this.notification = None;
                this.completed_unread = false;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &TerminalBackTab, _window, cx| {
                this.terminal.send_input(b"\x1b[Z"); // xterm 反向 Tab（back-tab）序列
                this.terminal.scroll_to_bottom();
                this.notification = None;
                this.completed_unread = false;
                cx.notify();
            }))
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _window, cx| {
                let ks = &ev.keystroke;
                let m = &ks.modifiers;
                // IME 合成中：这些键归输入法（backspace 删拼音、enter/space/数字选词），
                // 不能再往 PTY 发一份，否则终端会当成真实按键吃掉。上屏的文字走
                // replace_text_in_range 进来。
                if this.marked_text.is_some() && !m.platform {
                    return;
                }
                // Cmd+C 复制选区（alacritty 按缓冲区绝对行取文本，跨屏选区也完整）
                if m.platform && ks.key == "c" {
                    if let Some(text) = this.terminal.selection_text() {
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
                if let Some(bytes) = keystroke_to_bytes(
                    ks,
                    this.terminal.app_cursor_mode(),
                    this.terminal.kitty_keyboard_mode(),
                ) {
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
                    ScrollDelta::Lines(p) => p.y * line_px(),
                    ScrollDelta::Pixels(p) => f32::from(p.y),
                };
                this.scroll_accum += delta_px;
                let lines = (this.scroll_accum / line_px()).trunc();
                if lines != 0.0 {
                    this.scroll_accum -= lines * line_px();
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
                    // Option+点击：模拟 iTerm2/Terminal.app 的「点击移动光标」——只在
                    // 点击的正是光标所在那一行时才生效（shell 当前输入行），发对应数量
                    // 的左右方向键让 shell 的行编辑器（readline/zsh line editor）把光标
                    // 挪过去。终端本身没法直接把光标「传送」到任意格：光标位置由 shell
                    // 端的行编辑器状态决定，我们只能模拟按键让它自己移动。
                    if ev.modifiers.alt {
                        if let Some((cursor_row, cursor_col)) = this.cursor {
                            if cell.0 == cursor_row && cell.1 != cursor_col {
                                let app_cursor = this.terminal.app_cursor_mode();
                                let step: &[u8] = if cell.1 > cursor_col {
                                    if app_cursor { b"\x1bOC" } else { b"\x1b[C" }
                                } else if app_cursor {
                                    b"\x1bOD"
                                } else {
                                    b"\x1b[D"
                                };
                                let count = this.char_steps_between(cell.0, cursor_col, cell.1);
                                let mut bytes = Vec::with_capacity(step.len() * count);
                                for _ in 0..count {
                                    bytes.extend_from_slice(step);
                                }
                                this.terminal.send_input(&bytes);
                            }
                        }
                        return;
                    }
                    let kind = match ev.click_count {
                        2 => terminal::SelectionKind::Word,          // 双击选词（语义边界）
                        n if n >= 3 => terminal::SelectionKind::Line, // 三击选整行
                        _ => terminal::SelectionKind::Simple,
                    };
                    this.terminal.selection_start(
                        cell.0,
                        cell.1,
                        this.pos_in_left_half(ev.position),
                        kind,
                    );
                    this.selecting = true;
                    cx.notify();
                }),
            )
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, window, cx| {
                if ev.pressed_button == Some(MouseButton::Left) {
                    if this.selecting {
                        let (row, col) = this.pos_to_cell(ev.position, window);
                        this.terminal.selection_update(
                            row,
                            col,
                            this.pos_in_left_half(ev.position),
                        );
                        // 拖出可视区上/下边缘 → 记方向并启动自动滚动（一边滚一边扩选）。
                        let (_, oy) = this.grid_origin.get();
                        let (_, h) = this.grid_size.get();
                        let y = f32::from(ev.position.y) - oy;
                        this.drag_scroll = if y < 0.0 {
                            1
                        } else if y > h - 2.0 * PAD_Y {
                            -1
                        } else {
                            0
                        };
                        this.drag_scroll_col = col;
                        if this.drag_scroll != 0 {
                            this.start_drag_scroll(cx);
                        }
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
                    this.selecting = false;
                    this.drag_scroll = 0;
                    // 真的拖出了非空选区：保留本地框选、不转发给应用——拖拽选字
                    // 的意图比应用的鼠标点击上报优先级更高，这样才跟真实终端行为一致
                    // （之前版本按下就转发，导致开了鼠标上报的应用里完全没法拖拽选字）。
                    //
                    // 并且**选中即复制**（iTerm2 的 copy-on-select）：TUI（Claude Code
                    // 等）随时会重绘界面，重绘一碰到选区 alacritty 就把它清掉（内容变了
                    // 选区作废，防复制到错内容），等用户滚动完再按 Cmd+C 多半已经丢了——
                    // 松手瞬间文本就进剪贴板，之后界面怎么刷新都不影响已拿到的内容。
                    if let Some(text) = this.terminal.selection_text() {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
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
                    this.terminal.selection_clear();
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
                    .text_size(px(font_px()))
                    .line_height(px(line_px()))
                    .children(frame.rows.into_iter().enumerate().map(move |(r, row)| {
                        let cc = match cursor {
                            Some((cr, cc)) if cr == r => Some(cc),
                            _ => None,
                        };
                        let hl = match hover_url {
                            Some((hr, a, b)) if hr == r => Some((a, b)),
                            _ => None,
                        };
                        let ime_here = match &ime {
                            Some((text, (ir, ic))) if *ir == r => Some((text.clone(), *ic)),
                            _ => None,
                        };
                        render_row(row, cc, &base_font, hl, cell_w, ime_here)
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

/// 渲染一行。**终端是网格，第 N 列就必须画在 N × cell_w**——字体的字形宽度只决定
/// 「字长什么样」，不决定「它在哪」。
///
/// 之前整行拼成一个 StyledText 交给排版器自由流，位置就由字体的 advance 说了算：
/// 主字体（JetBrains Mono，advance 0.6em）没有中文字形，中文 fallback 到 PingFang
/// （advance 1.0em），而两格 = 1.2em ——每个中文字亏 0.2em，误差沿行累积，于是 Claude
/// Code 输出的表格里，含中文的行比纯 `─` 横线短，`│` 一路往左漂。
///
/// 现在照 Zed 终端的做法（同为 GPUI 栈）：按「样式相同 + 列号连续」把一行切成若干批，
/// 每批绝对定位在自己的起始列上。宽字符的第二格是 '\0' 占位，跳过它但列号照常前进，
/// 于是宽字符后面的内容列号对不上、自动断成新的一批——每个全角字各自成批、各自钉在
/// 自己的列上。字形窄于两格只是自己画窄一点，绝不会把后面的字符往左推。
///
/// `ime`（预编辑串, 起始列）不为空时，在行内叠一层：**必须画在行 div 里而不是另起一个
/// 绝对定位的层**——行的 y 由 flex 逐行堆叠决定，带像素对齐舍入，跟 `PAD + row × line_px`
/// 算出来的 y 差几个像素，几十行开外肉眼可见（预编辑串比同一行已上屏的中文高一截）。
fn render_row(
    row: Vec<terminal::Cell>,
    cursor_col: Option<usize>,
    base_font: &Font,
    hover_link: Option<(usize, usize)>,
    cell_w: f32,
    ime: Option<(String, usize)>,
) -> Div {
    let is_link = |i: usize| hover_link.map_or(false, |(a, b)| i >= a && i <= b);
    // 悬停链接：高亮色 + 下划线；再叠加光标反色 / 选区背景。
    let style_of = |i: usize| -> (u32, u32, bool, bool) {
        let c = &row[i];
        let mut fg = c.fg;
        let mut bg = c.bg;
        let mut underline = c.underline;
        if is_link(i) {
            fg = link_fg();
            underline = true;
        }
        if Some(i) == cursor_col {
            std::mem::swap(&mut fg, &mut bg);
        } else if c.selected {
            bg = sel_bg();
        }
        (fg, bg, c.bold, underline)
    };

    // 只渲染到内容末尾（或光标处）：终端每行都被补空格到满列宽，尾部空格没什么可画的。
    let mut end = row
        .iter()
        .rposition(|c| c.ch != ' ' && c.ch != '\0')
        .map_or(0, |i| i + 1);
    if let Some(cc) = cursor_col {
        end = end.max(cc + 1);
    }
    let end = end.min(row.len());

    let x_of = |col: usize| px(col as f32 * cell_w);

    // 背景层：按「连续同底色」的列区间画矩形。**必须走列区间而不是靠文字的
    // background_color**——后者只覆盖字形的实际宽度，中文字形窄于两格时选区高亮会露出
    // 缝隙，且宽字符占位格根本没有对应的字符去承载底色。
    let mut bg_rects: Vec<(usize, usize, u32)> = Vec::new(); // (起始列, 占几格, 颜色)
    let mut i = 0;
    while i < end {
        let bg = style_of(i).1;
        let start = i;
        while i < end && style_of(i).1 == bg {
            i += 1;
        }
        // 默认底色不画，让下面的背景层 / 图片 / 桌面透出。
        if bg != terminal::default_bg() {
            bg_rects.push((start, i - start, bg));
        }
    }

    let batches = text_batches(&row, end, &style_of);

    div()
        .relative()
        .h(px(line_px()))
        .children(bg_rects.into_iter().map(move |(col, span, bg)| {
            div()
                .absolute()
                .left(x_of(col))
                .w(px(span as f32 * cell_w))
                .h_full()
                .bg(rgb(bg))
        }))
        .children(batches.into_iter().map(move |(col, text, (fg, _, bold, underline))| {
            let mut fnt = base_font.clone();
            if bold {
                fnt.weight = FontWeight::BOLD;
            }
            let run = TextRun {
                len: text.len(),
                font: fnt,
                color: Hsla::from(rgb(fg)),
                // 底色交给上面的 bg_rects 画，这里不重复。
                background_color: None,
                underline: underline.then(|| UnderlineStyle {
                    thickness: px(1.0),
                    color: Some(Hsla::from(rgb(fg))),
                    wavy: false,
                }),
                strikethrough: None,
            };
            div()
                .absolute()
                .left(x_of(col))
                .child(StyledText::new(text).with_runs(vec![run]))
        }))
        // IME 预编辑串（见函数头注）：垫终端底色遮住底下的内容、下划线标示「合成中」，
        // 光标跟在拼音末尾（合成中网格里的光标块不画，见 render 里 cursor 的取值）。
        .children(ime.map(|(text, col)| {
            let run = TextRun {
                len: text.len(),
                font: base_font.clone(),
                color: Hsla::from(rgb(terminal::default_fg())),
                background_color: None,
                underline: Some(UnderlineStyle {
                    thickness: px(1.0),
                    color: Some(Hsla::from(rgb(terminal::default_fg()))),
                    wavy: false,
                }),
                strikethrough: None,
            };
            div()
                .absolute()
                .left(x_of(col))
                .h_full()
                .flex()
                .flex_row()
                .items_start()
                .bg(rgb(terminal::default_bg()))
                .child(StyledText::new(text).with_runs(vec![run]))
                .child(div().w(px(cell_w)).h_full().bg(rgb(terminal::default_fg())))
        }))
}

/// 单元格样式：(前景, 背景, 粗体, 下划线)。
type CellStyle = (u32, u32, bool, bool);

/// 把一行切成若干「批」：(起始网格列, 文本, 样式)。每批之后会被绝对定位到
/// `起始列 × cell_w`，所以这里算出的列号就是画面上的位置，是对齐的唯一依据。
///
/// 续接一批的条件（跟 Zed 终端一致）：**样式相同，且这一格紧接着本批已占的格子**。
/// 宽字符的第二格是 '\0'，跳过它但不跳过列号，于是它后面的字符「对不上列号」而断批——
/// 每个全角字因此各自成批、各自钉在自己的列上，字形宽窄再也推不动后面的字符。
fn text_batches(
    row: &[terminal::Cell],
    end: usize,
    style_of: &dyn Fn(usize) -> CellStyle,
) -> Vec<(usize, String, CellStyle)> {
    let mut out: Vec<(usize, String, CellStyle)> = Vec::new();
    // (起始列, 已占格数, 文本, 样式)
    let mut cur: Option<(usize, usize, String, CellStyle)> = None;
    for i in 0..end.min(row.len()) {
        let ch = row[i].ch;
        if ch == '\0' {
            continue; // 宽字符占位格：不产生字形，但列号照常前进
        }
        let style = style_of(i);
        match cur.as_mut() {
            Some((start, count, text, st)) if *st == style && *start + *count == i => {
                text.push(ch);
                *count += 1;
            }
            _ => {
                if let Some((col, _, text, st)) = cur.take() {
                    out.push((col, text, st));
                }
                cur = Some((i, 1, ch.to_string(), style));
            }
        }
    }
    if let Some((col, _, text, st)) = cur {
        out.push((col, text, st));
    }
    out
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
///
/// `app_cursor`：终端是否处于 DECCKM「应用光标键」模式（见 Terminal::app_cursor_mode）。
/// 方向键固定发 CSI（`ESC [ A`）之前，Claude Code 这类开了 DECCKM 的全屏 TUI 收不到
/// 它期待的 SS3（`ESC O A`）序列，方向键在这类界面里就跟没按一样。
///
/// `kitty_keys`：对端是否开了 kitty keyboard protocol（见 Terminal::kitty_keyboard_mode），
/// 决定带修饰键的 Enter 走 CSI u 还是遗留编码。
fn keystroke_to_bytes(ks: &Keystroke, app_cursor: bool, kitty_keys: bool) -> Option<Vec<u8>> {
    let m = &ks.modifiers;

    if m.platform {
        return None;
    }

    // Enter 单独拎出来：遗留编码里 Shift/Alt/Ctrl+Enter 全都塌缩成 `\r`，跟裸 Enter 无从
    // 区分，所以 Claude Code 那种「Shift+Enter 换行、Enter 提交」在传统终端里天然做不到。
    // 对端开了 kitty keyboard protocol 时才按 CSI u 上报修饰键（Shift+Enter → `ESC[13;2u`）；
    // 没开就必须继续发 `\r`，否则 bash/zsh 里按 Shift+Enter 会把 `[13;2u` 当文本吐出来。
    if ks.key == "enter" {
        let mods = csi_u_modifiers(m);
        if kitty_keys && mods > 1 {
            return Some(format!("\x1b[13;{mods}u").into_bytes());
        }
        // 协议没开时的兜底：Alt+Enter 按传统 meta 前缀发 `ESC` + `CR`，Claude Code 认这条。
        if m.alt {
            return Some(b"\x1b\r".to_vec());
        }
        return Some(b"\r".to_vec());
    }

    let named: Option<&[u8]> = match ks.key.as_str() {
        "backspace" => Some(b"\x7f"),
        "tab" => Some(b"\t"),
        "escape" => Some(b"\x1b"),
        "left" => Some(if app_cursor { b"\x1bOD" } else { b"\x1b[D" }),
        "right" => Some(if app_cursor { b"\x1bOC" } else { b"\x1b[C" }),
        "up" => Some(if app_cursor { b"\x1bOA" } else { b"\x1b[A" }),
        "down" => Some(if app_cursor { b"\x1bOB" } else { b"\x1b[B" }),
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

/// CSI u 序列里的修饰键参数：基数 1，再按位叠加 shift(1) / alt(2) / ctrl(4)。
/// 例：Shift+Enter → 2，于是 `ESC[13;2u`。
fn csi_u_modifiers(m: &Modifiers) -> u8 {
    1 + u8::from(m.shift) + (u8::from(m.alt) << 1) + (u8::from(m.control) << 2)
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：那会把 gpui 的 `test` 属性宏一起带进来，盖掉标准 #[test]。
    use super::{keystroke_to_bytes, text_batches, CellStyle};
    use crate::terminal::Cell;
    use gpui::{Keystroke, Modifiers};

    const PLAIN: CellStyle = (0xffffff, 0x000000, false, false);

    /// 造一行 cell：宽字符（中文）自动补一个 '\0' 占位格，跟 alacritty 的网格一致。
    fn row(s: &str) -> Vec<Cell> {
        let mut out = Vec::new();
        for ch in s.chars() {
            let wide = (ch as u32) >= 0x1100 && !ch.is_ascii();
            out.push(Cell {
                ch,
                fg: 0xffffff,
                bg: 0x000000,
                bold: false,
                underline: false,
                selected: false,
            });
            if wide {
                out.push(Cell {
                    ch: '\0',
                    fg: 0xffffff,
                    bg: 0x000000,
                    bold: false,
                    underline: false,
                    selected: false,
                });
            }
        }
        out
    }

    fn batches(cells: &[Cell]) -> Vec<(usize, String)> {
        text_batches(cells, cells.len(), &|_| PLAIN)
            .into_iter()
            .map(|(col, text, _)| (col, text))
            .collect()
    }

    /// 核心不变量：每个全角字各自成批，且起始列 = 它在网格里的真实列号。
    /// 这正是表格错位的修复点——「明细条数」这样的中文后面跟着的 `│`，其列号必须是
    /// 按「每个中文占 2 格」算出来的，而不是由字体的实际字形宽度累积出来的。
    #[test]
    fn wide_chars_each_get_their_own_batch_at_grid_columns() {
        // 网格：中(0,1) 文(2,3) a(4) b(5)
        let cells = row("中文ab");
        assert_eq!(
            batches(&cells),
            vec![(0, "中".into()), (2, "文".into()), (4, "ab".into())],
            "两个中文各自成批、列号 0 和 2；后面的 ascii 从第 4 列起连成一批"
        );
    }

    /// **整个方案成立的关键性质：宽字符永远落在一批的末尾。**
    ///
    /// 因为它后面必跟一个 '\0' 占位格，使得再后面的字符列号一定对不上 `start + count`
    /// 而断批。于是一批里除了最后那个字符，其余全是 advance 恰好等于 cell_w 的窄字符
    /// （等宽字体保证），批内位置天然正确；宽字符自己那点亏空（PingFang 的 1.0em vs
    /// 两格的 1.2em）后面再没有字符可推，于是推不动任何东西——旧渲染里正是这 0.2em
    /// 沿行累积，把表格的 `│` 一路往左拽。
    ///
    /// 所以 "a中x" 应该是 a 和 中 合成一批（中在批尾），x 断批后重新钉在第 3 列。
    #[test]
    fn wide_char_always_lands_at_end_of_its_batch() {
        // 网格：a(0) 中(1,2) x(3)
        let cells = row("a中x");
        assert_eq!(batches(&cells), vec![(0, "a中".into()), (3, "x".into())]);

        // 多个窄字符在前也一样：宽字符照样收尾，后面的 ascii 重新按列定位。
        let cells = row("ab中cd");
        assert_eq!(batches(&cells), vec![(0, "ab中".into()), (4, "cd".into())]);
    }

    /// 纯 ascii 不该被切碎——一整段连续同样式的文本仍然只有一批（保住性能）。
    #[test]
    fn plain_ascii_stays_one_batch() {
        let cells = row("hello world");
        assert_eq!(batches(&cells), vec![(0, "hello world".into())]);
    }

    /// 样式变了就断批，且新批的列号要对（否则上色段会整体错位）。
    #[test]
    fn style_change_splits_batch_at_right_column() {
        let cells = row("abcd");
        // 前两格一种颜色，后两格另一种。
        let got: Vec<(usize, String)> = text_batches(&cells, cells.len(), &|i| {
            if i < 2 { PLAIN } else { (0xff0000, 0x000000, false, false) }
        })
        .into_iter()
        .map(|(col, text, _)| (col, text))
        .collect();
        assert_eq!(got, vec![(0, "ab".into()), (2, "cd".into())]);
    }

    /// 空行不该产出任何批。
    #[test]
    fn empty_row_yields_no_batches() {
        assert!(batches(&[]).is_empty());
    }

    fn enter(shift: bool, alt: bool, control: bool) -> Keystroke {
        Keystroke {
            modifiers: Modifiers {
                shift,
                alt,
                control,
                ..Default::default()
            },
            key: "enter".into(),
            key_char: None,
        }
    }

    /// 开了 kitty keyboard protocol 的 TUI（Claude Code v2.1+）必须能把 Shift+Enter
    /// 跟裸 Enter 区分开，否则「换行」会被当成「提交」。
    #[test]
    fn shift_enter_reports_csi_u_when_kitty_enabled() {
        let bytes = keystroke_to_bytes(&enter(true, false, false), false, true).unwrap();
        assert_eq!(bytes, b"\x1b[13;2u");
    }

    /// 修饰键位的叠加：alt=2、ctrl=4，都在基数 1 上加。
    #[test]
    fn other_enter_modifiers_report_csi_u_when_kitty_enabled() {
        let alt = keystroke_to_bytes(&enter(false, true, false), false, true).unwrap();
        assert_eq!(alt, b"\x1b[13;3u");
        let ctrl = keystroke_to_bytes(&enter(false, false, true), false, true).unwrap();
        assert_eq!(ctrl, b"\x1b[13;5u");
        let all = keystroke_to_bytes(&enter(true, true, true), false, true).unwrap();
        assert_eq!(all, b"\x1b[13;8u");
    }

    /// 裸 Enter 即便在 kitty 模式下也发遗留的 `\r`——协议如此规定，也让协议没被复位时
    /// 用户还能在 shell 里敲 `reset` 把终端救回来。
    #[test]
    fn plain_enter_stays_carriage_return() {
        let with_kitty = keystroke_to_bytes(&enter(false, false, false), false, true).unwrap();
        assert_eq!(with_kitty, b"\r");
        let without = keystroke_to_bytes(&enter(false, false, false), false, false).unwrap();
        assert_eq!(without, b"\r");
    }

    /// 没开协议的程序（bash/zsh）不认 CSI u：这时 Shift+Enter 必须老老实实发 `\r`，
    /// 不然 `[13;2u` 会被 readline 当普通文本吐在命令行里。
    #[test]
    fn shift_enter_falls_back_to_carriage_return_without_kitty() {
        let bytes = keystroke_to_bytes(&enter(true, false, false), false, false).unwrap();
        assert_eq!(bytes, b"\r");
    }

    /// 协议没开时 Alt+Enter 还有条传统通道：meta 前缀 `ESC` + `CR`，Claude Code 认这个。
    #[test]
    fn alt_enter_falls_back_to_meta_prefix_without_kitty() {
        let bytes = keystroke_to_bytes(&enter(false, true, false), false, false).unwrap();
        assert_eq!(bytes, b"\x1b\r");
    }
}
