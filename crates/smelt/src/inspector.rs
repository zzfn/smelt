//! inspector：窗口右侧的图标条（56px 常驻）+ 面板（344px 可整体隐藏）。
//! FILES / GIT / TASKS / MCP / SETTINGS 五个 tab，点击图标切换或收合；面板头
//! 带「展开」把对应的旧全屏页盖到会话舞台上（stage_override），功能零删除。
//!
//! 跟 file_tree.rs 同一个套路：`impl Workspace` 方法，字段仍在 main.rs。

use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::*;

use crate::tasks::TaskStore;
use crate::{MainView, SETTINGS_PAGE_APPEARANCE, Workspace, ui_theme};

/// 侧栏任务卡片的 hover group 名：卡片 `.group()` + 操作条 `.group_hover()` 配对，
/// 鼠标移到卡片才显形「编辑 / 删除」。名字全卡共享，靠 DOM 祖先关系就近生效。
const TASK_CARD_GROUP: &str = "insp-task-card";

/// inspector 面板的五个 tab。
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum InspectorTab {
    Files,
    Git,
    Tasks,
    Skills,
    Settings,
}

impl InspectorTab {
    fn label(self) -> &'static str {
        match self {
            Self::Files => "FILES",
            Self::Git => "GIT",
            Self::Tasks => "TASKS",
            Self::Skills => "SKILL",
            Self::Settings => "SET",
        }
    }

    /// 面板头「⤢ 展开」对应的舞台全宽视图；None = 头上不放展开按钮。
    /// Files → 「文件树 + 内容」双栏，Git → 「变更列表 + diff」双栏。
    fn stage_view(self) -> Option<MainView> {
        match self {
            Self::Files => Some(MainView::Files),
            Self::Git => Some(MainView::Git),
            Self::Tasks => Some(MainView::Tasks),
            Self::Skills | Self::Settings => None,
        }
    }
}

impl Workspace {
    /// 当前 tab 是不是已经「提升到舞台」（⤢ 展开）。
    /// 提升后本体在舞台上，右侧就别再停靠一份——否则同一个文件树 / 变更列表
    /// 左右各渲染一遍，看着像出了两个面板。
    pub(crate) fn inspector_panel_promoted(&self) -> bool {
        self.stage_override.is_some() && self.inspector_tab.stage_view() == self.stage_override
    }

    /// 图标条点击：已提升到舞台 → 收回停靠；同 tab 再点 → 收合/展开面板；
    /// 异 tab → 切过去并保证展开。
    pub(crate) fn toggle_inspector_tab(
        &mut self,
        tab: InspectorTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.inspector_tab == tab && self.inspector_panel_promoted() {
            // 点的就是当前占着舞台的那个 → 等价于 ⤡ 收回停靠
            self.set_stage_override(None, window, cx);
            self.inspector_open = true;
            return;
        }
        if self.inspector_tab == tab && self.inspector_open {
            self.inspector_open = false;
        } else {
            self.inspector_tab = tab;
            self.inspector_open = true;
        }
        cx.notify();
    }

