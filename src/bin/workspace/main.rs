//! smelt 工作台 —— 基于 gpui-component 的桌面窗口。
//!
//! Workspace 管理多个终端标签（TerminalView）：顶部标签栏切换 / 新建 / 关闭，
//! 下方渲染当前活动终端。每个终端各自独立（PTY、IME、滚动、resize）。
//!
//! 运行： cargo run --bin workspace

mod agent;
mod dock;
mod hotspot;
mod pet;
mod session_history;
mod status_item;
mod terminal;
mod terminal_view;
mod updater;
mod usage_stats;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use chrono::Datelike;
use gpui::*;
use gpui::prelude::FluentBuilder;
use gpui::InteractiveElement;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::chart::BarChart;
use gpui_component::plot::shape::BarAlignment;
use gpui_component::sidebar::{
    Sidebar, SidebarCollapsible, SidebarGroup, SidebarMenu, SidebarMenuItem,
};
use gpui_component::color_picker::{ColorPicker, ColorPickerEvent, ColorPickerState};
use gpui_component::input::Input;
use gpui_component::list::{List, ListDelegate, ListEvent, ListItem, ListState};
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use gpui_component::badge::Badge;
use gpui_component::radio::{Radio, RadioGroup};
use gpui_component::setting::{Settings, SettingField, SettingGroup, SettingItem, SettingPage};
use gpui_component::slider::{Slider, SliderEvent, SliderState, SliderValue};
use gpui_component::text::TextView;
use gpui_component::resizable::{
    h_resizable, resizable_panel, v_resizable, ResizablePanelEvent, ResizableState,
};
use gpui_component::scroll::ScrollableElement;
use gpui_component::tab::{Tab, TabBar};
use gpui_component::tag::Tag;
use gpui_component::*;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use terminal_view::TerminalView;

// Cmd+Q 退出的应用级 action（gpui 无默认菜单栏，需自建菜单栏 + 键位绑定）。
gpui::actions!(smelt, [Quit, OpenSettings]);

/// 命令面板里的一个可执行动作。
#[derive(Clone)]
enum Cmd {
    NewTab,
    OpenProject,
    CloseTab,
    NextTab,
    PrevTab,
    SwitchTab(usize),
}

/// 命令面板的单个列表项：标签 + 选中态。
#[derive(IntoElement)]
struct CmdItem {
    base: ListItem,
    label: SharedString,
    selected: bool,
}

impl CmdItem {
    fn new(id: impl Into<ElementId>, label: SharedString, selected: bool) -> Self {
        Self {
            base: ListItem::new(id).selected(selected),
            label,
            selected,
        }
    }
}

impl Selectable for CmdItem {
    fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    fn is_selected(&self) -> bool {
        self.selected
    }
}

impl RenderOnce for CmdItem {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let fg = if self.selected {
            cx.theme().accent_foreground
        } else {
            cx.theme().foreground
        };
        self.base.px_3().py_1().child(div().text_color(fg).child(self.label))
    }
}

/// 命令面板列表的数据源：全部命令 + 当前查询过滤结果。
/// 搜索输入、上下选择、回车确认、Esc 取消都由 `ListState` 负责。
struct CmdDelegate {
    all: Vec<(SharedString, Cmd)>,
    matched: Vec<(SharedString, Cmd)>,
    selected_index: Option<IndexPath>,
}

impl CmdDelegate {
    fn new(all: Vec<(SharedString, Cmd)>) -> Self {
        Self {
            matched: all.clone(),
            all,
            selected_index: Some(IndexPath::default()),
        }
    }
}

impl ListDelegate for CmdDelegate {
    type Item = CmdItem;

    fn items_count(&self, _section: usize, _: &App) -> usize {
        self.matched.len()
    }

    fn perform_search(
        &mut self,
        query: &str,
        _: &mut Window,
        _: &mut Context<ListState<Self>>,
    ) -> Task<()> {
        let q = query.to_lowercase();
        self.matched = self
            .all
            .iter()
            .filter(|(label, _)| q.is_empty() || label.to_lowercase().contains(&q))
            .cloned()
            .collect();
        Task::ready(())
    }

    fn set_selected_index(
        &mut self,
        ix: Option<IndexPath>,
        _: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) {
        self.selected_index = ix;
        cx.notify();
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _: &mut Window,
        _: &mut Context<ListState<Self>>,
    ) -> Option<Self::Item> {
        let selected = Some(ix) == self.selected_index;
        self.matched
            .get(ix.row)
            .map(|(label, _)| CmdItem::new(ix, label.clone(), selected))
    }
}

/// 主区视图：总览 / 终端 / 文件树 / Git / 热力图。
#[derive(Clone, Copy, PartialEq)]
enum MainView {
    Overview,
    Terminal,
    Files,
    Git,
    Hotspot,
    History,
}

/// 会话里 agent 的状态（用于总览页状态徽章）。借鉴 codex 的 ThreadStatus 细分：
/// 「需要处理」不再一锅烩，等审批和一般等待是不同等级的行动召唤。
/// 排列顺序即优先级（值越小越靠前 / 越紧急）。
#[derive(Clone, Copy, PartialEq)]
enum AgentStatus {
    /// Claude 等你批准操作（通知文本含 permission/权限等）→ 最高优先，红色。
    WaitingApproval,
    /// 其他需要处理：等输入 / 响铃 / 自定义通知 → 橙色。
    NeedsAttention,
    /// 标题以 Braille spinner 开头 → 运行中，蓝色。
    Running,
    /// 任务刚完成、你还没回应过 → 「有结果可看」，绿色。
    Done,
    /// 其余 → 空闲，灰色。
    Idle,
}

/// 主区终端分屏布局树：叶子是一个终端，内部 Split 把区域按某轴切成多块。
/// 每个 Split 各持一个 ResizableState 记住拖动比例；递归即可任意嵌套分屏。
enum Pane {
    Leaf(Entity<TerminalView>),
    Split {
        axis: Axis,
        state: Entity<ResizableState>,
        children: Vec<Pane>,
    },
}

/// 拖拽会话排序时跟随鼠标的小预览 chip（侧栏「项目内会话拖拽」用）。
#[derive(Clone)]
struct SessionDrag {
    id: EntityId,
    title: SharedString,
}

impl Render for SessionDrag {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        div()
            .id("session-drag-preview")
            .cursor_grab()
            .py_1()
            .px_3()
            .rounded_md()
            .border_1()
            .border_color(t.border)
            .bg(t.popover)
            .text_xs()
            .text_color(t.foreground)
            .child(self.title.clone())
            // 拖起瞬间淡入，别让 chip "啪"地闪现
            .with_animation(
                "session-drag-in",
                Animation::new(std::time::Duration::from_millis(120)).with_easing(ease_out_quint()),
                |this, delta| this.opacity(delta),
            )
    }
}

/// 拖拽项目分组排序时跟随鼠标的小预览 chip。
#[derive(Clone)]
struct ProjectDrag {
    name: SharedString,
}

impl Render for ProjectDrag {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        div()
            .id("project-drag-preview")
            .cursor_grab()
            .py_1()
            .px_3()
            .rounded_md()
            .border_1()
            .border_color(t.border)
            .bg(t.popover)
            .text_xs()
            .text_color(t.foreground)
            .child(self.name.clone())
            // 拖起瞬间淡入，同 SessionDrag
            .with_animation(
                "project-drag-in",
                Animation::new(std::time::Duration::from_millis(120)).with_easing(ease_out_quint()),
                |this, delta| this.opacity(delta),
            )
    }
}

/// 一个会话 = 一棵独立分屏树 + 会话内当前活动 pane（终端）。
/// 侧栏每条对应一个会话；主区显示当前会话的分屏树。
struct Session {
    layout: Pane,
    active: Entity<TerminalView>,
}

impl Session {
    /// 单终端会话。
    fn single(view: Entity<TerminalView>) -> Self {
        Self { layout: Pane::Leaf(view.clone()), active: view }
    }

    /// 会话标题：仅当终端标题是 Claude Code 风格（✳ 或 Braille spinner 开头）时用它的
    /// 任务名，否则回退 cwd 末段——避免把普通 shell 的 user@host:path 标题当任务名。
    fn title(&self, cx: &App) -> String {
        let t = self.active.read(cx);
        if let Some(raw) = t.agent_title() {
            let head = raw.trim_start();
            let is_agent = head.starts_with('✳')
                || head
                    .chars()
                    .next()
                    .is_some_and(|c| ('\u{2801}'..='\u{28FF}').contains(&c));
            if is_agent {
                let task = strip_status(&raw);
                if !task.is_empty() && task != "Claude Code" && task != "claude" {
                    return task;
                }
            }
        }
        t.title().to_string()
    }

    /// 会话工作目录：活动终端的 cwd（侧栏分组用）。
    fn cwd(&self, cx: &App) -> Option<String> {
        self.active.read(cx).cwd()
    }

    /// 会话内 pane 数（判断 Cmd+W 是关 pane 还是关整会话）。
    fn pane_count(&self) -> usize {
        let mut v = Vec::new();
        collect_leaves(&self.layout, &mut v);
        v.len()
    }

    /// 会话内任一 pane 的待处理通知消息（供总览卡片显示「等你确认 xxx」）。
    fn notification_msg(&self, cx: &App) -> Option<String> {
        let mut v = Vec::new();
        collect_leaves(&self.layout, &mut v);
        v.iter().find_map(|t| t.read(cx).notification().map(|s| s.to_string()))
    }

    /// 活动 pane 末尾 n 行文本（总览卡片迷你预览）。
    fn preview(&self, cx: &App, n: usize) -> Vec<String> {
        self.active.read(cx).last_lines(n)
    }

    /// 会话内最近一次通知时刻（总览「N 分钟前」）。
    fn notified_at(&self, cx: &App) -> Option<Instant> {
        let mut v = Vec::new();
        collect_leaves(&self.layout, &mut v);
        v.iter().filter_map(|t| t.read(cx).notified_at()).max()
    }

    /// 会话状态：等审批 > 需要处理 > 运行中 > 刚完成未读 > 空闲（遍历全部 pane 取最高）。
    fn status(&self, cx: &App) -> AgentStatus {
        let mut v = Vec::new();
        collect_leaves(&self.layout, &mut v);
        // 等审批（红）压过一般注意（橙）。
        let mut attention = None;
        for t in &v {
            match t.read(cx).attention_kind() {
                Some(terminal_view::AttentionKind::Approval) => {
                    return AgentStatus::WaitingApproval
                }
                Some(terminal_view::AttentionKind::Attention) => {
                    attention = Some(AgentStatus::NeedsAttention)
                }
                None => {}
            }
        }
        if let Some(s) = attention {
            return s;
        }
        // 活动终端标题以 Braille spinner（非空盲文块）开头 → 运行中。
        if let Some(raw) = self.active.read(cx).agent_title() {
            if let Some(c) = raw.trim_start().chars().next() {
                if ('\u{2801}'..='\u{28FF}').contains(&c) {
                    return AgentStatus::Running;
                }
            }
        }
        // 有 pane 刚跑完还没被回应 → 提示「有结果可看」。
        if v.iter().any(|t| t.read(cx).completed_unread()) {
            return AgentStatus::Done;
        }
        AgentStatus::Idle
    }
}

/// 相对时间：「刚刚 / N 秒前 / N 分钟前 / N 小时前」。
fn ago(t: Instant) -> String {
    let s = t.elapsed().as_secs();
    if s < 10 {
        "刚刚".to_string()
    } else if s < 60 {
        format!("{s} 秒前")
    } else if s < 3600 {
        format!("{} 分钟前", s / 60)
    } else {
        format!("{} 小时前", s / 3600)
    }
}

/// 去掉 agent 标题开头的状态符号（✳ / Braille spinner ⠂⠐ 等）+ 空白，保留任务名。
fn strip_status(title: &str) -> String {
    title
        .trim_start_matches(|c: char| {
            c.is_whitespace()
                || c == '✳'
                || c == '·'
                || c == '*'
                || ('\u{2800}'..='\u{28FF}').contains(&c) // Braille 盲文块（spinner 动画帧）
        })
        .trim()
        .to_string()
}

/// 收集布局树里所有叶子终端（clone 句柄，顺序 = 深度优先遍历序）。
fn collect_leaves(pane: &Pane, out: &mut Vec<Entity<TerminalView>>) {
    match pane {
        Pane::Leaf(t) => out.push(t.clone()),
        Pane::Split { children, .. } => {
            for c in children {
                collect_leaves(c, out);
            }
        }
    }
}

/// 在布局树里找到 target 终端所在叶子，就地替换成「原叶子 + 新叶子」的二分 Split。
/// 找到并替换返回 true；未命中返回 false。
fn split_leaf(
    pane: &mut Pane,
    target: EntityId,
    axis: Axis,
    state: Entity<ResizableState>,
    new_leaf: Entity<TerminalView>,
) -> bool {
    match pane {
        Pane::Leaf(t) if t.entity_id() == target => {
            let old = Pane::Leaf(t.clone());
            *pane = Pane::Split {
                axis,
                state,
                children: vec![old, Pane::Leaf(new_leaf)],
            };
            true
        }
        Pane::Leaf(_) => false,
        Pane::Split { children, .. } => children
            .iter_mut()
            .any(|c| split_leaf(c, target, axis, state.clone(), new_leaf.clone())),
    }
}

/// 从布局树移除 target 终端的叶子；某 Split 移除后只剩一个子节点则塌缩掉这层。
fn remove_leaf(pane: &mut Pane, target: EntityId) {
    if let Pane::Split { children, .. } = pane {
        if let Some(pos) = children
            .iter()
            .position(|c| matches!(c, Pane::Leaf(t) if t.entity_id() == target))
        {
            children.remove(pos);
        } else {
            for c in children.iter_mut() {
                remove_leaf(c, target);
            }
        }
        if children.len() == 1 {
            *pane = children.remove(0);
        }
    }
}

/// 打开查看的文件：路径 + 可编辑的代码编辑器状态（gpui-component 的 Editor：
/// tree-sitter 语法高亮 + 行号 + 搜索，直接可编辑，不再是只读预览）。
struct OpenFile {
    path: String,
    editor: Entity<gpui_component::input::InputState>,
    /// 磁盘上（或最近一次保存后）的内容快照，跟编辑器当前内容一比就知道是否有未保存
    /// 改动——不用额外订阅 InputEvent::Change 维护一个脏标记，render 时比一下字符串就行。
    saved_content: Rc<String>,
    /// 最近一次保存失败 / 不允许保存的原因；成功保存或重新打开文件后清空。
    save_error: Option<String>,
    /// 文件是否按文本成功读取过。读取完成前 / 读取失败（比如二进制文件）时为 false，
    /// 禁止保存——避免误按 Cmd+S 把「无法读取」占位文案写回去覆盖了原文件。
    readable: bool,
    /// 上次保存时检测到磁盘内容跟 saved_content 对不上（外部改过）。为 true 时
    /// 再按一次 Cmd+S 会跳过冲突检查强制覆盖——用"再按一次"当作用户已确认覆盖。
    conflict_pending: bool,
}

/// 保存一次的结果：分 Saved / 检测到外部改动的 Conflict / 其它 IO 错误。
enum SaveOutcome {
    Saved,
    Conflict,
    Error(String),
}

/// 文件树搜索的一条命中。
struct SearchHit {
    /// 命中文件的绝对路径（点击时用它 view_file）。
    path: String,
    /// 相对项目根的展示路径。
    rel: String,
    /// 内容命中时的首个匹配行：(行号从 1 起, 该行文本预览)；仅文件名命中时为 None。
    line: Option<(usize, String)>,
}

/// 文件树搜索的一次结果快照。后台遍历项目填充，render 只读。
struct SearchState {
    /// 触发本次结果的查询串（用于判断是否需要重跑）。
    query: String,
    /// 后台遍历是否已跑完（false 时列表顶部显示「搜索中…」）。
    done: bool,
    /// 命中列表（文件名命中在前、内容命中在后，各自按路径序）。
    hits: Vec<SearchHit>,
    /// 是否因命中数触顶而截断（列表底部提示还有更多）。
    truncated: bool,
}

/// diff 行的类型，决定行号显示、前景色、整行背景与左侧色条。
#[derive(Clone, Copy, PartialEq)]
enum DiffKind {
    Add,     // 增行（+）
    Del,     // 删行（-）
    Context, // 上下文行（空格）
    Hunk,    // @@ 段头
    Meta,    // diff/index/+++/--- 等元信息
}

/// 一行 diff：旧/新行号（None 表示该侧无此行）、类型、去掉 +/-/空格前缀的文本。
/// segments 为 Some 时表示做过行内 diff：每段 (文本, 是否变化)，变化段渲染时上深底。
struct DiffLine {
    old_ln: Option<u32>,
    new_ln: Option<u32>,
    kind: DiffKind,
    text: String,
    segments: Option<Vec<(String, bool)>>,
}

/// Git 视图里当前选中查看的文件 diff：文件相对路径 + 结构化的 diff 行。
/// 用 Rc 供 uniform_list 闭包共享。
struct GitDiff {
    path: String,
    lines: Rc<Vec<DiffLine>>,
}

/// 一次 `git status` 的缓存结果（后台跑、render 只读，绝不在 render 同步跑 git）。
#[derive(Clone, Default)]
struct GitStatusData {
    /// git 命令是否成功（false = 不是 git 仓库 / git 不可用）。
    ok: bool,
    /// 当前分支名。
    branch: String,
    /// 改动文件：(porcelain 两位状态码, 路径)。
    files: Vec<(String, String)>,
}

/// 工作台的持久化状态：主区分屏布局树 + 活动叶子 + 侧栏宽度。
/// 存 ~/.smelt/workspace.json，启动时据此重建分屏（结构 / 嵌套 / 方向完整恢复）。
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct WsState {
    /// 所有会话（每个 = 一棵分屏树 + 会话内活动叶子遍历序）。
    #[serde(default)]
    sessions: Vec<SessionState>,
    /// 当前活动会话索引。
    #[serde(default)]
    active_session: usize,
    /// 会话侧栏拖出的宽度（px）；None = 用默认值。
    #[serde(default)]
    sidebar_w: Option<f32>,
    // --- 以下为旧存档兼容字段（读到就迁移，不再写出）---
    /// 旧格式：单棵分屏树。
    #[serde(default)]
    layout: Option<PaneState>,
    /// 更旧格式：终端 cwd 列表（每个迁移成一个独立会话）。
    #[serde(default)]
    tabs: Vec<Option<String>>,
    /// 旧格式的活动索引。
    #[serde(default)]
    active: usize,
}

/// 单个会话的持久化镜像：分屏树 + 会话内活动叶子（遍历序）。
#[derive(serde::Serialize, serde::Deserialize)]
struct SessionState {
    layout: PaneState,
    active: usize,
}

/// 可序列化的分屏布局镜像：叶子存该终端 cwd + 守护会话 id，Split 存方向 + 子节点。
/// 拖动比例暂不持久化，重开按均分；结构 / 嵌套 / 方向完整恢复。
/// id 用于重开 GUI 时 reattach smeltd 里还活着的会话（旧存档无 id → 开新会话）。
#[derive(serde::Serialize, serde::Deserialize)]
enum PaneState {
    Leaf {
        cwd: Option<String>,
        #[serde(default)]
        id: Option<String>,
    },
    Split { axis: SplitAxis, children: Vec<PaneState> },
}

/// 新会话 id（uuid v4）：GUI 与 smeltd 之间的持久身份。
fn new_sid() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Hsla → 0xRRGGBB（取色器回调把颜色写回 config 用）。
fn hsla_to_rgb(c: Hsla) -> u32 {
    let rgba = Rgba::from(c);
    let q = |f: f32| ((f.clamp(0.0, 1.0) * 255.0).round() as u32) & 0xff;
    (q(rgba.r) << 16) | (q(rgba.g) << 8) | q(rgba.b)
}

/// Split 方向的可序列化镜像（gpui::Axis 无法直接序列化）。
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy)]
enum SplitAxis {
    H,
    V,
}

impl From<Axis> for SplitAxis {
    fn from(a: Axis) -> Self {
        if matches!(a, Axis::Horizontal) {
            SplitAxis::H
        } else {
            SplitAxis::V
        }
    }
}

impl From<SplitAxis> for Axis {
    fn from(a: SplitAxis) -> Self {
        match a {
            SplitAxis::H => Axis::Horizontal,
            SplitAxis::V => Axis::Vertical,
        }
    }
}

/// 把渲染用的布局树导出成可序列化镜像（叶子读取各终端当前 cwd）。
fn pane_to_state(pane: &Pane, cx: &App) -> PaneState {
    match pane {
        Pane::Leaf(t) => PaneState::Leaf {
            cwd: t.read(cx).cwd(),
            id: Some(t.read(cx).session_id().to_string()),
        },
        Pane::Split { axis, children, .. } => PaneState::Split {
            axis: (*axis).into(),
            children: children.iter().map(|c| pane_to_state(c, cx)).collect(),
        },
    }
}

/// 按存档镜像重建布局树：深度优先遍历，遇叶子就新建终端并 push 到 tabs
/// （push 顺序即「遍历序」，与存档里的 active 索引一致）。
fn rebuild_pane(
    ps: &PaneState,
    tabs: &mut Vec<Entity<TerminalView>>,
    cx: &mut Context<Workspace>,
) -> Pane {
    match ps {
        PaneState::Leaf { cwd, id } => {
            // 有存档 id → reattach 守护里还活着的会话；旧存档无 id → 开新会话。
            let sid = id.clone().unwrap_or_else(new_sid);
            let v = cx.new(|cx| TerminalView::new(cx, cwd.clone(), sid, None));
            tabs.push(v.clone());
            Pane::Leaf(v)
        }
        PaneState::Split { axis, children } => {
            let state = cx.new(|_| ResizableState::default());
            let children = children.iter().map(|c| rebuild_pane(c, tabs, cx)).collect();
            Pane::Split { axis: (*axis).into(), state, children }
        }
    }
}

/// 收集布局树所有叶子终端的 EntityId，顺序 = 深度优先遍历序（= 存档 active 基准）。
fn collect_leaf_ids(pane: &Pane, out: &mut Vec<EntityId>) {
    match pane {
        Pane::Leaf(t) => out.push(t.entity_id()),
        Pane::Split { children, .. } => {
            for c in children {
                collect_leaf_ids(c, out);
            }
        }
    }
}

/// 老版本 appearance.json 没有 theme_mode 字段时的回退——保持升级前就有的深色观感，
/// 不能落到 ThemeMode::default()（Light）。
fn default_theme_mode() -> ThemeMode {
    ThemeMode::Dark
}

/// 终端外观设置（全局单例，供所有终端渲染读取；存 ~/.smelt/appearance.json）。
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Appearance {
    /// 终端底色（0xRRGGBB）。
    bg_color: u32,
    /// 背景图片绝对路径（None = 无）。
    bg_image: Option<String>,
    /// 不透明度 0.3–1.0；<1 时窗口转透明/模糊，桌面透出。
    opacity: f32,
    /// 毛玻璃模糊（macOS vibrancy，配合透明使用）。
    blur: bool,
    /// 明暗主题模式。
    #[serde(default = "default_theme_mode")]
    theme_mode: ThemeMode,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            bg_color: 0x1a1b26,
            bg_image: None,
            opacity: 1.0,
            blur: false,
            theme_mode: ThemeMode::Dark,
        }
    }
}

impl Global for Appearance {}

impl Appearance {
    /// 据当前设置推导窗口背景外观。
    fn window_bg(&self) -> WindowBackgroundAppearance {
        if self.blur {
            WindowBackgroundAppearance::Blurred
        } else if self.opacity < 1.0 {
            WindowBackgroundAppearance::Transparent
        } else {
            WindowBackgroundAppearance::Opaque
        }
    }
}

/// 外观设置文件路径：~/.smelt/appearance.json。
fn appearance_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".smelt").join("appearance.json"))
}

/// 读取外观设置；缺失/损坏回退默认。
fn load_appearance() -> Appearance {
    appearance_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// 写回外观设置（失败静默忽略）。
fn save_appearance(a: &Appearance) {
    let Some(path) = appearance_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string_pretty(a) {
        let _ = std::fs::write(path, json);
    }
}

/// Claude Code 快捷启动的权限模式（全局单例，存 ~/.smelt/launch.json）。
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct LaunchConfig {
    /// 开启后项目行「+」的 Claude Code 快捷入口改用
    /// `claude --dangerously-skip-permissions` 启动，跳过所有权限确认。
    claude_full_permissions: bool,
}

impl Default for LaunchConfig {
    fn default() -> Self {
        Self { claude_full_permissions: false }
    }
}

impl Global for LaunchConfig {}

fn launch_config_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".smelt").join("launch.json"))
}

/// 读取启动配置；缺失/损坏回退默认（默认不跳过权限确认，更安全）。
fn load_launch_config() -> LaunchConfig {
    launch_config_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// 写回启动配置（失败静默忽略）。
fn save_launch_config(c: &LaunchConfig) {
    let Some(path) = launch_config_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string_pretty(c) {
        let _ = std::fs::write(path, json);
    }
}

/// 改启动配置全局 + 存盘，不触发 view 重绘，用法同 [`apply_appearance`]。
fn apply_launch_config(f: impl FnOnce(&mut LaunchConfig), cx: &mut App) {
    let mut c = cx.global::<LaunchConfig>().clone();
    f(&mut c);
    save_launch_config(&c);
    cx.set_global(c);
}

/// 改外观全局 + 存盘，不触发 view 重绘（调用方按需自己 notify/refresh）。
/// 供只有 `&mut App`（没有 `Context<Self>`）的场景用，比如设置页 SettingField 的 get/set 闭包。
fn apply_appearance(f: impl FnOnce(&mut Appearance), cx: &mut App) {
    let mut a = cx.global::<Appearance>().clone();
    f(&mut a);
    save_appearance(&a);
    cx.set_global(a);
}

/// 改桌面宠物配置全局 + 存盘 + 显隐同步，不触发 view 重绘，用法同 [`apply_appearance`]。
fn apply_pet_config(f: impl FnOnce(&mut pet::PetConfig), cx: &mut App) {
    let mut c = cx.global::<pet::PetConfig>().clone();
    let was_enabled = c.enabled;
    f(&mut c);
    pet::save_pet_config(&c);
    if c.enabled != was_enabled {
        pet::sync_pet_window_visibility(cx, c.enabled);
    }
    cx.set_global(c);
}

/// 改宠物大脑（LLM）配置全局 + 存盘，不触发 view 重绘，用法同 [`apply_appearance`]。
fn apply_llm_config(f: impl FnOnce(&mut agent::LlmConfig), cx: &mut App) {
    let mut c = cx.global::<agent::LlmConfig>().clone();
    f(&mut c);
    agent::save_llm_config(&c);
    cx.set_global(c);
}

/// 存档文件路径：~/.smelt/workspace.json。
fn ws_state_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".smelt").join("workspace.json"))
}

/// 读取存档；文件不存在/损坏都返回 None，交由调用方回退默认。
fn load_ws_state() -> Option<WsState> {
    let data = std::fs::read_to_string(ws_state_path()?).ok()?;
    serde_json::from_str(&data).ok()
}

