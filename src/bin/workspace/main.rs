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
    Sidebar, SidebarCollapsible, SidebarGroup, SidebarHeader, SidebarMenu, SidebarMenuItem,
};
use gpui_component::list::{List, ListDelegate, ListEvent, ListItem, ListState};
use gpui_component::tag::Tag;
use gpui_component::tooltip::Tooltip;
use gpui_component::*;
use terminal_view::TerminalView;

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

/// 工作台根视图：多标签终端管理器。
struct Workspace {
    tabs: Vec<Entity<TerminalView>>,
    active: usize,
    /// 网格列数：1=单终端，2=两列，3=三列。
    layout_cols: usize,
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
    /// 命令面板（Cmd+K）；None 表示未打开。搜索/导航/确认由 ListState 负责。
    palette: Option<Entity<ListState<CmdDelegate>>>,
    /// 命令面板的事件订阅（确认/取消）；随面板关闭一并释放。
    _palette_sub: Option<Subscription>,
}

impl Workspace {
    fn new(cx: &mut Context<Self>) -> Self {
        let first = cx.new(|cx| TerminalView::new(cx, current_dir()));
        Self {
            tabs: vec![first],
            active: 0,
            layout_cols: 1,
            view: MainView::Terminal,
            expanded: HashSet::new(),
            open_file: None,
            file_gen: 0,
            git_diff: None,
            diff_gen: 0,
            diff_split: false,
            sidebar_open: true,
            palette: None,
            _palette_sub: None,
        }
    }

    /// 在指定目录新建标签并激活。
    fn add_tab(&mut self, cwd: Option<String>, cx: &mut Context<Self>) {
        let view = cx.new(|cx| TerminalView::new(cx, cwd));
        self.tabs.push(view);
        self.active = self.tabs.len() - 1;
        cx.notify();
    }

    /// 「+」新建标签：继承当前活动标签的目录。
    fn new_tab(&mut self, cx: &mut Context<Self>) {
        let cwd = self
            .tabs
            .get(self.active)
            .and_then(|t| t.read(cx).cwd())
            .or_else(current_dir);
        self.add_tab(cwd, cx);
    }