    /// 56px 图标条（最右，常驻）。
    pub(crate) fn render_inspector_rail(&mut self, cx: &mut Context<Self>) -> Div {
        let this = cx.entity();
        // GIT 角标：当前项目改动文件数（读 git status 缓存，没有就不显示）。
        let git_changes = self
            .cur()
            .and_then(|s| s.cwd(cx))
            .and_then(|cwd| self.git_status.get(&cwd))
            .map(|(_, d)| d.files.len())
            .unwrap_or(0);

        let item = |tab: InspectorTab, badge: usize, active: bool, this: Entity<Workspace>| {
            div()
                .id(tab.label())
                .relative()
                .w_full()
                .flex()
                .flex_col()
                .items_center()
                .gap_1()
                .py_3()
                .cursor_pointer()
                .map(|d| {
                    if active {
                        d.bg(ui_theme::tint(ui_theme::accent(), 0x14))
                            .border_l_2()
                            .border_color(rgb(ui_theme::accent()))
                    } else {
                        d.border_l_2()
                            .border_color(gpui::transparent_black())
                            .hover(|d| d.bg(rgb(ui_theme::bg_hover())))
                    }
                })
                .child(div().size(px(7.)).rounded_xs().bg(if active {
                    rgb(ui_theme::accent())
                } else {
                    rgb(ui_theme::border_focus())
                }))
                .child(
                    div()
                        .text_size(px(9.))
                        .font_semibold()
                        .text_color(if active {
                            rgb(ui_theme::text_bright())
                        } else {
                            rgb(ui_theme::text_faint())
                        })
                        .child(tab.label()),
                )
                .when(badge > 0, |d| {
                    d.child(
                        div()
                            .absolute()
                            .top(px(6.))
                            .right(px(7.))
                            .px(px(4.))
                            .rounded(px(8.))
                            .bg(rgb(ui_theme::accent()))
                            .text_size(px(8.))
                            .font_semibold()
                            .text_color(rgb(ui_theme::on_accent()))
                            .child(badge.to_string()),
                    )
                })
                .on_click(move |_ev, window, cx| {
                    this.update(cx, |ws, cx| ws.toggle_inspector_tab(tab, window, cx));
                })
        };

        let cur = self.inspector_tab;
        let open = self.inspector_open;
        div()
            .w(px(56.))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .pt_1()
            .bg(rgb(ui_theme::bg_rail()))
            .border_l_1()
            .border_color(rgb(ui_theme::border_dim()))
            .child(item(
                InspectorTab::Files,
                0,
                open && cur == InspectorTab::Files,
                this.clone(),
            ))
            .child(item(
                InspectorTab::Git,
                git_changes,
                open && cur == InspectorTab::Git,
                this.clone(),
            ))
            .child(item(
                InspectorTab::Tasks,
                0,
                open && cur == InspectorTab::Tasks,
                this.clone(),
            ))
            .child(item(
                InspectorTab::Skills,
                0,
                open && cur == InspectorTab::Skills,
                this.clone(),
            ))
            .child(div().flex_1())
            .child(item(
                InspectorTab::Settings,
                0,
                open && cur == InspectorTab::Settings,
                this,
            ))
    }

