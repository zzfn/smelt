//! 单个终端视图：一个 Terminal + 焦点 + IME + 网格渲染 + 键盘/滚轮输入。
//! 多个 TerminalView 由 Workspace 以标签形式管理。

use std::cell::Cell as StdCell;
use std::ops::Range;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::input::Input;
use gpui_component::menu::{ContextMenuExt, PopupMenuItem};
use smol::Timer;

use crate::terminal::{self, Terminal};
use crate::tasks::NewTaskPrefill;
use crate::NewTask;

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
gpui::actions!(smelt_terminal, [TerminalTab, TerminalBackTab, TerminalFind, TerminalFindNext, TerminalFindPrev, TerminalFindClose]);

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
    /// 上一帧的焦点状态，用来把「焦点变了」这件事上报给应用（DEC 1004，见 report_focus）。
    was_focused: bool,
    /// 上一帧的网格快照，渲染和命中测试共用（Zed 同样把 last_content 缓存在 model 上）。
    /// snapshot() 会把整个网格连同颜色/属性 clone 一遍，而 url_at / link_range_at /
    /// char_steps_between 都是鼠标事件里调的——按住 Cmd 划过屏幕时每个 move 事件都全量
    /// clone 一次实在太浪费。鼠标事件必然发生在刚渲染过的终端上，用上一帧足够。
    last_frame: Option<Rc<terminal::Frame>>,
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
    /// 应用鼠标上报模式：mousedown 时已把 press 转发给 TUI，后续 drag/release 也走
    /// 应用路径，不再做本地框选。按住 Shift 强制本地选区（xterm 约定旁路）。
    app_mouse: bool,
    /// 终端内搜索：打开时顶部出输入条（Cmd+F）；命中高亮在 paint 里画。
    search_open: bool,
    search_input: Option<Entity<gpui_component::input::InputState>>,
    _search_sub: Option<Subscription>,
    /// 当前可视区内所有搜索命中（含 active）；关搜索时清空。
    search_hits: Vec<terminal::SearchHit>,
    /// 「3/12」；total=0 表示无结果。
    search_status: terminal::SearchStatus,
    /// 滚动条拖动中：记录按下时 thumb 内偏移（像素），None = 没在拖。
    scrollbar_drag: Option<f32>,
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
    /// 绑定任务刚被标 Done 时写入「完成项目 cwd」；Workspace::render 取走后
    /// 触发同项目自动续跑下一条待办。None = 本帧无需续跑。
    pending_task_continue_cwd: Option<String>,
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
    /// 快捷启动实际命令行（硬重启守护 / 冷启动 id 不存在时用来重跑 agent）。
    /// 与 launch_label 分离：label 给人看，cmd 给 shell 跑。
    launch_cmd: Option<String>,
    /// 首帧布局后强制发一次 PTY resize（含真实 cell 像素）。reattach 后守护 jolt
    /// 用 cell=0；普通 `resize` 同尺寸早退——两者都盖不住「同网格但缺像素」的 TUI 排版。
    pty_kick_pending: bool,
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

/// 权限菜单里的一个可选项（从终端网格解析）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionOption {
    /// 注入 PTY 的键（通常是 `"1"` / `"2"` / `"3"`）。
    pub key: String,
    /// 选项原文。
    pub label: String,
    pub kind: PermissionOptionKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionOptionKind {
    Allow,
    Deny,
    Other,
}

/// 从可视区扫出的权限提示（Claude Code 等 TUI 数字菜单）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionPrompt {
    pub summary: Option<String>,
    pub options: Vec<PermissionOption>,
}

impl PermissionOption {
    /// 总览按钮短标签。
    pub fn button_label(&self) -> String {
        match self.kind {
            PermissionOptionKind::Allow => {
                let l = self.label.to_ascii_lowercase();
                if l == "yes" || l.starts_with("yes.") || l == "allow" || l == "approve" {
                    "允许".into()
                } else {
                    truncate_chars(&self.label, 16)
                }
            }
            PermissionOptionKind::Deny => {
                let l = self.label.to_ascii_lowercase();
                if l == "no"
                    || l.starts_with("no,")
                    || l.starts_with("no ")
                    || l == "deny"
                    || l == "reject"
                    || l == "cancel"
                    || self.label.starts_with('否')
                {
                    "拒绝".into()
                } else {
                    truncate_chars(&self.label, 16)
                }
            }
            PermissionOptionKind::Other => truncate_chars(&self.label, 16),
        }
    }

