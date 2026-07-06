//! smelt 工作台 —— 基于 gpui-component 的桌面窗口。
//!
//! Workspace 管理多个终端标签（TerminalView）：顶部标签栏切换 / 新建 / 关闭，
//! 下方渲染当前活动终端。每个终端各自独立（PTY、IME、滚动、resize）。
//!
//! 运行： cargo run --bin workspace

mod terminal;
mod terminal_view;

use std::collections::HashSet;
use std::path::Path;
use std::rc::Rc;
use std::sync::OnceLock;

use gpui::*;
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::sidebar::{
    Sidebar, SidebarCollapsible, SidebarGroup, SidebarMenu, SidebarMenuItem,
};
use gpui_component::list::{List, ListDelegate, ListEvent, ListItem, ListState};
use gpui_component::resizable::{
    h_resizable, resizable_panel, v_resizable, ResizablePanelEvent, ResizableState,
};
use gpui_component::scroll::ScrollableElement;
use gpui_component::tab::{Tab, TabBar};
use gpui_component::tag::Tag;
use gpui_component::tooltip::Tooltip;
use gpui_component::*;
use terminal_view::TerminalView;

// Cmd+Q 退出的应用级 action（gpui 无默认菜单栏，需自建菜单栏 + 键位绑定）。
gpui::actions!(smelt, [Quit]);

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

/// 主区视图：终端 / 文件树 / Git（按项目切换）。
#[derive(Clone, Copy, PartialEq)]
enum MainView {
    Terminal,
    Files,
    Git,
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