/// 工作台根视图：多标签终端管理器。
struct Workspace {
    /// 所有会话；每个会话 = 一棵独立分屏树 + 会话内活动 pane。
    sessions: Vec<Session>,
    /// 当前活动会话索引（主区显示它、侧栏高亮它）。
    active_session: usize,
    /// 主区当前视图：终端 / 文件树 / Git。
    view: MainView,
    /// 文件树里已展开的文件夹绝对路径。
    expanded: HashSet<String>,
    /// 目录列表缓存（绝对路径 → 已排序过滤的直接子项 (名, 是否目录)）。后台读盘填充，
    /// render 只读；此前 file_tree 在 render 里同步 fs::read_dir，大目录会像 git
    /// status 那样掉帧，这里改用同款「后台刷新 + 缓存 + render 只读」模式修复。
    dir_cache: HashMap<String, (Instant, Rc<Vec<(String, bool)>>)>,
    /// 正在后台读取的目录（防重复并发 spawn）。
    dir_inflight: HashSet<String>,
    /// 当前在文件树里打开查看的文件（含预高亮的行数据）。
    open_file: Option<OpenFile>,
    /// 打开文件的自增序号：后台高亮完成时用它判断结果是否已过期（切了别的文件）。
    file_gen: u64,
    /// 当前文件有未保存改动时，用户又点了别的文件——先记下目标路径弹确认弹窗，
    /// 等用户选了"不保存"/"保存并切换"才真正打开，见 render_unsaved_file_confirm。
    pending_file_switch: Option<String>,
    /// 「保存并切换」选择后，等这次 save_open_file 存盘成功再打开的目标路径；
    /// 存盘失败/冲突则放弃切换，留在当前文件上让用户处理。
    pending_switch_after_save: Option<String>,
    /// Git 视图里当前查看的文件 diff；None 表示未选中任何文件。
    git_diff: Option<GitDiff>,
    /// 打开 diff 的自增序号（独立于 file_gen，避免和文件高亮任务互相取消）。
    diff_gen: u64,
    /// diff 是否用并排（split）视图；false 为统一（unified）视图。
    diff_split: bool,
    /// 交互式 diff：选中待评论的行号集合（对应 GitDiff.lines 下标），换文件/重开 diff 时清空。
    diff_selected: HashSet<usize>,
    /// 交互式 diff 的评论输入框（懒创建，随 Git 视图渲染出待发送的 diff 时创建）。
    diff_comment_input: Option<Entity<gpui_component::input::InputState>>,
    /// 左侧会话侧栏是否展开（Cmd+B 切换）。
    sidebar_open: bool,
    /// 通知面板是否打开（标题栏铃铛切换）。
    notifications_open: bool,
    /// 命令面板（Cmd+K）；None 表示未打开。搜索/导航/确认由 ListState 负责。
    palette: Option<Entity<ListState<CmdDelegate>>>,
    /// 命令面板的事件订阅（确认/取消）；随面板关闭一并释放。
    _palette_sub: Option<Subscription>,
    /// 各滚动区的常驻滚动句柄——供 gpui-component Scrollbar 读取位置并绘制。
    /// 必须常驻（每帧新建会丢失滚动位置）。
    git_files_scroll: ScrollHandle,
    diff_scroll: UniformListScrollHandle,
    /// 文件树列表的滚动句柄（普通滚动，非虚拟滚动——见 file_tree 函数注释）。
    file_tree_scroll: ScrollHandle,
    /// 文件树顶部的过滤输入框；首次渲染文件树时懒创建（需要 window）。
    file_filter: Option<Entity<gpui_component::input::InputState>>,
    /// 过滤框的变更订阅（键入即重渲染）；随视图存活。
    _file_filter_sub: Option<Subscription>,
    /// 文件树搜索结果（文件名 + 文件内容）：后台遍历项目填充，render 只读。
    /// query 非空时左栏由树形切换为扁平命中列表。
    search_results: Option<SearchState>,
    /// 搜索任务自增序号：后台遍历完成时用它丢弃过期结果（期间又改了查询）。
    search_gen: u64,
    /// 根布局左右分栏（会话侧栏 ↔ 主区）的可拖拽状态；常驻以保住拖出的宽度。
    root_resize: Entity<ResizableState>,
    /// 侧栏初始宽度（px）：启动时从存档恢复，作为 resizable_panel 的初始 size。
    sidebar_w: f32,
    /// 侧栏 resize 事件订阅（拖动完写回存档）；随视图存活。
    _resize_sub: Subscription,
    /// git 信息缓存（cwd → (分支, 改动数)），总览页后台刷新、渲染读缓存。
    git_cache: HashMap<String, (String, usize)>,
    /// 宠物大脑（LLM）配置的输入框；首次打开设置面板时懒创建（需要 window）。
    llm_inputs: Option<LlmInputs>,
    /// 上面几个输入框的变更订阅（保活；随视图存活）。
    llm_subs: Vec<Subscription>,
    /// 设置面板的有状态组件（懒创建）：不透明度滑块 + 背景色 / 宠物色取色器。
    opacity_slider: Option<Entity<SliderState>>,
    bg_color_picker: Option<Entity<ColorPickerState>>,
    pet_color_picker: Option<Entity<ColorPickerState>>,
    /// 上面三个组件的变更订阅。
    settings_subs: Vec<Subscription>,
    /// 上次应用到窗口的背景外观：不透明度 / 模糊改了要 window 才能切，故在 render 里同步。
    applied_window_bg: Option<WindowBackgroundAppearance>,
    /// git status 缓存（root → (取得时刻, 数据)）。Git 页后台刷新，render 只读，
    /// 避免每帧同步跑 git status（大仓要 ~90ms，是掉帧元凶）。
    git_status: HashMap<String, (Instant, GitStatusData)>,
    /// 正在后台刷新 status 的 root（防重复并发 spawn）。
    git_status_inflight: HashSet<String>,
    /// 文件监听标脏的 root 集合：notify 的回调跑在独立系统线程上，故用 Arc<Mutex<..>>
    /// 跨线程共享；250ms 检查循环（见 ensure_git_watch）发现命中就清位 + 强制刷新。
    git_dirty: Arc<Mutex<HashSet<String>>>,
    /// 每个 root 常驻的文件监听器（root → watcher）。watcher 必须存活才会继续收事件，
    /// 故存在 Workspace 里跟应用同生命周期；只建一次，见 ensure_git_watch。
    git_watchers: HashMap<String, RecommendedWatcher>,
    /// 热力图缓存（root → (取得时刻, 数据)）：`git log` 扫 90 天历史比 status 更慢，
    /// 同样绝不在 render 里同步跑，后台算完缓存，render 只读。
    hotspot_data: HashMap<String, (Instant, Rc<Vec<hotspot::HotspotEntry>>)>,
    /// 正在后台计算热力的 root（防重复并发 spawn）。
    hotspot_inflight: HashSet<String>,
    /// 历史会话列表缓存（cwd → (取得时刻, 数据)）：后台扫描该项目下的 transcript
    /// 目录，render 只读。
    session_list: HashMap<String, (Instant, Rc<Vec<session_history::SessionSummary>>)>,
    /// 正在后台扫描历史会话列表的 cwd（防重复并发 spawn）。
    session_list_inflight: HashSet<String>,
    /// 当前选中查看的历史会话（路径 + 解析出的对话内容）；None 表示未选。
    session_detail: Option<(PathBuf, Rc<session_history::SessionDetail>)>,
    /// 加载会话详情的自增序号：后台解析完成时用它判断结果是否已过期（切了别的会话）。
    session_detail_gen: u64,
    /// 调试 HUD 开关（Cmd+Shift+F 切换）：开启时右上角显示帧率 + 帧耗时。
    debug_hud: bool,
    /// 上一帧渲染时刻（算帧间隔用）。
    last_frame: Option<Instant>,
    /// 平滑后的帧率（EMA）。
    fps_ema: f32,
    /// 退出确认拦截弹窗开关
    show_quit_confirm: bool,
    /// 在线更新状态机（检查/下载/暂存就绪），驱动设置页"更新"分区 + 齿轮强调色。
    update_status: updater::UpdateStatus,
    /// 上次同步给 Dock 角标的「需要关注」会话数；None 强制首帧同步一次。
    /// 只在这个数变化时才调用 Cocoa API，避免每次 render 都发一遍。
    dock_badge_count: Option<usize>,
    /// 用量页数据缓存：(取得时刻, 数据)。扫全部本地 transcript 可能有几十毫秒，
    /// 绝不在 render 里同步跑，后台算完缓存，render 只读。
    usage_cache: Option<(Instant, Rc<usage_stats::UsageData>)>,
    /// 正在后台扫描用量数据（防重复并发 spawn）。
    usage_inflight: bool,
    /// 会话拖拽悬停中的插入位置：(目标会话, 插它前面?)。由 drop 层的 on_drag_move
    /// 维护，驱动插入指示条的出现动画；起拖时清空，避免上次拖拽的残留闪一帧。
    sess_drop_hint: Option<(EntityId, bool)>,
    /// 项目分组拖拽悬停中的目标项目名，作用同上。
    proj_drop_hint: Option<SharedString>,
    /// 守护进程是否落后于磁盘上的 smeltd 二进制（重装/重编译后常见，需手动重启守护
    /// 才生效新代码）；None 表示还没查过，驱动设置页「更新」分区的重启提示。
    daemon_outdated: Option<bool>,
    /// 「重启守护进程」二次确认弹窗开关：点确定会断开所有当前终端会话。
    show_daemon_restart_confirm: bool,
}

/// 宠物大脑配置的四个输入框（base_url / api_key / model / persona）。
#[derive(Clone)]
struct LlmInputs {
    base_url: Entity<gpui_component::input::InputState>,
    api_key: Entity<gpui_component::input::InputState>,
    model: Entity<gpui_component::input::InputState>,
    persona: Entity<gpui_component::input::InputState>,
}

impl Workspace {
    fn new(cx: &mut Context<Self>) -> Self {
        // 优先按存档的会话列表重建；旧存档（单树 / cwd 列表）迁移，无存档则默认单会话。
        let saved = load_ws_state();
        let sidebar_w = saved.as_ref().and_then(|s| s.sidebar_w).unwrap_or(230.);

        let mut sessions: Vec<Session> = Vec::new();
        let mut active_session = 0;
        if let Some(s) = saved.as_ref() {
            if !s.sessions.is_empty() {
                for ss in &s.sessions {
                    let mut leaves = Vec::new();
                    let layout = rebuild_pane(&ss.layout, &mut leaves, cx);
                    if let Some(active) = leaves.get(ss.active).or_else(|| leaves.first()).cloned() {
                        sessions.push(Session { layout, active });
                    }
                }
                active_session = s.active_session;
            } else if let Some(ps) = &s.layout {
                // 旧格式：单棵树 → 一个会话。
                let mut leaves = Vec::new();
                let layout = rebuild_pane(ps, &mut leaves, cx);
                if let Some(active) = leaves.get(s.active).or_else(|| leaves.first()).cloned() {
                    sessions.push(Session { layout, active });
                }
            } else {
                // 更旧格式：cwd 列表 → 每个 cwd 一个独立会话。
                for cwd in s.tabs.clone() {
                    let v = cx.new(|cx| TerminalView::new(cx, cwd, new_sid(), None));
                    sessions.push(Session::single(v));
                }
                active_session = s.active;
            }
        }
        // 默认零会话：由用户自行「+ / 打开项目」创建，不再兜底建默认终端。
        active_session = active_session.min(sessions.len().saturating_sub(1));

        // 订阅侧栏 resize：拖动完 emit Resized，写回存档以持久化宽度。
        let root_resize = cx.new(|_| ResizableState::default());
        let _resize_sub = cx.subscribe(&root_resize, |this, _state, _e: &ResizablePanelEvent, cx| {
            this.save_state(cx);
        });

        let mut ws = Self {
            sessions,
            active_session,
            view: MainView::Terminal,
            expanded: HashSet::new(),
            dir_cache: HashMap::new(),
            dir_inflight: HashSet::new(),
            open_file: None,
            file_gen: 0,
            pending_file_switch: None,
            pending_switch_after_save: None,
            git_diff: None,
            diff_gen: 0,
            diff_split: false,
            diff_selected: HashSet::new(),
            diff_comment_input: None,
            sidebar_open: true,
            notifications_open: false,
            palette: None,
            _palette_sub: None,
            git_files_scroll: ScrollHandle::new(),
            diff_scroll: UniformListScrollHandle::new(),
            file_tree_scroll: ScrollHandle::new(),
            file_filter: None,
            _file_filter_sub: None,
            search_results: None,
            search_gen: 0,
            root_resize,
            sidebar_w,
            _resize_sub,
            git_cache: HashMap::new(),
            git_status: HashMap::new(),
            git_status_inflight: HashSet::new(),
            git_dirty: Arc::new(Mutex::new(HashSet::new())),
            git_watchers: HashMap::new(),
            hotspot_data: HashMap::new(),
            hotspot_inflight: HashSet::new(),
            session_list: HashMap::new(),
            session_list_inflight: HashSet::new(),
            session_detail: None,
            session_detail_gen: 0,
            llm_inputs: None,
            llm_subs: Vec::new(),
            opacity_slider: None,
            bg_color_picker: None,
            pet_color_picker: None,
            settings_subs: Vec::new(),
            applied_window_bg: None,
            debug_hud: false,
            last_frame: None,
            fps_ema: 0.0,
            show_quit_confirm: false,
            update_status: updater::UpdateStatus::default(),
            dock_badge_count: None,
            usage_cache: None,
            usage_inflight: false,
            sess_drop_hint: None,
            proj_drop_hint: None,
            daemon_outdated: None,
            show_daemon_restart_confirm: false,
        };
        // 立即写盘：把本次启动生成/沿用的会话 id 落到存档。否则首启（或旧存档迁移）
        // 生成的新 id 只在内存里，若用户不做任何布局操作就退出，重开会因无 id 而
        // 新开 shell，守护里旧会话成孤儿 —— reattach 全靠这一步。
        ws.save_state(cx);
        updater::cleanup_stale_backup();
        ws.check_for_update(true, cx);
        ws.check_daemon_outdated(cx);
        ws
    }

    /// 懒创建宠物大脑配置的输入框（需要 window，故在首次渲染设置面板时调）。
    /// 每个框预填当前配置值，变更时写回 LlmConfig 并存盘。
    fn init_llm_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use gpui_component::input::{InputEvent, InputState};
        let lc = cx.global::<agent::LlmConfig>().clone();

