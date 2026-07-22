//! 底部状态栏（26px，mono）：⎇ 分支 · agent 状态 · 项目/阻塞计数（点击进总览）
//! · 右侧版本号（带更新红点，点击跳 GitHub）。
//!
//! 跟 file_tree.rs 同一个套路：`impl Workspace` 方法，字段仍在 main.rs。

use gpui::*;

use crate::{ui_theme, AgentStatus, MainView, Workspace};

impl Workspace {
    /// 26px 底部状态栏。
    pub(crate) fn render_status_bar(&mut self, cx: &mut Context<Self>) -> Div {
        let this = cx.entity();

        // ⎇ 当前项目分支（repo_info 缓存；拿不到就不显示）。
        let cwd = self.cur().and_then(|s| s.cwd(cx));
        let branch = cwd
            .as_ref()
            .and_then(|c| self.repo_info.get(c.as_str()))
            .and_then(|(_, i)| i.as_ref())
            .map(|i| i.branch.clone());

        // 活动会话的 agent 状态。
        let cur_status = self
            .sessions
            .get(self.active_session)
            .map(|s| s.status(cx))
            .unwrap_or(AgentStatus::Idle);
        let (agent_text, agent_color) = match cur_status {
            AgentStatus::WaitingApproval => ("agent 等你批准", ui_theme::yellow()),
            AgentStatus::NeedsAttention => ("agent 需要处理", ui_theme::yellow()),
            AgentStatus::Running => ("agent 运行中", ui_theme::blue()),
            AgentStatus::Done => ("agent 完成", ui_theme::green()),
            AgentStatus::Idle => ("agent 就绪", ui_theme::green()),
        };

        // 项目数 + 阻塞项目数（组内有等审批/需处理会话的项目）。
        let statuses: Vec<AgentStatus> = self.sessions.iter().map(|s| s.status(cx)).collect();
        let groups = self.project_groups(cx);
        let blocked = groups
            .iter()
            .filter(|(_, _, ixs)| {
                ixs.iter().any(|&ix| {
                    matches!(
                        statuses.get(ix),
                        Some(AgentStatus::WaitingApproval | AgentStatus::NeedsAttention)
                    )
                })
            })
            .count();
        let projects_text = if blocked > 0 {
            format!("◆ {} projects · {} blocked", groups.len(), blocked)
        } else {
            format!("◆ {} projects", groups.len())
        };

        // 版本号 + 更新红点（从旧侧栏 footer 迁来）。
        let has_update = self.update_available();
        let version: AnyElement = if has_update {
            gpui_component::badge::Badge::new()
                .dot()
                .child(concat!("v", env!("CARGO_PKG_VERSION")))
                .into_any_element()
        } else {
            concat!("v", env!("CARGO_PKG_VERSION")).into_any_element()
        };

        let e_overview = this.clone();
        div()
            .h(px(26.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_4()
            .px_3()
            .bg(rgb(ui_theme::bg_status()))
            .border_t_1()
            .border_color(rgb(ui_theme::border_dim()))
            .text_xs()
            .font_family("monospace")
            .text_color(rgb(ui_theme::text_muted()))
            .children(
                branch.map(|b| {
                    div().text_color(rgb(ui_theme::accent())).child(format!("⎇ {b}"))
                }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .child(div().size(px(6.)).rounded_full().bg(rgb(agent_color)))
                    .child(div().text_color(rgb(agent_color)).child(agent_text)),
            )
            .child(
                // 项目/阻塞计数：点击盖出会话总览（旧「总览」页入口落位在这）。
                div()
                    .id("status-projects")
                    .text_color(if blocked > 0 {
                        rgb(ui_theme::yellow())
                    } else {
                        rgb(ui_theme::text_muted())
                    })
                    .cursor_pointer()
                    .hover(|d| d.text_color(rgb(ui_theme::text_bright())))
                    .child(projects_text)
                    .on_click(move |_ev, window, cx| {
                        e_overview.update(cx, |ws, cx| {
                            ws.refresh_git(cx); // 进总览 → 后台刷新 git 卡片信息
                            ws.set_stage_override(Some(MainView::Overview), window, cx);
                        });
                    }),
            )
            .child(div().flex_1())
            .child(
                div()
                    .id("status-version")
                    .cursor_pointer()
                    .hover(|d| d.text_color(rgb(ui_theme::text_mid())))
                    .child(version)
                    .on_mouse_down(MouseButton::Left, move |_, _window, cx| {
                        cx.open_url(if has_update {
                            "https://github.com/smelt-ai/smelt/releases"
                        } else {
                            "https://github.com/smelt-ai/smelt"
                        });
                    }),
            )
    }
}
