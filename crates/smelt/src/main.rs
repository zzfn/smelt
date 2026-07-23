//! smelt 工作台 —— 基于 gpui-component 的桌面窗口。
//!
//! Workspace 管理多个终端标签（TerminalView）：顶部标签栏切换 / 新建 / 关闭，
//! 下方渲染当前活动终端。每个终端各自独立（PTY、IME、滚动、resize）。
//!
//! 运行： cargo run --bin smelt

// ACP 连接层已经搬进 smelt_core::acp_conn（给 smeltd 未来托管 ACP 会话铺路），
// 这里不再 mod acp;，用的地方直接引 smelt_core::acp_conn。
mod acp_completion;
mod acp_view;
mod agent;
mod claude_memory;
mod dock;
mod file_tree;
mod git_log;
mod git_log_view;
mod git_panel;
mod hotspot;
mod inspector;
mod json_store;
mod markdown_mermaid;
mod mem_usage;
use smelt_core::osc;
// 权限菜单解析：唯一真源，与 smeltd 共用 smelt-core 里的同一份（smeltd 解析后随
// SessionState 下发给手机端）。曾经 Rust/TS 各一份并已实测漂移，别再在别处另写一版。
use smelt_core::permission_menu;
// 网格 → 文本行：同样与 smeltd 共用，避免两端各写一遍逐格拼行的宽字符/零宽处理。
use smelt_core::term_text;
mod pet;
mod session_history;
mod session_list;
mod settings;
mod skills;
mod stage;
mod status_bar;
mod status_item;
mod tasks;
mod toast;
mod terminal;
mod terminal_view;
mod ui_theme;
mod updater;
mod usage_stats;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use gpui::*;
use gpui::prelude::FluentBuilder;
use gpui::InteractiveElement;
use gpui_component::badge::Badge;
use gpui_component::color_picker::ColorPickerState;
use gpui_component::input::Input;
use gpui_component::list::{List, ListDelegate, ListEvent, ListItem, ListState};
use gpui_component::notification::Notification;
use gpui_component::slider::SliderState;
use gpui_component::resizable::{
    h_resizable, resizable_panel, v_resizable, ResizablePanelEvent, ResizableState,
};
use gpui_component::*;
use notify::RecommendedWatcher;
use terminal_view::TerminalView;

use file_tree::{
    file_content_pane, file_tree, search_results_view, DeleteFileTarget, OpenFile, SearchState,
};
use git_panel::{
    git_view, run_git, BranchList, DeleteWorktreeTarget, GitDiff, GitStatusData, RepoInfo,
};
use hotspot::hotspot_view;
use session_history::{history_view, HistoryListState, HistoryPane};
use settings::{load_appearance, load_launch_config, Appearance, LlmInputs};
use usage_stats::format_count;


// Cmd+Q 退出的应用级 action（gpui 无默认菜单栏，需自建菜单栏 + 键位绑定）。
gpui::actions!(
    smelt,
    [Quit, OpenSettings, CheckForUpdate, ReportIssue, SendSelectionToTerminal, NewTask]
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

/// 舞台覆盖页（stage_override 的取值）：会话总览 / 任务总览 / 文件树 / Git /
/// 热力图 / 历史。曾是主区 TabBar 的互斥视图（含 Terminal 变体）；改版后终端
/// 舞台 = `stage_override == None`，这里只剩「盖在舞台上的全屏页」。
#[derive(Clone, Copy, PartialEq)]
enum MainView {
    Overview,
    /// 任务总览（卡片网格，对齐会话总览交互，内容只含任务）。
    Tasks,
    /// 「文件树 + 内容」双栏全宽（inspector FILES 面板 ⤢ 提升上来；此时面板收起）。
    Files,
    /// 只有文件内容：树留在右侧停靠面板里，舞台不再摆第二份。
    /// 从停靠的 FILES 面板点文件走这条，不是整页跳转。
    FileDetail,
    /// 「变更列表 + diff」双栏全宽（inspector GIT 面板 ⤢ 提升上来；此时面板收起）。
    Git,
    /// 只有 diff：变更列表留在右侧停靠面板里，舞台只出详情。
    DiffDetail,
    Hotspot,
    History,
}

/// Git 页内部的子页。对标 JetBrains 的 Git 工具窗口——「提交」和「日志」是同一个
/// 窗口里的两个视图，不占两个顶层标签。
#[derive(Clone, Copy, PartialEq)]
enum GitTab {
    /// 工作区改动：文件树 + diff + 暂存 / 提交。
    Changes,
    /// 提交历史 + 分支图。
    Log,
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

impl AgentStatus {
    /// 优先级序（越小越紧急），与声明序一致：排序、聚合（项目 rail 的组内
    /// 最高优先级状态点）共用。
    fn rank(self) -> u8 {
        match self {
            AgentStatus::WaitingApproval => 0,
            AgentStatus::NeedsAttention => 1,
            AgentStatus::Running => 2,
            AgentStatus::Done => 3,
            AgentStatus::Idle => 4,
        }
    }
}

/// 总览页筛选：基于 AgentStatus / 状态通道，不猜 TUI。
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum OverviewFilter {
    #[default]
    All,
    /// 等批准 + 需要处理
    NeedsMe,
    Running,
}

/// 守护上报的会话状态镜像（全局单例，跨窗口共享）。key = smeltd session id
/// （每个 pane 一个，见 TerminalView.session_id——不是每个 GUI Session 一个）。
/// 由 main.rs 启动时那条常驻 subscribe 转发任务维护，`Session::status`/`pane_status`
/// 读它；daemon 没有对应 id 的数据（老版本守护/还没收到第一条上报）就退化到 OSC 猜测。
#[derive(Clone, Default)]
struct DaemonStates(Arc<Mutex<HashMap<String, terminal::DaemonSessionState>>>);

impl Global for DaemonStates {}

/// 状态通道待弹出的应用内 Notification（subscribe 线程无 Window，render 时 drain）。
#[derive(Clone, Default)]
struct PendingAgentNotifs(Arc<Mutex<Vec<(String, String, bool)>>>);
// (title, message, is_approval)

impl Global for PendingAgentNotifs {}

/// 取某个 pane 对应的守护状态；没有全局单例（比如极早期尚未走到注册那一步）或
/// 那个 session id 还没有数据都返回 None。
fn daemon_state_for(view: &Entity<TerminalView>, cx: &App) -> Option<terminal::DaemonSessionState> {
    let id = view.read(cx).session_id().to_string();
    cx.try_global::<DaemonStates>()?.0.lock().unwrap().get(&id).cloned()
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
/// 旧侧栏的项目行拖拽已撤；待接到项目 rail 后复活（收尾阶段定去留）。
#[derive(Clone)]
#[allow(dead_code)]
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

/// 一个会话的内容形态。Term 是第一通道（PTY 分屏树），Acp 是第二通道（结构化
/// 消息流，见 docs/project-report.md 第 5 节）——后者不参与分屏，一会话一视图。
enum SessionKind {
    /// 终端会话 = 一棵独立分屏树 + 会话内当前活动 pane（终端）。
    Term {
        layout: Pane,
        active: Entity<TerminalView>,
    },
    /// ACP 消息流会话：单视图，不参与分屏。
    Acp(Entity<acp_view::AcpView>),
}

/// 侧栏每条对应一个会话；主区显示当前会话的内容（分屏树或 ACP 消息流）。
struct Session {
    kind: SessionKind,
    /// 用户手动改过的会话名（侧栏右键「重命名」）；None = 用下面 title() 的自动推导。
    custom_title: Option<String>,
    /// ACP 会话内容变化（AcpViewEvent::Changed）→ save_state 的订阅；Term 会话
    /// 没有（终端内容不经这条通道持久化，走 daemon session id 就够）。
    _acp_persist_sub: Option<gpui::Subscription>,
}

impl Session {
    /// 单终端会话。
    fn single(view: Entity<TerminalView>) -> Self {
        Self {
            kind: SessionKind::Term { layout: Pane::Leaf(view.clone()), active: view },
            custom_title: None,
            _acp_persist_sub: None,
        }
    }

    /// 会话身份锚点：侧栏选中态、拖拽、activate 等都拿它做「是同一个会话吗」比较。
    /// Term = 活动终端的 entity id。
    fn anchor_id(&self) -> EntityId {
        match &self.kind {
            SessionKind::Term { active, .. } => active.entity_id(),
            SessionKind::Acp(view) => view.entity_id(),
        }
    }

    /// 终端会话的活动 pane；ACP 会话返回 None（调用方借此天然跳过终端专属操作）。
    fn active_term(&self) -> Option<&Entity<TerminalView>> {
        match &self.kind {
            SessionKind::Term { active, .. } => Some(active),
            SessionKind::Acp(_) => None,
        }
    }

    /// 切换终端会话的活动 pane；非终端会话是 no-op。
    fn set_active_term(&mut self, view: Entity<TerminalView>) {
        match &mut self.kind {
            SessionKind::Term { active, .. } => *active = view,
            SessionKind::Acp(_) => {}
        }
    }

    /// 终端会话的分屏树；ACP 会话没有。
    fn term_layout(&self) -> Option<&Pane> {
        match &self.kind {
            SessionKind::Term { layout, .. } => Some(layout),
            SessionKind::Acp(_) => None,
        }
    }

    fn term_layout_mut(&mut self) -> Option<&mut Pane> {
        match &mut self.kind {
            SessionKind::Term { layout, .. } => Some(layout),
            SessionKind::Acp(_) => None,
        }
    }

    /// 收集会话内全部终端叶子（ACP 会话得到空列表）。
    fn term_leaves(&self) -> Vec<Entity<TerminalView>> {
        let mut v = Vec::new();
        if let Some(layout) = self.term_layout() {
            collect_leaves(layout, &mut v);
        }
        v
    }

    /// 侧栏行图标：终端会话按启动方式（LaunchKind）对应，与「+」菜单图标一一对应。
    /// 新会话列表改用类型点（agent 紫圆 / 终端绿方），此图标暂时闲置——
    /// 收尾阶段决定是否用回行首或删除。
    #[allow(dead_code)]
    fn row_icon(&self, cx: &App) -> IconName {
        match &self.kind {
            SessionKind::Term { active, .. } => match active.read(cx).launch_kind() {
                terminal_view::LaunchKind::Claude => IconName::Asterisk,
                terminal_view::LaunchKind::Codex => IconName::Bot,
                terminal_view::LaunchKind::Copilot => IconName::Github,
                terminal_view::LaunchKind::Terminal => IconName::SquareTerminal,
            },
            SessionKind::Acp(_) => IconName::Bot,
        }
    }

    /// 会话标题：用户重命名过就用那个；否则仅当终端标题是 Claude Code 风格（✳ 或
    /// Braille spinner 开头）时取它的任务名，再否则回退 cwd 末段——避免把普通 shell 的
    /// user@host:path 标题当任务名。
    fn title(&self, cx: &App) -> String {
        self.custom_title.clone().unwrap_or_else(|| match &self.kind {
            SessionKind::Term { active, .. } => pane_auto_title(active, cx),
            SessionKind::Acp(view) => {
                let dir = view
                    .read(cx)
                    .cwd()
                    .map(|c| c.rsplit('/').next().unwrap_or(&c).to_string());
                let agent = view.read(cx).agent_kind().short_label();
                match dir {
                    Some(d) if !d.is_empty() => format!("{agent} 对话 · {d}"),
                    _ => format!("{agent} 对话"),
                }
            }
        })
    }

    /// 会话工作目录：活动终端的 cwd（侧栏分组用）。
    fn cwd(&self, cx: &App) -> Option<String> {
        match &self.kind {
            SessionKind::Term { active, .. } => active.read(cx).cwd(),
            SessionKind::Acp(view) => view.read(cx).cwd(),
        }
    }

    /// 会话内 pane 数（判断 Cmd+W 是关 pane 还是关整会话）。
    fn pane_count(&self) -> usize {
        match &self.kind {
            SessionKind::Term { .. } => self.term_leaves().len(),
            SessionKind::Acp(_) => 1,
        }
    }

    /// 会话内任一 pane 的待处理通知消息（供总览卡片显示「等你确认 xxx」）。
    fn notification_msg(&self, cx: &App) -> Option<String> {
        let v = self.term_leaves();
        // 优先：网格解析出的权限摘要 → OSC 审批文案 → 任意通知
        if let Some(p) = self.permission_prompt(cx) {
            if let Some(s) = p.summary {
                return Some(s);
            }
        }
        if let Some(t) = v.iter().find(|t| t.read(cx).is_awaiting_approval()) {
            if let Some(s) = t.read(cx).notification() {
                return Some(s.to_string());
            }
        }
        v.iter().find_map(|t| t.read(cx).notification().map(|s| s.to_string()))
    }

    /// 会话内扫到的权限菜单（优先含审批/菜单的 pane）。
    fn permission_prompt(&self, cx: &App) -> Option<permission_menu::PermissionPrompt> {
        let v = self.term_leaves();
        if let Some(t) = v.iter().find(|t| t.read(cx).is_awaiting_approval()) {
            if let Some(p) = t.read(cx).permission_prompt() {
                return Some(p);
            }
        }
        v.iter().find_map(|t| t.read(cx).permission_prompt())
    }

    /// 需要用户处理的 pane：优先等审批 / 权限菜单，其次任意「需要注意」。
    fn attention_pane(&self, cx: &App) -> Option<Entity<TerminalView>> {
        let v = self.term_leaves();
        if let Some(t) = v.iter().find(|t| t.read(cx).is_awaiting_approval()) {
            return Some(t.clone());
        }
        v.iter()
            .find(|t| t.read(cx).attention_kind().is_some())
            .cloned()
    }

    /// 活动 pane 末尾 n 行文本（总览卡片迷你预览）。
    fn preview(&self, cx: &App, n: usize) -> Vec<String> {
        match &self.kind {
            SessionKind::Term { active, .. } => active.read(cx).last_lines(n),
            SessionKind::Acp(view) => view.read(cx).last_lines(n),
        }
    }

    /// 会话内最近一次通知时刻（总览「N 分钟前」）。
    fn notified_at(&self, cx: &App) -> Option<Instant> {
        self.term_leaves()
            .iter()
            .filter_map(|t| t.read(cx).notified_at())
            .max()
    }

    /// 会话状态：等审批 > 需要处理 > 运行中 > 刚完成未读 > 空闲（遍历全部 pane 取最高）。
    fn status(&self, cx: &App) -> AgentStatus {
        let active = match &self.kind {
            SessionKind::Term { active, .. } => active,
            // ACP 会话：相位是协议事实，直接问视图，不经推断链。
            SessionKind::Acp(view) => {
                let v = view.read(cx);
                if v.is_awaiting_approval() {
                    return AgentStatus::WaitingApproval;
                }
                if v.is_awaiting_choice() {
                    return AgentStatus::NeedsAttention;
                }
                if v.is_running() {
                    return AgentStatus::Running;
                }
                if v.completed_unread() {
                    return AgentStatus::Done;
                }
                return AgentStatus::Idle;
            }
        };
        let v = self.term_leaves();
        // 等审批（红）压过一般注意（橙）：
        // 1) daemon 状态通道事实  2) OSC 文案  3) 网格权限菜单
        let mut attention = None;
        for t in &v {
            if let Some(state) = daemon_state_for(t, cx) {
                if state.phase == terminal::DaemonPhase::AwaitingApproval {
                    return AgentStatus::WaitingApproval;
                }
            }
            let tv = t.read(cx);
            if tv.is_awaiting_approval() {
                return AgentStatus::WaitingApproval;
            }
            if matches!(
                tv.attention_kind(),
                Some(terminal_view::AttentionKind::Attention)
            ) {
                attention = Some(AgentStatus::NeedsAttention);
            }
        }
        if let Some(s) = attention {
            return s;
        }
        // 活动终端：daemon 说在跑（Thinking/ExecutingTool）比标题 spinner 猜测更可信；
        // 没有 daemon 数据（老版本守护/还没收到第一条上报）才退化到猜。
        if let Some(state) = daemon_state_for(active, cx) {
            if matches!(
                state.phase,
                terminal::DaemonPhase::Thinking | terminal::DaemonPhase::ExecutingTool
            ) {
                return AgentStatus::Running;
            }
        } else if let Some(raw) = active.read(cx).agent_title() {
            if crate::osc::title_starts_with_spinner(raw.trim_start()) {
                return AgentStatus::Running;
            }
        }
        // 有 pane 刚跑完还没被回应 → 提示「有结果可看」。
        if v.iter().any(|t| t.read(cx).completed_unread()) {
            return AgentStatus::Done;
        }
        AgentStatus::Idle
    }
}

/// 设置窗口 pages 列表里的页下标——调整 `render_settings_content` 末尾那个
/// `pages(vec![...])` 的顺序时必须同步改这里，否则应用菜单「检查更新…」会跳错页。
const SETTINGS_PAGE_APPEARANCE: usize = 0;
// appearance / 桌面宠物 / 启动 / Agent 集成 / 更新 / 远程
const SETTINGS_PAGE_UPDATE: usize = 4;

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
        let is_agent = head.starts_with('✳') || crate::osc::title_starts_with_spinner(head);
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
    let daemon_state = daemon_state_for(view, cx);
    if let Some(state) = &daemon_state {
        if state.phase == terminal::DaemonPhase::AwaitingApproval {
            return AgentStatus::WaitingApproval;
        }
    }
    let t = view.read(cx);
    match t.attention_kind() {
        Some(terminal_view::AttentionKind::Approval) => return AgentStatus::WaitingApproval,
        Some(terminal_view::AttentionKind::Attention) => return AgentStatus::NeedsAttention,
        None => {}
    }
    if let Some(state) = &daemon_state {
        if matches!(
            state.phase,
            terminal::DaemonPhase::Thinking | terminal::DaemonPhase::ExecutingTool
        ) {
            return AgentStatus::Running;
        }
    } else if let Some(raw) = t.agent_title() {
        if crate::osc::title_starts_with_spinner(raw.trim_start()) {
            return AgentStatus::Running;
        }
    }
    if t.completed_unread() {
        return AgentStatus::Done;
    }
    AgentStatus::Idle
}

/// 总览卡片事实块是否值得展示（过滤终端预览/说明文案误入）。
fn overview_fact_is_usable(m: &str) -> bool {
    let t = m.trim();
    if t.is_empty() || t.len() < 2 {
        return false;
    }
    // 终端状态行 / 快捷键提示
    if t.contains("Shift+") || t.contains("Ctrl+") || t.contains("manual mode") {
        return false;
    }
    // 开发说明、UI 文案泄漏
    const BAD: &[&str] = &[
        "空筛选",
        "打开终端",
        "卡片信息",
        "完全退出",
        "重开 Smelt",
        "进 总览",
        "hook 事实",
        "待我处理",
        "权限菜单",
    ];
    if BAD.iter().any(|b| t.contains(b)) {
        return false;
    }
    // 纯状态栏碎片
    if t.starts_with("current ") || t.starts_with("weekly ") {
        return false;
    }
    true
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
    /// 文件树里额外 pin 进来的项目根（除当前活动项目外）；空 = 只看当前项目。
    #[serde(default)]
    pinned_file_tree_roots: Vec<String>,
    /// 文件树里被折叠起来的项目根（多根时）。
    #[serde(default)]
    collapsed_file_tree_roots: Vec<String>,
    /// Git 页左栏（变更文件列表）拖出的宽度（px）；None = 用默认值。
    #[serde(default)]
    git_left_w: Option<f32>,
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
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct SessionState {
    layout: PaneState,
    active: usize,
    #[serde(default)]
    custom_title: Option<String>,
    /// Some = ACP 消息流会话（layout 只是占位叶子，旧版 smelt 读到会降级开普通
    /// 终端，不炸档）。恢复时建占位视图（会话进程不持久化，见方案「已知不做」）。
    #[serde(default)]
    acp: Option<AcpSaved>,
}

/// ACP 会话的存档元数据：cwd/cmd 给「重新开始」按钮原样重启用；entries 是完整
/// 消息历史——GUI 重开后占位视图直接显示它，不是「已结束」四个字干瞪眼；
/// resume_session_id 是 agent 侧的会话 id，「重新开始」靠它尝试 session/load
/// 真续接（agent 记得之前聊了什么），而不只是摆样子的新对话。
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct AcpSaved {
    cwd: Option<String>,
    cmd: String,
    /// agent 种类标识（`AcpAgentKind::id()`）。旧存档没有这个字段 → None，恢复时
    /// 按 cmd 里的包名反推，反推不出就当 Claude（多 agent 之前只可能是它）。
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    entries: Vec<acp_view::AcpEntry>,
    #[serde(default)]
    resume_session_id: Option<agent_client_protocol::schema::v1::SessionId>,
}

/// 可序列化的分屏布局镜像：叶子存该终端 cwd + 守护会话 id，Split 存方向 + 子节点。
/// 拖动比例暂不持久化，重开按均分；结构 / 嵌套 / 方向完整恢复。
/// id 用于重开 GUI 时 reattach smeltd 里还活着的会话（旧存档无 id → 开新会话）。
#[derive(Clone, serde::Serialize, serde::Deserialize)]
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
        /// 快捷启动实际命令行（硬重启守护 / 冷启动新建时用来重跑 agent）。
        /// 旧存档没有 → None，只开裸 shell。
        #[serde(default)]
        launch_cmd: Option<String>,
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
                launch_cmd: t.launch_cmd().map(str::to_string),
            }
        }
        Pane::Split { axis, children, .. } => PaneState::Split {
            axis: (*axis).into(),
            children: children.iter().map(|c| pane_to_state(c, cx)).collect(),
        },
    }
}