        let base_url = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("https://api.deepseek.com/chat/completions")
                .default_value(lc.base_url.clone())
        });
        let api_key = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("sk-...（留空则用 config.toml/env）")
                .masked(true)
                .default_value(lc.api_key.clone())
        });
        let model = cx.new(|cx| {
            InputState::new(window, cx).placeholder("deepseek-chat").default_value(lc.model.clone())
        });
        let persona = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .auto_grow(2, 5)
                .placeholder("人设 / system prompt")
                .default_value(lc.persona.clone())
        });

        // 变更即写回对应字段（Change 覆盖键入，Blur 兜底）。
        let save_on = |ev: &InputEvent| matches!(ev, InputEvent::Change | InputEvent::Blur);
        self.llm_subs.clear();
        self.llm_subs.push(cx.subscribe(&base_url, move |this, s, ev: &InputEvent, cx| {
            if save_on(ev) {
                let v = s.read(cx).value().to_string();
                this.update_llm_config(|c| c.base_url = v, cx);
            }
        }));
        self.llm_subs.push(cx.subscribe(&api_key, move |this, s, ev: &InputEvent, cx| {
            if save_on(ev) {
                let v = s.read(cx).value().to_string();
                this.update_llm_config(|c| c.api_key = v, cx);
            }
        }));
        self.llm_subs.push(cx.subscribe(&model, move |this, s, ev: &InputEvent, cx| {
            if save_on(ev) {
                let v = s.read(cx).value().to_string();
                this.update_llm_config(|c| c.model = v, cx);
            }
        }));
        self.llm_subs.push(cx.subscribe(&persona, move |this, s, ev: &InputEvent, cx| {
            if save_on(ev) {
                let v = s.read(cx).value().to_string();
                this.update_llm_config(|c| c.persona = v, cx);
            }
        }));

        self.llm_inputs = Some(LlmInputs { base_url, api_key, model, persona });

        // —— 有状态组件：不透明度滑块 + 背景色 / 宠物色取色器 ——
        let ap = cx.global::<Appearance>().clone();
        let pc = cx.global::<pet::PetConfig>().clone();
        let opacity_slider = cx.new(|_| {
            SliderState::new().min(60.0).max(100.0).step(5.0).default_value(ap.opacity * 100.0)
        });
        let bg_color_picker =
            cx.new(|cx| ColorPickerState::new(window, cx).default_value(rgb(ap.bg_color)));
        let pet_color_picker =
            cx.new(|cx| ColorPickerState::new(window, cx).default_value(rgb(pc.color)));

        self.settings_subs.clear();
        self.settings_subs.push(cx.subscribe(
            &opacity_slider,
            |this, _s, ev: &SliderEvent, cx| {
                let (SliderEvent::Change(v) | SliderEvent::Release(v)) = ev;
                if let SliderValue::Single(x) = v {
                    let op = (*x / 100.0).clamp(0.3, 1.0);
                    this.set_appearance(move |a| a.opacity = op, cx);
                }
            },
        ));
        self.settings_subs.push(cx.subscribe(
            &bg_color_picker,
            |this, _s, ev: &ColorPickerEvent, cx| {
                let ColorPickerEvent::Change(c) = ev;
                if let Some(hsla) = c {
                    let color = hsla_to_rgb(*hsla);
                    this.set_appearance(move |a| a.bg_color = color, cx);
                }
            },
        ));
        self.settings_subs.push(cx.subscribe(
            &pet_color_picker,
            |this, _s, ev: &ColorPickerEvent, cx| {
                let ColorPickerEvent::Change(c) = ev;
                if let Some(hsla) = c {
                    let color = hsla_to_rgb(*hsla);
                    this.update_pet_config(move |cfg| cfg.color = color, cx);
                }
            },
        ));
        self.opacity_slider = Some(opacity_slider);
        self.bg_color_picker = Some(bg_color_picker);
        self.pet_color_picker = Some(pet_color_picker);
    }

    /// 后台刷新所有会话 cwd 的 git 信息（分支 + 改动数）到缓存，进总览时调用。
    fn refresh_git(&mut self, cx: &mut Context<Self>) {
        let cwds: Vec<String> = self.sessions.iter().filter_map(|s| s.cwd(cx)).collect();
        cx.spawn(async move |this, cx| {
            let results = cx
                .background_executor()
                .spawn(async move {
                    let mut out: Vec<(String, String, usize)> = Vec::new();
                    for cwd in cwds {
                        let branch = std::process::Command::new("git")
                            .args(["-C", &cwd, "rev-parse", "--abbrev-ref", "HEAD"])
                            .output()
                            .ok()
                            .filter(|o| o.status.success())
                            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
                        if let Some(branch) = branch {
                            let changed = std::process::Command::new("git")
                                .args(["-C", &cwd, "status", "--porcelain"])
                                .output()
                                .ok()
                                .map(|o| String::from_utf8_lossy(&o.stdout).lines().count())
                                .unwrap_or(0);
                            out.push((cwd, branch, changed));
                        }
                    }
                    out
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                for (cwd, branch, changed) in results {
                    this.git_cache.insert(cwd, (branch, changed));
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 当前活动会话（不可变引用）。
    fn cur(&self) -> Option<&Session> {
        self.sessions.get(self.active_session)
    }

    /// 按会话的 cwd 分组成「项目」：(项目名, cwd, 该项目下的会话下标列表)，
    /// 顺序 = 会话在 self.sessions 里首次出现的顺序。侧栏渲染和拖拽排序共用同一份算法，
    /// 避免两处各算一遍、行为跑偏。临时终端（cwd 落在 scratch_dir）单独归一组「临时终端」。
    fn project_groups(&self, cx: &App) -> Vec<(String, String, Vec<usize>)> {
        let mut projects: Vec<(String, String, Vec<usize>)> = Vec::new();
        for (ix, s) in self.sessions.iter().enumerate() {
            let cwd = s.cwd(cx).unwrap_or_default();
            let name = project_name_for_cwd(&cwd);
            match projects.iter_mut().find(|(n, _, _)| *n == name) {
                Some(p) => p.2.push(ix),
                None => projects.push((name, cwd, vec![ix])),
            }
        }
        projects
    }

    /// 拖拽排序：把 dragged 会话挪到 target 会话旁边（before=true 插到它前面，否则插到
    /// 它后面）。只在同一项目内生效——这是「项目内排序」，不是「跨项目挪会话」，
    /// dragged/target 分属不同项目时直接不动。用 entity_id 找位置而非缓存的下标：拖拽
    /// 跨越多帧，下标可能因为其间的关会话等操作失效。
    fn move_session_near(
        &mut self,
        dragged: EntityId,
        target: EntityId,
        before: bool,
        cx: &mut Context<Self>,
    ) {
        if dragged == target {
            return;
        }
        let groups = self.project_groups(cx);
        let group_of = |id: EntityId| {
            groups
                .iter()
                .position(|(_, _, ixs)| ixs.iter().any(|&ix| self.sessions[ix].active.entity_id() == id))
        };
        let (Some(dragged_group), Some(target_group)) = (group_of(dragged), group_of(target)) else {
            return;
        };
        if dragged_group != target_group {
            return;
        }
        let Some(from_ix) = self.sessions.iter().position(|s| s.active.entity_id() == dragged) else {
            return;
        };
        let Some(target_ix) = self.sessions.iter().position(|s| s.active.entity_id() == target) else {
            return;
        };

        let active_id = self.cur().map(|s| s.active.entity_id());
        let session = self.sessions.remove(from_ix);
        let adjusted_target_ix = if from_ix < target_ix { target_ix - 1 } else { target_ix };
        let insert_at = adjusted_target_ix + if before { 0 } else { 1 };
        self.sessions.insert(insert_at, session);

        if let Some(id) = active_id {
            if let Some(ix) = self.sessions.iter().position(|s| s.active.entity_id() == id) {
                self.active_session = ix;
            }
        }
        self.save_state(cx);
        cx.notify();
    }

    /// 拖拽排序：把 from 项目的所有会话（保持相对顺序）整体挪到 to 项目最前面。
    fn move_project_near(&mut self, from_name: SharedString, to_name: SharedString, cx: &mut Context<Self>) {
        if from_name == to_name {
            return;
        }
        let groups = self.project_groups(cx);
        let Some((_, _, from_ixs)) = groups.iter().find(|(n, _, _)| n.as_str() == from_name.as_ref())
        else {
            return;
        };
        if !groups.iter().any(|(n, _, _)| n.as_str() == to_name.as_ref()) {
            return;
        }
        let mut from_ixs = from_ixs.clone();
        from_ixs.sort_unstable();

        let active_id = self.cur().map(|s| s.active.entity_id());
        // 降序 remove 保证前面下标不受后面删除影响；收集完再倒回原相对顺序。
        let mut moved: Vec<Session> = from_ixs.iter().rev().map(|&ix| self.sessions.remove(ix)).collect();
        moved.reverse();

        let insert_at = self
            .sessions
            .iter()
            .position(|s| {
                let cwd = s.cwd(cx).unwrap_or_default();
                project_name_for_cwd(&cwd) == to_name.as_ref()
            })
            .unwrap_or(self.sessions.len());
        for (i, s) in moved.into_iter().enumerate() {
            self.sessions.insert(insert_at + i, s);
        }

        if let Some(id) = active_id {
            if let Some(ix) = self.sessions.iter().position(|s| s.active.entity_id() == id) {
                self.active_session = ix;
            }
        }
        self.save_state(cx);
        cx.notify();
    }

    /// 「+」/新建：开一个独立新会话（单终端），并切过去。
    fn add_session(&mut self, cwd: Option<String>, cx: &mut Context<Self>) {
        self.add_session_with_launch(cwd, None, cx);
    }

    /// 项目行「+」下拉菜单的 Claude Code / Codex 快捷入口：`launch` 编进 shell 的
    /// 启动命令行（见 terminal.rs::spawn / smeltd.rs::spawn_session），不是等
    /// shell 起来后再补发按键，从根上没有时序竞态。
    fn add_session_with_launch(
        &mut self,
        cwd: Option<String>,
        launch: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        let view = cx.new(|cx| TerminalView::new(cx, cwd, new_sid(), launch));
        self.sessions.push(Session::single(view));
        self.active_session = self.sessions.len() - 1;
        self.save_state(cx);
        cx.notify();
    }

    /// 在当前会话的活动 pane 上分屏：Horizontal=右侧并排，Vertical=下方堆叠。
    fn split_active(&mut self, axis: Axis, cx: &mut Context<Self>) {
        let Some(sess) = self.cur() else { return };
        let cwd = sess.active.read(cx).cwd().or_else(current_dir);
        let old = sess.active.entity_id();
        let view = cx.new(|cx| TerminalView::new(cx, cwd, new_sid(), None));
        let state = cx.new(|_| ResizableState::default());
        let sess = &mut self.sessions[self.active_session];
        split_leaf(&mut sess.layout, old, axis, state, view.clone());
        sess.active = view;
        self.save_state(cx);
        cx.notify();
    }

    /// 把所有会话（各自分屏树 + 活动叶子遍历序）+ 侧栏宽度写入 workspace.json（失败静默忽略）。
    fn save_state(&self, cx: &mut Context<Self>) {
        let Some(path) = ws_state_path() else { return };
        let sessions: Vec<SessionState> = self
            .sessions
            .iter()
            .map(|s| {
                let layout = pane_to_state(&s.layout, cx);
                let mut ids = Vec::new();
                collect_leaf_ids(&s.layout, &mut ids);
                let active = ids
                    .iter()
                    .position(|x| *x == s.active.entity_id())
                    .unwrap_or(0);
                SessionState { layout, active }
            })
            .collect();
        let sidebar_w = self.root_resize.read(cx).sizes().first().copied().map(f32::from);
        let state = WsState {
            sessions,
            active_session: self.active_session,
            sidebar_w,
            ..Default::default()
        };
        if let Ok(json) = serde_json::to_string_pretty(&state) {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&path, json);
        }
    }

    /// 「+」新建会话：继承当前会话活动终端的目录。
    fn new_tab(&mut self, cx: &mut Context<Self>) {
        let cwd = self.cur().and_then(|s| s.cwd(cx)).or_else(current_dir);
        self.add_session(cwd, cx);
    }

    /// 临时终端：不挂在任何项目下，固定落在 $HOME，侧栏单独分组「临时终端」。
    /// 跟"打开项目/项目内新建"平级，但不需要先切到某个项目才能开。
    fn new_scratch_session(&mut self, cx: &mut Context<Self>) {
        self.add_session(scratch_dir(), cx);
    }

    /// 顶部「新建终端」入口：已有临时终端就切过去，没有才新开一个
    /// （避免每次点这个常驻入口都新建一个空终端）。这个入口能从总览/设置页直接点，
    /// `activate`/`add_session` 都不管 `self.view`，这里补上，否则点了但看不到终端。
    fn activate_or_new_scratch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let home = scratch_dir();
        let existing = self.sessions.iter().position(|s| s.cwd(cx) == home);
        match existing {
            Some(ix) => self.activate(ix, window, cx),
            None => self.new_scratch_session(cx),
        }
        self.view = MainView::Terminal;
        self.focus_active(window, cx);
        cx.notify();
    }

    /// 「打开项目」：弹原生选择框选一个目录，在其中开新会话。
    fn open_project(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("选择项目目录".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                if let Some(dir) = paths.into_iter().next() {
                    let dir = dir.to_str().map(String::from);
                    this.update(cx, |this, cx| this.add_session(dir, cx)).ok();
                }
            }
        })
        .detach();
    }

    /// 从 Finder 拖入的路径各开一个会话：文件夹直接用，文件取其父目录。
    fn open_paths(&mut self, paths: &[std::path::PathBuf], cx: &mut Context<Self>) {
        for p in paths {
            let dir = if p.is_dir() { Some(p.as_path()) } else { p.parent() };
            if let Some(d) = dir.and_then(|d| d.to_str()) {
                self.add_session(Some(d.to_string()), cx);
            }
        }
    }

    /// 关闭第 ix 个会话（至少保留一个）。用户主动关 → 让守护杀掉这些 shell
    /// （区别于退出 GUI：那时不杀，会话在 smeltd 里持久活着）。
    fn close_session(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.sessions.len() <= 1 || ix >= self.sessions.len() {
            return;
        }
        let mut leaves = Vec::new();
        collect_leaves(&self.sessions[ix].layout, &mut leaves);
        for t in &leaves {
            terminal::kill_remote(t.read(cx).session_id());
        }
        self.sessions.remove(ix);
        if self.active_session >= self.sessions.len() {
            self.active_session = self.sessions.len() - 1;
        } else if self.active_session > ix {
            self.active_session -= 1;
        }
        self.save_state(cx);
        cx.notify();
    }

    /// Cmd+W：会话内多 pane 时关掉活动 pane（切到相邻），否则关整个会话。
    fn close_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sess) = self.cur() else { return };
        if sess.pane_count() > 1 {
            let target = sess.active.entity_id();
            // 用户主动关 pane → 守护真正杀掉该 shell。
            terminal::kill_remote(&sess.active.read(cx).session_id().to_string());
            let sess = &mut self.sessions[self.active_session];
            remove_leaf(&mut sess.layout, target);
            let mut leaves = Vec::new();
            collect_leaves(&sess.layout, &mut leaves);
            if let Some(first) = leaves.first().cloned() {
                sess.active = first;
            }
            self.focus_active(window, cx);
            self.save_state(cx);
            cx.notify();
        } else {
            self.close_session(self.active_session, cx);
            self.focus_active(window, cx);
        }
    }

    /// 点击 pane：把它设为当前会话的活动 pane 并聚焦（不换会话）。
    fn activate_pane(
        &mut self,
        e: &Entity<TerminalView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(sess) = self.sessions.get_mut(self.active_session) {
            sess.active = e.clone();
        }
        // 只聚焦、不清「需要注意」——查看≠处理，等用户实际输入回应了才清（见 TerminalView）。
        let h = e.read(cx).focus_handle();
        window.focus(&h, cx);
        self.save_state(cx);
        cx.notify();
    }

    /// 聚焦当前会话的活动终端。
    fn focus_active(&self, window: &mut Window, cx: &mut App) {
        if let Some(sess) = self.cur() {
            let h = sess.active.read(cx).focus_handle();
            window.focus(&h, cx);
        }
    }

    /// 切换到第 ix 个会话并聚焦。
    fn activate(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix < self.sessions.len() {
            self.active_session = ix;
            // 从总览点会话 → 进入该会话的终端视图。
            if self.view == MainView::Overview {
                self.view = MainView::Terminal;
            }
            // 切过去只是查看，不清「需要注意」——等用户实际输入回应了才清。
            self.focus_active(window, cx);
            self.save_state(cx);
            cx.notify();
        }
    }

    fn next_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let n = self.sessions.len();
        if n > 0 {
            self.activate((self.active_session + 1) % n, window, cx);
        }
    }

    fn prev_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let n = self.sessions.len();
        if n > 0 {
            self.activate((self.active_session + n - 1) % n, window, cx);
        }
    }

    /// 无 window 版：改全局 + 存盘 + 重绘。窗口背景（透明/模糊）由 render 里的
    /// applied_window_bg 同步——供 slider/color_picker 的订阅回调用（它们拿不到 window）。
    fn set_appearance(&mut self, f: impl FnOnce(&mut Appearance), cx: &mut Context<Self>) {
        apply_appearance(f, cx);
        cx.notify();
    }

    /// 修改桌面宠物配置：改全局 + 存盘 + 触发重绘。宠物窗口每帧读该全局，改动 ≤50ms 生效。
    fn update_pet_config(&mut self, f: impl FnOnce(&mut pet::PetConfig), cx: &mut Context<Self>) {
        apply_pet_config(f, cx);
        cx.notify();
    }

    /// 修改宠物大脑（LLM）配置：改全局 + 存盘 + 重绘。
    fn update_llm_config(&mut self, f: impl FnOnce(&mut agent::LlmConfig), cx: &mut Context<Self>) {
        apply_llm_config(f, cx);
        cx.notify();
    }

    /// 设置 / 清除背景图（不影响窗口透明度，故无需 window）。
    fn set_bg_image(&mut self, path: Option<String>, cx: &mut Context<Self>) {
        apply_appearance(|a| a.bg_image = path, cx);
        cx.notify();
    }

    /// 检查是否有新版本。`silent` 区分启动时的后台静默检查（离线/失败时不打扰用户，
    /// 悄悄退回 Idle）和设置页手动点「检查更新」（失败要如实展示原因）。
    /// 发现新版本会直接接上后台静默下载，不需要用户二次确认——这是"全自动静默更新"承诺的一环。
    fn check_for_update(&mut self, silent: bool, cx: &mut Context<Self>) {
        self.update_status = updater::UpdateStatus::Checking;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()?
                        .block_on(updater::fetch_latest())
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok((version, url)) if updater::is_newer(&version, env!("CARGO_PKG_VERSION")) => {
                        this.start_update_download(version, url, cx);
                        return; // start_update_download 里已经 notify 过
                    }
                    Ok(_) => this.update_status = updater::UpdateStatus::UpToDate,
                    Err(e) => {
                        this.update_status =
                            if silent { updater::UpdateStatus::Idle } else { updater::UpdateStatus::Failed(e.to_string()) };
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 后台静默下载新版 dmg 并暂存好 `.app`，完成后置 `ReadyToInstall`（不重启、不打断）。
    fn start_update_download(&mut self, version: String, url: String, cx: &mut Context<Self>) {
        self.update_status = updater::UpdateStatus::Downloading { version: version.clone() };
        cx.notify();
        cx.spawn(async move |this, cx| {
            let v = version.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()?
                        .block_on(updater::download_and_stage(&url, &v))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.update_status = match result {
                    Ok(staged_app) => updater::UpdateStatus::ReadyToInstall { version, staged_app },
                    Err(e) => updater::UpdateStatus::Failed(e.to_string()),
                };
                cx.notify();
            });
        })
        .detach();
    }

    /// 后台查一次守护是否落后于磁盘上的 smeltd 二进制，决定要不要在设置页/齿轮上
    /// 给出「重启守护」提示。本地 Unix socket 往返很快，但仍走后台线程，跟
    /// check_for_update 同款结构，别在 UI 线程里做阻塞 IO。
    fn check_daemon_outdated(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let outdated = cx.background_executor().spawn(async { terminal::daemon_outdated() }).await;
            let _ = this.update(cx, |this, cx| {
                this.daemon_outdated = Some(outdated);
                cx.notify();
            });
        })
        .detach();
    }

    /// 用户在弹窗里点了「确定重启」：让守护退出（断开所有会话）、拉起磁盘上最新的
    /// smeltd、再刷新状态。三步都涉及阻塞 IO（socket 往返 + 最坏 5s 轮询），全扔
    /// 后台线程，不卡 UI。新守护起来后，旧会话已经全死了（网格冻结、敲键盘没反应），
    /// 逐个 pane 调 reconnect() 换新会话顶上，不然只能靠用户手动发现、重开 GUI 才恢复。
    fn confirm_restart_daemon(&mut self, cx: &mut Context<Self>) {
        self.show_daemon_restart_confirm = false;
        self.daemon_outdated = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let outdated = cx
                .background_executor()
                .spawn(async {
                    terminal::restart_daemon();
                    terminal::ensure_daemon_running();
                    terminal::daemon_outdated()
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.daemon_outdated = Some(outdated);
                for sess in &this.sessions {
                    let mut leaves = Vec::new();
                    collect_leaves(&sess.layout, &mut leaves);
                    for leaf in leaves {
                        leaf.update(cx, |view, cx| view.reconnect(cx));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 弹原生选择框选一张背景图。
    fn pick_bg_image(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("选择背景图片".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                if let Some(p) =
                    paths.into_iter().next().and_then(|p| p.to_str().map(String::from))
                {
                    this.update(cx, |this, cx| this.set_bg_image(Some(p), cx)).ok();
                }
            }
        })
        .detach();
    }

    /// 收集所有待处理通知：(会话索引, pane 终端, 消息文本)。
    /// 排除「正在看的那个活动 pane」——用户已在看，不算待处理。
    fn collect_notifications(&self, cx: &App) -> Vec<(usize, Entity<TerminalView>, String)> {
        let mut out = Vec::new();
        for (si, s) in self.sessions.iter().enumerate() {
            let viewing = (si == self.active_session).then(|| s.active.entity_id());
            let mut leaves = Vec::new();
            collect_leaves(&s.layout, &mut leaves);
            for t in leaves {
                if Some(t.entity_id()) == viewing {
                    continue;
                }
                if let Some(msg) = t.read(cx).notification() {
                    out.push((si, t.clone(), msg.to_string()));
                }
            }
        }
        out
    }

    /// 跳到某条通知：切到该会话 + 聚焦该 pane（顺带清除通知）+ 关面板。
    fn goto_notification(
        &mut self,
        session_ix: usize,
        pane: &Entity<TerminalView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.activate(session_ix, window, cx);
        self.activate_pane(pane, window, cx);
        self.notifications_open = false;
        cx.notify();
    }

    /// 渲染通知面板浮层（标题栏铃铛打开）：列出所有待处理会话 + 消息，点击跳转。
    fn render_notifications(&self, cx: &mut Context<Self>) -> AnyElement {
        let (fg, muted, border, popover) = {
            let t = cx.theme();
            (t.foreground, t.muted_foreground, t.border, t.popover)
        };
        let items = self.collect_notifications(cx);

        let list: Vec<_> = items
            .into_iter()
            .map(|(si, pane, msg)| {
                let name = self.sessions.get(si).map(|s| s.title(cx)).unwrap_or_default();
                div()
                    .id(("notif", pane.entity_id()))
                    .p_2()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|s| s.bg(border))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().size_2().rounded_full().bg(rgb(0x4a9eff)))
                            .child(div().text_sm().text_color(fg).child(name)),
                    )
                    .child(div().text_xs().text_color(muted).child(msg))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _ev, window, cx| {
                            this.goto_notification(si, &pane, window, cx)
                        }),
                    )
            })
            .collect();

        let empty = list.is_empty();
        div()
            .absolute()
            .inset_0()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _w, cx| {
                    this.notifications_open = false;
                    cx.notify();
                }),
            )
            .child(
                div()
                    .absolute()
                    .top(px(40.))
                    .right(px(44.))
                    .w(px(300.))
                    .bg(popover)
                    .border_1()
                    .border_color(border)
                    .rounded_lg()
                    .shadow_lg()
                    .p_2()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(div().px_2().py_1().font_bold().text_color(fg).child("通知"))
                    .children(empty.then(|| {
                        div()
                            .px_2()
                            .py_2()
                            .text_sm()
                            .text_color(muted)
                            .child("没有待处理通知")
                    }))
                    .children(list),
            )
            .into_any_element()
    }

    /// 总览页：所有会话的卡片网格（状态徽章 + 任务名 + cwd + 窗格数），点击跳转。
    fn render_overview(&mut self, cx: &mut Context<Self>) -> Div {
        let (fg, muted) = {
            let t = cx.theme();
            (t.foreground, t.muted_foreground)
        };
        // 果冻感配色：柔和卡片底、细白边、状态色 + 半透明底做胶囊（统一 Hsla）。
        let card_bg = rgb(0x17181d);
        let card_border = rgba(0xffffff12);
        let soft_bg: Hsla = rgba(0xffffff0d).into();
        let c_red: Hsla = rgb(0xef4444).into();
        let c_blue: Hsla = rgb(0x4a9eff).into();
        let c_green: Hsla = rgb(0x22c55e).into();
        let c_amber: Hsla = rgb(0xf59e0b).into();
        let red_tint: Hsla = rgba(0xef444422).into();
        let blue_tint: Hsla = rgba(0x4a9eff22).into();
        let green_tint: Hsla = rgba(0x22c55e22).into();
        let amber_tint: Hsla = rgba(0xf59e0b22).into();
        // 空闲态圆点：低调灰，不与「已完成」的绿抢注意力。
        let c_muted_dot: Hsla = rgba(0x8b93a7aa).into();

        // 顶部汇总：标题 + 胶囊统计。「需要处理」= 等审批 + 一般注意。
        let statuses: Vec<AgentStatus> = self.sessions.iter().map(|s| s.status(cx)).collect();
        let need = statuses
            .iter()
            .filter(|s| matches!(s, AgentStatus::WaitingApproval | AgentStatus::NeedsAttention))
            .count();
        let running = statuses.iter().filter(|s| matches!(s, AgentStatus::Running)).count();
        let done = statuses.iter().filter(|s| matches!(s, AgentStatus::Done)).count();
        let pill = |text: String, color: Hsla, bg: Hsla| {
            div().px(px(11.)).py(px(4.)).rounded_full().bg(bg).text_sm().text_color(color).child(text)
        };
        let summary = div()
            .flex()
            .items_center()
            .gap_2()
            .child(div().text_xl().font_bold().text_color(fg).mr_2().child("总览"))
            .child(pill(format!("{} 会话", self.sessions.len()), fg, soft_bg))
            .child(pill(format!("{need} 需要处理"), c_red, red_tint))
            .child(pill(format!("{running} 运行中"), c_blue, blue_tint))
            .children((done > 0).then(|| pill(format!("{done} 已完成"), c_green, green_tint)));

        // 按状态排序：等审批 > 需要处理 > 运行中 > 刚完成 > 空闲（同级保持原顺序）。
        let mut order: Vec<usize> = (0..self.sessions.len()).collect();
        order.sort_by_key(|&ix| match statuses[ix] {
            AgentStatus::WaitingApproval => 0,
            AgentStatus::NeedsAttention => 1,
            AgentStatus::Running => 2,
            AgentStatus::Done => 3,
            AgentStatus::Idle => 4,
        });

        // 会话卡片。
        let cards: Vec<_> = order
            .into_iter()
            .map(|ix| {
                // 会话卡片「运行了多久 / 当前 token 用量 / 最近工具调用」直接前移复用
                // 历史会话页已有的扫描结果（ensure_session_list 有 10s 缓存，这里
                // 只是触发+读缓存，不会每帧重新扫盘）——取该项目目录下最近活跃的
                // 那份 transcript 当作"当前会话"的口径。
                let cwd_opt = self.sessions[ix].cwd(cx);
                if let Some(c) = cwd_opt.clone() {
                    self.ensure_session_list(c, cx);
                }
                let live = cwd_opt
                    .as_deref()
                    .and_then(|c| self.session_list.get(c))
                    .and_then(|(_, list)| list.first());
                let mut live_parts: Vec<String> = Vec::new();
                if let Some((a, b)) = live.and_then(|s| s.started_at.zip(s.last_active_at)) {
                    let mins = (b - a).num_minutes().max(0);
                    if mins > 0 {
                        live_parts.push(format!("⏱ 跑了 {mins} 分钟"));
                    }
                }
                if let Some(tokens) = live.map(|s| s.total_tokens).filter(|t| *t > 0) {
                    live_parts.push(format!("🔢 {} tokens", format_count(tokens)));
                }
                if let Some(tool) = live.and_then(|s| s.last_tool.clone()) {
                    live_parts.push(format!("🔧 最近 {tool}"));
                }
                let live_line = (!live_parts.is_empty()).then(|| live_parts.join(" · "));

                let s = &self.sessions[ix];
                let name = s.title(cx);
                let cwd = cwd_opt
                    .clone()
                    .unwrap_or_default()
                    .trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .unwrap_or("")
                    .to_string();
                let (dot, label, tint) = match statuses[ix] {
                    AgentStatus::WaitingApproval => (c_red, "等你批准", red_tint),
                    AgentStatus::NeedsAttention => (c_amber, "需要处理", amber_tint),
                    AgentStatus::Running => (c_blue, "运行中", blue_tint),
                    AgentStatus::Done => (c_green, "已完成", green_tint),
                    AgentStatus::Idle => (c_muted_dot, "空闲", soft_bg),
                };
                let panes = s.pane_count();
                let notif = s.notification_msg(cx);
                let when = s.notified_at(cx).map(ago);
                let preview = s.preview(cx, 3);
                let git = cwd_opt.as_ref().and_then(|c| self.git_cache.get(c).cloned());

                div()
                    .id(("ov-card", ix))
                    .w(px(380.))
                    .p_4()
                    .rounded(px(18.))
                    .border_1()
                    .border_color(card_border)
                    .bg(card_bg)
                    .shadow_sm()
                    .cursor_pointer()
                    // hover：边框亮起 + 抬起阴影 + 底色微亮，做出「果冻浮起」感。
                    .hover(|d| d.border_color(dot).shadow_lg().bg(rgb(0x1c1e24)))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _ev, window, cx| this.activate(ix, window, cx)),
                    )
                    // 状态点 + 会话名（任务） + 最近时间
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .min_w_0()
                            .child(div().size(px(9.)).rounded_full().bg(dot).flex_shrink_0())
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .font_semibold()
                                    .text_color(fg)
                                    .child(name),
                            )
                            .children(when.map(|w| {
                                div().text_xs().text_color(muted).flex_shrink_0().child(w)
                            })),
                    )
                    // cwd + 状态 + 窗格数
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_xs()
                            .child(
                                div()
                                    .px(px(8.))
                                    .py(px(2.))
                                    .rounded_full()
                                    .bg(tint)
                                    .text_color(dot)
                                    .child(label),
                            )
                            .child(div().text_color(muted).child(cwd))
                            .child(div().text_color(muted).child(format!("· {panes} 窗格"))),
                    )
                    // git 分支 + 改动数
                    .children(git.map(|(branch, changed)| {
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_xs()
                            .text_color(muted)
                            .child(format!("⎇ {branch}"))
                            .children((changed > 0).then(|| {
                                div().text_color(c_amber).child(format!("● {changed} 改动"))
                            }))
                    }))
                    // 当前会话：跑了多久 / token 用量 / 最近工具调用。卡片加宽后仍装不下
                    // 就换行，不再单行截断吞掉后半截（工具名常常最有用，之前恰恰被切没）。
                    .children(live_line.map(|line| {
                        div().text_xs().text_color(muted).child(line)
                    }))
                    // 需要处理时显示通知消息（红底胶囊，更醒目）
                    .children(notif.map(|m| {
                        div()
                            .px(px(8.))
                            .py(px(4.))
                            .rounded_lg()
                            .bg(rgba(0xef444418))
                            .text_xs()
                            .text_color(c_red)
                            .truncate()
                            .child(m)
                    }))
                    // 迷你终端预览（末尾几行）：改自动换行，装不下的原始终端行（尤其
                    // starship/oh-my-posh 这类花哨 prompt）折成两三行也比单行截断吞了
                    // 大半内容强，卡片高度跟着内容走，不再强行按住一行。
                    .children((!preview.is_empty()).then(|| {
                        div()
                            .p_2()
                            .rounded_lg()
                            .bg(rgb(0x0d0d10))
                            .font_family(terminal_view::FONT_FAMILY)
                            .text_xs()
                            .text_color(muted)
                            .flex()
                            .flex_col()
                            .gap_1()
                            .children(preview.into_iter().map(|line| {
                                div().child(if line.is_empty() {
                                    " ".to_string()
                                } else {
                                    line
                                })
                            }))
                    }))
            })
            .collect();

        div().flex_1().min_h_0().child(
            div()
                .id("overview-scroll")
                .size_full()
                .overflow_y_scroll()
                .p_5()
                .flex()
                .flex_col()
                .gap_5()
                .child(summary)
                .child(div().flex().flex_wrap().gap_4().children(cards)),
        )
    }

    /// 渲染无条件退出确认弹层：磨砂遮罩 + 确认退出/取消按钮。
    fn render_quit_confirm(&self, cx: &mut Context<Self>) -> Div {
        let (fg, muted, border, popover) = {
            let t = cx.theme();
            (t.foreground, t.muted_foreground, t.border, t.popover)
        };
        let c_blue_tint: Hsla = rgba(0x4a9eff24).into();
        let c_blue_hover: Hsla = rgba(0x4a9eff40).into();
        let c_neutral_bg: Hsla = rgba(0xffffff0a).into();
        let c_neutral_hover: Hsla = rgba(0xffffff1f).into();

        div()
            .absolute()
            .inset_0()
            .bg(rgba(0x000000aa))
            .flex()
            .items_center()
            .justify_center()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                v_flex()
                    .w(px(320.))
                    .p_5()
                    .bg(popover)
                    .border_1()
                    .border_color(border)
                    .rounded_lg()
                    .shadow_lg()
                    .gap_4()
                    .child(
                        div()
                            .font_bold()
                            .text_color(fg)
                            .text_lg()
                            .child("确定退出 Smelt 吗？")
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(muted)
                            .child("退出工作台后，后台守护进程仍在运行，但当前活动的终端连接将被断开。")
                    )
                    .child(
                        h_flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                div()
                                    .id("cancel-quit")
                                    .px_3()
                                    .py(px(5.))
                                    .rounded_lg()
                                    .bg(c_neutral_bg)
                                    .border_1()
                                    .border_color(rgba(0xffffff12))
                                    .text_sm()
                                    .text_color(fg)
                                    .cursor_pointer()
                                    .hover(move |s| s.bg(c_neutral_hover))
                                    .child("取消")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.show_quit_confirm = false;
                                        cx.notify();
                                    }))
                            )
                            .child(
                                div()
                                    .id("confirm-quit")
                                    .px_3()
                                    .py(px(5.))
                                    .rounded_lg()
                                    .bg(c_blue_tint)
                                    .border_1()
                                    .border_color(rgba(0xffffff12))
                                    .text_sm()
                                    .text_color(rgb(0x8fc7ff))
                                    .cursor_pointer()
                                    .hover(move |s| s.bg(c_blue_hover))
                                    .child("确定退出")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        // 有暂存好的新版本就在退出前落盘替换；失败静默忽略，
                                        // 不能因为自更新出岔子就把用户堵在退出流程里。
                                        if let updater::UpdateStatus::ReadyToInstall { staged_app, .. } =
                                            &this.update_status
                                        {
                                            let _ = updater::finalize_pending_update(staged_app);
                                        }
                                        cx.quit();
                                    }))
                            )
                    )
            )
    }

    /// 「重启守护进程」二次确认弹窗：明确告知会断开所有当前终端会话。与
    /// render_quit_confirm 同款视觉（居中卡片 + 半透明遮罩）。
    fn render_daemon_restart_confirm(&self, cx: &mut Context<Self>) -> Div {
        let (fg, muted, border, popover) = {
            let t = cx.theme();
            (t.foreground, t.muted_foreground, t.border, t.popover)
        };
        let c_red_tint: Hsla = rgba(0xef444424).into();
        let c_red_hover: Hsla = rgba(0xef444440).into();
        let c_neutral_bg: Hsla = rgba(0xffffff0a).into();
        let c_neutral_hover: Hsla = rgba(0xffffff1f).into();

        div()
            .absolute()
            .inset_0()
            .bg(rgba(0x000000aa))
            .flex()
            .items_center()
            .justify_center()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                v_flex()
                    .w(px(320.))
                    .p_5()
                    .bg(popover)
                    .border_1()
                    .border_color(border)
                    .rounded_lg()
                    .shadow_lg()
                    .gap_4()
                    .child(div().font_bold().text_color(fg).text_lg().child("确定重启守护进程吗？"))
                    .child(
                        div()
                            .text_sm()
                            .text_color(muted)
                            .child("守护进程升级后才会生效新版本。重启会立即断开并终止当前所有终端会话（包括正在跑的 agent），且无法恢复。"),
                    )
                    .child(
                        h_flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                div()
                                    .id("cancel-daemon-restart")
                                    .px_3()
                                    .py(px(5.))
                                    .rounded_lg()
                                    .bg(c_neutral_bg)
                                    .border_1()
                                    .border_color(rgba(0xffffff12))
                                    .text_sm()
                                    .text_color(fg)
                                    .cursor_pointer()
                                    .hover(move |s| s.bg(c_neutral_hover))
                                    .child("取消")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.show_daemon_restart_confirm = false;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                div()
                                    .id("confirm-daemon-restart")
                                    .px_3()
                                    .py(px(5.))
                                    .rounded_lg()
                                    .bg(c_red_tint)
                                    .border_1()
                                    .border_color(rgba(0xffffff12))
                                    .text_sm()
                                    .text_color(rgb(0xff8f8f))
                                    .cursor_pointer()
                                    .hover(move |s| s.bg(c_red_hover))
                                    .child("确定重启")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.confirm_restart_daemon(cx);
                                    })),
                            ),
                    ),
            )
    }

    /// 当前文件有未保存改动、又点了别的文件时弹的确认弹窗：取消 / 不保存直接切换 /
    /// 保存并切换。与 render_quit_confirm 同款视觉（居中卡片 + 半透明遮罩）。
    fn render_unsaved_file_confirm(&self, target: String, cx: &mut Context<Self>) -> Div {
        let (fg, muted, border, popover) = {
            let t = cx.theme();
            (t.foreground, t.muted_foreground, t.border, t.popover)
        };
        let c_blue_tint: Hsla = rgba(0x4a9eff24).into();
        let c_blue_hover: Hsla = rgba(0x4a9eff40).into();
        let c_neutral_bg: Hsla = rgba(0xffffff0a).into();
        let c_neutral_hover: Hsla = rgba(0xffffff1f).into();
        let cur_name = self
            .open_file
            .as_ref()
            .map(|of| of.path.rsplit('/').next().unwrap_or(of.path.as_str()).to_string())
            .unwrap_or_default();
        let target_name = target.rsplit('/').next().unwrap_or(target.as_str()).to_string();

        div()
            .absolute()
            .inset_0()
            .bg(rgba(0x000000aa))
            .flex()
            .items_center()
            .justify_center()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                v_flex()
                    .w(px(360.))
                    .p_5()
                    .bg(popover)
                    .border_1()
                    .border_color(border)
                    .rounded_lg()
                    .shadow_lg()
                    .gap_4()
                    .child(
                        div()
                            .font_bold()
                            .text_color(fg)
                            .text_lg()
                            .child(format!("「{cur_name}」有未保存的改动")),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(muted)
                            .child(format!("要切换到「{target_name}」了，这些改动还没保存，要怎么处理？")),
                    )
                    .child(
                        h_flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                div()
                                    .id("unsaved-cancel")
                                    .px_3()
                                    .py(px(5.))
                                    .rounded_lg()
                                    .bg(c_neutral_bg)
                                    .border_1()
                                    .border_color(rgba(0xffffff12))
                                    .text_sm()
                                    .text_color(fg)
                                    .cursor_pointer()
                                    .hover(move |s| s.bg(c_neutral_hover))
                                    .child("取消")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.pending_file_switch = None;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                div()
                                    .id("unsaved-discard")
                                    .px_3()
                                    .py(px(5.))
                                    .rounded_lg()
                                    .bg(c_neutral_bg)
                                    .border_1()
                                    .border_color(rgba(0xffffff12))
                                    .text_sm()
                                    .text_color(fg)
                                    .cursor_pointer()
                                    .hover(move |s| s.bg(c_neutral_hover))
                                    .child("不保存，直接切换")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        if let Some(target) = this.pending_file_switch.take() {
                                            this.open_file_now(target, window, cx);
                                        }
                                    })),
                            )
                            .child(
                                div()
                                    .id("unsaved-save-switch")
                                    .px_3()
                                    .py(px(5.))
                                    .rounded_lg()
                                    .bg(c_blue_tint)
                                    .border_1()
                                    .border_color(rgba(0xffffff12))
                                    .text_sm()
                                    .text_color(rgb(0x8fc7ff))
                                    .cursor_pointer()
                                    .hover(move |s| s.bg(c_blue_hover))
                                    .child("保存并切换")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if let Some(target) = this.pending_file_switch.take() {
                                            this.pending_switch_after_save = Some(target);
                                            this.save_open_file(cx);
                                        }
                                    })),
                            ),
                    ),
            )
    }

    /// 渲染独立设置页面：铺满主区、居中限宽、支持滚动。
    /// 设置页主体：外观 / 桌面宠物 / 更新三个分组。供嵌入式设置页（主窗口右上角齿轮，
    /// 带「返回」头）和独立设置窗口（原生标题栏，无需「返回」）共用，各自决定外层怎么包。
    fn render_settings_content(&self, cx: &mut Context<Self>) -> Div {
        let (fg, muted, border, popover) = {
            let t = cx.theme();
            (t.foreground, t.muted_foreground, t.border, t.popover)
        };
        let entity = cx.entity();

        // 统一的小按钮：固定高度 + flex_none，避免被 flex 布局拉伸成大块。
        // move 闭包：捕获的四个颜色都是 Copy，闭包本身因此也是 Copy，可以放心
        // 塞进下面多个 SettingField::render 的 move 闭包里各用一份。
        let btn = move |id: &'static str, label: String| {
            div()
                .id(id)
                .h(px(26.))
                .px_3()
                .flex()
                .flex_none()
                .items_center()
                .rounded_md()
                .cursor_pointer()
                .text_xs()
                .text_color(fg)
                .bg(popover)
                .border_1()
                .border_color(border)
                .hover(|s| s.bg(border))
                .child(label)
        };

        const PET_SIZES: [f32; 3] = [0.8, 1.0, 1.25];

        // —— 外观 ——
        let bg_color_picker = self.bg_color_picker.clone();
        let opacity_slider = self.opacity_slider.clone();
        let pick_entity = entity.clone();
        let clear_entity = entity.clone();
        let appearance_page = SettingPage::new("外观").default_open(true).group(
            SettingGroup::new().items(vec![
                SettingItem::new(
                    "主题模式",
                    SettingField::switch(
                        |cx: &App| cx.global::<Appearance>().theme_mode.is_dark(),
                        |v: bool, cx: &mut App| {
                            let mode = if v { ThemeMode::Dark } else { ThemeMode::Light };
                            apply_appearance(|a| a.theme_mode = mode, cx);
                            Theme::change(mode, None, cx);
                            cx.refresh_windows();
                        },
                    )
                    .default_value(true),
                )
                .description("开启为深色主题，关闭为浅色主题"),
                SettingItem::new(
                    "背景色",
                    SettingField::render(move |_, _, _| {
                        div().children(bg_color_picker.as_ref().map(|p| ColorPicker::new(p).small()))
                    }),
                ),
                SettingItem::new(
                    "背景图片",
                    SettingField::render(move |_, _, cx: &mut App| {
                        let img_name = cx
                            .global::<Appearance>()
                            .bg_image
                            .as_deref()
                            .and_then(|p| p.rsplit('/').next())
                            .unwrap_or("无")
                            .to_string();
                        let pick_entity = pick_entity.clone();
                        let clear_entity = clear_entity.clone();
                        h_flex()
                            .items_center()
                            .gap_2()
                            .child(div().text_xs().text_color(muted).child(img_name))
                            .child(btn("pick-img", "选择图片…".into()).on_mouse_down(
                                MouseButton::Left,
                                move |_, _window, cx: &mut App| {
                                    pick_entity.update(cx, |this, cx| this.pick_bg_image(cx));
                                },
                            ))
                            .child(
                                btn("clear-img", "清除".into()).text_color(muted).on_mouse_down(
                                    MouseButton::Left,
                                    move |_, _window, cx: &mut App| {
                                        clear_entity.update(cx, |this, cx| this.set_bg_image(None, cx));
                                    },
                                ),
                            )
                    }),
                ),
                SettingItem::new(
                    "不透明度",
                    SettingField::render(move |_, _, _| {
                        div().children(opacity_slider.as_ref().map(Slider::new))
                    }),
                ),
                SettingItem::new(
                    "背景模糊",
                    SettingField::switch(
                        |cx: &App| cx.global::<Appearance>().blur,
                        |v: bool, cx: &mut App| {
                            apply_appearance(|a| a.blur = v, cx);
                            cx.refresh_windows();
                        },
                    )
                    .default_value(false),
                ),
            ]),
        );

        // —— 桌面宠物 ——
        let pet_color_picker = self.pet_color_picker.clone();
        let llm_inputs = self.llm_inputs.clone();
        let pet_page = SettingPage::new("桌面宠物").group(
            SettingGroup::new().items(vec![
                SettingItem::new(
                    "显示宠物",
                    SettingField::switch(
                        |cx: &App| cx.global::<pet::PetConfig>().enabled,
                        |v: bool, cx: &mut App| apply_pet_config(|c| c.enabled = v, cx),
                    ),
                ),
                SettingItem::new(
                    "状态播报",
                    SettingField::switch(
                        |cx: &App| cx.global::<pet::PetConfig>().notify,
                        |v: bool, cx: &mut App| apply_pet_config(|c| c.notify = v, cx),
                    ),
                ),
                SettingItem::new(
                    "宠物大脑（LLM）",
                    SettingField::switch(
                        |cx: &App| cx.global::<agent::LlmConfig>().enabled,
                        |v: bool, cx: &mut App| apply_llm_config(|c| c.enabled = v, cx),
                    ),
                )
                .description("接入 OpenAI 兼容接口，点击或通知宠物时将调用 LLM 主动说话。"),
                SettingItem::render(move |_, _, _| {
                    let field = |label: &str, state: &Entity<gpui_component::input::InputState>| {
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(div().text_xs().text_color(muted).child(label.to_string()))
                            .child(Input::new(state).small())
                    };
                    div()
                        .w_full()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .children(llm_inputs.as_ref().map(|inp| {
                            div()
                                .flex()
                                .flex_col()
                                .gap_3()
                                .child(field("接口地址 base_url", &inp.base_url))
                                .child(field("API Key", &inp.api_key))
                                .child(field("模型 model", &inp.model))
                                .child(field("人设 persona", &inp.persona))
                        }))
                }),
                SettingItem::new(
                    "颜色",
                    SettingField::render(move |_, _, _| {
                        div().children(pet_color_picker.as_ref().map(|p| ColorPicker::new(p).small()))
                    }),
                ),
                SettingItem::new(
                    "大小",
                    SettingField::render(move |_, _, cx: &mut App| {
                        let scale = cx.global::<pet::PetConfig>().scale;
                        let size_ix = PET_SIZES.iter().position(|v| (scale - v).abs() < 0.01);
                        RadioGroup::horizontal("pet-size")
                            .selected_index(size_ix)
                            .on_click(|ix: &usize, _window, cx: &mut App| {
                                let val = PET_SIZES[*ix];
                                apply_pet_config(|c| c.scale = val, cx);
                            })
                            .children([
                                Radio::new("sz-s").label("小"),
                                Radio::new("sz-m").label("中"),
                                Radio::new("sz-l").label("大"),
                            ])
                    }),
                ),
            ]),
        );

        // —— 启动：项目「+」快捷启动 Claude Code 时用的参数 ——
        let launch_page = SettingPage::new("启动").group(
            SettingGroup::new().item(
                SettingItem::new(
                    "Claude Code 全权限启动",
                    SettingField::switch(
                        |cx: &App| cx.global::<LaunchConfig>().claude_full_permissions,
                        |v: bool, cx: &mut App| {
                            apply_launch_config(|c| c.claude_full_permissions = v, cx)
                        },
                    ),
                )
                .description(
                    "开启后项目行「+」的 Claude Code 快捷入口用 \
                     claude --dangerously-skip-permissions 启动，跳过所有权限确认；\
                     关闭则正常走 claude。",
                ),
            ),
        );

        // —— 更新：检查/下载全自动静默，生效推迟到退出时 ——
        let update_entity = entity.clone();
        let daemon_entity = entity.clone();
        let update_page = SettingPage::new("更新").resettable(false).group(
            SettingGroup::new()
                .item(SettingItem::render(move |_, _, cx: &mut App| {
                let status = update_entity.read(cx).update_status.clone();
                let status_text = match &status {
                    updater::UpdateStatus::Idle => String::new(),
                    updater::UpdateStatus::Checking => "检查中…".to_string(),
                    updater::UpdateStatus::UpToDate => "已是最新版本".to_string(),
                    updater::UpdateStatus::Downloading { version } => {
                        format!("正在下载 v{version}…")
                    }
                    updater::UpdateStatus::ReadyToInstall { version, .. } => {
                        format!("新版本 v{version} 已就绪，下次启动生效")
                    }
                    updater::UpdateStatus::Failed(e) => format!("检查失败：{e}"),
                };
                let busy = matches!(
                    status,
                    updater::UpdateStatus::Checking | updater::UpdateStatus::Downloading { .. }
                );
                let ready = matches!(status, updater::UpdateStatus::ReadyToInstall { .. });

                let check_entity = update_entity.clone();
                let check_btn = btn("check-update", if busy { "检查中…".into() } else { "检查更新".into() })
                    .text_color(if busy { muted } else { fg })
                    .on_mouse_down(MouseButton::Left, move |_, _window, cx: &mut App| {
                        check_entity.update(cx, |this, cx| {
                            if !matches!(
                                this.update_status,
                                updater::UpdateStatus::Checking | updater::UpdateStatus::Downloading { .. }
                            ) {
                                this.check_for_update(false, cx);
                            }
                        });
                    });
                let restart_btn = ready.then(|| {
                    btn("restart-update", "立即重启更新".into())
                        .text_color(rgb(0x8fc7ff))
                        .bg(Hsla::from(rgba(0x4a9eff24)))
                        .hover(|s| s.bg(Hsla::from(rgba(0x4a9eff40))))
                        .on_mouse_down(MouseButton::Left, move |_, _window, cx: &mut App| {
                            if let updater::UpdateStatus::ReadyToInstall { staged_app, .. } = &status {
                                if updater::finalize_pending_update(staged_app).is_ok() {
                                    cx.quit();
                                }
                            }
                        })
                });

                v_flex()
                    .w_full()
                    .gap_3()
                    .child(
                        h_flex()
                            .w_full()
                            .justify_between()
                            .items_center()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(fg)
                                            .child(concat!("当前版本 v", env!("CARGO_PKG_VERSION"))),
                                    )
                                    .child(
                                        div()
                                            .id("settings-github-link")
                                            .text_xs()
                                            .cursor_pointer()
                                            .text_color(muted)
                                            .hover(|s| s.text_color(fg))
                                            .child("GitHub ↗")
                                            .on_mouse_down(MouseButton::Left, |_, _window, cx| {
                                                cx.open_url("https://github.com/zzfn/smelt");
                                            }),
                                    ),
                            )
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .child(check_btn)
                                    .children(restart_btn),
                            ),
                    )
                    .children((!status_text.is_empty()).then(|| {
                        div().text_xs().text_color(muted).child(status_text)
                    }))
            }))
                .item(SettingItem::render(move |_, _, cx: &mut App| {
                    let outdated = daemon_entity.read(cx).daemon_outdated;
                    let restart_entity = daemon_entity.clone();
                    let restart_daemon_btn = (outdated == Some(true)).then(|| {
                        btn("restart-daemon", "重启守护进程".into())
                            .text_color(rgb(0xff8f8f))
                            .bg(Hsla::from(rgba(0xef444424)))
                            .hover(|s| s.bg(Hsla::from(rgba(0xef444440))))
                            .on_mouse_down(MouseButton::Left, move |_, _window, cx: &mut App| {
                                restart_entity.update(cx, |this, cx| {
                                    this.show_daemon_restart_confirm = true;
                                    cx.notify();
                                });
                            })
                    });
                    let status_text = match outdated {
                        Some(true) => "版本落后于当前安装包，新功能/修复需手动重启守护才生效。".to_string(),
                        Some(false) => "已是最新，无需重启。".to_string(),
                        None => "检测中…".to_string(),
                    };

                    v_flex()
                        .w_full()
                        .gap_3()
                        .child(
                            h_flex()
                                .w_full()
                                .justify_between()
                                .items_center()
                                .child(div().text_sm().text_color(fg).child("守护进程（smeltd）"))
                                .children(restart_daemon_btn),
                        )
                        .child(div().text_xs().text_color(muted).child(status_text))
                        .children((outdated == Some(true)).then(|| {
                            div()
                                .text_xs()
                                .text_color(rgb(0xff8f8f))
                                .child("重启会立即断开并终止当前所有终端会话（含正在跑的 agent），无法恢复。")
                        }))
                })),
        );

        div()
            .size_full()
            .child(Settings::new("settings").pages(vec![appearance_page, pet_page, launch_page, update_page]))
    }

    /// 打开独立设置窗口：已经开着就聚焦提到前台，不重复开第二扇。窗口只是个薄壳
    /// （[`SettingsWindow`]），真正的状态（颜色选择器/LLM 输入框等）还挂在这个
    /// Workspace 实体上没挪窝，薄壳每次渲染都转手调回来，天然跟主窗口保持同步。
    ///
    /// 必须用 `cx.defer` 推迟到当前这轮 `Workspace::update` 彻底返回之后再开窗：
    /// 这里被点齿轮的 `cx.listener` 调用时，`Workspace` 这个 entity 正被 update
    /// 占着；若同步 `cx.open_window`，新窗口首帧 `SettingsWindow::render` 里会
    /// 马上又对同一个 `Workspace` entity 调 `update`，两层嵌套 update 撞上 GPUI
    /// 的重入保护直接 panic 崩溃（"cannot update ... while it is already being
    /// updated"）——这就是「点齿轮整个 app 崩溃」的真正原因。
    fn open_settings_window(&self, cx: &mut Context<Self>) {
        let workspace = cx.entity();
        cx.defer(move |cx| {
            if let Some(handle) = cx.try_global::<SettingsWindowHandle>().and_then(|h| h.0) {
                if handle.update(cx, |_, window, _| window.activate_window()).is_ok() {
                    return;
                }
            }
            let bounds = WindowBounds::centered(size(px(640.), px(560.)), cx);
            let options = WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some("设置".into()),
                    ..Default::default()
                }),
                window_bounds: Some(bounds),
                ..Default::default()
            };
            let handle = cx
                .open_window(options, |window, cx| {
                    window.set_rem_size(px(18.));
                    let view = cx.new(|_cx| SettingsWindow { workspace: workspace.clone() });
                    cx.new(|cx| Root::new(view, window, cx))
                })
                .expect("打开设置窗口失败");
            cx.set_global(SettingsWindowHandle(Some(handle)));
        });
    }

    /// 用量页内容：后台刷新扫描结果 + 取当前项目/全局数据拼视图。原先挂在
    /// `MainView::Usage` 分支里，拆成独立窗口（[`UsageWindow`]）后单独抽出来，
    /// 主窗口 render 循环里那个「用量页才刷新扫描」的判断也一并挪到这——独立窗口
    /// 自己每次渲染都会调，不需要再靠 self.view 门控。
    fn render_usage_page(&mut self, cx: &mut Context<Self>) -> Div {
        self.ensure_usage_data(cx);
        let cur_project = self.cur().and_then(|s| s.cwd(cx));
        let data = self.usage_cache.as_ref().map(|(_, d)| d.clone());
        // usage_view 内部用 flex_1/min_h_0 撑满，依赖外层是个 flex 容器（原先挂在
        // 主窗口的 flex_col 主区里），独立窗口里補上这层容器它才能正确撑满。
        div().size_full().flex().flex_col().child(usage_view(cur_project, data, cx))
    }

    /// 打开独立用量窗口：已经开着就聚焦提到前台，不重复开第二扇。跟
    /// [`open_settings_window`] 同一套薄壳模式——真正状态（usage_cache 等）
    /// 还挂在这个 Workspace 实体上，薄壳每次渲染转手调回来。
    fn open_usage_window(&self, cx: &mut Context<Self>) {
        let workspace = cx.entity();
        cx.defer(move |cx| {
            if let Some(handle) = cx.try_global::<UsageWindowHandle>().and_then(|h| h.0) {
                if handle.update(cx, |_, window, _| window.activate_window()).is_ok() {
                    return;
                }
            }
            let bounds = WindowBounds::centered(size(px(900.), px(700.)), cx);
            let options = WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some("用量".into()),
                    ..Default::default()
                }),
                window_bounds: Some(bounds),
                ..Default::default()
            };
            let handle = cx
                .open_window(options, |window, cx| {
                    window.set_rem_size(px(18.));
                    let view = cx.new(|_cx| UsageWindow { workspace: workspace.clone() });
                    cx.new(|cx| Root::new(view, window, cx))
                })
                .expect("打开用量窗口失败");
            cx.set_global(UsageWindowHandle(Some(handle)));
        });
    }

    /// 文件树：展开/收起一个文件夹。
    fn toggle_expand(&mut self, path: String, cx: &mut Context<Self>) {
        if !self.expanded.remove(&path) {
            self.expanded.insert(path);
        }
        cx.notify();
    }

    /// 文件树：打开一个文件查看/编辑内容。当前文件有未保存改动时不直接切换——先弹
    /// 确认弹窗（见 pending_file_switch / render_unsaved_file_confirm），用户选了
    /// "不保存"或"保存并切换"才真正调用 open_file_now。
    fn view_file(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        let dirty = self
            .open_file
            .as_ref()
            .is_some_and(|of| of.editor.read(cx).value().to_string() != *of.saved_content);
        if dirty {
            self.pending_file_switch = Some(path);
            cx.notify();
            return;
        }
        self.open_file_now(path, window, cx);
    }

    /// 实际打开文件：用 gpui-component 的 Editor（InputState 的 code_editor 模式）：
    /// tree-sitter 语法高亮 + 行号 + 搜索，直接可编辑，Cmd+S（见 save_open_file）能
    /// 存回磁盘。读文件本身放到后台线程跑（大文件不卡 UI），读完回主线程灌进编辑器；
    /// 用自增 file_gen 丢弃过期结果（期间又切了别的文件）。
    fn open_file_now(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        use gpui_component::input::InputState;

        self.file_gen = self.file_gen.wrapping_add(1);
        let gen = self.file_gen;

        let language = editor_language_for_path(&path);
        let editor = cx.new(|cx| {
            InputState::new(window, cx)
                .code_editor(language)
                .line_number(true)
                .searchable(true)
                // 超长行横向滚动而不是自动换行——代码这种东西换行会破坏缩进对齐，
                // 多行输入默认开软换行，这里显式关掉。
                .soft_wrap(false)
        });
        self.open_file = Some(OpenFile {
            path: path.clone(),
            editor: editor.clone(),
            saved_content: Rc::new(String::new()),
            save_error: None,
            readable: false, // 读完确认是文本才翻真，防止读取完成前误按 Cmd+S
            conflict_pending: false,
        });
        cx.notify();

        cx.spawn(async move |this, cx| {
            let p = path.clone();
            let read = cx.background_executor().spawn(async move { std::fs::read_to_string(&p) }).await;
            let _ = this.update_in(cx, |this, window, cx| {
                // 只有当前仍是这次打开的文件才写入，避免旧任务覆盖新文件。
                if this.file_gen != gen {
                    return;
                }
                let Some(of) = this.open_file.as_mut() else { return };
                match read {
                    Ok(content) => {
                        editor.update(cx, |state, cx| {
                            state.set_value(content.clone(), window, cx);
                        });
                        of.saved_content = Rc::new(content);
                        of.readable = true;
                    }
                    Err(_) => {
                        editor.update(cx, |state, cx| {
                            state.set_value(
                                "（无法以文本方式读取：可能是二进制文件）",
                                window,
                                cx,
                            );
                        });
                        of.readable = false;
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Cmd+S：把当前打开文件的编辑器内容写回磁盘（仅 Files 页触发，见 on_key_down）。
    /// 写之前先读一次磁盘现状跟 saved_content 比对——不一样说明文件被外部改过，
    /// 这次先不写、把 conflict_pending 置位提示用户；用户再按一次 Cmd+S 就当作
    /// 已确认覆盖，跳过这次检查直接写。写文件本身放后台线程；成功后把
    /// saved_content 同步成刚写的内容（清掉"未保存"标记 + 错误提示），并且如果这
    /// 次保存是「保存并切换」触发的，顺带打开 pending_switch_after_save 里存的目标
    /// 文件；保存失败或起冲突则放弃这次切换，留在当前文件上让用户处理。
    fn save_open_file(&mut self, cx: &mut Context<Self>) {
        let Some(of) = &self.open_file else { return };
        if !of.readable {
            if let Some(of) = self.open_file.as_mut() {
                of.save_error = Some("此文件未能正常读取为文本，不支持保存".to_string());
            }
            self.pending_switch_after_save = None;
            cx.notify();
            return;
        }
        let path = of.path.clone();
        let content = of.editor.read(cx).value().to_string();
        // Rc<String> 不是 Send，进不了 background_executor；克隆成普通 String 再带过去。
        let expected_on_disk = (*of.saved_content).clone();
        let force = of.conflict_pending;
        let gen = self.file_gen;

        cx.spawn(async move |this, cx| {
            let check_path = path.clone();
            let write_content = content.clone();
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    if !force {
                        if let Ok(current) = std::fs::read_to_string(&check_path) {
                            if current != expected_on_disk {
                                return SaveOutcome::Conflict;
                            }
                        }
                    }
                    match std::fs::write(&check_path, write_content) {
                        Ok(()) => SaveOutcome::Saved,
                        Err(e) => SaveOutcome::Error(e.to_string()),
                    }
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.file_gen != gen {
                    return; // 写盘期间又切了别的文件，这次结果不再相关
                }
                let switch_target = this.pending_switch_after_save.take();
                let Some(of) = this.open_file.as_mut() else { return };
                match outcome {
                    SaveOutcome::Saved => {
                        of.saved_content = Rc::new(content);
                        of.save_error = None;
                        of.conflict_pending = false;
                        if let Some(target) = switch_target {
                            this.open_file_now(target, window, cx);
                        }
                    }
                    SaveOutcome::Conflict => {
                        of.conflict_pending = true;
                        of.save_error =
                            Some("文件已被外部修改；再按一次 Cmd+S 会强制覆盖磁盘上的改动".to_string());
                    }
                    SaveOutcome::Error(e) => of.save_error = Some(format!("保存失败：{e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 历史会话页：确保当前项目的会话列表缓存新鲜（>10s 或缺失就后台重新扫描）。
    fn ensure_session_list(&mut self, cwd: String, cx: &mut Context<Self>) {
        let fresh = self
            .session_list
            .get(&cwd)
            .is_some_and(|(t, _)| t.elapsed() < std::time::Duration::from_secs(10));
        if fresh || self.session_list_inflight.contains(&cwd) {
            return;
        }
        self.session_list_inflight.insert(cwd.clone());
        cx.spawn(async move |this, cx| {
            let c = cwd.clone();
            let sessions =
                cx.background_executor().spawn(async move { session_history::list_sessions(&c) }).await;
            let _ = this.update(cx, |this, cx| {
                this.session_list_inflight.remove(&cwd);
                this.session_list.insert(cwd, (Instant::now(), Rc::new(sessions)));
                cx.notify();
            });
        })
        .detach();
    }

    /// 历史会话页：点开一份会话，后台解析成 Turn 列表。用自增 gen 丢弃过期结果
    /// （解析期间又点了别的会话）。
    fn open_session_detail(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.session_detail_gen = self.session_detail_gen.wrapping_add(1);
        let gen = self.session_detail_gen;
        self.session_detail = None;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let p = path.clone();
            let detail =
                cx.background_executor().spawn(async move { session_history::load_session_detail(&p) }).await;
            let _ = this.update(cx, |this, cx| {
                if this.session_detail_gen != gen {
                    return;
                }
                if let Some(detail) = detail {
                    this.session_detail = Some((path, Rc::new(detail)));
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 文件树搜索：按 query 匹配文件名 + 文件内容，后台遍历项目、命中写回 search_results。
    /// 与 view_file 同款「background_executor + 自增 gen 丢弃过期结果」模式，绝不阻塞 render。
    /// query 未变（已有对应结果或正在跑同一 query）就跳过，避免每帧重扫。
    fn ensure_search(&mut self, root: String, query: String, cx: &mut Context<Self>) {
        // 已有本 query 的结果、或正有一次针对本 query 的遍历在跑，就不重复触发。
        if self.search_results.as_ref().is_some_and(|s| s.query == query) {
            return;
        }
        self.search_gen = self.search_gen.wrapping_add(1);
        let gen = self.search_gen;
        // 先占位：done=false 让列表顶部显示「搜索中…」，遍历完成后替换。
        self.search_results = Some(SearchState {
            query: query.clone(),
            done: false,
            hits: Vec::new(),
            truncated: false,
        });
        cx.notify();

        cx.spawn(async move |this, cx| {
            let (r, q) = (root.clone(), query.clone());
            let (hits, truncated) = cx
                .background_executor()
                .spawn(async move { search_project(&r, &q) })
                .await;
            let _ = this.update(cx, |this, cx| {
                // 只有仍是最新一次搜索才写入，丢弃期间被新查询取代的过期结果。
                if this.search_gen == gen {
                    this.search_results = Some(SearchState {
                        query,
                        done: true,
                        hits,
                        truncated,
                    });
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 给某个 root 建一次性的文件监听（notify crate，macOS 走 FSEvents）：仓库目录树
    /// 里任何东西变了就在 git_dirty 里标脏，配合下面 250ms 一次的检查循环，git 页
    /// 基本能做到「文件一变就刷新」而不是干等 1.5s 轮询窗口。
    ///
    /// 递归监听整棵目录树（含 .git/ 内部），没有按 .gitignore 过滤噪音——多余的唤醒
    /// 顶多让 ensure_git_status 多跑一次（已用 GIT_OPTIONAL_LOCKS=0 保证很轻），
    /// 250ms 的检查节流已经把最坏情况锁定在每秒最多 4 次，不会失控。
    /// 只在这个 root 第一次进 Git 页时建一次；watcher 存进 git_watchers 常驻到应用退出
    /// （必须持有 watcher 不被 drop，否则会停止收事件）。
    fn ensure_git_watch(&mut self, root: String, cx: &mut Context<Self>) {
        if self.git_watchers.contains_key(&root) {
            return;
        }
        let dirty = self.git_dirty.clone();
        let root_for_cb = root.clone();
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if res.is_ok() {
                if let Ok(mut set) = dirty.lock() {
                    set.insert(root_for_cb.clone());
                }
            }
        });
        let Ok(mut watcher) = watcher else { return };
        if watcher.watch(Path::new(&root), RecursiveMode::Recursive).is_err() {
            return;
        }
        self.git_watchers.insert(root.clone(), watcher);

        let dirty = self.git_dirty.clone();
        cx.spawn(async move |this, cx| loop {
            smol::Timer::after(std::time::Duration::from_millis(250)).await;
            let hit = dirty.lock().is_ok_and(|mut set| set.remove(&root));
            if !hit {
                continue;
            }
            // 只标脏 + 唤醒重绘：真正的重新拉取仍交给 ensure_git_status（render 里
            // 每帧都会调），这里不用重复实现一遍 git status 调用。
            let r = this.update(cx, |this, cx| {
                this.git_status.remove(&root);
                cx.notify();
            });
            if r.is_err() {
                break; // Workspace 已销毁
            }
        })
        .detach();
    }

    /// Git 视图：查看某个改动文件的 diff。已跟踪文件用 `git diff HEAD`，
    /// 未跟踪文件（??）用 `git diff --no-index` 展示全文（整体当作新增）。
    /// 确保某 root 的 git status 缓存新鲜（>1.5s 或缺失就后台刷新；ensure_git_watch
    /// 建的监听命中时会主动清缓存，比 1.5s 轮询更快触发这里重新拉取）。
    /// 绝不阻塞 render：git status 在大仓要 ~90ms，同步跑就是掉帧元凶。
    fn ensure_git_status(&mut self, root: String, cx: &mut Context<Self>) {
        let fresh = self
            .git_status
            .get(&root)
            .is_some_and(|(t, _)| t.elapsed() < std::time::Duration::from_millis(1500));
        if fresh || self.git_status_inflight.contains(&root) {
            return;
        }
        self.git_status_inflight.insert(root.clone());
        cx.spawn(async move |this, cx| {
            let r = root.clone();
            let data = cx
                .background_executor()
                .spawn(async move {
                    let out = std::process::Command::new("git")
                        .args(["-C", &r, "status", "--porcelain=v1", "-b"])
                        // 避免刷新索引 stat 缓存去抢 .git/index.lock——之前吃过这个亏
                        // （见 smeltd/GUI 并发跑 git 命令时的 index.lock 争用问题）；
                        // 顺带也防止我们自己的 status 调用触发上面那个文件监听自扰。
                        .env("GIT_OPTIONAL_LOCKS", "0")
                        .output();
                    let mut d = GitStatusData::default();
                    if let Ok(o) = out {
                        if o.status.success() {
                            d.ok = true;
                            let text = String::from_utf8_lossy(&o.stdout);
                            for line in text.lines() {
                                if let Some(b) = line.strip_prefix("## ") {
                                    d.branch =
                                        b.split("...").next().unwrap_or("").trim().to_string();
                                } else if line.len() >= 3 {
                                    d.files.push((line[..2].to_string(), line[3..].to_string()));
                                }
                            }
                        }
                    }
                    d
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.git_status_inflight.remove(&root);
                this.git_status.insert(root, (Instant::now(), data));
                cx.notify();
            });
        })
        .detach();
    }

    /// 确保某 root 的热力图数据缓存新鲜（>20s 或缺失就后台刷新）。`git log --since=90.days`
    /// 比 `git status` 慢得多，缓存窗口相应拉长，避免切换到热力图页就反复重算。
    fn ensure_hotspot(&mut self, root: String, cx: &mut Context<Self>) {
        let fresh = self
            .hotspot_data
            .get(&root)
            .is_some_and(|(t, _)| t.elapsed() < std::time::Duration::from_secs(20));
        if fresh || self.hotspot_inflight.contains(&root) {
            return;
        }
        self.hotspot_inflight.insert(root.clone());
        cx.spawn(async move |this, cx| {
            let r = root.clone();
            let entries = cx
                .background_executor()
                .spawn(async move { hotspot::compute(&r) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.hotspot_inflight.remove(&root);
                this.hotspot_data.insert(root, (Instant::now(), Rc::new(entries)));
                cx.notify();
            });
        })
        .detach();
    }

    /// 确保用量数据缓存新鲜（>30s 或缺失就后台重新扫描全部本地 transcript）。
    fn ensure_usage_data(&mut self, cx: &mut Context<Self>) {
        let fresh = self
            .usage_cache
            .as_ref()
            .is_some_and(|(t, _)| t.elapsed() < std::time::Duration::from_secs(30));
        if fresh || self.usage_inflight {
            return;
        }
        self.usage_inflight = true;
        cx.spawn(async move |this, cx| {
            let data = cx.background_executor().spawn(async move { usage_stats::scan() }).await;
            let _ = this.update(cx, |this, cx| {
                this.usage_inflight = false;
                this.usage_cache = Some((Instant::now(), Rc::new(data)));
                cx.notify();
            });
        })
        .detach();
    }

    /// 确保某目录的直接子项列表缓存新鲜（>2s 或缺失就后台刷新）。
    /// 绝不阻塞 render：此前 file_tree 在 render 里同步 fs::read_dir，大目录会
    /// 像 git status 那样掉帧，这里挪到后台执行器 + 缓存，render 只读。
    fn ensure_dir_listing(&mut self, dir: String, cx: &mut Context<Self>) {
        let fresh = self
            .dir_cache
            .get(&dir)
            .is_some_and(|(t, _)| t.elapsed() < std::time::Duration::from_millis(2000));
        if fresh || self.dir_inflight.contains(&dir) {
            return;
        }
        self.dir_inflight.insert(dir.clone());
        cx.spawn(async move |this, cx| {
            let d = dir.clone();
            let entries = cx
                .background_executor()
                .spawn(async move {
                    let mut items: Vec<std::fs::DirEntry> = match std::fs::read_dir(&d) {
                        Ok(rd) => rd.flatten().collect(),
                        Err(_) => return Vec::new(),
                    };
                    items.sort_by_key(|e| {
                        (
                            !e.path().is_dir(),
                            e.file_name().to_string_lossy().to_lowercase(),
                        )
                    });
                    items
                        .into_iter()
                        .filter_map(|e| {
                            let name = e.file_name().to_string_lossy().to_string();
                            if matches!(name.as_str(), ".git" | "node_modules" | "target" | ".DS_Store")
                            {
                                return None;
                            }
                            Some((name, e.path().is_dir()))
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.dir_inflight.remove(&dir);
                this.dir_cache.insert(dir, (Instant::now(), Rc::new(entries)));
                cx.notify();
            });
        })
        .detach();
    }

    /// 跑 git + 着色放后台，用 file_gen 丢弃过期结果。
    fn open_diff(&mut self, root: String, path: String, untracked: bool, cx: &mut Context<Self>) {
        self.diff_gen = self.diff_gen.wrapping_add(1);
        let gen = self.diff_gen;
        self.git_diff = Some(GitDiff { path: path.clone(), lines: Rc::new(Vec::new()) });
        self.diff_selected.clear(); // 换文件/重开 diff：旧的行选区不再对应新内容
        cx.notify();

        cx.spawn(async move |this, cx| {
            let (r, p) = (root.clone(), path.clone());
            let lines = cx
                .background_executor()
                .spawn(async move {
                    let out = if untracked {
                        std::process::Command::new("git")
                            .args(["-C", &r, "diff", "--no-index", "--", "/dev/null", &p])
                            .output()
                    } else {
                        std::process::Command::new("git")
                            .args(["-C", &r, "diff", "HEAD", "--", &p])
                            .output()
                    };
                    // --no-index 有差异时退出码为 1，所以不看 status，只要拿到 stdout。
                    let text = match out {
                        Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(),
                        Err(e) => format!("无法执行 git diff：{e}"),
                    };
                    parse_diff(&text)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.diff_gen == gen {
                    this.git_diff = Some(GitDiff { path, lines: Rc::new(lines) });
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 点击 diff 行：切换该行（按 GitDiff.lines 下标）是否被选中待评论。
    fn toggle_diff_line(&mut self, i: usize, cx: &mut Context<Self>) {
        if !self.diff_selected.remove(&i) {
            self.diff_selected.insert(i);
        }
        cx.notify();
    }

    /// 把选中的 diff 行 + 评论输入框内容拼成一段文本，写进当前激活终端的 PTY
    /// （不带回车，留给用户自己看一眼再发送）。
    fn send_diff_comments(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.diff_selected.is_empty() {
            return;
        }
        let Some(diff) = &self.git_diff else { return };
        let comment = self
            .diff_comment_input
            .as_ref()
            .map(|s| s.read(cx).value().trim().to_string())
            .unwrap_or_default();

        let mut selected: Vec<usize> = self.diff_selected.iter().copied().collect();
        selected.sort_unstable();
        let mut msg = format!("对 {} 的这几行有反馈：\n", diff.path);
        for i in selected {
            if let Some(l) = diff.lines.get(i) {
                let ln = l.new_ln.or(l.old_ln).map(|n| n.to_string()).unwrap_or_else(|| "?".into());
                let marker = match l.kind {
                    DiffKind::Add => "+",
                    DiffKind::Del => "-",
                    _ => " ",
                };
                msg.push_str(&format!("  L{ln} {marker} {}\n", l.text));
            }
        }
        if !comment.is_empty() {
            msg.push_str(&format!("\n{comment}\n"));
        }

        let target = self.cur().map(|s| s.active.clone());
        if let Some(view) = target {
            view.update(cx, |tv, cx| tv.send_text(&msg, cx));
        }
        self.diff_selected.clear();
        if let Some(state) = self.diff_comment_input.clone() {
            state.update(cx, |s, cx| s.set_value("", window, cx));
        }
        cx.notify();
    }

    fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let all: Vec<(SharedString, Cmd)> = self
            .all_commands(cx)
            .into_iter()
            .map(|(label, cmd)| (label.into(), cmd))
            .collect();
        let state = cx.new(|cx| ListState::new(CmdDelegate::new(all), window, cx).searchable(true));
        // 确认（回车/点击）执行命令；取消（Esc）关闭面板。
        self._palette_sub = Some(cx.subscribe_in(
            &state,
            window,
            |this, state, ev: &ListEvent, window, cx| match ev {
                ListEvent::Confirm(ix) => {
                    let cmd = state.read(cx).delegate().matched.get(ix.row).map(|(_, c)| c.clone());
                    if let Some(cmd) = cmd {
                        this.exec_cmd(cmd, window, cx);
                    }
                }
                ListEvent::Cancel => this.close_palette(window, cx),
                _ => {}
            },
        ));
        state.update(cx, |s, cx| s.focus(window, cx));
        self.palette = Some(state);
        cx.notify();
    }

    fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette = None;
        self._palette_sub = None;
        self.focus_active(window, cx);
        cx.notify();
    }

    /// 全部命令（含逐会话切换）。
    fn all_commands(&self, cx: &App) -> Vec<(String, Cmd)> {
        let mut v = vec![
            ("新建会话".to_string(), Cmd::NewTab),
            ("打开项目…".to_string(), Cmd::OpenProject),
            ("关闭当前会话/窗格".to_string(), Cmd::CloseTab),
            ("下一个会话".to_string(), Cmd::NextTab),
            ("上一个会话".to_string(), Cmd::PrevTab),
        ];
        for (i, s) in self.sessions.iter().enumerate() {
            v.push((format!("切换到: {}", s.title(cx)), Cmd::SwitchTab(i)));
        }
        v
    }

    fn exec_cmd(&mut self, cmd: Cmd, window: &mut Window, cx: &mut Context<Self>) {
        self.close_palette(window, cx);
        match cmd {
            Cmd::NewTab => self.new_tab(cx),
            Cmd::OpenProject => self.open_project(cx),
            Cmd::CloseTab => self.close_active(window, cx),
            Cmd::NextTab => self.next_active(window, cx),
            Cmd::PrevTab => self.prev_active(window, cx),
            Cmd::SwitchTab(i) => self.activate(i, window, cx),
        }
    }

    /// 递归渲染分屏布局树：Leaf 渲染一个终端（活动 pane 描边 + 点击聚焦），
    /// Split 用 h/v_resizable 把子节点排成可拖拽的并排 / 堆叠。
    fn render_pane(&self, pane: &Pane, path: &str, cx: &mut Context<Self>) -> AnyElement {
        match pane {
            Pane::Leaf(t) => {
                let active = self
                    .cur()
                    .is_some_and(|s| s.active.entity_id() == t.entity_id());
                // 不给任何 pane 描边（iTerm2 也不描，之前的蓝框提醒也拿掉了）：分屏时靠
                // 「压暗非活动 pane」区分谁是活动的就够了；单 pane 时压根没有别的 pane
                // 可比，不需要任何叠加层。
                let multi_pane = self.cur().is_some_and(|s| s.pane_count() > 1);
                let overlay = if !multi_pane || active {
                    div().absolute().inset_0()
                } else {
                    div().absolute().inset_0().bg(hsla(0., 0., 0., 0.28))
                };
                let te = t.clone();
                div()
                    .id(SharedString::from(path.to_string()))
                    .relative()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .overflow_hidden()
                    // 点击 pane 即设为当前会话的活动 pane（终端自身也会抢焦点，二者一致）。
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _ev, window, cx| {
                            this.activate_pane(&te, window, cx)
                        }),
                    )
                    .child(t.clone())
                    .child(overlay)
                    .into_any_element()
            }
            Pane::Split { axis, state, children } => {
                let id = SharedString::from(path.to_string());
                let mut group = if matches!(axis, Axis::Horizontal) {
                    h_resizable(id)
                } else {
                    v_resizable(id)
                }
                .with_state(state);
                for (i, c) in children.iter().enumerate() {
                    group = group.child(self.render_pane(c, &format!("{path}-{i}"), cx));
                }
                group.into_any_element()
            }
        }
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.active_session;
        let can_close = self.sessions.len() > 1;

        // Dock 角标：统计「等审批 + 需要处理」的会话数，变了才调 Cocoa API 更新
        // （避免每次 render 都发一遍 setBadgeLabel）。
        let attention_count = self
            .sessions
            .iter()
            .filter(|s| matches!(s.status(cx), AgentStatus::WaitingApproval | AgentStatus::NeedsAttention))
            .count();
        if self.dock_badge_count != Some(attention_count) {
            self.dock_badge_count = Some(attention_count);
            dock::set_badge(attention_count);
        }

        // Git 页：后台刷新改动列表（git status 慢，绝不在 render 里同步跑）。
        if self.view == MainView::Git {
            if let Some(root) = self.cur().and_then(|s| s.cwd(cx)) {
                self.ensure_git_watch(root.clone(), cx);
                self.ensure_git_status(root, cx);
            }
        }

        // 热力图页：后台刷新改动热力（git log 扫历史更慢，同样绝不同步跑）。
        if self.view == MainView::Hotspot {
            if let Some(root) = self.cur().and_then(|s| s.cwd(cx)) {
                self.ensure_hotspot(root, cx);
            }
        }

        // 历史会话页：后台刷新当前项目的会话列表。
        if self.view == MainView::History {
            if let Some(root) = self.cur().and_then(|s| s.cwd(cx)) {
                self.ensure_session_list(root, cx);
            }
        }

        // 文件树页：后台刷新根目录 + 所有已展开目录的直接子项列表（fs::read_dir 绝不
        // 在 render 里同步跑）。展开新目录时它会先落空，下一帧缓存到位后自动出现。
        if self.view == MainView::Files {
            // 搜索输入框懒创建（需要 window）：键入即 notify，触发文件名 + 内容搜索。
            if self.file_filter.is_none() {
                use gpui_component::input::{InputEvent, InputState};
                let state =
                    cx.new(|cx| InputState::new(window, cx).placeholder("搜索文件名 / 内容…"));
                self._file_filter_sub =
                    Some(cx.subscribe(&state, |_, _, ev: &InputEvent, cx| {
                        if matches!(ev, InputEvent::Change) {
                            cx.notify();
                        }
                    }));
                self.file_filter = Some(state);
            }
            let query = self
                .file_filter
                .as_ref()
                .map(|s| s.read(cx).value().trim().to_string())
                .unwrap_or_default();
            if let Some(root) = self.cur().and_then(|s| s.cwd(cx)) {
                if query.is_empty() {
                    // 无查询：正常树形浏览，清空上一次搜索结果。
                    self.search_results = None;
                    self.ensure_dir_listing(root.clone(), cx);
                    for dir in self.expanded.clone() {
                        self.ensure_dir_listing(dir, cx);
                    }
                } else {
                    // 有查询：切换为搜索结果视图，后台遍历项目（query 未变则不重扫）。
                    self.ensure_search(root, query, cx);
                }
            }
        }

        // 同步窗口背景外观：不透明度 / 模糊改了（可能来自 slider/取色器的无 window 回调）
        // → 这里用 window 切换透明/模糊。仅在变化时调，避免每帧重复。
        let want_bg = cx.global::<Appearance>().window_bg();
        if self.applied_window_bg != Some(want_bg) {
            window.set_background_appearance(want_bg);
            self.applied_window_bg = Some(want_bg);
        }

        // 调试 HUD：开启时用 request_animation_frame 驱动连续渲染，测真实帧率
        // （连续重绘会重跑整窗布局/绘制，diff 面板卡不卡直接反映到帧耗时上）。
        if self.debug_hud {
            let now = Instant::now();
            if let Some(prev) = self.last_frame {
                let dt = now.duration_since(prev).as_secs_f32();
                if dt > 0.0 {
                    let inst = 1.0 / dt;
                    self.fps_ema =
                        if self.fps_ema <= 0.0 { inst } else { self.fps_ema * 0.9 + inst * 0.1 };
                }
            }
            self.last_frame = Some(now);
            window.request_animation_frame();
        } else {
            self.last_frame = None;
        }

        // 主题色 token（跟随 gpui-component 主题，替代硬编码）
        let (c_bg, c_border, c_popover, c_muted, c_fg) = {
            let t = cx.theme();
            (t.background, t.border, t.popover, t.muted_foreground, t.foreground)
        };

        // 会话标题（取活动终端的 cwd 末段）
        let titles: Vec<(usize, String)> = self
            .sessions
            .iter()
            .enumerate()
            .map(|(ix, s)| (ix, s.title(cx)))
            .collect();
        // 各会话状态（预算好，避免在侧栏 map 闭包里借用 cx）。与总览页共用同一套五态配色。
        let statuses: Vec<AgentStatus> = self.sessions.iter().map(|s| s.status(cx)).collect();
        // 各会话的稳定身份（拖拽排序用：下标会因增删/排序失效，entity_id 不会）。
        let entity_ids: Vec<EntityId> = self.sessions.iter().map(|s| s.active.entity_id()).collect();
        // 待处理通知总数（标题栏铃铛用）。
        let notif_count = self.collect_notifications(cx).len();
        // 当前活动会话的标题：放到标题栏右侧作为上下文提示。
        let active_title = titles
            .iter()
            .find(|(ix, _)| *ix == active)
            .map(|(_, t)| t.clone())
            .unwrap_or_default();

        // 左侧会话侧栏：按会话的 cwd 分组成项目（保持出现顺序），
        // 记住每组的完整 cwd 供「在该项目新建终端」用。分组算法见 project_groups。
        let projects = self.project_groups(cx);

        // 项目 → 会话 两级菜单（gpui-component Sidebar）。
        // Sidebar 组件的回调是 Fn(&_, &mut Window, &mut App)，拿不到 Context<Self>，
        // 故捕获 entity 句柄在闭包里 update 自身。
        let this = cx.entity();
        let menu_items: Vec<SidebarMenuItem> = projects
            .iter()
            .enumerate()
            .map(|(pix, (name, cwd, ixs))| {
                let sess_items: Vec<SidebarMenuItem> = ixs
                    .iter()
                    .map(|&ix| {
                        let title = titles.get(ix).map(|(_, t)| t.clone()).unwrap_or_default();
                        let status = statuses.get(ix).copied().unwrap_or(AgentStatus::Idle);
                        // 只在「非活动」会话上亮点：正在看的那个不提醒（但通知仍留着）。
                        // 空闲不点，其余四态用与总览页一致的颜色，一眼区分等审批/需处理/运行中/已完成。
                        let status_dot = (status != AgentStatus::Idle && ix != active).then(|| {
                            match status {
                                AgentStatus::WaitingApproval => rgb(0xef4444),
                                AgentStatus::NeedsAttention => rgb(0xf59e0b),
                                AgentStatus::Running => rgb(0x4a9eff),
                                AgentStatus::Done => rgb(0x22c55e),
                                AgentStatus::Idle => unreachable!(),
                            }
                        });
                        let e_act = this.clone();
                        let entity_id = entity_ids[ix];
                        let drag_title: SharedString = title.clone().into();
                        let e_drop = this.clone();
                        let e_close = this.clone();
                        // 拖拽悬停指示：本行上/下边缘是否是当前插入位置（快照进 suffix
                        // 闭包；hint 变化会 notify 重渲染，快照不会过期）。
                        let hint_before = self.sess_drop_hint == Some((entity_id, true));
                        let hint_after = self.sess_drop_hint == Some((entity_id, false));
                        // 行图标跟建它时用的启动方式对齐（新建终端/Claude Code/Codex/
                        // Copilot），跟「+」下拉菜单里的图标一一对应，见 LaunchKind。
                        let row_icon = match self.sessions[ix].active.read(cx).launch_kind() {
                            terminal_view::LaunchKind::Claude => IconName::Asterisk,
                            terminal_view::LaunchKind::Codex => IconName::Bot,
                            terminal_view::LaunchKind::Copilot => IconName::Github,
                            terminal_view::LaunchKind::Terminal => IconName::SquareTerminal,
                        };
                        let item = SidebarMenuItem::new(title)
                            .icon(row_icon)
                            .active(ix == active)
                            .on_click(move |_ev, window, cx| {
                                e_act.update(cx, |ws, cx| ws.activate(ix, window, cx));
                            })
                            // suffix：拖拽手柄（项目内排序）+ 状态点 + 关闭按钮。
                            .suffix(move |_w, cx| {
                                let e_close = e_close.clone();
                                let e_drop_before = e_drop.clone();
                                let e_drop_after = e_drop.clone();
                                let e_clear = e_drop.clone();
                                let drag_title = drag_title.clone();
                                // 拖拽进行中才渲染整行 drop 接收层：suffix 位于行右端，
                                // 用负 left 往左铺满整行（超出部分被行容器裁掉），上半段
                                // 插到目标前、下半段插到目标后。平时不渲染，不挡点击。
                                let dragging = cx.has_active_drag();
                                // 插入位置指示条：淡入 + 从左展开的动画"插槽"。由
                                // on_drag_move 维护的 hint 状态驱动，不用 drag_over 样式
                                //（样式刷新是瞬时的，做不了出现动画）。
                                let make_hint = move |before: bool, e_hint: Entity<Workspace>| {
                                    move |ev: &DragMoveEvent<SessionDrag>, _w: &mut Window, cx: &mut App| {
                                        let inside = ev.bounds.contains(&ev.event.position);
                                        e_hint.update(cx, |ws, cx| {
                                            let this_hint = Some((entity_id, before));
                                            if inside && ws.sess_drop_hint != this_hint {
                                                ws.sess_drop_hint = this_hint;
                                                cx.notify();
                                            } else if !inside && ws.sess_drop_hint == this_hint {
                                                ws.sess_drop_hint = None;
                                                cx.notify();
                                            }
                                        });
                                    }
                                };
                                let indicator = |anim_id: (&'static str, usize), at_top: bool| {
                                    div()
                                        .absolute()
                                        .left(px(4.))
                                        .h(px(5.))
                                        .rounded(px(2.5))
                                        .bg(rgb(0x4a9eff))
                                        .map(|d| if at_top { d.top(px(2.)) } else { d.bottom(px(2.)) })
                                        .with_animation(
                                            anim_id,
                                            Animation::new(std::time::Duration::from_millis(160))
                                                .with_easing(ease_out_quint()),
                                            |this, delta| {
                                                this.opacity(0.4 + 0.6 * delta).w(relative(delta))
                                            },
                                        )
                                };
                                h_flex()
                                    .relative()
                                    .items_center()
                                    .gap_1()
                                    .child(
                                        // 拖拽手柄：不露图标，留一块看不见但比原 12px 图标明显更大的
                                        // 抓取区（原图标太小不好点住）；按住这里拖拽排序，行内其余
                                        // 点击（切换会话/关闭）走各自正常逻辑，互不影响。
                                        div()
                                            .id(("sess-drag", ix))
                                            .w(px(28.))
                                            .h(px(20.))
                                            .cursor_grab()
                                            .on_drag(
                                                SessionDrag { id: entity_id, title: drag_title },
                                                move |drag, _, _, cx| {
                                                    // 起拖先清掉上次拖拽残留的指示位置
                                                    e_clear.update(cx, |ws, _| ws.sess_drop_hint = None);
                                                    cx.new(|_| drag.clone())
                                                },
                                            ),
                                    )
                                    .children(
                                        status_dot
                                            .map(|c| div().size_2().rounded_full().bg(c)),
                                    )
                                    .children(can_close.then(|| {
                                        Button::new(("close-session", ix))
                                            .ghost()
                                            .xsmall()
                                            .icon(IconName::CircleX)
                                            .on_click(move |_ev, _w, cx| {
                                                // 别把点击冒泡成「切换到该会话」
                                                cx.stop_propagation();
                                                e_close.update(cx, |ws, cx| ws.close_session(ix, cx));
                                            })
                                    }))
                                    .when(dragging, |this| {
                                        this.child(
                                            div()
                                                .absolute()
                                                .top(px(-6.))
                                                .bottom(px(-6.))
                                                .left(px(-1000.))
                                                .right(px(-8.))
                                                .child(
                                                    div()
                                                        .id(("sess-drop-before", ix))
                                                        .absolute()
                                                        .top_0()
                                                        .left_0()
                                                        .right_0()
                                                        .h_1_2()
                                                        .on_drag_move(make_hint(true, e_drop_before.clone()))
                                                        .on_drop(move |drag: &SessionDrag, _window, cx| {
                                                            let dragged = drag.id;
                                                            e_drop_before.update(cx, |ws, cx| {
                                                                ws.sess_drop_hint = None;
                                                                ws.move_session_near(dragged, entity_id, true, cx)
                                                            });
                                                        }),
                                                )
                                                .child(
                                                    div()
                                                        .id(("sess-drop-after", ix))
                                                        .absolute()
                                                        .bottom_0()
                                                        .left_0()
                                                        .right_0()
                                                        .h_1_2()
                                                        .on_drag_move(make_hint(false, e_drop_after.clone()))
                                                        .on_drop(move |drag: &SessionDrag, _window, cx| {
                                                            let dragged = drag.id;
                                                            e_drop_after.update(cx, |ws, cx| {
                                                                ws.sess_drop_hint = None;
                                                                ws.move_session_near(dragged, entity_id, false, cx)
                                                            });
                                                        }),
                                                )
                                                .when(hint_before, |this| {
                                                    this.child(indicator(("sess-ind-b", ix), true))
                                                })
                                                .when(hint_after, |this| {
                                                    this.child(indicator(("sess-ind-a", ix), false))
                                                }),
                                        )
                                    })
                            });
                        item
                    })
                    .collect();
                // 项目行右侧「+」：点击弹出下拉菜单（新建终端 / Claude Code / Codex）。
                // 用 gpui-component 的 DropdownMenu 真浮层，不再用 hover 状态机模拟——
                // hover 版鼠标移向菜单项途中就会被延时收起，菜单项根本点不到。
                let e_new = this.clone();
                let e_proj_drop = this.clone();
                let project_name: SharedString = name.clone().into();
                let new_cwd = cwd.clone();
                let is_scratch_group = scratch_dir().as_deref() == Some(cwd.as_str());
                // 拖拽悬停指示：本项目行是否是当前插入位置。
                let proj_hinted = self.proj_drop_hint.as_deref() == Some(name.as_str());
                SidebarMenuItem::new(name.clone())
                    .icon(if is_scratch_group { IconName::SquareTerminal } else { IconName::Folder })
                    .default_open(true)
                    .click_to_toggle(true)
                    .suffix(move |_w, cx| {
                        let e_new = e_new.clone();
                        let e_proj_drop = e_proj_drop.clone();
                        let project_name = project_name.clone();
                        let cwd = new_cwd.clone();
                        let dragging = cx.has_active_drag();
                        h_flex()
                            .relative()
                            .items_center()
                            .gap_1()
                            .child(
                                // 拖拽手柄：项目分组之间排序。不露图标，留一块看不见但比原 12px
                                // 图标明显更大的抓取区，跟「+」下拉按钮各自独立不冲突。
                                div()
                                    .id(("project-drag", pix))
                                    .w(px(28.))
                                    .h(px(20.))
                                    .cursor_grab()
                                    .on_drag(ProjectDrag { name: project_name.clone() }, {
                                        let e_clear = e_proj_drop.clone();
                                        move |drag, _, _, cx| {
                                            // 起拖先清掉上次拖拽残留的指示位置
                                            e_clear.update(cx, |ws, _| ws.proj_drop_hint = None);
                                            cx.new(|_| drag.clone())
                                        }
                                    }),
                            )
                            .child(
                                Button::new(("new-in-project", pix))
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Plus)
                                    .dropdown_menu(move |menu, _window, _cx| {
                                        let cwd = (!cwd.is_empty()).then(|| cwd.clone());
                                        let cwd_new = cwd.clone();
                                        let cwd_claude = cwd.clone();
                                        let cwd_codex = cwd.clone();
                                        let cwd_copilot = cwd;
                                        let e_term = e_new.clone();
                                        let e_claude = e_new.clone();
                                        let e_codex = e_new.clone();
                                        let e_copilot = e_new.clone();
                                        menu.item(
                                            PopupMenuItem::new("新建终端")
                                                .icon(IconName::SquareTerminal)
                                                .on_click(move |_ev, _window, cx| {
                                                    let cwd = cwd_new.clone();
                                                    e_term.update(cx, |ws, cx| ws.add_session(cwd, cx));
                                                }),
                                        )
                                        .item(
                                            PopupMenuItem::new("Claude Code")
                                                .icon(IconName::Asterisk)
                                                .on_click(move |_ev, _window, cx| {
                                                    let cwd = cwd_claude.clone();
                                                    // 是否跳过权限确认由设置页的开关决定，每次点击都读最新值。
                                                    let full_perm = cx
                                                        .try_global::<LaunchConfig>()
                                                        .is_some_and(|c| c.claude_full_permissions);
                                                    let launch = if full_perm {
                                                        "claude --dangerously-skip-permissions"
                                                    } else {
                                                        "claude"
                                                    };
                                                    e_claude.update(cx, |ws, cx| {
                                                        ws.add_session_with_launch(cwd, Some(launch), cx)
                                                    });
                                                }),
                                        )
                                        .item(
                                            PopupMenuItem::new("Codex")
                                                .icon(IconName::Bot)
                                                .on_click(move |_ev, _window, cx| {
                                                    let cwd = cwd_codex.clone();
                                                    e_codex.update(cx, |ws, cx| {
                                                        ws.add_session_with_launch(cwd, Some("codex"), cx)
                                                    });
                                                }),
                                        )
                                        .item(
                                            PopupMenuItem::new("Copilot")
                                                .icon(IconName::Github)
                                                .on_click(move |_ev, _window, cx| {
                                                    let cwd = cwd_copilot.clone();
                                                    e_copilot.update(cx, |ws, cx| {
                                                        ws.add_session_with_launch(cwd, Some("copilot"), cx)
                                                    });
                                                }),
                                        )
                                    }),
                            )
                            .when(dragging, |this| {
                                // 拖拽进行中的整行 drop 接收层，同会话行的做法。
                                let e_hint = e_proj_drop.clone();
                                let hint_name = project_name.clone();
                                this.child(
                                    div()
                                        .id(("project-drop", pix))
                                        .absolute()
                                        .top(px(-6.))
                                        .bottom(px(-6.))
                                        .left(px(-1000.))
                                        .right(px(-8.))
                                        .on_drag_move(move |ev: &DragMoveEvent<ProjectDrag>, _w, cx| {
                                            let inside = ev.bounds.contains(&ev.event.position);
                                            let hint_name = hint_name.clone();
                                            e_hint.update(cx, |ws, cx| {
                                                if inside && ws.proj_drop_hint.as_ref() != Some(&hint_name) {
                                                    ws.proj_drop_hint = Some(hint_name);
                                                    cx.notify();
                                                } else if !inside && ws.proj_drop_hint.as_ref() == Some(&hint_name) {
                                                    ws.proj_drop_hint = None;
                                                    cx.notify();
                                                }
                                            });
                                        })
                                        .on_drop(move |drag: &ProjectDrag, _window, cx| {
                                            let from = drag.name.clone();
                                            let to = project_name.clone();
                                            e_proj_drop.update(cx, |ws, cx| {
                                                ws.proj_drop_hint = None;
                                                ws.move_project_near(from, to, cx)
                                            });
                                        })
                                        .when(proj_hinted, |this| {
                                            this.child(
                                                div()
                                                    .absolute()
                                                    .top(px(2.))
                                                    .left(px(4.))
                                                    .h(px(5.))
                                                    .rounded(px(2.5))
                                                    .bg(rgb(0x4a9eff))
                                                    .with_animation(
                                                        ("proj-ind", pix),
                                                        Animation::new(std::time::Duration::from_millis(160))
                                                            .with_easing(ease_out_quint()),
                                                        |this, delta| {
                                                            this.opacity(0.4 + 0.6 * delta).w(relative(delta))
                                                        },
                                                    ),
                                            )
                                        }),
                                )
                            })
                    })
                    .children(sess_items)
            })
            .collect();

        let overview_active = self.view == MainView::Overview;
        let e_overview = this.clone();
        let sidebar_el = Sidebar::new("workspace-sidebar")
            .collapsible(SidebarCollapsible::Offcanvas)
            // 宽度交给外层 resizable_panel 控制（可拖），这里填满 panel。
            // 品牌已移到顶部标题栏，侧栏直接从「会话」开始，避免重复。
            .w(relative(1.))
            // 总览：不挂在任何项目下的全局入口，跟当前在哪个项目无关，随时点得到。
            // 新建终端挪到底部跟「打开项目」放一起了（见 footer），都是「开个新地方干活」
            // 这一类操作，归在一块更好找。
            .child(
                SidebarGroup::new("").child(
                    SidebarMenu::new().children([
                        SidebarMenuItem::new("总览")
                            .icon(IconName::LayoutDashboard)
                            .active(overview_active)
                            .on_click(move |_ev, _window, cx| {
                                e_overview.update(cx, |ws, cx| {
                                    ws.view = MainView::Overview;
                                    ws.refresh_git(cx); // 进总览 → 后台刷新 git
                                    cx.notify();
                                });
                            }),
                    ]),
                ),
            )
            .child(SidebarGroup::new("会话").child(SidebarMenu::new().children(menu_items)))
            // 不用 SidebarFooter：它会给整块 footer 挂 hover 背景（sidebar_accent），
            // 盖住按钮自己的 hover。直接放普通容器，让每个按钮各自 hover 可见。
            .footer(
                div()
                    .flex()
                    .flex_col()
                    .w_full()
                    .gap_1()
                    .px_2()
                    .pt_2()
                    .pb_1()
                    .border_t_1()
                    .border_color(rgba(0xffffff0d))
                    // 「打开项目」+「新建终端」并排：都是"开个新地方干活"，归一块好找。
                    .child(
                        h_flex()
                            .w_full()
                            .gap_1()
                            .child(open_project_button(cx))
                            .child(scratch_terminal_button(cx)),
                    )
                    // 版本号居中：编译期取 Cargo.toml 的 version；点一下跳 GitHub 仓库。
                    .child(
                        div()
                            .id("version-github-link")
                            .w_full()
                            .flex()
                            .justify_center()
                            .cursor_pointer()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .hover(|s| s.text_color(cx.theme().foreground))
                            .child(concat!("v", env!("CARGO_PKG_VERSION")))
                            .on_mouse_down(MouseButton::Left, |_, _window, cx| {
                                cx.open_url("https://github.com/zzfn/smelt");
                            }),
                    ),
            );

        // 主内容：有会话就渲染当前会话的分屏布局树；无会话显示空状态引导。
        // 需 .flex()，否则单 pane 的叶子 flex_1 不生效、塌缩到内容高度（边框不到底）。
        let content = if self.sessions.get(self.active_session).is_some() {
            div()
                .flex_1()
                .min_w_0()
                .min_h_0()
                .flex()
                .child(self.render_pane(&self.sessions[self.active_session].layout, "pane", cx))
        } else {
            // 空状态：引导用户新建会话 / 打开项目。
            let btn = |id: &'static str, label: &'static str| {
                div()
                    .id(id)
                    .px_3()
                    .py(px(6.))
                    .rounded_md()
                    .cursor_pointer()
                    .border_1()
                    .border_color(c_border)
                    .text_color(c_fg)
                    .text_sm()
                    .hover(|s| s.bg(c_border))
                    .child(label.to_string())
            };
            div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_4()
                .child(Icon::new(IconName::SquareTerminal).size(px(40.)).text_color(c_muted))
                .child(div().text_color(c_muted).child("还没有会话"))
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(btn("empty-new", "+ 新建会话").on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _w, cx| this.new_tab(cx)),
                        ))
                        .child(btn("empty-open", "打开项目…").on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _w, cx| this.open_project(cx)),
                        )),
                )
        };

        // 命令面板弹层：搜索框 + 候选列表全部由 ListState 渲染。
        let palette_overlay = self.palette.as_ref().map(|state| {
            div()
                .absolute()
                .inset_0()
                .flex()
                .justify_center()
                .pt(px(80.))
                // 点背景空白处关闭面板
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| this.close_palette(window, cx)),
                )
                .child(
                    div()
                        // 点面板内部不冒泡到背景，避免误关
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .w(px(480.))
                        .h(px(360.))
                        .flex()
                        .flex_col()
                        .bg(c_popover)
                        .border_1()
                        .border_color(c_border)
                        .rounded_lg()
                        .shadow_lg()
                        .child(List::new(state).search_placeholder("输入命令…")),
                )
        });

        div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(c_bg)
            .font_family(terminal_view::FONT_FAMILY)
            .on_action(cx.listener(|this, _: &Quit, _window, cx| {
                this.show_quit_confirm = true;
                cx.notify();
            }))
            // Cmd+, / 应用菜单「设置…」：跟齿轮图标共用同一个独立设置窗口。
            .on_action(cx.listener(|this, _: &OpenSettings, window, cx| {
                if this.llm_inputs.is_none() {
                    this.init_llm_inputs(window, cx);
                }
                this.open_settings_window(cx);
            }))
            // 从 Finder 拖文件/文件夹进窗口 → 当作项目开新标签（文件取其父目录）。
            .on_drop::<ExternalPaths>(cx.listener(|this, ep: &ExternalPaths, _window, cx| {
                this.open_paths(ep.paths(), cx);
            }))
            // 全局快捷键：Cmd+K 面板 / Cmd+B 侧栏 / Cmd+\ 布局 / Cmd+[ ] 切换
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                let ks = &ev.keystroke;
                if !ks.modifiers.platform {
                    return;
                }
                match ks.key.as_str() {
                    "k" => {
                        if this.palette.is_some() {
                            this.close_palette(window, cx);
                        } else {
                            this.open_palette(window, cx);
                        }
                    }
                    "b" => {
                        this.sidebar_open = !this.sidebar_open;
                        cx.notify();
                    }
                    "[" => this.prev_active(window, cx),
                    "]" => this.next_active(window, cx),
                    // Cmd+D 竖切（右侧并排）/ Cmd+Shift+D 横切（下方堆叠）
                    "d" => {
                        let axis = if ks.modifiers.shift {
                            Axis::Vertical
                        } else {
                            Axis::Horizontal
                        };
                        this.split_active(axis, cx);
                    }
                    // Cmd+W 关闭当前 pane；会话只剩一个 pane 时关掉整个会话（至少留一个会话）
                    "w" => this.close_active(window, cx),
                    // Cmd+S：保存文件树里打开的文件（仅 Files 页，避免切到别的
                    // 视图时背着用户悄悄写盘）。
                    "s" if this.view == MainView::Files => this.save_open_file(cx),
                    // Cmd+Shift+F 切换调试 HUD（右上角帧率）
                    "f" if ks.modifiers.shift => {
                        this.debug_hud = !this.debug_hud;
                        this.fps_ema = 0.0;
                        this.last_frame = None;
                        cx.notify();
                    }
                    // Cmd+Q 退出交给应用菜单的 Quit action（全局绑定，见 main）
                    _ => {}
                }
            }))
            // 顶部集成标题栏：透明 + 红绿灯占位 + 可拖拽，替代割裂的系统灰条。
            .child(
                TitleBar::new().child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .w_full()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    img(Arc::new(Image::from_bytes(
                                        ImageFormat::Png,
                                        include_bytes!("../../../assets/icon-1024.png").to_vec(),
                                    )))
                                    .w(px(16.))
                                    .h(px(16.))
                                    .rounded(px(4.)),
                                )
                                .child(div().text_sm().font_bold().child("smelt"))
                                .child(div().text_sm().text_color(c_muted).child(active_title)),
                        )
                        // 右侧：铃铛（通知面板）+ 齿轮（外观设置）。stop_propagation 避免触发拖拽。
                        .child(
                            h_flex()
                                .items_center()
                                .gap_1()
                                // 留出右侧呼吸间距，别让齿轮贴到窗口边缘。
                                .pr_2()
                                .child(
                                    div()
                                        .id("usage-entry")
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .size_6()
                                        .rounded_md()
                                        .cursor_pointer()
                                        .text_color(c_muted)
                                        .hover(|s| s.bg(c_border))
                                        .child(Icon::new(IconName::ChartPie))
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(|this, _, _w, cx| {
                                                cx.stop_propagation();
                                                this.open_usage_window(cx);
                                            }),
                                        ),
                                )
                                .child(
                                    div()
                                        .id("notif-bell")
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .size_6()
                                        .rounded_md()
                                        .cursor_pointer()
                                        // 有待处理通知 → 铃铛变蓝
                                        .text_color(if notif_count > 0 {
                                            rgb(0x4a9eff).into()
                                        } else {
                                            c_muted
                                        })
                                        .hover(|s| s.bg(c_border))
                                        .child(Icon::new(IconName::Bell))
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(|this, _, _w, cx| {
                                                cx.stop_propagation();
                                                this.notifications_open = !this.notifications_open;
                                                cx.notify();
                                            }),
                                        ),
                                )
                                .child({
                                    // 有新版本在下载/已就绪，或守护落后于磁盘二进制 → 齿轮角上
                                    // 缀一个红点提醒「有待处理事项」，图标本身颜色不跟着变——
                                    // 之前让整个图标变蓝，看着像常驻高亮状态，容易被当成卡住了。
                                    let needs_attention = matches!(
                                        self.update_status,
                                        updater::UpdateStatus::Downloading { .. }
                                            | updater::UpdateStatus::ReadyToInstall { .. }
                                    ) || self.daemon_outdated == Some(true);
                                    let gear: AnyElement = if needs_attention {
                                        Badge::new().dot().child(Icon::new(IconName::Settings)).into_any_element()
                                    } else {
                                        Icon::new(IconName::Settings).into_any_element()
                                    };
                                    div()
                                        .id("settings-gear")
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .size_6()
                                        .rounded_md()
                                        .cursor_pointer()
                                        .text_color(c_muted)
                                        .hover(|s| s.bg(c_border))
                                        .child(gear)
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(|this, _, window, cx| {
                                                cx.stop_propagation();
                                                this.check_daemon_outdated(cx);
                                                if this.llm_inputs.is_none() {
                                                    this.init_llm_inputs(window, cx);
                                                }
                                                this.open_settings_window(cx);
                                            }),
                                        )
                                }),
                        ),
                ),
            )
            // 主体：左侧会话侧栏 + 右侧主区，占满标题栏以下的剩余高度。
            .child(
                div().flex_1().min_h_0().flex().child(
                h_resizable("root-split")
                    .with_state(&self.root_resize)
                    // 会话侧栏：可拖拽宽度（160–420），Cmd+B 整体显隐
                    .child(
                        resizable_panel()
                            .size(px(self.sidebar_w))
                            .size_range(px(160.)..px(420.))
                            .visible(self.sidebar_open)
                            .child(sidebar_el),
                    )
                    // 主区：顶部视图切换 + 内容
                    .child(resizable_panel().child(
                        div()
                            .flex_1()
                            // min_w_0：主区在根 flex 行里默认 min-width:auto，会被最长终端行
                            // 撑到不肯收缩，导致宽度被内容反向放大。归零后才能正常按剩余空间收缩。
                            .min_w_0()
                    .flex()
                    .flex_col()
                    // 会话视图 tab（终端/文件树/Git）——总览是全局视图，走侧栏入口，不在这排里；
                    // 用量页已拆成独立窗口，不再是 self.view 的一种取值。
                    .children((self.view != MainView::Overview)
                        .then(|| {
                        TabBar::new("main-view-tabs")
                            .underline()
                            // 左缩进 12px，与终端/文件内容左边基线对齐（不贴边）；
                            // underline 变体的底边线是绝对满宽 div，不受此内边距影响。
                            .pl(px(12.))
                            .selected_index(match self.view {
                                MainView::Terminal => 0,
                                MainView::Files => 1,
                                MainView::Git => 2,
                                MainView::Hotspot => 3,
                                _ => 4,
                            })
                            .on_click(cx.listener(|this, ix: &usize, _window, cx| {
                                this.view = match *ix {
                                    0 => MainView::Terminal,
                                    1 => MainView::Files,
                                    2 => MainView::Git,
                                    3 => MainView::Hotspot,
                                    _ => MainView::History,
                                };
                                cx.notify();
                            }))
                            .child(Tab::new().label("终端"))
                            .child(Tab::new().label("文件树"))
                            .child(Tab::new().label("Git"))
                            .child(Tab::new().label("热力图"))
                            .child(Tab::new().label("历史会话"))
                    }))
                    .child(match self.view {
                        MainView::Overview => self.render_overview(cx),
                        MainView::Terminal => content,
                        MainView::Files => {
                            let cwd = self.cur().and_then(|s| s.cwd(cx));
                            // 有查询串 → 显示搜索结果；否则显示文件树。
                            let has_query = self
                                .file_filter
                                .as_ref()
                                .is_some_and(|s| !s.read(cx).value().trim().is_empty());
                            let body = if has_query {
                                match &self.search_results {
                                    Some(state) => {
                                        search_results_view(state, &self.file_tree_scroll, cx)
                                    }
                                    // ensure_search 已在 render 顶部同步置位，通常到不了这里。
                                    None => div().flex_1().into_any_element(),
                                }
                            } else {
                                file_tree(cwd, &self.expanded, &self.dir_cache, &self.file_tree_scroll, cx)
                            };
                            // 顶部搜索框（file_filter 已在 render 顶部懒创建）。
                            let search_box = self.file_filter.as_ref().map(|state| {
                                div()
                                    .px_2()
                                    .py(px(6.))
                                    .border_b_1()
                                    .border_color(c_border)
                                    .child(Input::new(state).small())
                            });
                            let content = file_content_pane(&self.open_file, cx);
                            div()
                                .flex_1()
                                // min_h_0：否则这个 flex item 会被文件内容撑到整份文件那么高、
                                // 溢出窗口，导致内部 uniform_list 拿不到有界高度而无法滚动。
                                .min_h_0()
                                .flex()
                                .child(
                                    div()
                                        .w(px(260.))
                                        .flex()
                                        .flex_col()
                                        .min_h_0()
                                        .border_r_1()
                                        .border_color(c_border)
                                        .children(search_box)
                                        .child(body),
                                )
                                .child(content)
                        }
                        MainView::Git => {
                            let cwd = self.cur().and_then(|s| s.cwd(cx));
                            let status =
                                cwd.as_ref().and_then(|r| self.git_status.get(r).map(|(_, d)| d));
                            // 评论输入框懒创建（需要 window），跟文件树搜索框同一套模式。
                            if self.git_diff.is_some() && self.diff_comment_input.is_none() {
                                use gpui_component::input::InputState;
                                let state = cx.new(|cx| {
                                    InputState::new(window, cx)
                                        .placeholder("给选中的行写评论，发送前可以再改改…")
                                });
                                self.diff_comment_input = Some(state);
                            }
                            git_view(
                                cwd.clone(),
                                status,
                                &self.git_diff,
                                self.diff_split,
                                &self.diff_selected,
                                self.diff_comment_input.as_ref(),
                                &self.git_files_scroll,
                                &self.diff_scroll,
                                cx,
                            )
                        }
                        MainView::Hotspot => {
                            let cwd = self.cur().and_then(|s| s.cwd(cx));
                            let data = cwd
                                .as_ref()
                                .and_then(|r| self.hotspot_data.get(r).map(|(_, d)| d.clone()));
                            hotspot_view(cwd, data, cx)
                        }
                        MainView::History => {
                            let cwd = self.cur().and_then(|s| s.cwd(cx));
                            let sessions = cwd.as_ref().and_then(|r| self.session_list.get(r).map(|(_, d)| d.clone()));
                            history_view(sessions, &self.session_detail, cx)
                        }
                    }),
                    )),
                ),
            )
            // 命令面板（最上层）
            .children(palette_overlay)
            // 退出确认拦截弹层
            .children(self.show_quit_confirm.then(|| self.render_quit_confirm(cx)))
            // 重启守护进程确认拦截弹层
            .children(self.show_daemon_restart_confirm.then(|| self.render_daemon_restart_confirm(cx)))
            // 文件未保存切换确认拦截弹层
            .children(
                self.pending_file_switch
                    .clone()
                    .map(|target| self.render_unsaved_file_confirm(target, cx)),
            )
            // 通知面板浮层
            .children(self.notifications_open.then(|| self.render_notifications(cx)))
            // 调试 HUD：右上角帧率 + 帧耗时（Cmd+Shift+F 切换）
            .children(self.debug_hud.then(|| {
                let fps = self.fps_ema;
                let ms = if fps > 0.0 { 1000.0 / fps } else { 0.0 };
                // 帧率健康度着色：≥55 绿、≥30 黄、否则红。
                let color = if fps >= 55.0 {
                    rgb(0x22c55e)
                } else if fps >= 30.0 {
                    rgb(0xf59e0b)
                } else {
                    rgb(0xef4444)
                };
                div()
                    .absolute()
                    .top(px(40.))
                    .right(px(12.))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(rgba(0x000000cc))
                    .border_1()
                    .border_color(rgba(0xffffff22))
                    .font_family(terminal_view::FONT_FAMILY)
                    .text_xs()
                    .text_color(color)
                    .child(format!("{fps:.0} FPS · {ms:.1} ms"))
            }))
    }
}

/// 主区占位视图（文件树 / Git 尚未实现）。
fn placeholder_view(text: &str, muted: Hsla) -> Div {
    div()
        .flex_1()
        .flex()
        .items_center()
        .justify_center()
        .text_color(muted)
        .child(text.to_string())
}

/// 文件树视图：只读目录列表缓存渲染（ensure_dir_listing 后台刷新，绝不在这里碰
/// 文件系统），已展开的文件夹递归显示，点击文件夹展开/收起、点击文件打开。
///
/// 未用 uniform_list 虚拟滚动：实测它对这里的行内容（含 Icon）的孤立测量会算出
/// 异常偏大的行高，导致可视区间被判定只能塞下 1 行——已用隔离实验定位到具体是
/// uniform_list 的度量逻辑而非容器高度链的问题。文件树条目量级远小于 git diff，
/// 虚拟滚动只是锦上添花而非必需，故改走普通可滚动列表（与 git-files 同款写法），
/// 优先保证正确显示；虚拟滚动作为后续可选优化记在 docs/roadmap.md。
fn file_tree(
    cwd: Option<String>,
    expanded: &HashSet<String>,
    dir_cache: &HashMap<String, (Instant, Rc<Vec<(String, bool)>>)>,
    scroll: &ScrollHandle,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let (muted, fg, hover) = {
        let t = cx.theme();
        (t.muted_foreground, t.foreground, t.accent)
    };
    let Some(root) = cwd else {
        return placeholder_view("无项目目录", muted).into_any_element();
    };
    if !dir_cache.contains_key(&root) {
        // 首次进入该项目：ensure_dir_listing 已在 render 顶部触发，下一帧就有数据。
        return placeholder_view("加载中…", muted).into_any_element();
    }

    // 每行预先算好展开状态。
    let mut flat: Vec<(usize, String, bool, String, bool)> = Vec::new();
    walk_dir_cached(&root, dir_cache, expanded, 0, &mut flat);

    let this = cx.entity();
    let rows: Vec<AnyElement> = flat
        .into_iter()
        .enumerate()
        .map(|(i, (depth, name, is_dir, path, is_expanded))| {
            let indent = px(8.0 + depth as f32 * 14.0);
            // 展开箭头：目录用 chevron（展开朝下 / 收起朝右），文件留等宽占位对齐。
            let arrow = if is_dir {
                div()
                    .w(px(14.))
                    .flex()
                    .justify_center()
                    .child(
                        Icon::new(if is_expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .size(px(12.))
                        .text_color(muted),
                    )
                    .into_any_element()
            } else {
                div().w(px(14.)).into_any_element()
            };
            // 类型图标：目录（展开 / 收起用不同文件夹图标）与文件区分。
            let type_icon = Icon::new(if is_dir {
                if is_expanded {
                    IconName::FolderOpen
                } else {
                    IconName::Folder
                }
            } else {
                IconName::File
            })
            .size(px(14.))
            .text_color(if is_dir { fg } else { muted });
            let this = this.clone();
            let p = path.clone();
            div()
                .id(("file", i))
                .flex()
                .items_center()
                .gap_1()
                .pl(indent)
                .pr_2()
                .py(px(1.0))
                .text_sm()
                .text_color(if is_dir { fg } else { muted })
                .hover(move |s| s.bg(hover))
                .on_click(move |_ev, window, cx| {
                    this.update(cx, |ws, cx| {
                        if is_dir {
                            ws.toggle_expand(p.clone(), cx);
                        } else {
                            ws.view_file(p.clone(), window, cx);
                        }
                    });
                })
                .child(arrow)
                .child(type_icon)
                .child(name)
                .into_any_element()
        })
        .collect();

    div()
        .id("file-tree")
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .py_1()
        .track_scroll(scroll)
        .vertical_scrollbar(scroll)
        .children(rows)
        .into_any_element()
}

/// 历史会话页：左侧列出当前项目下 Claude Code 保存的历史会话，右侧显示选中会话的
/// 对话内容（只读浏览，不支持 resume）。数据来自 session_history 模块，跟「用量」
/// 页读的是同一份 `~/.claude/projects/**/*.jsonl`，但这里还原对话本身而非统计聚合。
fn history_view(
    sessions: Option<Rc<Vec<session_history::SessionSummary>>>,
    detail: &Option<(PathBuf, Rc<session_history::SessionDetail>)>,
    cx: &mut Context<Workspace>,
) -> Div {
    let (muted, fg, c_border, accent, secondary) = {
        let t = cx.theme();
        (t.muted_foreground, t.foreground, t.border, t.primary, t.secondary)
    };
    let selected_path = detail.as_ref().map(|(p, _)| p.clone());
    let this = cx.entity();

    let list_body: AnyElement = match &sessions {
        None => placeholder_view("加载中…", muted).into_any_element(),
        Some(sessions) if sessions.is_empty() => {
            placeholder_view("这个项目还没有本地保存的历史会话", muted).into_any_element()
        }
        Some(sessions) => div()
            .id("session-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .children(sessions.iter().enumerate().map(|(i, s)| {
                let is_selected = selected_path.as_deref() == Some(s.path.as_path());
                let this = this.clone();
                let path = s.path.clone();
                // 有明显跨度（>1 分钟）就顺带标一下这个会话跑了多久，纯单条消息的
                // 会话就只显示时间点，不必画蛇添足展示"0 分钟"。
                let when = match (s.started_at, s.last_active_at) {
                    (Some(start), Some(last)) if (last - start).num_minutes() >= 1 => format!(
                        "{} · 跑了 {} 分钟",
                        last.with_timezone(&chrono::Local).format("%m-%d %H:%M"),
                        (last - start).num_minutes()
                    ),
                    (_, Some(last)) => last.with_timezone(&chrono::Local).format("%m-%d %H:%M").to_string(),
                    _ => String::new(),
                };
                div()
                    .id(("session-row", i))
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .px_3()
                    .py_2()
                    .cursor_pointer()
                    .when(is_selected, |el| el.bg(c_border))
                    .hover(move |s| s.bg(c_border))
                    .on_click(move |_ev, _window, cx| {
                        this.update(cx, |ws, cx| ws.open_session_detail(path.clone(), cx));
                    })
                    .child(div().text_sm().text_color(fg).child(s.title.clone()))
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(format!("{when} · {} 条消息", s.message_count)),
                    )
                    .into_any_element()
            }))
            .into_any_element(),
    };

    let detail_body: AnyElement = match detail {
        None => placeholder_view("← 选择一个历史会话查看内容", muted).into_any_element(),
        Some((_, d)) if d.turns.is_empty() => {
            placeholder_view("这份会话没有可展示的对话内容", muted).into_any_element()
        }
        Some((_, d)) => div()
            .id("session-detail")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_3()
            .p_3()
            .children(d.turns.iter().enumerate().map(|(i, t)| {
                let role = if t.is_user { "用户" } else { "Claude" };
                let role_color = if t.is_user { accent } else { fg };
                let bubble_bg = if t.is_user { accent.opacity(0.12) } else { secondary };
                // 工具名按出现顺序去重计数，多次调用同一工具合并成一行摘要
                // （比如连续 3 次 Bash 就显示"Bash ×3"），不然长会话里全是重复胶囊。
                let tool_summary = (!t.tools.is_empty()).then(|| {
                    let mut order: Vec<&String> = Vec::new();
                    let mut counts: HashMap<&String, usize> = HashMap::new();
                    for tool in &t.tools {
                        counts.entry(tool).and_modify(|c| *c += 1).or_insert_with(|| {
                            order.push(tool);
                            1
                        });
                    }
                    order
                        .into_iter()
                        .map(|name| {
                            let c = counts[name];
                            if c > 1 { format!("{name} ×{c}") } else { name.clone() }
                        })
                        .collect::<Vec<_>>()
                        .join(" · ")
                });
                v_flex()
                    .gap_1()
                    .px_3()
                    .py_2()
                    .rounded(px(8.))
                    .bg(bubble_bg)
                    .when(t.is_user, |el| el.max_w(px(560.)))
                    .child(
                        h_flex()
                            .gap_2()
                            .items_baseline()
                            .child(div().font_semibold().text_sm().text_color(role_color).child(role))
                            .children(t.timestamp.map(|ts| {
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .child(ts.with_timezone(&chrono::Local).format("%m-%d %H:%M").to_string())
                            })),
                    )
                    // 必须逐气泡给唯一 id：便捷函数 text::markdown() 拿调用处代码位置
                    // 当 id，循环里所有气泡会共享同一份 TextView 状态（文本互踩、高度
                    // 测量错乱，气泡整个叠在一起）。
                    .child(
                        div()
                            .text_sm()
                            .text_color(fg)
                            .child(TextView::markdown(("turn-md", i), t.text.clone())),
                    )
                    .children(tool_summary.map(|s| {
                        div().text_xs().text_color(muted).child(format!("🔧 {s}"))
                    }))
                    .into_any_element()
            }))
            .into_any_element(),
    };

    div()
        .flex_1()
        .min_h_0()
        .flex()
        .child(
            div()
                .w(px(280.))
                .flex()
                .flex_col()
                .min_h_0()
                .border_r_1()
                .border_color(c_border)
                .child(list_body),
        )
        .child(detail_body)
}

/// 用量页：本地 Claude Code 会话用量统计——今日走势 + 活动热力图（全局口径），
/// 加当前项目 / 全局汇总各自的按模型、按工具拆分（全局汇总另加按项目拆分）。
/// 数据来自 usage_stats::scan（后台扫描 `~/.claude/projects/**/*.jsonl` 缓存的结果）。
fn usage_view(
    cur_project: Option<String>,
    data: Option<Rc<usage_stats::UsageData>>,
    cx: &mut Context<Workspace>,
) -> Div {
    let (muted, c_border, chart_1, chart_2) = {
        let t = cx.theme();
        (t.muted_foreground, t.border, t.chart_1, t.chart_2)
    };

    let Some(data) = data else {
        return placeholder_view("统计中…", muted);
    };
    if data.events.is_empty() {
        return placeholder_view("没有找到本地 Claude Code 会话记录（~/.claude/projects）", muted);
    }

    // 活动热力图：近 12 周，按周对齐成「列=周，行=周一到周日」的日历格（全局口径）。
    let heat = usage_stats::daily_heatmap(&data.events, 12);
    let heat_total: u64 = heat.iter().map(|(_, v)| *v).sum();
    let max_heat = heat.iter().map(|(_, v)| *v).max().unwrap_or(0).max(1);
    let lead = heat.first().map(|(d, _)| d.weekday().num_days_from_monday()).unwrap_or(0) as usize;
    let mut cells: Vec<Option<u64>> =
        std::iter::repeat(None).take(lead).chain(heat.iter().map(|(_, v)| Some(*v))).collect();
    while cells.len() % 7 != 0 {
        cells.push(None);
    }
    let week_columns: Vec<AnyElement> = cells
        .chunks(7)
        .map(|week| {
            v_flex()
                .gap(px(2.))
                .children(week.iter().map(|cell| {
                    div().size(px(11.)).rounded(px(2.)).bg(heat_cell_color(*cell, max_heat, chart_1))
                }))
                .into_any_element()
        })
        .collect();
    let heatmap_section = usage_section(
        "活动热力图（近 12 周）",
        &format!("共 {} tokens", format_count(heat_total)),
        muted,
        c_border,
        h_flex().gap(px(2.)).p_2().children(week_columns).into_any_element(),
    );

    // 当前项目 / 全局汇总的按模型、按工具拆分（全局另加按项目拆分）。
    let cur_model = cur_project.as_deref().map(|p| usage_stats::by_model(&data.events, Some(p))).unwrap_or_default();
    let cur_tool = cur_project.as_deref().map(|p| usage_stats::by_tool(&data.tools, Some(p))).unwrap_or_default();
    let global_model = usage_stats::by_model(&data.events, None);
    let global_tool = usage_stats::by_tool(&data.tools, None);
    // 展示用截短成目录末段：project_label 是完整 cwd 路径，横向条形图标签区虽然
    // 不会叠字了，但整条路径依然又长又占地方，用不着的前缀部分不如省下来。
    let global_project: Vec<(String, u64)> = usage_stats::by_project(&data.events)
        .into_iter()
        .map(|(path, tokens)| {
            let short = path
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .filter(|s| !s.is_empty())
                .unwrap_or(&path)
                .to_string();
            (short, tokens)
        })
        .collect();

    let cur_project_row = h_flex()
        .gap_3()
        .child(bar_section("当前项目 · 按模型", muted, c_border, chart_1, cur_model))
        .child(bar_section("当前项目 · 按工具", muted, c_border, chart_2, cur_tool));

    let global_row = h_flex()
        .gap_3()
        .child(bar_section("全局 · 按模型", muted, c_border, chart_1, global_model))
        .child(bar_section("全局 · 按工具", muted, c_border, chart_2, global_tool))
        .child(bar_section("全局 · 按项目", muted, c_border, chart_1, global_project));

    div()
        .flex_1()
        .min_h_0()
        .overflow_hidden()
        .flex()
        .flex_col()
        .gap_3()
        .p_3()
        .child(heatmap_section)
        .child(cur_project_row)
        .child(global_row)
}

/// 用量页统一的卡片外壳：标题 + 右侧小字 caption + 内容。
fn usage_section(title: &str, caption: &str, muted: Hsla, border: Hsla, body: AnyElement) -> Div {
    v_flex()
        .flex_1()
        .gap_2()
        .p_3()
        .border_1()
        .border_color(border)
        .rounded(px(8.))
        .child(
            h_flex()
                .justify_between()
                .items_baseline()
                .child(div().font_semibold().child(title.to_string()))
                .child(div().text_xs().text_color(muted).child(caption.to_string())),
        )
        .child(body)
}

/// 用量页的一个「按 X 拆分」柱状图区块；data 为空时显示「无数据」占位。
///
/// 横向条形图（`BarAlignment::Left`）：类目名沿 y 轴一行一个排开，标签区宽度按实际
/// 文字量测预留，不会像竖向柱状图那样把长类目名（`mcp__xxx__yyy` 工具名、项目全路径）
/// 挤在 x 轴上互相叠字看不清。
fn bar_section(title: &str, muted: Hsla, border: Hsla, color: Hsla, data: Vec<(String, u64)>) -> Div {
    // 种类一多（尤其工具名，含各种 mcp__xxx__yyy 前缀）行会太挤，只画头部几项，
    // 其余合并成一根"其他"条。
    let data = cap_top_n(data, 6);
    let total: u64 = data.iter().map(|(_, v)| *v).sum();
    let body = if data.is_empty() {
        div().h(px(180.)).flex().items_center().justify_center().text_color(muted).text_sm().child("无数据")
    } else {
        div().h(px(180.)).child(
            BarChart::new(data)
                .band(|d: &(String, u64)| d.0.clone())
                .value(|d: &(String, u64)| d.1 as f64)
                .fill(move |_, _, _, _| color)
                .alignment(BarAlignment::Left)
                .tick_margin(1),
        )
    };
    usage_section(title, &format!("共 {}", format_count(total)), muted, border, body.into_any_element())
}

/// 大数字加 K/M 单位（千/百万才简化，保留一位小数），token 数 / 调用数这类展示都拿它过一遍。
fn format_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// 只保留输入里的头部 N 项，其余合并成一项"其他"（调用方传入的 data 已经按值降序，
/// 效果就是「前 N 大 + 其余汇总」）。用来防止种类过多时柱状图 x 轴标签挤在一起。
fn cap_top_n(mut data: Vec<(String, u64)>, n: usize) -> Vec<(String, u64)> {
    if data.len() <= n {
        return data;
    }
    let rest: u64 = data.split_off(n).into_iter().map(|(_, v)| v).sum();
    if rest > 0 {
        data.push(("其他".to_string(), rest));
    }
    data
}

/// 热力格颜色：无数据用极淡的底色描边，有数据按 `sqrt(占比)` 映射透明度——避免线性
/// 映射下小数值都挤成看不出深浅的一片。
fn heat_cell_color(v: Option<u64>, max: u64, base: Hsla) -> Hsla {
    match v {
        None => base.opacity(0.0),
        Some(0) => base.opacity(0.08),
        Some(v) => {
            let t = (v as f32 / max as f32).sqrt().clamp(0.25, 1.0);
            base.opacity(t)
        }
    }
}

/// 文件树搜索结果视图：扁平命中列表（替代 query 非空时的树形浏览）。
/// 每项显示相对路径 + 内容命中行预览，点击用 view_file 打开该文件。
fn search_results_view(
    state: &SearchState,
    scroll: &ScrollHandle,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let (muted, fg, hover, accent) = {
        let t = cx.theme();
        (t.muted_foreground, t.foreground, t.accent, t.primary)
    };
    // 顶栏状态：搜索中 / 无结果 / N 项命中(是否截断)。
    let status = if !state.done {
        "搜索中…".to_string()
    } else if state.hits.is_empty() {
        "无匹配".to_string()
    } else if state.truncated {
        format!("命中 {}+ 项（已截断）", state.hits.len())
    } else {
        format!("命中 {} 项", state.hits.len())
    };

    let this = cx.entity();
    let rows: Vec<AnyElement> = state
        .hits
        .iter()
        .enumerate()
        .map(|(i, hit)| {
            let this = this.clone();
            let p = hit.path.clone();
            // 拆出目录前缀与文件名：文件名高亮、目录弱化，便于扫读。
            let (dir_part, name_part) = match hit.rel.rfind('/') {
                Some(idx) => (hit.rel[..=idx].to_string(), hit.rel[idx + 1..].to_string()),
                None => (String::new(), hit.rel.clone()),
            };
            let preview = hit.line.clone();
            div()
                .id(("search-hit", i))
                .flex()
                .flex_col()
                .gap(px(1.0))
                .px_2()
                .py(px(2.0))
                .hover(move |s| s.bg(hover))
                .on_click(move |_ev, window, cx| {
                    this.update(cx, |ws, cx| ws.view_file(p.clone(), window, cx));
                })
                // 第一行：目录（弱）+ 文件名（强）。
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .text_sm()
                        .child(Icon::new(IconName::File).size(px(13.)).text_color(muted))
                        .child(div().text_color(muted).child(dir_part))
                        .child(div().text_color(fg).child(name_part)),
                )
                // 第二行：内容命中的行号 + 行预览（仅内容命中时有）。
                .children(preview.map(|(no, text)| {
                    div()
                        .flex()
                        .gap_1()
                        .pl(px(18.))
                        .text_xs()
                        .text_color(muted)
                        .child(div().text_color(accent).child(format!("{no}")))
                        .child(div().min_w_0().child(text))
                }))
                .into_any_element()
        })
        .collect();

    div()
        .id("search-results")
        .flex_1()
        .min_h_0()
        .flex()
        .flex_col()
        .child(
            div()
                .px_2()
                .py_1()
                .text_xs()
                .text_color(muted)
                .child(status),
        )
        .child(
            div()
                .id("search-results-list")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .pb_1()
                .track_scroll(scroll)
                .vertical_scrollbar(scroll)
                .children(rows),
        )
        .into_any_element()
}

/// 只读缓存的递归收集目录条目（仅进入已展开且已缓存的文件夹）；绝不做任何 fs 调用。
/// 展开了但尚未缓存的目录会被跳过——render 每帧检查并后台补齐，下一帧自动出现。
fn walk_dir_cached(
    dir: &str,
    dir_cache: &HashMap<String, (Instant, Rc<Vec<(String, bool)>>)>,
    expanded: &HashSet<String>,
    depth: usize,
    out: &mut Vec<(usize, String, bool, String, bool)>,
) {
    let Some((_, entries)) = dir_cache.get(dir) else { return };
    for (name, is_dir) in entries.iter() {
        let path = Path::new(dir).join(name).to_string_lossy().to_string();
        let is_expanded = expanded.contains(&path);
        out.push((depth, name.clone(), *is_dir, path.clone(), is_expanded));
        if *is_dir && is_expanded {
            walk_dir_cached(&path, dir_cache, expanded, depth + 1, out);
        }
    }
}

/// 搜索命中数上限：触顶即停并标记截断，避免超大仓遍历/渲染失控。
const SEARCH_HIT_LIMIT: usize = 200;
/// 内容搜索跳过的单文件大小上限（512KB）：更大的多半是数据/构建产物，逐行扫不划算。
const SEARCH_MAX_FILE_BYTES: u64 = 512 * 1024;

/// 后台遍历项目搜索 query（大小写不敏感）：文件名命中或文件内容逐行命中。
/// 跳过 .git/node_modules/target/.DS_Store、隐藏目录、大文件与二进制（含 NUL 字节）。
/// 返回 (命中列表, 是否因触顶截断)；文件名命中排在内容命中前。绝不在此之外做 UI 调用。
fn search_project(root: &str, query: &str) -> (Vec<SearchHit>, bool) {
    let needle = query.to_lowercase();
    let mut name_hits: Vec<SearchHit> = Vec::new();
    let mut content_hits: Vec<SearchHit> = Vec::new();
    let mut stack: Vec<std::path::PathBuf> = vec![std::path::PathBuf::from(root)];
    let root_path = std::path::Path::new(root);
    let mut truncated = false;

    'outer: while let Some(dir) = stack.pop() {
        let mut entries: Vec<std::fs::DirEntry> = match std::fs::read_dir(&dir) {
            Ok(rd) => rd.flatten().collect(),
            Err(_) => continue,
        };
        // 目录序稳定：按名字排序，命中列表才不会每次遍历顺序抖动。
        entries.sort_by_key(|e| e.file_name().to_string_lossy().to_lowercase());
        for e in entries {
            let name = e.file_name().to_string_lossy().to_string();
            // 排除规则与 ensure_dir_listing 对齐，另跳过所有隐藏文件/目录。
            if matches!(name.as_str(), ".git" | "node_modules" | "target" | ".DS_Store")
                || name.starts_with('.')
            {
                continue;
            }
            let path = e.path();
            let is_dir = path.is_dir();
            if is_dir {
                stack.push(path);
                continue;
            }
            let rel = path
                .strip_prefix(root_path)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();
            let abs = path.to_string_lossy().to_string();
            // 文件名命中：直接记一条（不再看内容），命中行留空。
            if name.to_lowercase().contains(&needle) {
                name_hits.push(SearchHit { path: abs, rel, line: None });
                if name_hits.len() + content_hits.len() >= SEARCH_HIT_LIMIT {
                    truncated = true;
                    break 'outer;
                }
                continue;
            }
            // 内容命中：跳过大文件；读文本失败（二进制/非 UTF-8）则跳过。
            if e.metadata().map(|m| m.len()).unwrap_or(u64::MAX) > SEARCH_MAX_FILE_BYTES {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            // 含 NUL 视为二进制，不逐行扫。
            if text.as_bytes().contains(&0) {
                continue;
            }
            if let Some((no, line)) = text
                .lines()
                .enumerate()
                .find(|(_, l)| l.to_lowercase().contains(&needle))
            {
                // 预览行去掉首尾空白并截断，避免超长行撑爆列表。
                let preview: String = line.trim().chars().take(200).collect();
                content_hits.push(SearchHit {
                    path: abs,
                    rel,
                    line: Some((no + 1, preview)),
                });
                if name_hits.len() + content_hits.len() >= SEARCH_HIT_LIMIT {
                    truncated = true;
                    break 'outer;
                }
            }
        }
    }

    name_hits.extend(content_hits);
    (name_hits, truncated)
}

/// 冷→热配色：t∈[0,1]（由排名百分位归一化，见 hotspot_view）从冷蓝经琥珀到警示红。
fn heat_color(t: f32) -> Hsla {
    let t = t.clamp(0.0, 1.0);
    let stops: [(u8, u8, u8); 3] = [(0x2a, 0x41, 0x5c), (0xd9, 0x8a, 0x2e), (0xe0, 0x38, 0x38)];
    let (lo, hi, local_t) = if t < 0.5 { (stops[0], stops[1], t / 0.5) } else { (stops[1], stops[2], (t - 0.5) / 0.5) };
    let lerp = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * local_t).round() as u32;
    let packed = (lerp(lo.0, hi.0) << 16) | (lerp(lo.1, hi.1) << 8) | lerp(lo.2, hi.2);
    rgb(packed).into()
}

/// 热力图视图：squarified treemap——每个矩形是一个近 90 天内改动过的文件。
/// 面积 = 热力分数（改动频率 × 时间衰减，见 hotspot::compute）；颜色则按热力排名百分位
/// 取色（而非分数原始值直接映射）——分数分布是指数衰减的长尾，直接按分数取色会导致只有
/// 前一两名亮红/亮橙、其余瞬间跌成同一片暗色，按排名取色才能让整张图有连续的冷暖梯度。
/// 右上角小圆点额外标出「最近改动」（2 天内）的文件；点击某块直接在文件树里打开对应文件。
fn hotspot_view(
    cwd: Option<String>,
    data: Option<Rc<Vec<hotspot::HotspotEntry>>>,
    cx: &mut Context<Workspace>,
) -> Div {
    let (muted, c_bg) = {
        let t = cx.theme();
        (t.muted_foreground, t.background)
    };
    let Some(root) = cwd else {
        return placeholder_view("无项目目录", muted);
    };
    let Some(entries) = data else {
        return placeholder_view("计算改动热力中…", muted);
    };
    if entries.is_empty() {
        return placeholder_view(
            &format!(
                "近 {} 天无改动记录（非 git 仓库，或近期无改动）",
                hotspot::WINDOW_DAYS
            ),
            muted,
        );
    }

    // 只画热力最高的一批：太多小方块既放不下标签也没有辨识度。
    const MAX_TILES: usize = 80;
    let total = entries.len();
    let shown: Vec<&hotspot::HotspotEntry> = entries.iter().take(MAX_TILES).collect();
    let weights: Vec<f64> = shown.iter().map(|e| e.score.max(1e-6)).collect();
    let rects = hotspot::squarify(&weights);
    let last_ix = shown.len().saturating_sub(1).max(1) as f32;

    let this = cx.entity();
    let tiles: Vec<AnyElement> = shown
        .iter()
        .zip(rects.iter())
        .enumerate()
        .map(|(i, (entry, rect))| {
            // 排名百分位（0 = 最热）取色，与面积（真实分数）解耦，避免长尾把色阶压平。
            let heat = 1.0 - (i as f32 / last_ix);
            let recent = entry.days_since < 2.0;
            let name = Path::new(&entry.rel_path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| entry.rel_path.clone());
            let abs_path = Path::new(&root).join(&entry.rel_path).to_string_lossy().into_owned();
            let this = this.clone();
            let show_label = rect.w > 0.05 && rect.h > 0.06;

            let mut tile = div()
                .id(("hotspot-tile", i))
                .absolute()
                .left(relative(rect.x))
                .top(relative(rect.y))
                .w(relative(rect.w))
                .h(relative(rect.h))
                .overflow_hidden()
                .cursor_pointer()
                .rounded(px(4.))
                .border_2()
                .border_color(c_bg)
                .bg(heat_color(heat))
                .hover(|d| d.border_color(rgb(0x4a9eff)))
                .on_click(move |_ev, window, cx| {
                    this.update(cx, |ws, cx| {
                        ws.view = MainView::Files;
                        ws.view_file(abs_path.clone(), window, cx);
                    });
                });

            // 太小的方块放不下文字，索性留白，靠颜色传达信息即可。
            if show_label {
                tile = tile
                    // 底部暗角渐变：不管方块本身冷暖，文字永远压在深色底上，保证可读。
                    .child(
                        div().absolute().inset_0().bg(linear_gradient(
                            180.,
                            linear_color_stop(rgba(0x00000000), 0.0),
                            linear_color_stop(rgba(0x00000099), 1.0),
                        )),
                    )
                    .child(
                        div()
                            .absolute()
                            .left_0()
                            .right_0()
                            .bottom_0()
                            .p(px(4.))
                            .flex()
                            .flex_col()
                            .gap(px(1.))
                            .text_xs()
                            .text_color(rgb(0xf3f5f8))
                            .child(div().overflow_hidden().whitespace_nowrap().child(name))
                            .child(
                                div()
                                    .text_color(rgba(0xffffffa8))
                                    .child(format!("×{} · {:.0}d", entry.commits, entry.days_since)),
                            ),
                    );
            }
            // 最近改动：右上角一颗小圆点，不影响整体边框/网格的干净观感。
            if recent {
                tile = tile.child(
                    div()
                        .absolute()
                        .top(px(4.))
                        .right(px(4.))
                        .size(px(6.))
                        .rounded_full()
                        .bg(rgb(0x4a9eff))
                        .shadow_sm(),
                );
            }
            tile.into_any_element()
        })
        .collect();

    let caption = if total > MAX_TILES {
        format!(
            "改动热力 · 近 {} 天 · 显示热力最高的 {} / 共 {} 个文件 · 🔵 圆点 = 最近 2 天内改动",
            hotspot::WINDOW_DAYS, MAX_TILES, total
        )
    } else {
        format!(
            "改动热力 · 近 {} 天 · 共 {} 个文件 · 🔵 圆点 = 最近 2 天内改动",
            hotspot::WINDOW_DAYS, total
        )
    };

    div()
        .flex_1()
        .min_h_0()
        .flex()
        .flex_col()
        .child(
            div()
                .px_3()
                .py_2()
                .text_xs()
                .text_color(muted)
                .child(caption),
        )
        .child(
            div()
                .id("hotspot-canvas")
                .flex_1()
                .min_h_0()
                .relative()
                .m_2()
                .children(tiles),
        )
}

/// Git 视图：左侧分支 + 改动文件列表（可点击），右侧显示选中文件的 diff。
fn git_view(
    cwd: Option<String>,
    status: Option<&GitStatusData>,
    git_diff: &Option<GitDiff>,
    split: bool,
    diff_selected: &HashSet<usize>,
    diff_comment_input: Option<&Entity<gpui_component::input::InputState>>,
    files_scroll: &ScrollHandle,
    diff_scroll: &UniformListScrollHandle,
    cx: &mut Context<Workspace>,
) -> Div {
    let (muted, fg, border, accent) = {
        let t = cx.theme();
        (t.muted_foreground, t.foreground, t.border, t.accent)
    };
    let Some(root) = cwd else {
        return placeholder_view("无项目目录", muted);
    };
    // 只读后台缓存（ensure_git_status 负责刷新）：缺失=首次加载中，ok=false=非 git 仓库。
    let Some(data) = status else {
        return placeholder_view("加载改动中…", muted);
    };
    if !data.ok {
        return placeholder_view("不是 git 仓库，或 git 不可用", muted);
    }
    let branch = data.branch.clone();
    let files = data.files.clone();

    let selected = git_diff.as_ref().map(|d| d.path.clone());
    let file_list = if files.is_empty() {
        placeholder_view("工作区干净，无改动 ✓", muted).into_any_element()
    } else {
        div()
            .id("git-files")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(files_scroll)
            .vertical_scrollbar(files_scroll)
            .flex()
            .flex_col()
            .p_1()
            .children(files.into_iter().enumerate().map(|(i, (st, path))| {
                let st_trim = st.trim();
                // 状态标记用 Tag 彩色胶囊：新增=绿 删除=红 修改=黄 未跟踪=灰 其余=蓝。
                let label = if st_trim.is_empty() {
                    "•".to_string()
                } else {
                    st_trim.to_string()
                };
                let status_tag = if st_trim.contains('?') {
                    Tag::secondary()
                } else if st_trim.contains('A') {
                    Tag::success()
                } else if st_trim.contains('D') {
                    Tag::danger()
                } else if st_trim.contains('M') {
                    Tag::warning()
                } else {
                    Tag::info()
                }
                .small()
                .child(label);
                let untracked = st.contains('?');
                let is_sel = selected.as_deref() == Some(path.as_str());
                let (r, p) = (root.clone(), path.clone());
                let row = div()
                    .id(("git", i))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py(px(1.0))
                    .text_sm()
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(|d| d.bg(accent))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_diff(r.clone(), p.clone(), untracked, cx)
                    }))
                    .child(status_tag)
                    .child(div().min_w_0().text_color(fg).child(path));
                // 选中项高亮背景（无 .when，用普通条件分支）。
                if is_sel {
                    row.bg(accent)
                } else {
                    row
                }
            }))
            .into_any_element()
    };

    let left = div()
        .w(px(300.))
        .min_h_0()
        .flex()
        .flex_col()
        .border_r_1()
        .border_color(border)
        .child(
            div()
                .px_3()
                .py_2()
                .text_sm()
                .text_color(fg)
                .border_b_1()
                .border_color(border)
                .child(format!("⎇ {branch}")),
        )
        .child(file_list);

    div()
        .flex_1()
        .min_h_0()
        .flex()
        .child(left)
        .child(git_diff_pane(git_diff, split, diff_selected, diff_comment_input, diff_scroll, cx))
}