    /// 面板统一头：36px，标题 + 可选「展开」（盖到舞台）按钮 + 自定义右侧内容。
    pub(crate) fn inspector_header(
        &self,
        title: &'static str,
        tab: InspectorTab,
        cx: &mut Context<Self>,
    ) -> Div {
        let this = cx.entity();
        div()
            .h(px(36.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_between()
            .px_3()
            .border_b_1()
            .border_color(rgb(ui_theme::border_dim()))
            .child(
                div()
                    .text_xs()
                    .font_semibold()
                    .text_color(rgb(ui_theme::text_muted()))
                    .child(title),
            )
            .children(tab.stage_view().map(|view| {
                div()
                    .id(("inspector-expand", tab as usize))
                    .text_xs()
                    .text_color(rgb(ui_theme::text_faint()))
                    .cursor_pointer()
                    .hover(|d| d.text_color(rgb(ui_theme::text_mid())))
                    .child("展开 ⤢")
                    .on_click(move |_ev, window, cx| {
                        this.update(cx, |ws, cx| ws.set_stage_override(Some(view), window, cx));
                    })
            }))
    }

    /// 344px 面板本体：按当前 tab 分派。
    pub(crate) fn render_inspector_panel(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let body: AnyElement = match self.inspector_tab {
            InspectorTab::Files => self.render_inspector_files(window, cx),
            InspectorTab::Git => self.render_inspector_git(window, cx),
            InspectorTab::Tasks => self.render_inspector_tasks(cx),
            InspectorTab::Skills => self.render_inspector_skills(cx),
            InspectorTab::Settings => self.render_inspector_settings(cx),
        };
        div()
            .w(px(344.))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .min_h_0()
            .bg(rgb(ui_theme::bg_elev()))
            .border_l_1()
            .border_color(rgb(ui_theme::border_dim()))
            .child(body)
    }

    /// TASKS 面板：任务卡片列表（复用 TaskStore；卡片行动 = focus_or_run_task）。
    fn render_inspector_tasks(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let this = cx.entity();
        let mut tasks = TaskStore::load().tasks;
        tasks.sort_by_key(|t| t.column.sidebar_rank());
        let count = tasks.len();

        let e_new = this.clone();
        let header = self
            .inspector_header("TASKS", InspectorTab::Tasks, cx)
            .child(
                div()
                    .id("inspector-task-new")
                    .text_xs()
                    .font_semibold()
                    .text_color(rgb(ui_theme::accent()))
                    .cursor_pointer()
                    .hover(|d| d.opacity(0.8))
                    .child(format!("+ 新建 · {count}"))
                    .on_click(move |_ev, window, cx| {
                        e_new.update(cx, |ws, cx| ws.open_new_task_modal(window, cx));
                    }),
            );

        let mut list = div()
            .id("inspector-task-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_2()
            .p_2p5();
        if tasks.is_empty() {
            list = list.child(
                div()
                    .pt_8()
                    .flex()
                    .justify_center()
                    .text_sm()
                    .text_color(rgb(ui_theme::text_faint()))
                    .child("还没有任务"),
            );
        }
        for (tix, t) in tasks.into_iter().enumerate() {
            let done = t.column == crate::tasks::TaskColumn::Done;
            let color = rgb(t.column.color());
            let has_session = t.session_id.is_some();
            let action_label = if has_session {
                "打开 →"
            } else if t.column.is_todo() {
                "运行 →"
            } else {
                ""
            };
            let tid = t.id.clone();
            let e_act = this.clone();
            // 平时透明、鼠标移到卡片才显形的操作条（编辑 / 删除）。stop_propagation
            // 拦住 mouse_down，避免同时触发整卡的 focus_or_run。group 名见卡片 `.group()`。
            let e_edit = this.clone();
            let e_del = this.clone();
            let tid_edit = t.id.clone();
            let tid_del = t.id.clone();
            let hover_bar = div()
                .flex()
                .items_center()
                .gap_1()
                .flex_shrink_0()
                .opacity(0.0)
                .group_hover(TASK_CARD_GROUP, |s| s.opacity(1.0))
                .child(
                    div()
                        .id(("inspector-task-edit", tix))
                        .px_1()
                        .text_xs()
                        .cursor_pointer()
                        .text_color(rgb(ui_theme::text_faint()))
                        .hover(|s| s.text_color(rgb(ui_theme::accent())))
                        .child("编辑")
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(move |_ev, window, cx| {
                            let tid = tid_edit.clone();
                            e_edit.update(cx, |ws, cx| ws.open_edit_task_modal(&tid, window, cx));
                        }),
                )
                .child(
                    div()
                        .id(("inspector-task-del", tix))
                        .px_1()
                        .text_xs()
                        .cursor_pointer()
                        .text_color(rgb(ui_theme::text_faint()))
                        .hover(|s| s.text_color(rgb(ui_theme::red())))
                        .child("删除")
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(move |_ev, _window, cx| {
                            let tid = tid_del.clone();
                            e_del.update(cx, |ws, cx| ws.delete_task(&tid, cx));
                        }),
                );
            // 结构：外层横排 = 左侧 3px 状态色竖条 + 内容列（GPUI 边框色是单值，
            // 左边框异色做不到，用嵌套竖条实现设计稿的左色条）。
            let card = div()
                .id(("inspector-task", tix))
                .group(TASK_CARD_GROUP)
                .rounded(px(9.))
                .border_1()
                .border_color(rgb(ui_theme::border_mid()))
                .bg(if done {
                    rgb(ui_theme::bg_panel())
                } else {
                    rgb(ui_theme::bg_card())
                })
                .when(done, |d| d.opacity(0.55))
                .overflow_hidden()
                .flex()
                .cursor_pointer()
                .child(div().w(px(3.)).flex_shrink_0().bg(color))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .p_3()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_sm()
                                        .font_semibold()
                                        .text_color(rgb(ui_theme::text_bright()))
                                        .child(if t.title.is_empty() {
                                            "（未命名任务）".to_string()
                                        } else {
                                            t.title.clone()
                                        }),
                                )
                                .child(hover_bar),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(div().size(px(6.)).rounded_full().bg(color))
                                .child(div().text_xs().text_color(color).child(t.column.label()))
                                .child(div().flex_1())
                                .when(!action_label.is_empty(), |d| {
                                    d.child(
                                        div()
                                            .text_xs()
                                            .font_semibold()
                                            .text_color(if has_session {
                                                rgb(ui_theme::purple())
                                            } else {
                                                rgb(ui_theme::green())
                                            })
                                            .child(action_label),
                                    )
                                }),
                        ),
                )
                .on_click(move |_ev, window, cx| {
                    let tid = tid.clone();
                    e_act.update(cx, |ws, cx| ws.focus_or_run_task(&tid, window, cx));
                });
            list = list.child(card);
        }

        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(header)
            .child(list)
            .into_any_element()
    }

