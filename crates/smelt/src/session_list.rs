//! 会话列表：窗口最左的单列（280px），**按项目上下分组**——项目是分组标题行，
//! 它的会话缩进排在下面，一屏看全所有项目的所有会话（不必先切项目）。
//!
//! 设计稿原本是「64px 项目 rail + 270px 会话列」的左右两列，实测割裂：项目在
//! rail 上只剩一个字母，且必须先点中某个项目才看得到它的会话。改回单列分组。
//!
//! 分组标题行：caret（折叠）+ 项目色点 + 项目名 + 会话数 + 聚合状态点 + `+`
//! 下拉（按通道分组：终端 / 对话）；worktree 分组右键补「删除 Worktree」。
//! 会话行：类型标识（agent 紫圆 / 终端绿方）+ 名称 + 状态点 + 副标题 + 关闭，
//! 支持右键（新建任务 / 重命名）、拖拽排序、分屏子行。
//!
//! 跟 file_tree.rs 同一个套路：`impl Workspace` 方法，字段仍在 main.rs。

use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::menu::{ContextMenuExt, DropdownMenu, PopupMenuItem};
use gpui_component::*;

use crate::git_panel::main_repo_root_from_common_dir;
use crate::settings::{active_launch_entries, icon_for_launch_command, AcpAgentKind};
use crate::{
    pane_status, pane_title, ui_theme, AgentStatus, MainView, RenameTarget, SessionDrag,
    SessionKind, Workspace,
};

/// 会话行 hover group 名：行 `.group()` + 右端操作条 `.group_hover()` 配对，
/// 鼠标移到行才显形「拖拽 / 关闭」；选中行则常显。跟 inspector 任务卡片同一套路。
const SESS_ROW_GROUP: &str = "sess-row-hover";

/// 会话行副标题里的状态文案（与菜单栏下拉同一套口径）。
fn status_text(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::WaitingApproval => "等你批准",
        AgentStatus::NeedsAttention => "需要处理",
        AgentStatus::Running => "运行中",
        AgentStatus::Done => "已完成",
        AgentStatus::Idle => "空闲",
    }
}

impl Workspace {
    /// 当前活动项目的分组名：优先用用户点选的 `active_project`，该组已消失
    /// （会话全关了）则回退到活动会话所在组，再回退第一组。
    pub(crate) fn active_project_name(&self, cx: &App) -> Option<String> {
        let groups = self.project_groups(cx);
        if let Some(name) = &self.active_project {
            if groups.iter().any(|(n, _, _)| n == name) {
                return Some(name.clone());
            }
        }
        groups
            .iter()
            .find(|(_, _, ixs)| ixs.contains(&self.active_session))
            .or(groups.first())
            .map(|(n, _, _)| n.clone())
    }