/// Git diff 查看面板：uniform_list 虚拟滚动。split 为 true 时并排（左旧右新），
/// 否则统一视图。顶部文件名右侧有「统一/并排」切换按钮。改动行（+/-）可点选，
/// 选中后配合底部评论框「发送到终端」，把反馈批量写进当前激活终端的 PTY。
fn git_diff_pane(
    git_diff: &Option<GitDiff>,
    split: bool,
    diff_selected: &HashSet<usize>,
    diff_comment_input: Option<&Entity<gpui_component::input::InputState>>,
    diff_scroll: &UniformListScrollHandle,
    cx: &mut Context<Workspace>,
) -> Div {
    let (muted, fg, border, accent) = {
        let t = cx.theme();
        (t.muted_foreground, t.foreground, t.border, t.accent)
    };
    match git_diff {
        None => placeholder_view("← 选择改动文件查看 diff", muted),
        Some(d) => {
            let name = d.path.rsplit('/').next().unwrap_or(d.path.as_str()).to_string();
            let lines = d.lines.clone();
            let ws = cx.entity();

            let list = if split {
                let rows = Rc::new(build_split_rows(&lines));
                let count = rows.len();
                let lines2 = lines.clone();
                let sel2 = diff_selected.clone();
                let ws2 = ws.clone();
                uniform_list("git-diff-split", count, move |range, _w, _cx| {
                    range
                        .map(|i| render_split_row(i, &rows[i], &lines2, &sel2, &ws2))
                        .collect::<Vec<_>>()
                })
            } else {
                let count = lines.len();
                let sel2 = diff_selected.clone();
                let ws2 = ws.clone();
                uniform_list("git-diff", count, move |range, _w, _cx| {
                    range
                        .map(|i| render_diff_line(i, &lines[i], sel2.contains(&i), &ws2))
                        .collect::<Vec<_>>()
                })
            }
            .flex_1()
            .min_h_0()
            .w_full()
            .py_1()
            .font_family(terminal_view::FONT_FAMILY)
            .text_sm()
            .track_scroll(diff_scroll);

            // 「统一 / 并排」切换按钮。
            let toggle = div()
                .id("diff-split-toggle")
                .px_2()
                .py(px(1.0))
                .text_xs()
                .rounded_sm()
                .cursor_pointer()
                .text_color(fg)
                .bg(accent)
                .hover(|d| d.opacity(0.8))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.diff_split = !this.diff_split;
                    cx.notify();
                }))
                .child(if split { "并排 ⇄" } else { "统一 ☰" }.to_string());

            div()
                .flex_1()
                .min_w_0()
                .min_h_0()
                .flex()
                .flex_col()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py_1()
                        .text_sm()
                        .text_color(muted)
                        .border_b_1()
                        .border_color(border)
                        .child(div().flex_1().min_w_0().child(name))
                        .child(toggle),
                )
                // 包一层 relative 容器承载 gpui-component 竖向滚动条（覆盖在 diff 上）。
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .relative()
                        .flex()
                        .flex_col()
                        .child(list)
                        .vertical_scrollbar(diff_scroll),
                )
                .child(diff_comment_bar(diff_selected, diff_comment_input, cx))
        }
    }
}

