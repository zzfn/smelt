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

/// 命令面板状态。
struct Palette {
    query: String,
    selected: usize,
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
    /// 左侧会话侧栏是否展开（Cmd+B 切换）。
    sidebar_open: bool,
    /// 命令面板（Cmd+K）；None 表示未打开。
    palette: Option<Palette>,
    palette_focus: FocusHandle,
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
            sidebar_open: true,
            palette: None,
            palette_focus: cx.focus_handle(),
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

    fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette = Some(Palette {
            query: String::new(),
            selected: 0,
        });
        window.focus(&self.palette_focus, cx);
        cx.notify();
    }

    fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette = None;
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

    /// 按查询过滤后的命令。
    fn filtered(&self, cx: &App) -> Vec<(String, Cmd)> {
        let q = self
            .palette
            .as_ref()
            .map(|p| p.query.to_lowercase())
            .unwrap_or_default();
        self.all_commands(cx)
            .into_iter()
            .filter(|(label, _)| q.is_empty() || label.to_lowercase().contains(&q))
            .collect()
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
        let (c_bg, c_sidebar, c_border, c_muted, c_accent, c_accent_fg, c_primary, c_popover, c_fg) = {
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
                t.foreground,
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

        // 命令面板弹层
        let palette_overlay = self.palette.as_ref().map(|p| {
            let cmds = self.filtered(cx);
            let sel = if cmds.is_empty() {
                0
            } else {
                p.selected.min(cmds.len() - 1)
            };
            let query = p.query.clone();
            let items: Vec<Stateful<Div>> = cmds
                .iter()
                .enumerate()
                .map(|(i, (label, cmd))| {
                    let is_sel = i == sel;
                    let cmd = cmd.clone();
                    let mut d = div()
                        .id(("cmd", i))
                        .px_3()
                        .py_1()
                        .text_color(if is_sel { c_accent_fg } else { c_muted })
                        .on_click(cx.listener(move |this, _ev, window, cx| {
                            this.exec_cmd(cmd.clone(), window, cx)
                        }))
                        .child(label.clone());
                    if is_sel {
                        d = d.bg(c_accent);
                    }
                    d
                })
                .collect();

            div()
                .absolute()
                .inset_0()
                .flex()
                .justify_center()
                .pt(px(80.))
                .child(
                    div()
                        .track_focus(&self.palette_focus)
                        .on_key_down(cx.listener(palette_key))
                        .w(px(480.))
                        .flex()
                        .flex_col()
                        .bg(c_popover)
                        .border_1()
                        .border_color(c_border)
                        .rounded_lg()
                        .shadow_lg()
                        .child(
                            div()
                                .px_3()
                                .py_2()
                                .text_color(c_fg)
                                .child(if query.is_empty() {
                                    "› 输入命令…".to_string()
                                } else {
                                    format!("› {}", query)
                                }),
                        )
                        .children(items),
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
                            git_view(cwd, cx)
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
            let icon = if is_dir {
                if expanded.contains(&path) {
                    "▾"
                } else {
                    "▸"
                }
            } else {
                " "
            };
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
                .child(icon.to_string())
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

/// Git 视图：显示当前分支 + 改动文件（git status）。
fn git_view(cwd: Option<String>, cx: &mut Context<Workspace>) -> Div {
    let (muted, fg, border) = {
        let t = cx.theme();
        (t.muted_foreground, t.foreground, t.border)
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

    let body = if files.is_empty() {
        placeholder_view("工作区干净，无改动 ✓", muted)
    } else {
        div()
            .flex_1()
            .flex()
            .flex_col()
            .p_1()
            .children(files.into_iter().enumerate().map(|(i, (st, path))| {
                let color = git_status_color(&st);
                div()
                    .id(("git", i))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py(px(1.0))
                    .text_sm()
                    .child(
                        div()
                            .w(px(22.))
                            .text_color(color)
                            .child(if st.trim().is_empty() {
                                "•".to_string()
                            } else {
                                st.trim().to_string()
                            }),
                    )
                    .child(div().text_color(fg).child(path))
            }))
    };

    div()
        .flex_1()
        .flex()
        .flex_col()
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
        .child(body)
}

/// git 状态码 → 颜色（约定色）。
fn git_status_color(st: &str) -> Rgba {
    if st.contains('?') {
        rgb(0x565f89) // 未跟踪
    } else if st.contains('A') {
        rgb(0x9ece6a) // 新增
    } else if st.contains('D') {
        rgb(0xf7768e) // 删除
    } else if st.contains('M') {
        rgb(0xe0af68) // 修改
    } else {
        rgb(0x7aa2f7)
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
fn palette_key(
    this: &mut Workspace,
    ev: &KeyDownEvent,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if this.palette.is_none() {
        return;
    }
    let ks = &ev.keystroke;
    match ks.key.as_str() {
        "escape" => this.close_palette(window, cx),
        "up" => {
            if let Some(p) = this.palette.as_mut() {
                p.selected = p.selected.saturating_sub(1);
            }
            cx.notify();
        }
        "down" => {
            let len = this.filtered(cx).len();
            if let Some(p) = this.palette.as_mut() {
                if len > 0 && p.selected + 1 < len {
                    p.selected += 1;
                }
            }
            cx.notify();
        }
        "backspace" => {
            if let Some(p) = this.palette.as_mut() {
                p.query.pop();
                p.selected = 0;
            }
            cx.notify();
        }
        "enter" => {
            let sel = this.palette.as_ref().map(|p| p.selected).unwrap_or(0);
            let cmds = this.filtered(cx);
            if let Some((_, cmd)) = cmds.into_iter().nth(sel) {
                this.exec_cmd(cmd, window, cx);
            }
        }
        _ => {
            if !ks.modifiers.platform && !ks.modifiers.control && !ks.modifiers.function {
                if let Some(kc) = ks.key_char.clone() {
                    if !kc.is_empty() {
                        if let Some(p) = this.palette.as_mut() {
                            p.query.push_str(&kc);
                            p.selected = 0;
                        }
                        cx.notify();
                    }
                }
            }
        }
    }
}

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