    /// 280px 会话列表（全部项目，按项目分组）。
    pub(crate) fn render_session_list(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let active = self.active_session;
        let can_close = self.sessions.len() > 1;
        let this = cx.entity();
        let groups = self.project_groups(cx);
        let active_name = self.active_project_name(cx);
        let active_cwd = groups
            .iter()
            .find(|(n, _, _)| Some(n.as_str()) == active_name.as_deref())
            .and_then(|(_, cwd, _)| (!cwd.is_empty()).then(|| cwd.clone()));

        let titles: Vec<(usize, String)> =
            self.sessions.iter().enumerate().map(|(ix, s)| (ix, s.title(cx))).collect();
        let statuses: Vec<AgentStatus> = self.sessions.iter().map(|s| s.status(cx)).collect();
        let entity_ids: Vec<EntityId> = self.sessions.iter().map(|s| s.anchor_id()).collect();

        // ---- 头部：SESSIONS · 总数 + 新建入口 + 历史 ----
        let e_acp = this.clone();
        let e_term = this.clone();
        let e_hist = this.clone();
        let cwd_acp = active_cwd.clone();
        let cwd_term = active_cwd.clone();
        let header = div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_between()
            .px_3()
            .pt_3()
            .pb_2()
            .child(
                div()
                    .text_xs()
                    .font_semibold()
                    .text_color(rgb(ui_theme::text_faint()))
                    .child(format!("SESSIONS · {}", self.sessions.len())),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2p5()
                    .child(
                        // 「+Agent」：接哪家 agent 由下拉选（Claude / Copilot /
                        // Codex 都走 ACP 同一条通道）。用 Button 而不是 div——
                        // dropdown_menu 只对 Button 实现，外观靠 ghost + 覆盖
                        // text_color 贴回原来的纯文字样式。
                        Button::new("sess-new-agent")
                            .ghost()
                            .xsmall()
                            .label("对话")
                            .text_xs()
                            .font_semibold()
                            .text_color(rgb(ui_theme::purple()))
                            .dropdown_menu(move |mut menu, _window, _cx| {
                                for agent in AcpAgentKind::ALL {
                                    let e_acp = e_acp.clone();
                                    let cwd_acp = cwd_acp.clone();
                                    menu = menu.item(
                                        PopupMenuItem::new(agent.label())
                                            .icon(IconName::Bot)
                                            .on_click(move |_ev, window, cx| {
                                                let cwd = cwd_acp.clone();
                                                e_acp.update(cx, |ws, cx| {
                                                    ws.add_acp_session(agent, cwd, window, cx)
                                                });
                                            }),
                                    );
                                }
                                menu
                            }),
                    )
                    .child(
                        div()
                            .id("sess-new-term")
                            .text_xs()
                            .font_semibold()
                            .text_color(rgb(ui_theme::green()))
                            .cursor_pointer()
                            .hover(|d| d.opacity(0.8))
                            .child("终端")
                            .on_click(move |_ev, _window, cx| {
                                let cwd = cwd_term.clone();
                                e_term.update(cx, |ws, cx| ws.add_session(cwd, cx));
                            }),
                    )
                    .child(
                        // 历史会话页入口（原顶部 TabBar 的「历史会话」标签迁到这里）。
                        div()
                            .id("sess-history")
                            .text_xs()
                            .text_color(rgb(ui_theme::text_faint()))
                            .cursor_pointer()
                            .hover(|d| d.text_color(rgb(ui_theme::text_mid())))
                            .child("历史")
                            .on_click(move |_ev, window, cx| {
                                e_hist.update(cx, |ws, cx| {
                                    ws.stage_override = Some(MainView::History);
                                    cx.notify();
                                });
                                let h = e_hist.read(cx).focus_handle.clone();
                                window.focus(&h, cx);
                            }),
                    ),
            );

        let mut rows = div()
            .id("session-rows")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .pb_2();

        for (pix, (name, cwd, ixs)) in groups.iter().enumerate() {
            // ---- 项目分组标题行 ----
            let collapsed = self.collapsed_projects.contains(name);
            let is_active_group = Some(name.as_str()) == active_name.as_deref();
            // 组内最高优先级状态（声明序即优先级）当聚合状态点；折叠时尤其有用。
            let agg = ixs
                .iter()
                .filter_map(|&i| statuses.get(i).copied())
                .min_by_key(|s| s.rank())
                .unwrap_or(AgentStatus::Idle);

            let repo_info_here = self.repo_info.get(cwd.as_str()).and_then(|(_, i)| i.clone());
            let is_worktree_group = repo_info_here.as_ref().is_some_and(|i| i.is_worktree());
            let worktree_main_root = repo_info_here
                .as_ref()
                .and_then(|i| main_repo_root_from_common_dir(&i.common_dir))
                .unwrap_or_else(|| cwd.clone());
            let worktree_branch =
                repo_info_here.as_ref().map(|i| i.branch.clone()).unwrap_or_default();
            // 分支名：worktree / 普通仓库都显示，跟项目名并排（淡色）。
            let branch_label = repo_info_here.as_ref().map(|i| i.branch.clone());

            let e_toggle = this.clone();
            let toggle_name = name.clone();
            let e_menu = this.clone();
            let menu_cwd = cwd.clone();
            let group_name: SharedString = name.clone().into();

            rows = rows.child(
                div()
                    .id(("proj-group", pix))
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .mt_1p5()
                    .px_3()
                    .py(px(4.))
                    // 通栏满宽色带 + 下沿细线：读作「区段分隔」而不是「又一行内容」。
                    // 内嵌小圆角块跟会话行太像，这才是层级最强的信号（且不动字号）。
                    // 活动项目的色带亮一档：+Agent/+Term 就是新建到这个项目里，
                    // 得看得出是哪个。
                    .bg(ui_theme::tint(0xffffff, if is_active_group { 0x1e } else { 0x12 }))
                    .border_b_1()
                    .border_color(rgb(ui_theme::border_dim()))
                    .cursor_pointer()
                    .hover(|d| d.bg(ui_theme::tint(0xffffff, 0x1c)))
                    .child(
                        div()
                            .w(px(10.))
                            .flex_shrink_0()
                            .text_size(px(9.))
                            .text_color(rgb(ui_theme::text_faint()))
                            .child(if collapsed { "▸" } else { "▾" }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            // 基线对齐：项目名 12.5px、分支名 10px，居中对齐会让
                            // 小字看起来往上飘，对齐基线才是一条线上的。
                            .items_baseline()
                            .gap_1p5()
                            .overflow_hidden()
                            .child(
                                // 项目名跟会话名同字号（像文件树里文件夹和文件那样）：
                                // 层级靠色带 + 字重 + 缩进引导线立住，不靠压小字号
                                // ——压小只会让项目名难读。
                                div()
                                    .flex_shrink_0()
                                    .text_size(px(14.))
                                    .font_semibold()
                                    .text_color(rgb(ui_theme::text_bright()))
                                    .child(group_name.clone()),
                            )
                            .children(branch_label.map(|b| {
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(11.))
                                    .font_family("monospace")
                                    .text_color(rgb(ui_theme::text_faint()))
                                    .child(b)
                            })),
                    )
                    // 折叠时把组内状态收成一个点，展开时数量足够
                    .when(collapsed && agg != AgentStatus::Idle, |d| {
                        d.child(
                            div()
                                .flex_shrink_0()
                                .size(px(6.))
                                .rounded_full()
                                .bg(ui_theme::session_dot_color(agg)),
                        )
                    })
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(px(11.))
                            .text_color(rgb(ui_theme::text_faint()))
                            .child(ixs.len().to_string()),
                    )
                    .child(
                        // 「+」下拉：在本项目里新建（终端通道 / 对话通道）
                        Button::new(("proj-new", pix))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Plus)
                            .dropdown_menu({
                                let e_menu = e_menu.clone();
                                let menu_cwd = menu_cwd.clone();
                                move |menu, _window, cx| {
                                    let cwd_opt = (!menu_cwd.is_empty()).then(|| menu_cwd.clone());
                                    let entries = active_launch_entries(cx);
                                    let e_term = e_menu.clone();
                                    let cwd_new = cwd_opt.clone();
                                    // 按「通道」分组：同一个 agent 既能跑在终端里
                                    // （它自带的 TUI），也能接进 smelt 原生界面对话。
                                    // 分组标题把差别说清，菜单项就不用背长名字了。
                                    let mut menu = menu
                                        .item(PopupMenuItem::label("终端 · agent 自带 TUI"))
                                        .item(
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
                                        let cwd_launch = cwd_opt.clone();
                                        let e_launch = e_menu.clone();
                                        let icon = icon_for_launch_command(&command);
                                        menu = menu.item(
                                            PopupMenuItem::new(label.clone()).icon(icon).on_click(
                                                move |_ev, _window, cx| {
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
                                                },
                                            ),
                                        );
                                    }
                                    menu = menu
                                        .separator()
                                        .item(PopupMenuItem::label("对话 · smelt 原生界面"));
                                    // 三家 agent 走同一条 ACP 通道，菜单项从枚举派生：
                                    // 加一家 agent 不用回来改这段。
                                    for agent in AcpAgentKind::ALL {
                                        let e_acp = e_menu.clone();
                                        let cwd_acp = cwd_opt.clone();
                                        menu = menu.item(
                                            PopupMenuItem::new(agent.label())
                                                .icon(IconName::Bot)
                                                .on_click(move |_ev, window, cx| {
                                                    let cwd = cwd_acp.clone();
                                                    e_acp.update(cx, |ws, cx| {
                                                        ws.add_acp_session(agent, cwd, window, cx)
                                                    });
                                                }),
                                        );
                                    }
                                    menu
                                }
                            }),
                    )
                    .on_click(move |_ev, _window, cx| {
                        let name = toggle_name.clone();
                        e_toggle.update(cx, |ws, cx| {
                            if !ws.collapsed_projects.remove(&name) {
                                ws.collapsed_projects.insert(name);
                            }
                            cx.notify();
                        });
                    })
                    .context_menu({
                        // 复制路径人人有份；「删除 Worktree」只给 worktree 分组
                        //（`when` 的两个分支类型必须一致，所以是菜单项按条件加，
                        // 不是整个 context_menu 按条件挂）。
                        let e_del = e_menu.clone();
                        let e_pin = e_menu.clone();
                        let path = cwd.clone();
                        let del_main_root = worktree_main_root.clone();
                        let del_branch = worktree_branch.clone();
                        move |menu, _window, cx| {
                            let copy_path = path.clone();
                            let del_path = path.clone();
                            let pin_path = path.clone();
                            let e_del = e_del.clone();
                            let e_pin = e_pin.clone();
                            let del_main_root = del_main_root.clone();
                            let del_branch = del_branch.clone();
                            // 已 pin → 显示「从文件树移除」，否则「加到文件树」（当前活动项目
                            // 天然在文件树里，pin 它=切走后仍保留，所以照样给这个开关）。
                            let pinned = e_pin.read(cx).is_file_tree_root_pinned(&pin_path);
                            let pin_label = if pinned { "从文件树移除" } else { "加到文件树" };
                            menu.item(PopupMenuItem::new("复制项目路径").on_click(
                                move |_ev, _window, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        copy_path.clone(),
                                    ));
                                },
                            ))
                            .item(PopupMenuItem::new(pin_label).icon(IconName::Folder).on_click(
                                move |_ev, _window, cx| {
                                    let pin_path = pin_path.clone();
                                    e_pin.update(cx, |ws, cx| ws.toggle_file_tree_root(pin_path, cx));
                                },
                            ))
                            .when(is_worktree_group, move |menu| {
                                menu.separator().item(
                                    PopupMenuItem::new("删除 Worktree")
                                        .icon(IconName::Delete)
                                        .on_click(move |_ev, _window, cx| {
                                            let del_path = del_path.clone();
                                            let del_main_root = del_main_root.clone();
                                            let del_branch = del_branch.clone();
                                            e_del.update(cx, |ws, cx| {
                                                ws.start_delete_worktree(
                                                    del_path,
                                                    del_main_root,
                                                    del_branch,
                                                    cx,
                                                )
                                            });
                                        }),
                                )
                            })
                        }
                    }),
            );

            if collapsed {
                continue;
            }

            // ---- 组内会话行 ----
            // 装进一个带左侧引导线的容器：缩进 + 竖线让「这些会话属于上面那个
            // 项目」一眼成立，不靠读缩进像素差。
            let mut group_body = div()
                .flex()
                .flex_col()
                .gap(px(1.))
                .ml(px(17.))
                .pl(px(10.))
                .border_l_1()
                .border_color(rgb(ui_theme::border_dim()));
            for &ix in ixs {
                let title = titles.get(ix).map(|(_, t)| t.clone()).unwrap_or_default();
                let status = statuses.get(ix).copied().unwrap_or(AgentStatus::Idle);
                let is_active = ix == active;
                let entity_id = entity_ids[ix];
                let is_acp = matches!(self.sessions[ix].kind, SessionKind::Acp(_));
                // 单行行高：副标题只保留「有增量信息」的部分——分屏数。
                // agent 名（claude-agent-acp）已由紫色类型点表达，状态由状态点表达。
                let subtitle = match &self.sessions[ix].kind {
                    SessionKind::Acp(_) => None,
                    SessionKind::Term { .. } => {
                        let n = self.sessions[ix].pane_count();
                        (n > 1).then(|| format!("⑂{n}"))
                    }
                };
                // 「要人管」的状态才配文字（等你批准 / 需要处理）。
                let attention_label = matches!(
                    status,
                    AgentStatus::WaitingApproval | AgentStatus::NeedsAttention
                )
                .then(|| status_text(status));
                let hint_before = self.sess_drop_hint == Some((entity_id, true));
                let hint_after = self.sess_drop_hint == Some((entity_id, false));
                let e_act = this.clone();
                let e_close = this.clone();
                let e_rename = this.clone();
                let e_drop = this.clone();
                let drag_title: SharedString = title.clone().into();
                // 分屏组（无父行）要复用「拖拽排序 / 关闭整会话」，但这俩会被下面的
                // 父行 on_drag / close 按钮吃掉所有权，先给分屏分支留一份克隆。
                let e_close_grp = this.clone();
                let drag_title_grp = drag_title.clone();

                // 类型标识：agent 紫圆点 / 终端绿方块。
                // 会话标记一律圆点（项目是方块），颜色区分类型：紫 = agent
                // 消息流、绿 = 终端。形状管层级、颜色管类型，各司其职。
                let type_dot: AnyElement = div()
                    .size(px(7.))
                    .rounded_full()
                    .bg(rgb(if is_acp { ui_theme::purple() } else { ui_theme::green() }))
                    .into_any_element();

                let dragging = cx.has_active_drag();
                let make_hint = |before: bool, e_hint: Entity<Workspace>| {
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
                        .right(px(4.))
                        .h(px(5.))
                        .rounded(px(2.5))
                        .bg(rgb(ui_theme::blue()))
                        .map(|d| if at_top { d.top(px(-3.)) } else { d.bottom(px(-3.)) })
                        .with_animation(
                            anim_id,
                            Animation::new(std::time::Duration::from_millis(160))
                                .with_easing(ease_out_quint()),
                            |this, delta| this.opacity(0.4 + 0.6 * delta).w(relative(delta)),
                        )
                };

                let row = div()
                    .id(("sess-row", ix))
                    .group(SESS_ROW_GROUP)
                    .relative()
                    .flex()
                    .items_center()
                    .gap_2()
                    .pl_2()
                    .pr_3()
                    .py(px(2.))
                    .rounded(px(6.))
                    .cursor_pointer()
                    // Discord 选中态：微亮底 + 左侧白色圆角竖条(pill)，不再用 blurple
                    // 描边。常态透明、hover 微亮；pill 只在选中时出（下面 absolute 子元素）。
                    .map(|d| {
                        if is_active {
                            d.bg(rgb(ui_theme::bg_selected()))
                        } else {
                            d.hover(|d| d.bg(rgb(ui_theme::bg_hover())))
                        }
                    })
                    // 选中 pill：贴行左缘的白色圆角竖条，比行矮、上下 inset 留边居中。
                    // 深色近白 / 浅色近黑，text_bright 天然满足。
                    .when(is_active, |row| {
                        row.child(
                            div()
                                .absolute()
                                .left(px(-6.))
                                .top(px(4.))
                                .bottom(px(4.))
                                .w(px(3.))
                                .rounded_full()
                                .bg(rgb(ui_theme::text_bright())),
                        )
                    })
                    .child(div().flex_shrink_0().child(type_dot))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(14.))
                            // 选中行标题提亮到 bright（Discord 选中频道名变白）。
                            .text_color(rgb(if is_active {
                                ui_theme::text_bright()
                            } else {
                                ui_theme::text_mid()
                            }))
                            .truncate()
                            .child(title.clone()),
                    )
                    // 分屏数：单行行高下用小角标带出来，别为它多占一行
                    .children(subtitle.map(|s| {
                        div()
                            .flex_shrink_0()
                            .text_size(px(11.))
                            .font_family("monospace")
                            .text_color(rgb(ui_theme::text_faint()))
                            .child(s)
                    }))
                    // 状态文字只在「要人管」时才出（空闲/运行中靠状态点表达就够，
                    // 每行都写一遍「空闲」等于用一整行高度说一句废话）
                    .children(attention_label.map(|label| {
                        div()
                            .flex_shrink_0()
                            .text_size(px(11.))
                            .text_color(ui_theme::session_dot_color(status))
                            .child(label)
                    }))
                    .child(
                        div()
                            .flex_shrink_0()
                            .size(px(6.))
                            .rounded_full()
                            .bg(ui_theme::session_dot_color(status)),
                    )
                    .child(
                        // 右端：拖拽手柄 + 关闭。常态透明、hover 或该行选中才显形
                        //（Discord 频道行那样常态极简、操作藏起来）。group 名见行 `.group()`。
                        div()
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .when(!is_active, |d| d.opacity(0.0))
                            .group_hover(SESS_ROW_GROUP, |s| s.opacity(1.0))
                            .child(
                                div()
                                    .id(("sess-drag", ix))
                                    .w(px(14.))
                                    .h(px(18.))
                                    .cursor_grab()
                                    .on_drag(SessionDrag { id: entity_id, title: drag_title }, {
                                        let e_clear = e_drop.clone();
                                        move |drag, _, _, cx| {
                                            e_clear.update(cx, |ws, _| ws.sess_drop_hint = None);
                                            cx.new(|_| drag.clone())
                                        }
                                    }),
                            )
                            .children(can_close.then(|| {
                                Button::new(("close-session", ix))
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::CircleX)
                                    .on_click(move |_ev, _w, cx| {
                                        cx.stop_propagation();
                                        e_close.update(cx, |ws, cx| ws.close_session(ix, cx));
                                    })
                            })),
                    )
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
                                    if let Some(pane) = ws
                                        .sessions
                                        .get(sess_ix)
                                        .and_then(|s| s.active_term().cloned())
                                    {
                                        ws.open_new_task_for_terminal(&pane, window, cx);
                                    }
                                });
                            },
                        ))
                        .item(PopupMenuItem::new("重命名").on_click(move |_ev, window, cx| {
                            e_rename.update(cx, |ws, cx| {
                                ws.start_rename(RenameTarget::Session(ix), window, cx)
                            });
                        }))
                    })
                    .when(dragging, |row| {
                        // 拖拽进行中才渲染整行 drop 接收层：上半段插到目标前、下半段插到后。
                        let e_before = e_drop.clone();
                        let e_after = e_drop.clone();
                        row.child(
                            div()
                                .absolute()
                                .inset_0()
                                .child(
                                    div()
                                        .id(("sess-drop-before", ix))
                                        .absolute()
                                        .top_0()
                                        .left_0()
                                        .right_0()
                                        .h_1_2()
                                        .on_drag_move(make_hint(true, e_before.clone()))
                                        .on_drop(move |drag: &SessionDrag, _window, cx| {
                                            let dragged = drag.id;
                                            e_before.update(cx, |ws, cx| {
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
                                        .on_drag_move(make_hint(false, e_after.clone()))
                                        .on_drop(move |drag: &SessionDrag, _window, cx| {
                                            let dragged = drag.id;
                                            e_after.update(cx, |ws, cx| {
                                                ws.sess_drop_hint = None;
                                                ws.move_session_near(dragged, entity_id, false, cx)
                                            });
                                        }),
                                ),
                        )
                        .when(hint_before, |row| row.child(indicator(("sess-ind-b", ix), true)))
                        .when(hint_after, |row| row.child(indicator(("sess-ind-a", ix), false)))
                    });
                // 单 pane 会话：整会话就是这一行（上面构建的 row）。
                // 分屏会话：不显示会话名父行；把同一 tab 的多个 pane 用内层括线
                // 圈在一起平铺，会话级操作（拖拽排序 / 关闭整会话 / drop 提示）
                // 挂到 pane 组容器上，pane 行本身只管「切到该 pane」。
                if self.sessions[ix].pane_count() <= 1 {
                    group_body = group_body.child(row);
                } else {
                    let leaves = self.sessions[ix].term_leaves();
                    let active_pane_id = self.sessions[ix].anchor_id();
                    // 组内重名的 pane 标题补序号，避免「smelt 里又一个 smelt」。
                    let raw_titles: Vec<String> =
                        leaves.iter().map(|v| pane_title(v, cx).to_string()).collect();

                    // 内层括线：比项目引导线再内缩一档，左侧圆角竖线把同一 tab 的
                    // 几个 pane 圈成一组（视觉上 ≈ ╭…╰ 括号）。
                    // pane 行跟普通会话行同缩进、同字号，不再内缩变小；「成组」只由
                    // 左侧一条括线表达（见下方 pane_group 的 absolute 竖线）。
                    let mut pane_rows = div().flex().flex_col().gap(px(1.));
                    for (lix, view) in leaves.into_iter().enumerate() {
                        let base = raw_titles[lix].clone();
                        let dup = raw_titles.iter().filter(|t| **t == base).count() > 1;
                        let p_title = if dup {
                            format!("{} · {}", base, lix + 1)
                        } else {
                            base
                        };
                        let p_status = pane_status(&view, cx);
                        let is_current_view = ix == active && view.entity_id() == active_pane_id;
                        let e_pane_act = this.clone();
                        let e_pane_menu = this.clone();
                        let pane = view.clone();
                        let menu_pane = view.clone();
                        pane_rows = pane_rows.child(
                            div()
                                .id(("sess-pane-row", ix * 100 + lix))
                                .flex()
                                .items_center()
                                .gap_2()
                                .px_2()
                                .py(px(2.))
                                .rounded(px(6.))
                                .cursor_pointer()
                                .map(|d| {
                                    if is_current_view {
                                        d.bg(rgb(ui_theme::bg_selected()))
                                    } else {
                                        d.hover(|d| d.bg(rgb(ui_theme::bg_hover())))
                                    }
                                })
                                .child(
                                    div()
                                        .flex_shrink_0()
                                        .size(px(6.))
                                        .rounded_full()
                                        .bg(ui_theme::session_dot_color(p_status)),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .text_size(px(14.))
                                        // 当前 pane 提亮，其余用中灰——和会话行选中态同口径。
                                        .text_color(rgb(if is_current_view {
                                            ui_theme::text_bright()
                                        } else {
                                            ui_theme::text_mid()
                                        }))
                                        .truncate()
                                        .child(p_title),
                                )
                                .on_click(move |_ev, window, cx| {
                                    let pane = pane.clone();
                                    e_pane_act.update(cx, |ws, cx| {
                                        ws.activate_session_pane(ix, pane, window, cx)
                                    });
                                })
                                .context_menu(move |menu, _window, _cx| {
                                    let e_task = e_pane_menu.clone();
                                    let e_rename = e_pane_menu.clone();
                                    let task_pane = menu_pane.clone();
                                    let rename_pane = menu_pane.clone();
                                    menu.item(PopupMenuItem::new("新建任务").on_click(
                                        move |_ev, window, cx| {
                                            let pane = task_pane.clone();
                                            e_task.update(cx, |ws, cx| {
                                                ws.open_new_task_for_terminal(&pane, window, cx);
                                            });
                                        },
                                    ))
                                    .item(PopupMenuItem::new("重命名").on_click(
                                        move |_ev, window, cx| {
                                            let target = RenameTarget::Pane(rename_pane.clone());
                                            e_rename.update(cx, |ws, cx| {
                                                ws.start_rename(target, window, cx)
                                            });
                                        },
                                    ))
                                }),
                        );
                    }

                    // 组容器：relative + group 名，让操作条 hover 才显形；承载 drop 层。
                    let mut pane_group = div()
                        .id(("sess-pane-group", ix))
                        .group(SESS_ROW_GROUP)
                        .relative()
                        // 成组的唯一标记：贴左缘一条圆角竖线，把这几个 pane 圈在一起，
                        // 不挤占内容宽度（pane 行仍与普通会话行左对齐、同字号）。
                        .child(
                            div()
                                .absolute()
                                .left(px(1.))
                                .top(px(2.))
                                .bottom(px(2.))
                                .w(px(9.))
                                // 圆弧括号 ╭…╰：只描左/上/下三条边 + 整体圆角，右边开口，
                                // 顶底两个圆角拐角把这组 pane「抱」起来（不挤占内容宽度）。
                                .border_l_1()
                                .border_t_1()
                                .border_b_1()
                                .border_color(rgb(ui_theme::border_loud()))
                                .rounded(px(7.)),
                        )
                        .child(pane_rows);

                    // 会话级操作条：拖拽手柄（整会话排序）+ 关闭整会话，贴组右上角。
                    let ops = div()
                        .absolute()
                        .top(px(2.))
                        .right(px(6.))
                        .flex()
                        .items_center()
                        .opacity(0.0)
                        .group_hover(SESS_ROW_GROUP, |s| s.opacity(1.0))
                        .child(
                            div()
                                .id(("sess-grp-drag", ix))
                                .w(px(14.))
                                .h(px(18.))
                                .cursor_grab()
                                .on_drag(
                                    SessionDrag { id: entity_id, title: drag_title_grp },
                                    {
                                        let e_clear = e_drop.clone();
                                        move |drag, _, _, cx| {
                                            e_clear.update(cx, |ws, _| ws.sess_drop_hint = None);
                                            cx.new(|_| drag.clone())
                                        }
                                    },
                                ),
                        )
                        .children(can_close.then(|| {
                            Button::new(("close-session-grp", ix))
                                .ghost()
                                .xsmall()
                                .icon(IconName::CircleX)
                                .on_click(move |_ev, _w, cx| {
                                    cx.stop_propagation();
                                    e_close_grp.update(cx, |ws, cx| ws.close_session(ix, cx));
                                })
                        }));
                    pane_group = pane_group.child(ops);

                    // drop 接收层：拖别的会话到这组前/后（沿用父行那套，键在会话 entity_id）。
                    if dragging {
                        let e_before = e_drop.clone();
                        let e_after = e_drop.clone();
                        pane_group = pane_group.child(
                            div()
                                .absolute()
                                .inset_0()
                                .child(
                                    div()
                                        .id(("sess-grp-drop-before", ix))
                                        .absolute()
                                        .top_0()
                                        .left_0()
                                        .right_0()
                                        .h_1_2()
                                        .on_drag_move(make_hint(true, e_before.clone()))
                                        .on_drop(move |drag: &SessionDrag, _window, cx| {
                                            let dragged = drag.id;
                                            e_before.update(cx, |ws, cx| {
                                                ws.sess_drop_hint = None;
                                                ws.move_session_near(dragged, entity_id, true, cx)
                                            });
                                        }),
                                )
                                .child(
                                    div()
                                        .id(("sess-grp-drop-after", ix))
                                        .absolute()
                                        .bottom_0()
                                        .left_0()
                                        .right_0()
                                        .h_1_2()
                                        .on_drag_move(make_hint(false, e_after.clone()))
                                        .on_drop(move |drag: &SessionDrag, _window, cx| {
                                            let dragged = drag.id;
                                            e_after.update(cx, |ws, cx| {
                                                ws.sess_drop_hint = None;
                                                ws.move_session_near(dragged, entity_id, false, cx)
                                            });
                                        }),
                                ),
                        );
                    }
                    if hint_before {
                        pane_group = pane_group.child(indicator(("sess-ind-b", ix), true));
                    }
                    if hint_after {
                        pane_group = pane_group.child(indicator(("sess-ind-a", ix), false));
                    }

                    group_body = group_body.child(pane_group);
                }
            }
            rows = rows.child(group_body);
        }

        // ---- 底部：打开项目 / 临时终端（原项目 rail 底部的「+」）----
        let e_open = this.clone();
        let e_scratch = this.clone();
        let footer = div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_3()
            .px_3()
            .py_1p5()
            .border_t_1()
            .border_color(rgb(ui_theme::border_dim()))
            .child(
                div()
                    .id("open-project")
                    .text_xs()
                    .text_color(rgb(ui_theme::text_muted()))
                    .cursor_pointer()
                    .hover(|d| d.text_color(rgb(ui_theme::text_bright())))
                    .child("+ 打开项目")
                    .on_click(move |_ev, _window, cx| {
                        e_open.update(cx, |ws, cx| ws.open_project(cx));
                    }),
            )
            .child(
                div()
                    .id("scratch-terminal")
                    .text_xs()
                    .text_color(rgb(ui_theme::text_faint()))
                    .cursor_pointer()
                    .hover(|d| d.text_color(rgb(ui_theme::text_mid())))
                    .child("临时终端")
                    .on_click(move |_ev, _window, cx| {
                        e_scratch.update(cx, |ws, cx| ws.new_scratch_session(cx));
                    }),
            );

        div()
            .w(px(280.))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(rgb(ui_theme::bg_elev()))
            .border_r_1()
            .border_color(rgb(ui_theme::border_dim()))
            .child(header)
            .child(rows)
            .child(footer)
    }
}
