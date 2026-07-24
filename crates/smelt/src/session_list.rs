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
use crate::settings::{AcpAgentKind, active_launch_entries, icon_for_launch_command};
use crate::{
    AgentStatus, MainView, RenameTarget, SessionDrag, SessionKind, Workspace, pane_status,
    pane_title, ui_theme,
};

/// 会话行 hover group 名：行 `.group()` + 右端操作条 `.group_hover()` 配对，
/// 鼠标移到行才显形「拖拽 / 关闭」；选中行则常显。跟 inspector 任务卡片同一套路。
const SESS_ROW_GROUP: &str = "sess-row-hover";

/// 分屏 pane 行自己的 hover group 名：每行右端的「关掉这个 pane」按钮靠它显形。
/// 必须跟 SESS_ROW_GROUP 分开——共用一个名字的话 hover 组内任意位置，所有 pane 行的
/// × 会一起亮，用户分不清点下去关的是哪一个（正是「关一个 pane 结果两个都没了」的来源）。
const PANE_ROW_GROUP: &str = "sess-pane-row-hover";

/// 项目分组标题行的 hover group 名：整行 `.group()` + 左端 chevron `.group_hover()`。
/// 学 Discord 的 category header——chevron 平时是次级灰（不抢项目名），鼠标压上整行
/// 才提亮到白，明确告诉用户「这一行本身可点，点了折叠」。
const PROJ_HEADER_GROUP: &str = "proj-header-hover";

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
    /// 当前活动项目的 **root 路径**：优先用用户点选的 `active_project`，该项目已被关掉
    /// 则回退到活动会话所在组，再回退第一组。
    pub(crate) fn active_project_root(&self, cx: &App) -> Option<String> {
        let groups = self.project_groups(cx);
        if let Some(root) = &self.active_project {
            if groups.iter().any(|g| g.root == *root) {
                return Some(root.clone());
            }
        }
        groups
            .iter()
            .find(|g| g.sessions.contains(&self.active_session))
            .or(groups.first())
            .map(|g| g.root.clone())
    }

    /// 280px 会话列表（全部项目，按项目分组）。
    pub(crate) fn render_session_list(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let active = self.active_session;
        // 注：项目实体化后允许关到一个会话都不剩（侧栏还有项目行撑着，舞台落到引导页），
        // 所以关闭键不再有「最后一个不许关」那道门槛，全都常显。
        let this = cx.entity();
        let groups = self.project_groups(cx);
        let active_root = self.active_project_root(cx);

        let titles: Vec<(usize, String)> = self
            .sessions
            .iter()
            .enumerate()
            .map(|(ix, s)| (ix, s.title(cx)))
            .collect();
        let statuses: Vec<AgentStatus> = self.sessions.iter().map(|s| s.status(cx)).collect();
        let entity_ids: Vec<EntityId> = self.sessions.iter().map(|s| s.anchor_id()).collect();

        // ---- 头部：SESSIONS · 总数 + 历史入口 ----
        // 新建入口全撤：建会话一律走「项目行 hover 出的 +」（落到那个项目），不属于任何
        // 项目的裸终端走底部「终端」。顶部只留查看类的「历史」——原来的「对话 / 终端」是
        // 落到当前项目的快捷新建，跟 + 重复，且两个彩色词太吵。
        let e_hist = this.clone();
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
            );

        let mut rows = div()
            .id("session-rows")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .pb_2();

        for (pix, group) in groups.iter().enumerate() {
            // ---- 项目分组标题行 ----
            // 分组身份一律用 root 路径（末段同名的两个目录是两个项目，见 ProjectGroup）；
            // name 只是显示用的。
            let cwd = &group.root;
            let name = &group.label;
            let ixs = &group.sessions;
            let collapsed = self.collapsed_projects.contains(cwd);
            let is_active_group = Some(cwd.as_str()) == active_root.as_deref();
            // 组内最高优先级状态（声明序即优先级）当聚合状态点；折叠时尤其有用。
            let agg = ixs
                .iter()
                .filter_map(|&i| statuses.get(i).copied())
                .min_by_key(|s| s.rank())
                .unwrap_or(AgentStatus::Idle);

            let repo_info_here = self
                .repo_info
                .get(cwd.as_str())
                .and_then(|(_, i)| i.clone());
            let is_worktree_group = repo_info_here.as_ref().is_some_and(|i| i.is_worktree());
            let worktree_main_root = repo_info_here
                .as_ref()
                .and_then(|i| main_repo_root_from_common_dir(&i.common_dir))
                .unwrap_or_else(|| cwd.clone());
            let worktree_branch = repo_info_here
                .as_ref()
                .map(|i| i.branch.clone())
                .unwrap_or_default();
            // 分支名：worktree / 普通仓库都显示，跟项目名并排（淡色）。
            let branch_label = repo_info_here.as_ref().map(|i| i.branch.clone());

            let e_toggle = this.clone();
            let toggle_root = cwd.clone();
            let e_menu = this.clone();
            let menu_cwd = cwd.clone();
            let group_name: SharedString = name.clone().into();

            rows = rows.child(
                div()
                    .id(("proj-group", pix))
                    // relative：右端的 + 浮层靠 absolute 定位到这行内。
                    .relative()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    // 组间留白：十来个项目排下来，靠这一档间距 + 项目名比会话名大一档 +
                    // group_body 的缩进引导线一起把层级立住，不靠色带。
                    .mt_4()
                    .px_3()
                    .py(px(4.))
                    .cursor_pointer()
                    .group(PROJ_HEADER_GROUP)
                    // 常态无底，hover 才浮起一层（跟会话行同一个 bg_row_hover，
                    // 「鼠标在这行」在整个侧栏是同一种说法）。
                    .hover(|d| d.bg(rgb(ui_theme::bg_row_hover())))
                    .child(
                        // 折叠指示：跟文件树同一套 chevron（展开朝下 / 收起朝右），
                        // 不再用 9px 的 Unicode 三角——那个字号太小、字体渲染还不稳，
                        // 在色带底上几乎看不出朝向，等于没有状态提示。
                        div()
                            .w(px(14.))
                            .flex_shrink_0()
                            .flex()
                            .justify_center()
                            .text_color(rgb(ui_theme::text_muted()))
                            .group_hover(PROJ_HEADER_GROUP, |s| {
                                s.text_color(rgb(ui_theme::text_bright()))
                            })
                            .child(
                                Icon::new(if collapsed {
                                    IconName::ChevronRight
                                } else {
                                    IconName::ChevronDown
                                })
                                .size(px(13.)),
                            ),
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
                                // 项目名是「分组标签」不是主体：驾驶舱日常盯的是会话（agent
                                // 状态），项目只是把会话归类的容器。所以项目名压成小灰标签
                                //（13px semibold muted），把视觉主体让给会话名——像 Discord
                                // 你盯频道、category 标题只是个小灰标签，不抢戏。
                                //
                                // 当前项目（+ 的落点）靠亮度拎出来：亮白 vs 其余的灰。
                                div()
                                    .flex_shrink_0()
                                    .text_size(px(13.))
                                    .font_semibold()
                                    .text_color(rgb(if is_active_group {
                                        ui_theme::text_bright()
                                    } else {
                                        ui_theme::text_muted()
                                    }))
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
                        // 「+」新建下拉：在本项目里新建（终端通道 / 对话通道）。
                        // absolute 浮在行右端，平时不占位（会话数因此贴到最右、无留白），
                        // hover 整行才淡入、盖在会话数上。背景取 bg_row_hover（= 项目行
                        // hover 底），无缝把下面的数字盖住。
                        div()
                            .absolute()
                            .top(px(3.))
                            .bottom(px(3.))
                            .right(px(6.))
                            .flex()
                            .items_center()
                            .pl_3()
                            .rounded(px(4.))
                            .bg(rgb(ui_theme::bg_row_hover()))
                            .opacity(0.0)
                            .group_hover(PROJ_HEADER_GROUP, |s| s.opacity(1.0))
                            .child(
                                Button::new(("proj-new", pix))
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Plus)
                                    .dropdown_menu({
                                        let e_menu = e_menu.clone();
                                        let menu_cwd = menu_cwd.clone();
                                        move |menu, _window, cx| {
                                            let cwd_opt =
                                                (!menu_cwd.is_empty()).then(|| menu_cwd.clone());
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
                                                            e_term.update(cx, |ws, cx| {
                                                                ws.add_session(cwd, cx)
                                                            });
                                                        }),
                                                );
                                            for entry in entries {
                                                let label = entry.label;
                                                let command = entry.command;
                                                let cwd_launch = cwd_opt.clone();
                                                let e_launch = e_menu.clone();
                                                let icon = icon_for_launch_command(&command);
                                                menu = menu.item(
                                                    PopupMenuItem::new(label.clone())
                                                        .icon(icon)
                                                        .on_click(move |_ev, _window, cx| {
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
                                                        }),
                                                );
                                            }
                                            menu = menu.separator().item(PopupMenuItem::label(
                                                "对话 · smelt 原生界面",
                                            ));
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
                                                                ws.add_acp_session(
                                                                    agent, cwd, window, cx,
                                                                )
                                                            });
                                                        }),
                                                );
                                            }
                                            menu
                                        }
                                    }),
                            ),
                    )
                    .on_click(move |_ev, _window, cx| {
                        let root = toggle_root.clone();
                        e_toggle.update(cx, |ws, cx| {
                            if !ws.collapsed_projects.remove(&root) {
                                ws.collapsed_projects.insert(root);
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
                        let e_close_proj = e_menu.clone();
                        let path = cwd.clone();
                        let close_root = cwd.clone();
                        let sess_n = ixs.len();
                        let del_main_root = worktree_main_root.clone();
                        let del_branch = worktree_branch.clone();
                        move |menu, _window, cx| {
                            let copy_path = path.clone();
                            let del_path = path.clone();
                            let pin_path = path.clone();
                            let e_del = e_del.clone();
                            let e_pin = e_pin.clone();
                            let e_close_proj = e_close_proj.clone();
                            let close_root = close_root.clone();
                            let del_main_root = del_main_root.clone();
                            let del_branch = del_branch.clone();
                            // 已 pin → 显示「从文件树移除」，否则「加到文件树」（当前活动项目
                            // 天然在文件树里，pin 它=切走后仍保留，所以照样给这个开关）。
                            let pinned = e_pin.read(cx).is_file_tree_root_pinned(&pin_path);
                            let pin_label = if pinned {
                                "从文件树移除"
                            } else {
                                "加到文件树"
                            };
                            menu.item(PopupMenuItem::new("复制项目路径").on_click(
                                move |_ev, _window, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        copy_path.clone(),
                                    ));
                                },
                            ))
                            .item(
                                PopupMenuItem::new(pin_label)
                                    .icon(IconName::Folder)
                                    .on_click(move |_ev, _window, cx| {
                                        let pin_path = pin_path.clone();
                                        e_pin.update(cx, |ws, cx| {
                                            ws.toggle_file_tree_root(pin_path, cx)
                                        });
                                    }),
                            )
                            // 关项目 = 从工作台移走这个项目，连带关掉它下面的会话
                            //（标数量，别让人点完才发现关掉了一堆活）。
                            .separator()
                            .item(
                                PopupMenuItem::new(if sess_n > 0 {
                                    format!("关闭项目（含 {sess_n} 个会话）")
                                } else {
                                    "关闭项目".to_string()
                                })
                                .icon(IconName::CircleX)
                                .on_click(
                                    move |_ev, _window, cx| {
                                        let root = close_root.clone();
                                        e_close_proj
                                            .update(cx, |ws, cx| ws.start_close_project(root, cx));
                                    },
                                ),
                            )
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

            // 空项目（打开了但一个会话都没开 / 会话都关光了）：给一行淡色占位，
            // 不然只剩一截孤零零的引导线，看着像渲染坏了。
            if ixs.is_empty() {
                // + 改成 hover 才显形后，这行不能再指望「点 + 新建」那个 + 看得见——
                // 整行做成可点，点了直接给这个项目开一个终端会话。
                let e_empty = this.clone();
                let empty_cwd = cwd.clone();
                rows = rows.child(
                    div()
                        .id(("proj-empty", pix))
                        .ml(px(17.))
                        .pl(px(10.))
                        .py(px(3.))
                        .border_l_1()
                        .border_color(rgb(ui_theme::border()))
                        .text_size(px(12.))
                        .text_color(rgb(ui_theme::text_faint()))
                        .cursor_pointer()
                        .hover(|d| d.text_color(rgb(ui_theme::text_mid())))
                        .child("还没有会话 · 点这里新建")
                        .on_click(move |_ev, _window, cx| {
                            let cwd = (!empty_cwd.is_empty()).then(|| empty_cwd.clone());
                            e_empty.update(cx, |ws, cx| ws.add_session(cwd, cx));
                        }),
                );
                continue;
            }

            // ---- 组内会话行 ----
            // 装进一个带左侧引导线的容器：缩进 + 竖线让「这些会话属于上面那个
            // 项目」一眼成立，不靠读缩进像素差。
            // 引导线从 border_dim 提到 border：色带撤掉后，「这一坨会话属于上面那个
            // 项目」全靠这条线说，dim(0x202226) 压在 bg_elev(0x232428) 上几乎不可见。
            let mut group_body = div()
                .flex()
                .flex_col()
                .gap(px(1.))
                .ml(px(17.))
                .pl(px(10.))
                .border_l_1()
                .border_color(rgb(ui_theme::border()));
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
                // 分屏组（无父行）要复用「拖拽排序」，但标题会被下面父行的 on_drag
                // 吃掉所有权，先给分屏分支留一份克隆。
                let drag_title_grp = drag_title.clone();

                // 类型标识：agent 紫圆点 / 终端绿方块。
                // 会话标记一律圆点（项目是方块），颜色区分类型：紫 = agent
                // 消息流、绿 = 终端。形状管层级、颜色管类型，各司其职。
                let type_dot: AnyElement = div()
                    .size(px(7.))
                    .rounded_full()
                    .bg(rgb(if is_acp {
                        ui_theme::purple()
                    } else {
                        ui_theme::green()
                    }))
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
                        .map(|d| {
                            if at_top {
                                d.top(px(-3.))
                            } else {
                                d.bottom(px(-3.))
                            }
                        })
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
                    // hover 走 bg_row_hover（比 bg_hover 淡一大截）：鼠标划过时屏幕上
                    // 会同时存在「划过的行」和「当前选中的行」两块底，两块一样亮就分不出
                    // 哪个才是当前——划过只该是「浮起一点」，亮起来是选中的专属信号。
                    .map(|d| {
                        if is_active {
                            d.bg(rgb(ui_theme::bg_selected()))
                        } else {
                            d.hover(|d| d.bg(rgb(ui_theme::bg_row_hover())))
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
                            // 会话名是侧栏主体（比项目标签大、也更亮）：驾驶舱日常盯的就是
                            // 这一行行 agent 会话，项目名反而退成上面的小灰标签。
                            .text_size(px(14.))
                            // 选中行标题提亮到 bright（Discord 选中频道名变白），其余是清晰
                            // 正文 text——不压到 text_mid，主体不该发灰。
                            .text_color(rgb(if is_active {
                                ui_theme::text_bright()
                            } else {
                                ui_theme::text()
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
                    // 每行都写一遍「空闲」等于用一整行高度说一句废话）。
                    //
                    // 它和下面的状态点在 hover 时一起淡出：右端要交给 absolute 操作条
                    // 浮层，不淡出的话浮层的背景会盖掉半个标签，「需要处理」只剩「需要」，
                    // 看着像文字坏了。用 opacity 淡出而不是不渲染——位置留着，标题宽度
                    // 不变，才不会真的抖。
                    .children(attention_label.map(|label| {
                        div()
                            .flex_shrink_0()
                            .text_size(px(11.))
                            .text_color(ui_theme::session_dot_color(status))
                            .group_hover(SESS_ROW_GROUP, |s| s.opacity(0.0))
                            .child(label)
                    }))
                    .child(
                        div()
                            .flex_shrink_0()
                            .size(px(6.))
                            .rounded_full()
                            .bg(ui_theme::session_dot_color(status))
                            .group_hover(SESS_ROW_GROUP, |s| s.opacity(0.0)),
                    )
                    .child(
                        // 右端操作条：拖拽手柄 + 关闭。absolute 浮在行右端，平时不占位
                        //（status dot 因此贴到最右、无留白），hover 那行才淡入、盖在
                        // status dot 上（VSCode 行内 action 同款）。背景取行当前底色
                        //（选中 bg_selected / 否则 bg_row_hover）才能把下面盖干净。
                        div()
                            .absolute()
                            .top(px(1.))
                            .bottom(px(1.))
                            .right(px(6.))
                            .flex()
                            .items_center()
                            .pl_3()
                            .rounded(px(6.))
                            .bg(rgb(if is_active {
                                ui_theme::bg_selected()
                            } else {
                                ui_theme::bg_row_hover()
                            }))
                            .opacity(0.0)
                            .group_hover(SESS_ROW_GROUP, |s| s.opacity(1.0))
                            .child(
                                div()
                                    .id(("sess-drag", ix))
                                    .w(px(14.))
                                    .h(px(18.))
                                    .cursor_grab()
                                    .on_drag(
                                        SessionDrag {
                                            id: entity_id,
                                            title: drag_title,
                                        },
                                        {
                                            let e_clear = e_drop.clone();
                                            move |drag, _, _, cx| {
                                                e_clear
                                                    .update(cx, |ws, _| ws.sess_drop_hint = None);
                                                cx.new(|_| drag.clone())
                                            }
                                        },
                                    ),
                            )
                            .child(
                                // 关闭键不用 ghost Button：它的 hover 底是
                                // secondary(bg_card 0x3a3c42).lighten(0.1)，压在选中行的
                                // bg_selected(0x45474f) 上比底色还暗——鼠标压上去等于没反应，
                                // 且 ghost 的图标色 hover 前后不变。这里自己画，hover **只把
                                // 图标转红、不加底**：平时可见的只有 14px 图标，一加 20px 的
                                // hover 底就像整个键突然撑大一圈（布局没变，但眼睛就是这么读的）。
                                // 灰→红本身已经是够强的反馈，也把「这是关掉」说清楚了。
                                div()
                                    .id(("close-session", ix))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .size(px(20.))
                                    .rounded(px(4.))
                                    .cursor_pointer()
                                    .text_color(rgb(ui_theme::text_muted()))
                                    .hover(|d| d.text_color(rgb(ui_theme::red())))
                                    .child(Icon::new(IconName::CircleX).size(px(14.)))
                                    .on_click(move |_ev, _w, cx| {
                                        cx.stop_propagation();
                                        e_close.update(cx, |ws, cx| ws.close_session(ix, cx));
                                    }),
                            ),
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
                        .item(
                            PopupMenuItem::new("重命名").on_click(move |_ev, window, cx| {
                                e_rename.update(cx, |ws, cx| {
                                    ws.start_rename(RenameTarget::Session(ix), window, cx)
                                });
                            }),
                        )
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
                        .when(hint_before, |row| {
                            row.child(indicator(("sess-ind-b", ix), true))
                        })
                        .when(hint_after, |row| {
                            row.child(indicator(("sess-ind-a", ix), false))
                        })
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
                    let raw_titles: Vec<String> = leaves
                        .iter()
                        .map(|v| pane_title(v, cx).to_string())
                        .collect();

                    // 内层括线：比项目引导线再内缩一档，左侧圆角竖线把同一 tab 的
                    // 几个 pane 圈成一组（视觉上 ≈ ╭…╰ 括号）。
                    // pane 行跟普通会话行同缩进、同字号，不再内缩变小；「成组」只由
                    // 左侧一条括线表达（见下方 pane_group 的 absolute 竖线）。
                    // 右侧留出 16px 给组右上角的拖拽手柄，pane 行内的 × 才不会被它压住。
                    let mut pane_rows = div().flex().flex_col().gap(px(1.)).pr(px(16.));
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
                        let e_pane_close = this.clone();
                        let pane = view.clone();
                        let menu_pane = view.clone();
                        let close_pane = view.clone();
                        pane_rows = pane_rows.child(
                            div()
                                .id(("sess-pane-row", ix * 100 + lix))
                                .group(PANE_ROW_GROUP)
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
                                        d.hover(|d| d.bg(rgb(ui_theme::bg_row_hover())))
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
                                // 每个 pane 自己的关闭键：只关这一个 pane，剩下的照常活着。
                                // 常态透明，hover 本行才显形（本行 group，不是整组）。
                                .child(
                                    div()
                                        .flex_shrink_0()
                                        .opacity(0.0)
                                        .group_hover(PANE_ROW_GROUP, |s| s.opacity(1.0))
                                        .child(
                                            Button::new(("close-pane", ix * 100 + lix))
                                                .ghost()
                                                .xsmall()
                                                .icon(IconName::CircleX)
                                                .on_click(move |_ev, window, cx| {
                                                    cx.stop_propagation();
                                                    let pane = close_pane.clone();
                                                    e_pane_close.update(cx, |ws, cx| {
                                                        ws.close_session_pane(ix, pane, window, cx)
                                                    });
                                                }),
                                        ),
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
                                    let e_close_all = e_pane_menu.clone();
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
                                    // 行内的 × 只关这一个 pane；要连整组一起关走这里。
                                    .item(
                                        PopupMenuItem::new("关闭整个会话（含全部分屏）").on_click(
                                            move |_ev, _window, cx| {
                                                e_close_all
                                                    .update(cx, |ws, cx| ws.close_session(ix, cx));
                                            },
                                        ),
                                    )
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

                    // 会话级操作条：只留拖拽手柄（整会话排序），贴组右上角。
                    // 这里以前还有个「关闭整会话」×，位置正好压在组内第一个 pane 行的
                    // 右端，看着像「关这一行」，点下去却把整组分屏都关了。关闭一律下沉
                    // 到 pane 行自己的 ×，整组要关走 pane 行右键菜单。
                    let ops = div()
                        .absolute()
                        .top(px(2.))
                        .right(px(2.))
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
                                    SessionDrag {
                                        id: entity_id,
                                        title: drag_title_grp,
                                    },
                                    {
                                        let e_clear = e_drop.clone();
                                        move |drag, _, _, cx| {
                                            e_clear.update(cx, |ws, _| ws.sess_drop_hint = None);
                                            cx.new(|_| drag.clone())
                                        }
                                    },
                                ),
                        );
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
                // 不属于任何项目的裸终端（iTerm 式随手开个 shell）。跟「打开项目」并排
                // 归在底部——都是「不针对某个已有项目」的全局动作。
                div()
                    .id("scratch-terminal")
                    .flex()
                    .items_center()
                    .gap_1()
                    .text_xs()
                    .text_color(rgb(ui_theme::text_faint()))
                    .cursor_pointer()
                    .hover(|d| d.text_color(rgb(ui_theme::text_mid())))
                    .child(Icon::new(IconName::SquareTerminal).size(px(12.)))
                    .child("终端")
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