    /// 「打开项目」：弹原生选择框选一个目录，在其中开新标签。
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
                    this.update(cx, |this, cx| this.add_tab(dir, cx)).ok();
                }
            }
        })
        .detach();
    }

    fn close_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.tabs.len() <= 1 || ix >= self.tabs.len() {
            return; // 至少保留一个终端
        }
        self.tabs.remove(ix);
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        } else if self.active > ix {
            self.active -= 1;
        }
        cx.notify();
    }

    /// 聚焦当前活动终端。
    fn focus_active(&self, window: &mut Window, cx: &mut App) {
        if let Some(t) = self.tabs.get(self.active) {
            let h = t.read(cx).focus_handle();
            window.focus(&h, cx);
        }
    }

    /// 切换到第 ix 个标签并聚焦。
    fn activate(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix < self.tabs.len() {
            self.active = ix;
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    /// 循环切换网格布局：1 → 2 → 3 → 1 列。
    fn cycle_layout(&mut self, cx: &mut Context<Self>) {
        self.layout_cols = match self.layout_cols {
            1 => 2,
            2 => 3,
            _ => 1,
        };
        cx.notify();
    }

    fn next_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let n = self.tabs.len();
        if n > 0 {
            self.activate((self.active + 1) % n, window, cx);
        }
    }

    fn prev_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let n = self.tabs.len();
        if n > 0 {
            self.activate((self.active + n - 1) % n, window, cx);
        }
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

    /// 全部命令（含逐标签切换）。
    fn all_commands(&self, cx: &App) -> Vec<(String, Cmd)> {
        let mut v = vec![
            ("新建标签".to_string(), Cmd::NewTab),
            ("打开项目…".to_string(), Cmd::OpenProject),
            ("关闭当前标签".to_string(), Cmd::CloseTab),
            ("下一个标签".to_string(), Cmd::NextTab),
            ("上一个标签".to_string(), Cmd::PrevTab),
        ];
        for (i, t) in self.tabs.iter().enumerate() {
            v.push((format!("切换到: {}", t.read(cx).title()), Cmd::SwitchTab(i)));
        }
        v
    }

    fn exec_cmd(&mut self, cmd: Cmd, window: &mut Window, cx: &mut Context<Self>) {
        self.close_palette(window, cx);
        match cmd {
            Cmd::NewTab => self.new_tab(cx),
            Cmd::OpenProject => self.open_project(cx),
            Cmd::CloseTab => self.close_tab(self.active, cx),
            Cmd::NextTab => {
                let n = self.tabs.len();
                if n > 0 {
                    self.activate((self.active + 1) % n, window, cx);
                }
            }
            Cmd::PrevTab => {
                let n = self.tabs.len();
                if n > 0 {
                    self.activate((self.active + n - 1) % n, window, cx);
                }
            }
            Cmd::SwitchTab(i) => self.activate(i, window, cx),
        }
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.active;
        let can_close = self.tabs.len() > 1;

        // 主题色 token（跟随 gpui-component 主题，替代硬编码）
        let (c_bg, c_sidebar, c_border, c_muted, c_accent, c_accent_fg, c_primary, c_popover) = {
            let t = cx.theme();
            (
                t.background,
                t.sidebar,
                t.border,
                t.muted_foreground,
                t.sidebar_accent,
                t.sidebar_accent_foreground,
                t.primary,
                t.popover,
            )
        };

        // 先收集标签标题，释放对 self.tabs 的借用
        let titles: Vec<(usize, String)> = self
            .tabs
            .iter()
            .enumerate()
            .map(|(ix, v)| (ix, v.read(cx).title().to_string()))
            .collect();

        // 左侧会话侧栏
        // 按 cwd 把终端分组成项目（保持出现顺序）
        let mut projects: Vec<(String, Vec<usize>)> = Vec::new();
        for (ix, _title) in titles.iter() {
            let cwd = self.tabs[*ix].read(cx).cwd().unwrap_or_default();
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

        // 项目 → 终端 两级菜单（gpui-component Sidebar）。
        // Sidebar 组件的回调是 Fn(&_, &mut Window, &mut App)，拿不到 Context<Self>，
        // 故捕获 entity 句柄在闭包里 update 自身。
        let this = cx.entity();
        let menu_items: Vec<SidebarMenuItem> = projects
            .iter()
            .map(|(name, ixs)| {
                let term_items: Vec<SidebarMenuItem> = ixs
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
                                Button::new(("close-tab", ix))
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::CircleX)
                                    .on_click(move |_ev, _w, cx| {
                                        // 别把点击冒泡成「切换到该终端」
                                        cx.stop_propagation();
                                        e.update(cx, |ws, cx| ws.close_tab(ix, cx));
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
                    .children(term_items)
            })
            .collect();

        let sidebar_el = Sidebar::new("workspace-sidebar")
            .collapsible(SidebarCollapsible::Offcanvas)
            .collapsed(!self.sidebar_open)
            .w(px(230.))
            .header(
                SidebarHeader::new().child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(Icon::new(IconName::SquareTerminal))
                        .child(div().font_bold().child("smelt")),
                ),
            )
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
                    .child(open_project_button(cx))
                    .child(div().flex_1())
                    .child(layout_button(self.layout_cols, cx)),
            );

        // 主内容：单终端 或 网格（多列）
        let cols = self.layout_cols;
        let n = self.tabs.len();
        let content = if cols <= 1 {
            div().flex_1().min_w_0().child(self.tabs[active].clone())
        } else {
            let rows: Vec<Div> = (0..n)
                .step_by(cols)
                .map(|start| {
                    let end = (start + cols).min(n);
                    let cards: Vec<Div> = (start..end)
                        .map(|ix| {
                            let is_active = ix == active;
                            let view = self.tabs[ix].clone();
                            let title = titles.get(ix).map(|(_, t)| t.clone()).unwrap_or_default();
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .border_1()
                                .border_color(if is_active { c_primary } else { c_border })
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _ev, window, cx| {
                                        this.activate(ix, window, cx)
                                    }),
                                )
                                // 卡片标题头
                                .child(
                                    div()
                                        .px_2()
                                        .py_1()
                                        .text_sm()
                                        .bg(if is_active { c_accent } else { c_sidebar })
                                        .text_color(if is_active { c_accent_fg } else { c_muted })
                                        .child(title),
                                )
                                .child(div().flex_1().min_w_0().child(view))
                        })
                        .collect();
                    div().flex_1().min_w_0().flex().gap_1().children(cards)
                })
                .collect();
            div().flex_1().flex().flex_col().gap_1().p_1().children(rows)
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
            .size_full()
            .bg(c_bg)
            .font_family(terminal_view::FONT_FAMILY)
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
                    "\\" => this.cycle_layout(cx),
                    "[" => this.prev_active(window, cx),
                    "]" => this.next_active(window, cx),
                    _ => {}
                }
            }))
            // 左侧会话侧栏（gpui-component Sidebar 组件）
            .child(sidebar_el)
            // 主区：顶部视图切换 + 内容
            .child(
                div()
                    .flex_1()
                    // min_w_0：主区在根 flex 行里默认 min-width:auto，会被最长终端行
                    // 撑到不肯收缩，导致宽度被内容反向放大。归零后才能正常按剩余空间收缩。
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .px_2()
                            .py_1()
                            .border_b_1()
                            .border_color(c_border)
                            .child(view_tab(0, "终端", self.view == MainView::Terminal, MainView::Terminal, cx))
                            .child(view_tab(1, "文件树", self.view == MainView::Files, MainView::Files, cx))
                            .child(view_tab(2, "Git", self.view == MainView::Git, MainView::Git, cx)),
                    )
                    .child(match self.view {
                        MainView::Terminal => content,
                        MainView::Files => {
                            let cwd = self.tabs.get(active).and_then(|t| t.read(cx).cwd());
                            let tree = file_tree(cwd, &self.expanded, cx);
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
                                        .border_r_1()
                                        .border_color(c_border)
                                        .child(tree),
                                )
                                .child(content)
                        }
                        MainView::Git => {
                            let cwd = self.tabs.get(active).and_then(|t| t.read(cx).cwd());
                            git_view(cwd, &self.git_diff, self.diff_split, cx)
                        }
                    }),
            )
            // 命令面板（最上层）
            .children(palette_overlay)
    }
}

