//! smelt 工作台 —— 基于 gpui-component 的桌面窗口。
//!
//! Workspace 管理多个终端标签（TerminalView）：顶部标签栏切换 / 新建 / 关闭，
//! 下方渲染当前活动终端。每个终端各自独立（PTY、IME、滚动、resize）。
//!
//! 运行： cargo run --bin workspace

mod agent;
mod claude_memory;
mod dock;
mod file_tree;
mod git_panel;
mod hotspot;
mod json_store;
mod mem_usage;
mod pet;
mod session_history;
mod settings;
mod status_item;
mod tasks;
mod terminal;
mod terminal_view;
mod updater;
mod usage_stats;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpui::*;
use gpui::prelude::FluentBuilder;
use gpui::InteractiveElement;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::sidebar::{
    Sidebar, SidebarCollapsible, SidebarGroup, SidebarItem, SidebarMenu, SidebarMenuItem,
};
use gpui_component::color_picker::ColorPickerState;
use gpui_component::input::Input;
use gpui_component::list::{List, ListDelegate, ListEvent, ListItem, ListState};
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use gpui_component::badge::Badge;
use gpui_component::notification::Notification;
use gpui_component::slider::SliderState;
use gpui_component::table::TableState;
use gpui_component::resizable::{
    h_resizable, resizable_panel, v_resizable, ResizablePanelEvent, ResizableState,
};
use gpui_component::tab::{Tab, TabBar};
use gpui_component::*;
use notify::RecommendedWatcher;
use terminal_view::TerminalView;

use file_tree::{
    file_content_pane, file_tree, search_results_view, DeleteFileTarget, OpenFile, SearchState,
};
use git_panel::{
    git_view, main_repo_root_from_common_dir, repo_label_from_common_dir, run_git, BranchList,
    DeleteWorktreeTarget, GitDiff, GitStatusData, NewWorktreeTarget, RepoInfo,
};
use hotspot::hotspot_view;
use session_history::{history_view, HistoryListState, HistoryPane, SessionHistoryDelegate};
use settings::{
    active_launch_entries, icon_for_launch_command, load_appearance, load_launch_config, Appearance,
    LlmInputs,
};
use usage_stats::format_count;


// Cmd+Q 退出的应用级 action（gpui 无默认菜单栏，需自建菜单栏 + 键位绑定）。
gpui::actions!(
    smelt,
    [Quit, OpenSettings, CheckForUpdate, SendSelectionToTerminal, NewTask]
);

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

/// 主区视图：会话总览 / 任务总览 / 终端 / 文件树 / Git / 热力图 / 历史。
/// 任务总览与会话总览并列独立页，互不混内容；入口在侧栏「任务」。
#[derive(Clone, Copy, PartialEq)]
enum MainView {
    Overview,
    /// 任务总览（卡片网格，对齐会话总览交互，内容只含任务）。
    Tasks,
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
    /// 用户手动改过的会话名（侧栏右键「重命名」）；None = 用下面 title() 的自动推导。
    custom_title: Option<String>,
}

impl Session {
    /// 单终端会话。
    fn single(view: Entity<TerminalView>) -> Self {
        Self { layout: Pane::Leaf(view.clone()), active: view, custom_title: None }
    }