/// 交互式 diff 底部工具条：已选行数提示 + 评论输入框 + 「发送到终端」按钮，
/// 发送目标固定是当前激活的终端标签（不跨项目匹配，简单直接）。
fn diff_comment_bar(
    selected: &HashSet<usize>,
    input: Option<&Entity<gpui_component::input::InputState>>,
    cx: &mut Context<Workspace>,
) -> Div {
    let (muted, border) = {
        let t = cx.theme();
        (t.muted_foreground, t.border)
    };
    let n = selected.len();
    let can_send = n > 0;
    let hint =
        if n == 0 { "点选中改动行（+/-），可选写评论，发给当前终端".to_string() } else { format!("已选 {n} 行") };
    let ws = cx.entity();

    div()
        .flex()
        .items_center()
        .gap_2()
        .px_3()
        .py_2()
        .border_t_1()
        .border_color(border)
        .child(div().flex_none().text_xs().text_color(muted).child(hint))
        .children(input.map(|state| div().flex_1().min_w_0().child(Input::new(state).small())))
        .child(
            Button::new("diff-send")
                .small()
                .label("发送到终端")
                .disabled(!can_send)
                .on_click(move |_ev, window, cx| {
                    ws.update(cx, |this, cx| this.send_diff_comments(window, cx));
                }),
        )
}

