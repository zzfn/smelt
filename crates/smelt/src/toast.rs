//! 阻塞 toast：右上角浮层（322px），会话进入「等你批准 / 需要处理」时弹出。
//! 派生态而非事件流——render 每帧从 `Session::status` 重算，不存在丢事件；
//! ✕ / Snooze 记在 `toast_dismissed` / `toast_snoozed`，状态解除自动清除，
//! 同一会话再次阻塞会重新弹。
//!
//! 跟 file_tree.rs 同一个套路：`impl Workspace` 方法，字段仍在 main.rs。

use std::time::{Duration, Instant};

use gpui::*;
use gpui_component::*;

use crate::{ui_theme, AgentStatus, Workspace};

/// Snooze 时长：10 分钟。
const SNOOZE: Duration = Duration::from_secs(600);

impl Workspace {
    /// 右上角阻塞 toast 浮层；没有需要展示的就返回 None。
    pub(crate) fn render_blocked_toasts(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let now = Instant::now();
        let statuses: Vec<AgentStatus> = self.sessions.iter().map(|s| s.status(cx)).collect();

        // 状态解除的会话从 dismissed/snoozed 里清掉（下次再阻塞要重新弹）。
        let blocked_ids: std::collections::HashSet<EntityId> = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(ix, _)| {
                matches!(
                    statuses.get(*ix),
                    Some(AgentStatus::WaitingApproval | AgentStatus::NeedsAttention)
                )
            })
            .map(|(_, s)| s.anchor_id())
            .collect();
        self.toast_dismissed.retain(|id| blocked_ids.contains(id));
        self.toast_snoozed.retain(|id, _| blocked_ids.contains(id));

        // 终端通道的具体通知文案（有就用，没有回退到状态短语）。
        let notif_msgs = self.collect_notifications(cx);

        let mut items: Vec<(usize, EntityId, String, String)> = Vec::new();
        for (ix, s) in self.sessions.iter().enumerate() {
            let st = statuses.get(ix).copied().unwrap_or(AgentStatus::Idle);
            if !matches!(st, AgentStatus::WaitingApproval | AgentStatus::NeedsAttention) {
                continue;
            }
            let id = s.anchor_id();
            if self.toast_dismissed.contains(&id) {
                continue;
            }
            if self.toast_snoozed.get(&id).is_some_and(|t| now < *t) {
                continue;
            }
            let msg = notif_msgs
                .iter()
                .find(|(si, _, _)| *si == ix)
                .map(|(_, _, m)| m.clone())
                .unwrap_or_else(|| match st {
                    AgentStatus::WaitingApproval => "Agent 暂停中——需要你批准才能继续。".to_string(),
                    _ => "Agent 在等你处理。".to_string(),
                });
            items.push((ix, id, s.title(cx), msg));
        }
        if items.is_empty() {
            return None;
        }

        let this = cx.entity();
        // 右下角：贴着状态栏上方堆叠（状态栏 26px，再留 10px 呼吸）。
        // 放右上会压住 inspector 面板头和文件树顶部，那正是要看的内容。
        let mut stack = div()
            .absolute()
            .bottom(px(36.))
            .right(px(16.))
            .w(px(322.))
            .flex()
            .flex_col()
            .gap_2();
        // 最多同时叠 3 条，再多只会全屏都是卡片。
        for (ix, id, name, msg) in items.into_iter().take(3) {
            let e_review = this.clone();
            let e_snooze = this.clone();
            let e_dismiss = this.clone();
            stack = stack.child(
                div()
                    .id(("toast-card", id))
                    // 浮层卡片必须 occlude：不然除按钮外的区域鼠标会穿透到
                    // 下层（inspector/文件树跟着 hover、点击直接落进去）。
                    .occlude()
                    .rounded(px(10.))
                    .bg(rgb(ui_theme::bg_hover()))
                    .border_1()
                    .border_color(rgb(ui_theme::border_focus()))
                    .border_l_2()
                    .shadow_lg()
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().size(px(8.)).rounded_full().bg(rgb(ui_theme::yellow())))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_xs()
                                    .font_semibold()
                                    .text_color(rgb(ui_theme::yellow()))
                                    .truncate()
                                    .child(format!("Blocked · {name}")),
                            )
                            .child(
                                div()
                                    .id(("toast-dismiss", id))
                                    .text_xs()
                                    .text_color(rgb(ui_theme::text_faint()))
                                    .cursor_pointer()
                                    .hover(|d| d.text_color(rgb(ui_theme::text_bright())))
                                    .child("✕")
                                    .on_click(move |_ev, _window, cx| {
                                        e_dismiss.update(cx, |ws, cx| {
                                            ws.toast_dismissed.insert(id);
                                            cx.notify();
                                        });
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .line_height(px(18.))
                            .text_color(rgb(ui_theme::text()))
                            .child(msg),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(
                                div()
                                    .id(("toast-review", id))
                                    .px_3()
                                    .py_1()
                                    .rounded(px(6.))
                                    .bg(rgb(ui_theme::yellow()))
                                    .text_xs()
                                    .font_semibold()
                                    .text_color(rgb(ui_theme::on_accent()))
                                    .cursor_pointer()
                                    .hover(|d| d.opacity(0.9))
                                    .child("查看")
                                    .on_click(move |_ev, window, cx| {
                                        e_review.update(cx, |ws, cx| {
                                            ws.activate(ix, window, cx);
                                        });
                                    }),
                            )
                            .child(
                                div()
                                    .id(("toast-snooze", id))
                                    .px_3()
                                    .py_1()
                                    .rounded(px(6.))
                                    .border_1()
                                    .border_color(rgb(ui_theme::border_focus()))
                                    .text_xs()
                                    .text_color(rgb(ui_theme::text_mid()))
                                    .cursor_pointer()
                                    .hover(|d| d.opacity(0.85))
                                    .child("稍后")
                                    .on_click(move |_ev, _window, cx| {
                                        e_snooze.update(cx, |ws, cx| {
                                            ws.toast_snoozed.insert(id, Instant::now() + SNOOZE);
                                            cx.notify();
                                        });
                                    }),
                            ),
                    ),
            );
        }
        Some(stack.into_any_element())
    }
}