    /// 会话标题：取活动终端标题（cwd 末段）。
    fn title(&self, cx: &App) -> String {
        self.active.read(cx).title().to_string()
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

/// 打开查看的文件：路径 + 预高亮的行。每行是若干 (颜色, 文本) 片段。
/// 高亮在打开时一次算好并存起来，滚动时 uniform_list 只按可见范围取行渲染。
/// 用 Rc 让 uniform_list 的 'static 闭包能廉价地共享这份数据。
struct OpenFile {
    path: String,
    lines: Rc<Vec<Vec<(Rgba, String)>>>,
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

/// 可序列化的分屏布局镜像：叶子存该终端 cwd，Split 存方向 + 子节点。
/// 拖动比例暂不持久化，重开按均分；结构 / 嵌套 / 方向完整恢复。
#[derive(serde::Serialize, serde::Deserialize)]
enum PaneState {
    Leaf { cwd: Option<String> },
    Split { axis: SplitAxis, children: Vec<PaneState> },
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
        Pane::Leaf(t) => PaneState::Leaf { cwd: t.read(cx).cwd() },
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
        PaneState::Leaf { cwd } => {
            let v = cx.new(|cx| TerminalView::new(cx, cwd.clone()));
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
}

impl Default for Appearance {
    fn default() -> Self {
        Self { bg_color: 0x1a1b26, bg_image: None, opacity: 1.0, blur: false }
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
    /// 当前在文件树里打开查看的文件（含预高亮的行数据）。
    open_file: Option<OpenFile>,
    /// 打开文件的自增序号：后台高亮完成时用它判断结果是否已过期（切了别的文件）。
    file_gen: u64,
    /// Git 视图里当前查看的文件 diff；None 表示未选中任何文件。
    git_diff: Option<GitDiff>,
    /// 打开 diff 的自增序号（独立于 file_gen，避免和文件高亮任务互相取消）。
    diff_gen: u64,
    /// diff 是否用并排（split）视图；false 为统一（unified）视图。
    diff_split: bool,
    /// 左侧会话侧栏是否展开（Cmd+B 切换）。
    sidebar_open: bool,
    /// 外观设置面板是否打开（标题栏齿轮切换）。
    settings_open: bool,
    /// 命令面板（Cmd+K）；None 表示未打开。搜索/导航/确认由 ListState 负责。
    palette: Option<Entity<ListState<CmdDelegate>>>,
    /// 命令面板的事件订阅（确认/取消）；随面板关闭一并释放。
    _palette_sub: Option<Subscription>,
    /// 各滚动区的常驻滚动句柄——供 gpui-component Scrollbar 读取位置并绘制。
    /// 必须常驻（每帧新建会丢失滚动位置）。
    git_files_scroll: ScrollHandle,
    diff_scroll: UniformListScrollHandle,
    file_scroll: UniformListScrollHandle,
    /// 根布局左右分栏（会话侧栏 ↔ 主区）的可拖拽状态；常驻以保住拖出的宽度。
    root_resize: Entity<ResizableState>,
    /// 侧栏初始宽度（px）：启动时从存档恢复，作为 resizable_panel 的初始 size。
    sidebar_w: f32,
    /// 侧栏 resize 事件订阅（拖动完写回存档）；随视图存活。
    _resize_sub: Subscription,
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
                    let v = cx.new(|cx| TerminalView::new(cx, cwd));
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

        Self {
            sessions,
            active_session,
            view: MainView::Terminal,
            expanded: HashSet::new(),
            open_file: None,
            file_gen: 0,
            git_diff: None,
            diff_gen: 0,
            diff_split: false,
            sidebar_open: true,
            settings_open: false,
            palette: None,
            _palette_sub: None,
            git_files_scroll: ScrollHandle::new(),
            diff_scroll: UniformListScrollHandle::new(),
            file_scroll: UniformListScrollHandle::new(),
            root_resize,
            sidebar_w,
            _resize_sub,
        }
    }

    /// 当前活动会话（不可变引用）。
    fn cur(&self) -> Option<&Session> {
        self.sessions.get(self.active_session)
    }

    /// 「+」/新建：开一个独立新会话（单终端），并切过去。
    fn add_session(&mut self, cwd: Option<String>, cx: &mut Context<Self>) {
        let view = cx.new(|cx| TerminalView::new(cx, cwd));
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
        let view = cx.new(|cx| TerminalView::new(cx, cwd));
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

    /// 关闭第 ix 个会话（至少保留一个）。
    fn close_session(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.sessions.len() <= 1 || ix >= self.sessions.len() {
            return;
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

    /// 修改外观设置：改全局 + 存盘 + 同步窗口背景（透明/模糊）+ 触发重绘。
    fn update_appearance(
        &mut self,
        window: &mut Window,
        f: impl FnOnce(&mut Appearance),
        cx: &mut Context<Self>,
    ) {
        let mut ap = cx.global::<Appearance>().clone();
        f(&mut ap);
        save_appearance(&ap);
        let win_bg = ap.window_bg();
        cx.set_global(ap);
        window.set_background_appearance(win_bg);
        cx.notify();
    }

    /// 设置 / 清除背景图（不影响窗口透明度，故无需 window）。
    fn set_bg_image(&mut self, path: Option<String>, cx: &mut Context<Self>) {
        let mut ap = cx.global::<Appearance>().clone();
        ap.bg_image = path;
        save_appearance(&ap);
        cx.set_global(ap);
        cx.notify();
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

    /// 渲染外观设置浮层（标题栏齿轮打开）：背景色 / 背景图 / 不透明度 / 模糊。
    fn render_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let (fg, muted, border, popover, ring) = {
            let t = cx.theme();
            (t.foreground, t.muted_foreground, t.border, t.popover, t.ring)
        };
        let ap = cx.global::<Appearance>().clone();

        // 预设背景色：名称仅作区分，值为 0xRRGGBB。
        let presets: [u32; 6] =
            [0x1a1b26, 0x000000, 0x1e1e1e, 0x0d1117, 0x1c1917, 0x0f1a17];
        let swatches: Vec<_> = presets
            .iter()
            .map(|&color| {
                let sel = ap.bg_color == color;
                div()
                    .id(("bg-swatch", color as usize))
                    .size_6()
                    .rounded_md()
                    .cursor_pointer()
                    .bg(rgb(color))
                    .border_2()
                    .border_color(if sel { ring } else { border })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            this.update_appearance(window, move |a| a.bg_color = color, cx)
                        }),
                    )
            })
            .collect();

        // 不透明度档位。
        let opacity_row: Vec<_> = [100u32, 90, 80, 70, 60]
            .iter()
            .map(|&pct| {
                let val = pct as f32 / 100.0;
                let sel = (ap.opacity - val).abs() < 0.005;
                div()
                    .id(("op", pct as usize))
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .text_xs()
                    .text_color(if sel { fg } else { muted })
                    .bg(if sel { border } else { popover })
                    .hover(|s| s.bg(border))
                    .child(format!("{pct}%"))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            this.update_appearance(window, move |a| a.opacity = val, cx)
                        }),
                    )
            })
            .collect();

        let blur_on = ap.blur;
        let blur_chip = div()
            .id("blur")
            .px_2()
            .py_1()
            .rounded_md()
            .cursor_pointer()
            .text_xs()
            .text_color(if blur_on { fg } else { muted })
            .bg(if blur_on { border } else { popover })
            .hover(|s| s.bg(border))
            .child(if blur_on { "毛玻璃 · 开" } else { "毛玻璃 · 关" }.to_string())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.update_appearance(window, |a| a.blur = !a.blur, cx)
                }),
            );

        let pick_btn = div()
            .id("pick-img")
            .px_2()
            .py_1()
            .rounded_md()
            .cursor_pointer()
            .text_xs()
            .text_color(fg)
            .bg(popover)
            .hover(|s| s.bg(border))
            .child("选择图片…".to_string())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _w, cx| this.pick_bg_image(cx)),
            );
        let clear_btn = div()
            .id("clear-img")
            .px_2()
            .py_1()
            .rounded_md()
            .cursor_pointer()
            .text_xs()
            .text_color(muted)
            .bg(popover)
            .hover(|s| s.bg(border))
            .child("清除".to_string())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _w, cx| this.set_bg_image(None, cx)),
            );
        let img_name = ap
            .bg_image
            .as_deref()
            .and_then(|p| p.rsplit('/').next())
            .unwrap_or("无")
            .to_string();

        let section = |title: &str| div().text_xs().text_color(muted).child(title.to_string());

        // 点背景空白关闭；面板停在右上（齿轮下方）。
        div()
            .absolute()
            .inset_0()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _w, cx| {
                    this.settings_open = false;
                    cx.notify();
                }),
            )
            .child(
                div()
                    .absolute()
                    .top(px(40.))
                    .right(px(8.))
                    .w(px(280.))
                    .bg(popover)
                    .border_1()
                    .border_color(border)
                    .rounded_lg()
                    .shadow_lg()
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_3()
                    // 点面板内部不冒泡到背景，避免误关。
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(div().font_bold().text_color(fg).child("外观"))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(section("背景色"))
                            .child(div().flex().gap_2().flex_wrap().children(swatches)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(section("背景图片"))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(pick_btn)
                                    .child(clear_btn)
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .text_xs()
                                            .text_color(muted)
                                            .child(img_name),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(section("不透明度"))
                            .child(div().flex().gap_1().children(opacity_row)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(section("背景模糊"))
                            .child(blur_chip),
                    ),
            )
            .into_any_element()
    }

    /// 文件树：展开/收起一个文件夹。
    fn toggle_expand(&mut self, path: String, cx: &mut Context<Self>) {
        if !self.expanded.remove(&path) {
            self.expanded.insert(path);
        }
        cx.notify();
    }

    /// 文件树：打开一个文件查看内容。读文本 + 语法高亮放到后台线程跑（大文件不卡 UI），
    /// 算完回主线程写入。用自增 file_gen 丢弃过期结果（期间又切了别的文件）。
    fn view_file(&mut self, path: String, cx: &mut Context<Self>) {
        self.file_gen = self.file_gen.wrapping_add(1);
        let gen = self.file_gen;
        // 先占位（清空旧内容 + 显示文件名），高亮完成后替换。
        self.open_file = Some(OpenFile { path: path.clone(), lines: Rc::new(Vec::new()) });
        cx.notify();

        cx.spawn(async move |this, cx| {
            let p = path.clone();
            let lines = cx
                .background_executor()
                .spawn(async move {
                    match std::fs::read_to_string(&p) {
                        Ok(text) => highlight_all(&p, &text),
                        Err(_) => vec![vec![(
                            rgb(0x808080),
                            "（无法以文本方式读取：可能是二进制文件）".to_string(),
                        )]],
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                // 只有当前仍是这次打开的文件才写入，避免旧任务覆盖新文件。
                if this.file_gen == gen {
                    this.open_file = Some(OpenFile { path, lines: Rc::new(lines) });
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Git 视图：查看某个改动文件的 diff。已跟踪文件用 `git diff HEAD`，
    /// 未跟踪文件（??）用 `git diff --no-index` 展示全文（整体当作新增）。
    /// 跑 git + 着色放后台，用 file_gen 丢弃过期结果。
    fn open_diff(&mut self, root: String, path: String, untracked: bool, cx: &mut Context<Self>) {
        self.diff_gen = self.diff_gen.wrapping_add(1);
        let gen = self.diff_gen;
        self.git_diff = Some(GitDiff { path: path.clone(), lines: Rc::new(Vec::new()) });
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
                // 叠加层（absolute，不占布局、不挡点击）区分活动 / 非活动：
                // 活动 pane 用 ring 色描一圈；非活动 pane 整块压暗拉开对比。
                let overlay = if active {
                    div()
                        .absolute()
                        .inset_0()
                        .border_2()
                        .border_color(cx.theme().ring)
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.active_session;
        let can_close = self.sessions.len() > 1;

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
        // 当前活动会话的标题：放到标题栏右侧作为上下文提示。
        let active_title = titles
            .iter()
            .find(|(ix, _)| *ix == active)
            .map(|(_, t)| t.clone())
            .unwrap_or_default();

        // 左侧会话侧栏：按会话的 cwd 分组成项目（保持出现顺序）
        let mut projects: Vec<(String, Vec<usize>)> = Vec::new();
        for (ix, _title) in titles.iter() {
            let cwd = self.sessions[*ix].cwd(cx).unwrap_or_default();
            let name = cwd
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .filter(|s| !s.is_empty())
                .unwrap_or("项目")
                .to_string();
            match projects.iter_mut().find(|(n, _)| *n == name) {
                Some(p) => p.1.push(*ix),
                None => projects.push((name, vec![*ix])),
            }
        }

        // 项目 → 会话 两级菜单（gpui-component Sidebar）。
        // Sidebar 组件的回调是 Fn(&_, &mut Window, &mut App)，拿不到 Context<Self>，
        // 故捕获 entity 句柄在闭包里 update 自身。
        let this = cx.entity();
        let menu_items: Vec<SidebarMenuItem> = projects
            .iter()
            .map(|(name, ixs)| {
                let sess_items: Vec<SidebarMenuItem> = ixs
                    .iter()
                    .map(|&ix| {
                        let title = titles.get(ix).map(|(_, t)| t.clone()).unwrap_or_default();
                        let e_act = this.clone();
                        let mut item = SidebarMenuItem::new(title)
                            .icon(IconName::SquareTerminal)
                            .active(ix == active)
                            .on_click(move |_ev, window, cx| {
                                e_act.update(cx, |ws, cx| ws.activate(ix, window, cx));
                            });
                        if can_close {
                            let e_close = this.clone();
                            item = item.suffix(move |_w, _cx| {
                                let e = e_close.clone();
                                Button::new(("close-session", ix))
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::CircleX)
                                    .on_click(move |_ev, _w, cx| {
                                        // 别把点击冒泡成「切换到该会话」
                                        cx.stop_propagation();
                                        e.update(cx, |ws, cx| ws.close_session(ix, cx));
                                    })
                            });
                        }
                        item
                    })
                    .collect();
                SidebarMenuItem::new(name.clone())
                    .icon(IconName::Folder)
                    .default_open(true)
                    .click_to_toggle(true)
                    .children(sess_items)
            })
            .collect();

        let sidebar_el = Sidebar::new("workspace-sidebar")
            .collapsible(SidebarCollapsible::Offcanvas)
            // 宽度交给外层 resizable_panel 控制（可拖），这里填满 panel。
            // 品牌已移到顶部标题栏，侧栏直接从「会话」开始，避免重复。
            .w(relative(1.))
            .child(SidebarGroup::new("会话").child(SidebarMenu::new().children(menu_items)))
            // 不用 SidebarFooter：它会给整块 footer 挂 hover 背景（sidebar_accent），
            // 盖住按钮自己的 hover。直接放普通容器，让每个按钮各自 hover 可见。
            .footer(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .w_full()
                    .p_1()
                    .child(new_tab_button(cx))
                    .child(open_project_button(cx)),
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
                                .child(Icon::new(IconName::SquareTerminal))
                                .child(div().font_bold().child("smelt"))
                                .child(div().text_color(c_muted).child(active_title)),
                        )
                        // 右侧齿轮：打开外观设置面板。stop_propagation 避免触发标题栏拖拽。
                        .child(
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
                                .child(Icon::new(IconName::Settings))
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _, _w, cx| {
                                        cx.stop_propagation();
                                        this.settings_open = !this.settings_open;
                                        cx.notify();
                                    }),
                                ),
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
                    .child(
                        TabBar::new("main-view-tabs")
                            .underline()
                            // 左缩进 12px，与终端/文件内容左边基线对齐（不贴边）；
                            // underline 变体的底边线是绝对满宽 div，不受此内边距影响。
                            .pl(px(12.))
                            .selected_index(match self.view {
                                MainView::Terminal => 0,
                                MainView::Files => 1,
                                MainView::Git => 2,
                            })
                            .on_click(cx.listener(|this, ix: &usize, _window, cx| {
                                this.view = match *ix {
                                    0 => MainView::Terminal,
                                    1 => MainView::Files,
                                    _ => MainView::Git,
                                };
                                cx.notify();
                            }))
                            .child(Tab::new().label("终端"))
                            .child(Tab::new().label("文件树"))
                            .child(Tab::new().label("Git")),
                    )
                    .child(match self.view {
                        MainView::Terminal => content,
                        MainView::Files => {
                            let cwd = self.cur().and_then(|s| s.cwd(cx));
                            let tree = file_tree(cwd, &self.expanded, cx);
                            let content = file_content_pane(&self.open_file, &self.file_scroll, cx);
                            div()
                                .flex_1()
                                // min_h_0：否则这个 flex item 会被文件内容撑到整份文件那么高、
                                // 溢出窗口，导致内部 uniform_list 拿不到有界高度而无法滚动。
                                .min_h_0()
                                .flex()
                                .child(
                                    div()
                                        .w(px(260.))
                                        .border_r_1()
                                        .border_color(c_border)
                                        .child(tree),
                                )
                                .child(content)
                        }
                        MainView::Git => {
                            let cwd = self.cur().and_then(|s| s.cwd(cx));
                            git_view(
                                cwd,
                                &self.git_diff,
                                self.diff_split,
                                &self.git_files_scroll,
                                &self.diff_scroll,
                                cx,
                            )
                        }
                    }),
                    )),
                ),
            )
            // 命令面板（最上层）
            .children(palette_overlay)
            // 外观设置浮层
            .children(self.settings_open.then(|| self.render_settings(cx)))
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

/// 文件树视图：读取项目目录，已展开的文件夹递归显示，点击文件夹展开/收起。
fn file_tree(cwd: Option<String>, expanded: &HashSet<String>, cx: &mut Context<Workspace>) -> Div {
    let (muted, fg, hover) = {
        let t = cx.theme();
        (t.muted_foreground, t.foreground, t.accent)
    };
    let Some(root) = cwd else {
        return placeholder_view("无项目目录", muted);
    };
    let mut flat: Vec<(usize, String, bool, String)> = Vec::new();
    walk_dir(Path::new(&root), expanded, 0, &mut flat);

    let rows: Vec<Stateful<Div>> = flat
        .into_iter()
        .enumerate()
        .map(|(i, (depth, name, is_dir, path))| {
            let indent = px(8.0 + depth as f32 * 14.0);
            // 展开箭头：目录用 chevron 图标（展开朝下 / 收起朝右），文件留等宽占位对齐。
            let arrow = if is_dir {
                div()
                    .w(px(14.))
                    .flex()
                    .justify_center()
                    .child(
                        Icon::new(if expanded.contains(&path) {
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
                if expanded.contains(&path) {
                    IconName::FolderOpen
                } else {
                    IconName::Folder
                }
            } else {
                IconName::File
            })
            .size(px(14.))
            .text_color(if is_dir { fg } else { muted });
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
                .on_click(cx.listener(move |this, _ev, _window, cx| {
                    if is_dir {
                        this.toggle_expand(p.clone(), cx);
                    } else {
                        this.view_file(p.clone(), cx);
                    }
                }))
                .child(arrow)
                .child(type_icon)
                .child(name)
        })
        .collect();

    div().flex_1().flex().flex_col().py_1().children(rows)
}

/// 递归收集目录条目（仅进入已展开的文件夹），忽略常见重目录。
fn walk_dir(
    root: &Path,
    expanded: &HashSet<String>,
    depth: usize,
    out: &mut Vec<(usize, String, bool, String)>,
) {
    let mut entries: Vec<std::fs::DirEntry> = match std::fs::read_dir(root) {
        Ok(rd) => rd.flatten().collect(),
        Err(_) => return,
    };
    entries.sort_by_key(|e| {
        (
            !e.path().is_dir(),
            e.file_name().to_string_lossy().to_lowercase(),
        )
    });
    for e in entries {
        let path = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        if matches!(name.as_str(), ".git" | "node_modules" | "target" | ".DS_Store") {
            continue;
        }
        let is_dir = path.is_dir();
        let ps = path.to_string_lossy().to_string();
        out.push((depth, name, is_dir, ps.clone()));
        if is_dir && expanded.contains(&ps) {
            walk_dir(&path, expanded, depth + 1, out);
        }
    }
}

/// Git 视图：左侧分支 + 改动文件列表（可点击），右侧显示选中文件的 diff。
fn git_view(
    cwd: Option<String>,
    git_diff: &Option<GitDiff>,
    split: bool,
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
    let output = std::process::Command::new("git")
        .args(["-C", &root, "status", "--porcelain=v1", "-b"])
        .output();
    let text = match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => return placeholder_view("不是 git 仓库，或 git 不可用", muted),
    };

    let mut branch = String::from("?");
    let mut files: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        if let Some(b) = line.strip_prefix("## ") {
            branch = b.split("...").next().unwrap_or("").trim().to_string();
        } else if line.len() >= 3 {
            files.push((line[..2].to_string(), line[3..].to_string()));
        }
    }

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
        .child(git_diff_pane(git_diff, split, diff_scroll, cx))
}

/// Git diff 查看面板：uniform_list 虚拟滚动。split 为 true 时并排（左旧右新），
/// 否则统一视图。顶部文件名右侧有「统一/并排」切换按钮。
fn git_diff_pane(
    git_diff: &Option<GitDiff>,
    split: bool,
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

            let list = if split {
                let rows = Rc::new(build_split_rows(&lines));
                let count = rows.len();
                let lines2 = lines.clone();
                uniform_list("git-diff-split", count, move |range, _w, _cx| {
                    range.map(|i| render_split_row(&rows[i], &lines2)).collect::<Vec<_>>()
                })
            } else {
                let count = lines.len();
                uniform_list("git-diff", count, move |range, _w, _cx| {
                    range.map(|i| render_diff_line(&lines[i])).collect::<Vec<_>>()
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
        }
    }
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
fn render_diff_line(l: &DiffLine) -> Div {
    // (前景, 整行背景, 色条, 行内变化片段深底) —— None 表示不上色。
    let (fg, bg, bar, hl): (Rgba, Option<Rgba>, Option<Rgba>, Rgba) = match l.kind {
        DiffKind::Add => (rgb(0xb5e08a), Some(rgb(0x16261a)), Some(rgb(0x4ba14b)), rgb(0x2f6b34)),
        DiffKind::Del => (rgb(0xf7a3ae), Some(rgb(0x2a1620)), Some(rgb(0xc75c6a)), rgb(0x7a2836)),
        DiffKind::Context => (rgb(0xc0caf5), None, None, rgb(0)),
        DiffKind::Hunk => (rgb(0x7dcfff), Some(rgb(0x16202e)), None, rgb(0)),
        DiffKind::Meta => (rgb(0x565f89), None, None, rgb(0)),
    };
    let gutter = |n: Option<u32>| {
        div()
            .w(px(44.))
            .px_1()
            .flex()
            .justify_end()
            .text_color(rgb(0x4a5178))
            .child(n.map(|v| v.to_string()).unwrap_or_default())
    };

    // 文本区：有行内 diff 就拆成多段（变化段上深底），否则整行一个文本。
    let text_area = match &l.segments {
        Some(segs) => div().flex_1().px_2().text_color(fg).flex().children(
            segs.iter().map(|(s, changed)| {
                let span = div().child(s.clone());
                if *changed {
                    span.bg(hl).rounded_sm()
                } else {
                    span
                }
            }),
        ),
        None => div()
            .flex_1()
            .px_2()
            .text_color(fg)
            .child(if l.text.is_empty() { "\u{00a0}".to_string() } else { l.text.clone() }),
    };

    let mut row = div().flex().items_center().h(px(FILE_LINE_H)).whitespace_nowrap();
    if let Some(b) = bg {
        row = row.bg(b);
    }
    row
        // 左侧色条：增/删才有，其它用等宽透明占位保持对齐。
        .child(match bar {
            Some(c) => div().w(px(2.)).h_full().bg(c),
            None => div().w(px(2.)).h_full(),
        })
        .child(gutter(l.old_ln))
        .child(gutter(l.new_ln))
        .child(text_area)
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
/// left=true 用旧行号，否则用新行号。
fn render_half(idx: Option<usize>, left: bool, lines: &[DiffLine]) -> Div {
    // overflow_hidden：长行必须裁剪在本半区内，否则会溢出盖住另一半，并排就糊了。
    let base = div().flex_1().min_w_0().overflow_hidden().flex().items_center().h_full();
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

/// 渲染并排视图的一行。
fn render_split_row(row: &SplitRow, lines: &[DiffLine]) -> Div {
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
            .child(render_half(*l, true, lines))
            .child(div().w(px(1.)).h_full().bg(rgb(0x2a2e3d))) // 中缝分隔
            .child(render_half(*r, false, lines)),
    }
}

/// 语法集/主题集只加载一次（load_defaults 较重），进程内缓存复用。
fn syntax_set() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(SyntaxSet::load_defaults_newlines)
}
fn theme_set() -> &'static ThemeSet {
    static SET: OnceLock<ThemeSet> = OnceLock::new();
    SET.get_or_init(ThemeSet::load_defaults)
}

/// syntect 颜色 → gpui 颜色。
fn syn_color(c: syntect::highlighting::Color) -> Rgba {
    rgb(((c.r as u32) << 16) | ((c.g as u32) << 8) | c.b as u32)
}

/// 文件查看的固定行高（供 uniform_list 虚拟滚动，需每行等高）。
const FILE_LINE_H: f32 = 20.0;

/// 一次性把整份文本语法高亮成「行 → (颜色, 片段) 列表」。
/// syntect 的高亮是有状态的（逐行累积），必须从头顺序处理，不能随机访问，
/// 所以在打开文件时算好、存下来，滚动时只取可见行。最多 20000 行。
fn highlight_all(path: &str, content: &str) -> Vec<Vec<(Rgba, String)>> {
    let ss = syntax_set();
    let ext = Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or("");
    let syntax = ss
        .find_syntax_by_extension(ext)
        .unwrap_or_else(|| ss.find_syntax_plain_text());
    let mut hl = HighlightLines::new(syntax, &theme_set().themes["base16-ocean.dark"]);
    content
        .lines()
        .take(20000)
        .map(|line| {
            hl.highlight_line(line, ss)
                .unwrap_or_default()
                .into_iter()
                .map(|(style, text)| (syn_color(style.foreground), text.to_string()))
                .collect()
        })
        .collect()
}

/// 文件内容查看面板：uniform_list 虚拟滚动，只渲染可见行（高亮已预计算）。
fn file_content_pane(
    open_file: &Option<OpenFile>,
    file_scroll: &UniformListScrollHandle,
    cx: &mut Context<Workspace>,
) -> Div {
    let (muted, fg, border) = {
        let t = cx.theme();
        (t.muted_foreground, t.foreground, t.border)
    };
    match open_file {
        None => placeholder_view("← 从左侧选择文件查看内容", muted),
        Some(of) => {
            let name = of.path.rsplit('/').next().unwrap_or(of.path.as_str()).to_string();
            let lines = of.lines.clone(); // Rc clone：闭包按可见范围取行
            let count = lines.len();

            let list = uniform_list("file-content", count, move |range, _window, _cx| {
                range
                    .map(|i| {
                        let spans = &lines[i];
                        let row = div().flex().whitespace_nowrap().h(px(FILE_LINE_H));
                        if spans.is_empty() {
                            // 空行放不间断空格占位，保持行高。
                            row.child("\u{00a0}".to_string())
                        } else {
                            row.children(spans.iter().map(|(color, text)| {
                                div().text_color(*color).child(text.clone())
                            }))
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .flex_1()
            .min_h_0()
            .w_full()
            .p_2()
            .font_family(terminal_view::FONT_FAMILY)
            .text_sm()
            .text_color(fg)
            .track_scroll(file_scroll);

            div()
                .flex_1()
                .min_w_0()
                .min_h_0()
                .flex()
                .flex_col()
                .child(
                    div()
                        .px_3()
                        .py_1()
                        .text_sm()
                        .text_color(muted)
                        .border_b_1()
                        .border_color(border)
                        .child(name),
                )
                // relative 容器承载竖向滚动条。
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .relative()
                        .flex()
                        .flex_col()
                        .child(list)
                        .vertical_scrollbar(file_scroll),
                )
        }
    }
}

/// 命令面板的键盘处理：字符过滤、上下选择、回车执行、Esc 关闭。
/// 侧栏底部工具按钮：图标 + 明显 hover + tooltip。
/// （组件 Button 的 ghost 在暗色下 hover 几乎不可见，这里自绘保证反馈明显。）
fn tool_button(
    id: &'static str,
    icon: IconName,
    tip: &'static str,
    cx: &mut Context<Workspace>,
    handler: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
) -> Stateful<Div> {
    let (fg, hover) = {
        let t = cx.theme();
        (t.sidebar_foreground, t.sidebar_accent)
    };
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .size_7()
        .rounded_md()
        .text_color(fg)
        .hover(move |s| s.bg(hover))
        .tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
        .child(Icon::new(icon))
        .on_click(cx.listener(move |this, _ev, window, cx| handler(this, window, cx)))
}

/// 「+」新建终端按钮（继承当前项目目录）。
fn new_tab_button(cx: &mut Context<Workspace>) -> Stateful<Div> {
    tool_button("new-tab", IconName::Plus, "新建终端", cx, |this, _w, cx| {
        this.new_tab(cx)
    })
}

/// 「打开项目」按钮：弹选择框选目录，在其中开新标签。
fn open_project_button(cx: &mut Context<Workspace>) -> Stateful<Div> {
    tool_button("open-project", IconName::Folder, "打开项目", cx, |this, _w, cx| {
        this.open_project(cx)
    })
}

/// 当前工作目录字符串。
fn current_dir() -> Option<String> {
    std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
}

fn main() {
    // with_assets 注册组件库图标资源，Sidebar 的 IconName svg 才能渲染。
    gpui_platform::application()
        .with_assets(gpui_component_assets::Assets)
        .run(move |cx| {
        // 用任何 gpui-component 功能前必须先初始化。
        gpui_component::init(cx);
        // 深色主题（与终端配色一致）
        Theme::change(ThemeMode::Dark, None, cx);

        // 应用菜单栏 + Cmd+Q 退出：macOS 顶部「Smelt」菜单，含「退出 Smelt ⌘Q」。
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
        cx.set_menus(vec![
            Menu::new("Smelt").items([MenuItem::action("退出 Smelt", Quit)]),
        ]);

        // 外观设置：读盘设为全局单例，据此确定窗口背景外观（透明 / 模糊）。
        let appearance = load_appearance();
        let window_bg = appearance.window_bg();
        cx.set_global(appearance);

        cx.spawn(async move |cx| {
            let window_options = WindowOptions {
                // 透明标题栏：红绿灯浮在内容上，拖拽 / 双击最大化由自定义 TitleBar 接管。
                titlebar: Some(TitleBar::title_bar_options()),
                // 透明/模糊背景（跟随外观设置；终端底色带 alpha 时桌面透出）。
                window_background: window_bg,
                ..Default::default()
            };
            cx.open_window(window_options, |window, cx| {
                let view = cx.new(|cx| Workspace::new(cx));
                // 顶层视图必须包一层 Root（组件库的主题/遮罩系统要求）。
                cx.new(|cx| Root::new(view, window, cx))
            })
            .expect("打开窗口失败");
        })
        .detach();
    });
}