/// 后台线程里已经 spawn/reattach 好的叶子终端（尚未挂 GPUI Entity）。
struct SpawnedLeaf {
    terminal: terminal::Terminal,
    sid: String,
    cwd: Option<String>,
    launch: Option<String>,
    label: Option<String>,
    custom_title: Option<String>,
}

/// 阻塞：按 DFS 顺序 spawn 一棵布局树的全部叶子（**只**在后台线程调用）。
fn spawn_layout_leaves(ps: &PaneState) -> Result<Vec<SpawnedLeaf>, String> {
    let mut out = Vec::new();
    spawn_layout_leaves_rec(ps, &mut out)?;
    Ok(out)
}

fn spawn_layout_leaves_rec(ps: &PaneState, out: &mut Vec<SpawnedLeaf>) -> Result<(), String> {
    match ps {
        PaneState::Leaf {
            cwd,
            id,
            custom_title,
            launch_label,
            launch_cmd,
        } => {
            let sid = id.clone().unwrap_or_else(new_sid);
            let terminal = terminal::Terminal::spawn(
                24,
                80,
                cwd.as_deref(),
                &sid,
                launch_cmd.as_deref(),
            )
            .map_err(|e| {
                eprintln!("[workspace] 恢复会话 {sid}（{cwd:?}）失败：{e:#}");
                e.to_string()
            })?;
            out.push(SpawnedLeaf {
                terminal,
                sid,
                cwd: cwd.clone(),
                launch: launch_cmd.clone(),
                label: launch_label.clone(),
                custom_title: custom_title.clone(),
            });
            Ok(())
        }
        PaneState::Split { children, .. } => {
            for c in children {
                spawn_layout_leaves_rec(c, out)?;
            }
            Ok(())
        }
    }
}

/// 用已 spawn 的叶子（DFS 序）重建布局树；**只**在 UI 线程建 Entity。
fn rebuild_pane_ready(
    ps: &PaneState,
    leaves: &mut std::vec::IntoIter<SpawnedLeaf>,
    tabs: &mut Vec<Entity<TerminalView>>,
    cx: &mut Context<Workspace>,
) -> Option<Pane> {
    match ps {
        PaneState::Leaf { .. } => {
            let leaf = leaves.next()?;
            let v = cx.new(|cx| {
                let mut view = TerminalView::from_terminal(
                    cx,
                    leaf.terminal,
                    leaf.cwd,
                    leaf.sid,
                    leaf.launch.as_deref(),
                    leaf.label.as_deref(),
                );
                view.set_custom_title(leaf.custom_title);
                view
            });
            tabs.push(v.clone());
            Some(Pane::Leaf(v))
        }
        PaneState::Split { axis, children } => {
            let mut kept: Vec<Pane> = children
                .iter()
                .filter_map(|c| rebuild_pane_ready(c, leaves, tabs, cx))
                .collect();
            match kept.len() {
                0 => None,
                1 => Some(kept.remove(0)),
                _ => Some(Pane::Split {
                    axis: (*axis).into(),
                    state: cx.new(|_| ResizableState::default()),
                    children: kept,
                }),
            }
        }
    }
}

/// 把存档里的会话列表规范成 `Vec<SessionState>`（兼容旧 layout / tabs 字段）。
fn normalize_saved_sessions(s: &WsState) -> (Vec<SessionState>, usize) {
    if !s.sessions.is_empty() {
        return (s.sessions.clone(), s.active_session);
    }
    if let Some(ps) = &s.layout {
        return (
            vec![SessionState {
                layout: ps.clone(),
                active: s.active,
                custom_title: None,
                acp: None,
            }],
            0,
        );
    }
    let sessions: Vec<SessionState> = s
        .tabs
        .iter()
        .map(|cwd| SessionState {
            layout: PaneState::Leaf {
                cwd: cwd.clone(),
                id: None,
                custom_title: None,
                launch_label: None,
                launch_cmd: None,
            },
            active: 0,
            custom_title: None,
            acp: None,
        })
        .collect();
    (sessions, s.active)
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
    /// 舞台覆盖页：Some = 旧全屏页（总览/任务/文件树/Git/热力图/历史）盖住会话
    /// 舞台；None = 正常显示当前会话。Esc / 各入口收回时清 None。
    stage_override: Option<MainView>,
    /// inspector 当前 tab（右侧图标条切换；见 inspector.rs）。
    inspector_tab: inspector::InspectorTab,
    /// inspector 面板是否展开（Cmd+B / 图标条再点收合）。
    inspector_open: bool,
    /// 阻塞 toast 的 ✕ / 稍后 记录（键 = 会话 anchor_id）；会话状态解除时自动
    /// 清除，同一会话再次阻塞会重新弹。见 toast.rs。
    toast_dismissed: HashSet<EntityId>,
    toast_snoozed: HashMap<EntityId, Instant>,
    /// 总览页筛选（全部 / 待我处理 / 运行中）。
    overview_filter: OverviewFilter,
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
    /// F7/Shift+F7 当前跳到第几个改动块（None = 还没跳过）。换文件重开 diff 时清空。
    active_hunk: Option<usize>,
    /// Git 页变更文件树里被折叠的目录（存相对仓库根的路径）。默认全展开——改动
    /// 文件通常没几个，一进来就全看见比让人挨个点开更顺手。
    git_tree_collapsed: HashSet<String>,
    /// diff 看哪一层改动（全部 / 已暂存 / 未暂存）。默认全部，保持既有观感。
    diff_scope: git_panel::DiffScope,
    /// 「日志」页（git 提交历史 + 分支图）的全部状态。
    git_log: git_log::GitLogState,
    /// Git 页当前在看哪个子页（改动 / 日志）。
    git_tab: GitTab,
    /// 正在推送（按钮显示「推送中…」并禁用，避免连点推两次）。
    pushing: bool,
    /// 正在确认删除的分支：(仓库根, 分支名, 是否远端分支)。
    delete_branch_target: Option<(String, String, bool)>,
    /// 日志页三栏（分支树 / 提交列表 / 详情）的拖拽状态。窗口窄时靠它腾地方。
    git_log_resize: Entity<ResizableState>,
    /// 交互式 diff：选中待评论的行号集合（对应 GitDiff.lines 下标），换文件/重开 diff 时清空。
    diff_selected: HashSet<usize>,
    /// 交互式 diff 的评论输入框（懒创建，随 Git 视图渲染出待发送的 diff 时创建）。
    diff_comment_input: Option<Entity<gpui_component::input::InputState>>,
    /// Git 视图的 commit message 输入框（懒创建，随 Git 视图首次渲染时创建；跟
    /// diff_comment_input 是两个独立的框，一个针对选中的 diff 行，一个是整体提交信息）。
    commit_msg_input: Option<Entity<gpui_component::input::InputState>>,
    /// 「生成」按钮请求 LLM 生成 commit message 进行中（防连点、按钮显示"生成中…"）。
    commit_msg_generating: bool,
    /// 活动项目（project_groups 的分组名）：会话列表里高亮哪一组、顶栏显示谁、
    /// 「+Agent/+Term」新建到哪个 cwd。None 或该组已消失时回退到活动会话所在组
    /// （见 active_project_name）。
    active_project: Option<String>,
    /// 会话列表里被折叠起来的项目分组名。
    collapsed_projects: HashSet<String>,
    /// 文件树里被用户折叠起来的项目根目录（多根工作区才有；默认全展开，只有在这个
    /// 集合里的才收起，见 file_tree / toggle_root_collapsed）。持久化。
    collapsed_roots: HashSet<String>,
    /// 文件树里额外 pin 进来的项目根（除当前活动项目外）。默认空 → 文件树就是当前
    /// 项目单根；用户从「+ 项目」把别的项目挂进来才变多根。持久化，重启还在。
    pinned_roots: Vec<String>,
    /// SKILLS 面板缓存：(取得时刻, 列表) + 扫的是哪个项目 + 是否正在后台扫。
    skills_cache: skills::SkillsCache,
    skills_cache_cwd: Option<String>,
    skills_inflight: bool,
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
    /// Git 页左栏 resize 状态；宽度落盘到 git_left_w，同文件树一套。
    git_left_resize: Entity<ResizableState>,
    git_left_w: f32,
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
    /// 弹窗处于「编辑」模式时的任务 id；None = 新建模式。
    task_editing: Option<String>,
    /// 定时任务扫描循环是否已启动（避免 render 重复 spawn）。
    task_schedule_started: bool,
    /// 文件树搜索结果（文件名 + 文件内容）：后台遍历项目填充，render 只读。
    /// query 非空时左栏由树形切换为扁平命中列表。
    search_results: Option<SearchState>,
    /// 搜索任务自增序号：后台遍历完成时用它丢弃过期结果（期间又改了查询）。
    search_gen: u64,
    /// 文件树列初始宽度（px）：启动时从存档恢复，作为 resizable_panel 的初始 size。
    file_tree_w: f32,
    /// 文件树列 resize 事件订阅（拖动完写回存档）；随视图存活。
    _file_tree_resize_sub: Subscription,
    _git_left_resize_sub: Subscription,
    /// git 信息缓存（cwd → (分支, 改动数)），总览页后台刷新、渲染读缓存。
    git_cache: HashMap<String, (String, usize)>,
    /// 宠物大脑（LLM）配置的输入框；首次打开设置面板时懒创建（需要 window）。
    llm_inputs: Option<LlmInputs>,
    /// 上面几个输入框的变更订阅（保活；随视图存活）。
    llm_subs: Vec<Subscription>,
    /// 远程：信令服务地址输入框（用户自部署 smelt-signal，无内置默认）。
    signal_http_input: Option<Entity<gpui_component::input::InputState>>,
    /// 启动项列表编辑器（设置页「启动」分组懒创建）。
    launch_inputs: Option<settings::LaunchInputs>,
    /// 手动添加 workspace 列表编辑器（设置页「Agent 集成」分组懒创建）。
    profile_inputs: Option<settings::ProfileInputs>,
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
    /// 历史会话列表缓存（`"{agent_id}:{cwd}"` → (取得时刻, 数据)）：后台扫描该 agent
    /// 在该项目下的本地存储，render 只读。key 带上 agent_id 是因为四家 agent 的历史
    /// 各存各的，同一个 cwd 换个 tab 就是完全不同的一份数据。
    /// 注意：总览卡片那边（`self.sessions` 渲染，展示"最近一次 Claude 活动"）也复用
    /// 这份缓存，固定传 `AcpAgentKind::Claude`——历史会话页加多 agent tab 不该改变
    /// 那个功能的行为，两处刻意共享同一套读写路径而不是各建一份。
    session_list: HashMap<String, (Instant, Rc<Vec<session_history::SessionSummary>>)>,
    /// 正在后台扫描历史会话列表的 key（同上 `"{agent_id}:{cwd}"`，防重复并发 spawn）。
    session_list_inflight: HashSet<String>,
    /// 当前选中查看的历史会话（路径 + 解析出的对话内容）；None 表示未选。
    session_detail: Option<(PathBuf, Rc<session_history::SessionDetail>)>,
    /// 加载会话详情的自增序号：后台解析完成时用它判断结果是否已过期（切了别的会话）。
    session_detail_gen: u64,
    /// 「继续」正在后台加载的目标（`"{agent_id}:{cwd}:{resume_id}"`），挡连点
    /// 同一条历史记录重复发起、开出好几个重复标签页。
    resume_inflight: HashSet<String>,
    /// 「继续」操作的自增序号：只有发起时最新的那次操作，加载完成后才会真的抢
    /// 激活态——连点两条不同历史记录时，防止加载慢的那条后完成反而把已经激活
    /// 的那条顶掉（会话本身照样建，只是不抢焦点）。
    resume_gen: u64,
    /// 历史会话页当前显示的是「会话」还是「记忆」（同一套左列表 + 右详情布局）。
    history_pane: HistoryPane,
    /// 历史会话页「会话」子页当前选中查看哪家 agent 的历史（Claude/Copilot/Codex/
    /// Grok 分 tab，各自存储格式不同，见 session_history.rs 头部注释）。
    history_agent: settings::AcpAgentKind,
    /// 选中的是手动添加的 workspace profile（而不是某个基础 agent 槽位）时是
    /// `Some(profile_id)`；`history_agent` 这时候是该 profile 底层接的种类。
    history_profile: Option<String>,
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
    /// （项目拖拽待接到 rail，暂时闲置；见 ProjectDrag 注释。）
    #[allow(dead_code)]
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
    /// 正在确认删除的 worktree（None = 没在删）。
    delete_worktree_target: Option<DeleteWorktreeTarget>,
    /// 正在确认丢弃的 diff 块：(仓库根, hunk 下标)。丢弃直接改工作区文件且不进
    /// reflog，找不回来，所以必须过一道确认。
    discard_hunk_target: Option<(String, usize)>,
    /// 正在确认丢弃整个文件的改动：(仓库根, 相对路径, 是否未跟踪)。未跟踪文件是
    /// 直接删盘，比 restore 更狠，文案要分开写。
    discard_file_target: Option<(String, String, bool)>,
    /// 「丢弃全部改动」确认弹窗的目标仓库根（Some = 弹窗开着）。见 git_panel.rs。
    discard_all_target: Option<String>,
    /// git 远端同步 / stash 操作进行中：Some(操作名) = 正在跑，None = 空闲。
    /// 既做并发闸门（防连点抢 index.lock），也给 SOURCE CONTROL 头显示「拉取中…」
    /// 这类进行中反馈——否则点了按钮几秒内毫无动静。见 git_panel.rs run_git_op。
    git_op: Option<&'static str>,
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
    /// 守护自报的运行信息（PID / 启动时刻 / 会话数），设置页「更新」里展示。
    /// 跟 daemon_outdated 同一趟后台探测回填；守护没起 → None。
    daemon_info: Option<terminal::DaemonInfo>,
    /// 「重启守护进程」二次确认弹窗开关：点确定会断开所有当前终端会话。
    show_daemon_restart_confirm: bool,
    /// 「会话管理」弹窗开关：设置页「更新」tab 点开会话数详情用。守护进程持有
    /// 的会话不只 GUI 侧栏认领的那些——测试跑出来的孤儿、忘了关的临时会话都会
    /// 计进「N 个会话」里但从没在任何侧栏露过面，只有这里能看见并单独清理，
    /// 不用被迫走「重启守护进程」这种会误伤正常会话的核选项。
    session_manager_open: bool,
    /// 弹窗数据：最近一次 list 查询结果，None = 正在查/还没查过。
    session_manager_list: Option<Vec<terminal::DaemonSessionState>>,
    /// 启动时从存档恢复失败的会话（守护未就绪等）。仍写回 workspace.json，避免
    /// 「恢复失败 → 写空盘 → 会话永久蒸发」。侧栏本帧看不到它们，下次冷启动会重试。
    restore_orphans: Vec<SessionState>,
    /// 根节点自己的焦点句柄：总览/文件树/Git/热力图/历史会话这些页面自身没有可
    /// 聚焦的元素，切过去后如果谁都不 focus，窗口的 focus 仍停在切走前那个（可能
    /// 已经不在当前渲染树里的）终端上——GPUI 找不到就把 focus 兜底纠正到 window 的
    /// 真正根节点，而 Workspace 这层的 on_key_down（Cmd+Shift+F 等全局快捷键）挂在
    /// Root 组件之下、并非那个根节点，于是收不到事件，表现为"切到别的页面后快捷键
    /// 全部失灵"。切到非终端页面时把 focus 显式认领到这个句柄上，保证 Workspace 的
    /// on_key_down 始终在 dispatch 路径上。
    focus_handle: FocusHandle,
}