    /// FILES 面板：文件树（复用全屏页的 file_tree 组件）。点文件不替换本面板，
    /// 而是把「文件树 + 内容」双栏提升到舞台（见 open_file_now），树在这儿一直在。
    fn render_inspector_files(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let header = self.inspector_header("EXPLORER", InspectorTab::Files, cx);
        self.try_flush_file_tree_reveal(cx);
        let open_path = self.open_file.as_ref().map(|of| of.path.as_str());
        let selected = self.file_tree_selected.as_deref();
        // 多根工作区：inspector 的 EXPLORER 也把所有项目根一起挂出来（跟全屏 Files 页
        // 同一套 workspace_roots / collapsed_roots，行为一致）。
        let roots = self.workspace_roots(cx);
        let tree = crate::file_tree::file_tree(
            &roots,
            &self.expanded,
            &self.collapsed_roots,
            &self.dir_cache,
            &self.file_tree_scroll,
            open_path,
            selected,
            344.,
            &self.git_status,
            cx,
        );
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(header)
            .child(div().flex_1().min_h_0().flex().flex_col().child(tree))
            .into_any_element()
    }

    /// GIT 面板：窄版 SOURCE CONTROL（实现见 git_panel.rs 的 git_narrow_panel，
    /// 需要访问 GitStatusData / DiffLine 的模块内私有字段）。
    fn render_inspector_git(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        self.git_narrow_panel(window, cx)
    }