    /// 主按钮：第一个「允许」类且不是「不再询问」。
    pub fn is_primary(&self) -> bool {
        matches!(self.kind, PermissionOptionKind::Allow)
            && !self.label.to_ascii_lowercase().contains("don't ask")
            && !self.label.contains("不再")
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    let t = s.trim();
    if t.chars().count() <= max {
        t.to_string()
    } else {
        format!(
            "{}…",
            t.chars().take(max.saturating_sub(1)).collect::<String>()
        )
    }
}

fn classify_option_label(label: &str) -> PermissionOptionKind {
    let l = label.to_ascii_lowercase();
    if l == "no"
        || l.starts_with("no,")
        || l.starts_with("no ")
        || l.starts_with("no.")
        || l.contains("deny")
        || l.contains("reject")
        || l.contains("cancel")
        || l.contains("refuse")
        || label.contains("拒绝")
        || label.starts_with('否')
    {
        return PermissionOptionKind::Deny;
    }
    if l == "yes"
        || l.starts_with("yes")
        || l.contains("allow")
        || l.contains("approve")
        || l.contains("proceed")
        || l.contains("accept")
        || label.contains("允许")
        || label.contains("批准")
        || label.starts_with('是')
    {
        return PermissionOptionKind::Allow;
    }
    PermissionOptionKind::Other
}

/// 尝试把一行解析成 `1. label` / `1) label` / `[1] label`。
/// TUI 画边框用的竖线：选项行常常长成 `│ ❯ 1. Yes`，边框不剥掉就认不出选项。
const BORDER_CHARS: [char; 6] = ['│', '|', '┃', '║', ' ', '\t'];

/// 高亮指针字符集——真实 TUI 里远不止 `❯`。这份清单与手机端
/// `remote-web/src/lib/parseChoiceMenu.ts` 的 OPTION_RE 对齐：那边踩过
/// 「旧正则只认 `>`，高亮的第 1 项被漏掉、跑到标题槽位里」的线上问题并修好了，
/// 这边一直没拿到那个补丁，于是同一个菜单手机认得出、桌面认不出。
///
/// 认不出的代价不是少个按钮，而是误操作：扫不到菜单 → 界面落回硬编码兜底
/// （批准=打 1 / 拒绝=打 3）→ 盲发，而真实菜单未必是这个顺序。
const POINTER_CHARS: [char; 15] = [
    '❯', '>', '›', '▶', '►', '→', '➜', '•', '●', '◆', '*', '✦', '➢', '➤', ' ',
];

fn parse_numbered_option_line(raw: &str) -> Option<(String, String)> {
    let line = raw.trim();
    if line.is_empty() {
        return None;
    }
    // 先剥边框，再剥高亮指针（顺序不能反：指针画在边框里侧）。
    let line = line
        .trim_start_matches(BORDER_CHARS)
        .trim_start()
        .trim_start_matches(POINTER_CHARS)
        .trim_start();

    if let Some(rest) = line.strip_prefix('[') {
        let (n, after) = rest.split_once(']')?;
        let n = n.trim();
        if n.is_empty() || n.len() > 2 || !n.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let label = after
            .trim()
            .trim_start_matches(['.', ')', ':', '-', ' '])
            .trim();
        if label.is_empty() {
            return None;
        }
        return Some((n.to_string(), label.to_string()));
    }

    let digits: String = line.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() || digits.len() > 2 {
        return None;
    }
    let rest = line[digits.len()..].trim_start();
    // 必须有分隔符，避免把 `1foo` 当选项。含中文 TUI 常用的顿号与全角句点
    // （与手机端 OPTION_RE 的 [\.．、:)\]] 对齐）。
    let rest = rest
        .strip_prefix('.')
        .or_else(|| rest.strip_prefix('．'))
        .or_else(|| rest.strip_prefix('、'))
        .or_else(|| rest.strip_prefix(')'))
        .or_else(|| rest.strip_prefix(':'))?;
    let label = rest.trim();
    if label.is_empty() || label.starts_with('/') || label.starts_with("http") {
        return None;
    }
    Some((digits, label.to_string()))
}

/// 从终端末尾行解析权限数字菜单（Claude Code 等）。
///
/// 典型形态：
/// ```text
/// Do you want to proceed?
/// ❯ 1. Yes
///   2. Yes, and don't ask again …
///   3. No, and tell Claude what to do differently
/// ```
pub fn parse_permission_prompt(lines: &[String]) -> Option<PermissionPrompt> {
    let mut options: Vec<PermissionOption> = Vec::new();
    let mut option_line_idxs: Vec<usize> = Vec::new();

    for (i, raw) in lines.iter().enumerate() {
        let Some((key, label)) = parse_numbered_option_line(raw) else {
            continue;
        };
        options.push(PermissionOption {
            key,
            label: label.clone(),
            kind: classify_option_label(&label),
        });
        option_line_idxs.push(i);
    }

    if options.len() < 2 {
        return None;
    }

    let has_perm_word = options
        .iter()
        .any(|o| !matches!(o.kind, PermissionOptionKind::Other));
    let first = *option_line_idxs.first()?;
    let last = *option_line_idxs.last()?;
    let ctx_start = first.saturating_sub(4);
    let context_hint = lines[ctx_start..=last].iter().any(|l| {
        let t = l.to_ascii_lowercase();
        t.contains("permission")
            || t.contains("approv")
            || t.contains("proceed")
            || t.contains("allow")
            || t.contains("do you want")
            || t.contains("bash command")
            || t.contains("tool call")
            || t.contains("run this")
            || t.contains("权限")
            || t.contains("批准")
            || t.contains("是否")
            || t.contains("允许")
            || t.contains("esc to cancel")
            || t.contains("don't ask")
    });
    if !has_perm_word && !context_hint {
        return None;
    }

    let summary = lines[..first].iter().rev().find_map(|l| {
        let t = l.trim();
        if t.is_empty() || parse_numbered_option_line(t).is_some() {
            return None;
        }
        Some(truncate_chars(t, 120))
    });

    // 不截断：被截掉的选项在界面上永远够不着，而 Claude Code 的权限菜单确实会有
    // 5 项（Yes / Yes 别再问 / 仅此一次 / 先编辑命令 / No）。上游 parse 已经用
    // 「2~12 项 + 序号连续」把误报挡住了，这里再砍一刀只会让真菜单缺项。
    Some(PermissionPrompt { summary, options })
}

/// 同一终端同文本的系统通知最小间隔。
const NOTIFY_DEDUP: Duration = Duration::from_secs(60);

/// 标题是否以 braille spinner（U+2801–U+28FF）开头 —— 与 Session::status 的 Running 判定一致。
fn title_is_running(title: Option<String>) -> bool {
    title.is_some_and(|t| crate::osc::title_starts_with_spinner(&t))
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
    /// 用已 spawn 的 `Terminal` 包一层视图。**这是唯一的构造入口**——先
    /// `Terminal::spawn`（可失败）再决定是否建 Entity；不提供包着 expect 的
    /// 便捷构造：所有调用方都在 GPUI 的 ObjC 回调栈上（启动
    /// did_finish_launching / 用户事件），panic 不能跨 FFI unwind，一炸就是
    /// 整个 app abort——历史上「重启就崩」「拖文件夹就崩」都是它。
    pub fn from_terminal(
        cx: &mut Context<Self>,
        terminal: Terminal,
        cwd: Option<String>,
        session_id: String,
        launch: Option<&str>,
        launch_label: Option<&str>,
    ) -> Self {
        // Zed 式事件驱动重绘：读线程一有新内容就唤醒这里 cx.notify()（见 drive_redraws）。
        Self::drive_redraws(terminal.redraw_channel(), cx);
        let launch_kind = classify_launch(launch);
        let launch_cmd = launch
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
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
                    // 绑了本 session 的任务 → Done；若确实收尾了任务，挂旗让 Workspace
                    // 同项目自动 claim 下一条待办（见 `on_session_task_idle`）。
                    let sid = this.session_id.clone();
                    if let Some(cwd) = crate::tasks::TaskStore::mark_session_done(&sid) {
                        this.pending_task_continue_cwd = Some(cwd);
                    }
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
                // 注：内容变化的重绘现在主要由事件驱动任务（drive_redraws）负责——读线程
                // 一有输出就唤醒；这里的 content_changed 仍保留，一是驱动外观变化的重绘，
                // 二是作内容重绘的兜底（万一 channel 那条漏了，轮询还能兜住）。
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
                    // 滚动/输出变了：搜索高亮要按新的 display_offset 重算可视区命中。
                    this.refresh_search_highlights();
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
            was_focused: false,
            last_frame: None,
            marked_text: None,
            title,
            custom_title: None,
            cwd,
            selecting: false,
            app_mouse: false,
            search_open: false,
            search_input: None,
            _search_sub: None,
            search_hits: Vec::new(),
            search_status: terminal::SearchStatus::default(),
            scrollbar_drag: None,
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
            pending_task_continue_cwd: None,
            last_appearance: None,
            scroll_accum: 0.0,
            launch_kind,
            launch_label,
            launch_cmd,
            pty_kick_pending: true,
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

    /// 快捷启动实际命令行（硬重启守护时重跑 agent 用）；裸终端为 None。
    pub fn launch_cmd(&self) -> Option<&str> {
        self.launch_cmd.as_deref()
    }

    /// 当前注意力等级：有待处理通知时按文本分类（等审批 > 一般注意）。
    pub fn attention_kind(&self) -> Option<AttentionKind> {
        self.notification.as_deref().map(classify_attention)
    }

    /// 从终端末尾网格解析权限菜单；无菜单时 `None`。
    pub fn permission_prompt(&self) -> Option<PermissionPrompt> {
        let lines = self.last_lines(28);
        parse_permission_prompt(&lines)
    }

    /// 是否像「等审批」：OSC 文案分类 **或** 网格里扫到权限菜单。
    pub fn is_awaiting_approval(&self) -> bool {
        matches!(self.attention_kind(), Some(AttentionKind::Approval))
            || self.permission_prompt().is_some()
    }

    /// 是否「任务完成未读」（Running→Idle 后用户还没回应过）。
    pub fn completed_unread(&self) -> bool {
        self.completed_unread
    }

    /// 守护里的会话 id（关 pane 时用它让守护真正杀掉 shell）。
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// 驱动重绘的常驻任务：await 读线程的唤醒 → `cx.notify()`。内容一到就画，
    /// 不靠轮询。读线程退出（发送端 drop）时 recv 返回 Err，任务随之结束。
    fn drive_redraws(rx: smol::channel::Receiver<()>, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            while rx.recv().await.is_ok() {
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break; // 视图已销毁
                }
            }
        })
        .detach();
    }

    /// 守护整个重启后（旧会话随守护进程一起死掉，见 `terminal::restart_daemon`），
    /// 换一个全新会话顶替冻结的旧连接——同 id 在全新守护里查无此会话，走 `handle_open`
    /// 的新建分支，等效于重开一个终端。旧网格尺寸不丢：grid_size 仍是上次量到的值，
    /// 下一帧 render() 会照常把新终端 resize 到位，用户侧只是内容被清空重开。
    /// 连不上守护就原地不动（仍是冻结的旧终端），不 panic。
    ///
    /// **注意**：`Terminal::spawn` 内部会 sleep 重试，禁止在 UI 线程对多 pane 连环调用；
    /// 硬重启请走 [`Self::adopt_terminal`]（后台建好再塞进来）。
    pub fn reconnect(&mut self, cx: &mut Context<Self>) {
        // 带 launch_cmd：硬重启后守护里 id 已不存在，新建会话时要重跑 agent。
        let Ok(terminal) = Terminal::spawn(
            24,
            80,
            self.cwd.as_deref(),
            &self.session_id,
            self.launch_cmd.as_deref(),
        ) else {
            return;
        };
        self.adopt_terminal(terminal, cx);
    }

    /// 用已经在后台线程建好的 [`Terminal`] 替换当前连接（硬重启守护后批量重连用）。
    pub fn adopt_terminal(&mut self, terminal: Terminal, cx: &mut Context<Self>) {
        self.terminal = terminal;
        // 旧 Terminal 一 drop，它读线程的发送端随之关闭，老的 redraw 任务 recv 到 Err
        // 自行退出；这里给新连接挂一个新的重绘任务。
        Self::drive_redraws(self.terminal.redraw_channel(), cx);
        self.notification = None;
        self.notified_at = None;
        self.was_running = false;
        self.running_frames = 0;
        self.stuck_notified = false;
        self.completed_unread = false;
        self.last_notified = None;
        // 新 Terminal 自带空选区，只需重置本视图的拖选 / 应用鼠标交互态。
        self.selecting = false;
        self.app_mouse = false;
        self.drag_scroll = 0;
        self.cursor = None;
        // 重连后必须再 force 一次带 cell 像素的 resize（见 pty_kick_pending）。
        self.pty_kick_pending = true;
        cx.notify();
    }

    /// 最近通知时刻（总览页「N 分钟前」用）。
    pub fn notified_at(&self) -> Option<Instant> {
        self.notified_at
    }


    /// 终端末尾最多 n 行非空文本（总览页迷你预览用）。走 [`Terminal::text_lines`] 的纯文本
    /// 路径，不为了几行字把整个网格连同颜色/属性 clone 一遍。
    pub fn last_lines(&self, n: usize) -> Vec<String> {
        let mut lines = self.terminal.text_lines();
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

    /// 取走「任务完成 → 自动续跑」挂旗（完成项目 cwd）；Workspace::render 每帧调用。
    pub fn take_pending_task_continue(&mut self) -> Option<String> {
        self.pending_task_continue_cwd.take()
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
    /// 走 [`Terminal::paste`]：bracketed paste + 换行规范化，跟 Cmd+V 同一条路。
    ///
    /// **不会提交**：Claude 等开了 bracketed paste 时，粘贴内容里的 `\n` 只是多行文本。
    /// 需要回车执行时用 [`Self::send_text_and_submit`]。
    pub fn send_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.terminal.paste(text);
        self.notification = None;
        self.completed_unread = false;
        cx.notify();
    }

    /// 把正文当作**键盘输入**写入 PTY（不走 bracketed paste）。
    pub fn type_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let body = text.trim_end_matches(['\n', '\r']);
        if !body.is_empty() {
            self.terminal.send_input(body.as_bytes());
        }
        self.notification = None;
        self.completed_unread = false;
        cx.notify();
    }

    /// 发送一次 Enter（裸 `\r`）。
    pub fn send_enter(&mut self, cx: &mut Context<Self>) {
        self.terminal.send_input(b"\r");
        self.notification = None;
        self.completed_unread = false;
        cx.notify();
    }

    /// 键入正文并回车（已有终端上执行任务用）。
    pub fn send_text_and_submit(&mut self, text: &str, cx: &mut Context<Self>) {
        self.type_text(text, cx);
        self.send_enter(cx);
    }

    /// 打开终端内搜索条（Cmd+F）。输入框获焦；Enter 下一个，Shift+Enter 上一个，Esc 关闭。
    pub fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use gpui_component::input::{InputEvent, InputState};
        if self.search_open {
            if let Some(input) = &self.search_input {
                input.update(cx, |s, cx| s.focus(window, cx));
            }
            return;
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("在终端中查找…"));
        input.update(cx, |s, cx| s.focus(window, cx));
        self._search_sub = Some(cx.subscribe_in(
            &input,
            window,
            |this, input, ev: &InputEvent, _window, cx| {
                match ev {
                    InputEvent::PressEnter { shift, .. } => {
                        let q = input.read(cx).value().to_string();
                        this.run_search(&q, *shift, cx);
                    }
                    InputEvent::Change => {
                        let q = input.read(cx).value().to_string();
                        // 边输入边重建命中列表，全部高亮；不滚动（等 Enter 再跳）。
                        this.search_status = this.terminal.set_search_query(&q);
                        this.search_hits = this.terminal.viewport_search_hits();
                        cx.notify();
                    }
                    _ => {}
                }
            },
        ));
        self.search_input = Some(input);
        self.search_open = true;
        self.search_hits.clear();
        self.search_status = terminal::SearchStatus::default();
        self.terminal.clear_search();
        cx.notify();
    }

    pub fn close_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search_open = false;
        self.search_input = None;
        self._search_sub = None;
        self.search_hits.clear();
        self.search_status = terminal::SearchStatus::default();
        self.terminal.clear_search();
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn run_search(&mut self, query: &str, backward: bool, cx: &mut Context<Self>) {
        self.search_status = self.terminal.find_next(query, backward);
        self.search_hits = self.terminal.viewport_search_hits();
        cx.notify();
    }

    fn refresh_search_highlights(&mut self) {
        if self.search_open {
            self.search_hits = self.terminal.viewport_search_hits();
            self.search_status = self.terminal.search_status();
        }
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
        let Some(cells) = self.last_frame.as_ref().and_then(|f| f.rows.get(row)) else {
            return hi - lo;
        };
        cells[lo..hi.min(cells.len())].iter().filter(|c| c.ch != '\0').count()
    }

    /// 点击单元处若落在某个链接上，返回该目标（未做 file:// 转换，打开前还要经
    /// [`open_target`]）。
    fn url_at(&self, (r, c): (usize, usize)) -> Option<String> {
        let row = self.last_frame.as_ref()?.rows.get(r)?;
        link_at(row, c).map(|(_, _, url)| url)
    }

    /// 单元处链接的范围 (行, 起列, 止列)，用于悬停高亮。
    fn link_range_at(&self, (r, c): (usize, usize)) -> Option<(usize, usize, usize)> {
        let row = self.last_frame.as_ref()?.rows.get(r)?;
        link_at(row, c).map(|(a, b, _)| (r, a, b))
    }
}