/// 历史会话页「继续」把已读出的 `Turn` 列表转成 `AcpEntry`，好塞进新建的占位
/// 会话当本地快照（见 `Workspace::resume_acp_session`）。`Turn` 是给只读浏览
/// 用的压扁视图（工具调用只留名字，没有 id/参数/输出），转回来必然有损——
/// `id` 现造一个本会话内唯一的、`kind` 统一归 `Other`、`status` 记
/// `Completed`（历史记录里的调用理应都跑完了）、`output` 留空。这不是"完整
/// 还原"，只求「切换到这条历史会话时本地能看到个大概，而不是一片空白」；
/// 真续接成功后（`ReadyKind::ResumedWithReplay`）这份快照会被 agent 重放的
/// 内容整个替换掉，这里的近似值不会长期存在。
fn turns_to_acp_entries(turns: &[session_history::Turn]) -> Vec<acp_view::AcpEntry> {
    let mut out = Vec::new();
    for (i, t) in turns.iter().enumerate() {
        if t.is_user {
            if !t.text.trim().is_empty() {
                out.push(acp_view::AcpEntry::User(t.text.clone()));
            }
            continue;
        }
        if !t.text.trim().is_empty() {
            out.push(acp_view::AcpEntry::Assistant { text: t.text.clone(), thought: false });
        }
        for (j, tool) in t.tools.iter().enumerate() {
            out.push(acp_view::AcpEntry::ToolCall {
                id: format!("history-{i}-{j}"),
                title: tool.clone(),
                kind: acp_view::ToolKind::Other,
                status: acp_view::ToolCallStatus::Completed,
                output: Vec::new(),
            });
        }
    }
    out
}

impl Workspace {
    fn new(cx: &mut Context<Self>) -> Self {
        // 存档只读元数据；**不**在 UI 线程同步 Terminal::spawn（会 beachball 数秒）。
        // 会话 reattach 丢后台线程，窗口先起来用户即可点侧栏/设置。
        let saved = load_ws_state();
        let file_tree_w = saved.as_ref().and_then(|s| s.file_tree_w).unwrap_or(260.);
        let git_left_w = saved.as_ref().and_then(|s| s.git_left_w).unwrap_or(300.);

        let (pending_sessions, active_session) = saved
            .as_ref()
            .map(normalize_saved_sessions)
            .unwrap_or_default();
        // 恢复完成前先放进 orphans：save_state 会合并 orphans，避免空 sessions 窗口期抹盘。
        let restore_orphans = pending_sessions.clone();
        let sessions: Vec<Session> = Vec::new();

        // 文件树列 resize：拖动完 emit Resized，写回存档持久化宽度。
        let file_tree_resize = cx.new(|_| ResizableState::default());
        let _file_tree_resize_sub =
            cx.subscribe(&file_tree_resize, |this, _state, _e: &ResizablePanelEvent, cx| {
                this.save_state(cx);
            });
        // 日志页三栏 resize（不落盘：日志是临时查看，没必要持久化）。
        let git_log_resize = cx.new(|_| ResizableState::default());
        // Git 页左栏 resize：同上一套，拖完落盘。
        let git_left_resize = cx.new(|_| ResizableState::default());
        let _git_left_resize_sub =
            cx.subscribe(&git_left_resize, |this, _state, _e: &ResizablePanelEvent, cx| {
                this.save_state(cx);
            });

        let mut ws = Self {
            sessions,
            active_session,
            stage_override: None,
            inspector_tab: inspector::InspectorTab::Files,
            inspector_open: true,
            toast_dismissed: HashSet::new(),
            toast_snoozed: HashMap::new(),
            overview_filter: OverviewFilter::All,
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
            active_hunk: None,
            git_tree_collapsed: HashSet::new(),
            diff_scope: git_panel::DiffScope::All,
            git_log: git_log::GitLogState::default(),
            git_tab: GitTab::Changes,
            pushing: false,
            delete_branch_target: None,
            git_log_resize,
            diff_selected: HashSet::new(),
            diff_comment_input: None,
            commit_msg_input: None,
            commit_msg_generating: false,
            active_project: None,
            collapsed_projects: HashSet::new(),
            collapsed_roots: saved
                .as_ref()
                .map(|s| s.collapsed_file_tree_roots.iter().cloned().collect())
                .unwrap_or_default(),
            pinned_roots: saved
                .as_ref()
                .map(|s| s.pinned_file_tree_roots.clone())
                .unwrap_or_default(),
            skills_cache: None,
            skills_cache_cwd: None,
            skills_inflight: false,
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
            task_editing: None,
            task_schedule_started: false,
            search_results: None,
            search_gen: 0,
            file_tree_w,
            _file_tree_resize_sub,
            git_left_resize,
            git_left_w,
            _git_left_resize_sub,
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
            resume_inflight: HashSet::new(),
            resume_gen: 0,
            history_pane: HistoryPane::Sessions,
            history_agent: settings::AcpAgentKind::Claude,
            history_profile: None,
            memory_list: HashMap::new(),
            memory_list_inflight: HashSet::new(),
            memory_selected: None,
            llm_inputs: None,
            llm_subs: Vec::new(),
            signal_http_input: None,
            launch_inputs: None,
            profile_inputs: None,
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
            delete_worktree_target: None,
            discard_hunk_target: None,
            discard_file_target: None,
            discard_all_target: None,
            git_op: None,
            background_error: None,
            daemon_outdated: None,
            daemon_upgrade_msg: None,
            daemon_upgrading: false,
            daemon_info: None,
            show_daemon_restart_confirm: false,
            session_manager_open: false,
            session_manager_list: None,
            restore_orphans,
            focus_handle: cx.focus_handle(),
        };
        // orphans 已挂上全部待恢复会话 → 写盘不会抹掉存档。
        ws.save_state(cx);
        updater::cleanup_stale_backup();
        ws.check_for_update(true, cx);
        // 有待恢复会话：ensure+reattach 在 restore 线程串行做完后再 check_daemon_outdated，
        // 避免与 ensure handoff 三线并行踩踏。无会话则直接查守护状态。
        if !pending_sessions.is_empty() {
            eprintln!(
                "[workspace] 后台恢复 {} 个会话（不堵 UI）…",
                pending_sessions.len()
            );
            ws.schedule_session_restore(pending_sessions, active_session, cx);
        } else {
            ws.check_daemon_outdated(cx);
        }
        ws
    }

    /// 冷启动：专用 OS 线程里 **先 ensure managed 守护，再 reattach 全部会话**。
    /// 完成后才 `check_daemon_outdated`（不与 restore 并行 upgrade）。
    fn schedule_session_restore(
        &mut self,
        pending: Vec<SessionState>,
        active_session: usize,
        cx: &mut Context<Self>,
    ) {
        // ACP 会话不走守护 reattach：进程没有持久化，UI 线程直接建「已结束」占位
        // （一键同 cmd/cwd 重开）。摘出后剩下的照旧走后台恢复。
        let (acp_saved, pending): (Vec<SessionState>, Vec<SessionState>) =
            pending.into_iter().partition(|ss| ss.acp.is_some());
        for ss in acp_saved {
            let Some(saved) = ss.acp else { continue };
            // 有旧 session id 的会在切到它时自动续接（见 maybe_auto_resume），
            // 文案别再让人去点按钮；没有 id 的只能手动开新会话。
            let reason = if saved.resume_session_id.is_some() {
                "上次的对话已随 GUI 退出（历史消息已保留，切到本会话会自动续接）"
            } else {
                "上次的对话已随 GUI 退出结束（历史消息已保留，点击重新开始继续）"
            };
            let agent = saved
                .agent
                .as_deref()
                .and_then(settings::AcpAgentKind::from_id)
                .unwrap_or_else(|| acp_agent_from_cmd(&saved.cmd));
            let view = cx.new(|cx| {
                acp_view::AcpView::placeholder(
                    cx,
                    agent,
                    saved.cmd,
                    saved.cwd,
                    reason.to_string(),
                    saved.entries,
                    saved.resume_session_id,
                )
            });
            let _acp_persist_sub = Some(self.subscribe_acp_persist(&view, cx));
            self.sessions.push(Session {
                kind: SessionKind::Acp(view),
                custom_title: ss.custom_title,
                _acp_persist_sub,
            });
        }
        if pending.is_empty() {
            self.check_daemon_outdated(cx);
            cx.notify();
            return;
        }
        // 逐个交货，别攒成一整包：会话之间互不依赖，攒一包等于让窗口空等最慢的那次
        // attach——表现为「冷启动后一个会话都不显示，过一会才全部冒出来」。改成恢复好
        // 一个就发一个，第一个会话立刻上屏，其余陆续补齐。unbounded 保证后台线程不会
        // 因为 UI 还没来得及收而卡住。
        let (tx, rx) = smol::channel::unbounded();
        std::thread::Builder::new()
            .name("smelt-restore-sessions".into())
            .spawn(move || {
                // 1) 完整 ensure（可能 handoff）→ 2) 再 reattach。禁止与 UI 侧并行 upgrade。
                let _ = terminal::ensure_managed_daemon_current();
                terminal::ensure_daemon_running();
                let mut daemon_ok = true;
                for ss in pending {
                    let outcome = if daemon_ok {
                        match spawn_layout_leaves(&ss.layout) {
                            Ok(leaves) => Ok(leaves),
                            Err(e) => {
                                if e.contains("smeltd 未就绪") {
                                    daemon_ok = false;
                                }
                                Err(e)
                            }
                        }
                    } else {
                        Err("smeltd 未就绪（先前会话已失败）".to_string())
                    };
                    // 接收端没了（窗口已关）就别再白跑剩下的
                    if tx.send_blocking((ss, outcome)).is_err() {
                        return;
                    }
                }
            })
            .expect("spawn smelt-restore-sessions 线程");

        cx.spawn(async move |this, cx| {
            let mut failed: Vec<SessionState> = Vec::new();
            let mut restored = 0usize;

            // 收一个渲染一个。后台线程跑完会 drop sender，recv 报错即代表全部处理完。
            while let Ok((ss, result)) = rx.recv().await {
                let outcome = this.update(cx, |this, cx| {
                    let leaves = match result {
                        Ok(leaves) => leaves,
                        Err(e) => {
                            eprintln!("[workspace] 会话恢复失败，保留 orphan：{e}");
                            return Some(ss);
                        }
                    };
                    let mut leaf_iter = leaves.into_iter();
                    let mut tabs = Vec::new();
                    let Some(layout) = rebuild_pane_ready(&ss.layout, &mut leaf_iter, &mut tabs, cx)
                    else {
                        return Some(ss);
                    };
                    let Some(active) = tabs.get(ss.active).or_else(|| tabs.first()).cloned() else {
                        return Some(ss);
                    };
                    this.sessions.push(Session {
                        kind: SessionKind::Term { layout, active },
                        custom_title: ss.custom_title,
                        _acp_persist_sub: None,
                    });
                    // 让这一个立刻上屏，不等其余的
                    cx.notify();
                    None
                });
                match outcome {
                    Ok(Some(ss)) => failed.push(ss),
                    Ok(None) => restored += 1,
                    Err(_) => return, // 窗口已关，收摊
                }
            }

            let _ = this.update(cx, |this, cx| {
                this.restore_orphans = failed;
                this.active_session = active_session.min(this.sessions.len().saturating_sub(1));
                this.save_state(cx);
                if !this.restore_orphans.is_empty() {
                    eprintln!(
                        "[workspace] {} 个会话未能恢复，已保留在存档中，下次启动会重试",
                        this.restore_orphans.len()
                    );
                }
                eprintln!(
                    "[workspace] 后台恢复完成：成功 {restored}，失败 {}",
                    this.restore_orphans.len()
                );
                // restore 完成后再查/升级守护，避免与 reattach 并行 handoff
                this.check_daemon_outdated(cx);
                cx.notify();
            });
        })
        .detach();
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
                .position(|(_, _, ixs)| ixs.iter().any(|&ix| self.sessions[ix].anchor_id() == id))
        };
        let (Some(dragged_group), Some(target_group)) = (group_of(dragged), group_of(target)) else {
            return;
        };
        if dragged_group != target_group {
            return;
        }
        let Some(from_ix) = self.sessions.iter().position(|s| s.anchor_id() == dragged) else {
            return;
        };
        let Some(target_ix) = self.sessions.iter().position(|s| s.anchor_id() == target) else {
            return;
        };

        let active_id = self.cur().map(|s| s.anchor_id());
        let session = self.sessions.remove(from_ix);
        let adjusted_target_ix = if from_ix < target_ix { target_ix - 1 } else { target_ix };
        let insert_at = adjusted_target_ix + if before { 0 } else { 1 };
        self.sessions.insert(insert_at, session);