    /// 会话标题：用户重命名过就用那个；否则仅当终端标题是 Claude Code 风格（✳ 或
    /// Braille spinner 开头）时取它的任务名，再否则回退 cwd 末段——避免把普通 shell 的
    /// user@host:path 标题当任务名。
    fn title(&self, cx: &App) -> String {
        self.custom_title.clone().unwrap_or_else(|| pane_auto_title(&self.active, cx))
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

/// 状态点颜色：等审批(红) > 需要处理(橙) > 运行中(蓝) > 已完成未读(绿)。
/// 会话行状态点、侧栏展开出的分屏 pane 状态点共用同一套配色。
fn status_color(status: AgentStatus) -> gpui::Rgba {
    match status {
        AgentStatus::WaitingApproval => rgb(0xef4444),
        AgentStatus::NeedsAttention => rgb(0xf59e0b),
        AgentStatus::Running => rgb(0x4a9eff),
        AgentStatus::Done => rgb(0x22c55e),
        AgentStatus::Idle => unreachable!(),
    }
}

/// 设置窗口 pages 列表里的页下标——调整 `render_settings_content` 末尾那个
/// `pages(vec![...])` 的顺序时必须同步改这里，否则应用菜单「检查更新…」会跳错页。
const SETTINGS_PAGE_APPEARANCE: usize = 0;
const SETTINGS_PAGE_UPDATE: usize = 3;

/// 重命名弹窗改的是谁：侧栏会话行改整个会话的名，分屏子行只改那一个 pane 的名。
#[derive(Clone)]
enum RenameTarget {
    Session(usize),
    Pane(Entity<TerminalView>),
}

/// 单个终端 pane 自动推导的标题：优先 agent 上报的任务名，其次快捷启动显示名，
/// 再回退建终端时的 cwd 名。不看用户改的名字——`Session::title` 靠它拿活动 pane
/// 的「客观」标题。
fn pane_auto_title(view: &Entity<TerminalView>, cx: &App) -> String {
    let t = view.read(cx);
    if let Some(raw) = t.agent_title() {
        let head = raw.trim_start();
        let is_agent = head.starts_with('✳')
            || head
                .chars()
                .next()
                .is_some_and(|c| ('\u{2801}'..='\u{28FF}').contains(&c));
        if is_agent {
            let task = strip_status(&raw);
            // agent 默认标题（"Claude Code" / "claude"）不算任务名，继续往下回退。
            if !task.is_empty() && task != "Claude Code" && task != "claude" {
                // 也别跟启动项显示名撞车（例如菜单叫 Claude Code，agent 也只报这个）。
                if t.launch_label().is_none_or(|l| l != task) {
                    return task;
                }
            }
        }
    }
    if let Some(label) = t.launch_label() {
        return label.to_string();
    }
    t.title().to_string()
}

/// 侧栏分屏子行显示的 pane 标题：用户改过名就用改的，否则走自动推导。
///
/// 跟 `pane_auto_title` 分开是有意的：`Session::title` 拿的是活动 pane 的自动标题，
/// 若这里的自定义名漏进去，给活动 pane 改名会连带改掉侧栏父行（会话名），切换
/// 活动 pane 后父行又跳回来——会话名和 pane 名得各归各的。
fn pane_title(view: &Entity<TerminalView>, cx: &App) -> String {
    view.read(cx)
        .custom_title()
        .map(str::to_string)
        .unwrap_or_else(|| pane_auto_title(view, cx))
}

/// 单个终端 pane 的状态：逻辑同 Session::status，但只看这一个 pane 自己
/// （Session::status 是取会话内所有 pane 的最高态）。
fn pane_status(view: &Entity<TerminalView>, cx: &App) -> AgentStatus {
    let t = view.read(cx);
    match t.attention_kind() {
        Some(terminal_view::AttentionKind::Approval) => return AgentStatus::WaitingApproval,
        Some(terminal_view::AttentionKind::Attention) => return AgentStatus::NeedsAttention,
        None => {}
    }
    if let Some(raw) = t.agent_title() {
        if let Some(c) = raw.trim_start().chars().next() {
            if ('\u{2801}'..='\u{28FF}').contains(&c) {
                return AgentStatus::Running;
            }
        }
    }
    if t.completed_unread() {
        return AgentStatus::Done;
    }
    AgentStatus::Idle
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
    /// 文件树列拖出的宽度（px）；None = 用默认值。
    #[serde(default)]
    file_tree_w: Option<f32>,
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

/// 单个会话的持久化镜像：分屏树 + 会话内活动叶子（遍历序）+ 用户重命名过的会话名。
#[derive(serde::Serialize, serde::Deserialize)]
struct SessionState {
    layout: PaneState,
    active: usize,
    #[serde(default)]
    custom_title: Option<String>,
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
        /// 用户给这个 pane 起的名字。旧存档没有这个字段 → None，行为不变。
        #[serde(default)]
        custom_title: Option<String>,
        /// 快捷启动项显示名。旧存档没有 → None，回退 cwd 末段。
        #[serde(default)]
        launch_label: Option<String>,
    },
    Split { axis: SplitAxis, children: Vec<PaneState> },
}

/// 新会话 id（uuid v4）：GUI 与 smeltd 之间的持久身份。
fn new_sid() -> String {
    uuid::Uuid::new_v4().to_string()
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
        Pane::Leaf(t) => {
            let t = t.read(cx);
            PaneState::Leaf {
                cwd: t.cwd(),
                id: Some(t.session_id().to_string()),
                custom_title: t.custom_title().map(str::to_string),
                launch_label: t.launch_label().map(str::to_string),
            }
        }
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
        PaneState::Leaf {
            cwd,
            id,
            custom_title,
            launch_label,
        } => {
            // 有存档 id → reattach 守护里还活着的会话；旧存档无 id → 开新会话。
            // reattach 不再带 launch 命令（shell 已在跑），只恢复显示名与自定义名。
            let sid = id.clone().unwrap_or_else(new_sid);
            let label = launch_label.clone();
            let v = cx.new(|cx| {
                let mut view = TerminalView::new(cx, cwd.clone(), sid, None, label.as_deref());
                // 自定义名跟着同一条 Leaf 存取，reattach 后灌回来，否则重开就丢。
                view.set_custom_title(custom_title.clone());
                view
            });
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
    /// 文件树键盘选中的条目绝对路径（↑↓ 导航用）。
    file_tree_selected: Option<String>,
    /// 打开文件后要 reveal 的路径：祖先目录缓存齐了再 scroll_to_item。
    file_tree_pending_reveal: Option<String>,
    /// 当前在文件树里打开查看的文件（含预高亮的行数据）。
    open_file: Option<OpenFile>,
    /// 打开文件的自增序号：后台高亮完成时用它判断结果是否已过期（切了别的文件）。
    file_gen: u64,
    /// 当前文件有未保存改动时，用户又点了别的文件——先记下目标路径弹确认弹窗，
    /// 等用户选了"不保存"/"保存并切换"才真正打开，见 render_unsaved_file_confirm。
    pending_file_switch: Option<String>,
    /// 文件树右键「删除文件」的二次确认目标（None = 没在删）。
    delete_file_target: Option<DeleteFileTarget>,
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
    /// Git 视图的 commit message 输入框（懒创建，随 Git 视图首次渲染时创建；跟
    /// diff_comment_input 是两个独立的框，一个针对选中的 diff 行，一个是整体提交信息）。
    commit_msg_input: Option<Entity<gpui_component::input::InputState>>,
    /// 「生成」按钮请求 LLM 生成 commit message 进行中（防连点、按钮显示"生成中…"）。
    commit_msg_generating: bool,
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
    /// 文件树列宽拖拽状态（对面板：文件树 + 右侧文件内容）；拖动完通过 save_state
    /// 落盘到 file_tree_w，重启后从存档恢复。
    file_tree_resize: Entity<ResizableState>,
    /// 文件树顶部的过滤输入框；首次渲染文件树时懒创建（需要 window）。
    file_filter: Option<Entity<gpui_component::input::InputState>>,
    /// 过滤框的变更订阅（键入即重渲染）；随视图存活。
    _file_filter_sub: Option<Subscription>,
    /// 总览任务区：标题 / prompt 输入（懒创建）。
    task_title_input: Option<Entity<gpui_component::input::InputState>>,
    task_body_input: Option<Entity<gpui_component::input::InputState>>,
    /// 定时任务：执行时间输入（`YYYY-MM-DD HH:MM`，懒创建）。
    task_run_at_input: Option<Entity<gpui_component::input::InputState>>,
    /// 新建任务类型（普通 / 单次定时）。
    task_kind: tasks::TaskKind,
    /// 新建任务是否允许系统自动执行（任务级 `auto_run`；定时强制 true）。
    task_auto_run: bool,
    /// 任务列表选中项 id。
    task_selected: Option<String>,
    /// 新建任务绑定的项目 cwd。
    task_bind_project: Option<String>,
    /// 新建任务选用的 launch 命令（与设置页启动项 command 对齐）。
    task_bind_launch: Option<String>,
    /// 在已有终端执行：Some(smeltd session id)；None = 新开终端。
    /// 由「终端/会话右键 → 新建任务」写入。
    task_bind_session: Option<String>,
    /// 任务总览状态筛选：None = 全部。
    task_column_filter: Option<tasks::TaskColumn>,
    /// 标题输入的 Enter 订阅（回车 = 创建并开跑）。
    _task_title_sub: Option<Subscription>,
    /// 新建任务弹窗（Cmd+Shift+N / 侧栏「新建任务」）。
    show_new_task_modal: bool,
    /// 定时任务扫描循环是否已启动（避免 render 重复 spawn）。
    task_schedule_started: bool,
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
    /// 文件树列初始宽度（px）：启动时从存档恢复，作为 resizable_panel 的初始 size。
    file_tree_w: f32,
    /// 文件树列 resize 事件订阅（拖动完写回存档）；随视图存活。
    _file_tree_resize_sub: Subscription,
    /// git 信息缓存（cwd → (分支, 改动数)），总览页后台刷新、渲染读缓存。
    git_cache: HashMap<String, (String, usize)>,
    /// 宠物大脑（LLM）配置的输入框；首次打开设置面板时懒创建（需要 window）。
    llm_inputs: Option<LlmInputs>,
    /// 上面几个输入框的变更订阅（保活；随视图存活）。
    llm_subs: Vec<Subscription>,
    /// 启动项列表编辑器（设置页「启动」分组懒创建）。
    launch_inputs: Option<settings::LaunchInputs>,
    /// 设置面板的有状态组件（懒创建）：不透明度滑块 + 字体大小滑块 + 背景色 / 宠物色取色器。
    opacity_slider: Option<Entity<SliderState>>,
    font_size_slider: Option<Entity<SliderState>>,
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
    /// 分支列表缓存（root → (取得时刻, 数据)），Git 页头部分支切换下拉用；同
    /// git_status 一套只在 Git 页打开时后台刷新。
    branches: HashMap<String, (Instant, BranchList)>,
    /// 正在后台刷新分支列表的 root（防重复并发 spawn）。
    branches_inflight: HashSet<String>,
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
    /// 历史会话表格的 Entity（懒建，见 ensure_session_table）；None = 还没建过。
    session_table: Option<Entity<TableState<SessionHistoryDelegate>>>,
    /// session_table 当前装的是哪个项目（cwd），项目切换时判定要不要重建 Entity
    /// （重置排序/滚动位置——体感上是"进了一个新页面"，同项目内刷新则保留这些状态）。
    session_table_key: Option<String>,
    /// TableEvent 订阅句柄，session_table 重建时一起换。
    session_table_sub: Option<Subscription>,
    /// 历史会话页当前显示的是「会话」还是「记忆」（同一套左列表 + 右详情布局）。
    history_pane: HistoryPane,
    /// 记忆列表缓存（cwd → (取得时刻, 数据)），跟 session_list 同一套 TTL 模板。
    memory_list: HashMap<String, (Instant, Rc<Vec<claude_memory::MemoryEntry>>)>,
    /// 正在后台扫描记忆的 cwd（防重复并发 spawn）。
    memory_list_inflight: HashSet<String>,
    /// 当前选中查看的记忆，存在列表里的下标；切项目/切列表时会被清掉。
    memory_selected: Option<usize>,
    /// 调试 HUD 开关（Cmd+Shift+F 切换）：开启时右上角显示帧率 + 帧耗时 + RSS。
    debug_hud: bool,
    /// 上一帧渲染时刻（算帧间隔用）。
    last_frame: Option<Instant>,
    /// 平滑后的帧率（EMA）。
    fps_ema: f32,
    /// 调试 HUD 上次采样的 RSS（字节）；约每秒刷新一次，避免每帧调系统 API。
    debug_mem_rss: Option<u64>,
    /// 调试 HUD 上次内存采样时刻。
    debug_mem_sampled_at: Option<Instant>,
    /// 退出确认拦截弹窗开关
    show_quit_confirm: bool,
    /// 在线更新状态机（检查/下载/暂存就绪），驱动设置页"更新"分区 + 齿轮强调色。
    update_status: updater::UpdateStatus,
    /// 设置窗口打开时要停在第几页（索引对应 `render_settings_content` 里 pages 的顺序）。
    settings_page_ix: usize,
    /// 每请求跳一次页就 +1，用来变更 `Settings` 元素的 id。
    ///
    /// `Settings` 把当前选中页存在 `use_keyed_state` 里，只有该 id 首次出现时才读
    /// `default_selected_index`——窗口已经开着时改字段是不起作用的。把这个自增序号
    /// 编进 id，就能强制它按新的 default 重建一次。不用页号本身当 id：用户手动切走后
    /// 再点同一个入口，页号没变，id 也就没变，照样跳不过去。
    settings_page_nonce: usize,
    /// 设置页「终端字体」下拉的选项，首次渲染时算一次就缓存住。
    ///
    /// `all_font_names()` 在 mac 上枚举的是全部字体 face 的 descriptor（本机 902 个），
    /// 再逐个 CopyAttribute 取 family name，实测约 50ms/次——远超 60fps 的 16.6ms 预算。
    /// 它原先直接写在 `render_settings_content` 里，设置窗口每帧都要重算一遍，下拉一
    /// 展开就肉眼可见掉帧。字体列表在进程生命周期内几乎不变，不值得每帧重扫。
    font_options: std::cell::OnceCell<Vec<(SharedString, SharedString)>>,
    /// 上次同步给 Dock 角标的「需要关注」会话数；None 强制首帧同步一次。
    /// 只在这个数变化时才调用 Cocoa API，避免每次 render 都发一遍。
    dock_badge_count: Option<usize>,
    /// 上次同步给菜单栏下拉菜单的会话快照；None 强制首帧同步一次。只在快照真的变化
    /// 时才重建 AppKit 菜单，避免每次 render 都拆了重建。
    status_menu_snapshot: Option<Vec<status_item::SessionEntry>>,
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
    /// 正在重命名的对象 + 弹窗里的文本框（None = 没在重命名）。见
    /// `start_rename`/`confirm_rename`。
    rename_target: Option<RenameTarget>,
    rename_input: Option<Entity<gpui_component::input::InputState>>,
    /// 重命名文本框的事件订阅句柄，随 rename_input 一起换（回车/失焦提交）。
    _rename_sub: Option<Subscription>,
    /// 仓库身份缓存（cwd → git-dir/common-dir/分支）：判断某个会话是不是 worktree
    /// 检出、侧栏聚簇排序、拼「仓库名 · 分支名」标签都靠它。None = 探测过但不是
    /// git 仓库（比如临时终端落脚的 $HOME），不会重复无意义地重试。
    repo_info: HashMap<String, (Instant, Option<RepoInfo>)>,
    /// 正在后台探测仓库身份、避免重复起进程的 cwd 集合。
    repo_info_inflight: HashSet<String>,
    /// 正在新建的 worktree 目标 + 弹窗里的分支名文本框（None = 没在新建）。
    new_worktree_target: Option<NewWorktreeTarget>,
    new_worktree_input: Option<Entity<gpui_component::input::InputState>>,
    _new_worktree_sub: Option<Subscription>,
    /// 正在确认删除的 worktree（None = 没在删）。
    delete_worktree_target: Option<DeleteWorktreeTarget>,
    /// 各类后台操作（建/删 worktree、生成 commit message 等）失败时的提示，render
    /// 顶部取走并弹成通知；后台任务里没有 Window，弹不了通知，所以先暂存到这。
    background_error: Option<String>,
    /// 守护进程是否落后于磁盘上的 smeltd 二进制（重装/重编译后常见，需手动重启守护
    /// 才生效新代码）；None 表示还没查过，驱动设置页「更新」分区的重启提示。
    daemon_outdated: Option<bool>,
    /// 最近一次无缝升级的结果提示（设置页守护分区显示；None = 没试过）。
    daemon_upgrade_msg: Option<String>,
    /// 无缝升级进行中（按钮置灰防连点）。
    daemon_upgrading: bool,
    /// 「重启守护进程」二次确认弹窗开关：点确定会断开所有当前终端会话。
    show_daemon_restart_confirm: bool,
    /// 根节点自己的焦点句柄：总览/文件树/Git/热力图/历史会话这些页面自身没有可
    /// 聚焦的元素，切过去后如果谁都不 focus，窗口的 focus 仍停在切走前那个（可能
    /// 已经不在当前渲染树里的）终端上——GPUI 找不到就把 focus 兜底纠正到 window 的
    /// 真正根节点，而 Workspace 这层的 on_key_down（Cmd+Shift+F 等全局快捷键）挂在
    /// Root 组件之下、并非那个根节点，于是收不到事件，表现为"切到别的页面后快捷键
    /// 全部失灵"。切到非终端页面时把 focus 显式认领到这个句柄上，保证 Workspace 的
    /// on_key_down 始终在 dispatch 路径上。
    focus_handle: FocusHandle,
}

impl Workspace {
    fn new(cx: &mut Context<Self>) -> Self {
        // 优先按存档的会话列表重建；旧存档（单树 / cwd 列表）迁移，无存档则默认单会话。
        let saved = load_ws_state();
        let sidebar_w = saved.as_ref().and_then(|s| s.sidebar_w).unwrap_or(230.);
        let file_tree_w = saved.as_ref().and_then(|s| s.file_tree_w).unwrap_or(260.);

        let mut sessions: Vec<Session> = Vec::new();
        let mut active_session = 0;
        if let Some(s) = saved.as_ref() {
            if !s.sessions.is_empty() {
                for ss in &s.sessions {
                    let mut leaves = Vec::new();
                    let layout = rebuild_pane(&ss.layout, &mut leaves, cx);
                    if let Some(active) = leaves.get(ss.active).or_else(|| leaves.first()).cloned() {
                        sessions.push(Session { layout, active, custom_title: ss.custom_title.clone() });
                    }
                }
                active_session = s.active_session;
            } else if let Some(ps) = &s.layout {
                // 旧格式：单棵树 → 一个会话。
                let mut leaves = Vec::new();
                let layout = rebuild_pane(ps, &mut leaves, cx);
                if let Some(active) = leaves.get(s.active).or_else(|| leaves.first()).cloned() {
                    sessions.push(Session { layout, active, custom_title: None });
                }
            } else {
                // 更旧格式：cwd 列表 → 每个 cwd 一个独立会话。
                for cwd in s.tabs.clone() {
                    let v = cx.new(|cx| TerminalView::new(cx, cwd, new_sid(), None, None));
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
        // 文件树列 resize：同侧栏一套写法，拖动完写回存档持久化宽度。
        let file_tree_resize = cx.new(|_| ResizableState::default());
        let _file_tree_resize_sub =
            cx.subscribe(&file_tree_resize, |this, _state, _e: &ResizablePanelEvent, cx| {
                this.save_state(cx);
            });

        let mut ws = Self {
            sessions,
            active_session,
            view: MainView::Terminal,
            expanded: HashSet::new(),
            dir_cache: HashMap::new(),
            dir_inflight: HashSet::new(),
            file_tree_selected: None,
            file_tree_pending_reveal: None,
            open_file: None,
            file_gen: 0,
            pending_file_switch: None,
            delete_file_target: None,
            pending_switch_after_save: None,
            git_diff: None,
            diff_gen: 0,
            diff_split: false,
            diff_selected: HashSet::new(),
            diff_comment_input: None,
            commit_msg_input: None,
            commit_msg_generating: false,
            sidebar_open: true,
            notifications_open: false,
            palette: None,
            _palette_sub: None,
            git_files_scroll: ScrollHandle::new(),
            diff_scroll: UniformListScrollHandle::new(),
            file_tree_scroll: ScrollHandle::new(),
            file_tree_resize,
            file_filter: None,
            _file_filter_sub: None,
            task_title_input: None,
            task_body_input: None,
            task_run_at_input: None,
            task_kind: tasks::TaskKind::Once,
            task_auto_run: true,
            task_selected: None,
            task_bind_project: None,
            task_bind_launch: None,
            task_bind_session: None,
            task_column_filter: None,
            _task_title_sub: None,
            show_new_task_modal: false,
            task_schedule_started: false,
            search_results: None,
            search_gen: 0,
            root_resize,
            sidebar_w,
            _resize_sub,
            file_tree_w,
            _file_tree_resize_sub,
            git_cache: HashMap::new(),
            git_status: HashMap::new(),
            git_status_inflight: HashSet::new(),
            branches: HashMap::new(),
            branches_inflight: HashSet::new(),
            git_dirty: Arc::new(Mutex::new(HashSet::new())),
            git_watchers: HashMap::new(),
            hotspot_data: HashMap::new(),
            hotspot_inflight: HashSet::new(),
            session_list: HashMap::new(),
            session_list_inflight: HashSet::new(),
            session_detail: None,
            session_detail_gen: 0,
            session_table: None,
            session_table_key: None,
            session_table_sub: None,
            history_pane: HistoryPane::Sessions,
            memory_list: HashMap::new(),
            memory_list_inflight: HashSet::new(),
            memory_selected: None,
            llm_inputs: None,
            llm_subs: Vec::new(),
            launch_inputs: None,
            opacity_slider: None,
            font_size_slider: None,
            bg_color_picker: None,
            pet_color_picker: None,
            settings_subs: Vec::new(),
            applied_window_bg: None,
            debug_hud: false,
            last_frame: None,
            debug_mem_rss: None,
            debug_mem_sampled_at: None,
            fps_ema: 0.0,
            show_quit_confirm: false,
            update_status: updater::UpdateStatus::default(),
            settings_page_ix: 0,
            settings_page_nonce: 0,
            font_options: std::cell::OnceCell::new(),
            dock_badge_count: None,
            status_menu_snapshot: None,
            usage_cache: None,
            usage_inflight: false,
            sess_drop_hint: None,
            proj_drop_hint: None,
            rename_target: None,
            rename_input: None,
            _rename_sub: None,
            repo_info: HashMap::new(),
            repo_info_inflight: HashSet::new(),
            new_worktree_target: None,
            new_worktree_input: None,
            _new_worktree_sub: None,
            delete_worktree_target: None,
            background_error: None,
            daemon_outdated: None,
            daemon_upgrade_msg: None,
            daemon_upgrading: false,
            show_daemon_restart_confirm: false,
            focus_handle: cx.focus_handle(),
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

    /// 后台刷新所有会话 cwd 的 git 信息（分支 + 改动数）到缓存，进总览时调用。
    fn refresh_git(&mut self, cx: &mut Context<Self>) {
        let cwds: Vec<String> = self.sessions.iter().filter_map(|s| s.cwd(cx)).collect();
        cx.spawn(async move |this, cx| {
            let results = cx
                .background_executor()
                .spawn(async move {
                    let mut out: Vec<(String, String, usize)> = Vec::new();
                    for cwd in cwds {
                        let branch = run_git(&cwd, &["rev-parse", "--abbrev-ref", "HEAD"])
                            .ok()
                            .filter(|o| o.status.success())
                            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
                        if let Some(branch) = branch {
                            let changed = run_git(&cwd, &["status", "--porcelain"])
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

    /// 按会话的 cwd 分组成「项目」：(项目名, cwd, 该项目下的会话下标列表)。
    /// 侧栏渲染和拖拽排序共用同一份算法，避免两处各算一遍、行为跑偏。临时终端
    /// （cwd 落在 scratch_dir）单独归一组「临时终端」；worktree 检出显示「仓库名 ·
    /// 分支名」（见 group_info_for_cwd），且跟主仓库、其余 worktree 聚在一起排序，
    /// 不会因为创建时间跟别的项目穿插而散落在列表各处——组内、组间相对顺序仍按
    /// 「同一簇里最早出现的组」的先后来（stable_sort，不会无意义打乱手动拖拽过的
    /// 顺序）。
    fn project_groups(&self, cx: &App) -> Vec<(String, String, Vec<usize>)> {
        let mut projects: Vec<(String, String, Vec<usize>)> = Vec::new();
        let mut cluster_of: HashMap<String, Option<String>> = HashMap::new();
        for (ix, s) in self.sessions.iter().enumerate() {
            let cwd = s.cwd(cx).unwrap_or_default();
            let (name, cluster) = self.group_info_for_cwd(&cwd);
            match projects.iter_mut().find(|(n, _, _)| *n == name) {
                Some(p) => p.2.push(ix),
                None => {
                    cluster_of.insert(name.clone(), cluster);
                    projects.push((name, cwd, vec![ix]));
                }
            }
        }
        let mut first_seen: HashMap<String, usize> = HashMap::new();
        for (i, (name, _, _)) in projects.iter().enumerate() {
            let key = cluster_of.get(name).cloned().flatten().unwrap_or_else(|| name.clone());
            first_seen.entry(key).or_insert(i);
        }
        projects.sort_by_key(|(name, _, _)| {
            let key = cluster_of.get(name).cloned().flatten().unwrap_or_else(|| name.clone());
            first_seen[&key]
        });
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
                // 必须用跟 project_groups 同一套名字推导（group_info_for_cwd），
                // 不能退回纯目录名——worktree 分组显示名带了分支后缀，两边不一致
                // 会导致这里永远找不到目标组、挪动直接失效。
                self.group_info_for_cwd(&cwd).0 == to_name.as_ref()
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
        self.add_session_with_launch(cwd, None, None, cx);
    }

    /// 项目行「+」下拉菜单的快捷入口：`launch` 编进 shell 的启动命令行（见
    /// terminal.rs::spawn / smeltd.rs::spawn_session），`label` 用作侧栏初始显示名。
    fn add_session_with_launch(
        &mut self,
        cwd: Option<String>,
        launch: Option<&str>,
        label: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        let view = cx.new(|cx| TerminalView::new(cx, cwd, new_sid(), launch, label));
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
        let view = cx.new(|cx| TerminalView::new(cx, cwd, new_sid(), None, None));
        let state = cx.new(|_| ResizableState::default());
        let sess = &mut self.sessions[self.active_session];
        split_leaf(&mut sess.layout, old, axis, state, view.clone());
        sess.active = view;
        self.save_state(cx);
        cx.notify();
    }

    /// 把所有会话（各自分屏树 + 活动叶子遍历序）+ 侧栏宽度 + 文件树列宽写入
    /// workspace.json（失败静默忽略）。
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
                SessionState { layout, active, custom_title: s.custom_title.clone() }
            })
            .collect();
        let sidebar_w = self.root_resize.read(cx).sizes().first().copied().map(f32::from);
        let file_tree_w = self.file_tree_resize.read(cx).sizes().first().copied().map(f32::from);
        let state = WsState {
            sessions,
            active_session: self.active_session,
            sidebar_w,
            file_tree_w,
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

    /// 删 worktree 前先清掉 cwd 落在 `path`（或它子目录）下的所有会话，不然会留下
    /// 指向即将被删除目录的死会话。close_session 拒绝关到全局只剩 0 个会话，所以
    /// 如果这些要关的会话恰好是当前仅有的会话，先开一个安全的临时终端垫底。
    fn close_sessions_under(&mut self, path: &str, cx: &mut Context<Self>) {
        let prefix = format!("{}/", path.trim_end_matches('/'));
        let mut ixs: Vec<usize> = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                let cwd = s.cwd(cx).unwrap_or_default();
                cwd == path || cwd.starts_with(&prefix)
            })
            .map(|(ix, _)| ix)
            .collect();
        if ixs.len() == self.sessions.len() {
            self.new_scratch_session(cx);
        }
        // 降序关闭：前面的下标不受后面 remove 影响（同 move_project_near 的做法）。
        ixs.sort_unstable_by(|a, b| b.cmp(a));
        for ix in ixs {
            self.close_session(ix, cx);
        }
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

    /// 侧栏展开会话看到的分屏子行：点击某个 pane → 切到它所在会话，并把该 pane
    /// 设为会话内的活动 pane（分屏树本身不变，只是换了「当前看哪个」）。
    fn activate_session_pane(
        &mut self,
        ix: usize,
        pane: Entity<TerminalView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.activate(ix, window, cx);
        self.activate_pane(&pane, window, cx);
    }

    /// 切换到第 ix 个会话并聚焦。
    fn activate(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix < self.sessions.len() {
            self.active_session = ix;
            // 从会话总览 / 任务总览点会话 → 进入终端视图（否则主区仍停在总览页）。
            if matches!(self.view, MainView::Overview | MainView::Tasks) {
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

    /// Cmd+[ / Cmd+] 在当前会话的分屏树里循环切换活动 pane（对齐 iTerm2 默认键位：
    /// 这两个键管「同一会话内切哪个格子」，会话本身的切换交给 Cmd+1~9）。
    /// 只有一个 pane（没分屏）时什么都不做。
    fn cycle_pane(&mut self, delta: i32, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sess) = self.cur() else { return };
        let mut leaves = Vec::new();
        collect_leaves(&sess.layout, &mut leaves);
        if leaves.len() < 2 {
            return;
        }
        let cur_id = sess.active.entity_id();
        let Some(ix) = leaves.iter().position(|l| l.entity_id() == cur_id) else {
            return;
        };
        let n = leaves.len() as i32;
        let next = (ix as i32 + delta).rem_euclid(n) as usize;
        let target = leaves[next].clone();
        self.activate_pane(&target, window, cx);
    }

    /// 侧栏右键「重命名」：弹出文本框，预填当前标题。回车 / 点「确定」提交，见
    /// `confirm_rename`；提交前的输入放在独立的 rename_input，不影响目标对象
    /// 本身，点「取消」（走 cancel_rename）就等于什么都没发生。
    ///
    /// 注意：这里故意不监听 `InputEvent::Blur` 去自动提交——点「取消」按钮本身会先
    /// 让输入框失焦，若失焦也提交，「取消」就会在关闭前先把文本框里的内容存下来，
    /// 跟按钮的字面意思相反。所以提交只认 Enter 或显式点「确定」。
    fn start_rename(&mut self, target: RenameTarget, window: &mut Window, cx: &mut Context<Self>) {
        use gpui_component::input::{InputEvent, InputState};
        let current = match &target {
            RenameTarget::Session(ix) => {
                let Some(s) = self.sessions.get(*ix) else { return };
                s.title(cx)
            }
            RenameTarget::Pane(view) => pane_title(view, cx),
        };
        let input = cx.new(|cx| InputState::new(window, cx).default_value(current));
        input.update(cx, |s, cx| s.focus(window, cx));
        self._rename_sub = Some(cx.subscribe_in(&input, window, |this, _input, ev: &InputEvent, window, cx| {
            if matches!(ev, InputEvent::PressEnter { .. }) {
                this.confirm_rename(window, cx);
            }
        }));
        self.rename_target = Some(target);
        self.rename_input = Some(input);
        cx.notify();
    }

    /// 提交重命名：空输入等于清掉自定义名，回退到自动推导的标题。
    fn confirm_rename(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.rename_target.take() else { return };
        let Some(input) = self.rename_input.take() else { return };
        self._rename_sub = None;
        let text = input.read(cx).value().trim().to_string();
        match target {
            RenameTarget::Session(ix) => {
                if let Some(s) = self.sessions.get_mut(ix) {
                    s.custom_title = (!text.is_empty()).then_some(text);
                }
            }
            RenameTarget::Pane(view) => {
                view.update(cx, |t, _| t.set_custom_title(Some(text)));
            }
        }
        self.save_state(cx);
        cx.notify();
    }

    /// 取消重命名：不落地任何改动。
    fn cancel_rename(&mut self, cx: &mut Context<Self>) {
        self.rename_target = None;
        self.rename_input = None;
        self._rename_sub = None;
        cx.notify();
    }

    /// 侧栏版本号与设置齿轮共用它决定要不要缀红点。
    fn update_available(&self) -> bool {
        matches!(
            self.update_status,
            updater::UpdateStatus::Downloading { .. }
                | updater::UpdateStatus::Installing { .. }
                | updater::UpdateStatus::ReadyToInstall { .. }
        )
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
    /// 下载线程通过 channel 往回推字节进度，UI 线程照单刷新状态；发送端随下载任务结束而
    /// drop，`recv` 收到 Err 即代表下载收尾，此时再 `await` 任务拿最终结果。
    fn start_update_download(&mut self, version: String, url: String, cx: &mut Context<Self>) {
        self.update_status =
            updater::UpdateStatus::Downloading { version: version.clone(), received: 0, total: None };
        cx.notify();
        cx.spawn(async move |this, cx| {
            let (tx, rx) = smol::channel::unbounded::<updater::DownloadProgress>();
            let v = version.clone();
            let task = cx.background_executor().spawn(async move {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?
                    .block_on(updater::download_and_stage(&url, &v, |p| {
                        let _ = tx.try_send(p);
                    }))
            });

            while let Ok(progress) = rx.recv().await {
                let version = version.clone();
                let _ = this.update(cx, |this, cx| {
                    this.update_status = match progress {
                        updater::DownloadProgress::Bytes { received, total } => {
                            updater::UpdateStatus::Downloading { version, received, total }
                        }
                        updater::DownloadProgress::Installing => {
                            updater::UpdateStatus::Installing { version }
                        }
                    };
                    cx.notify();
                });
            }

            let result = task.await;
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
                // 落后就自动无缝升级——「随版本更新且不中断」：exec 交接保留所有会话；
                // 正在跑的守护太旧不支持时，结果提示会引导到设置页手动重启。
                if outdated {
                    this.upgrade_daemon_seamless(cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 逐 pane 调 reconnect()：会话 id 都还在，走正常 reattach + 重放恢复画面。
    /// 无缝升级（Upgraded/Failed，见下）和硬重启都要用同一套。
    fn reconnect_all_terminals(&self, cx: &mut Context<Self>) {
        for sess in &self.sessions {
            let mut leaves = Vec::new();
            collect_leaves(&sess.layout, &mut leaves);
            for leaf in leaves {
                leaf.update(cx, |view, cx| view.reconnect(cx));
            }
        }
    }

    /// 无缝升级守护：守护 exec 新二进制、PTY fd 原地交接，会话不中断（smeltd.rs 头注释）。
    /// 成功后逐 pane reconnect——会话 id 都还在，走正常 reattach + 重放，画面最多闪一下。
    /// 正在跑的守护太旧不认识 upgrade op 时提示改用下面的硬重启。
    fn upgrade_daemon_seamless(&mut self, cx: &mut Context<Self>) {
        if self.daemon_upgrading {
            return;
        }
        self.daemon_upgrading = true;
        self.daemon_upgrade_msg = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let outcome =
                cx.background_executor().spawn(async { terminal::upgrade_daemon() }).await;
            let outdated =
                cx.background_executor().spawn(async { terminal::daemon_outdated() }).await;
            let _ = this.update(cx, |this, cx| {
                this.daemon_upgrading = false;
                this.daemon_outdated = Some(outdated);
                this.daemon_upgrade_msg = Some(match outcome {
                    terminal::UpgradeOutcome::Upgraded => {
                        this.reconnect_all_terminals(cx);
                        "已无缝升级，所有会话保持运行。".to_string()
                    }
                    terminal::UpgradeOutcome::Unsupported => {
                        // 守护完全没认这个 op，控制连接以外的东西没被碰过，各 pane
                        // 的流式连接照常连着，不需要重连。
                        "正在跑的守护版本过旧，不支持无缝升级；请用「重启守护进程」（会断开会话）。"
                            .to_string()
                    }
                    terminal::UpgradeOutcome::Failed => {
                        // 守护回了 ok:true 才会 exec：只要走到这一步，exec 大概率已经
                        // 发生、旧连接已经随之断开，只是我们没能在轮询窗口内确认新
                        // 进程的 mtime 追平——按"可能已断"保守重连，好过让用户以为
                        // 终端只是卡了一下、实际连接早就死了却不知道要重开。
                        this.reconnect_all_terminals(cx);
                        "升级结果未确认（可能已生效但检测超时），已尝试重连各终端；如仍无响应可重试或改用重启。".to_string()
                    }
                });
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
                this.reconnect_all_terminals(cx);
                cx.notify();
            });
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

    /// 总览页：会话态势监控（任务在左侧栏，不在此页）。
    fn render_overview(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> Div {
        let (fg, muted, border) = {
            let t = cx.theme();
            (t.foreground, t.muted_foreground, t.border)
        };

        let need_attn = self
            .sessions
            .iter()
            .filter(|s| {
                matches!(
                    s.status(cx),
                    AgentStatus::WaitingApproval | AgentStatus::NeedsAttention
                )
            })
            .count();

        let header = div()
            .px_5()
            .pt_4()
            .pb_2()
            .border_b_1()
            .border_color(border)
            .flex()
            .items_center()
            .justify_between()
            .child(
                div()
                    .text_xl()
                    .font_bold()
                    .text_color(fg)
                    .child("总览"),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(if need_attn > 0 {
                        format!("会话监控 · {need_attn} 需关注 · 点卡片进入终端")
                    } else {
                        format!("会话监控 · {} · 点卡片进入终端", self.sessions.len())
                    }),
            );

        let body = self.render_overview_sessions(cx);

        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(header)
            .child(
                div()
                    .id("overview-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_5()
                    .child(body),
            )
    }

    /// 会话监控网格。
    fn render_overview_sessions(&mut self, cx: &mut Context<Self>) -> Div {
        let (fg, muted) = {
            let t = cx.theme();
            (t.foreground, t.muted_foreground)
        };
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
        let c_muted_dot: Hsla = rgba(0x8b93a7aa).into();

        let statuses: Vec<AgentStatus> = self.sessions.iter().map(|s| s.status(cx)).collect();
        let need = statuses
            .iter()
            .filter(|s| matches!(s, AgentStatus::WaitingApproval | AgentStatus::NeedsAttention))
            .count();
        let running = statuses.iter().filter(|s| matches!(s, AgentStatus::Running)).count();
        let done = statuses.iter().filter(|s| matches!(s, AgentStatus::Done)).count();
        let pill = |text: String, color: Hsla, bg: Hsla| {
            div()
                .px(px(11.))
                .py(px(4.))
                .rounded_full()
                .bg(bg)
                .text_sm()
                .text_color(color)
                .child(text)
        };

        let summary = div()
            .flex()
            .items_center()
            .gap_2()
            .flex_wrap()
            .mb_4()
            .child(pill(format!("{} 会话", self.sessions.len()), fg, soft_bg))
            .child(pill(format!("{need} 需要处理"), c_red, red_tint))
            .child(pill(format!("{running} 运行中"), c_blue, blue_tint))
            .children((done > 0).then(|| pill(format!("{done} 已完成"), c_green, green_tint)));

        if self.sessions.is_empty() {
            return div()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_3()
                .py_16()
                .child(div().text_sm().text_color(muted).child("还没有会话"))
                .child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child("侧栏打开项目或「新建终端」后，状态会出现在这里"),
                );
        }

        let mut order: Vec<usize> = (0..self.sessions.len()).collect();
        order.sort_by_key(|&ix| match statuses[ix] {
            AgentStatus::WaitingApproval => 0,
            AgentStatus::NeedsAttention => 1,
            AgentStatus::Running => 2,
            AgentStatus::Done => 3,
            AgentStatus::Idle => 4,
        });

        let cards: Vec<_> = order
            .into_iter()
            .map(|ix| {
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
                    .w(px(300.))
                    .p_4()
                    .rounded(px(18.))
                    .border_1()
                    .border_color(card_border)
                    .bg(card_bg)
                    .shadow_sm()
                    .cursor_pointer()
                    .hover(|d| d.border_color(dot).shadow_lg().bg(rgb(0x1c1e24)))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _ev, window, cx| this.activate(ix, window, cx)),
                    )
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
                    .children(live_line.map(|line| {
                        div().text_xs().text_color(muted).truncate().child(line)
                    }))
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
                    .children((!preview.is_empty()).then(|| {
                        div()
                            .p_2()
                            .rounded_lg()
                            .bg(rgb(0x0d0d10))
                            .font_family(terminal_view::font_family())
                            .text_xs()
                            .text_color(muted)
                            .flex()
                            .flex_col()
                            .children(preview.into_iter().map(|line| {
                                div().truncate().whitespace_nowrap().child(if line.is_empty() {
                                    " ".to_string()
                                } else {
                                    line
                                })
                            }))
                    }))
            })
            .collect();

        div()
            .flex()
            .flex_col()
            .child(summary)
            .child(div().flex().flex_wrap().gap_4().children(cards))
    }

    /// 弹窗遮罩 + 居中卡片壳：宽度 `width`，颜色取当前主题。`content` 是调用方已经
    /// 拼好的标题/正文/按钮行（`v_flex().child(...)...`），这里只负责外层半透明遮罩
    /// 和卡片本身的边框/圆角/阴影/内边距——是所有确认弹窗共享的视觉容器。
    ///
    /// `heavy` 控制遮罩压暗程度：真正不可逆/高后果的操作（退出、删除 worktree、
    /// 重启守护进程、丢弃未保存改动）用 `true`——全屏压暗，明确打断当前操作；
    /// 纯输入类的低风险操作（重命名）用 `false`——只留一层很淡的遮罩防止误点
    /// 背景，不用完全打断视觉，跟操作本身的后果对齐（见交互设计讨论）。
    fn modal_shell(width: f32, heavy: bool, content: Div, cx: &mut Context<Self>) -> Div {
        let (border, popover) = {
            let t = cx.theme();
            (t.border, t.popover)
        };
        let backdrop = if heavy { rgba(0x000000aa) } else { rgba(0x00000026) };
        div()
            .absolute()
            .inset_0()
            .bg(backdrop)
            .flex()
            .items_center()
            .justify_center()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                content
                    .w(px(width))
                    .p_5()
                    .bg(popover)
                    .border_1()
                    .border_color(border)
                    .rounded_lg()
                    .shadow_lg()
                    .gap_4(),
            )
    }

    /// 弹窗按钮的中性/强调配色：(中性底色, 中性 hover, 强调底色, 强调 hover, 强调文字色)。
    /// `danger=true` 强调色用红（危险操作，如删除/重启），`false` 用蓝（普通确认）。
    fn modal_accent_colors(danger: bool) -> (Hsla, Hsla, Hsla, Hsla, Hsla) {
        let neutral_bg: Hsla = rgba(0xffffff0a).into();
        let neutral_hover: Hsla = rgba(0xffffff1f).into();
        if danger {
            (
                neutral_bg,
                neutral_hover,
                rgba(0xef444424).into(),
                rgba(0xef444440).into(),
                Hsla::from(rgb(0xff8f8f)),
            )
        } else {
            (
                neutral_bg,
                neutral_hover,
                rgba(0x4a9eff24).into(),
                rgba(0x4a9eff40).into(),
                Hsla::from(rgb(0x8fc7ff)),
            )
        }
    }

    /// 弹窗按钮的基础样式（尺寸/圆角/字号/底色/文字色/label），不含点击行为——大部分
    /// 调用方直接用 [`Self::modal_button`]；`render_delete_worktree_confirm` 的
    /// 「检查中…」禁用态需要条件性挂 hover/on_click，才会单独调这个再自己 `.when(...)`。
    fn modal_button_base(id: &'static str, label: &'static str, bg: Hsla, text_color: Hsla) -> Stateful<Div> {
        div()
            .id(id)
            .px_3()
            .py(px(5.))
            .rounded_lg()
            .bg(bg)
            .border_1()
            .border_color(rgba(0xffffff12))
            .text_sm()
            .text_color(text_color)
            .child(label)
    }

    /// 弹窗按钮：基础样式 + hover 变色 + 点击行为，覆盖绝大多数弹窗按钮的用法。
    fn modal_button(
        id: &'static str,
        label: &'static str,
        bg: Hsla,
        hover_bg: Hsla,
        text_color: Hsla,
        on_click: impl Fn(&mut Self, &ClickEvent, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        Self::modal_button_base(id, label, bg, text_color)
            .cursor_pointer()
            .hover(move |s| s.bg(hover_bg))
            .on_click(cx.listener(on_click))
    }

    /// 渲染无条件退出确认弹层：磨砂遮罩 + 确认退出/取消按钮。
    fn render_quit_confirm(&self, cx: &mut Context<Self>) -> Div {
        let (fg, muted) = {
            let t = cx.theme();
            (t.foreground, t.muted_foreground)
        };
        let (neutral_bg, neutral_hover, tint, hover, accent_text) = Self::modal_accent_colors(false);

        let content = v_flex()
            .child(div().font_bold().text_color(fg).text_lg().child("确定退出 Smelt 吗？"))
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child("退出工作台后，后台守护进程仍在运行，但当前活动的终端连接将被断开。"),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(Self::modal_button(
                        "cancel-quit",
                        "取消",
                        neutral_bg,
                        neutral_hover,
                        fg,
                        |this, _, _, cx| {
                            this.show_quit_confirm = false;
                            cx.notify();
                        },
                        cx,
                    ))
                    .child(Self::modal_button(
                        "confirm-quit",
                        "确定退出",
                        tint,
                        hover,
                        accent_text,
                        |this, _, _, cx| {
                            // 有暂存好的新版本就在退出前落盘替换；失败静默忽略，
                            // 不能因为自更新出岔子就把用户堵在退出流程里。
                            if let updater::UpdateStatus::ReadyToInstall { staged_app, .. } =
                                &this.update_status
                            {
                                let _ = updater::finalize_pending_update(staged_app);
                            }
                            cx.quit();
                        },
                        cx,
                    )),
            );
        Self::modal_shell(320., true, content, cx)
    }

    /// 侧栏「重命名」弹层：与 render_quit_confirm 同款视觉（居中卡片 + 半透明遮罩），
    /// 正文换成预填当前标题的文本框。仅在 self.rename_input 就绪时被调用（见
    /// start_rename/上面 .children(self.rename_target.is_some()...)）。
    fn render_rename_session(&self, cx: &mut Context<Self>) -> Div {
        let (fg, muted) = {
            let t = cx.theme();
            (t.foreground, t.muted_foreground)
        };
        let (neutral_bg, neutral_hover, tint, hover, accent_text) = Self::modal_accent_colors(false);
        let Some(input) = self.rename_input.as_ref() else { return div() };
        // 会话行和分屏子行共用这个弹窗，标题得说清改的是哪个。
        let heading = match self.rename_target {
            Some(RenameTarget::Pane(_)) => "重命名终端",
            _ => "重命名会话",
        };

        let content = v_flex()
            .child(div().font_bold().text_color(fg).text_lg().child(heading))
            .child(div().text_sm().text_color(muted).child("留空则恢复自动识别的标题。"))
            .child(Input::new(input))
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(Self::modal_button(
                        "cancel-rename",
                        "取消",
                        neutral_bg,
                        neutral_hover,
                        fg,
                        |this, _, _, cx| this.cancel_rename(cx),
                        cx,
                    ))
                    .child(Self::modal_button(
                        "confirm-rename",
                        "确定",
                        tint,
                        hover,
                        accent_text,
                        |this, _, window, cx| this.confirm_rename(window, cx),
                        cx,
                    )),
            );
        Self::modal_shell(320., false, content, cx)
    }

    /// 「重启守护进程」二次确认弹窗：明确告知会断开所有当前终端会话。与
    /// render_quit_confirm 同款视觉（居中卡片 + 半透明遮罩）。
    fn render_daemon_restart_confirm(&self, cx: &mut Context<Self>) -> Div {
        let (fg, muted) = {
            let t = cx.theme();
            (t.foreground, t.muted_foreground)
        };
        let (neutral_bg, neutral_hover, tint, hover, accent_text) = Self::modal_accent_colors(true);

        let content = v_flex()
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
                    .child(Self::modal_button(
                        "cancel-daemon-restart",
                        "取消",
                        neutral_bg,
                        neutral_hover,
                        fg,
                        |this, _, _, cx| {
                            this.show_daemon_restart_confirm = false;
                            cx.notify();
                        },
                        cx,
                    ))
                    .child(Self::modal_button(
                        "confirm-daemon-restart",
                        "确定重启",
                        tint,
                        hover,
                        accent_text,
                        |this, _, _, cx| this.confirm_restart_daemon(cx),
                        cx,
                    )),
            );
        Self::modal_shell(320., true, content, cx)
    }

    /// 当前文件有未保存改动、又点了别的文件时弹的确认弹窗：取消 / 不保存直接切换 /
    /// 保存并切换。与 render_quit_confirm 同款视觉（居中卡片 + 半透明遮罩）。
    fn render_unsaved_file_confirm(&self, target: String, cx: &mut Context<Self>) -> Div {
        let (fg, muted) = {
            let t = cx.theme();
            (t.foreground, t.muted_foreground)
        };
        let (neutral_bg, neutral_hover, tint, hover, accent_text) = Self::modal_accent_colors(false);
        let cur_name = self
            .open_file
            .as_ref()
            .map(|of| of.path.rsplit('/').next().unwrap_or(of.path.as_str()).to_string())
            .unwrap_or_default();
        let target_name = target.rsplit('/').next().unwrap_or(target.as_str()).to_string();

        let content = v_flex()
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
                    .child(Self::modal_button(
                        "unsaved-cancel",
                        "取消",
                        neutral_bg,
                        neutral_hover,
                        fg,
                        |this, _, _, cx| {
                            this.pending_file_switch = None;
                            cx.notify();
                        },
                        cx,
                    ))
                    .child(Self::modal_button(
                        "unsaved-discard",
                        "不保存，直接切换",
                        neutral_bg,
                        neutral_hover,
                        fg,
                        |this, _, window, cx| {
                            if let Some(target) = this.pending_file_switch.take() {
                                this.open_file_now(target, None, window, cx);
                            }
                        },
                        cx,
                    ))
                    .child(Self::modal_button(
                        "unsaved-save-switch",
                        "保存并切换",
                        tint,
                        hover,
                        accent_text,
                        |this, _, _, cx| {
                            if let Some(target) = this.pending_file_switch.take() {
                                this.pending_switch_after_save = Some(target);
                                this.save_open_file(cx);
                            }
                        },
                        cx,
                    )),
            );
        Self::modal_shell(360., true, content, cx)
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

        // Dock 角标 + 菜单栏图标角标/下拉菜单：同一份状态数据源（AgentStatus），
        // 变了才调 Cocoa API 更新（避免每次 render 都发一遍）。
        let statuses: Vec<AgentStatus> = self.sessions.iter().map(|s| s.status(cx)).collect();
        let attention_count =
            statuses.iter().filter(|s| matches!(s, AgentStatus::WaitingApproval | AgentStatus::NeedsAttention)).count();
        if self.dock_badge_count != Some(attention_count) {
            self.dock_badge_count = Some(attention_count);
            dock::set_badge(attention_count);
            status_item::set_badge(attention_count);
        }

        // 菜单栏下拉菜单：按状态优先级排的会话列表（等审批 > 需要处理 > 运行中 >
        // 刚完成 > 空闲），跟总览页卡片同一套排序/配色口径。
        let mut menu_order: Vec<usize> = (0..self.sessions.len()).collect();
        menu_order.sort_by_key(|&ix| match statuses[ix] {
            AgentStatus::WaitingApproval => 0,
            AgentStatus::NeedsAttention => 1,
            AgentStatus::Running => 2,
            AgentStatus::Done => 3,
            AgentStatus::Idle => 4,
        });
        let menu_snapshot: Vec<status_item::SessionEntry> = menu_order
            .into_iter()
            .map(|ix| {
                let (color, status_text) = match statuses[ix] {
                    AgentStatus::WaitingApproval => ((0xef, 0x44, 0x44), "等你批准"),
                    AgentStatus::NeedsAttention => ((0xf5, 0x9e, 0x0b), "需要处理"),
                    AgentStatus::Running => ((0x4a, 0x9e, 0xff), "运行中"),
                    AgentStatus::Done => ((0x22, 0xc5, 0x5e), "已完成"),
                    AgentStatus::Idle => ((0x8b, 0x93, 0xa7), "空闲"),
                };
                status_item::SessionEntry { session_ix: ix, title: self.sessions[ix].title(cx), status_text, color }
            })
            .collect();
        if self.status_menu_snapshot.as_ref() != Some(&menu_snapshot) {
            status_item::update_menu(&menu_snapshot);
            self.status_menu_snapshot = Some(menu_snapshot);
        }

        // 定时任务扫描：启动后约 2s 首扫，之后 30s 一轮；到期 → run_task。
        if !self.task_schedule_started {
            self.task_schedule_started = true;
            cx.spawn_in(window, async move |this, cx| {
                smol::Timer::after(std::time::Duration::from_secs(2)).await;
                loop {
                    let alive = this
                        .update_in(cx, |this, window, cx| {
                            this.tick_scheduled_tasks(window, cx);
                        })
                        .is_ok();
                    if !alive {
                        break;
                    }
                    smol::Timer::after(std::time::Duration::from_secs(30)).await;
                }
            })
            .detach();
        }

        // 组件 toast：app 前台但没在看的 pane 有新通知时弹一条（右上角浮层，5s
        // 自动消失）。完全切到别的 app 时不弹 toast，走 terminal_view.rs 里的系统
        // 通知；正在看的那个 pane 直接吃掉待发消息，不弹——你自己看得见。
        // 每帧都要来取（不管是否前台），否则待发消息会一直攒着，等哪天恰好前台又
        // 不是当前 pane 时全冒出来。
        // 同时捞「任务完成 → 自动续跑」挂旗（先收集再处理，避免 run 时改 sessions）。
        let window_active = window.is_window_active();
        let mut task_continues: Vec<(String, String)> = Vec::new();
        for (ix, sess) in self.sessions.iter().enumerate() {
            let mut leaves = Vec::new();
            collect_leaves(&sess.layout, &mut leaves);
            let active_pane_id = sess.active.entity_id();
            for leaf in &leaves {
                let (toast, cont_cwd) = leaf.update(cx, |t, _cx| {
                    (t.take_pending_toast(), t.take_pending_task_continue())
                });
                if let Some(cwd) = cont_cwd {
                    let sid = leaf.read(cx).session_id().to_string();
                    task_continues.push((sid, cwd));
                }
                let Some(msg) = toast else { continue };
                let is_current_view = ix == active && leaf.entity_id() == active_pane_id;
                if window_active && !is_current_view {
                    window.push_notification(Notification::info(msg), cx);
                }
            }
        }
        for (sid, cwd) in task_continues {
            self.on_session_task_idle(&sid, &cwd, window, cx);
        }

        // 侧栏项目分组：后台刷新每个会话 cwd 的仓库身份（是不是 worktree + 分支名），
        // 让 worktree 的会话能跟主仓库聚在一起显示、标签带上分支名。侧栏一直显示
        // 全部项目，不像 git status/hotspot 那样只关心当前打开的那个，所以对
        // self.sessions 里出现过的所有 cwd 都要探测，而不是只探测 self.cur()。
        let repo_cwds: HashSet<String> = self.sessions.iter().filter_map(|s| s.cwd(cx)).collect();
        for cwd in repo_cwds {
            self.ensure_repo_info(cwd, cx);
        }

        // 各类后台操作（建/删 worktree、生成 commit message）失败时，错误信息暂存在
        // 这个字段（后台任务里没有 Window，弹不了通知），render 一开始就取走弹成通知。
        if let Some(msg) = self.background_error.take() {
            window.push_notification(Notification::error(msg), cx);
        }

        // Git 页：后台刷新改动列表 + 分支列表（git status/for-each-ref 慢，绝不在
        // render 里同步跑）。
        if self.view == MainView::Git {
            if let Some(root) = self.cur().and_then(|s| s.cwd(cx)) {
                self.ensure_git_watch(root.clone(), cx);
                self.ensure_git_status(root.clone(), cx);
                self.ensure_branches(root, cx);
            }
        }

        // 热力图页：后台刷新改动热力（git log 扫历史更慢，同样绝不同步跑）。
        if self.view == MainView::Hotspot {
            if let Some(root) = self.cur().and_then(|s| s.cwd(cx)) {
                self.ensure_hotspot(root, cx);
            }
        }

        // 历史会话页：后台刷新当前项目的会话列表 / 记忆列表（看当前是哪个子页）。
        if self.view == MainView::History {
            if let Some(root) = self.cur().and_then(|s| s.cwd(cx)) {
                match self.history_pane {
                    HistoryPane::Sessions => self.ensure_session_list(root, cx),
                    HistoryPane::Memories => self.ensure_memory_list(root, cx),
                }
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
                // 改动文件 M/A/D 标要用 git status；不强制用户先去过 Git 页才有数据，
                // Files 页自己也确保一份缓存新鲜（ensure_git_status 内部已有 TTL）。
                self.ensure_git_status(root.clone(), cx);
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
            let mem_due = self
                .debug_mem_sampled_at
                .is_none_or(|t| now.duration_since(t) >= Duration::from_secs(1));
            if mem_due {
                self.debug_mem_rss = mem_usage::current_rss_bytes();
                self.debug_mem_sampled_at = Some(now);
            }
            window.request_animation_frame();
        } else {
            self.last_frame = None;
            self.debug_mem_rss = None;
            self.debug_mem_sampled_at = None;
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
                        let status_dot = (status != AgentStatus::Idle && ix != active)
                            .then(|| status_color(status));
                        let e_act = this.clone();
                        let entity_id = entity_ids[ix];
                        let drag_title: SharedString = title.clone().into();
                        let e_drop = this.clone();
                        let e_close = this.clone();
                        let e_rename = this.clone();
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
                        // 会话内有分屏（>1 个 pane）时，展开出子行：一 pane 一行，各自标题 +
                        // 状态点，点击直接切到该会话并聚焦该 pane。只有 1 个 pane 的会话
                        // pane_items 为空，SidebarMenuItem 判定无 children 就不长出展开箭头，
                        // 行为跟改动前完全一样。
                        let pane_items: Vec<SidebarMenuItem> = if self.sessions[ix].pane_count() > 1 {
                            let mut leaves = Vec::new();
                            collect_leaves(&self.sessions[ix].layout, &mut leaves);
                            let active_pane_id = self.sessions[ix].active.entity_id();
                            leaves
                                .into_iter()
                                .map(|view| {
                                    let p_title = pane_title(&view, cx);
                                    let p_status = pane_status(&view, cx);
                                    let is_current_view =
                                        ix == active && view.entity_id() == active_pane_id;
                                    let p_dot = (p_status != AgentStatus::Idle && !is_current_view)
                                        .then(|| status_color(p_status));
                                    let e_pane_act = this.clone();
                                    let e_pane_rename = this.clone();
                                    let pane = view.clone();
                                    let rename_pane = view.clone();
                                    SidebarMenuItem::new(p_title)
                                        .icon(IconName::SquareTerminal)
                                        .active(is_current_view)
                                        .on_click(move |_ev, window, cx| {
                                            let pane = pane.clone();
                                            e_pane_act.update(cx, |ws, cx| {
                                                ws.activate_session_pane(ix, pane, window, cx)
                                            });
                                        })
                                        // 分屏子行也能改名，写到 pane 自己的 custom_title，
                                        // 不影响所属会话（父行）的名字。
                                        .context_menu(move |menu, _window, _cx| {
                                            let e_pane_rename = e_pane_rename.clone();
                                            let rename_pane = rename_pane.clone();
                                            let e_task = e_pane_rename.clone();
                                            let task_pane = rename_pane.clone();
                                            menu.item(PopupMenuItem::new("新建任务").on_click(
                                                move |_ev, window, cx| {
                                                    let pane = task_pane.clone();
                                                    e_task.update(cx, |ws, cx| {
                                                        ws.open_new_task_for_terminal(
                                                            &pane, window, cx,
                                                        );
                                                    });
                                                },
                                            ))
                                            .item(PopupMenuItem::new("重命名").on_click(
                                                move |_ev, window, cx| {
                                                    let target =
                                                        RenameTarget::Pane(rename_pane.clone());
                                                    e_pane_rename.update(cx, |ws, cx| {
                                                        ws.start_rename(target, window, cx)
                                                    });
                                                },
                                            ))
                                        })
                                        .suffix(move |_w, _cx| {
                                            div().children(
                                                p_dot.map(|c| div().size_2().rounded_full().bg(c)),
                                            )
                                        })
                                })
                                .collect()
                        } else {
                            Vec::new()
                        };
                        let item = SidebarMenuItem::new(title)
                            .icon(row_icon)
                            .active(ix == active)
                            // 分屏子行第一次出现（1 pane → >1 pane）时默认展开：SidebarMenuItem
                            // 内部用 window 按 id 记的持久 open 状态，default_open 只在该 id
                            // 首次创建（也就是 is_submenu 第一次为 true）时生效，之后用户手动
                            // 收起会一直记住，不会被这里重置。
                            .default_open(true)
                            .children(pane_items)
                            .on_click(move |_ev, window, cx| {
                                e_act.update(cx, |ws, cx| ws.activate(ix, window, cx));
                            })
                            .context_menu(move |menu, _window, _cx| {
                                let e_rename = e_rename.clone();
                                let e_task = e_rename.clone();
                                let sess_ix = ix;
                                menu.item(PopupMenuItem::new("新建任务").on_click(
                                    move |_ev, window, cx| {
                                        e_task.update(cx, |ws, cx| {
                                            if let Some(sess) = ws.sessions.get(sess_ix) {
                                                let pane = sess.active.clone();
                                                ws.open_new_task_for_terminal(&pane, window, cx);
                                            }
                                        });
                                    },
                                ))
                                .item(PopupMenuItem::new("重命名").on_click(
                                    move |_ev, window, cx| {
                                        e_rename.update(cx, |ws, cx| {
                                            ws.start_rename(RenameTarget::Session(ix), window, cx)
                                        });
                                    },
                                ))
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
                // worktree 相关：本组是不是 worktree 检出（决定右键菜单要不要露出「删除
                // Worktree」）、给「新建 Worktree」弹窗用的仓库名（就算本组自己就是
                // worktree，也要用仓库本名而不是「仓库名 · 分支名」这个复合展示名）、
                // 以及删除时 `git worktree remove` 要从哪个稳定目录发起。
                let repo_info_here = self.repo_info.get(cwd.as_str()).and_then(|(_, i)| i.clone());
                let is_worktree_group = repo_info_here.as_ref().is_some_and(|i| i.is_worktree());
                let repo_label = repo_info_here
                    .as_ref()
                    .and_then(|i| repo_label_from_common_dir(&i.common_dir))
                    .unwrap_or_else(|| project_name_for_cwd(&cwd));
                let worktree_main_root = repo_info_here
                    .as_ref()
                    .and_then(|i| main_repo_root_from_common_dir(&i.common_dir));
                let worktree_branch =
                    repo_info_here.as_ref().map(|i| i.branch.clone()).unwrap_or_default();
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
                        let repo_label = repo_label.clone();
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
                                    .dropdown_menu(move |menu, _window, cx| {
                                        let raw_cwd = cwd.clone();
                                        let repo_label = repo_label.clone();
                                        let e_worktree = e_new.clone();
                                        let cwd = (!cwd.is_empty()).then(|| cwd.clone());
                                        let cwd_new = cwd.clone();
                                        let e_term = e_new.clone();
                                        let entries = active_launch_entries(cx);
                                        let mut menu = menu.item(
                                            PopupMenuItem::new("新建终端")
                                                .icon(IconName::SquareTerminal)
                                                .on_click(move |_ev, _window, cx| {
                                                    let cwd = cwd_new.clone();
                                                    e_term.update(cx, |ws, cx| ws.add_session(cwd, cx));
                                                }),
                                        );
                                        for entry in entries {
                                            let label = entry.label;
                                            let command = entry.command;
                                            let cwd_launch = cwd.clone();
                                            let e_launch = e_new.clone();
                                            let icon = icon_for_launch_command(&command);
                                            menu = menu.item(
                                                PopupMenuItem::new(label.clone())
                                                    .icon(icon)
                                                    .on_click(move |_ev, _window, cx| {
                                                        let cwd = cwd_launch.clone();
                                                        let cmd = command.clone();
                                                        let name = label.clone();
                                                        e_launch.update(cx, |ws, cx| {
                                                            ws.add_session_with_launch(
                                                                cwd,
                                                                Some(cmd.as_str()),
                                                                Some(name.as_str()),
                                                                cx,
                                                            );
                                                        });
                                                    }),
                                            );
                                        }
                                        // 临时终端（$HOME）不是真项目，建不了 worktree；空 cwd
                                        // 同理（会话还没上报出目录）。
                                        menu
                                        .when(!is_scratch_group && !raw_cwd.is_empty(), |menu| {
                                            menu.separator().item(
                                                PopupMenuItem::new("新建 Worktree…")
                                                    .icon(IconName::Folder)
                                                    .on_click(move |_ev, window, cx| {
                                                        let repo_root = raw_cwd.clone();
                                                        let repo_label = repo_label.clone();
                                                        e_worktree.update(cx, |ws, cx| {
                                                            ws.start_new_worktree(
                                                                repo_root,
                                                                repo_label,
                                                                window,
                                                                cx,
                                                            )
                                                        });
                                                    }),
                                            )
                                        })
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
                    // 只有 worktree 分组才露出「删除 Worktree」——主仓库/临时终端那组
                    // 没有对应的 git 操作。
                    .when(is_worktree_group, |item| {
                        let e_del = this.clone();
                        let del_path = cwd.clone();
                        let del_main_root = worktree_main_root.clone().unwrap_or_else(|| cwd.clone());
                        let del_branch = worktree_branch.clone();
                        item.context_menu(move |menu, _window, _cx| {
                            let e_del = e_del.clone();
                            let del_path = del_path.clone();
                            let del_main_root = del_main_root.clone();
                            let del_branch = del_branch.clone();
                            menu.item(PopupMenuItem::new("删除 Worktree").icon(IconName::Delete).on_click(
                                move |_ev, _window, cx| {
                                    let del_path = del_path.clone();
                                    let del_main_root = del_main_root.clone();
                                    let del_branch = del_branch.clone();
                                    e_del.update(cx, |ws, cx| {
                                        ws.start_delete_worktree(del_path, del_main_root, del_branch, cx)
                                    });
                                },
                            ))
                        })
                    })
                    .children(sess_items)
            })
            .collect();

        let overview_active = self.view == MainView::Overview;
        let e_overview = this.clone();
        // 总览行右侧的状态摘要：四态各自的会话数，零的不显示。侧栏折起来看不到会话行的
        // 状态点，agent 一多也懒得逐行扫——「有几个在等我」应该在第一眼落点上直接答完。
        // 顺序即紧急度（等审批 → 需处理 → 运行中 → 已完成），与 AgentStatus 声明序一致。
        let status_counts: [(AgentStatus, usize); 4] = [
            AgentStatus::WaitingApproval,
            AgentStatus::NeedsAttention,
            AgentStatus::Running,
            AgentStatus::Done,
        ]
        .map(|st| (st, statuses.iter().filter(|s| **s == st).count()));

        let sidebar_el = Sidebar::new("workspace-sidebar")
            .collapsible(SidebarCollapsible::Offcanvas)
            // 宽度交给外层 resizable_panel 控制（可拖），这里填满 panel。
            // 品牌已移到顶部标题栏，侧栏直接从「会话」开始，避免重复。
            .w(relative(1.))
            // 总览：不挂在任何项目下的全局入口，跟当前在哪个项目无关，随时点得到。
            // 新建终端挪到底部跟「打开项目」放一起了（见 footer），都是「开个新地方干活」
            // 这一类操作，归在一块更好找。
            //
            // 放 header 而不是 `SidebarGroup::new("")`：SidebarGroup 无条件渲染分组标题行
            // 且写死 h_8，空标题也照占 32px，「总览」上方会白白空出一大块。header 没有这个
            // 标题行，左右内边距（px_3）跟下面的分组内容一致，视觉照样对齐。
            .header(
                // 外面包一层 w_full：header 容器是 h_flex，不撑宽的话「总览」行只有文字那么宽，
                // hover 高亮和点击热区都缩成一小条，跟下面的会话行对不齐。
                div().w_full().child(
                SidebarMenu::new()
                    .children([SidebarMenuItem::new("总览")
                        .icon(IconName::LayoutDashboard)
                        .active(overview_active)
                        .on_click(move |_ev, window, cx| {
                            e_overview.update(cx, |ws, cx| {
                                ws.view = MainView::Overview;
                                ws.refresh_git(cx); // 进总览 → 后台刷新 git
                                cx.notify();
                            });
                            // 总览页没有可聚焦元素，focus 显式认领到根节点，
                            // 不然 Cmd+Shift+F 等全局快捷键在这页会收不到事件。
                            let h = e_overview.read(cx).focus_handle.clone();
                            window.focus(&h, cx);
                        })
                        .suffix(move |_w, cx: &mut App| {
                            let muted = cx.theme().muted_foreground;
                            h_flex().gap_2().items_center().children(
                                status_counts.into_iter().filter(|(_, n)| *n > 0).map(
                                    |(st, n)| {
                                        h_flex()
                                            .gap_1()
                                            .items_center()
                                            .child(
                                                div()
                                                    .size_2()
                                                    .rounded_full()
                                                    .bg(status_color(st)),
                                            )
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(muted)
                                                    .child(n.to_string()),
                                            )
                                    },
                                ),
                            )
                        })])
                    .render("sidebar-overview", window, cx),
                ),
            )
            .child({
                // 侧栏「任务」：任务总览入口 + 执行中快捷项（新建在总览页顶栏 / 终端右键）
                let e_task = this.clone();
                let tasks_active = self.view == MainView::Tasks;
                let mut task_items: Vec<SidebarMenuItem> = Vec::new();
                {
                    let e = e_task.clone();
                    let e2 = e_task.clone();
                    task_items.push(
                        SidebarMenuItem::new("任务总览")
                            .icon(IconName::Bot)
                            .active(tasks_active)
                            .on_click(move |_ev, window, cx| {
                                e.update(cx, |ws, cx| {
                                    ws.view = MainView::Tasks;
                                    cx.notify();
                                });
                                let h = e2.read(cx).focus_handle.clone();
                                window.focus(&h, cx);
                            }),
                    );
                }
                let mut listed = tasks::TaskStore::load().tasks;
                listed.retain(|t| t.column.is_active());
                listed.sort_by_key(|t| t.column.sidebar_rank());
                for t in listed.into_iter().take(12) {
                    let tid = t.id.clone();
                    let e = e_task.clone();
                    let e2 = e_task.clone();
                    let title = t.title.clone();
                    let st_color = rgb(t.column.color());
                    let selected = self.task_selected.as_deref() == Some(tid.as_str())
                        && self.view == MainView::Terminal;
                    task_items.push(
                        SidebarMenuItem::new(title)
                            .icon(IconName::Bot)
                            .active(selected)
                            .on_click(move |_ev, window, cx| {
                                let tid = tid.clone();
                                e.update(cx, |ws, cx| {
                                    ws.focus_or_run_task(&tid, window, cx);
                                });
                                let h = e2.read(cx).focus_handle.clone();
                                window.focus(&h, cx);
                            })
                            .suffix(move |_w, _cx: &mut App| {
                                div()
                                    .size_2()
                                    .rounded_full()
                                    .bg(st_color)
                                    .into_any_element()
                            }),
                    );
                }
                SidebarGroup::new("任务").child(SidebarMenu::new().children(task_items))
            })
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
                    // 有新版本时缀红点（跟设置齿轮同一信号），点击改跳 Releases 看更新内容。
                    .child({
                        let has_update = self.update_available();
                        let version: AnyElement = if has_update {
                            Badge::new()
                                .dot()
                                .child(concat!("v", env!("CARGO_PKG_VERSION")))
                                .into_any_element()
                        } else {
                            concat!("v", env!("CARGO_PKG_VERSION")).into_any_element()
                        };
                        div()
                            .id("version-github-link")
                            .w_full()
                            .flex()
                            .justify_center()
                            .cursor_pointer()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .hover(|s| s.text_color(cx.theme().foreground))
                            .child(version)
                            .on_mouse_down(MouseButton::Left, move |_, _window, cx| {
                                cx.open_url(if has_update {
                                    "https://github.com/smelt-ai/smelt/releases"
                                } else {
                                    "https://github.com/smelt-ai/smelt"
                                });
                            })
                    }),
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
            .font_family(terminal_view::font_family())
            // 见 focus_handle 字段注释：非终端页面没有可聚焦的子元素时，靠这个把
            // window 的 focus 兜底钉在这层，保证下面的全局 on_key_down 收得到事件。
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &Quit, _window, cx| {
                this.show_quit_confirm = true;
                cx.notify();
            }))
            // Cmd+, / 应用菜单「设置…」：跟齿轮图标共用同一个独立设置窗口。
            .on_action(cx.listener(|this, _: &OpenSettings, window, cx| {
                if this.llm_inputs.is_none() {
                    this.init_llm_inputs(window, cx);
                }
                // 不动 nonce：窗口已开着就保持用户当前所在页，只是把它提到前台；
                // 但下次新开窗口得回到外观页，不能停在「检查更新…」跳过去的那页。
                this.settings_page_ix = SETTINGS_PAGE_APPEARANCE;
                this.open_settings_window(cx);
            }))
            // Cmd+Shift+N：全局新建任务（侧栏任务列表 + 弹窗）。
            .on_action(cx.listener(|this, _: &NewTask, window, cx| {
                this.open_new_task_modal(window, cx);
            }))
            // 应用菜单「检查更新…」：顺手发起一次检查，再把设置窗口开到「更新」页看进度。
            .on_action(cx.listener(|this, _: &CheckForUpdate, window, cx| {
                if this.llm_inputs.is_none() {
                    this.init_llm_inputs(window, cx);
                }
                if !matches!(
                    this.update_status,
                    updater::UpdateStatus::Checking
                        | updater::UpdateStatus::Downloading { .. }
                        | updater::UpdateStatus::Installing { .. }
                ) {
                    this.check_for_update(false, cx);
                }
                this.settings_page_ix = SETTINGS_PAGE_UPDATE;
                this.settings_page_nonce += 1;
                this.open_settings_window(cx);
            }))
            // 文件内容视图右键菜单里的「发送选中内容到终端」，见 send_open_file_selection。
            .on_action(cx.listener(|this, _: &SendSelectionToTerminal, _window, cx| {
                this.send_open_file_selection(cx);
            }))
            // 从 Finder 拖文件/文件夹进窗口 → 当作项目开新标签（文件取其父目录）。
            .on_drop::<ExternalPaths>(cx.listener(|this, ep: &ExternalPaths, _window, cx| {
                this.open_paths(ep.paths(), cx);
            }))
            // 全局快捷键：Cmd+K 面板 / Cmd+B 侧栏 / Cmd+[ ] 切当前会话内的 pane /
            // Cmd+1~9 跳到第 N 个会话（键位分工对齐 iTerm2）
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                let ks = &ev.keystroke;
                // 文件树键盘导航：搜索框 / 编辑器聚焦时不抢键。
                if this.view == MainView::Files && !ks.modifiers.platform && !ks.modifiers.control {
                    use gpui::Focusable;
                    let search_focused = this.file_filter.as_ref().is_some_and(|s| {
                        s.read(cx).focus_handle(cx).is_focused(window)
                    });
                    let editor_focused = this.open_file.as_ref().is_some_and(|of| {
                        of.editor.read(cx).focus_handle(cx).is_focused(window)
                    });
                    if !search_focused && !editor_focused {
                        match ks.key.as_str() {
                            "up" => {
                                this.file_tree_move_selection(-1, cx);
                                return;
                            }
                            "down" => {
                                this.file_tree_move_selection(1, cx);
                                return;
                            }
                            "left" => {
                                this.file_tree_key_left(cx);
                                return;
                            }
                            "right" => {
                                this.file_tree_key_right(window, cx);
                                return;
                            }
                            "enter" => {
                                this.file_tree_key_enter(window, cx);
                                return;
                            }
                            _ => {}
                        }
                    }
                }
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
                    // 切当前会话内的活动 pane（分屏），不是切会话——切会话见下面的 Cmd+1~9。
                    "[" => this.cycle_pane(-1, window, cx),
                    "]" => this.cycle_pane(1, window, cx),
                    // Cmd+1~9：直接跳到侧栏第 N 个会话（对齐 iTerm2），超出现有会话数就
                    // 什么都不做。
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" => {
                        let n = ks.key.as_bytes()[0] - b'1';
                        let ix = n as usize;
                        if ix < this.sessions.len() {
                            this.activate(ix, window, cx);
                        }
                    }
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
                    // Cmd+Shift+F 切换调试 HUD（右上角帧率 + 内存）
                    "f" if ks.modifiers.shift => {
                        this.debug_hud = !this.debug_hud;
                        this.fps_ema = 0.0;
                        this.last_frame = None;
                        this.debug_mem_rss = None;
                        this.debug_mem_sampled_at = None;
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
                                    let needs_attention =
                                        self.update_available() || self.daemon_outdated == Some(true);
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
                                                // 同 OpenSettings：新开的窗口回到外观页。
                                                this.settings_page_ix = SETTINGS_PAGE_APPEARANCE;
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
                    .children((!matches!(self.view, MainView::Overview | MainView::Tasks))
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
                            .on_click(cx.listener(|this, ix: &usize, window, cx| {
                                this.view = match *ix {
                                    0 => MainView::Terminal,
                                    1 => MainView::Files,
                                    2 => MainView::Git,
                                    3 => MainView::Hotspot,
                                    _ => MainView::History,
                                };
                                // 切到非终端页面时把 focus 认领到根节点：这几页自己
                                // 没有可聚焦元素，不然全局快捷键在这些页面会失灵。
                                // 切回终端页要显式把 focus 还给当前活动 pane——TerminalView
                                // 的首帧自动聚焦（did_focus）只在它第一次渲染时触发一次，
                                // 从文件树/Git 页切回来时它早就渲染过了，不会再抢，不然
                                // 焦点会一直留在根节点上，得手动点一下终端才能输入。
                                if this.view != MainView::Terminal {
                                    window.focus(&this.focus_handle, cx);
                                } else {
                                    this.focus_active(window, cx);
                                }
                                cx.notify();
                            }))
                            .child(Tab::new().label("终端"))
                            .child(Tab::new().label("文件树"))
                            .child(Tab::new().label("Git"))
                            .child(Tab::new().label("热力图"))
                            .child(Tab::new().label("历史会话"))
                    }))
                    .child(match self.view {
                        MainView::Overview => self.render_overview(window, cx),
                        MainView::Tasks => self.render_tasks_page(window, cx),
                        MainView::Terminal => content,
                        MainView::Files => {
                            let cwd = self.cur().and_then(|s| s.cwd(cx));
                            // 有查询串 → 显示搜索结果；否则显示文件树。
                            let has_query = self
                                .file_filter
                                .as_ref()
                                .is_some_and(|s| !s.read(cx).value().trim().is_empty());
                            // 打开文件后的 reveal：祖先目录缓存齐了就滚到树里对应行。
                            if !has_query {
                                self.try_flush_file_tree_reveal(cx);
                            }
                            let body = if has_query {
                                match &self.search_results {
                                    Some(state) => {
                                        search_results_view(state, &self.file_tree_scroll, cx)
                                    }
                                    // ensure_search 已在 render 顶部同步置位，通常到不了这里。
                                    None => div().flex_1().into_any_element(),
                                }
                            } else {
                                let open_path = self.open_file.as_ref().map(|of| of.path.as_str());
                                let selected = self.file_tree_selected.as_deref();
                                // 借当前 git status 缓存给改动文件标 M/A/D；没有缓存就是 None。
                                let changed_files = cwd
                                    .as_ref()
                                    .and_then(|r| self.git_status.get(r))
                                    .map(|(_, d)| d.files.as_slice());
                                file_tree(
                                    cwd,
                                    &self.expanded,
                                    &self.dir_cache,
                                    &self.file_tree_scroll,
                                    open_path,
                                    selected,
                                    self.file_tree_w,
                                    changed_files,
                                    cx,
                                )
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
                                    // 文件树列宽可拖拽（拖右边框），不再写死 260px——文件名
                                    // 超长时至少还能拖宽了看，配合行上的 tooltip 一起解决
                                    // 「长文件名看不全」的问题。
                                    h_resizable("file-tree-split")
                                        .with_state(&self.file_tree_resize)
                                        .child(
                                            resizable_panel()
                                                .size(px(self.file_tree_w))
                                                .size_range(px(160.)..px(480.))
                                                .child(
                                                    div()
                                                        .size_full()
                                                        .flex()
                                                        .flex_col()
                                                        .min_h_0()
                                                        .border_r_1()
                                                        .border_color(c_border)
                                                        .children(search_box)
                                                        .child(body),
                                                ),
                                        )
                                        .child(resizable_panel().child(content)),
                                )
                        }
                        MainView::Git => {
                            let cwd = self.cur().and_then(|s| s.cwd(cx));
                            let status =
                                cwd.as_ref().and_then(|r| self.git_status.get(r).map(|(_, d)| d));
                            let branches =
                                cwd.as_ref().and_then(|r| self.branches.get(r).map(|(_, d)| d));
                            // 评论输入框懒创建（需要 window），跟文件树搜索框同一套模式。
                            if self.git_diff.is_some() && self.diff_comment_input.is_none() {
                                use gpui_component::input::InputState;
                                let state = cx.new(|cx| {
                                    InputState::new(window, cx)
                                        .placeholder("给选中的行写评论，发送前可以再改改…")
                                });
                                self.diff_comment_input = Some(state);
                            }
                            // commit message 输入框懒创建，跟上面评论框同一套模式；只要
                            // 进了 Git 页就常驻（不像评论框依赖已经打开某个 diff）。
                            if self.commit_msg_input.is_none() {
                                use gpui_component::input::InputState;
                                let state = cx.new(|cx| {
                                    InputState::new(window, cx)
                                        .placeholder("Commit message（点「生成」用 AI 起草，也可以自己写）")
                                });
                                self.commit_msg_input = Some(state);
                            }
                            git_view(
                                cwd.clone(),
                                status,
                                branches,
                                &self.git_diff,
                                self.diff_split,
                                &self.diff_selected,
                                self.diff_comment_input.as_ref(),
                                self.commit_msg_input.as_ref(),
                                self.commit_msg_generating,
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
                            let list_state = match (&cwd, sessions) {
                                (_, None) => HistoryListState::Loading,
                                (Some(_), Some(s)) if s.is_empty() => HistoryListState::Empty,
                                (Some(cwd), Some(s)) => {
                                    HistoryListState::Ready(self.ensure_session_table(cwd, s, window, cx))
                                }
                                (None, _) => HistoryListState::Empty,
                            };
                            // 没选项目时给 Some(空表)，走「还没有记忆」而不是一直转圈。
                            let memories = match &cwd {
                                Some(root) => self.memory_list.get(root).map(|(_, d)| d.clone()),
                                None => Some(Rc::new(Vec::new())),
                            };
                            history_view(
                                self.history_pane,
                                list_state,
                                &self.session_detail,
                                memories,
                                self.memory_selected,
                                cx,
                            )
                        }
                    }),
                    )),
                ),
            )
            // 命令面板（最上层）
            .children(palette_overlay)
            // 退出确认拦截弹层
            .children(self.show_quit_confirm.then(|| self.render_quit_confirm(cx)))
            // 会话重命名拦截弹层
            .children(self.rename_target.is_some().then(|| self.render_rename_session(cx)))
            // 新建任务弹窗
            .children(self.show_new_task_modal.then(|| self.render_new_task_modal(cx)))
            // 新建 Worktree 拦截弹层
            .children(self.new_worktree_target.is_some().then(|| self.render_new_worktree_dialog(cx)))
            // 删除 Worktree 确认拦截弹层
            .children(self.delete_worktree_target.is_some().then(|| self.render_delete_worktree_confirm(cx)))
            // 删除文件二次确认拦截弹层
            .children(self.delete_file_target.is_some().then(|| self.render_delete_file_confirm(cx)))
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
            // 调试 HUD：右上角帧率 + 帧耗时 + RSS（Cmd+Shift+F 切换）
            .children(self.debug_hud.then(|| {
                let fps = self.fps_ema;
                let ms = if fps > 0.0 { 1000.0 / fps } else { 0.0 };
                let mem = self
                    .debug_mem_rss
                    .map(mem_usage::format_rss)
                    .unwrap_or_else(|| "—".into());
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
                    .font_family(terminal_view::font_family())
                    .text_xs()
                    .text_color(color)
                    .child(format!("{fps:.0} FPS · {ms:.1} ms · RSS {mem}"))
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
    // 菜单栏常驻图标/下拉菜单点击：见 status_item.rs 顶部注释，回调发生在纯 AppKit 层
    // （没有 GPUI 的 cx），一样经 channel 转发到下面 run() 里 drain。
    let (status_tx, status_rx) = smol::channel::unbounded::<status_item::StatusItemEvent>();

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
        // 内嵌终端默认字体 JetBrainsMono Nerd Font Mono（Regular/Bold），Ghostty 同款
        // 思路：默认字体自己带，不赌用户装没装——任何机器上默认字体族都能解析成功，
        // 杜绝"没装字体 → 测量/渲染各自 fallback 到不同字体 → 列宽错乱"。它是打过
        // Nerd Font 补丁的完整版，自带全部图标码位，兼任图标 fallback（用户在设置页
        // 自选的字体缺图标时落到它，见 terminal_view::terminal_font）。
        cx.text_system()
            .add_fonts(vec![
                std::borrow::Cow::Borrowed(
                    include_bytes!("../../../assets/fonts/JetBrainsMonoNerdFontMono-Regular.ttf")
                        .as_slice(),
                ),
                std::borrow::Cow::Borrowed(
                    include_bytes!("../../../assets/fonts/JetBrainsMonoNerdFontMono-Bold.ttf")
                        .as_slice(),
                ),
            ])
            .expect("加载内嵌字体失败");
        // 应用菜单栏：macOS 顶部「Smelt」菜单，含「设置… ⌘,」+「退出 Smelt ⌘Q」
        // （跟齿轮图标一样开独立设置窗口，符合 mac 惯例——系统偏好设置一般都在这）。
        cx.bind_keys([
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("cmd-,", OpenSettings, None),
            KeyBinding::new("cmd-shift-n", NewTask, None),
            // 把 Tab/Shift-Tab 从 gpui-component Root 的全局焦点跳转手里要回来，
            // 终端聚焦时改发给 shell（见 terminal_view.rs 里 TerminalTab 的注释）。
            KeyBinding::new("tab", terminal_view::TerminalTab, Some("Terminal")),
            KeyBinding::new("shift-tab", terminal_view::TerminalBackTab, Some("Terminal")),
        ]);
        cx.set_menus(vec![Menu::new("Smelt").items([
            MenuItem::action("新建任务…", NewTask),
            MenuItem::action("检查更新…", CheckForUpdate),
            MenuItem::Separator,
            MenuItem::action("设置…", OpenSettings),
            MenuItem::Separator,
            MenuItem::action("退出 Smelt", Quit),
        ])]);

        // 外观设置：读盘设为全局单例，据此确定窗口背景外观（透明 / 模糊）+ 明暗主题
        // （默认深色，与终端配色一致；用户可在设置页切换，选择会持久化）。
        let appearance = load_appearance();
        let window_bg = appearance.window_bg();
        Theme::change(appearance.theme_mode, None, cx);
        terminal::set_dark_mode(appearance.theme_mode.is_dark());
        terminal_view::set_font_px(appearance.font_px);
        terminal_view::set_font_family(&appearance.font_family);
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

        // 菜单栏图标/下拉菜单事件：主窗口还活着就前置 app（跳会话时顺带切过去），
        // 没了就跟 on_reopen 一样重开一扇（此时会话下标已经没意义，只重开窗口）。
        cx.spawn(async move |cx| {
            while let Ok(event) = status_rx.recv().await {
                let alive = current_ws_status.borrow().as_ref().is_some_and(|w| w.upgrade().is_some());
                if alive {
                    if let status_item::StatusItemEvent::JumpToSession(ix) = event {
                        let ws = current_ws_status.borrow().clone();
                        if let Some(ws) = ws {
                            let _ = ws.update(cx, |ws, cx| {
                                if ix < ws.sessions.len() {
                                    ws.active_session = ix;
                                    if ws.view == MainView::Overview {
                                        ws.view = MainView::Terminal;
                                    }
                                    ws.save_state(cx);
                                    cx.notify();
                                }
                            });
                        }
                    }
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

#[cfg(test)]
mod pane_state_tests {
    use super::PaneState;

    /// pane 自定义名必须能跟着 Leaf 存下来、读回来（否则重开 GUI 就丢名字）。
    #[test]
    fn leaf_custom_title_roundtrips() {
        let leaf = PaneState::Leaf {
            cwd: Some("/tmp/x".into()),
            id: Some("sid-1".into()),
            custom_title: Some("跑测试的终端".into()),
            launch_label: Some("Claude Code".into()),
        };
        let json = serde_json::to_string(&leaf).unwrap();
        let back: PaneState = serde_json::from_str(&json).unwrap();
        match back {
            PaneState::Leaf {
                custom_title,
                launch_label,
                id,
                cwd,
            } => {
                assert_eq!(custom_title.as_deref(), Some("跑测试的终端"));
                assert_eq!(launch_label.as_deref(), Some("Claude Code"));
                assert_eq!(id.as_deref(), Some("sid-1"));
                assert_eq!(cwd.as_deref(), Some("/tmp/x"));
            }
            _ => panic!("应当反序列化成 Leaf"),
        }
    }

    /// 旧存档没有 custom_title / launch_label 字段，必须读成 None 而不是解析失败。
    #[test]
    fn old_archive_without_custom_title_still_loads() {
        let old = r#"{"Leaf":{"cwd":"/tmp/x","id":"sid-1"}}"#;
        let back: PaneState = serde_json::from_str(old).unwrap();
        match back {
            PaneState::Leaf {
                custom_title,
                launch_label,
                id,
                ..
            } => {
                assert!(custom_title.is_none(), "旧存档不该凭空冒出自定义名");
                assert!(launch_label.is_none(), "旧存档不该凭空冒出启动项名");
                assert_eq!(id.as_deref(), Some("sid-1"));
            }
            _ => panic!("应当反序列化成 Leaf"),
        }
    }
}