/// 把 git diff 文本解析成结构化的行：从 @@ 段头取起始行号，逐行推进旧/新行号，
/// 并按前缀判定类型、剥掉 +/-/空格前缀。空 diff 给一句提示。
fn parse_diff(text: &str) -> Vec<DiffLine> {
    let mk = |old_ln, new_ln, kind, text: &str| DiffLine {
        old_ln,
        new_ln,
        kind,
        text: text.to_string(),
        segments: None,
    };
    if text.trim().is_empty() {
        return vec![mk(None, None, DiffKind::Meta, "（无差异）")];
    }
    let mut old_ln = 0u32;
    let mut new_ln = 0u32;
    let mut out = Vec::new();
    for line in text.lines() {
        if line.starts_with("@@") {
            let (o, n) = parse_hunk(line);
            old_ln = o;
            new_ln = n;
            out.push(mk(None, None, DiffKind::Hunk, line));
        } else if line.starts_with("+++")
            || line.starts_with("---")
            || line.starts_with("diff ")
            || line.starts_with("index ")
            || line.starts_with("new file")
            || line.starts_with("deleted file")
            || line.starts_with("similarity")
            || line.starts_with("rename ")
        {
            out.push(mk(None, None, DiffKind::Meta, line));
        } else if let Some(t) = line.strip_prefix('+') {
            out.push(mk(None, Some(new_ln), DiffKind::Add, t));
            new_ln += 1;
        } else if let Some(t) = line.strip_prefix('-') {
            out.push(mk(Some(old_ln), None, DiffKind::Del, t));
            old_ln += 1;
        } else {
            // 上下文行（以空格开头，或 diff 末尾的空行）。
            let t = line.strip_prefix(' ').unwrap_or(line);
            out.push(mk(Some(old_ln), Some(new_ln), DiffKind::Context, t));
            old_ln += 1;
            new_ln += 1;
        }
    }
    mark_inline(&mut out);
    out
}

