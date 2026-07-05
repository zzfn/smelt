//! smelt 工作台 —— 基于 gpui-component 的桌面窗口。
//!
//! Workspace 管理多个终端标签（TerminalView）：顶部标签栏切换 / 新建 / 关闭，
//! 下方渲染当前活动终端。每个终端各自独立（PTY、IME、滚动、resize）。
//!
//! 运行： cargo run --bin workspace

mod terminal;
mod terminal_view;

use gpui::*;
use gpui_component::*;
use terminal_view::TerminalView;

/// 工作台根视图：多标签终端管理器。
struct Workspace {
    tabs: Vec<Entity<TerminalView>>,
    active: usize,
}

impl Workspace {
    fn new(cx: &mut Context<Self>) -> Self {
        let first = cx.new(|cx| TerminalView::new(cx, current_dir()));
        Self {
            tabs: vec![first],
            active: 0,
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
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.active;
        let can_close = self.tabs.len() > 1;

        // 先收集标签标题，释放对 self.tabs 的借用
        let titles: Vec<(usize, String)> = self
            .tabs
            .iter()
            .enumerate()
            .map(|(ix, v)| (ix, v.read(cx).title().to_string()))
            .collect();

        let tab_buttons: Vec<Stateful<Div>> = titles
            .into_iter()
            .map(|(ix, title)| tab_button(ix, title, ix == active, can_close, cx))
            .collect();

        let active_view = self.tabs[active].clone();

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(0x1a1b26))
            .font_family(terminal_view::FONT_FAMILY)
            // 标签栏
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .bg(rgb(0x16161e))
                    .children(tab_buttons)
                    .child(new_tab_button(cx))
                    .child(open_project_button(cx)),
            )
            // 活动终端
            .child(div().flex_1().child(active_view))
    }
}

/// 一个标签按钮：点击切换；活动态高亮；可关闭时带「×」。
fn tab_button(
    ix: usize,
    title: String,
    active: bool,
    can_close: bool,
    cx: &mut Context<Workspace>,
) -> Stateful<Div> {
    let (bg, fg) = if active {
        (rgb(0x2a2b3d), rgb(0xc0caf5))
    } else {
        (rgb(0x16161e), rgb(0x565f89))
    };

    let mut tab = div()
        .id(("tab", ix))
        .flex()
        .items_center()
        .gap_2()
        .px_3()
        .py_1()
        .rounded_md()
        .bg(bg)
        .text_color(fg)
        .text_sm()
        .on_click(cx.listener(move |this, _ev, window, cx| {
            if ix >= this.tabs.len() {
                return;
            }
            this.active = ix;
            let h = this.tabs[ix].read(cx).focus_handle();
            window.focus(&h, cx);
            cx.notify();
        }))
        .child(title);

    if can_close {
        tab = tab.child(
            div()
                .id(("close", ix))
                .px_1()
                .rounded_sm()
                .text_color(rgb(0x565f89))
                .on_click(cx.listener(move |this, _ev, _window, cx| {
                    // 阻止冒泡到标签切换
                    cx.stop_propagation();
                    this.close_tab(ix, cx);
                }))
                .child("×"),
        );
    }

    tab
}

/// 「+」新建标签按钮（继承当前项目目录）。
fn new_tab_button(cx: &mut Context<Workspace>) -> Stateful<Div> {
    div()
        .id("new-tab")
        .px_2()
        .py_1()
        .rounded_md()
        .text_color(rgb(0x7aa2f7))
        .on_click(cx.listener(|this, _ev, _window, cx| {
            this.new_tab(cx);
        }))
        .child("+")
}

/// 「打开项目」按钮：弹选择框选目录，在其中开新标签。
fn open_project_button(cx: &mut Context<Workspace>) -> Stateful<Div> {
    div()
        .id("open-project")
        .px_2()
        .py_1()
        .rounded_md()
        .text_color(rgb(0x565f89))
        .on_click(cx.listener(|this, _ev, _window, cx| {
            this.open_project(cx);
        }))
        .child("📂")
}

/// 当前工作目录字符串。
fn current_dir() -> Option<String> {
    std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
}

fn main() {
    gpui_platform::application().run(move |cx| {
        // 用任何 gpui-component 功能前必须先初始化。
        gpui_component::init(cx);

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
