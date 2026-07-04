//! smelt 工作台 —— 基于 gpui-component 的前端页面。
//!
//! 从裸 gpui 迁到 gpui-component（Apache-2.0 组件库）：为后续的多面板工作台
//! （Dock / 标签 / 分屏）+ 内嵌终端做基座。当前仍是终端风格首页（静态）。
//!
//! 运行： cargo run --bin workspace

use gpui::*;
use gpui_component::*;

/// 工作台根视图：终端风格首页。
struct Workspace;

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(0x1a1b26))
            .text_color(rgb(0xc0caf5))
            .font_family("monospace")
            .text_sm()
            // 顶部标题栏：mac 三色圆点 + 标题
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_4()
                    .py_2()
                    .bg(rgb(0x16161e))
                    .child(dot(0xff5f56))
                    .child(dot(0xffbd2e))
                    .child(dot(0x27c93f))
                    .child(
                        div()
                            .ml_2()
                            .text_color(rgb(0x7aa2f7))
                            .child("smelt workspace"),
                    ),
            )
            // 终端主体：说明 + 提示符 + 块状光标
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .p_4()
                    .gap_1()
                    .child(
                        div()
                            .text_color(rgb(0x565f89))
                            .child("smelt workspace v0.1  ·  基于 gpui-component"),
                    )
                    .child(div().h(px(8.)))
                    .child("将在这里嵌入真正的终端，在你的项目里运行 claude code。")
                    .child("基座已切到 gpui-component，下一步可加 Dock / 标签 / 面板。")
                    .child(div().h(px(8.)))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().text_color(rgb(0x9ece6a)).child("smelt ▸"))
                            .child(div().w(px(8.)).h(px(18.)).bg(rgb(0xc0caf5))),
                    ),
            )
    }
}

/// 标题栏上的一个 mac 风格圆点。
fn dot(color: u32) -> impl IntoElement {
    div().w(px(12.)).h(px(12.)).rounded_full().bg(rgb(color))
}

fn main() {
    gpui_platform::application().run(move |cx| {
        // 用任何 gpui-component 功能前必须先初始化。
        gpui_component::init(cx);

        cx.spawn(async move |cx| {
            cx.open_window(WindowOptions::default(), |window, cx| {
                let view = cx.new(|_| Workspace);
                // 顶层视图必须包一层 Root（组件库的主题/遮罩系统要求）。
                cx.new(|cx| Root::new(view, window, cx))
            })
            .expect("打开窗口失败");
        })
        .detach();
    });
}