/// 顶部视图切换标签。
fn view_tab(
    id: usize,
    label: &str,
    active: bool,
    view: MainView,
    cx: &mut Context<Workspace>,
) -> Stateful<Div> {
    let t = cx.theme();
    let (fg, bg, hover) = if active {
        (t.foreground, t.accent, t.accent)
    } else {
        (t.muted_foreground, t.background, t.accent)
    };
    div()
        .id(("view", id))
        .px_3()
        .py_1()
        .rounded_md()
        .text_sm()
        .bg(bg)
        .text_color(fg)
        .hover(move |s| s.bg(hover))
        .on_click(cx.listener(move |this, _ev, _window, cx| {
            this.view = view;
            cx.notify();
        }))
        .child(label.to_string())
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
        .child(git_diff_pane(git_diff, split, cx))
}

/// Git diff 查看面板：uniform_list 虚拟滚动。split 为 true 时并排（左旧右新），
/// 否则统一视图。顶部文件名右侧有「统一/并排」切换按钮。
fn git_diff_pane(git_diff: &Option<GitDiff>, split: bool, cx: &mut Context<Workspace>) -> Div {
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
            .text_sm();

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
                .child(list)
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
fn file_content_pane(open_file: &Option<OpenFile>, cx: &mut Context<Workspace>) -> Div {
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
            .text_color(fg);

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
                .child(list)
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

/// 布局切换按钮：显示当前列数图标，点击循环 1/2/3 列（无匹配 svg，用字符）。
fn layout_button(cols: usize, cx: &mut Context<Workspace>) -> Stateful<Div> {
    let glyph = match cols {
        1 => "▢",
        2 => "▥",
        _ => "▦",
    };
    let (fg, hover) = {
        let t = cx.theme();
        (t.sidebar_foreground, t.sidebar_accent)
    };
    div()
        .id("layout")
        .flex()
        .items_center()
        .justify_center()
        .size_7()
        .rounded_md()
        .text_color(fg)
        .hover(move |s| s.bg(hover))
        .tooltip(move |window, cx| Tooltip::new("切换布局").build(window, cx))
        .child(glyph)
        .on_click(cx.listener(|this, _ev, _window, cx| this.cycle_layout(cx)))
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