/// 后处理：对每组「连续删行紧跟连续增行」按顺序逐行配对，做字符级 inline diff，
/// 把两侧变化的具体片段标出来（存进各自的 segments）。行太长则跳过（避免 O(n·m)）。
fn mark_inline(lines: &mut [DiffLine]) {
    let n = lines.len();
    let mut i = 0;
    while i < n {
        if lines[i].kind != DiffKind::Del {
            i += 1;
            continue;
        }
        let del_start = i;
        while i < n && lines[i].kind == DiffKind::Del {
            i += 1;
        }
        let add_start = i;
        while i < n && lines[i].kind == DiffKind::Add {
            i += 1;
        }
        let pairs = (add_start - del_start).min(i - add_start);
        for k in 0..pairs {
            let (di, ai) = (del_start + k, add_start + k);
            let (dt, at) = (lines[di].text.clone(), lines[ai].text.clone());
            if dt.len() + at.len() > 4000 {
                continue; // 超长行不做行内 diff
            }
            let (dseg, aseg) = inline_segments(&dt, &at);
            lines[di].segments = Some(dseg);
            lines[ai].segments = Some(aseg);
        }
    }
}

/// 对一对 (旧行, 新行) 做字符级 diff，分别产出两侧的 (片段, 是否变化) 列表。
/// 旧行里被删除的字符标变化，新行里新增的字符标变化，相等部分两侧都不标。
fn inline_segments(old: &str, new: &str) -> (Vec<(String, bool)>, Vec<(String, bool)>) {
    let diff = similar::TextDiff::from_chars(old, new);
    let mut olds: Vec<(String, bool)> = Vec::new();
    let mut news: Vec<(String, bool)> = Vec::new();
    // 把相邻同状态的字符合并成段，减少 span 数量。
    let push = |v: &mut Vec<(String, bool)>, ch: &str, changed: bool| {
        if let Some(last) = v.last_mut() {
            if last.1 == changed {
                last.0.push_str(ch);
                return;
            }
        }
        v.push((ch.to_string(), changed));
    };
    for change in diff.iter_all_changes() {
        let val = change.value();
        match change.tag() {
            similar::ChangeTag::Equal => {
                push(&mut olds, val, false);
                push(&mut news, val, false);
            }
            similar::ChangeTag::Delete => push(&mut olds, val, true),
            similar::ChangeTag::Insert => push(&mut news, val, true),
        }
    }
    (olds, news)
}

