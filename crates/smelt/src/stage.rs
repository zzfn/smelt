//! 会话舞台的头部（44px：类型点 + 会话名 + 状态胶囊 + 副标题 + split/菜单）
//! 与终端会话的底条（26px：launch 名 · 相位 · 快捷键提示）。
//!
//! 跟 file_tree.rs 同一个套路：`impl Workspace` 方法，字段仍在 main.rs。

use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::menu::{ContextMenuExt, PopupMenuItem};
use gpui_component::*;

use crate::{AgentStatus, MainView, RenameTarget, SessionKind, Workspace, ui_theme};

/// 状态胶囊文案（与会话列表副标题同一套口径）。
fn phase_text(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::WaitingApproval => "等你批准",
        AgentStatus::NeedsAttention => "需要处理",
        AgentStatus::Running => "运行中",
        AgentStatus::Done => "已完成",
        AgentStatus::Idle => "空闲",
    }
}

impl Workspace {
    /// 全屏覆盖页顶部的返回条（32px）：显式的「‹ 返回会话」出口 + 页面名 +
    /// Esc 提示。Esc 只在焦点没被输入框吃掉时管用，必须有一个总能点到的按钮。
    pub(crate) fn render_stage_back_bar(&self, v: MainView, cx: &mut Context<Self>) -> Div {
        // Files/Git 是「面板提升上来的全宽双栏」，收回叫 minimize（回停靠）；
        // 其余是盖在舞台上的独立页，收回叫「返回会话」。
        let (label, back) = match v {
            MainView::Overview => ("会话总览", "‹ 返回会话"),
            MainView::Tasks => ("任务总览", "‹ 返回会话"),
            MainView::Files => ("文件树 + 内容", "⤡ 收回停靠"),
            MainView::FileDetail => ("文件", "‹ 返回会话"),
            MainView::Git => ("变更 + diff", "⤡ 收回停靠"),
            MainView::DiffDetail => ("diff", "‹ 返回会话"),
            MainView::Hotspot => ("热力图", "‹ 返回会话"),
            MainView::History => ("历史会话", "‹ 返回会话"),
        };
        let this = cx.entity();
        div()
            .h(px(32.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_2p5()
            .px_3()
            .bg(rgb(ui_theme::bg_status()))
            .border_b_1()
            .border_color(rgb(ui_theme::border_dim()))
            .child(
                div()
                    .id("stage-back")
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py(px(2.))
                    .rounded(px(6.))
                    .text_sm()
                    .text_color(rgb(ui_theme::text_mid()))
                    .cursor_pointer()
                    .hover(|d| {
                        d.bg(rgb(ui_theme::bg_hover()))
                            .text_color(rgb(ui_theme::text_bright()))
                    })
                    .child(back)
                    .on_click(move |_ev, window, cx| {
                        this.update(cx, |ws, cx| ws.set_stage_override(None, window, cx));
                    }),
            )
            .child(
                div()
                    .text_xs()
                    .font_semibold()
                    .text_color(rgb(ui_theme::text_faint()))
                    .child(label),
            )
            .child(div().flex_1())
            .child(
                div()
                    .text_xs()
                    .font_family("monospace")
                    .text_color(rgb(ui_theme::text_faint()))
                    .child("Esc"),
            )
    }

    /// 44px 舞台头。没有会话时返回 None（空态自带引导）。
    pub(crate) fn render_stage_header(&mut self, cx: &mut Context<Self>) -> Option<Div> {
        let ix = self.active_session;
        let sess = self.sessions.get(ix)?;
        let title = sess.title(cx);
        let is_term = matches!(sess.kind, SessionKind::Term { .. });
        // 状态胶囊：ACP 直接问视图要相位（它有自己的相位机，经五态映射会把
        // 「启动中 / 已结束」都塌成「空闲」）；终端仍走 AgentStatus 那套。
        let (phase_label, phase_color) = match &sess.kind {
            SessionKind::Acp(view) => {
                let (label, color) = view.read(cx).phase_label();
                (label, rgb(color))
            }
            SessionKind::Term { .. } => {
                let st = sess.status(cx);
                (phase_text(st), ui_theme::session_dot_color(st))
            }
        };
        // ACP 会话把当前模型也摆到舞台头上——「这轮对话用的哪个模型」是随时
        // 要能确认的事实，不该只藏在输入栏胶囊里。
        let model = match &sess.kind {
            SessionKind::Acp(view) => view.read(cx).model_name(),
            SessionKind::Term { .. } => None,
        };
        let cwd_tail = sess
            .cwd(cx)
            .map(|c| crate::project_name_for_cwd(&c))
            .unwrap_or_default();
        let this = cx.entity();

        // 类型点：agent 紫圆 / 终端绿方（与会话列表一致）。
        let type_dot: AnyElement = if is_term {
            div()
                .size(px(9.))
                .rounded_xs()
                .bg(rgb(ui_theme::green()))
                .into_any_element()
        } else {
            div()
                .size(px(9.))
                .rounded_full()
                .bg(rgb(ui_theme::purple()))
                .into_any_element()
        };

        let e_split = this.clone();
        let e_menu = this.clone();
        // 分屏会话里「关闭会话」= 连所有分屏一起关，文案要点明（关单个 pane 用 ⌘W）。
        let multi_pane = sess.pane_count() > 1;
        Some(
            div()
                .h(px(44.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .gap_2p5()
                .px_4()
                .border_b_1()
                .border_color(rgb(ui_theme::border_dim()))
                .child(type_dot)
                .child(
                    div()
                        .text_sm()
                        .font_semibold()
                        .text_color(rgb(ui_theme::text_bright()))
                        .child(title),
                )
                .child(
                    // 状态胶囊：点 + 文案。
                    div()
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .px_2()
                        .py(px(2.))
                        .rounded(px(6.))
                        .bg(rgb(ui_theme::bg_card()))
                        .border_1()
                        .border_color(rgb(ui_theme::border_mid()))
                        .text_xs()
                        .text_color(rgb(ui_theme::text_muted()))
                        .child(div().size(px(6.)).rounded_full().bg(phase_color))
                        .child(phase_label),
                )
                .children(model.map(|m| {
                    div()
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .px_2()
                        .py(px(2.))
                        .rounded(px(6.))
                        .bg(rgb(ui_theme::bg_card()))
                        .border_1()
                        .border_color(rgb(ui_theme::border_mid()))
                        .text_xs()
                        .text_color(rgb(ui_theme::text_mid()))
                        .child(
                            div()
                                .size(px(6.))
                                .rounded_full()
                                .bg(rgb(ui_theme::purple())),
                        )
                        .child(m)
                }))
                .child(
                    div()
                        .text_xs()
                        .font_family("monospace")
                        .text_color(rgb(ui_theme::text_faint()))
                        .child(cwd_tail),
                )
                .child(div().flex_1())
                .children(is_term.then(|| {
                    div()
                        .id("stage-split")
                        .text_xs()
                        .font_family("monospace")
                        .text_color(rgb(ui_theme::text_faint()))
                        .cursor_pointer()
                        .hover(|d| d.text_color(rgb(ui_theme::text_mid())))
                        .child("split ⌘D")
                        .on_click(move |_ev, _window, cx| {
                            e_split.update(cx, |ws, cx| ws.split_active(Axis::Horizontal, cx));
                        })
                }))
                .child(
                    // ⋯ 菜单：重命名 / 关闭会话（与会话行右键同款操作）。
                    div()
                        .id("stage-more")
                        .px_1()
                        .text_base()
                        .text_color(rgb(ui_theme::text_faint()))
                        .cursor_pointer()
                        .hover(|d| d.text_color(rgb(ui_theme::text_mid())))
                        .child("⋯")
                        .context_menu(move |menu, _window, _cx| {
                            let e_rename = e_menu.clone();
                            let e_close = e_menu.clone();
                            menu.item(PopupMenuItem::new("重命名").on_click(
                                move |_ev, window, cx| {
                                    e_rename.update(cx, |ws, cx| {
                                        let ix = ws.active_session;
                                        ws.start_rename(RenameTarget::Session(ix), window, cx);
                                    });
                                },
                            ))
                            .item(
                                PopupMenuItem::new(if multi_pane {
                                    "关闭整个会话（含全部分屏）"
                                } else {
                                    "关闭会话"
                                })
                                .on_click(
                                    move |_ev, _window, cx| {
                                        e_close.update(cx, |ws, cx| {
                                            let ix = ws.active_session;
                                            ws.close_session(ix, cx);
                                        });
                                    },
                                ),
                            )
                        }),
                ),
        )
    }

    /// 26px 终端底条：launch 名 · 相位 · 右侧快捷键提示。仅 Term 会话显示。
    pub(crate) fn render_terminal_status_bar(&mut self, cx: &mut Context<Self>) -> Option<Div> {
        let sess = self.sessions.get(self.active_session)?;
        let SessionKind::Term { active, .. } = &sess.kind else {
            return None;
        };
        let status = sess.status(cx);
        let launch = match active.read(cx).launch_kind() {
            crate::terminal_view::LaunchKind::Claude => "claude",
            crate::terminal_view::LaunchKind::Codex => "codex",
            crate::terminal_view::LaunchKind::Copilot => "copilot",
            _ => "shell",
        };
        let panes = sess.pane_count();
        Some(
            div()
                .h(px(26.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .gap_3()
                .px_4()
                .bg(rgb(ui_theme::bg_status()))
                .border_t_1()
                .border_color(rgb(ui_theme::border_dim()))
                .text_xs()
                .font_family("monospace")
                .text_color(rgb(ui_theme::text_faint()))
                .child(launch)
                .when(panes > 1, |d| d.child(format!("{panes} 分屏")))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .child(
                            div()
                                .size(px(6.))
                                .rounded_full()
                                .bg(ui_theme::session_dot_color(status)),
                        )
                        .child(phase_text(status)),
                )
                .child(div().flex_1())
                .child("split ⌘D · 关闭 ⌘W"),
        )
    }
}