        if let Some(id) = active_id {
            if let Some(ix) = self.sessions.iter().position(|s| s.anchor_id() == id) {
                self.active_session = ix;
            }
        }
        self.save_state(cx);
        cx.notify();
    }

    /// 拖拽排序：把 from 项目的所有会话（保持相对顺序）整体挪到 to 项目最前面。
    /// （项目拖拽待接到 rail，暂时闲置；见 ProjectDrag 注释。）
    #[allow(dead_code)]
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

        let active_id = self.cur().map(|s| s.anchor_id());
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
            if let Some(ix) = self.sessions.iter().position(|s| s.anchor_id() == id) {
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

    /// ACP 会话内容变化（AcpViewEvent::Changed）→ 立即 save_state。与侧栏/文件树
    /// resize 订阅同一惯用法（main.rs::new 里的 _resize_sub）。
    fn subscribe_acp_persist(
        &mut self,
        view: &Entity<acp_view::AcpView>,
        cx: &mut Context<Self>,
    ) -> gpui::Subscription {
        cx.subscribe(view, |this: &mut Self, _view, _ev: &acp_view::AcpViewEvent, cx| {
            this.save_state(cx);
        })
    }

    /// 「+」菜单「对话 · smelt 原生界面」下那几项：新建 ACP 会话（第二种会话类型，
    /// 结构化消息流）。`agent` 决定接哪家（Claude / Copilot / Codex），命令从对应的
    /// 全局配置取。spawn_acp 只起线程立即返回，不需要 add_session_with_launch 那套
    /// 后台三段舞。
    fn add_acp_session(
        &mut self,
        agent: settings::AcpAgentKind,
        cmd_override: Option<String>,
        cwd: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cmd = cmd_override.unwrap_or_else(|| settings::acp_cmd_for(agent, cx));
        let view = cx.new(|cx| acp_view::AcpView::start(window, cx, agent, cmd, cwd));
        let _acp_persist_sub = Some(self.subscribe_acp_persist(&view, cx));
        self.sessions.push(Session {
            kind: SessionKind::Acp(view),
            custom_title: None,
            _acp_persist_sub,
        });
        self.active_session = self.sessions.len() - 1;
        self.stage_override = None;
        self.save_state(cx);
        cx.notify();
    }

    /// 找当前已开的、匹配某个 agent+cwd+具体 session id 的 ACP 会话下标——「继续」
    /// 点击时和后台加载完成时各查一次，两处逻辑必须完全一致，抽出来避免漂移。
    fn find_open_acp_session(
        &self,
        agent: settings::AcpAgentKind,
        cwd: &str,
        target_id: &agent_client_protocol::schema::v1::SessionId,
        cx: &App,
    ) -> Option<usize> {
        self.sessions.iter().position(|s| match &s.kind {
            SessionKind::Acp(view) => {
                let v = view.read(cx);
                v.agent_kind() == agent
                    && v.cwd().as_deref() == Some(cwd)
                    && v.resume_session_id_for_save().as_ref() == Some(target_id)
            }
            _ => false,
        })
    }

    /// 历史会话页「继续」：同一个 agent + 项目已经开着一个会话就直接跳过去
    /// （不重复开），没有就新建一个「已结束」占位、带上 `resume_session_id`，
    /// 靠 `activate()` 里已有的 `maybe_auto_resume` 触发真续接——跟"重开 GUI 后
    /// 切到旧会话自动续接"走的是完全同一套机制，这里不重新发明一遍。
    ///
    /// 历史内容要后台读、转换才能塞进新会话当本地快照（见下），这段异步窗口期
    /// 里得防两件事：连点同一条历史记录开出好几个重复标签页（`resume_inflight`
    /// 挡重复发起）；连点两条不同的历史记录时，文件更大/加载更慢那条后完成，
    /// 反而把已经激活的那条顶掉（`resume_gen` 保证只有"最新一次点击"才能真的
    /// 抢激活态，但该建的会话还是照建，不会凭空消失）。
    pub fn resume_acp_session(
        &mut self,
        agent: settings::AcpAgentKind,
        cmd_override: Option<String>,
        cwd: String,
        resume_id: String,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 已经开着的必须是**这一条**历史会话（比对 agent + cwd + 具体 session id），
        // 不能只认 agent+cwd——同项目同 agent 可能同时开着好几条不同的历史会话，
        // 之前只按 agent+cwd 找，点哪条「继续」都会跳到"第一个凑巧匹配的"那条。
        let target_id = agent_client_protocol::schema::v1::SessionId::new(resume_id.clone());
        if let Some(ix) = self.find_open_acp_session(agent, &cwd, &target_id, cx) {
            self.activate(ix, window, cx);
            return;
        }

        let inflight_key = format!("{}:{cwd}:{resume_id}", agent.id());
        if !self.resume_inflight.insert(inflight_key.clone()) {
            return; // 这一条已经在后台加载了，连点不重复发起
        }
        self.resume_gen = self.resume_gen.wrapping_add(1);
        let my_gen = self.resume_gen;

        let cmd = cmd_override.unwrap_or_else(|| settings::acp_cmd_for(agent, cx));
        // 先把历史内容读出来转成本地快照——`session/resume`（不重放历史，见 acp.rs
        // 里 apply_event 对 ReadyKind::ResumedKeepHistory 的注释）信任的就是这份本地
        // 快照，给个空的会话开出来就是一片空白（真实教训：第一版就是这么写的，
        // 点「继续」开出来的对话完全看不到历史）。放后台线程读，跟 open_session_detail
        // 同款套路，避免大会话文件卡住 UI 线程。
        cx.spawn_in(window, async move |this, cx| {
            let p = path.clone();
            let entries = cx
                .background_executor()
                .spawn(async move {
                    session_history::load_session_detail_for(agent, &p)
                        .map(|d| turns_to_acp_entries(&d.turns))
                        .unwrap_or_default()
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.resume_inflight.remove(&inflight_key);
                // 完成时再查一遍：万一这段等待期里这条会话已经从别处冒出来了
                // （比如 inflight 生效前就已经在飞的另一次请求刚落地），别再建
                // 重复的一份，跳过去就行。
                if let Some(ix) = this.find_open_acp_session(agent, &cwd, &target_id, cx) {
                    if this.resume_gen == my_gen {
                        this.activate(ix, window, cx);
                    }
                    return;
                }

                let view = cx.new(|cx| {
                    acp_view::AcpView::placeholder(
                        cx,
                        agent,
                        cmd,
                        Some(cwd),
                        "正在续接历史会话…".to_string(),
                        entries,
                        Some(agent_client_protocol::schema::v1::SessionId::new(resume_id)),
                    )
                });
                let _acp_persist_sub = Some(this.subscribe_acp_persist(&view, cx));
                this.sessions.push(Session {
                    kind: SessionKind::Acp(view),
                    custom_title: None,
                    _acp_persist_sub,
                });
                let ix = this.sessions.len() - 1;
                // 只有还是"最新一次点击"才抢激活态——连点两条不同历史记录时，
                // 加载慢的那条完成得晚也不该把已经激活的那条顶掉，但会话本身
                // 还是要建，不然用户点了却像什么都没发生，得自己翻标签页找。
                if this.resume_gen == my_gen {
                    this.activate(ix, window, cx);
                }
                this.save_state(cx);
            });
        })
        .detach();
    }

    /// 项目行「+」下拉菜单的快捷入口：`launch` 编进 shell 的启动命令行（见
    /// terminal.rs::spawn / smeltd.rs::spawn_session），`label` 用作侧栏初始显示名。
    ///
    /// **禁止**在 UI/`update`/拖放 FFI 回调里同步 `Terminal::spawn`：连守护 + 握手
    /// 含 sleep/超时，拖文件夹进窗口会整窗 beachball（见 `confirm_restart_daemon`）。
    /// 专用 OS 线程做阻塞 spawn，主线程只接结果建 Entity（比塞进 async executor 更稳）。
    fn add_session_with_launch(
        &mut self,
        cwd: Option<String>,
        launch: Option<&str>,
        label: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        let sid = new_sid();
        let cwd_bg = cwd.clone();
        let launch_owned = launch.map(str::to_string);
        let label_owned = label.map(str::to_string);
        let sid_bg = sid.clone();
        let launch_bg = launch_owned.clone();
        // 立刻给反馈，避免「点了像没点」。
        self.stage_override = None;
        eprintln!(
            "[workspace] 新建会话 cwd={cwd:?} launch={launch:?} sid={sid}"
        );
        cx.notify();

        let (tx, rx) = smol::channel::bounded(1);
        std::thread::Builder::new()
            .name("smelt-spawn-session".into())
            .spawn(move || {
                let r = terminal::Terminal::spawn(
                    24,
                    80,
                    cwd_bg.as_deref(),
                    &sid_bg,
                    launch_bg.as_deref(),
                );
                let _ = tx.send_blocking(r);
            })
            .expect("spawn smelt-spawn-session 线程");

        cx.spawn(async move |this, cx| {
            let result = match rx.recv().await {
                Ok(r) => r,
                Err(_) => {
                    let _ = this.update(cx, |this, cx| {
                        this.background_error =
                            Some("新建会话内部通道断开，请重试".into());
                        cx.notify();
                    });
                    return;
                }
            };
            let terminal = match result {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("[workspace] 新建会话失败（{cwd:?}）：{e:#}");
                    let msg = format!("新建会话失败：{e:#}");
                    let _ = this.update(cx, |this, cx| {
                        this.background_error = Some(msg);
                        cx.notify();
                    });
                    return;
                }
            };
            let _ = this.update(cx, |this, cx| {
                let view = cx.new(|cx| {
                    TerminalView::from_terminal(
                        cx,
                        terminal,
                        cwd,
                        sid,
                        launch_owned.as_deref(),
                        label_owned.as_deref(),
                    )
                });
                this.sessions.push(Session::single(view));
                this.active_session = this.sessions.len() - 1;
                this.stage_override = None;
                this.save_state(cx);
                eprintln!(
                    "[workspace] 新建会话成功，当前共 {} 个",
                    this.sessions.len()
                );
                cx.notify();
            });
        })
        .detach();
    }

    /// 在当前会话的活动 pane 上分屏：Horizontal=右侧并排，Vertical=下方堆叠。
    /// ACP 会话没有分屏树，直接忽略。
    fn split_active(&mut self, axis: Axis, cx: &mut Context<Self>) {
        let Some(sess) = self.cur() else { return };
        let Some(active) = sess.active_term() else { return };
        let cwd = active.read(cx).cwd().or_else(current_dir);
        let old = sess.anchor_id();
        let session_ix = self.active_session;
        let sid = new_sid();
        let cwd_bg = cwd.clone();
        let sid_bg = sid.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    terminal::Terminal::spawn(24, 80, cwd_bg.as_deref(), &sid_bg, None)
                })
                .await;
            let terminal = match result {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("[workspace] 分屏失败（{cwd:?}）：{e:#}");
                    return;
                }
            };
            let _ = this.update(cx, |this, cx| {
                // 分屏目标会话可能在握手期间被关掉——对不上就丢弃这个终端。
                if session_ix >= this.sessions.len() {
                    eprintln!("[workspace] 分屏目标会话已不存在，丢弃");
                    return;
                }
                let view =
                    cx.new(|cx| TerminalView::from_terminal(cx, terminal, cwd, sid, None, None));
                let state = cx.new(|_| ResizableState::default());
                let sess = &mut this.sessions[session_ix];
                // old 叶子若已被拆掉/关掉，split_leaf 找不到就不动。
                let Some(layout) = sess.term_layout_mut() else {
                    eprintln!("[workspace] 分屏目标会话不是终端会话，丢弃");
                    return;
                };
                if !split_leaf(layout, old, axis, state, view.clone()) {
                    eprintln!("[workspace] 分屏目标 pane 已不存在，丢弃");
                    return;
                }
                sess.set_active_term(view);
                this.save_state(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// 把所有会话（各自分屏树 + 活动叶子遍历序）+ 侧栏宽度 + 文件树列宽写入
    /// workspace.json（失败静默忽略）。
    fn save_state(&self, cx: &mut Context<Self>) {
        let Some(path) = ws_state_path() else { return };
        let mut sessions: Vec<SessionState> = self
            .sessions
            .iter()
            .map(|s| match &s.kind {
                SessionKind::Term { layout: l, .. } => {
                    let layout = pane_to_state(l, cx);
                    let mut ids = Vec::new();
                    collect_leaf_ids(l, &mut ids);
                    let active = ids
                        .iter()
                        .position(|x| *x == s.anchor_id())
                        .unwrap_or(0);
                    SessionState { layout, active, custom_title: s.custom_title.clone(), acp: None }
                }
                SessionKind::Acp(view) => {
                    let v = view.read(cx);
                    SessionState {
                        // 占位叶子：旧版 smelt 读到降级开普通终端，不炸档。
                        layout: PaneState::Leaf {
                            cwd: v.cwd(),
                            id: None,
                            custom_title: None,
                            launch_label: None,
                            launch_cmd: None,
                        },
                        active: 0,
                        custom_title: s.custom_title.clone(),
                        acp: Some(AcpSaved {
                            cwd: v.cwd(),
                            cmd: v.launch_cmd().to_string(),
                            agent: Some(v.agent_kind().id().to_string()),
                            entries: v.entries_for_save(),
                            resume_session_id: v.resume_session_id_for_save(),
                        }),
                    }
                }
            })
            .collect();
        // 启动时恢复失败的会话继续挂在存档里，下次冷启动重试。
        sessions.extend(self.restore_orphans.iter().cloned());

        // 安全阀：内存里一个会话都没有、也没有 orphan，但磁盘上还有旧存档 → 绝不
        // 用空列表覆盖（历史上「守护未就绪 → 恢复全失败 → save_state 抹盘」会把
        // 用户所有侧栏会话永久清掉）。
        if sessions.is_empty() {
            if let Some(existing) = load_ws_state() {
                let had = !existing.sessions.is_empty()
                    || existing.layout.is_some()
                    || !existing.tabs.is_empty();
                if had {
                    eprintln!(
                        "[workspace] 内存会话为空但磁盘存档有数据，跳过写盘以免抹掉 workspace.json"
                    );
                    return;
                }
            }
        }

        let file_tree_w = self.file_tree_resize.read(cx).sizes().first().copied().map(f32::from);
        let state = WsState {
            sessions,
            active_session: self.active_session,
            // 旧档的 sidebar_w 字段保留声明只为 serde 兼容；新布局固定宽，不再写。
            sidebar_w: None,
            file_tree_w,
            git_left_w: self.git_left_resize.read(cx).sizes().first().copied().map(f32::from),
            pinned_file_tree_roots: self.pinned_roots.clone(),
            collapsed_file_tree_roots: self.collapsed_roots.iter().cloned().collect(),
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
    /// 点一下就在 $HOME 新建一个终端——就这么简单，不做「已有就复用」那套
    /// （复用反而制造困惑：分不清是新建还是切旧的）。异步 spawn，完成后
    /// add_session_with_launch 会把舞台切到这个新终端。
    fn new_scratch_session(&mut self, cx: &mut Context<Self>) {
        self.add_session(scratch_dir(), cx);
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
    ///
    /// 整段路径判定 + `Terminal::spawn` 都在后台跑——`on_drop` / `on_open_urls` 在
    /// ObjC FFI 栈上，同步 spawn 会把整个窗口卡成 beachball（拖多文件更甚）。
    fn open_paths(&mut self, paths: &[std::path::PathBuf], cx: &mut Context<Self>) {
        if paths.is_empty() {
            eprintln!("[workspace] open_paths: 空路径列表，忽略");
            return;
        }
        eprintln!(
            "[workspace] open_paths: 收到 {} 条路径 {:?}",
            paths.len(),
            paths
        );
        // 立刻切到终端页并提示，避免用户以为拖了没反应（spawn 在后台要几百毫秒～数秒）。
        self.stage_override = None;
        cx.notify();

        let paths: Vec<std::path::PathBuf> = paths.to_vec();
        let (tx, rx) = smol::channel::bounded(1);
        std::thread::Builder::new()
            .name("smelt-open-paths".into())
            .spawn(move || {
                let mut out = Vec::with_capacity(paths.len());
                for p in paths {
                    let dir = if p.is_dir() {
                        p
                    } else {
                        match p.parent() {
                            Some(parent) => parent.to_path_buf(),
                            None => continue,
                        }
                    };
                    let Some(cwd) = dir.to_str().map(str::to_string) else {
                        continue;
                    };
                    let sid = new_sid();
                    let result = terminal::Terminal::spawn(24, 80, Some(&cwd), &sid, None);
                    out.push((cwd, sid, result));
                }
                let _ = tx.send_blocking(out);
            })
            .expect("spawn smelt-open-paths 线程");

        cx.spawn(async move |this, cx| {
            let built = match rx.recv().await {
                Ok(v) => v,
                Err(_) => {
                    let _ = this.update(cx, |this, cx| {
                        this.background_error = Some("打开路径内部通道断开".into());
                        cx.notify();
                    });
                    return;
                }
            };

            let _ = this.update(cx, |this, cx| {
                let mut ok_n = 0usize;
                let mut err_msgs: Vec<String> = Vec::new();
                for (cwd, sid, result) in built {
                    match result {
                        Ok(terminal) => {
                            let view = cx.new(|cx| {
                                TerminalView::from_terminal(
                                    cx,
                                    terminal,
                                    Some(cwd),
                                    sid,
                                    None,
                                    None,
                                )
                            });
                            this.sessions.push(Session::single(view));
                            this.active_session = this.sessions.len() - 1;
                            ok_n += 1;
                        }
                        Err(e) => {
                            eprintln!("[workspace] 拖入打开失败（{cwd}）：{e:#}");
                            err_msgs.push(format!("{cwd}: {e:#}"));
                        }
                    }
                }
                if ok_n > 0 {
                    this.stage_override = None;
                    this.save_state(cx);
                }
                if !err_msgs.is_empty() {
                    let head = err_msgs.into_iter().take(2).collect::<Vec<_>>().join("；");
                    this.background_error = Some(if ok_n > 0 {
                        format!("已打开 {ok_n} 个，另有失败：{head}")
                    } else {
                        format!("拖入打开失败：{head}")
                    });
                } else if ok_n == 0 {
                    this.background_error =
                        Some("拖入的路径无法作为项目目录打开".into());
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 关闭第 ix 个会话（至少保留一个）。用户主动关 → 让守护杀掉这些 shell
    /// （区别于退出 GUI：那时不杀，会话在 smeltd 里持久活着）。
    fn close_session(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.sessions.len() <= 1 || ix >= self.sessions.len() {
            return;
        }
        for t in &self.sessions[ix].term_leaves() {
            terminal::kill_remote(t.read(cx).session_id());
        }
        if let SessionKind::Acp(view) = &self.sessions[ix].kind {
            view.update(cx, |v, cx| v.shutdown(cx));
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
            let target = sess.anchor_id();
            // 用户主动关 pane → 守护真正杀掉该 shell。多 pane 必是终端会话。
            if let Some(active) = sess.active_term() {
                terminal::kill_remote(&active.read(cx).session_id().to_string());
            }
            let sess = &mut self.sessions[self.active_session];
            if let Some(layout) = sess.term_layout_mut() {
                remove_leaf(layout, target);
            }
            if let Some(first) = sess.term_leaves().first().cloned() {
                sess.set_active_term(first);
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
            sess.set_active_term(e.clone());
        }
        // 只聚焦、不清「需要注意」——查看≠处理，等用户实际输入回应了才清（见 TerminalView）。
        let h = e.read(cx).focus_handle();
        window.focus(&h, cx);
        self.save_state(cx);
        cx.notify();
    }

    /// 聚焦当前会话的活动终端（ACP 会话的聚焦走视图自身，这里跳过）。
    /// 设置/收回舞台覆盖页并处理焦点：全屏页自己没有可聚焦元素，焦点认领到根
    /// 让全局快捷键仍收得到；收回时把焦点还给活动会话（终端 pane / ACP 输入框）。
    pub(crate) fn set_stage_override(
        &mut self,
        v: Option<MainView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.stage_override = v;
        match v {
            Some(_) => window.focus(&self.focus_handle, cx),
            None => self.focus_active_stage(window, cx),
        }
        cx.notify();
    }

    /// 焦点还给活动会话：Term → 活动 pane；ACP → 消息流输入框。
    fn focus_active_stage(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(Session { kind: SessionKind::Acp(view), .. }) =
            self.sessions.get(self.active_session)
        {
            let view = view.clone();
            view.update(cx, |v, cx| v.focus_input(window, cx));
        } else {
            self.focus_active(window, cx);
        }
    }

    fn focus_active(&self, window: &mut Window, cx: &mut App) {
        if let Some(active) = self.cur().and_then(|s| s.active_term()) {
            let h = active.read(cx).focus_handle();
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

    /// 总览：打开会话，并尽量聚焦到「需要处理」的那个 pane。
    fn overview_open_session(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if ix >= self.sessions.len() {
            return;
        }
        let attention = self.sessions[ix].attention_pane(cx);
        self.activate(ix, window, cx);
        if let Some(pane) = attention {
            self.activate_pane(&pane, window, cx);
        }
    }

    /// 总览审批：向权限菜单所在 pane 注入选项键（来自网格解析的 `key`，如 `1`/`3`）。
    /// 留在总览，方便连批；不强制切终端页。
    fn overview_select_permission(
        &mut self,
        ix: usize,
        key: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if ix >= self.sessions.len() || key.is_empty() {
            return;
        }
        let Some(pane) = self.sessions[ix]
            .attention_pane(cx)
            .or_else(|| self.sessions[ix].active_term().cloned())
        else {
            return; // ACP 会话的审批走视图内按钮，不注 key
        };
        if let Some(sess) = self.sessions.get_mut(ix) {
            sess.set_active_term(pane.clone());
        }
        self.active_session = ix;
        let key = key.to_string();
        pane.update(cx, |tv, cx| {
            tv.type_text(&key, cx);
            tv.send_enter(cx);
        });
        cx.notify();
    }

    /// 切换到第 ix 个会话并聚焦。
    fn activate(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix < self.sessions.len() {
            self.active_session = ix;
            // 项目 rail 的选中态跟着活动会话走：切到别的项目的会话时 rail 与
            // 会话列表同步换过去。
            if let Some((name, _, _)) = self
                .project_groups(cx)
                .into_iter()
                .find(|(_, _, ixs)| ixs.contains(&ix))
            {
                self.active_project = Some(name);
            }
            // 点会话 = 回到会话舞台：收掉任何全屏覆盖页（总览/Git/文件树/…）。
            // 新布局没有常驻 TabBar，这里不收的话覆盖页会一直盖着舞台，
            // 「点了会话却还满屏 Git」看起来就像回不到终端。
            self.stage_override = None;
            // 切过去只是查看，不清「需要注意」——等用户实际输入回应了才清。
            // ACP 会话例外：绿点「有结果可看」查看即清（消息流全文可见，看到=处理）。
            if let SessionKind::Acp(view) = &self.sessions[ix].kind {
                view.update(cx, |v, cx| {
                    // 冷恢复占位第一次被切到 → 自动续接（免手点「重新开始」）。
                    v.maybe_auto_resume(window, cx);
                    v.mark_read();
                    v.focus_input(window, cx);
                });
            } else {
                self.focus_active(window, cx);
            }
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
        let leaves = sess.term_leaves();
        if leaves.len() < 2 {
            return;
        }
        let cur_id = sess.anchor_id();
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
                    smelt_core::block_on::block_on_tokio(updater::fetch_latest()).and_then(|r| r)
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
                smelt_core::block_on::block_on_tokio(updater::download_and_stage(&url, &v, |p| {
                    let _ = tx.try_send(p);
                }))
                .and_then(|r| r)
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
            // 只探测状态，不再在此 ensure/handoff（冷启动 ensure 由 restore 线程串行做完）。
            // 仍落后则无缝升级到磁盘最新 smeltd。
            let (outdated, info) = cx
                .background_executor()
                .spawn(async { (terminal::daemon_outdated(), terminal::daemon_info()) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.daemon_info = info;
                this.daemon_outdated = Some(outdated);
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
            let leaves = sess.term_leaves();
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
            // exec 换代后 PID / 启动时刻都变了，跟版本一起重新问一遍。
            let (outdated, info) = cx
                .background_executor()
                .spawn(async { (terminal::daemon_outdated(), terminal::daemon_info()) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.daemon_upgrading = false;
                this.daemon_info = info;
                this.daemon_outdated = Some(outdated);
                this.daemon_upgrade_msg = Some(match outcome {
                    terminal::UpgradeOutcome::Upgraded => {
                        // 交接后守护侧 jolt 要等客户端 resize；略延迟再 reconnect，
                        // 避免刚 exec 完就 attach 撞上空 Term + jolt 还没完成。
                        this.schedule_reconnect_all_terminals(cx);
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
                        this.schedule_reconnect_all_terminals(cx);
                        "升级结果未确认（可能已生效但检测超时），已尝试重连各终端；如仍无响应可重试或改用重启。".to_string()
                    }
                });
                cx.notify();
            });
        })
        .detach();
    }

    /// GUI 侧栏当前认领的全部 session id（Term 会话；ACP 会话是 GUI 直接 spawn
    /// 的子进程，根本不经过 smeltd，不出现在 list 结果里，不用管）。跟 list
    /// 查回来的全量做差集，剩下的就是「守护持有但没有任何侧栏在追踪」的孤儿
    /// ——测试跑出来的、忘了关的临时会话，都会落在这一类。
    fn tracked_session_ids(&self, cx: &App) -> std::collections::HashSet<String> {
        self.sessions
            .iter()
            .flat_map(|s| s.term_leaves())
            .map(|t| t.read(cx).session_id().to_string())
            .collect()
    }

    /// 打开「会话管理」弹窗并触发一次查询（每次打开都重新拉最新数据，不复用
    /// 上次缓存——孤儿是不是还在、有没有新泄漏，都得是当下的事实）。
    fn open_session_manager(&mut self, cx: &mut Context<Self>) {
        self.session_manager_open = true;
        self.session_manager_list = None;
        cx.notify();
        self.refresh_session_manager(cx);
    }

    fn refresh_session_manager(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let list = cx.background_executor().spawn(async { terminal::list_daemon_sessions() }).await;
            let _ = this.update(cx, |this, cx| {
                this.session_manager_list = Some(list);
                cx.notify();
            });
        })
        .detach();
    }

    /// 关掉守护进程里的一个会话（真杀底层 shell），关完刷新列表。
    fn kill_session_in_manager(&mut self, id: String, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .spawn({
                    let id = id.clone();
                    async move { terminal::kill_remote(&id) }
                })
                .await;
            let _ = this.update(cx, |this, cx| this.refresh_session_manager(cx));
        })
        .detach();
    }

    /// 批量清理「没有任何侧栏在追踪」的孤儿——不碰任何 GUI 认领的正常会话，
    /// 不需要走「重启守护进程」那种连坐所有会话的核选项。
    fn kill_all_orphans_in_manager(&mut self, cx: &mut Context<Self>) {
        let tracked = self.tracked_session_ids(cx);
        let Some(list) = self.session_manager_list.clone() else { return };
        let orphan_ids: Vec<String> =
            list.into_iter().map(|s| s.id).filter(|id| !tracked.contains(id)).collect();
        if orphan_ids.is_empty() {
            return;
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .spawn(async move {
                    for id in &orphan_ids {
                        terminal::kill_remote(id);
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| this.refresh_session_manager(cx));
        })
        .detach();
    }

    /// 「会话管理」弹窗：列出守护进程持有的全部会话，标出哪些是孤儿（没有任何
    /// 侧栏在追踪），逐个/批量清理。跟 render_quit_confirm 同款视觉。
    fn render_session_manager(&self, cx: &mut Context<Self>) -> Div {
        let (fg, muted, border) = {
            let t = cx.theme();
            (t.foreground, t.muted_foreground, t.border)
        };
        let (neutral_bg, neutral_hover, tint, hover, accent_text) = Self::modal_accent_colors(true);
        let tracked = self.tracked_session_ids(cx);

        let body: AnyElement = match &self.session_manager_list {
            None => div().text_sm().text_color(muted).child("查询中…").into_any_element(),
            Some(list) if list.is_empty() => {
                div().text_sm().text_color(muted).child("守护进程当前没有任何会话。").into_any_element()
            }
            Some(list) => {
                let orphan_count = list.iter().filter(|s| !tracked.contains(&s.id)).count();
                let mut rows =
                    v_flex().id("session-manager-list").gap_1().max_h(px(360.)).overflow_y_scroll();
                for s in list {
                    let is_orphan = !tracked.contains(&s.id);
                    let label = s
                        .cwd
                        .clone()
                        .or_else(|| s.title.clone())
                        .unwrap_or_else(|| s.id.clone());
                    let id_for_kill = s.id.clone();
                    rows = rows.child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .justify_between()
                            .py_1()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .min_w_0()
                                    .child(div().size_2().rounded_full().bg(if is_orphan {
                                        rgb(ui_theme::red())
                                    } else {
                                        rgb(ui_theme::green())
                                    }))
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(fg)
                                            .truncate()
                                            .child(label),
                                    )
                                    .children(is_orphan.then(|| {
                                        div()
                                            .text_xs()
                                            .text_color(rgb(ui_theme::red()))
                                            .child("孤儿（无侧栏追踪）")
                                    })),
                            )
                            .child(Self::modal_button(
                                "kill-session-in-manager",
                                "关闭",
                                neutral_bg,
                                neutral_hover,
                                fg,
                                move |this, _, _, cx| {
                                    this.kill_session_in_manager(id_for_kill.clone(), cx);
                                },
                                cx,
                            )),
                    );
                }
                v_flex()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(format!("共 {} 个，{orphan_count} 个孤儿", list.len())),
                    )
                    .child(rows)
                    .into_any_element()
            }
        };

        let has_orphans = self
            .session_manager_list
            .as_ref()
            .map(|l| l.iter().any(|s| !tracked.contains(&s.id)))
            .unwrap_or(false);

        let content = v_flex()
            .child(div().font_bold().text_color(fg).text_lg().child("会话管理"))
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child("守护进程持有的全部会话；孤儿是没有被任何窗口侧栏追踪的（测试跑出来的、忘了关的临时会话），清理它们不影响正常使用中的会话。"),
            )
            .child(div().border_t_1().border_color(border).pt_3().child(body))
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(Self::modal_button(
                        "close-session-manager",
                        "关闭",
                        neutral_bg,
                        neutral_hover,
                        fg,
                        |this, _, _, cx| {
                            this.session_manager_open = false;
                            cx.notify();
                        },
                        cx,
                    ))
                    .when(has_orphans, |el| {
                        el.child(Self::modal_button(
                            "kill-all-orphans",
                            "清理全部孤儿",
                            tint,
                            hover,
                            accent_text,
                            |this, _, _, cx| {
                                this.kill_all_orphans_in_manager(cx);
                            },
                            cx,
                        ))
                    }),
            );
        Self::modal_shell(420., true, content, cx)
    }

    /// upgrade 完成后延迟 reattach：给守护 handoff 泵线程 / jolt 一点时间。
    fn schedule_reconnect_all_terminals(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(400))
                .await;
            let _ = this.update(cx, |this, cx| {
                this.reconnect_all_terminals(cx);
            });
        })
        .detach();
    }

    /// 用户在弹窗里点了「确定重启」：让守护退出（断开所有会话）、拉起磁盘上最新的
    /// smeltd、再刷新状态。
    ///
    /// **禁止**在 `update` 里同步 `Terminal::spawn`：握手含 sleep/轮询，多 pane 会把
    /// UI 卡死（「点重启守护就假死」）。流程：后台杀+拉起守护 → 后台按 cwd/sid 建
    /// Terminal → 主线程 `adopt_terminal` 挂回各 pane。
    fn confirm_restart_daemon(&mut self, cx: &mut Context<Self>) {
        self.show_daemon_restart_confirm = false;
        self.daemon_outdated = None;
        // 收集重建参数（Entity 可 Clone；真正 spawn 扔后台）。
        // launch_cmd 必须带上：硬重启会清掉守护里的会话，同 id 走新建分支，
        // 不带 launch 就只剩裸 shell，agent 会话等于全丢。
        let mut jobs: Vec<(Entity<TerminalView>, Option<String>, String, Option<String>)> =
            Vec::new();
        for sess in &self.sessions {
            let leaves = sess.term_leaves();
            for leaf in leaves {
                let view = leaf.read(cx);
                let cwd = view.cwd();
                let sid = view.session_id().to_string();
                let launch = view.launch_cmd().map(str::to_string);
                jobs.push((leaf, cwd, sid, launch));
            }
        }
        cx.notify();
        cx.spawn(async move |this, cx| {
            // 硬重启后是全新进程，PID / 启动时刻 / 会话数都得重问。
            let (outdated, info) = cx
                .background_executor()
                .spawn(async {
                    terminal::restart_daemon();
                    terminal::ensure_daemon_running();
                    (terminal::daemon_outdated(), terminal::daemon_info())
                })
                .await;

            // 握手/重试全在后台；主线程只接结果
            let built = cx
                .background_executor()
                .spawn(async move {
                    let mut out = Vec::with_capacity(jobs.len());
                    for (entity, cwd, sid, launch) in jobs {
                        let term = terminal::Terminal::spawn(
                            24,
                            80,
                            cwd.as_deref(),
                            &sid,
                            launch.as_deref(),
                        );
                        out.push((entity, term));
                    }
                    out
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                this.daemon_info = info;
                this.daemon_outdated = Some(outdated);
                let mut failed = 0usize;
                for (entity, term) in built {
                    match term {
                        Ok(t) => {
                            entity.update(cx, |view, cx| view.adopt_terminal(t, cx));
                        }
                        Err(e) => {
                            failed += 1;
                            eprintln!("[workspace] 硬重启后重开终端失败：{e:#}");
                        }
                    }
                }
                if failed > 0 {
                    this.background_error = Some(format!(
                        "守护已重启，但有 {failed} 个终端没能重开（侧栏会话仍在，可关了再开）"
                    ));
                } else {
                    this.daemon_upgrade_msg =
                        Some("守护已硬重启，会话已按原目录/启动命令重建。".into());
                }
                // 布局没变，写盘刷新 launch_cmd 等字段即可。
                this.save_state(cx);
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
            let viewing = (si == self.active_session).then(|| s.anchor_id());
            let leaves = s.term_leaves();
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
                            .child(div().size_2().rounded_full().bg(rgb(ui_theme::blue())))
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
                        format!(
                            "会话监控 · {need_attn} 需关注 · hook 事实 + 终端权限菜单"
                        )
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
        let card_bg = rgb(ui_theme::bg_card());
        let card_border = ui_theme::overlay(0x12);
        let soft_bg: Hsla = ui_theme::overlay(0x0d).into();
        let c_red: Hsla = rgb(ui_theme::red()).into();
        let c_blue: Hsla = rgb(ui_theme::blue()).into();
        let c_green: Hsla = rgb(ui_theme::green()).into();
        let c_amber: Hsla = rgb(ui_theme::yellow()).into();
        let red_tint: Hsla = ui_theme::tint(ui_theme::red(), 0x22).into();
        let blue_tint: Hsla = ui_theme::tint(ui_theme::blue(), 0x22).into();
        let green_tint: Hsla = ui_theme::tint(ui_theme::green(), 0x22).into();
        let amber_tint: Hsla = ui_theme::tint(ui_theme::yellow(), 0x22).into();
        let c_muted_dot: Hsla = ui_theme::tint(ui_theme::text_muted(), 0xaa).into();

        let statuses: Vec<AgentStatus> = self.sessions.iter().map(|s| s.status(cx)).collect();
        let need = statuses
            .iter()
            .filter(|s| matches!(s, AgentStatus::WaitingApproval | AgentStatus::NeedsAttention))
            .count();
        let running = statuses.iter().filter(|s| matches!(s, AgentStatus::Running)).count();
        let done = statuses.iter().filter(|s| matches!(s, AgentStatus::Done)).count();
        let filter = self.overview_filter;
        // 筛选：要一眼像「分段按钮」，未选中也有底/边/hover，别和装饰 pill 混。
        let filter_chip = |id: &'static str, label: String, f: OverviewFilter, color: Hsla, tint: Hsla| {
            let on = filter == f;
            let idle_bg: Hsla = ui_theme::overlay(0x14).into();
            let idle_border: Hsla = ui_theme::overlay(0x28).into();
            div()
                .id(id)
                .px(px(12.))
                .py(px(6.))
                .rounded_md()
                .cursor_pointer()
                .bg(if on { tint } else { idle_bg })
                .border_1()
                .border_color(if on { color } else { idle_border })
                .text_sm()
                .font_weight(if on {
                    gpui::FontWeight::SEMIBOLD
                } else {
                    gpui::FontWeight::NORMAL
                })
                .text_color(if on { color } else { fg })
                .hover(|d| {
                    d.bg(if on { tint } else { ui_theme::overlay(0x22).into() })
                        .border_color(color)
                })
                .child(label)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        this.overview_filter = f;
                        cx.notify();
                    }),
                )
        };

        let summary = div()
            .flex()
            .items_center()
            .gap_2()
            .flex_wrap()
            .mb_4()
            .child(filter_chip(
                "ov-f-all",
                format!("全部 {}", self.sessions.len()),
                OverviewFilter::All,
                fg,
                soft_bg,
            ))
            .child(filter_chip(
                "ov-f-need",
                format!("待我处理 {need}"),
                OverviewFilter::NeedsMe,
                c_red,
                red_tint,
            ))
            .child(filter_chip(
                "ov-f-run",
                format!("运行中 {running}"),
                OverviewFilter::Running,
                c_blue,
                blue_tint,
            ))
            .children((done > 0).then(|| {
                // 纯统计，不可点——灰一点与上面筛选按钮区分
                div()
                    .px(px(12.))
                    .py(px(6.))
                    .rounded_md()
                    .bg(soft_bg)
                    .text_sm()
                    .text_color(muted)
                    .child(format!("{done} 已完成"))
            }));

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

        let mut order: Vec<usize> = (0..self.sessions.len())
            .filter(|&ix| match filter {
                OverviewFilter::All => true,
                OverviewFilter::NeedsMe => matches!(
                    statuses[ix],
                    AgentStatus::WaitingApproval | AgentStatus::NeedsAttention
                ),
                OverviewFilter::Running => matches!(statuses[ix], AgentStatus::Running),
            })
            .collect();
        order.sort_by_key(|&ix| match statuses[ix] {
            AgentStatus::WaitingApproval => 0,
            AgentStatus::NeedsAttention => 1,
            AgentStatus::Running => 2,
            AgentStatus::Done => 3,
            AgentStatus::Idle => 4,
        });

        if order.is_empty() {
            let empty_hint = match filter {
                OverviewFilter::NeedsMe => "当前没有需要你处理的会话",
                OverviewFilter::Running => "当前没有运行中的会话",
                OverviewFilter::All => "没有会话",
            };
            return div()
                .child(summary)
                .child(
                    div()
                        .py_12()
                        .flex()
                        .justify_center()
                        .text_sm()
                        .text_color(muted)
                        .child(empty_hint),
                );
        }

        let cards: Vec<_> = order
            .into_iter()
            .map(|ix| {
                let cwd_opt = self.sessions[ix].cwd(cx);
                if let Some(c) = cwd_opt.clone() {
                    self.ensure_session_list(settings::AcpAgentKind::Claude, None, c, cx);
                }
                let live = cwd_opt
                    .as_deref()
                    .and_then(|c| {
                        self.session_list.get(&session_history::session_list_key(
                            settings::AcpAgentKind::Claude,
                            None,
                            c,
                        ))
                    })
                    .and_then(|(_, list)| list.first());
                // 状态通道（hook 事实）优先；jsonl 作补充。
                let daemon_detail = self.sessions[ix]
                    .term_leaves()
                    .iter()
                    .find_map(|t| daemon_state_for(t, cx));
                let phase_label = daemon_detail
                    .as_ref()
                    .map(|d| d.phase_label().to_string());
                let phase_detail = daemon_detail.as_ref().and_then(|d| d.detail_line());
                let phase_age = daemon_detail.as_ref().and_then(|d| d.phase_age_secs());
                let mut meta_parts: Vec<String> = Vec::new();
                if let Some((a, b)) = live.and_then(|s| s.started_at.zip(s.last_active_at)) {
                    let mins = (b - a).num_minutes().max(0);
                    if mins > 0 {
                        meta_parts.push(format!("⏱ {mins} 分钟"));
                    }
                }
                if let Some(tokens) = live.map(|s| s.total_tokens).filter(|t| *t > 0) {
                    meta_parts.push(format!("🔢 {}", format_count(tokens)));
                }
                if phase_detail.is_none() {
                    if let Some(tool) = live.and_then(|s| s.last_tool.clone()) {
                        meta_parts.push(format!("🔧 最近 {tool}"));
                    }
                }
                let meta_line = (!meta_parts.is_empty()).then(|| meta_parts.join(" · "));

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
                let status = statuses[ix];
                let (dot, label, tint) = match status {
                    AgentStatus::WaitingApproval => (c_red, "等你批准", red_tint),
                    AgentStatus::NeedsAttention => (c_amber, "需要处理", amber_tint),
                    AgentStatus::Running => (c_blue, "运行中", blue_tint),
                    AgentStatus::Done => (c_green, "已完成", green_tint),
                    AgentStatus::Idle => (c_muted_dot, "空闲", soft_bg),
                };
                let needs_user = matches!(
                    status,
                    AgentStatus::WaitingApproval | AgentStatus::NeedsAttention
                );
                let is_approval = status == AgentStatus::WaitingApproval;
                let perm = s.permission_prompt(cx);
                let panes = s.pane_count();
                let notif = s.notification_msg(cx);
                let when = s.notified_at(cx).map(ago);
                let preview = s.preview(cx, 3);
                let git = cwd_opt.as_ref().and_then(|c| self.git_cache.get(c).cloned());
                // 等审批时描红边，方便在网格里扫到
                let card_edge: Hsla = if is_approval {
                    c_red.into()
                } else if status == AgentStatus::NeedsAttention {
                    c_amber.into()
                } else {
                    card_border.into()
                };
                // 审批/工具事实：hook 优先；过滤掉终端预览误扫、说明文案等垃圾。
                let fact_question = phase_detail
                    .clone()
                    .or_else(|| {
                        // 仅审批/需关注时用 OSC 通知垫底，避免空闲会话把预览塞进红块
                        if matches!(
                            statuses[ix],
                            AgentStatus::WaitingApproval | AgentStatus::NeedsAttention
                        ) {
                            notif.clone()
                        } else {
                            None
                        }
                    })
                    .filter(|m| overview_fact_is_usable(m));
                let age_str = phase_age.map(|a| {
                    if a < 60 {
                        format!("已等 {a} 秒")
                    } else {
                        format!("已等 {} 分钟", a / 60)
                    }
                });

                div()
                    .id(("ov-card", ix))
                    .w(px(300.))
                    .p_4()
                    .rounded(px(18.))
                    .border_1()
                    .border_color(card_edge)
                    .when(is_approval, |d| d.border_2().border_color(c_red))
                    // 等审批的卡片底压一层薄红，跟普通卡片一眼分开
                    .bg(if is_approval {
                        ui_theme::tint(ui_theme::red(), 0x1a)
                    } else {
                        card_bg
                    })
                    .shadow_sm()
                    .cursor_pointer()
                    .hover(|d| d.border_color(dot).shadow_lg().bg(rgb(ui_theme::bg_hover())))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _ev, window, cx| {
                            this.overview_open_session(ix, window, cx);
                        }),
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
                            .children(age_str.clone().or(when).map(|w| {
                                div()
                                    .text_xs()
                                    .text_color(if is_approval { c_red } else { muted })
                                    .flex_shrink_0()
                                    .child(w)
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
                                    .child(
                                        phase_label
                                            .clone()
                                            .unwrap_or_else(|| label.to_string()),
                                    ),
                            )
                            .child(div().text_color(muted).child(cwd))
                            .child(div().text_color(muted).child(format!("· {panes} 窗格"))),
                    )
                    // hook 事实块：工具 / 审批问句（审批时更醒目）
                    .children(fact_question.as_ref().map(|q| {
                        let (bg, tc) = if is_approval {
                            (ui_theme::tint(ui_theme::red(), 0x33), c_red)
                        } else if matches!(status, AgentStatus::Running) {
                            (ui_theme::tint(ui_theme::blue(), 0x22), c_blue)
                        } else if matches!(status, AgentStatus::NeedsAttention) {
                            (ui_theme::tint(ui_theme::yellow(), 0x22), c_amber)
                        } else {
                            (ui_theme::overlay(0x0d), muted)
                        };
                        div()
                            .px(px(10.))
                            .py(px(8.))
                            .rounded_lg()
                            .bg(bg)
                            .text_sm()
                            .text_color(tc)
                            .line_clamp(4)
                            .child(q.clone())
                    }))
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
                    .children(meta_line.map(|line| {
                        div().text_xs().text_color(muted).truncate().child(line)
                    }))
                    // OSC 通知且与 hook 问句不同时再显示
                    .children(
                        notif
                            .filter(|m| {
                                fact_question
                                    .as_ref()
                                    .is_none_or(|q| !q.contains(m.as_str()) && m != q)
                            })
                            .map(|m| {
                                let (bg, tc) = if is_approval {
                                    (ui_theme::tint(ui_theme::red(), 0x22), c_red)
                                } else {
                                    (ui_theme::tint(ui_theme::yellow(), 0x22), c_amber)
                                };
                                div()
                                    .px(px(8.))
                                    .py(px(4.))
                                    .rounded_lg()
                                    .bg(bg)
                                    .text_xs()
                                    .text_color(tc)
                                    .line_clamp(2)
                                    .child(m)
                            }),
                    )
                    .children((!preview.is_empty()).then(|| {
                        div()
                            .p_2()
                            .rounded_lg()
                            .bg(rgb(ui_theme::bg_status()))
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
                    // 需要用户时：实心/描边按钮，和侧栏「新建终端」一样有底有边
                    .when(needs_user, |card| {
                        let ix_open = ix;
                        let opts = perm.as_ref().map(|p| p.options.clone()).unwrap_or_default();
                        let has_opts = !opts.is_empty();
                        let btn_idle: Hsla = ui_theme::overlay(0x18).into();
                        let btn_border: Hsla = ui_theme::overlay(0x30).into();
                        card.child(
                            div()
                                .flex()
                                .flex_wrap()
                                .items_center()
                                .gap_2()
                                .pt_1()
                                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                    cx.stop_propagation()
                                })
                                .children(opts.into_iter().enumerate().map(|(oi, opt)| {
                                    let ix_sel = ix;
                                    let key = opt.key.clone();
                                    let label = opt.button_label();
                                    let primary = opt.is_primary();
                                    div()
                                        .id(SharedString::from(format!("ov-perm-{ix_sel}-{oi}")))
                                        .px(px(12.))
                                        .py(px(6.))
                                        .rounded_md()
                                        .cursor_pointer()
                                        .border_1()
                                        .border_color(if primary {
                                            c_green
                                        } else {
                                            btn_border
                                        })
                                        .bg(if primary {
                                            green_tint
                                        } else {
                                            btn_idle
                                        })
                                        .text_sm()
                                        .font_weight(gpui::FontWeight::MEDIUM)
                                        .text_color(if primary { c_green } else { fg })
                                        .hover(|d| d.opacity(0.88).border_color(fg))
                                        .child(label)
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(move |this, _, window, cx| {
                                                this.overview_select_permission(
                                                    ix_sel, &key, window, cx,
                                                );
                                            }),
                                        )
                                }))
                                // 网格没扫到菜单时的兜底：固定 1/3
                                .when(is_approval && !has_opts, |row| {
                                    let ix_a = ix;
                                    let ix_d = ix;
                                    row.child(
                                        div()
                                            .id(("ov-allow", ix_a))
                                            .px(px(12.))
                                            .py(px(6.))
                                            .rounded_md()
                                            .cursor_pointer()
                                            .border_1()
                                            .border_color(c_green)
                                            .bg(green_tint)
                                            .text_sm()
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .text_color(c_green)
                                            .hover(|d| d.opacity(0.88))
                                            .child("允许")
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                cx.listener(move |this, _, window, cx| {
                                                    this.overview_select_permission(
                                                        ix_a, "1", window, cx,
                                                    );
                                                }),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .id(("ov-deny", ix_d))
                                            .px(px(12.))
                                            .py(px(6.))
                                            .rounded_md()
                                            .cursor_pointer()
                                            .border_1()
                                            .border_color(c_red)
                                            .bg(red_tint)
                                            .text_sm()
                                            .font_weight(gpui::FontWeight::MEDIUM)
                                            .text_color(c_red)
                                            .hover(|d| d.opacity(0.88))
                                            .child("拒绝")
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                cx.listener(move |this, _, window, cx| {
                                                    this.overview_select_permission(
                                                        ix_d, "3", window, cx,
                                                    );
                                                }),
                                            ),
                                    )
                                })
                                .child(
                                    div()
                                        .id(("ov-open", ix_open))
                                        .px(px(12.))
                                        .py(px(6.))
                                        .rounded_md()
                                        .cursor_pointer()
                                        .border_1()
                                        .border_color(if is_approval || has_opts {
                                            btn_border
                                        } else {
                                            c_blue
                                        })
                                        .bg(if is_approval || has_opts {
                                            btn_idle
                                        } else {
                                            blue_tint
                                        })
                                        .text_sm()
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .text_color(if is_approval || has_opts {
                                            fg
                                        } else {
                                            c_blue
                                        })
                                        .hover(|d| d.opacity(0.88).border_color(c_blue))
                                        .child(if is_approval {
                                            "打开终端"
                                        } else {
                                            "打开"
                                        })
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(move |this, _, window, cx| {
                                                this.overview_open_session(ix_open, window, cx);
                                            }),
                                        ),
                                ),
                        )
                    })
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
        let neutral_bg: Hsla = ui_theme::overlay(0x0a).into();
        let neutral_hover: Hsla = ui_theme::overlay(0x1f).into();
        if danger {
            (
                neutral_bg,
                neutral_hover,
                ui_theme::tint(ui_theme::red(), 0x24).into(),
                ui_theme::tint(ui_theme::red(), 0x40).into(),
                Hsla::from(rgb(ui_theme::red())),
            )
        } else {
            // 主操作（确定退出 / 提交 等）用**实心品牌色 blurple + 白字**，不再是
            // 「薄底 + 彩字」那种轮廓感——之前还错用了青蓝 blue()，既不突出也不是
            // 色板的强调色。danger 保持红薄底（危险操作克制警示）。
            (
                neutral_bg,
                neutral_hover,
                Hsla::from(rgb(ui_theme::accent())),
                ui_theme::tint(ui_theme::accent(), 0xdd).into(),
                Hsla::from(rgb(ui_theme::on_accent())),
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
            .border_color(ui_theme::overlay(0x12))
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
                                // 与设置页「立即重启更新」相同：先 handoff 守护再换包。
                                let _ =
                                    crate::terminal::install_app_preserving_sessions(staged_app);
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
    ///
    /// 入口只在设置窗「更新」页；弹层挂在设置窗上（见 `SettingsWindow::render`），
    /// 不再画到主窗口，避免「按钮在设置、确认框跑到主界面」的割裂感。
    pub(crate) fn render_daemon_restart_confirm(&self, cx: &mut Context<Self>) -> Div {
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
                    .is_some_and(|s| s.anchor_id() == t.entity_id());
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


impl Workspace {
    /// 舞台覆盖页（旧全屏页）：总览 / 任务 / 文件树 / Git / 热力图 / 历史。
    /// 原主区 TabBar 分派的 match 各臂原样搬入，由 stage_override 驱动。
    fn render_stage_override(
        &mut self,
        v: MainView,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (c_border, c_muted, c_fg) = {
            let t = cx.theme();
            (t.border, t.muted_foreground, t.foreground)
        };
        let _ = (c_border, c_muted, c_fg);
        match v {
            MainView::Overview => self.render_overview(window, cx).into_any_element(),
            // 只出详情：对应的列表/树在右侧停靠面板里常驻，舞台不再摆第二份
            MainView::FileDetail => div()
                .flex_1()
                .min_w_0()
                .min_h_0()
                .flex()
                .child(file_content_pane(&self.open_file, cx))
                .into_any_element(),
            MainView::DiffDetail => self.git_diff_only_pane(window, cx),
            MainView::Tasks => self.render_tasks_page(window, cx).into_any_element(),
                        MainView::Files => {
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
                                // 多根工作区：把所有项目根一起铺开（顺序同侧栏项目列表）；
                                // 每个根各查各自的 git status 标 M/A/D，归属由 file_tree 内部
                                // 按行所属的根处理，不再是全局一份。
                                let roots = self.workspace_roots(cx);
                                file_tree(
                                    &roots,
                                    &self.expanded,
                                    &self.collapsed_roots,
                                    &self.dir_cache,
                                    &self.file_tree_scroll,
                                    open_path,
                                    selected,
                                    self.file_tree_w,
                                    &self.git_status,
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
                                .into_any_element()
                        }
                        MainView::Git => {
                            let cwd = self.cur().and_then(|s| s.cwd(cx));
                            let c_border = cx.theme().border;
                            // 「改动 / 日志」子标签。两者同属 Git 工具窗口，不占两个
                            // 顶层标签（JetBrains 也是这个结构）。
                            let sub_tabs = {
                                let (fg, muted, accent) = {
                                    let t = cx.theme();
                                    (t.foreground, t.muted_foreground, t.accent)
                                };
                                h_flex()
                                    .gap_1()
                                    .px_3()
                                    .py_1()
                                    .border_b_1()
                                    .border_color(c_border)
                                    .children([(GitTab::Changes, "改动"), (GitTab::Log, "日志")].map(
                                        |(tab, label)| {
                                            let on = self.git_tab == tab;
                                            div()
                                                .id(label)
                                                .px_2()
                                                .py(px(1.0))
                                                .text_sm()
                                                .rounded_sm()
                                                .cursor_pointer()
                                                .text_color(if on { fg } else { muted })
                                                .when(on, |d| d.bg(accent))
                                                .hover(|d| d.opacity(0.8))
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.git_tab = tab;
                                                    cx.notify();
                                                }))
                                                .child(label)
                                        },
                                    ))
                                    // 热力图入口：原顶部 TabBar 撤掉后挂靠在 Git 页
                                    // （同为「看仓库」维度的全屏视图）。
                                    .child(
                                        div()
                                            .id("git-tab-hotspot")
                                            .px_2()
                                            .py(px(1.0))
                                            .text_sm()
                                            .rounded_sm()
                                            .cursor_pointer()
                                            .text_color(muted)
                                            .hover(|d| d.opacity(0.8))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.stage_override = Some(MainView::Hotspot);
                                                cx.notify();
                                            }))
                                            .child("热力图"),
                                    )
                            };

                            let body = match self.git_tab {
                                GitTab::Changes => {
                                    let status = cwd
                                        .as_ref()
                                        .and_then(|r| self.git_status.get(r).map(|(_, d)| d));
                                    let branches = cwd
                                        .as_ref()
                                        .and_then(|r| self.branches.get(r).map(|(_, d)| d));
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
                                            // 多行 + 自增高：commit message 的规范写法是
                                            // 「标题空行正文」，单行框根本写不了 body，
                                            // AI 生成的多段说明也会被挤成一行。
                                            InputState::new(window, cx)
                                                .multi_line(true)
                                                .auto_grow(2, 8)
                                                .placeholder("Commit message（可多行；点「AI 生成」起草）")
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
                                        self.pushing,
                                        &self.git_files_scroll,
                                        &self.diff_scroll,
                                        self.active_hunk,
                                        &self.git_left_resize,
                                        self.git_left_w,
                                        &self.git_tree_collapsed,
                                        self.diff_scope,
                                        cx,
                                    )
                                }
                                GitTab::Log => {
                                    if let Some(root) = cwd.clone() {
                                        // 进页面就保证数据在（内部按 root 去重，不会每帧拉）；
                                        // 分支列表复用 Git 页那份缓存，左侧树才有内容。
                                        self.ensure_git_log(root.clone(), cx);
                                        self.ensure_branches(root, cx);
                                    }
                                    // 状态在这里取好再传下去：渲染函数内部绝不能 read 自己的
                                    // entity（render 期间它正被可变借用，重入会直接 abort）。
                                    let branches = cwd
                                        .as_ref()
                                        .and_then(|r| self.branches.get(r))
                                        .map(|(_, b)| b);
                                    // 当前检出的分支：日志默认看它，分支树里也要标出来。
                                    // 复用 Git 页已有的 status 缓存，不另跑一次 git。
                                    let head_branch = cwd
                                        .as_ref()
                                        .and_then(|r| self.git_status.get(r))
                                        .map(|(_, d)| d.branch_name().to_string())
                                        .filter(|b| !b.is_empty());
                                    // 三栏都可拖拽：窗口窄的时候能自己腾地方，写死
                                    // 宽度的话中间的提交列表会被挤得没法看。
                                    div().flex_1().min_h_0().flex().child(
                                        h_resizable("git-log-split")
                                            .with_state(&self.git_log_resize)
                                            // 左：分支树
                                            .child(
                                                resizable_panel()
                                                    .size(px(200.))
                                                    .size_range(px(140.)..px(360.))
                                                    .child(
                                                        div()
                                                            .size_full()
                                                            .min_h_0()
                                                            .border_r_1()
                                                            .border_color(c_border)
                                                            .child(git_log_view::branch_tree(
                                                                cwd.clone(),
                                                                branches,
                                                                &self.git_log.scope,
                                                                head_branch.clone(),
                                                                cx,
                                                            )),
                                                    ),
                                            )
                                            // 中：分支图 + 提交列表
                                            // wrapper 必须 .flex()：div 默认 Block，
                                            // 里面 flex_1 根节点会高度塌 0（同 Git
                                            // 改动页 diff 面板的坑）。
                                            .child(resizable_panel().child(
                                                div()
                                                    .size_full()
                                                    .flex()
                                                    .min_w_0()
                                                    .min_h_0()
                                                    .border_r_1()
                                                    .border_color(c_border)
                                                    .child(git_log_view::git_log_view(
                                                        cwd.clone(),
                                                        &self.git_log,
                                                        head_branch,
                                                        cx,
                                                    )),
                                            ))
                                            // 右：提交详情
                                            .child(
                                                resizable_panel()
                                                    .size(px(380.))
                                                    .size_range(px(240.)..px(640.))
                                                    .child(
                                                        div()
                                                            .size_full()
                                                            .flex()
                                                            .min_w_0()
                                                            .min_h_0()
                                                            .child(
                                                                git_log_view::commit_detail_pane(
                                                                    cwd,
                                                                    &self.git_log,
                                                                    cx,
                                                                ),
                                                            ),
                                                    ),
                                            ),
                                    )
                                }
                            };

                            v_flex().flex_1().min_h_0().child(sub_tabs).child(body).into_any_element()
                        }
                        MainView::Hotspot => {
                            let cwd = self.cur().and_then(|s| s.cwd(cx));
                            let data = cwd
                                .as_ref()
                                .and_then(|r| self.hotspot_data.get(r).map(|(_, d)| d.clone()));
                            hotspot_view(cwd, data, cx).into_any_element()
                        }
                        MainView::History => {
                            let cwd = self.cur().and_then(|s| s.cwd(cx));
                            let list_key = cwd.as_ref().map(|c| {
                                session_history::session_list_key(
                                    self.history_agent,
                                    self.history_profile.as_deref(),
                                    c,
                                )
                            });
                            let sessions = list_key
                                .as_ref()
                                .and_then(|k| self.session_list.get(k).map(|(_, d)| d.clone()));
                            let list_state = match sessions {
                                None => HistoryListState::Loading,
                                Some(s) if s.is_empty() => HistoryListState::Empty,
                                Some(s) => HistoryListState::Ready(s),
                            };
                            // 没选项目时给 Some(空表)，走「还没有记忆」而不是一直转圈。
                            let memories = match &cwd {
                                Some(root) => self.memory_list.get(root).map(|(_, d)| d.clone()),
                                None => Some(Rc::new(Vec::new())),
                            };
                            history_view(
                                self.history_pane,
                                self.history_agent,
                                self.history_profile.clone(),
                                cwd,
                                list_state,
                                &self.session_detail,
                                memories,
                                self.memory_selected,
                                cx,
                            )
                            .into_any_element()
                        }
        }
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.active_session;

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
        menu_order.sort_by_key(|&ix| statuses[ix].rank());
        let menu_snapshot: Vec<status_item::SessionEntry> = menu_order
            .into_iter()
            .map(|ix| {
                let color = ui_theme::agent_status_rgb8(statuses[ix]);
                let status_text = match statuses[ix] {
                    AgentStatus::WaitingApproval => "等你批准",
                    AgentStatus::NeedsAttention => "需要处理",
                    AgentStatus::Running => "运行中",
                    AgentStatus::Done => "已完成",
                    AgentStatus::Idle => "空闲",
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
            let leaves = sess.term_leaves();
            let active_pane_id = sess.anchor_id();
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
        // 状态通道：等批准 / 等输入 → gpui-component Notification
        if let Some(pending) = cx.try_global::<PendingAgentNotifs>() {
            let batch = std::mem::take(&mut *pending.0.lock().unwrap());
            for (title, message, is_approval) in batch {
                // 等批准类改由右上角阻塞 toast 展示（派生态，见 toast.rs），
                // 这里不再重复弹组件通知，避免同屏双弹。
                if is_approval {
                    continue;
                }
                window.push_notification(Notification::info(message).title(title), cx);
            }
        }

        // Git 页：后台刷新改动列表 + 分支列表（git status/for-each-ref 慢，绝不在
        // render 里同步跑）。
        if matches!(self.stage_override, Some(MainView::Git | MainView::DiffDetail))
            || (self.inspector_open && self.inspector_tab == inspector::InspectorTab::Git)
        {
            if let Some(root) = self.cur().and_then(|s| s.cwd(cx)) {
                self.ensure_git_watch(root.clone(), cx);
                self.ensure_git_status(root.clone(), cx);
                self.ensure_branches(root, cx);
            }
        }

        // 热力图页：后台刷新改动热力（git log 扫历史更慢，同样绝不同步跑）。
        if self.stage_override == Some(MainView::Hotspot) {
            if let Some(root) = self.cur().and_then(|s| s.cwd(cx)) {
                self.ensure_hotspot(root, cx);
            }
        }

        // 历史会话页：后台刷新当前项目的会话列表 / 记忆列表（看当前是哪个子页）。
        if self.stage_override == Some(MainView::History) {
            if let Some(root) = self.cur().and_then(|s| s.cwd(cx)) {
                match self.history_pane {
                    HistoryPane::Sessions => {
                        let pid = self.history_profile.clone();
                        self.ensure_session_list(self.history_agent, pid, root, cx)
                    }
                    HistoryPane::Memories => self.ensure_memory_list(root, cx),
                }
            }
        }

        // 文件树页：后台刷新根目录 + 所有已展开目录的直接子项列表（fs::read_dir 绝不
        // 在 render 里同步跑）。展开新目录时它会先落空，下一帧缓存到位后自动出现。
        if matches!(self.stage_override, Some(MainView::Files | MainView::FileDetail))
            || (self.inspector_open && self.inspector_tab == inspector::InspectorTab::Files)
        {
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
            // 多根工作区：文件树同时挂着所有项目根，后台刷新要覆盖每个根。改动文件
            // M/A/D 标要用 git status；不强制用户先去过 Git 页才有数据，Files 页自己
            // 也确保各根缓存新鲜（ensure_git_status 内部已有 TTL）。
            let roots = self.workspace_roots(cx);
            if query.is_empty() {
                // 无查询：正常树形浏览，清空上一次搜索结果。
                self.search_results = None;
                for root in &roots {
                    self.ensure_git_status(root.clone(), cx);
                    self.ensure_dir_listing(root.clone(), cx);
                }
                // 展开的子目录是绝对路径、跟属于哪个根无关，一次性全刷。
                for dir in self.expanded.clone() {
                    self.ensure_dir_listing(dir, cx);
                }
            } else if let Some(root) = self.cur().and_then(|s| s.cwd(cx)) {
                // 有查询：搜索先只在当前会话根做（跨根搜索留作后续）；顺带刷一份该根的
                // git status 给结果视图用。
                self.ensure_git_status(root.clone(), cx);
                self.ensure_search(root, query, cx);
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
        // 待处理通知总数（标题栏铃铛用）。
        let notif_count = self.collect_notifications(cx).len();
        // 标题栏分支胶囊：当前项目的分支名（repo_info 缓存，拿不到不显示）。
        let title_branch = self
            .cur()
            .and_then(|s| s.cwd(cx))
            .and_then(|c| self.repo_info.get(c.as_str()).cloned())
            .and_then(|(_, i)| i)
            .map(|i| i.branch);
        // 标题栏 agent 胶囊：跟着**当前会话**的 agent 走（多 agent 之后死盯全局
        // Claude 命令，会在 Copilot 会话上顶着「Claude Code」）；当前不是 ACP 会话
        // 就退回 Claude 那条配置，跟以前一致。
        let acp_label = match self.sessions.get(self.active_session).map(|s| &s.kind) {
            Some(SessionKind::Acp(view)) => {
                let v = view.read(cx);
                acp_pill_label(v.agent_kind(), v.launch_cmd())
            }
            _ => {
                let agent = settings::AcpAgentKind::Claude;
                acp_pill_label(agent, &settings::acp_cmd_for(agent, cx))
            }
        };
        // 当前活动会话的标题：放到标题栏右侧作为上下文提示。
        let active_title = titles
            .iter()
            .find(|(ix, _)| *ix == active)
            .map(|(_, t)| t.clone())
            .unwrap_or_default();

        // 会话列表：单列按项目上下分组（替代旧 gpui-component Sidebar 两级菜单；
        // 设计稿的「rail + 列表」左右两列实测割裂，见 session_list.rs 文件头）。
        let list_el = self.render_session_list(window, cx);
        // inspector：56px 图标条常驻 + 344px 面板（Cmd+B / 图标条切换显隐）。
        let inspector_rail_el = self.render_inspector_rail(cx);
        // 提升到舞台的那个 tab 不再停靠一份（见 inspector_panel_promoted）。
        let inspector_panel_el = (self.inspector_open && !self.inspector_panel_promoted())
            .then(|| self.render_inspector_panel(window, cx));
        // 底部状态栏 + 右上阻塞 toast。
        let status_bar_el = self.render_status_bar(cx);
        let toast_el = self.render_blocked_toasts(cx);

        // ACP 冷恢复会话「上屏即续接」：挂在这里而不是只挂 activate()——冷启动
        // 后停在哪个会话上，那个会话压根不会收到 activate 调用，只挂那边的话
        // 「重开 GUI 后当前这个 ACP 会话仍要手点重新开始」。maybe_auto_resume
        // 自带一次性闸门，每帧调无副作用。
        if let Some(SessionKind::Acp(view)) =
            self.sessions.get(self.active_session).map(|s| &s.kind)
        {
            let view = view.clone();
            view.update(cx, |v, cx| v.maybe_auto_resume(window, cx));
        }

        // 主内容（会话舞台）：舞台头 + 当前会话（分屏树 / ACP 消息流）+ 终端底条。
        // 需 .flex()，否则单 pane 的叶子 flex_1 不生效、塌缩到内容高度（边框不到底）。
        // 旧右侧「结构面板」已被 inspector + 舞台头承接，不再渲染。
        let content = if self.sessions.get(self.active_session).is_some() {
            let stage_header = self.render_stage_header(cx);
            let term_bar = self.render_terminal_status_bar(cx);
            div()
                .flex_1()
                .min_w_0()
                .min_h_0()
                .flex()
                .flex_col()
                .children(stage_header)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .min_h_0()
                        .flex()
                        .child(match &self.sessions[self.active_session].kind {
                            SessionKind::Term { .. } => self.render_pane(
                                self.sessions[self.active_session]
                                    .term_layout()
                                    .expect("Term 会话必有 layout"),
                                "pane",
                                cx,
                            ),
                            // ACP 会话：整块主区就是消息流视图（无分屏树）。
                            SessionKind::Acp(view) => view.clone().into_any_element(),
                        }),
                )
                .children(term_bar)
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
            // 应用菜单「反馈问题…」：跳 GitHub issue 模板选择页。
            .on_action(cx.listener(|_this, _: &ReportIssue, _window, cx| {
                cx.open_url("https://github.com/smelt-ai/smelt/issues/new/choose");
            }))
            // 文件内容视图右键菜单里的「发送选中内容到终端」，见 send_open_file_selection。
            .on_action(cx.listener(|this, _: &SendSelectionToTerminal, _window, cx| {
                this.send_open_file_selection(cx);
            }))
            // 全局快捷键：Cmd+K 面板 / Cmd+B 侧栏 / Cmd+[ ] 切当前会话内的 pane /
            // Cmd+1~9 跳到第 N 个会话（键位分工对齐 iTerm2）
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                let ks = &ev.keystroke;
                // 文件树键盘导航：搜索框 / 编辑器聚焦时不抢键。
                if matches!(this.stage_override, Some(MainView::Files | MainView::FileDetail))
                    && !ks.modifiers.platform && !ks.modifiers.control {
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
                // Git 页 F7 / Shift+F7：在改动块之间跳（对齐 JetBrains 的 next/previous
                // difference）。不带 Cmd，所以要赶在下面的 platform 判断之前处理。
                if matches!(this.stage_override, Some(MainView::Git | MainView::DiffDetail))
                    && this.git_tab == GitTab::Changes
                    && ks.key == "f7"
                    && !ks.modifiers.platform
                {
                    this.jump_hunk(!ks.modifiers.shift, cx);
                    return;
                }
                // Esc：收掉舞台上的全屏覆盖页，回到当前会话。弹层开着时不抢
                // （弹层各自处理关闭）。
                if ks.key == "escape"
                    && this.stage_override.is_some()
                    && this.palette.is_none()
                    && this.rename_target.is_none()
                    && !this.show_new_task_modal
                    && !this.show_quit_confirm
                    && this.delete_worktree_target.is_none()
                {
                    this.set_stage_override(None, window, cx);
                    return;
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
                    // Cmd+B：inspector 面板显隐（旧语义是左侧栏；会话列表现在常驻）。
                    "b" => {
                        this.inspector_open = !this.inspector_open;
                        cx.notify();
                    }
                    // 切当前会话内的活动 pane（分屏），不是切会话——切会话见下面的 Cmd+1~9。
                    "[" => this.cycle_pane(-1, window, cx),
                    "]" => this.cycle_pane(1, window, cx),
                    // Cmd+1~9：跳到会话列表里第 N 个会话——按列表显示顺序（各项目
                    // 分组依次铺平）数，所见即所得；超出会话数就什么都不做。
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" => {
                        let n = (ks.key.as_bytes()[0] - b'1') as usize;
                        let visible: Vec<usize> = this
                            .project_groups(cx)
                            .into_iter()
                            .flat_map(|(_, _, ixs)| ixs)
                            .collect();
                        if let Some(&ix) = visible.get(n) {
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
                    "s" if matches!(this.stage_override, Some(MainView::Files | MainView::FileDetail)) => {
                        this.save_open_file(cx)
                    }
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
                                .child(div().text_sm().text_color(c_muted).child(active_title))
                                .children(title_branch.map(|b| {
                                    // 分支胶囊：绿点 + 当前项目分支（repo_info 缓存）。
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_1p5()
                                        .px_2()
                                        .py(px(2.))
                                        .rounded(px(6.))
                                        .bg(rgb(ui_theme::bg_hover()))
                                        .border_1()
                                        .border_color(rgb(ui_theme::border_mid()))
                                        .text_xs()
                                        .font_family("monospace")
                                        .text_color(rgb(ui_theme::text_muted()))
                                        .child(
                                            div()
                                                .size(px(6.))
                                                .rounded_full()
                                                .bg(rgb(ui_theme::green())),
                                        )
                                        .child(b)
                                })),
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
                                        .relative()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .size_6()
                                        .rounded_md()
                                        .cursor_pointer()
                                        .text_color(c_muted)
                                        .hover(|s| s.bg(c_border))
                                        .child(Icon::new(IconName::Bell))
                                        // 待处理通知数：黄色角标（对齐设计稿）。
                                        .when(notif_count > 0, |d| {
                                            d.child(
                                                div()
                                                    .absolute()
                                                    .top(px(-3.))
                                                    .right(px(-3.))
                                                    .min_w(px(14.))
                                                    .h(px(14.))
                                                    .px(px(3.))
                                                    .rounded(px(7.))
                                                    .bg(rgb(ui_theme::yellow()))
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .text_size(px(9.))
                                                    .font_semibold()
                                                    .text_color(rgb(ui_theme::on_accent()))
                                                    .child(notif_count.to_string()),
                                            )
                                        })
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(|this, _, _w, cx| {
                                                cx.stop_propagation();
                                                this.notifications_open = !this.notifications_open;
                                                cx.notify();
                                            }),
                                        ),
                                )
                                .child(
                                    // agent 胶囊：紫点 + 当前 ACP agent 展示名，点击开
                                    // 设置窗「Agent 集成」页（换 agent 命令的入口）。
                                    div()
                                        .id("agent-pill")
                                        .flex()
                                        .items_center()
                                        .gap_1p5()
                                        .px_2()
                                        .py(px(3.))
                                        .rounded(px(6.))
                                        .bg(rgb(ui_theme::bg_hover()))
                                        .border_1()
                                        .border_color(rgb(ui_theme::border_mid()))
                                        .cursor_pointer()
                                        .hover(|s| s.border_color(rgb(ui_theme::border_focus())))
                                        .text_xs()
                                        .font_family("monospace")
                                        .text_color(rgb(ui_theme::text_mid()))
                                        .child(
                                            div()
                                                .size(px(6.))
                                                .rounded_full()
                                                .bg(rgb(ui_theme::purple())),
                                        )
                                        .child(acp_label.clone())
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(|this, _, window, cx| {
                                                cx.stop_propagation();
                                                if this.llm_inputs.is_none() {
                                                    this.init_llm_inputs(window, cx);
                                                }
                                                this.settings_page_ix = 3; // Agent 集成页
                                                this.settings_page_nonce += 1;
                                                this.open_settings_window(cx);
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
                div()
                    .size_full()
                    .flex()
                    // 会话列表（280px，按项目分组）
                    .child(list_el)
                    // 主区：顶部视图切换 + 内容
                    .child(
                        div()
                            .flex_1()
                            // min_w_0：主区在根 flex 行里默认 min-width:auto，会被最长终端行
                            // 撑到不肯收缩，导致宽度被内容反向放大。归零后才能正常按剩余空间收缩。
                            .min_w_0()
                    .flex()
                    .flex_col()
                    // 舞台：覆盖页（旧全屏页）优先，否则显示当前会话。
                    // 覆盖页顶部带显式返回条——Esc 之外总得有个能点的出口。
                    .child(match self.stage_override {
                        Some(v) => v_flex()
                            .flex_1()
                            .min_h_0()
                            .child(self.render_stage_back_bar(v, cx))
                            .child(self.render_stage_override(v, window, cx))
                            .into_any_element(),
                        None => content.into_any_element(),
                    }),
                    )
                    .children(inspector_panel_el)
                    .child(inspector_rail_el),
                ),
            )
            // 底部状态栏
            .child(status_bar_el)
            // 右上阻塞 toast（浮层，命令面板之下）
            .children(toast_el)
            // 命令面板（最上层）
            .children(palette_overlay)
            // 退出确认拦截弹层
            .children(self.show_quit_confirm.then(|| self.render_quit_confirm(cx)))
            // 会话管理弹窗（设置页「更新」tab 点开）
            .children(self.session_manager_open.then(|| self.render_session_manager(cx)))
            // 会话重命名拦截弹层
            .children(self.rename_target.is_some().then(|| self.render_rename_session(cx)))
            // 新建任务弹窗
            .children(self.show_new_task_modal.then(|| self.render_new_task_modal(cx)))
            // 删除 Worktree 确认拦截弹层
            .children(self.delete_worktree_target.is_some().then(|| self.render_delete_worktree_confirm(cx)))
            .children(self.discard_hunk_target.is_some().then(|| self.render_discard_hunk_confirm(cx)))
            .children(self.discard_file_target.is_some().then(|| self.render_discard_file_confirm(cx)))
            .children(self.discard_all_target.is_some().then(|| self.render_discard_all_confirm(cx)))
            .children(self.delete_branch_target.is_some().then(|| self.render_delete_branch_confirm(cx)))
            // 删除文件二次确认拦截弹层
            .children(self.delete_file_target.is_some().then(|| self.render_delete_file_confirm(cx)))
            // 重启守护确认弹层改挂在设置窗（SettingsWindow::render），不在主窗口画。
            // Finder 拖文件/文件夹：只在有拖拽时叠全窗 drop 层。
            // 常驻 hitbox 会盖住按钮（「新建终端」像没反应）；对齐「有 drag 才出现」。
            // 终端 hitbox 会挡住根 on_drop，所以必须用上层目标接 ExternalPaths。
            .when(cx.has_active_drag(), |root| {
                root.child(
                    div()
                        .id("file-drop-overlay")
                        .absolute()
                        .inset_0()
                        .bg(ui_theme::tint(ui_theme::blue(), 0x28))
                        .border_2()
                        .border_color(rgb(ui_theme::blue()))
                        .on_drop::<ExternalPaths>(cx.listener(
                            |this, ep: &ExternalPaths, _window, cx| {
                                this.open_paths(ep.paths(), cx);
                            },
                        )),
                )
            })
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
                    rgb(ui_theme::green())
                } else if fps >= 30.0 {
                    rgb(ui_theme::yellow())
                } else {
                    rgb(ui_theme::red())
                };
                div()
                    .absolute()
                    .top(px(40.))
                    .right(px(12.))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(ui_theme::tint(ui_theme::bg_card(), 0xcc))
                    .border_1()
                    .border_color(ui_theme::overlay(0x22))
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



/// ACP 启动命令 → 标题栏胶囊的展示名（`bunx @scope/claude-agent-acp@0.59.0` →
/// `claude-agent-acp`，`copilot --acp` → `copilot`）。与 acp_view 输入栏胶囊
/// 同一套抠名逻辑；没有模型名数据源，不硬编。
fn acp_agent_label(cmd: &str) -> String {
    let tok = cmd.split_whitespace().rev().find(|t| !t.starts_with('-')).unwrap_or("agent");
    let name = tok.rsplit('/').next().unwrap_or(tok);
    name.split('@').find(|s| !s.is_empty()).unwrap_or(name).to_string()
}

/// 标题栏 agent 胶囊的文字：命令还是出厂值就显示人话名（`Claude Code`），用户
/// 自定义过就显示命令里抠出的包名——自定义了还写「Claude Code」等于撒谎。
fn acp_pill_label(agent: settings::AcpAgentKind, cmd: &str) -> String {
    if cmd.trim() == agent.default_cmd() {
        agent.label().to_string()
    } else {
        acp_agent_label(cmd)
    }
}

/// 旧存档没记 agent 种类时，从启动命令反推一把（命令里出现过 copilot / codex
/// 字样就归给它们）；认不出当 Claude——多 agent 之前的存档只可能是它。
fn acp_agent_from_cmd(cmd: &str) -> settings::AcpAgentKind {
    let c = cmd.to_ascii_lowercase();
    if c.contains("copilot") {
        settings::AcpAgentKind::Copilot
    } else if c.contains("codex") {
        settings::AcpAgentKind::Codex
    } else if c.contains("grok") {
        settings::AcpAgentKind::Grok
    } else {
        settings::AcpAgentKind::Claude
    }
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
        window.set_rem_size(px(19.));
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
            MenuItem::action("反馈问题…", ReportIssue),
            MenuItem::Separator,
            MenuItem::action("退出 Smelt", Quit),
        ])]);

        // 外观设置：读盘设为全局单例，据此确定窗口背景外观（透明 / 模糊）。
        // 主题模式在建窗口之前落地，首帧就是对的，不会先闪一下深色再变浅。
        let appearance = load_appearance();
        let window_bg = appearance.window_bg();
        settings::apply_theme_mode(appearance.theme_mode, cx);
        terminal_view::set_font_px(appearance.font_px);
        terminal_view::set_font_family(&appearance.font_family);
        cx.set_global(appearance);
        cx.set_global(load_launch_config());

        // 桌面宠物：配置 + 播报邮箱 + LLM 大脑配置（跨窗口全局单例），再开独立透明浮窗。
        cx.set_global(pet::load_pet_config());
        cx.set_global(pet::PetMailbox::default());
        cx.set_global(agent::load_llm_config());
        pet::open_pet_window(cx);

        // 状态通道：常驻订阅守护的 subscribe，维护 DaemonStates 全局单例，
        // Session::status/pane_status 靠它把"猜"换成"读事实"（见
        // docs/state-channel-plan.md）。阻塞的 socket 读循环放专门的 OS 线程，
        // 断线/守护没起来就等一下重连；smol::channel 两头都能用（OS 线程用
        // try_send，GPUI 任务用 async recv），跟 terminal.rs 的 redraw_tx/rx
        // 是同一个搭桥模式。
        let daemon_states = DaemonStates::default();
        cx.set_global(daemon_states.clone());
        cx.set_global(PendingAgentNotifs::default());
        cx.set_global(settings::load_agent_ui_config());
        let (daemon_state_tx, daemon_state_rx) =
            smol::channel::unbounded::<terminal::DaemonStateEvent>();
        thread::spawn(move || loop {
            terminal::subscribe_daemon_states_blocking(&daemon_state_tx);
            thread::sleep(Duration::from_secs(2)); // 断线/连不上，等一下重试
        });
        cx.spawn(async move |cx| {
            while let Ok(event) = daemon_state_rx.recv().await {
                let _ = cx.update(|cx| {
                    let states = cx.global::<DaemonStates>().0.clone();
                    let notify_on = cx
                        .try_global::<settings::AgentUiConfig>()
                        .map(|c| c.notify_awaiting)
                        .unwrap_or(true);
                    let pending = cx
                        .try_global::<PendingAgentNotifs>()
                        .map(|p| p.0.clone());
                    {
                        let mut map = states.lock().unwrap();
                        match event {
                            terminal::DaemonStateEvent::Snapshot(list) => {
                                // 只清守护侧条目：`acp-` 前缀是 GUI 内 ACP 会话自己
                                // 维护的状态，smeltd 重连发快照时不能把它们抹掉。
                                map.retain(|k, _| k.starts_with("acp-"));
                                for s in list {
                                    map.insert(s.id.clone(), s);
                                }
                            }
                            terminal::DaemonStateEvent::Update(s) => {
                                let prev = map.get(&s.id).map(|p| p.phase);
                                let entered_await = matches!(
                                    s.phase,
                                    terminal::DaemonPhase::AwaitingApproval
                                        | terminal::DaemonPhase::WaitingForUser
                                ) && prev != Some(s.phase);
                                if notify_on && entered_await {
                                    if let Some(q) = pending {
                                        let title = s.phase_label().to_string();
                                        let msg = s
                                            .detail_line()
                                            .or_else(|| s.title.clone())
                                            .unwrap_or_else(|| {
                                                format!("会话 {}", &s.id[..8.min(s.id.len())])
                                            });
                                        let is_appr = s.phase
                                            == terminal::DaemonPhase::AwaitingApproval;
                                        q.lock().unwrap().push((title, msg, is_appr));
                                    }
                                }
                                map.insert(s.id.clone(), s);
                            }
                        }
                    }
                    cx.refresh_windows(); // 状态点跟着这次变化重绘
                });
            }
        })
        .detach();

        // 远程操作网关：只记「用户上次希望它开着」这个开关；真去问/让守护开的部分
        // 扔进后台任务——涉及连 unix socket、可能要等守护自己起来（最坏几秒），
        // 不能卡首帧渲染。settings.rs 的「远程」设置页读 RemoteRuntimeState 展示。
        //
        // 网关和隧道**串在同一条后台任务**里对齐：先问守护现状（幂等 hydrate），
        // 没有再 start。以前两条 spawn 并行时，隧道可能先回 URL、token 还是空的，
        // UI 会拼出 `?token=` 的死链。
        let remote_config = settings::load_remote_config();
        let want_remote = remote_config.enabled;
        // 隧道依赖本机网关；配置里 tunnel_enabled=true 时 enabled 理应也是 true
        // （apply_tunnel_toggle 存盘时就是这么同步的），但独立判断一次更保险。
        let want_tunnel = remote_config.tunnel_enabled;
        let want_webrtc = remote_config.webrtc_enabled;
        let want_write = remote_config.write_enabled;
        cx.set_global(remote_config);
        cx.set_global(settings::RemoteRuntimeState::default());
        cx.set_global(settings::TunnelRuntimeState::default());
        cx.set_global(settings::WebrtcRuntimeState::default());
        cx.set_global(settings::SignalProbeState::default());
        // Cmd+Q 直接退出 App（没有先手动关跨网开关）时，WebRTC bridge 子进程原本
        // 没人管——会被系统收养成孤儿，一直占着信令服务器上的房间和本机网关连接
        // 直到房间 TTL 到期。退出前顺手杀掉它。
        //
        // ACP 会话（Copilot/Claude/Codex/Grok 的 CLI 子进程）原本完全没管：
        // Cmd+Q 直接终止整个 GUI 进程时，每条 ACP 连接线程会被系统一并带走，
        // 没机会跑到自己的收尾逻辑（agent_client_protocol 内部靠 Drop 杀子进程），
        // 子进程就变孤儿——真实症状：孤儿还占着旧登录会话，下次「重新开始」
        // 新起一个进程去认证会撞上它，报出看着不相关的 Authentication required。
        // 这里给每条活跃 ACP 连接发 Shutdown，再异步等线程真正收尾（超时兜底，
        // 别让某个不听话的 agent 卡住整个 App 退出）。
        let current_ws_for_quit = current_ws.clone();
        cx.on_app_quit(move |cx| {
            settings::stop_webrtc_bridge_on_quit(cx);
            let handles: Vec<smelt_core::acp_conn::AcpHandle> = current_ws_for_quit
                .borrow()
                .as_ref()
                .and_then(|w| w.upgrade())
                .map(|ws| {
                    ws.update(cx, |ws, cx| {
                        ws.sessions
                            .iter()
                            .filter_map(|s| match &s.kind {
                                SessionKind::Acp(view) => {
                                    view.update(cx, |v, _cx| v.take_handle_for_quit())
                                }
                                _ => None,
                            })
                            .collect()
                    })
                })
                .unwrap_or_default();
            async move {
                for h in handles {
                    smelt_core::acp_conn::wait_for_shutdown(h, Duration::from_secs(3)).await;
                }
            }
        })
        .detach();
        // 恢复跨网：隧道仍走下面 spawn；WebRTC 在网关 hydrate 后再拉 bridge
        if want_remote || want_tunnel || want_webrtc {
            if want_tunnel {
                cx.set_global(settings::TunnelRuntimeState {
                    connecting: true,
                    url: None,
                    error: None,
                    write: false,
                });
            }
            cx.spawn(async move |cx| {
                let (remote_rt, tunnel_rt, want_webrtc) = cx
                    .background_executor()
                    .spawn(async move {
                        terminal::ensure_daemon_running();

                        // 1) 本机网关：已在跑就复用 token，否则按配置 start
                        // WebRTC 也需要本机网关 token
                        let remote_rt = if want_remote || want_tunnel || want_webrtc {
                            let existing = terminal::remote_status();
                            if existing.running
                                && existing.token.as_ref().is_some_and(|t| !t.is_empty())
                            {
                                settings::RemoteRuntimeState {
                                    token: existing.token,
                                    addr: existing.addr,
                                    write: existing.write,
                                    error: None,
                                }
                            } else {
                                match terminal::remote_start("127.0.0.1", want_write) {
                                    Ok(s) => settings::RemoteRuntimeState {
                                        token: s.token,
                                        addr: s.addr,
                                        write: s.write,
                                        error: None,
                                    },
                                    Err(e) => settings::RemoteRuntimeState {
                                        token: None,
                                        addr: None,
                                        write: false,
                                        error: Some(e),
                                    },
                                }
                            }
                        } else {
                            settings::RemoteRuntimeState::default()
                        };

                        // 2) 隧道：同样先 status 再 start；最终以「有 token 才能展示 URL」为准
                        let tunnel_rt = if want_tunnel {
                            let has_token =
                                remote_rt.token.as_ref().is_some_and(|t| !t.is_empty());
                            if !has_token {
                                settings::TunnelRuntimeState {
                                    connecting: false,
                                    url: None,
                                    error: Some(
                                        "本机远程网关没起来，无法建立隧道".into(),
                                    ),
                                    write: false,
                                }
                            } else {
                                let existing = terminal::tunnel_status();
                                if existing.running && existing.url.is_some() {
                                    settings::TunnelRuntimeState {
                                        connecting: false,
                                        url: existing.url,
                                        error: None,
                                        write: existing.write,
                                    }
                                } else {
                                    match terminal::tunnel_start(want_write) {
                                        Ok(s) => settings::TunnelRuntimeState {
                                            connecting: false,
                                            url: s.url,
                                            error: None,
                                            write: s.write,
                                        },
                                        Err(e) => settings::TunnelRuntimeState {
                                            connecting: false,
                                            url: None,
                                            error: Some(e),
                                            write: false,
                                        },
                                    }
                                }
                            }
                        } else {
                            settings::TunnelRuntimeState::default()
                        };

                        // 隧道 start 可能顺带（重）开了网关：再读一次 token，避免 UI 仍空
                        let remote_rt = if want_remote || want_tunnel || want_webrtc {
                            let again = terminal::remote_status();
                            if again.running && again.token.as_ref().is_some_and(|t| !t.is_empty())
                            {
                                settings::RemoteRuntimeState {
                                    token: again.token,
                                    addr: again.addr,
                                    write: again.write,
                                    error: None,
                                }
                            } else {
                                remote_rt
                            }
                        } else {
                            remote_rt
                        };

                        // token 仍空时，即使隧道有 URL 也不给 UI 展示（防 `?token=`）
                        let tunnel_rt = if tunnel_rt.url.is_some()
                            && !remote_rt.token.as_ref().is_some_and(|t| !t.is_empty())
                        {
                            settings::TunnelRuntimeState {
                                connecting: false,
                                url: None,
                                error: Some(
                                    "隧道在跑但拿不到网关 token，请在设置里重开远程访问".into(),
                                ),
                                write: false,
                            }
                        } else {
                            tunnel_rt
                        };

                        (remote_rt, tunnel_rt, want_webrtc)
                    })
                    .await;
                let _ = cx.update(|cx| {
                    cx.set_global(remote_rt);
                    cx.set_global(tunnel_rt);
                    // 网关 token 就绪后再恢复 WebRTC bridge（否则建房有 token 却无本机网关）
                    if want_webrtc {
                        settings::spawn_webrtc_start_public(cx);
                    }
                });
            })
            .detach();
        }
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
                                    if ws.stage_override == Some(MainView::Overview) {
                                        ws.stage_override = None;
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
            launch_cmd: Some("claude --dangerously-skip-permissions".into()),
        };
        let json = serde_json::to_string(&leaf).unwrap();
        let back: PaneState = serde_json::from_str(&json).unwrap();
        match back {
            PaneState::Leaf {
                custom_title,
                launch_label,
                launch_cmd,
                id,
                cwd,
            } => {
                assert_eq!(custom_title.as_deref(), Some("跑测试的终端"));
                assert_eq!(launch_label.as_deref(), Some("Claude Code"));
                assert_eq!(
                    launch_cmd.as_deref(),
                    Some("claude --dangerously-skip-permissions")
                );
                assert_eq!(id.as_deref(), Some("sid-1"));
                assert_eq!(cwd.as_deref(), Some("/tmp/x"));
            }
            _ => panic!("应当反序列化成 Leaf"),
        }
    }

    /// 旧存档没有 custom_title / launch_label / launch_cmd 字段，必须读成 None 而不是解析失败。
    #[test]
    fn old_archive_without_custom_title_still_loads() {
        let old = r#"{"Leaf":{"cwd":"/tmp/x","id":"sid-1"}}"#;
        let back: PaneState = serde_json::from_str(old).unwrap();
        match back {
            PaneState::Leaf {
                custom_title,
                launch_label,
                launch_cmd,
                id,
                ..
            } => {
                assert!(custom_title.is_none(), "旧存档不该凭空冒出自定义名");
                assert!(launch_label.is_none(), "旧存档不该凭空冒出启动项名");
                assert!(launch_cmd.is_none(), "旧存档不该凭空冒出启动命令");
                assert_eq!(id.as_deref(), Some("sid-1"));
            }
            _ => panic!("应当反序列化成 Leaf"),
        }
    }
}

#[cfg(test)]
mod acp_agent_tests {
    use super::{acp_agent_from_cmd, acp_pill_label, AcpSaved};
    use crate::settings::AcpAgentKind;

    /// 多 agent 之前的 ACP 存档没有 `agent` 字段：必须读得进来（None），
    /// 不能整条会话解析失败——那等于用户重开 GUI 少一个会话。
    #[test]
    fn old_acp_archive_without_agent_field_still_loads() {
        let old = r#"{"cwd":"/tmp/x","cmd":"bunx --bun @agentclientprotocol/claude-agent-acp@0.59.0"}"#;
        let back: AcpSaved = serde_json::from_str(old).unwrap();
        assert!(back.agent.is_none(), "旧存档不该凭空冒出 agent 字段");
        assert_eq!(acp_agent_from_cmd(&back.cmd), AcpAgentKind::Claude);
    }

    /// 旧存档反推：命令里带 copilot / codex 字样的归给对应 agent，其余当 Claude。
    #[test]
    fn agent_inferred_from_legacy_cmd() {
        assert_eq!(acp_agent_from_cmd("copilot --acp"), AcpAgentKind::Copilot);
        assert_eq!(
            acp_agent_from_cmd("bunx --bun @zed-industries/codex-acp"),
            AcpAgentKind::Codex
        );
        assert_eq!(acp_agent_from_cmd("some-other-agent"), AcpAgentKind::Claude);
    }

    /// 存档标识必须往返得回来（改了 id 就等于把用户的会话认成别家 agent）。
    #[test]
    fn agent_id_roundtrips() {
        for a in AcpAgentKind::ALL {
            assert_eq!(AcpAgentKind::from_id(a.id()), Some(a));
        }
        assert_eq!(AcpAgentKind::from_id("gemini"), None);
    }

    /// 胶囊文案：出厂命令显示人话名，自定义过就显示命令里抠出的包名。
    #[test]
    fn pill_label_tells_truth_about_custom_cmd() {
        let claude = AcpAgentKind::Claude;
        assert_eq!(acp_pill_label(claude, &claude.default_cmd()), "Claude Code");
        assert_eq!(acp_pill_label(claude, "bunx my-own-acp@1.2.3"), "my-own-acp");
    }
}