/// 从 hunk 头 `@@ -a,b +c,d @@` 解析出旧/新起始行号（a、c）。
fn parse_hunk(line: &str) -> (u32, u32) {
    let (mut old, mut new) = (0u32, 0u32);
    for tok in line.split_whitespace() {
        if let Some(s) = tok.strip_prefix('-') {
            old = s.split(',').next().and_then(|x| x.parse().ok()).unwrap_or(0);
        } else if let Some(s) = tok.strip_prefix('+') {
            new = s.split(',').next().and_then(|x| x.parse().ok()).unwrap_or(0);
        }
    }
    (old, new)
}

/// 渲染一行 diff：左侧色条 + 旧/新行号槽 + 文本；整行按类型上淡背景。
/// 若有 segments（行内 diff 结果），变化片段再叠一层更深的底色。
/// 增/删行（i 为 GitDiff.lines 下标）可点选：选中态描边，点击切给 Workspace
/// 的 toggle_diff_line，配合底部评论框批量发给当前终端。
fn render_diff_line(i: usize, l: &DiffLine, selected: bool, ws: &Entity<Workspace>) -> Stateful<Div> {
    let (fg, bg, bar, hl) = diff_colors(l.kind);
    let gutter = |n: Option<u32>| {
        div()
            .w(px(44.))
            .px_1()
            .flex()
            .justify_end()
            .text_color(rgb(0x4a5178))
            .child(n.map(|v| v.to_string()).unwrap_or_default())
    };

    let mut row = div()
        .id(("diff-line", i))
        .flex()
        .items_center()
        .h(px(FILE_LINE_H))
        .whitespace_nowrap();
    if let Some(b) = bg {
        row = row.bg(b);
    }
    if matches!(l.kind, DiffKind::Add | DiffKind::Del) {
        row = row.cursor_pointer();
        if selected {
            row = row.border_2().border_color(rgb(0x4a9eff));
        }
        let ws = ws.clone();
        row = row.on_click(move |_ev, _window, cx| {
            ws.update(cx, |this, cx| this.toggle_diff_line(i, cx));
        });
    }
    row
        // 左侧色条：增/删才有，其它用等宽透明占位保持对齐。
        .child(match bar {
            Some(c) => div().w(px(2.)).h_full().bg(c),
            None => div().w(px(2.)).h_full(),
        })
        .child(gutter(l.old_ln))
        .child(gutter(l.new_ln))
        .child(diff_text_area(l, fg, hl))
}

/// 并排视图的一行：Both = 左(旧侧)/右(新侧)各一行（None 为空侧占位）；
/// Full = 横跨整宽的 hunk/meta 行。存的是 GitDiff.lines 里的索引。
enum SplitRow {
    Both(Option<usize>, Option<usize>),
    Full(usize),
}

/// 把线性的 diff 行重排成并排的行对：上下文左右对齐；一组删/增按顺序配对，
/// 数量不等时多出的一侧留空；纯新增（无对应删行）左侧空。
fn build_split_rows(lines: &[DiffLine]) -> Vec<SplitRow> {
    let n = lines.len();
    let mut rows = Vec::new();
    let mut i = 0;
    while i < n {
        match lines[i].kind {
            DiffKind::Hunk | DiffKind::Meta => {
                rows.push(SplitRow::Full(i));
                i += 1;
            }
            DiffKind::Context => {
                rows.push(SplitRow::Both(Some(i), Some(i)));
                i += 1;
            }
            DiffKind::Del => {
                let ds = i;
                while i < n && lines[i].kind == DiffKind::Del {
                    i += 1;
                }
                let de = i;
                let as_ = i;
                while i < n && lines[i].kind == DiffKind::Add {
                    i += 1;
                }
                let ae = i;
                let (dn, an) = (de - ds, ae - as_);
                for k in 0..dn.max(an) {
                    let l = (k < dn).then_some(ds + k);
                    let r = (k < an).then_some(as_ + k);
                    rows.push(SplitRow::Both(l, r));
                }
            }
            DiffKind::Add => {
                // 纯新增块（前面没有删行）：左侧空、右侧逐行。
                while i < n && lines[i].kind == DiffKind::Add {
                    rows.push(SplitRow::Both(None, Some(i)));
                    i += 1;
                }
            }
        }
    }
    rows
}

/// diff 行类型 → (前景, 整行背景, 左色条, 行内变化片段深底)。
fn diff_colors(kind: DiffKind) -> (Rgba, Option<Rgba>, Option<Rgba>, Rgba) {
    match kind {
        DiffKind::Add => (rgb(0xb5e08a), Some(rgb(0x16261a)), Some(rgb(0x4ba14b)), rgb(0x2f6b34)),
        DiffKind::Del => (rgb(0xf7a3ae), Some(rgb(0x2a1620)), Some(rgb(0xc75c6a)), rgb(0x7a2836)),
        DiffKind::Context => (rgb(0xc0caf5), None, None, rgb(0)),
        DiffKind::Hunk => (rgb(0x7dcfff), Some(rgb(0x16202e)), None, rgb(0)),
        DiffKind::Meta => (rgb(0x565f89), None, None, rgb(0)),
    }
}

/// 文本区（flex_1）：有 segments 就拆成多段（变化段上深底），否则整行一段。
fn diff_text_area(l: &DiffLine, fg: Rgba, hl: Rgba) -> Div {
    match &l.segments {
        Some(segs) => div().flex_1().px_2().text_color(fg).flex().children(segs.iter().map(
            |(s, changed)| {
                let span = div().child(s.clone());
                if *changed {
                    span.bg(hl).rounded_sm()
                } else {
                    span
                }
            },
        )),
        None => div()
            .flex_1()
            .px_2()
            .text_color(fg)
            .child(if l.text.is_empty() { "\u{00a0}".to_string() } else { l.text.clone() }),
    }
}

/// 渲染并排的半行（左或右，flex_1）。idx 为 None 时是空侧占位（暗底）。
/// left=true 用旧行号，否则用新行号。ri 是并排行在 rows 里的下标，只用来拼 id
/// （idx 本身在 Both(None, Some(i)) 这类情况下左右可能撞号，ri+left 才唯一）。
/// 增/删行同 render_diff_line 一样可点选，选中态描边。
fn render_half(
    ri: usize,
    idx: Option<usize>,
    left: bool,
    lines: &[DiffLine],
    selected: &HashSet<usize>,
    ws: &Entity<Workspace>,
) -> Stateful<Div> {
    // overflow_hidden：长行必须裁剪在本半区内，否则会溢出盖住另一半，并排就糊了。
    let base = div()
        .id(("diff-half", ri * 2 + left as usize))
        .flex_1()
        .min_w_0()
        .overflow_hidden()
        .flex()
        .items_center()
        .h_full();
    let Some(i) = idx else {
        // 空侧：略暗的底表示「此侧无对应行」。
        return base.bg(rgb(0x101218));
    };
    let l = &lines[i];
    let (fg, bg, bar, hl) = diff_colors(l.kind);
    let ln = if left { l.old_ln } else { l.new_ln };
    let mut row = base;
    if let Some(b) = bg {
        row = row.bg(b);
    }
    if matches!(l.kind, DiffKind::Add | DiffKind::Del) {
        row = row.cursor_pointer();
        if selected.contains(&i) {
            row = row.border_2().border_color(rgb(0x4a9eff));
        }
        let ws = ws.clone();
        row = row.on_click(move |_ev, _window, cx| {
            ws.update(cx, |this, cx| this.toggle_diff_line(i, cx));
        });
    }
    row.child(match bar {
        Some(c) => div().w(px(2.)).h_full().bg(c),
        None => div().w(px(2.)).h_full(),
    })
    .child(
        div()
            .w(px(44.))
            .px_1()
            .flex()
            .justify_end()
            .text_color(rgb(0x4a5178))
            .child(ln.map(|v| v.to_string()).unwrap_or_default()),
    )
    .child(diff_text_area(l, fg, hl))
}

/// 渲染并排视图的一行。ri 是该行在 rows 里的下标，透传给 render_half 拼 id。
fn render_split_row(
    ri: usize,
    row: &SplitRow,
    lines: &[DiffLine],
    selected: &HashSet<usize>,
    ws: &Entity<Workspace>,
) -> Div {
    match row {
        SplitRow::Full(i) => {
            let l = &lines[*i];
            let (fg, bg, _, _) = diff_colors(l.kind);
            let mut d = div()
                .flex()
                .items_center()
                .h(px(FILE_LINE_H))
                .w_full()
                .overflow_hidden()
                .whitespace_nowrap();
            if let Some(b) = bg {
                d = d.bg(b);
            }
            d.child(div().px_2().text_color(fg).child(l.text.clone()))
        }
        SplitRow::Both(l, r) => div()
            .flex()
            .items_center()
            .h(px(FILE_LINE_H))
            // w_full 关键：容器占满整宽，两个 flex_1 半区才会真正各占一半；
            // 否则容器 hug content，grow 失效，空侧塌成 0 宽、内容顶到最左。
            .w_full()
            .whitespace_nowrap()
            .child(render_half(ri, *l, true, lines, selected, ws))
            .child(div().w(px(1.)).h_full().bg(rgb(0x2a2e3d))) // 中缝分隔
            .child(render_half(ri, *r, false, lines, selected, ws)),
    }
}

/// 文件查看的固定行高（供 diff 视图 uniform_list 虚拟滚动，需每行等高）。
const FILE_LINE_H: f32 = 20.0;

/// 文件扩展名 → Editor 的语法高亮语言名。gpui-component 的 `Language::from_name`
/// 本身就认常见扩展名（"rs"/"py"/"md" 等），这里只需把扩展名传过去；识别不了的
/// 名字组件会自动回退成纯文本，不会 panic。没有扩展名的文件（Makefile 等）退而
/// 用文件名本身（能命中 "makefile" 这类按文件名匹配的语言）。
fn editor_language_for_path(path: &str) -> String {
    let p = Path::new(path);
    match p.extension().and_then(|e| e.to_str()) {
        Some(ext) => ext.to_lowercase(),
        None => p.file_name().and_then(|n| n.to_str()).unwrap_or("text").to_lowercase(),
    }
}

/// 文件内容查看/编辑面板：直接用 gpui-component 的 Editor（InputState code_editor
/// 模式），自带语法高亮、行号、搜索、大文件下的增量编辑，不用再自己管虚拟滚动。
fn file_content_pane(open_file: &Option<OpenFile>, cx: &mut Context<Workspace>) -> Div {
    let (muted, border, warning) = {
        let t = cx.theme();
        (t.muted_foreground, t.border, t.warning)
    };
    match open_file {
        None => placeholder_view("← 从左侧选择文件查看内容", muted),
        Some(of) => {
            let name = of.path.rsplit('/').next().unwrap_or(of.path.as_str()).to_string();
            let dirty = of.editor.read(cx).value().to_string() != *of.saved_content;
            let header = h_flex()
                .items_center()
                .gap_2()
                .px_3()
                .py_1()
                .border_b_1()
                .border_color(border)
                .child(div().text_sm().text_color(muted).child(name))
                // 未保存改动：文件名后一个小圆点，Cmd+S 保存后消失。
                .when(dirty, |el| {
                    el.child(div().size(px(6.)).rounded_full().bg(warning))
                })
                // 保存失败 / 不支持保存的提示。
                .children(
                    of.save_error.clone().map(|msg| div().text_xs().text_color(warning).child(msg)),
                );
            div()
                .flex_1()
                .min_w_0()
                .min_h_0()
                .flex()
                .flex_col()
                .child(header)
                .child(div().flex_1().min_h_0().child(Input::new(&of.editor).h_full()))
        }
    }
}

/// 侧栏底部胶囊按钮：图标 + 文字，果冻感（tint 底 + 细白边 + hover 提亮），
/// 与总览页设计语言一致。accent = 品牌蓝主按钮，否则中性次按钮。
/// （组件 Button 的 ghost 在暗色下 hover 几乎不可见，这里自绘保证反馈明显。）
fn tool_button(
    id: &'static str,
    icon: IconName,
    label: &'static str,
    accent: bool,
    cx: &mut Context<Workspace>,
    handler: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
) -> Stateful<Div> {
    let (fg, bg, bg_hover): (Hsla, Hsla, Hsla) = if accent {
        (rgb(0x8fc7ff).into(), rgba(0x4a9eff24).into(), rgba(0x4a9eff40).into())
    } else {
        (
            cx.theme().sidebar_foreground,
            rgba(0xffffff0a).into(),
            rgba(0xffffff1f).into(),
        )
    };
    div()
        .id(id)
        .flex_1()
        .flex()
        .items_center()
        .justify_center()
        .gap_1()
        .py(px(5.))
        .rounded_lg()
        .bg(bg)
        .border_1()
        .border_color(rgba(0xffffff12))
        .text_sm()
        .text_color(fg)
        .hover(move |s| s.bg(bg_hover))
        .child(Icon::new(icon).size(px(14.)))
        .child(label)
        .on_click(cx.listener(move |this, _ev, window, cx| handler(this, window, cx)))
}

/// 「打开项目」按钮：弹选择框选目录，在其中开新标签。
fn open_project_button(cx: &mut Context<Workspace>) -> Stateful<Div> {
    tool_button("open-project", IconName::Folder, "打开项目", true, cx, |this, _w, cx| {
        this.open_project(cx)
    })
}

/// 「新建终端」按钮：不用先选项目，直接落在 $HOME 开/切一个终端（原先是顶部
/// 独立入口，挪到跟「打开项目」并排，都是"开个新地方干活"，归一块好找）。
fn scratch_terminal_button(cx: &mut Context<Workspace>) -> Stateful<Div> {
    tool_button(
        "scratch-terminal",
        IconName::SquareTerminal,
        "新建终端",
        false,
        cx,
        |this, window, cx| this.activate_or_new_scratch(window, cx),
    )
}

/// 当前工作目录字符串。
fn current_dir() -> Option<String> {
    std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
}

/// 临时终端的落脚目录：固定用 $HOME，跟任何项目区分开、且多个临时终端共享同一
/// 目录字符串，侧栏才能按 cwd 分组把它们聚成一组（见 render 里的 `is_scratch_cwd`）。
fn scratch_dir() -> Option<String> {
    dirs::home_dir().and_then(|p| p.to_str().map(String::from))
}

/// cwd → 侧栏项目分组显示名，统一取目录末段——scratch_dir 就是 $HOME，末段天然是
/// 用户名（比如 c.chen），不用再特判成「临时终端」这种跟其他项目组风格不一致的名字。
/// Workspace::project_groups（侧栏渲染）和拖拽排序（找会话/插入点归属的项目）共用。
fn project_name_for_cwd(cwd: &str) -> String {
    cwd.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("项目")
        .to_string()
}

/// file:// URL → 本地路径（percent 解码，支持中文 / 空格目录名）。
fn file_url_to_path(url: &str) -> Option<std::path::PathBuf> {
    let rest = url.strip_prefix("file://")?;
    // 跳过可能的 host 段（file://localhost/…），从首个 '/' 起才是路径。
    let path = &rest[rest.find('/')?..];
    let b = path.as_bytes();
    let mut bytes = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).ok()?, 16) {
                bytes.push(v);
                i += 3;
                continue;
            }
        }
        bytes.push(b[i]);
        i += 1;
    }
    Some(std::path::PathBuf::from(String::from_utf8(bytes).ok()?))
}

/// 独立设置窗口的根 view：只是个薄壳，真正状态都还在传进来的 Workspace 实体上，
/// 每次渲染转手调 `render_settings_content`，天然跟主窗口设置页保持同步。
struct SettingsWindow {
    workspace: Entity<Workspace>,
}

impl Render for SettingsWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.workspace.update(cx, |ws, cx| ws.render_settings_content(cx))
    }
}

/// 独立设置窗口的单例句柄：已经开着就聚焦复用，避免重复开出好几扇一样的窗口。
struct SettingsWindowHandle(Option<WindowHandle<Root>>);
impl Global for SettingsWindowHandle {}

/// 独立用量窗口的根 view：同 [`SettingsWindow`]，薄壳转手调 `render_usage_page`。
struct UsageWindow {
    workspace: Entity<Workspace>,
}

impl Render for UsageWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.workspace.update(cx, |ws, cx| ws.render_usage_page(cx))
    }
}

/// 独立用量窗口的单例句柄，同 [`SettingsWindowHandle`]。
struct UsageWindowHandle(Option<WindowHandle<Root>>);
impl Global for UsageWindowHandle {}

/// 开一扇主工作台窗口（Workspace + Root 包装），返回其 weak 引用。
/// 首启和「点 Dock 图标重开」共用这一份：`Workspace::new` 本来就会从存档 + smeltd
/// 重新拼出会话布局，跟正常重启应用效果一致。
fn open_workspace_window(cx: &mut App, window_bg: WindowBackgroundAppearance) -> WeakEntity<Workspace> {
    let window_options = WindowOptions {
        // 透明标题栏：红绿灯浮在内容上，拖拽 / 双击最大化由自定义 TitleBar 接管。
        titlebar: Some(TitleBar::title_bar_options()),
        // 透明/模糊背景（跟随外观设置；终端底色带 alpha 时桌面透出）。
        window_background: window_bg,
        ..Default::default()
    };
    let mut workspace = None;
    cx.open_window(window_options, |window, cx| {
        // 界面文字（侧边栏/标签页/状态栏等）用的都是 text_xs/text_sm 这类相对 rem
        // 单位，默认 rem_size=16px 偏小；这里统一调大，全局跟着等比例放大，不用
        // 逐个改 .text_xs()/.text_sm()。终端内容本身的字号另由 terminal_view.rs
        // 的 FONT_PX 控制，不受这个影响。
        window.set_rem_size(px(18.));
        let view = cx.new(|cx| Workspace::new(cx));
        workspace = Some(view.clone());
        // 顶层视图必须包一层 Root（组件库的主题/遮罩系统要求）。
        cx.new(|cx| Root::new(view, window, cx))
    })
    .expect("打开窗口失败");
    workspace.expect("回调里一定会设置 workspace").downgrade()
}

fn main() {
    // with_assets 注册组件库图标资源，Sidebar 的 IconName svg 才能渲染。
    let app = gpui_platform::application().with_assets(gpui_component_assets::Assets);
    // Dock / Finder「打开」投递的 file:// URL（拖文件夹到 Dock 图标、右键用 Smelt 打开）。
    // 回调里没有 cx，经 channel 转发；unbounded 会缓存首启动时窗口建好前到达的 URL。
    let (url_tx, url_rx) = smol::channel::unbounded::<Vec<String>>();
    app.on_open_urls(move |urls| {
        let _ = url_tx.send_blocking(urls);
    });
    // 菜单栏常驻图标点击：见 status_item.rs 顶部注释，回调发生在纯 AppKit 层
    // （没有 GPUI 的 cx），一样经 channel 转发到下面 run() 里 drain。
    let (status_tx, status_rx) = smol::channel::unbounded::<()>();

    // 当前存活的主窗口（weak，随窗口关闭自然失效）。首启时在 run() 里写入；
    // URL 投递循环和「点 Dock 图标重开」都读它判断当前有没有主窗口。
    // on_reopen 得在 run() 之前挂在 Application builder 上（跟 on_open_urls 一样），
    // 但它触发时 run() 早已跑起来，Rc 到时候已经被 run() 里的首启逻辑填过了。
    let current_ws: Rc<RefCell<Option<WeakEntity<Workspace>>>> = Rc::new(RefCell::new(None));
    {
        let current_ws = current_ws.clone();
        // 点 Dock 图标 / 双击程序图标重开：GPUI 只在系统判定「没有可见窗口」时才会调这个
        // 回调（宠物浮窗一直挂着，是否会被系统计入可见窗口未经验证，这里做好兜底：
        // 主窗口还活着就什么都不做，只有真的没了才重新开一扇）。
        app.on_reopen(move |cx| {
            let alive = current_ws.borrow().as_ref().is_some_and(|w| w.upgrade().is_some());
            if !alive {
                let window_bg = cx
                    .try_global::<Appearance>()
                    .map(|a| a.window_bg())
                    .unwrap_or(WindowBackgroundAppearance::Opaque);
                let ws = open_workspace_window(cx, window_bg);
                *current_ws.borrow_mut() = Some(ws);
            }
        });
    }

    app.run(move |cx| {
        // 用任何 gpui-component 功能前必须先初始化。
        gpui_component::init(cx);
        // 内嵌 Nerd Font 图标 fallback 字体：主字体查不到的图标码位会落到这里
        // （terminal_view::terminal_font），不必强求用户机器装了打过 Nerd Font 补丁的字体。
        cx.text_system()
            .add_fonts(vec![std::borrow::Cow::Borrowed(
                include_bytes!("../../../assets/fonts/SymbolsNerdFontMono-Regular.ttf").as_slice(),
            )])
            .expect("加载图标 fallback 字体失败");
        // 应用菜单栏：macOS 顶部「Smelt」菜单，含「设置… ⌘,」+「退出 Smelt ⌘Q」
        // （跟齿轮图标一样开独立设置窗口，符合 mac 惯例——系统偏好设置一般都在这）。
        cx.bind_keys([
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("cmd-,", OpenSettings, None),
        ]);
        cx.set_menus(vec![Menu::new("Smelt").items([
            MenuItem::action("设置…", OpenSettings),
            MenuItem::Separator,
            MenuItem::action("退出 Smelt", Quit),
        ])]);

        // 外观设置：读盘设为全局单例，据此确定窗口背景外观（透明 / 模糊）+ 明暗主题
        // （默认深色，与终端配色一致；用户可在设置页切换，选择会持久化）。
        let appearance = load_appearance();
        let window_bg = appearance.window_bg();
        Theme::change(appearance.theme_mode, None, cx);
        cx.set_global(appearance);
        cx.set_global(load_launch_config());

        // 桌面宠物：配置 + 播报邮箱 + LLM 大脑配置（跨窗口全局单例），再开独立透明浮窗。
        cx.set_global(pet::load_pet_config());
        cx.set_global(pet::PetMailbox::default());
        cx.set_global(agent::load_llm_config());
        pet::open_pet_window(cx);
        // 菜单栏常驻图标：点击唤出/前置主窗口，见 status_item.rs。
        status_item::setup(status_tx);

        // 首启主窗口，记入 current_ws（reopen 回调 / URL 投递循环都靠它判断当前主窗口）。
        *current_ws.borrow_mut() = Some(open_workspace_window(cx, window_bg));

        // 消费 Dock / Finder 投递的目录：每个开一个会话（文件取父目录）。常驻到应用退出，
        // 不因主窗口一度被关掉而停——重开窗口后应继续能接文件投递。
        let current_ws_status = current_ws.clone();
        cx.spawn(async move |cx| {
            while let Ok(urls) = url_rx.recv().await {
                let paths: Vec<std::path::PathBuf> =
                    urls.iter().filter_map(|u| file_url_to_path(u)).collect();
                if paths.is_empty() {
                    continue;
                }
                let ws = current_ws.borrow().clone();
                if let Some(ws) = ws {
                    let _ = ws.update(cx, |ws, cx| ws.open_paths(&paths, cx));
                }
            }
        })
        .detach();

        // 菜单栏图标点击：主窗口还活着就前置 app，没了就跟 on_reopen 一样重开一扇。
        cx.spawn(async move |cx| {
            while status_rx.recv().await.is_ok() {
                let alive = current_ws_status.borrow().as_ref().is_some_and(|w| w.upgrade().is_some());
                if alive {
                    status_item::activate_app();
                } else {
                    cx.update(|cx| {
                        let window_bg = cx
                            .try_global::<Appearance>()
                            .map(|a| a.window_bg())
                            .unwrap_or(WindowBackgroundAppearance::Opaque);
                        let ws = open_workspace_window(cx, window_bg);
                        *current_ws_status.borrow_mut() = Some(ws);
                    });
                }
            }
        })
        .detach();
    });
}