/// 某一格上的链接：(起列, 止列, 目标)。
///
/// **先看 OSC 8**（`Cell::link`，终端协议层的链接）：`eza` / `gh` / `cargo` 这类输出里，
/// 可见文本往往只是标题、真正的 URL 藏在协议里，正则扫可见文本根本找不到。没有 OSC 8
/// 才回退到正则扫出来的 URL / 本地路径（[`find_links`]）。
fn link_at(row: &[terminal::Cell], c: usize) -> Option<(usize, usize, String)> {
    if let Some(uri) = row.get(c).and_then(|cell| cell.link.clone()) {
        // 同一个链接铺在连续若干格上，向两侧扩到 uri 变化为止。
        let same = |i: usize| row.get(i).and_then(|x| x.link.as_deref()) == Some(&*uri);
        let mut a = c;
        while a > 0 && same(a - 1) {
            a -= 1;
        }
        let mut b = c;
        while b + 1 < row.len() && same(b + 1) {
            b += 1;
        }
        return Some((a, b, uri.to_string()));
    }
    find_links(row).into_iter().find(|&(a, b, _)| c >= a && c <= b)
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
                let cell_w_px = cell_w.round().clamp(1.0, 64.0) as u16;
                let cell_h_px = line_px().round().clamp(1.0, 128.0) as u16;
                if self.pty_kick_pending {
                    // 首帧 / reattach：无条件发 resize（含真实 cell 像素）。
                    // 守护 jolt 用 cell=0；普通 resize 同尺寸会早退——两处都补不到像素。
                    self.terminal
                        .force_resize(grid_rows, cols, cell_w_px, cell_h_px);
                    self.pty_kick_pending = false;
                } else {
                    self.terminal.resize(grid_rows, cols, cell_w_px, cell_h_px);
                }
            }
        }

        // 这一帧的网格：渲染和命中测试（url_at / link_range_at / char_steps_between）共用，
        // 见 last_frame 字段注释。
        let frame = Rc::new(self.terminal.snapshot());
        self.last_frame = Some(frame.clone());
        // 画反色块用可见光标（应用 CSI ?25l 藏光标时为 None）；IME 候选窗/预编辑
        // 定位、Option+点击移光标用**位置**（cursor_pos，含隐藏）——TUI 藏了光标
        // 输入法照样要知道往哪落。
        //
        // IME 合成中不画网格里的光标：预编辑串自带光标（画在拼音末尾，跟 iTerm2 一致）。
        let cursor = if self.marked_text.is_some() { None } else { frame.cursor };
        self.cursor = frame.cursor_pos;
        // 失焦的终端把光标画成空心框（见 paint_row）——多个终端并排时才看得出焦点在谁身上。
        let focused = self.focus_handle.is_focused(window);
        // 焦点变化上报给应用（DEC 1004；没开这个模式的应用收不到，见 report_focus）。
        if focused != self.was_focused {
            self.was_focused = focused;
            self.terminal.report_focus(focused);
        }
        let hover_url = self.hover_url;
        let has_hover = hover_url.is_some();
        // 滚动会改 display_offset：每帧按当前 offset 把绝对命中映到可视区。
        if self.search_open {
            self.search_hits = self.terminal.viewport_search_hits();
            self.search_status = self.terminal.search_status();
        }
        let search_hits = self.search_hits.clone();
        let search_status = self.search_status;
        let base_font = terminal_font();
        // 网格列宽：paint_row 用它把每一批文本钉到 col * cell_w 上（见 paint_row 头注）。
        let cell_w = self.cell_w;

        // IME 合成中的拼音预编辑串（marked text）：macOS 的分工是候选词浮窗由系统画
        // （bounds_for_range 只负责告诉它摆哪），**预编辑串由应用自己画**——不画的话
        // 打拼音就是盲打，只有候选窗没有输入回显。交给 paint_row 画在光标所在行的行内，
        // 光标已上滚离开可视区（cursor_pos 为 None）时自然不画。
        let ime = self.marked_text.clone().zip(frame.cursor_pos);
        let fh = self.focus_handle.clone();
        let entity = cx.entity();
        let origin_cell = self.grid_origin.clone();
        let size_cell = self.grid_size.clone();
        let search_open = self.search_open;
        let search_input = self.search_input.clone();
        let scroll_info = self.terminal.scroll_info();

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

        let menu_sid = self.session_id.clone();
        let menu_cwd = self.cwd.clone();
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
            .on_action(cx.listener(|this, _: &TerminalFind, window, cx| {
                this.open_search(window, cx);
            }))
            .on_action(cx.listener(|this, _: &TerminalFindNext, _window, cx| {
                if let Some(input) = &this.search_input {
                    let q = input.read(cx).value().to_string();
                    this.run_search(&q, false, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &TerminalFindPrev, _window, cx| {
                if let Some(input) = &this.search_input {
                    let q = input.read(cx).value().to_string();
                    this.run_search(&q, true, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &TerminalFindClose, window, cx| {
                this.close_search(window, cx);
            }))
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                let ks = &ev.keystroke;
                let m = &ks.modifiers;
                // 搜索条打开时：Esc 关闭；其它键留给输入框（不要灌进 PTY）。
                if this.search_open {
                    if ks.key == "escape" {
                        this.close_search(window, cx);
                    }
                    return;
                }
                // IME 合成中：这些键归输入法（backspace 删拼音、enter/space/数字选词），
                // 不能再往 PTY 发一份，否则终端会当成真实按键吃掉。上屏的文字走
                // replace_text_in_range 进来。
                if this.marked_text.is_some() && !m.platform {
                    return;
                }
                // Cmd+F 打开搜索（action 也会绑，这里兜底）。
                if m.platform && ks.key == "f" {
                    this.open_search(window, cx);
                    return;
                }
                // Cmd+C 复制选区（alacritty 按缓冲区绝对行取文本，跨屏选区也完整）
                if m.platform && ks.key == "c" {
                    if let Some(text) = this.terminal.selection_text() {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                    }
                    return;
                }
                // Cmd+V 粘贴：读剪贴板写入 PTY（bracketed paste / 换行规范化见 Terminal::paste）
                if m.platform && ks.key == "v" {
                    if let Some(text) = cx.read_from_clipboard().and_then(|it| it.text()) {
                        this.terminal.paste(&text);
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
                    // 应用开了鼠标上报且没按 Shift → 把 press 转发给 TUI（vim/less/
                    // Claude 等靠这个点选）。Shift 旁路 = 强制本地框选（xterm 约定）。
                    // 双击/三击永远走本地选词/选行（应用鼠标协议没有语义选区）。
                    let app_wants_mouse = this.terminal.mouse_mode()
                        && !ev.modifiers.shift
                        && ev.click_count <= 1;
                    if app_wants_mouse && this.terminal.mouse_button(0, true, cell.0, cell.1) {
                        this.app_mouse = true;
                        this.selecting = false;
                        this.terminal.selection_clear();
                        cx.notify();
                        return;
                    }
                    this.app_mouse = false;
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
                    let (row, col) = this.pos_to_cell(ev.position, window);
                    if this.app_mouse {
                        // TUI 拖选/拖动：左键 motion（button 32）。
                        this.terminal.mouse_drag(0, row, col);
                        return;
                    }
                    if this.selecting {
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
                    let cell = this.pos_to_cell(ev.position, window);
                    // 全开 MOUSE_MOTION 时无键悬停也上报（button 35）
                    if this.terminal.mouse_mode() && !ev.modifiers.shift {
                        this.terminal.mouse_motion(cell.0, cell.1);
                    }
                    // 按住 Cmd 悬停链接：记录链接范围（用于高亮 + 手型）
                    let hl = if ev.modifiers.platform {
                        this.link_range_at(cell)
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
            // 中键：MOUSE_MODE 时转发给应用；否则粘贴剪贴板（X11 风格，macOS 触控板少见）
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                    window.focus(&this.focus_handle, cx);
                    let cell = this.pos_to_cell(ev.position, window);
                    if !ev.modifiers.shift && this.terminal.mouse_button(1, true, cell.0, cell.1) {
                        return;
                    }
                    if let Some(text) = cx.read_from_clipboard().and_then(|it| it.text()) {
                        this.terminal.paste(&text);
                        this.notification = None;
                        this.completed_unread = false;
                        cx.notify();
                    }
                }),
            )
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(|this, ev: &MouseUpEvent, window, _cx| {
                    if !ev.modifiers.shift {
                        let cell = this.pos_to_cell(ev.position, window);
                        this.terminal.mouse_button(1, false, cell.0, cell.1);
                    }
                }),
            )
            // 右键：TUI 开了鼠标上报时转发给应用（Shift 旁路 → 系统菜单）；
            // 否则不转发，交给 context_menu（新建任务等）。
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                    window.focus(&this.focus_handle, cx);
                    if ev.modifiers.shift {
                        return;
                    }
                    if this.terminal.mouse_mode() {
                        let cell = this.pos_to_cell(ev.position, window);
                        this.terminal.mouse_button(2, true, cell.0, cell.1);
                    }
                }),
            )
            .on_mouse_up(
                MouseButton::Right,
                cx.listener(|this, ev: &MouseUpEvent, window, _cx| {
                    if ev.modifiers.shift {
                        return;
                    }
                    if this.terminal.mouse_mode() {
                        let cell = this.pos_to_cell(ev.position, window);
                        this.terminal.mouse_button(2, false, cell.0, cell.1);
                    }
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseUpEvent, window, cx| {
                    this.drag_scroll = 0;
                    // 应用鼠标路径：补发 release，不再碰本地选区。
                    if this.app_mouse {
                        this.app_mouse = false;
                        this.selecting = false;
                        let cell = this.pos_to_cell(ev.position, window);
                        this.terminal.mouse_button(0, false, cell.0, cell.1);
                        cx.notify();
                        return;
                    }
                    this.selecting = false;
                    // 真的拖出了非空选区：选中即复制（iTerm2 copy-on-select）。TUI 重绘
                    // 会清掉 alacritty 选区，松手瞬间进剪贴板才稳。
                    if let Some(text) = this.terminal.selection_text() {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                        return;
                    }
                    // 未拖动的本地单击：若应用开了鼠标但 mousedown 没接管（比如当时
                    // 按着 Shift，现在松了），仍可在 mouseup 发一次 click 脉冲——但
                    // 当前若 mouse_mode 且没 shift，mousedown 已经走 app 路径了。
                    // 这里只清空选区。
                    this.terminal.selection_clear();
                    cx.notify();
                }),
            )
            // 背景层（最底）：底色 / 背景图 / 透明度
            .child(bg_layer)
            // 终端主体：逐行画 alacritty 网格快照（底色 / 文字 / 光标 / 选区 / IME）。
            //
            // 走 canvas 直接 paint、而不是「每行一个 div + StyledText」：网格对齐要靠
            // `shape_line(force_width = cell_w)`（见 paint_row 头注），而 StyledText 走的是
            // shape_text，压根没有这个参数。顺带也省掉了每行每批一个元素的布局开销。
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, cx| {
                        let rows = &frame.rows;
                        // 网格原点吸到设备像素上（照 Zed：terminal_element.rs:1062，它的注释说
                        // 分数原点会让字形在帧与帧之间抖，看着像闪烁）。分屏时布局给的 bounds
                        // 很容易落在半个像素上。
                        let scale = window.scale_factor();
                        let snap = |v: Pixels| px((f32::from(v) * scale).floor() / scale);
                        let ox = snap(bounds.origin.x + px(PAD_X));
                        let oy = snap(bounds.origin.y + px(PAD_Y));
                        for (r, row) in rows.iter().enumerate() {
                            let cur = match cursor {
                                Some((cr, cc, kind)) if cr == r => Some((cc, kind)),
                                _ => None,
                            };
                            let hl = match hover_url {
                                Some((hr, a, b)) if hr == r => Some((a, b)),
                                _ => None,
                            };
                            // 同一行可能有多段命中；传整行命中列表给 paint_row。
                            let row_hits: Vec<(usize, usize, bool)> = search_hits
                                .iter()
                                .filter(|h| h.row == r)
                                .map(|h| (h.col_start, h.col_end, h.active))
                                .collect();
                            let ime_here = match &ime {
                                Some((text, (ir, ic))) if *ir == r => Some((text.as_str(), *ic)),
                                _ => None,
                            };
                            let origin = point(ox, oy + px(r as f32 * line_px()));
                            paint_row(
                                row,
                                origin,
                                cur,
                                focused,
                                &base_font,
                                hl,
                                &row_hits,
                                cell_w,
                                ime_here,
                                window,
                                cx,
                            );
                        }
                    },
                )
                .absolute()
                .inset_0(),
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
            // 搜索条：叠在顶部，不抢网格布局（absolute）
            .when(search_open, |root| {
                let status_label = if search_status.total == 0 {
                    "无结果".to_string()
                } else {
                    format!("{}/{}", search_status.current, search_status.total)
                };
                let bar = div()
                    .absolute()
                    .top_2()
                    .right_2()
                    .w(px(300.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(if terminal::is_dark() {
                        rgb(0x0024_283b)
                    } else {
                        rgb(0x00ff_ffff)
                    })
                    .border_1()
                    .border_color(if terminal::is_dark() {
                        rgb(0x003d_4a6a)
                    } else {
                        rgb(0x00d0_d7de)
                    })
                    .shadow_md()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(if let Some(input) = search_input {
                                Input::new(&input).cleanable(true).into_any_element()
                            } else {
                                div().into_any_element()
                            }),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(if terminal::is_dark() {
                                0x0080_8a9a
                            } else {
                                0x0057_6069
                            }))
                            .child(status_label),
                    );
                root.child(bar)
            })
            // 滚动条：有 scrollback 时画在右侧；拖 thumb / 点轨道跳转。
            .when(scroll_info.max_offset > 0, |root| {
                root.child(self.render_scrollbar(scroll_info, cx))
            })
            // TUI 未开鼠标模式时右键出菜单；开了则右键转给应用（见 on_mouse_down）。
            .context_menu(move |menu, _window, _cx| {
                let sid = menu_sid.clone();
                let cwd = menu_cwd.clone();
                menu.item(PopupMenuItem::new("新建任务").on_click(move |_ev, window, cx| {
                    *cx.default_global::<NewTaskPrefill>() = NewTaskPrefill {
                        session_id: Some(sid.clone()),
                        cwd: cwd.clone(),
                    };
                    window.dispatch_action(Box::new(NewTask), cx);
                }))
            })
    }
}

/// 滚动条轨道宽度（像素）。
const SCROLLBAR_W: f32 = 9.0;
/// thumb 最短高度，太短不好点。
const SCROLLBAR_THUMB_MIN: f32 = 28.0;

/// 滚动条 thumb 几何：返回 (thumb 高度, thumb 顶部 y)。
/// offset=0 → thumb 在底部；offset=max → 顶部（跟 alacritty display_offset 一致）。
fn scrollbar_thumb(track_h: f32, viewport_rows: usize, max_offset: usize, offset: usize) -> (f32, f32) {
    let total = viewport_rows.saturating_add(max_offset).max(1);
    // 首帧 grid_size 未量出来时轨道可能比 THUMB_MIN 还矮，min 必须让位，
    // 否则 clamp 遇到 min > max 直接 panic。
    let thumb_min = SCROLLBAR_THUMB_MIN.min(track_h);
    let thumb_h = ((viewport_rows as f32 / total as f32) * track_h).clamp(thumb_min, track_h);
    let travel = (track_h - thumb_h).max(0.0);
    let thumb_y = if max_offset == 0 {
        0.0
    } else {
        travel * (1.0 - offset as f32 / max_offset as f32)
    };
    (thumb_h, thumb_y)
}

impl TerminalView {
    /// 右侧滚动条：offset=0 贴底，offset=max 贴顶（跟 alacritty display_offset 一致）。
    fn render_scrollbar(
        &self,
        info: terminal::ScrollInfo,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let (_, h) = self.grid_size.get();
        let track_h = (h - 2.0 * PAD_Y).max(1.0);
        let (thumb_h, thumb_y) =
            scrollbar_thumb(track_h, info.viewport_rows, info.max_offset, info.offset);
        let thumb_color = if terminal::is_dark() {
            rgb(0x005a_657a)
        } else {
            rgb(0x00af_b8c1)
        };
        let max_off = info.max_offset;
        let viewport = info.viewport_rows;

        div()
            .id("term-scrollbar")
            .absolute()
            .top(px(PAD_Y))
            .right(px(2.0))
            .w(px(SCROLLBAR_W))
            .h(px(track_h))
            .rounded_full()
            .bg(if terminal::is_dark() {
                rgba(0x00_2c_3149_55)
            } else {
                rgba(0x00_d0_d7_de_66)
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                    let (_, oy) = this.grid_origin.get();
                    let y = (f32::from(ev.position.y) - oy).clamp(0.0, track_h);
                    // 点在 thumb 上：开始拖；点在轨道：跳到对应位置
                    if y >= thumb_y && y <= thumb_y + thumb_h {
                        this.scrollbar_drag = Some(y - thumb_y);
                    } else {
                        this.scrollbar_drag = None;
                        this.jump_scrollbar_to(y, track_h, thumb_h, max_off, cx);
                    }
                    window.prevent_default();
                    cx.notify();
                }),
            )
            .on_mouse_move(cx.listener(move |this, ev: &MouseMoveEvent, _window, cx| {
                if let Some(grab) = this.scrollbar_drag {
                    if ev.pressed_button == Some(MouseButton::Left) {
                        let (_, oy) = this.grid_origin.get();
                        let y = (f32::from(ev.position.y) - oy - grab).clamp(0.0, track_h - thumb_h);
                        this.jump_scrollbar_to(y + thumb_h * 0.5, track_h, thumb_h, max_off, cx);
                    }
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _ev, _window, cx| {
                    this.scrollbar_drag = None;
                    cx.notify();
                }),
            )
            .child(
                div()
                    .absolute()
                    .top(px(thumb_y))
                    .left(px(1.0))
                    .w(px(SCROLLBAR_W - 2.0))
                    .h(px(thumb_h))
                    .rounded_full()
                    .bg(thumb_color)
                    // 占位避免编译器抱怨 viewport 未用
                    .when(viewport == 0, |d| d),
            )
    }

    /// 根据轨道上的 y（相对网格顶）设置 display_offset。
    fn jump_scrollbar_to(
        &mut self,
        y: f32,
        track_h: f32,
        thumb_h: f32,
        max_off: usize,
        cx: &mut Context<Self>,
    ) {
        if max_off == 0 || track_h <= thumb_h {
            self.terminal.set_scroll_offset(0);
        } else {
            // thumb 中心位置 → 0..=1，再反转到 offset（底=0）
            let center = y.clamp(thumb_h * 0.5, track_h - thumb_h * 0.5);
            let t = (center - thumb_h * 0.5) / (track_h - thumb_h);
            let offset = ((1.0 - t) * max_off as f32).round() as usize;
            self.terminal.set_scroll_offset(offset.min(max_off));
        }
        self.refresh_search_highlights();
        cx.notify();
    }
}

/// 画一行。**终端是网格，第 N 列就必须画在 N × cell_w**——字体的字形宽度只决定「字长
/// 什么样」，不决定「它在哪」。
///
/// 这一点靠 `shape_line(.., force_width = Some(cell_w))` 落实（跟 Zed 终端同一手法）：
/// GPUI 的 `apply_force_width_to_layout` 会把每个 glyph 的 x **强制钉到 `序号 × cell_w`**，
/// 字体自己的 advance 直接作废。于是批内位置也跟字宽彻底脱钩——中文 fallback 到 PingFang
/// 也好，`·` `—` 这类「东亚歧义宽度」字符（终端只给一格、字体却画成全角）也好，都只是
/// 自己画宽一点、覆盖到邻格上，**推不动后面任何一个字符**。
///
/// 这正是 div + StyledText 做不到的：`StyledText` 走 `shape_text`，只有 wrap_width、没有
/// force_width，批内只能交给排版器按字体 advance 自由流——`——正` 这种批里，两个破折号
/// 会把「正」一路顶右，撞到下一批钉在网格列上的字上（用户可见：中文叠字）。
///
/// 唯一还须守住的不变量：**宽字符必须落在批尾**。force_width 是按 glyph 序号钉位的，而
/// 宽字符占两格却只算一个 glyph，它后面若还有同批字符就会整体少一格。好在宽字符后面必
/// 跟 '\0' 占位格，使下一个字符列号对不上而断批（见 [`text_batches`] 及其测试）。
///
/// `ime`（预编辑串, 起始列）不为空时在行内叠一层：垫终端底色遮住底下的内容、下划线标示
/// 「合成中」，光标跟在拼音末尾（合成中网格里的光标块不画，见 render 里 cursor 的取值）。
#[allow(clippy::too_many_arguments)]
fn paint_row(
    row: &[terminal::Cell],
    origin: Point<Pixels>,
    cursor: Option<(usize, terminal::CursorKind)>, // (列, 形状)
    focused: bool,
    base_font: &Font,
    hover_link: Option<(usize, usize)>,
    // 本行搜索命中：(起列, 止列含, 是否当前 active)
    search_hits: &[(usize, usize, bool)],
    cell_w: f32,
    ime: Option<(&str, usize)>, // (预编辑串, 起始列 —— 来自 cursor_pos，含被 TUI 隐藏的光标)
    window: &mut Window,
    cx: &mut App,
) {
    // 失焦时一律画成空心框（跟 iTerm2 / Zed 一致，terminal_element.rs:1250）——驾驶舱里
    // 多个终端并排，每个都亮着一模一样的实心块的话，根本看不出焦点在谁身上。
    let cursor = cursor.map(|(col, kind)| {
        let kind = if focused { kind } else { terminal::CursorKind::Hollow };
        (col, kind)
    });
    // 只有实心块要把底下的字反色（底色由 bg_spans 画、字色由 style_of 换）。竖线 / 下划线 /
    // 空心框都不动文字，光标本身作为一个 quad 画在文字之上。
    let block_at = match cursor {
        Some((col, terminal::CursorKind::Block)) => Some(col),
        _ => None,
    };
    // 光标压在宽字符（中文/emoji）上时要盖满**两格**：第二格是 '\0' 占位，样式得跟着一起换，
    // 否则实心块只有半格（Zed 走的是 shaped_width.max(cell_width)，我们直接看占位格）。
    let is_wide_at = |col: usize| row.get(col + 1).is_some_and(|c| c.ch == '\0');
    let in_block = |i: usize| match block_at {
        Some(col) => i == col || (i == col + 1 && is_wide_at(col)),
        None => false,
    };

    let is_link = |i: usize| hover_link.map_or(false, |(a, b)| i >= a && i <= b);
    // 返回 (是否命中, 是否 active)。active 画得更亮。
    let search_at = |i: usize| -> Option<bool> {
        search_hits
            .iter()
            .find(|(a, b, _)| i >= *a && i <= *b)
            .map(|(_, _, active)| *active)
    };
    // 悬停链接：高亮色 + 下划线；再叠加光标反色 / 选区 / 搜索命中背景。
    let style_of = |i: usize| -> CellStyle {
        let c = &row[i];
        let mut fg = c.fg;
        // None = 默认底色（不画，让背景层透出），见 CellStyle::bg。
        let mut bg = (!c.bg_default).then_some(c.bg);
        // OSC 8 链接常驻下划线（跟 Zed 一致：terminal_element.rs:623 把 hyperlink 也算进
        // underline）——不然可见文本只是普通标题，用户根本看不出这里有链接可点。
        let mut underline = c.underline || c.link.is_some();
        if is_link(i) {
            fg = link_fg();
            underline = true;
        }
        if in_block(i) {
            // 光标实心块：底色取前景色，字色取原底色（默认底色时就是终端底色）。
            let under = bg.unwrap_or_else(terminal::default_bg);
            bg = Some(fg);
            fg = under;
        } else if c.selected {
            bg = Some(sel_bg());
        } else if let Some(active) = search_at(i) {
            // 搜索：普通命中暗琥珀，当前命中亮琥珀（跟选区蓝区分开）。
            bg = Some(if active {
                if terminal::is_dark() {
                    0x00d4_a017
                } else {
                    0x00ff_c107
                }
            } else if terminal::is_dark() {
                0x007a_5c20
            } else {
                0x00ff_e9a8
            });
        }
        CellStyle {
            fg,
            bg,
            bold: c.bold,
            italic: c.italic,
            dim: c.dim,
            underline,
            undercurl: c.undercurl,
            strikeout: c.strikeout,
        }
    };

    let h = px(line_px());
    let at = |col: usize| point(origin.x + px(col as f32 * cell_w), origin.y);
    let run_of = |text: &str, st: CellStyle| {
        let mut font = base_font.clone();
        if st.bold {
            font.weight = FontWeight::BOLD;
        }
        if st.italic {
            font.style = FontStyle::Italic;
        }
        let mut color = Hsla::from(rgb(st.fg));
        if st.dim {
            // faint(SGR 2)：压 alpha 而不是改 RGB——跟 Zed / alacritty 的观感一致。
            color.a *= 0.7;
        }
        TextRun {
            len: text.len(),
            font,
            color,
            background_color: None, // 底色走 quad，见 bg_spans
            underline: st.underline.then(|| UnderlineStyle {
                thickness: px(1.0),
                color: Some(color),
                wavy: st.undercurl,
            }),
            strikethrough: st.strikeout.then(|| StrikethroughStyle {
                thickness: px(1.0),
                color: Some(color),
            }),
        }
    };
    let shape = |text: String, run: TextRun, window: &mut Window| {
        window.text_system().shape_line(
            text.into(),
            px(font_px()),
            std::slice::from_ref(&run),
            Some(px(cell_w)), // ← 网格定位的关键，见函数头注
        )
    };

    // 底色**扫整行**（bg_spans 内部按 row.len() 走，没有截断参数可传错）。
    // 起点向下取整、宽度向上取整（照 Zed 的 LayoutRect::paint）：相邻两块不同底色的矩形
    // 若落在半个像素上，中间会透出一条缝，背景图 / 半透明底下尤其显眼。
    for (col, span, bg) in bg_spans(row, &style_of) {
        let x0 = at(col).x.floor();
        let x1 = (at(col).x + px(span as f32 * cell_w)).ceil();
        window.paint_quad(fill(Bounds::new(point(x0, origin.y), size(x1 - x0, h)), rgb(bg)));
    }

    // 字形只画到最后一个「非 blank」，尾部那些什么都不画的空格不必进批次。
    // 实心块光标压在尾部空格上时，那格要画（反色后的空格底色已由 bg_spans 铺好，这里是为了
    // 让批次覆盖到它——真正要紧的是块下若有字符，得用反色重画一遍）。
    let mut end = visible_end(row, &is_link);
    if let Some(col) = block_at {
        end = end.max((col + 1).min(row.len()));
    }
    for b in text_batches(row, end, &style_of) {
        let run = run_of(&b.text, b.style);
        if b.wide {
            // 宽字符：字形（约 1.0em）比两格（1.2em）窄，左对齐会让它贴着格子左边——光标块
            // （满两格）压上去时左右空隙就不对称，整行中文看着也都偏左。这里**不加**
            // force_width 地 shape 一次拿到真实字形宽度，再居中放进两格里。
            // （force_width 只会把 glyph 钉到 `序号 × cell_w`，做不了居中；宽字符为此独占
            // 一批，见 text_batches。）
            let shaped = window.text_system().shape_line(
                b.text.into(),
                px(font_px()),
                std::slice::from_ref(&run),
                None,
            );
            let slack = 2.0 * cell_w - f32::from(shaped.width);
            let x = at(b.col).x + px(slack.max(0.0) / 2.0);
            let _ = shaped.paint(point(x, origin.y), h, TextAlign::Left, None, window, cx);
        } else {
            let shaped = shape(b.text, run, window);
            let _ = shaped.paint(at(b.col), h, TextAlign::Left, None, window, cx);
        }
    }

    // 非实心块的光标形状：画在文字之上。宽度按宽字符占几格算。
    if let Some((col, kind)) = cursor {
        let fg = rgb(terminal::default_fg());
        let w = px(if is_wide_at(col) { 2.0 * cell_w } else { cell_w });
        let bounds = Bounds::new(at(col), size(w, h));
        match kind {
            // 实心块已经靠 bg_spans + 反色字画好了
            terminal::CursorKind::Block => {}
            terminal::CursorKind::Hollow => {
                window.paint_quad(outline(bounds, fg, BorderStyle::Solid));
            }
            terminal::CursorKind::Bar => {
                window.paint_quad(fill(Bounds::new(at(col), size(px(2.0), h)), fg));
            }
            terminal::CursorKind::Underline => {
                let y = origin.y + h - px(2.0);
                window.paint_quad(fill(
                    Bounds::new(point(at(col).x, y), size(w, px(2.0))),
                    fg,
                ));
            }
        }
    }

    if let Some((text, col)) = ime {
        let fg = terminal::default_fg();
        // 预编辑串：终端默认前景 + 下划线标示「合成中」。
        let run = run_of(text, CellStyle { fg, underline: true, ..CellStyle::default() });
        let shaped = shape(text.to_string(), run, window);
        let w = shaped.width;
        // 先垫底色盖住底下的终端内容，再画拼音，最后把光标接在末尾。
        window.paint_quad(fill(
            Bounds::new(at(col), size(w + px(cell_w), h)),
            rgb(terminal::default_bg()),
        ));
        let _ = shaped.paint(at(col), h, TextAlign::Left, None, window, cx);
        let cursor_at = point(at(col).x + w, origin.y);
        window.paint_quad(fill(Bounds::new(cursor_at, size(px(cell_w), h)), rgb(fg)));
    }
}

/// 一格的最终样式（cell 自带的属性 + 光标反色 / 选区 / 悬停链接叠加之后）。
/// 同样式且列号连续的格子会连成一批，见 [`text_batches`]。
#[derive(Clone, Copy, PartialEq, Eq)]
struct CellStyle {
    fg: u32,
    /// 底色。**None = 终端默认底色**，这种格子不画底色矩形，让底下的背景层 / 背景图 /
    /// 桌面透出来。用 Option 而不是「跟 default_bg() 比 RGB」：应用可以显式设一个恰好
    /// 等于默认底色的 RGB，那是真要画的一块底色（见 `Cell::bg_default`）。
    bg: Option<u32>,
    bold: bool,
    italic: bool,
    dim: bool,
    underline: bool,
    undercurl: bool,
    strikeout: bool,
}

impl Default for CellStyle {
    fn default() -> Self {
        Self {
            fg: terminal::default_fg(),
            bg: None,
            bold: false,
            italic: false,
            dim: false,
            underline: false,
            undercurl: false,
            strikeout: false,
        }
    }
}

/// 一行里要画底色的列区间：(起始列, 占几格, 颜色)。默认底色不产出，让底下的背景层 /
/// 背景图 / 桌面透出。
///
/// **必须扫到行尾，不能跟着字形一起截断在最后一个可见字符**——终端每行都补空格到满列宽，
/// 而「拿空格承载底色」是常规操作：fzf 的选中行、tmux/vim 的状态栏、TUI 菜单的选中项、
/// 多行拖选的中间行，尾巴上全是「带底色的空格」。截断的话高亮就缺一截（多行选区看着像
/// 锯齿），整行彩色空格的状态条更是一个像素都画不出来。所以这里只吃 `row`、不收 end 参数，
/// 没有截断可传错。Zed 同样在 `is_blank` 判定**之前**无条件收底色（terminal_element.rs:407）。
///
/// 底色走列区间而不是靠文字的 background_color：后者只覆盖字形的实际宽度，中文字形窄于两格
/// 时选区高亮会露出缝隙，且宽字符的 '\0' 占位格根本没有字符去承载底色。
fn bg_spans(
    row: &[terminal::Cell],
    style_of: &dyn Fn(usize) -> CellStyle,
) -> Vec<(usize, usize, u32)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < row.len() {
        let bg = style_of(i).bg;
        let start = i;
        while i < row.len() && style_of(i).bg == bg {
            i += 1;
        }
        // None = 默认底色：不画，让背景层 / 背景图 / 桌面透出。
        if let Some(bg) = bg {
            out.push((start, i - start, bg));
        }
    }
    out
}

/// 字形要画到第几列为止：最后一个「非 blank」的下一列。
///
/// blank = 空格（或宽字符的 '\0' 占位）且没有任何要画的装饰。**底色不在判据里**——底色由
/// [`bg_spans`] 独立扫全行负责，这里只关心「有没有字形 / 下划线 / 删除线要画」。带下划线的
/// 空格（含悬停链接）是要画线的，不算 blank。对应 Zed 的 `is_blank`（terminal_element.rs:1630，
/// 它的 `has_visible_style_modifier` = `ALL_UNDERLINES | INVERSE | STRIKEOUT`；INVERSE 在我们这
/// 边 snapshot 时就换成非默认底色了，由 bg_spans 兜住）。
fn visible_end(row: &[terminal::Cell], is_link: &dyn Fn(usize) -> bool) -> usize {
    let blank = |i: usize| {
        let c = &row[i];
        (c.ch == ' ' || c.ch == '\0')
            && !c.underline
            && !c.strikeout
            && c.link.is_none() // OSC 8 铺在空格上时也要画下划线
            && !is_link(i)
    };
    (0..row.len()).rposition(|i| !blank(i)).map_or(0, |i| i + 1)
}

/// 一批文本：钉在 `起始列 × cell_w` 上绘制。`wide` = 这批是**单个宽字符**（中文 / emoji）。
struct Batch {
    col: usize,
    text: String,
    style: CellStyle,
    wide: bool,
}

/// 把一行切成若干「批」。
///
/// 续接一批的条件：**样式相同，且这一格紧接着本批已占的格子**。
///
/// **宽字符（占两格的）独占一批**：它的字形（约 1.0em）窄于两格（1.2em），得按真实字形宽度
/// 在两格内**居中**才好看——左对齐的话字会贴着格子左边，光标块一压上去左右空隙就不对称
/// （见 [`paint_row`] 里的居中绘制）。而 force_width 只会把 glyph 钉到 `序号 × cell_w`、
/// 一律左对齐，所以宽字符不能跟别人混在一批里，否则没法单独定位。
///
/// 零宽字符（变体选择器 / 组合变音符，见 `Cell::zw`）紧跟基字符进同一批，但 `count`
/// **不加**——`count` 记的是**格数**不是字符数。加了的话后面每个字符的列号都会错一格。
/// gpui 的 `apply_force_width_to_layout` 认得它们（排在基字符同一个 x，不推进 glyph 计数器，
/// 贴着基字符走），所以位置不受影响。跟 Zed 的 `append_zero_width_chars` 是一回事。
fn text_batches(
    row: &[terminal::Cell],
    end: usize,
    style_of: &dyn Fn(usize) -> CellStyle,
) -> Vec<Batch> {
    let mut out: Vec<Batch> = Vec::new();
    // (起始列, 已占格数, 文本, 样式)
    let mut cur: Option<(usize, usize, String, CellStyle)> = None;
    let flush = |cur: &mut Option<(usize, usize, String, CellStyle)>, out: &mut Vec<Batch>| {
        if let Some((col, _, text, style)) = cur.take() {
            out.push(Batch { col, text, style, wide: false });
        }
    };
    for i in 0..end.min(row.len()) {
        let cell = &row[i];
        let ch = cell.ch;
        if ch == '\0' {
            continue; // 宽字符占位格：不产生字形，但列号照常前进
        }
        let style = style_of(i);
        let zw = cell.zw.as_deref().unwrap_or_default();
        let mut text = String::from(ch);
        text.extend(zw); // 零宽字符：进文本，不占格

        // 宽字符（后面跟着 '\0' 占位格）：独占一批，绘制时按真实字形宽度居中到两格里。
        if row.get(i + 1).is_some_and(|c| c.ch == '\0') {
            flush(&mut cur, &mut out);
            out.push(Batch { col: i, text, style, wide: true });
            continue;
        }

        match cur.as_mut() {
            Some((start, count, buf, st)) if *st == style && *start + *count == i => {
                buf.push_str(&text);
                *count += 1;
            }
            _ => {
                flush(&mut cur, &mut out);
                cur = Some((i, 1, text, style));
            }
        }
    }
    flush(&mut cur, &mut out);
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
            // 去掉结尾的标点（跳过宽字符占位格再判）
            let mut end = j;
            while end > i {
                let ch = row[end - 1].ch;
                if ch == '\0' {
                    end -= 1;
                    continue;
                }
                if matches!(ch, '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '"' | '\'') {
                    end -= 1;
                    continue;
                }
                break;
            }
            if end > i {
                let url = cells_to_token(row, i, end);
                // 最短合法 URL 大约 "http://a.b"（10 字符级）；滤掉误扫到的短前缀
                if url.len() >= 10 {
                    out.push((i, end - 1, url));
                }
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
            while end > i {
                let ch = row[end - 1].ch;
                if ch == '\0' {
                    end -= 1;
                    continue;
                }
                if matches!(ch, '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '"' | '\'') {
                    end -= 1;
                    continue;
                }
                break;
            }
            if end > i {
                // 宽字符第二格是 `'\0'` 占位——拼 token 时必须跳过，否则带中文的路径会夹
                // NUL，`Path::exists` 永远失败。扫描时仍把 `'\0'` 当 token 内字符（见
                // is_url_char），这样 `/Users/中文/x` 不会在「中」后面被截断。
                let token = cells_to_token(row, i, end);
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

/// 把 `[start, end)` 列上的可见字符拼成字符串：跳过宽字符占位 `'\0'`，并带上基字符
/// 上的零宽字符（变体选择器等）。列范围本身仍含占位格，悬停高亮才能盖满两格。
fn cells_to_token(row: &[terminal::Cell], start: usize, end: usize) -> String {
    let mut s = String::new();
    for k in start..end.min(row.len()) {
        let c = &row[k];
        if c.ch == '\0' {
            continue;
        }
        s.push(c.ch);
        if let Some(zw) = c.zw.as_deref() {
            s.extend(zw.iter().copied());
        }
    }
    s
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
    // `'\0'` = 宽字符占位格：扫描 token 时要当成「继续」而不是断点，否则
    // `/Users/中文/x` 会在「中」后被截断。真正拼字符串时再跳过（见 cells_to_token）。
    c == '\0' || (!c.is_whitespace() && !matches!(c, '<' | '>' | '"' | '`' | '|' | '{' | '}' | '^'))
}

/// 把一次「非文本按键」转成写给 PTY 的字节：特殊键和 Ctrl 组合。
/// 可打印字符与空格走 IME 的 replace_text_in_range，不在这里处理。
///
/// 键表大体对齐 Zed `mappings/keys.rs`（xterm PC-style function keys）：
/// - 裸方向键 / Home / End 尊重 DECCKM（app_cursor → SS3）
/// - 带修饰的方向键 / F 键 / Page / Home / End → CSI `1;{mod}` 或 `N;{mod}~`
/// - Enter：开了 kitty 消歧层时带修饰走 CSI u；否则遗留编码
///
/// `app_cursor`：DECCKM（见 Terminal::app_cursor_mode）。
/// `kitty_keys`：kitty keyboard DISAMBIGUATE 层（见 Terminal::kitty_keyboard_mode）。
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
        // 无 kitty 时 Shift+Enter 跟 Zed 一样发 LF（部分多行 prompt 靠这个）；裸 Enter 仍是 CR。
        // 注意：bash/zsh 默认把 LF 也当提交，行为与 CR 接近；有 kitty 时走上面 CSI u。
        if m.shift {
            return Some(b"\n".to_vec());
        }
        return Some(b"\r".to_vec());
    }

    // 有任意修饰键时，优先发 xterm 修饰序列（方向 / F / 导航键）。
    // 必须在「裸键」表之前：否则 Shift+Up 会掉进裸 `\x1b[A`，readline 词跳等全废。
    if m.shift || m.alt || m.control {
        if let Some(seq) = modified_special_key(ks.key.as_str(), m) {
            return Some(seq);
        }
    }

    // 裸特殊键 + 部分固定修饰（Shift+Tab / Ctrl+Backspace 等）
    let named: Option<Vec<u8>> = match (ks.key.as_str(), m.shift, m.alt, m.control) {
        ("backspace", _, true, _) => Some(b"\x1b\x7f".to_vec()),
        ("backspace", _, _, true) => Some(b"\x08".to_vec()),
        ("backspace", _, _, _) => Some(b"\x7f".to_vec()),
        ("tab", true, _, _) => Some(b"\x1b[Z".to_vec()),
        ("tab", _, _, _) => Some(b"\t".to_vec()),
        ("escape", _, _, _) => Some(b"\x1b".to_vec()),
        ("left", _, _, _) => Some(if app_cursor { b"\x1bOD" } else { b"\x1b[D" }.to_vec()),
        ("right", _, _, _) => Some(if app_cursor { b"\x1bOC" } else { b"\x1b[C" }.to_vec()),
        ("up", _, _, _) => Some(if app_cursor { b"\x1bOA" } else { b"\x1b[A" }.to_vec()),
        ("down", _, _, _) => Some(if app_cursor { b"\x1bOB" } else { b"\x1b[B" }.to_vec()),
        ("home", _, _, _) => Some(if app_cursor { b"\x1bOH" } else { b"\x1b[H" }.to_vec()),
        ("end", _, _, _) => Some(if app_cursor { b"\x1bOF" } else { b"\x1b[F" }.to_vec()),
        ("insert", _, _, _) => Some(b"\x1b[2~".to_vec()),
        ("delete", _, _, _) => Some(b"\x1b[3~".to_vec()),
        ("pageup", _, _, _) => Some(b"\x1b[5~".to_vec()),
        ("pagedown", _, _, _) => Some(b"\x1b[6~".to_vec()),
        ("f1", _, _, _) => Some(b"\x1bOP".to_vec()),
        ("f2", _, _, _) => Some(b"\x1bOQ".to_vec()),
        ("f3", _, _, _) => Some(b"\x1bOR".to_vec()),
        ("f4", _, _, _) => Some(b"\x1bOS".to_vec()),
        ("f5", _, _, _) => Some(b"\x1b[15~".to_vec()),
        ("f6", _, _, _) => Some(b"\x1b[17~".to_vec()),
        ("f7", _, _, _) => Some(b"\x1b[18~".to_vec()),
        ("f8", _, _, _) => Some(b"\x1b[19~".to_vec()),
        ("f9", _, _, _) => Some(b"\x1b[20~".to_vec()),
        ("f10", _, _, _) => Some(b"\x1b[21~".to_vec()),
        ("f11", _, _, _) => Some(b"\x1b[23~".to_vec()),
        ("f12", _, _, _) => Some(b"\x1b[24~".to_vec()),
        _ => None,
    };
    if let Some(bytes) = named {
        return Some(bytes);
    }

    // Ctrl+字母 / 若干标点 → C0 控制符
    if m.control && !m.alt {
        if let Some(b) = ctrl_byte(ks.key.as_str()) {
            return Some(vec![b]);
        }
    }

    None
}

/// xterm 修饰特殊键。mod 编码：1+ shift|alt<<1|ctrl<<2 → 2..=8。
/// 见 https://invisible-island.net/xterm/ctlseqs/ctlseqs.html#h2-PC-Style-Function-Keys
fn modified_special_key(key: &str, m: &Modifiers) -> Option<Vec<u8>> {
    let mod_code = csi_u_modifiers(m);
    if mod_code <= 1 {
        return None;
    }
    let seq = match key {
        "up" => format!("\x1b[1;{mod_code}A"),
        "down" => format!("\x1b[1;{mod_code}B"),
        "right" => format!("\x1b[1;{mod_code}C"),
        "left" => format!("\x1b[1;{mod_code}D"),
        "home" => format!("\x1b[1;{mod_code}H"),
        "end" => format!("\x1b[1;{mod_code}F"),
        "f1" => format!("\x1b[1;{mod_code}P"),
        "f2" => format!("\x1b[1;{mod_code}Q"),
        "f3" => format!("\x1b[1;{mod_code}R"),
        "f4" => format!("\x1b[1;{mod_code}S"),
        "f5" => format!("\x1b[15;{mod_code}~"),
        "f6" => format!("\x1b[17;{mod_code}~"),
        "f7" => format!("\x1b[18;{mod_code}~"),
        "f8" => format!("\x1b[19;{mod_code}~"),
        "f9" => format!("\x1b[20;{mod_code}~"),
        "f10" => format!("\x1b[21;{mod_code}~"),
        "f11" => format!("\x1b[23;{mod_code}~"),
        "f12" => format!("\x1b[24;{mod_code}~"),
        "insert" => format!("\x1b[2;{mod_code}~"),
        "delete" => format!("\x1b[3;{mod_code}~"),
        "pageup" => format!("\x1b[5;{mod_code}~"),
        "pagedown" => format!("\x1b[6;{mod_code}~"),
        _ => return None,
    };
    Some(seq.into_bytes())
}

fn ctrl_byte(key: &str) -> Option<u8> {
    if key.len() != 1 {
        return None;
    }
    let c = key.as_bytes()[0];
    match c {
        b'@' => Some(0x00),
        b'a'..=b'z' => Some(c - b'a' + 1),
        b'A'..=b'Z' => Some(c.to_ascii_lowercase() - b'a' + 1),
        b'[' => Some(0x1b),
        b'\\' => Some(0x1c),
        b']' => Some(0x1d),
        b'^' => Some(0x1e),
        b'_' => Some(0x1f),
        b'?' => Some(0x7f),
        b' ' => Some(0x00),
        _ => None,
    }
}

/// CSI u / xterm 修饰键参数：基数 1，再按位叠加 shift(1) / alt(2) / ctrl(4)。
/// 例：Shift+Enter → 2，于是 `ESC[13;2u`；Ctrl+Left → 5，于是 `ESC[1;5D`。
fn csi_u_modifiers(m: &Modifiers) -> u8 {
    1 + u8::from(m.shift) + (u8::from(m.alt) << 1) + (u8::from(m.control) << 2)
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：那会把 gpui 的 `test` 属性宏一起带进来，盖掉标准 #[test]。
    use super::{
        bg_spans, cells_to_token, keystroke_to_bytes, link_at, scrollbar_thumb, text_batches,
        visible_end, CellStyle, SCROLLBAR_THUMB_MIN,
    };
    use crate::terminal::Cell;
    use gpui::{Keystroke, Modifiers};

    /// 默认样式：bg = None 表示「终端默认底色」（不画底色矩形）。
    const PLAIN: CellStyle = CellStyle {
        fg: 0xffffff,
        bg: None,
        bold: false,
        italic: false,
        dim: false,
        underline: false,
        undercurl: false,
        strikeout: false,
    };

    /// 造一行 cell：宽字符（中文）自动补一个 '\0' 占位格，跟 alacritty 的网格一致。
    ///
    /// 宽度判定必须跟真实终端一致，否则测不出「歧义宽度」这类 bug：CJK / 全角
    /// （U+2E80 起）算两格，而 `·`(U+00B7) `—`(U+2014) `…`(U+2026) 这些**东亚歧义
    /// 宽度**字符终端只给一格——正是它们的字形（fallback 到中文字体后是全角）跟格子
    /// 对不上，才会把同批的后续字符顶歪。
    fn row(s: &str) -> Vec<Cell> {
        let cell = |ch: char| Cell {
            ch,
            fg: 0xffffff,
            bg: 0x000000,
            bg_default: true, // 默认底色（不画底色矩形）
            bold: false,
            italic: false,
            dim: false,
            underline: false,
            undercurl: false,
            strikeout: false,
            zw: None,
            link: None,
            selected: false,
        };
        let mut out = Vec::new();
        for ch in s.chars() {
            let wide = (ch as u32) >= 0x2e80;
            out.push(cell(ch));
            if wide {
                out.push(cell('\0'));
            }
        }
        out
    }

    fn batches(cells: &[Cell]) -> Vec<(usize, String)> {
        text_batches(cells, cells.len(), &|_| PLAIN)
            .into_iter()
            .map(|b| (b.col, b.text))
            .collect()
    }

    /// 造一个只改了底色的样式（给 bg_spans 的测试用）。
    fn with_bg(bg: u32) -> CellStyle {
        CellStyle { bg: Some(bg), ..PLAIN }
    }

    /// 「拿空格承载底色」是终端里的常规操作：fzf 的选中行、tmux/vim 的状态栏、TUI 菜单的
    /// 选中项、多行拖选的中间行——文字后面跟着一长串**带底色的空格**（终端每行都补空格到
    /// 满列宽）。底色必须一路画到行尾；跟着字形一起截断在最后一个可见字符的话，高亮就缺
    /// 一截，多行选区看着像锯齿，而整行彩色空格的状态条会一个像素都画不出来。
    #[test]
    fn background_runs_to_end_of_row_not_to_last_glyph() {
        // 一行 8 格：ab + 6 个空格，整行同一个非默认底色（状态栏那种）
        let cells = row("ab      ");
        let sel = 0x0033_4a6a;
        assert_eq!(
            bg_spans(&cells, &|_| with_bg(sel)),
            vec![(0, 8, sel)],
            "底色要铺满 8 格，而不是停在最后一个可见字符（第 2 格）"
        );

        // 整行全是带底色的空格（纯色状态条）：一个可见字符都没有，照样得画满。
        let blanks = row("        ");
        assert_eq!(
            bg_spans(&blanks, &|_| with_bg(sel)),
            vec![(0, 8, sel)],
            "没有任何可见字符时，整行底色不能消失"
        );
    }

    /// 「默认底色」必须按**颜色枚举**判（`Cell::bg_default`），不能拿 RGB 去比。应用完全可以
    /// 显式设一个恰好等于默认底色的 RGB（`\e[48;2;…m`）——那是真要画的一块底色，而默认底色的
    /// 格子是**留空让背景图 / 透明度透出来**的。判错就会在本该是纯色块的地方漏出背景图。
    #[test]
    fn explicit_bg_equal_to_default_rgb_is_still_painted() {
        let bg = 0x1a1b26; // 假设它恰好就是当前主题的默认底色 RGB
        let mut cells = row("ab");
        for c in &mut cells {
            c.bg = bg;
            c.bg_default = false; // 应用显式设的，不是默认底色
        }
        let style_of = |i: usize| CellStyle {
            bg: (!cells[i].bg_default).then_some(cells[i].bg),
            ..PLAIN
        };
        assert_eq!(
            bg_spans(&cells, &style_of),
            vec![(0, 2, bg)],
            "应用显式设的底色要画出来，哪怕它的 RGB 跟默认底色一模一样"
        );

        // 反过来：默认底色的格子不画（让背景层透出）。
        let blanks = row("ab"); // row() 造出来的就是 bg_default = true
        assert_eq!(
            bg_spans(&blanks, &|i| CellStyle {
                bg: (!blanks[i].bg_default).then_some(blanks[i].bg),
                ..PLAIN
            }),
            vec![],
            "默认底色不画底色矩形"
        );
    }

    /// 反过来，字形不必画到行尾：尾部那些「什么都不画的空格」不进批次。但带下划线的空格
    /// （下划线本身要画）不算 blank。
    #[test]
    fn glyphs_stop_at_last_non_blank_cell() {
        let cells = row("ab      ");
        assert_eq!(visible_end(&cells, &|_| false), 2, "尾部纯空格不出字形");

        // 第 5 格是带下划线的空格（比如 OSC 8 链接铺在空格上）：要画线，不能截在它前面。
        let mut underlined = row("ab      ");
        underlined[5].underline = true;
        assert_eq!(visible_end(&underlined, &|_| false), 6, "带下划线的空格不算 blank");

        // 悬停链接高亮压在尾部空格上时同理。
        assert_eq!(visible_end(&cells, &|i| i == 4), 5, "链接高亮的空格不算 blank");
    }

    /// OSC 8 超链接：可见文本只是标题（`Release notes`），真正的 URL 藏在协议里。正则扫
    /// 可见文本是找不到的，必须读 `Cell::link`，且范围要覆盖铺着同一个 URI 的所有格子。
    #[test]
    fn osc8_link_wins_over_regex_and_spans_its_cells() {
        use std::sync::Arc;
        let uri: Arc<str> = Arc::from("https://example.com/notes");
        let mut cells = row("ab cd");
        // 「cd」两格挂着 OSC 8 链接
        cells[3].link = Some(uri.clone());
        cells[4].link = Some(uri.clone());

        assert_eq!(
            link_at(&cells, 3),
            Some((3, 4, "https://example.com/notes".to_string())),
            "命中 OSC 8：范围覆盖挂着同一 URI 的连续格子"
        );
        assert_eq!(link_at(&cells, 4), link_at(&cells, 3), "同一链接内任意一格结果相同");
        assert_eq!(link_at(&cells, 0), None, "没挂链接、可见文本也不是 URL 的格子：没有链接");
    }

    /// 零宽字符（变体选择器 U+FE0F、组合变音符等）挂在基字符那一格上（alacritty 的
    /// `cell.zerowidth()`）。它们**必须**跟着基字符一起进批交给排版器——丢了的话 `⚠️` 掉成
    /// 黑白的 `⚠`、`é` 掉成 `e`，而复制出去的文本却是带着它们的（alacritty 复制时会带），
    /// 于是「看到的 ≠ 复制到的」。
    ///
    /// 但它们**不能占格子**：`count` 记的是格数，多算一格的话，这一批里它后面每个字符的
    /// 列号都会偏，且下一批的续接判定（`start + count == i`）也会错位。
    #[test]
    fn zero_width_chars_ride_along_without_taking_a_cell() {
        // 网格：⚠(0) x(1) —— U+26A0 是窄字符，占一格；U+FE0F 挂在它身上、占零格。
        let mut cells = row("⚠x");
        cells[0].zw = Some(vec!['\u{fe0f}'].into_boxed_slice());

        assert_eq!(
            batches(&cells),
            vec![(0, "⚠\u{fe0f}x".to_string())],
            "变体选择器要跟着基字符进同一批，且不占格——x 仍然续在这一批里（列号连续）"
        );
    }

    /// 东亚歧义宽度字符（`·` `—` `…` 中文引号等）终端只给一格、字体却画成全角，**不需要**
    /// 在分批这层特殊照顾：批内每个 glyph 的 x 由 `shape_line` 的 force_width 钉死在
    /// `序号 × cell_w`（见 paint_row 头注），字形宽窄推不动任何人。所以它们跟后面的字符
    /// 连成一批是正确的，这里只钉住「确实连成了一批」，免得日后有人又去分批层打补丁。
    #[test]
    fn ambiguous_width_chars_batch_normally() {
        // 网格：—(0) —(1) 正(2,3) —— 破折号只占一格、不带 '\0' 占位，所以它俩连成一批；
        // 「正」是宽字符，按规矩独占一批（见 wide_chars_never_share_a_batch）。
        assert_eq!(batches(&row("——正")), vec![(0, "——".into()), (2, "正".into())]);
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

    /// **宽字符永远独占一批**，两条理由都是硬的：
    ///
    /// 1. **要居中**。中文字形约 1.0em，两格是 1.2em，左对齐会让字贴着格子左边——光标块
    ///    （满两格）压上去时左右空隙不对称，整行中文也都偏左。paint_row 得单独把它按真实
    ///    字形宽度居中到两格里，混在批里就没法单独定位。
    /// 2. **force_width 按 glyph 序号钉位**（`glyph_pos × cell_w`），而宽字符占两格却只算
    ///    一个 glyph。它后面若还有同批字符，那些字符会整体少一格。独占一批就彻底没这问题。
    #[test]
    fn wide_chars_never_share_a_batch() {
        // 网格：a(0) 中(1,2) x(3)
        assert_eq!(
            batches(&row("a中x")),
            vec![(0, "a".into()), (1, "中".into()), (3, "x".into())],
            "「中」独占一批钉在第 1 列，x 重新钉在第 3 列"
        );

        // 多个窄字符在前也一样。
        assert_eq!(
            batches(&row("ab中cd")),
            vec![(0, "ab".into()), (2, "中".into()), (4, "cd".into())],
        );
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
            if i < 2 { PLAIN } else { CellStyle { fg: 0xff0000, ..PLAIN } }
        })
        .into_iter()
        .map(|b| (b.col, b.text))
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

    /// 没开协议的程序不认 CSI u：Shift+Enter 不能吐 `[13;2u` 乱码。跟 Zed 一样发 LF
    /// （`\n`）；bash/zsh 默认把 LF 也当提交，行为与裸 Enter 接近。
    #[test]
    fn shift_enter_falls_back_to_lf_without_kitty() {
        let bytes = keystroke_to_bytes(&enter(true, false, false), false, false).unwrap();
        assert_eq!(bytes, b"\n");
    }

    /// 协议没开时 Alt+Enter 还有条传统通道：meta 前缀 `ESC` + `CR`，Claude Code 认这个。
    #[test]
    fn alt_enter_falls_back_to_meta_prefix_without_kitty() {
        let bytes = keystroke_to_bytes(&enter(false, true, false), false, false).unwrap();
        assert_eq!(bytes, b"\x1b\r");
    }

    fn ks(key: &str, shift: bool, alt: bool, control: bool) -> Keystroke {
        Keystroke {
            modifiers: Modifiers {
                shift,
                alt,
                control,
                ..Default::default()
            },
            key: key.into(),
            key_char: None,
        }
    }

    /// readline / zsh 词跳靠 Ctrl+Left/Right 的 xterm 修饰序列；发裸方向键等于没按。
    #[test]
    fn ctrl_arrow_sends_xterm_modifier_sequence() {
        let left = keystroke_to_bytes(&ks("left", false, false, true), false, false).unwrap();
        assert_eq!(left, b"\x1b[1;5D");
        let up = keystroke_to_bytes(&ks("up", true, false, false), false, false).unwrap();
        assert_eq!(up, b"\x1b[1;2A");
    }

    #[test]
    fn function_keys_encode_like_xterm() {
        assert_eq!(
            keystroke_to_bytes(&ks("f5", false, false, false), false, false).unwrap(),
            b"\x1b[15~"
        );
        assert_eq!(
            keystroke_to_bytes(&ks("f1", false, false, false), false, false).unwrap(),
            b"\x1bOP"
        );
        // 带修饰
        assert_eq!(
            keystroke_to_bytes(&ks("f5", true, false, false), false, false).unwrap(),
            b"\x1b[15;2~"
        );
    }

    /// 宽字符第二格是 `'\0'` 占位：拼路径 token 时必须跳过，否则 `Path::exists` 因 NUL 失败。
    #[test]
    fn cells_to_token_skips_wide_char_spacers() {
        // 网格：/(0) 中(1,2=\0) 文(3,4=\0)
        let cells = row("/中文");
        assert!(
            cells.iter().any(|c| c.ch == '\0'),
            "测试前提：中文应带占位格"
        );
        let token = cells_to_token(&cells, 0, cells.len());
        assert_eq!(token, "/中文");
        assert!(!token.as_bytes().contains(&0), "token 里不能夹 NUL");
    }

    /// 首帧 grid_size 还是 (0,0)，track_h 兜底成 1.0——比 THUMB_MIN 还矮。
    /// `clamp(28.0, 1.0)` 会 panic（min > max），GPUI 启动回调不能 unwind → 整个 app abort。
    #[test]
    fn scrollbar_thumb_survives_first_frame_tiny_track() {
        let (thumb_h, thumb_y) = scrollbar_thumb(1.0, 40, 100, 0);
        assert!(thumb_h <= 1.0, "thumb 不能超出轨道: {thumb_h}");
        assert!(thumb_y >= 0.0);
    }

    /// 正常尺寸下 thumb 高度按可视行占比走，且不小于最短高度。
    #[test]
    fn scrollbar_thumb_normal_geometry() {
        let track_h = 600.0;
        // 一半可视一半回滚：thumb 占轨道一半
        let (thumb_h, thumb_y) = scrollbar_thumb(track_h, 50, 50, 0);
        assert_eq!(thumb_h, 300.0);
        assert_eq!(thumb_y, 300.0, "offset=0 应贴底");
        // 回滚极深：受最短高度托底
        let (thumb_h, thumb_y) = scrollbar_thumb(track_h, 40, 100_000, 100_000);
        assert_eq!(thumb_h, SCROLLBAR_THUMB_MIN);
        assert_eq!(thumb_y, 0.0, "offset=max 应贴顶");
        // 无回滚：占满轨道、贴顶
        let (thumb_h, thumb_y) = scrollbar_thumb(track_h, 40, 0, 0);
        assert_eq!(thumb_h, track_h);
        assert_eq!(thumb_y, 0.0);
    }
}

#[cfg(test)]
mod permission_prompt_tests {
    use super::{parse_permission_prompt, PermissionOptionKind};

    fn lines(s: &str) -> Vec<String> {
        s.lines().map(|l| l.to_string()).collect()
    }

    #[test]
    fn parses_claude_style_numbered_menu() {
        let p = parse_permission_prompt(&lines(
            "Do you want to proceed?\n\
             ❯ 1. Yes\n\
               2. Yes, and don't ask again for bash commands\n\
               3. No, and tell Claude what to do differently\n\
             Esc to cancel",
        ))
        .expect("prompt");
        assert_eq!(p.summary.as_deref(), Some("Do you want to proceed?"));
        assert_eq!(p.options.len(), 3);
        assert_eq!(p.options[0].key, "1");
        assert_eq!(p.options[0].kind, PermissionOptionKind::Allow);
        assert!(p.options[0].is_primary());
        assert_eq!(p.options[1].kind, PermissionOptionKind::Allow);
        assert!(!p.options[1].is_primary());
        assert_eq!(p.options[2].key, "3");
        assert_eq!(p.options[2].kind, PermissionOptionKind::Deny);
        assert_eq!(p.options[0].button_label(), "允许");
        assert_eq!(p.options[2].button_label(), "拒绝");
    }

    #[test]
    fn rejects_plain_numbered_list_without_permission_context() {
        assert!(
            parse_permission_prompt(&lines("1. install deps\n2. run tests\n3. deploy")).is_none()
        );
    }

    #[test]
    fn accepts_bracket_style_with_permission_hint() {
        let p = parse_permission_prompt(&lines(
            "Permission required\n[1] Allow\n[2] Deny",
        ))
        .expect("prompt");
        assert_eq!(p.options[0].kind, PermissionOptionKind::Allow);
        assert_eq!(p.options[1].kind, PermissionOptionKind::Deny);
    }

    // 以下几组是手机端（remote-web/src/lib/parseChoiceMenu.ts）已经认、而这边一直
    // 认不出的真实 TUI 形态。两份解析器同日诞生后各自演化，手机那份陆续吃过线上
    // 补丁（它注释里记着「高亮项常用 ❯ / › / ▶ 等，旧正则只认 `>`，会把第 1 项漏掉」），
    // 这边从没拿到。
    //
    // 认不出的代价不是「少个按钮」而是**误操作**：扫不到菜单 → has_opts=false →
    // 界面落回硬编码兜底（main.rs 的「批准=打 1 / 拒绝=打 3」）→ 盲发 1/3，
    // 而真实菜单未必是这个顺序。

    /// TUI 常用 `│` 画边框，选项行前缀是边框而非空白。
    #[test]
    fn accepts_options_behind_box_drawing_border() {
        let p = parse_permission_prompt(&lines(
            "Do you want to proceed?\n\
             │ ❯ 1. Yes\n\
             │   2. No, tell Claude what to do differently",
        ))
        .expect("边框前缀的菜单也该认出来");
        assert_eq!(p.options.len(), 2);
        assert_eq!(p.options[0].key, "1");
        assert_eq!(p.options[0].kind, PermissionOptionKind::Allow);
        assert_eq!(p.options[1].kind, PermissionOptionKind::Deny);
    }

    /// 高亮指针不止 `❯`：`›`/`▶`/`→` 等都在真实 TUI 里出现过。
    #[test]
    fn accepts_alternate_highlight_pointers() {
        for ptr in ["›", "▶", "►", "→", "➜", "✦", "➢", "➤"] {
            let src = format!(
                "Do you want to proceed?\n{ptr} 1. Yes\n  2. No, cancel"
            );
            let p = parse_permission_prompt(&lines(&src))
                .unwrap_or_else(|| panic!("指针 {ptr} 开头的菜单该认出来"));
            assert_eq!(p.options.len(), 2, "指针 {ptr}");
            assert_eq!(p.options[0].key, "1", "指针 {ptr}");
        }
    }

    /// 中文 TUI 常用顿号或全角句点做分隔符。
    #[test]
    fn accepts_cjk_separators() {
        for sep in ["、", "．"] {
            let src = format!("是否继续执行？\n❯ 1{sep}允许\n  2{sep}拒绝");
            let p = parse_permission_prompt(&lines(&src))
                .unwrap_or_else(|| panic!("分隔符 {sep} 的菜单该认出来"));
            assert_eq!(p.options.len(), 2, "分隔符 {sep}");
            assert_eq!(p.options[0].kind, PermissionOptionKind::Allow, "分隔符 {sep}");
            assert_eq!(p.options[1].kind, PermissionOptionKind::Deny, "分隔符 {sep}");
        }
    }

    /// 超过 4 项不得静默截断——被截掉的选项在界面上永远够不着。
    #[test]
    fn keeps_all_options_beyond_four() {
        let p = parse_permission_prompt(&lines(
            "Do you want to proceed?\n\
             ❯ 1. Yes\n\
               2. Yes, and don't ask again\n\
               3. Yes, but only this once\n\
               4. Edit the command first\n\
               5. No, tell Claude what to do differently",
        ))
        .expect("prompt");
        assert_eq!(p.options.len(), 5, "5 项菜单不该被截断成 4 项");
        assert_eq!(p.options[4].key, "5");
        assert_eq!(p.options[4].kind, PermissionOptionKind::Deny);
    }

    /// 补齐字符集不能把语义闸门一起放开：纯编号列表仍须拒绝。
    /// （手机那份没有闸门，会把这种误判成权限菜单——收敛时别把这个 bug 一起吸收。）
    #[test]
    fn still_rejects_plain_lists_with_new_separators_and_pointers() {
        assert!(
            parse_permission_prompt(&lines("› 1、install deps\n  2、run tests\n  3、deploy"))
                .is_none(),
            "换了指针和顿号，纯待办列表仍然不是权限菜单"
        );
    }
}