    /// SKILLS 面板：列出用户级 / 项目级 skill（`~/.claude/skills`、
    /// `<项目>/.claude/skills`），点一条把 `/name` 填进当前会话。
    ///
    /// 不做「启用/停用」开关：Claude Code 侧没有对应机制（settings.json 的
    /// `enabledPlugins` 管插件不管 skill），拨了不生效的开关比没有更糟。
    fn render_inspector_skills(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let cwd = self.cur().and_then(|s| s.cwd(cx));
        self.ensure_skills(cwd, cx);
        let this = cx.entity();
        let skills = self.skills_cache.as_ref().map(|(_, d)| d.clone());

        let header = div()
            .h(px(36.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_between()
            .px_3()
            .border_b_1()
            .border_color(rgb(ui_theme::border_dim()))
            .child(
                div()
                    .text_xs()
                    .font_semibold()
                    .text_color(rgb(ui_theme::text_muted()))
                    .child("SKILLS"),
            )
            .children(skills.as_ref().map(|s| {
                div()
                    .text_size(px(10.))
                    .text_color(rgb(ui_theme::text_faint()))
                    .child(s.len().to_string())
            }));

        let mut list = div()
            .id("inspector-skill-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_1p5()
            .p_2p5();
        match skills {
            None => {
                list = list.child(
                    div()
                        .pt_8()
                        .flex()
                        .justify_center()
                        .text_sm()
                        .text_color(rgb(ui_theme::text_faint()))
                        .child("加载中…"),
                );
            }
            Some(items) if items.is_empty() => {
                list = list.child(
                    div()
                        .pt_8()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap_1()
                        .text_color(rgb(ui_theme::text_faint()))
                        .child(div().text_sm().child("还没有 skill"))
                        .child(
                            div()
                                .text_xs()
                                .font_family("monospace")
                                .child("~/.claude/skills/<名字>/SKILL.md"),
                        ),
                );
            }
            Some(items) => {
                let mut last_scope: Option<bool> = None;
                for (six, sk) in items.iter().enumerate() {
                    // 分组小标题：项目级 / 用户级（scan_skills 已按 scope 排好序）。
                    if last_scope != Some(sk.project_scope) {
                        last_scope = Some(sk.project_scope);
                        list = list.child(
                            div()
                                .px_1()
                                .pt_1p5()
                                .text_size(px(10.))
                                .font_semibold()
                                .text_color(rgb(ui_theme::text_faint()))
                                .child(if sk.project_scope {
                                    "项目级"
                                } else {
                                    "用户级"
                                }),
                        );
                    }
                    let dot = if sk.project_scope {
                        rgb(ui_theme::accent())
                    } else {
                        rgb(ui_theme::blue())
                    };
                    let e_use = this.clone();
                    let cmd = format!("/{}", sk.name);
                    list = list.child(
                        div()
                            .id(("inspector-skill", six))
                            .rounded(px(8.))
                            .border_1()
                            .border_color(rgb(ui_theme::border_mid()))
                            .bg(rgb(ui_theme::bg_card()))
                            .px_2p5()
                            .py_2()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .cursor_pointer()
                            .hover(|d| d.border_color(rgb(ui_theme::border_focus())))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1p5()
                                    .child(div().flex_shrink_0().size(px(6.)).rounded_xs().bg(dot))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .text_xs()
                                            .font_semibold()
                                            .font_family("monospace")
                                            .text_color(rgb(ui_theme::text_bright()))
                                            .truncate()
                                            .child(sk.name.clone()),
                                    ),
                            )
                            .when(!sk.description.is_empty(), |d| {
                                d.child(
                                    div()
                                        .text_size(px(10.))
                                        .line_height(px(14.))
                                        .text_color(rgb(ui_theme::text_muted()))
                                        // 描述常是一长段触发条件，卡片里只留两行的量。
                                        .max_h(px(28.))
                                        .overflow_hidden()
                                        .child(sk.description.clone()),
                                )
                            })
                            .on_click(move |_ev, window, cx| {
                                let cmd = cmd.clone();
                                e_use.update(cx, |ws, cx| {
                                    ws.send_skill_to_session(&cmd, window, cx)
                                });
                            }),
                    );
                }
            }
        }

        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(header)
            .child(list)
            .into_any_element()
    }

    /// SETTINGS 面板：分组列表，点击跳独立设置窗对应页（面板内嵌不现实——
    /// 设置页组件按 900px 窗口版式设计，见方案）。
    fn render_inspector_settings(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let this = cx.entity();
        let header = self.inspector_header("SETTINGS", InspectorTab::Settings, cx);
        // 页序与 settings.rs `render_settings_content` 末尾 pages(vec![...]) 一致。
        let pages: [(&'static str, usize); 6] = [
            ("外观", SETTINGS_PAGE_APPEARANCE),
            ("桌面宠物", 1),
            ("启动", 2),
            ("Agent 集成", 3),
            ("更新", crate::SETTINGS_PAGE_UPDATE),
            ("远程", 5),
        ];
        let mut list = div()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .flex()
            .flex_col()
            .gap_1()
            .p_2();
        for (label, ix) in pages {
            let e = this.clone();
            list = list.child(
                div()
                    .id(("inspector-set", ix))
                    .px_3()
                    .py_2()
                    .rounded(px(7.))
                    .text_sm()
                    .text_color(rgb(ui_theme::text_mid()))
                    .cursor_pointer()
                    .hover(|d| {
                        d.bg(rgb(ui_theme::bg_hover()))
                            .text_color(rgb(ui_theme::text_bright()))
                    })
                    .child(label)
                    .on_click(move |_ev, window, cx| {
                        e.update(cx, |ws, cx| {
                            if ws.llm_inputs.is_none() {
                                ws.init_llm_inputs(window, cx);
                            }
                            ws.settings_page_ix = ix;
                            ws.settings_page_nonce += 1;
                            ws.open_settings_window(cx);
                        });
                    }),
            );
        }
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(header)
            .child(list)
            .into_any_element()
    }
}
