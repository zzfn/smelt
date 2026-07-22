//! 「日志」页的 GPUI 渲染：左侧分支树 + 分支图 + 提交列表 + 右侧提交详情。
//!
//! 布局算法在 [`crate::git_log`]，本文件只负责把算好的 [`GraphRow`] 画出来。
//! 分支图走 `canvas` + `PathBuilder`：竖线和斜线都是真实路径，缩放不会糊，也不用
//! 像解析 `git log --graph` 的字符画那样跟行高较劲。
//!
//! **本文件的所有渲染函数都必须靠参数拿状态，绝不能 `cx.entity().read(cx)`**：
//! render 期间 Workspace 正被可变借用，再 read 一次就是重入借用，而 GPUI 的 render
//! 跑在 FFI 边界上不可 unwind，panic 会直接 abort——表现为「点一下标签整个 App 闪退」，
//! 且崩溃报告里只有 `panic_cannot_unwind`，看不到真正的 panic 消息，极难查。
//! `cx.entity()` 本身只是拿句柄，安全；危险的是紧跟着的 `.read(cx)`。
//! 同样的教训见 file_tree.rs 里 context_menu 那段注释。

use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::menu::{ContextMenuExt, PopupMenuItem};
use gpui_component::scroll::ScrollableElement;
use gpui_component::{h_flex, v_flex, ActiveTheme};

use crate::git_log::{Edge, GitLogState, GraphRow, LogScope};
use crate::ui_theme;
use crate::{placeholder_view, Workspace};

/// 每行高度。与提交列表逐行对齐，图和文字必须共用同一个值。
pub const ROW_H: f32 = 24.0;
/// 分支图每列宽度。
const LANE_W: f32 = 14.0;
/// 提交圆点半径。
const DOT_R: f32 = 3.5;
/// 图区左右各留一点，免得第 0 列贴边。
const GRAPH_PAD: f32 = 8.0;

/// 分支线配色：按列号取模。刻意避开红/绿——那两个颜色在 diff 里表示增删，
/// 用在分支线上会让人误读。
/// 深浅两套：深色沿用原来的亮色系；浅色整体压深，否则细线在近白底上看不见。
const LANE_COLORS_DARK: [u32; 6] = [0x7dcfff, 0xbb9af7, 0xe0af68, 0x2ac3de, 0xff9e64, 0x9ece6a];
const LANE_COLORS_LIGHT: [u32; 6] = [0x1f7aa8, 0x7a4aa8, 0x9a6b0a, 0x117a8a, 0xb85c1f, 0x4a7a1f];

fn lane_color(i: usize) -> Hsla {
    let ring =
        if ui_theme::is_light() { &LANE_COLORS_LIGHT } else { &LANE_COLORS_DARK };
    rgb(ring[i % ring.len()]).into()
}

/// 图区总宽度。至少留 3 列的位置，免得历史前几行只有一列时图区窄得贴着文字跳动。
pub fn graph_width(rows: &[GraphRow]) -> f32 {
    let max = rows.iter().map(|r| r.lanes).max().unwrap_or(1).max(3);
    GRAPH_PAD * 2.0 + max as f32 * LANE_W
}

/// 把整张分支图画在一个 canvas 上。
///
/// 一次画完所有行而不是每行一个 canvas：线段跨行（上一行的下边缘就是下一行的上
/// 边缘），分行画的话每段都要自己算邻居状态，还容易在行边界留缝。
///
/// 只画落在可视区里的行：一屏几十行，而列表有几百条，全量画等于每帧构造上千条
/// 路径。裁剪区从 `window.content_mask()` 拿——canvas 在滚动容器里，它给的就是
/// 当前真正可见的那块。
pub fn graph_canvas(rows: Vec<GraphRow>, width: f32) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _cx| {
            let ox = bounds.origin.x + px(GRAPH_PAD);
            let oy = bounds.origin.y;
            // 列中心的 x。
            let lane_x = |l: usize| ox + px(l as f32 * LANE_W + LANE_W / 2.0);
            let vis = window.content_mask().bounds;

            for (ri, row) in rows.iter().enumerate() {
                let top = oy + px(ri as f32 * ROW_H);
                // 整行都在可视区外就跳过。留一行余量，免得跨行的线在边界处断头。
                if top + px(ROW_H * 2.0) < vis.origin.y
                    || top > vis.origin.y + vis.size.height + px(ROW_H)
                {
                    continue;
                }
                let mid = top + px(ROW_H / 2.0);
                let bottom = top + px(ROW_H);
                let node_x = lane_x(row.node);

                for edge in &row.edges {
                    // 每段线的颜色跟着它所在的列走，分支才好用颜色区分。
                    let (from_x, from_y, to_x, to_y, color) = match *edge {
                        Edge::Through { from, to } => {
                            (lane_x(from), top, lane_x(to), bottom, lane_color(from))
                        }
                        Edge::In { from } => (lane_x(from), top, node_x, mid, lane_color(from)),
                        Edge::Out { to } => (node_x, mid, lane_x(to), bottom, lane_color(to)),
                    };
                    let mut pb = PathBuilder::stroke(px(1.5));
                    pb.move_to(point(from_x, from_y));
                    if (from_x - to_x).abs() < px(0.5) {
                        // 同一列：直线。
                        pb.line_to(point(to_x, to_y));
                    } else {
                        // 换列：用一段二次贝塞尔拐过去，控制点放在起点正下方，
                        // 线就会先沿原列走一小段再平滑地拐进新列（gitk / JetBrains
                        // 都是这个观感），比直接斜拉过去干净。
                        let ctrl = point(from_x, to_y);
                        pb.curve_to(point(to_x, to_y), ctrl);
                    }
                    if let Ok(path) = pb.build() {
                        window.paint_path(path, color);
                    }
                }

                // 提交圆点画在连线之上，避免被线压住。
                let mut dot = PathBuilder::fill();
                let c = point(node_x, mid);
                let r = px(DOT_R);
                // 用四段圆弧拼一个圆。
                dot.move_to(point(c.x - r, c.y));
                dot.arc_to(point(r, r), px(0.), false, true, point(c.x + r, c.y));
                dot.arc_to(point(r, r), px(0.), false, true, point(c.x - r, c.y));
                if let Ok(path) = dot.build() {
                    window.paint_path(path, lane_color(row.node));
                }
            }
        },
    )
    .w(px(width))
}

/// 「日志」页主视图。状态一律由调用方传入，理由见文件头注释。
pub fn git_log_view(
    root: Option<String>,
    state: &GitLogState,
    head_branch: Option<String>,
    cx: &mut Context<Workspace>,
) -> Div {
    let (muted, fg, border, accent) = {
        let t = cx.theme();
        (t.muted_foreground, t.foreground, t.border, t.accent)
    };
    // 只拿句柄给回调用，不 read。
    let ws = cx.entity();

    let Some(root) = root else {
        return placeholder_view("当前会话没有工作目录", muted);
    };

    if state.commits.is_empty() {
        let hint = if state.loading { "正在读取提交历史…" } else { "没有提交记录" };
        return placeholder_view(hint, muted);
    }

    let rows = state.graph.clone();
    let commits = state.commits.clone();
    let selected = state.selected.clone();
    let gw = graph_width(&rows);
    let scroll = state.scroll.clone();

    // 提交列表：每行 = 图（只在第一行渲染整张 canvas，靠绝对定位覆盖）+ 文本列。
    // 图必须与文本行严格等高，所以行高统一走 ROW_H。
    let list = div()
        .id("git-log-list")
        .flex_1()
        .min_h_0()
        // 宽度也要夹住：只有 min_h_0 的话，行内容会横向溢出容器、直接画到右边
        // 的详情面板上（内容和面板文字叠在一起）。overflow_y_scroll 只管纵向。
        .w_full()
        .min_w_0()
        .overflow_hidden()
        // flex + flex_col 不能省：少了它，滚动容器的宽度由内容决定，行里的
        // `w_full` / `flex_1` 就失去参照——表现是作者、时间列不再各自成列，
        // 而是紧跟在标题文字后面，每行位置都不一样。Git 页的文件列表一直带着
        // 这两个，照抄时漏了。
        .flex()
        .flex_col()
        .overflow_y_scroll()
        .track_scroll(&scroll)
        .vertical_scrollbar(&scroll)
        .child(
            div()
                .relative()
                .w_full()
                .min_w_0()
                .flex_none()
                // 整张图铺在列表底层，行文字浮在上面。
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .h(px(commits.len() as f32 * ROW_H))
                        .child(graph_canvas(rows, gw)),
                )
                .child(v_flex().w_full().children(commits.iter().enumerate().map(|(i, c)| {
                    let is_sel = selected.as_deref() == Some(c.hash.as_str());
                    let hash = c.hash.clone();
                    let ws_click = ws.clone();
                    let root_click = root.clone();
                    h_flex()
                        .id(("log-row", i))
                        .h(px(ROW_H))
                        .w_full()
                        .min_w_0()
                        // 兜底：窄到连固定列都塞不下时，多出来的部分裁掉而不是
                        // 画到隔壁面板上。
                        .overflow_hidden()
                        .items_center()
                        .gap_2()
                        .pr_3()
                        .text_sm()
                        .cursor_pointer()
                        .hover(|d| d.bg(accent))
                        .when(is_sel, |d| d.bg(accent))
                        .on_click(move |_ev, _w, cx| {
                            let (h, r) = (hash.clone(), root_click.clone());
                            ws_click.update(cx, |this, cx| this.select_commit(r, h, cx));
                        })
                        // 给分支图让出位置。
                        .child(div().w(px(gw)).flex_none())
                        // 引用标签（分支 / tag）。最多两个、每个限宽——CI 生成的
                        // tag 名能长到 `3242.67318.0.f5206a4c`，不限宽会把标题挤没。
                        .children(c.refs.iter().take(2).map(|r| {
                            div()
                                .flex_none()
                                .max_w(px(120.))
                                .truncate()
                                .px_1()
                                .rounded_sm()
                                .text_xs()
                                .bg(ui_theme::tint(ui_theme::blue(), 0x33))
                                .text_color(rgb(ui_theme::text_mid()))
                                .child(r.clone())
                        }))
                        .child(div().flex_1().min_w_0().truncate().text_color(fg).child(c.subject.clone()))
                        .child(
                            div()
                                .flex_none()
                                .w(px(110.))
                                .truncate()
                                .text_xs()
                                .text_color(muted)
                                .child(c.author.clone()),
                        )
                        .child(
                            div()
                                .flex_none()
                                .w(px(110.))
                                // 右对齐：今年的是 `07-16 17:53`、往年的是
                                // `2025-04-22`，宽度不一样，左对齐会参差不齐。
                                .flex()
                                .justify_end()
                                .text_xs()
                                .text_color(muted)
                                .child(fmt_time(c.time)),
                        )
                }))),
        );

    v_flex()
        .flex_1()
        .min_h_0()
        // 根节点也得夹宽度，否则里层的 flex_1 没有可收缩的参照，一路溢出到隔壁栏。
        .w_full()
        .min_w_0()
        .child(
            // 顶部：过滤条。
            h_flex()
                .w_full()
                .min_w_0()
                .gap_2()
                .px_3()
                .py_1()
                .border_b_1()
                .border_color(border)
                .text_sm()
                .text_color(muted)
                .child(div().flex_1().min_w_0().child(filter_summary(state, head_branch.as_deref())))
                .child(
                    div()
                        .id("git-log-reload")
                        .px_2()
                        .py(px(1.0))
                        .rounded_sm()
                        .cursor_pointer()
                        .text_xs()
                        .bg(accent)
                        .text_color(fg)
                        .hover(|d| d.opacity(0.8))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let root = this.cur().and_then(|s| s.cwd(cx));
                            if let Some(r) = root {
                                this.reload_git_log(r, cx);
                            }
                        }))
                        .child("刷新"),
                ),
        )
        .child(div().flex_1().min_h_0().flex().child(list))
}

/// 过滤条的文字描述。真正的下拉筛选（分支 / 用户 / 路径）后续再补，先把当前
/// 生效的范围显示出来，不然用户不知道自己在看哪一段历史。
fn filter_summary(state: &GitLogState, head: Option<&str>) -> String {
    let scope = match &state.scope {
        LogScope::Head => match head {
            Some(b) => format!("当前分支（{b}）"),
            None => "当前分支".into(),
        },
        LogScope::All => "全部分支".into(),
        LogScope::Branch(b) => b.clone(),
    };
    format!("{scope} · 最近 {} 条", state.commits.len())
}

/// Unix 秒 → `MM-DD HH:MM`。跨年时补上年份，免得把去年的提交看成今天的。
fn fmt_time(ts: i64) -> String {
    use chrono::{Datelike, Local, TimeZone};
    let Some(dt) = Local.timestamp_opt(ts, 0).single() else { return String::new() };
    let now = Local::now();
    if dt.year() == now.year() {
        dt.format("%m-%d %H:%M").to_string()
    } else {
        dt.format("%Y-%m-%d").to_string()
    }
}

/// 右侧「提交详细信息」：标题 + 作者 + 完整 sha + 该提交的 diff。
pub fn commit_detail_pane(
    root: Option<String>,
    state: &GitLogState,
    cx: &mut Context<Workspace>,
) -> Div {
    let (muted, fg, border) = {
        let t = cx.theme();
        (t.muted_foreground, t.foreground, t.border)
    };
    let Some(sel) = state.selected.as_ref() else {
        return placeholder_view("选择要查看更改的提交", muted);
    };
    let Some(c) = state.commits.iter().find(|c| &c.hash == sel) else {
        return placeholder_view("选择要查看更改的提交", muted);
    };

    v_flex()
        .flex_1()
        .min_h_0()
        .w_full()
        .min_w_0()
        .overflow_hidden()
        .child(
            v_flex()
                .w_full()
                .min_w_0()
                .gap_1()
                .px_3()
                .py_2()
                .border_b_1()
                .border_color(border)
                // 提交标题常常很长，这一栏只有 380px，必须截断。
                .child(div().w_full().min_w_0().truncate().text_sm().text_color(fg).child(c.subject.clone()))
                .child(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .gap_2()
                        .text_xs()
                        .text_color(muted)
                        .child(div().flex_1().min_w_0().truncate().child(c.author.clone()))
                        .child(div().flex_none().child(fmt_time(c.time)))
                        .child(
                            div()
                                .flex_none()
                                .font_family(crate::terminal_view::font_family())
                                .child(c.short.clone()),
                        ),
                ),
        )
        .child(
            div()
                .flex_1()
                .min_h_0()
                .w_full()
                .min_w_0()
                .child(if state.detail_loading {
                    placeholder_view("正在读取这次提交的改动…", muted)
                } else {
                    commit_diff_list(root, state, cx)
                }),
        )
}

/// 提交详情里的文件列表：点一个文件在下方看它的 diff。
fn commit_diff_list(
    root: Option<String>,
    state: &GitLogState,
    cx: &mut Context<Workspace>,
) -> Div {
    let (muted, fg, accent) = {
        let t = cx.theme();
        (t.muted_foreground, t.foreground, t.accent)
    };
    if state.detail_files.is_empty() {
        return placeholder_view("这次提交没有文件改动", muted);
    }
    let ws = cx.entity();
    let files = state.detail_files.clone();
    let sel_file = state.detail_selected_file.clone();
    let diff_lines = state.detail_diff.clone();
    let diff_scroll = state.detail_scroll.clone();

    let file_list = v_flex().flex_none().max_h(px(160.)).p_1().children(
        files.into_iter().enumerate().map(|(i, (st, path))| {
            let is_sel = sel_file.as_deref() == Some(path.as_str());
            let ws_click = ws.clone();
            let (p, r) = (path.clone(), root.clone());
            h_flex()
                .id(("commit-file", i))
                .gap_2()
                .px_2()
                .py(px(1.0))
                .text_sm()
                .rounded_sm()
                .cursor_pointer()
                .hover(|d| d.bg(accent))
                .when(is_sel, |d| d.bg(accent))
                .on_click(move |_ev, _w, cx| {
                    let (p, r) = (p.clone(), r.clone());
                    ws_click.update(cx, |this, cx| {
                        if let Some(r) = r {
                            this.select_commit_file(r, p, cx);
                        }
                    });
                })
                .child(div().flex_none().w(px(14.)).text_xs().text_color(muted).child(st))
                .child(div().flex_1().min_w_0().truncate().text_color(fg).child(path))
        }),
    );

    // 选中文件的 diff：复用 Git 页的行渲染，配色与行内高亮保持一致。
    let diff = if sel_file.is_none() {
        placeholder_view("选择一个文件查看这次提交的改动", muted).into_any_element()
    } else if diff_lines.is_empty() {
        placeholder_view("正在读取…", muted).into_any_element()
    } else {
        div()
            .id("commit-diff")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&diff_scroll)
            .vertical_scrollbar(&diff_scroll)
            .font_family(crate::terminal_view::font_family())
            .text_sm()
            .children({
                let gw = crate::git_panel::gutter_width(&diff_lines);
                diff_lines
                    .iter()
                    .map(move |l| crate::git_panel::render_readonly_diff_line(l, gw))
                    .collect::<Vec<_>>()
            })
            .into_any_element()
    };

    v_flex().flex_1().min_h_0().child(file_list).child(diff)
}

/// 左侧分支树。
///
/// 顶部两个固定项：「当前分支」（默认，对应 HEAD）和「全部分支」。下面按本地 /
/// 远程列出具体分支，当前检出的那个带 ● 标记——不然一堆分支名摆在那儿，根本看不
/// 出自己站在哪条线上。
pub fn branch_tree(
    root: Option<String>,
    branches: Option<&crate::git_panel::BranchList>,
    scope: &LogScope,
    // head_branch：当前检出的分支名，用于打 ● 标记。
    head_branch: Option<String>,
    cx: &mut Context<Workspace>,
) -> Div {
    let (muted, fg, accent) = {
        let t = cx.theme();
        (t.muted_foreground, t.foreground, t.accent)
    };
    let ws = cx.entity();
    let (local, remote) = match branches {
        Some(b) => (b.local_names().to_vec(), b.remote_names().to_vec()),
        None => (Vec::new(), Vec::new()),
    };

    // 顶部固定项：点一下切换日志范围。
    let fixed = |id: &'static str,
                 label: String,
                 target: LogScope,
                 on: bool,
                 ws: Entity<Workspace>,
                 root: Option<String>| {
        h_flex()
            .id(id)
            .gap_1()
            .px_2()
            .py(px(1.0))
            .text_sm()
            .rounded_sm()
            .cursor_pointer()
            .hover(|d| d.bg(accent))
            .when(on, |d| d.bg(accent))
            .on_click(move |_ev, _w, cx| {
                let (r, s) = (root.clone(), target.clone());
                ws.update(cx, |this, cx| {
                    if let Some(r) = r {
                        this.set_log_scope(r, s, cx);
                    }
                });
            })
            .child(div().min_w_0().truncate().text_color(fg).child(label))
    };

    let group = |title: &'static str,
                 names: Vec<String>,
                 scope: LogScope,
                 head: Option<String>,
                 ws: Entity<Workspace>,
                 root: Option<String>,
                 is_remote: bool| {
        v_flex()
            .child(div().px_2().py(px(1.0)).text_xs().text_color(muted).child(title))
            .children(names.into_iter().enumerate().map(move |(i, name)| {
                let on = scope == LogScope::Branch(name.clone());
                let is_head = head.as_deref() == Some(name.as_str());
                let (ws2, r2, n2) = (ws.clone(), root.clone(), name.clone());
                // 右键菜单要用的副本（上面那组会被 on_click 闭包吃掉）。
                let (ws2b, r2b, head2) = (ws.clone(), root.clone(), head.clone());
                h_flex()
                    .id((title, i))
                    .gap_1()
                    .pl_3()
                    .pr_2()
                    .py(px(1.0))
                    .text_sm()
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(|d| d.bg(accent))
                    .when(on, |d| d.bg(accent))
                    .on_click(move |_ev, _w, cx| {
                        let (r, n) = (r2.clone(), n2.clone());
                        ws2.update(cx, |this, cx| {
                            if let Some(r) = r {
                                this.set_log_scope(r, LogScope::Branch(n), cx);
                            }
                        });
                    })
                    // 当前检出的分支打个实心点，其余留同宽空位保持对齐。
                    .child(
                        div()
                            .w(px(6.))
                            .flex_none()
                            .text_xs()
                            .text_color(rgb(ui_theme::green()))
                            .child(if is_head { "●" } else { "" }),
                    )
                    // 分支名常常比这一栏宽（feature/xxx-yyy 之类），截断后挂
                    // tooltip 才看得全。
                    .child(
                        div()
                            .id(("branch-name", i))
                            .min_w_0()
                            .truncate()
                            .text_color(fg)
                            .child(name.clone())
                            .tooltip({
                                let n = name.clone();
                                move |window, cx| {
                                    gpui_component::tooltip::Tooltip::new(n.clone())
                                        .build(window, cx)
                                }
                            }),
                    )
                    // 右键：签出 / 合并到当前 / 删除。远端分支签出时 git 的 DWIM
                    // 会自动建同名本地跟踪分支，所以两边用同一条命令。
                    .context_menu({
                        let (ws3, r3, n3, head3) = (ws2b.clone(), r2b.clone(), name.clone(), head2.clone());
                        move |menu, _window, _cx| {
                            let is_head = head3.as_deref() == Some(n3.as_str());
                            let (ws_co, r_co, n_co) = (ws3.clone(), r3.clone(), n3.clone());
                            let (ws_mg, r_mg, n_mg) = (ws3.clone(), r3.clone(), n3.clone());
                            let (ws_del, r_del, n_del) = (ws3.clone(), r3.clone(), n3.clone());
                            let remote = is_remote;
                            let mut menu = menu;
                            if is_head {
                                // 当前分支不能签出到自己、也不能合并/删除自己。
                                menu = menu.item(PopupMenuItem::new("（当前分支）"));
                                return menu;
                            }
                            menu = menu
                                .item(PopupMenuItem::new("签出").on_click(
                                    move |_ev, _window, cx| {
                                        let r = r_co.clone();
                                        // 远程分支要用短名（去掉 `<remote>/` 前缀）走 git
                                        // 内建 DWIM 自动建跟踪分支；直接传 `origin/xxx`
                                        // 全名会变成 detached HEAD。Git 页的分支下拉早
                                        // 就这么处理了，这里别再踩一遍。
                                        let n = if remote {
                                            n_co.split_once('/')
                                                .map(|(_, b)| b.to_string())
                                                .unwrap_or_else(|| n_co.clone())
                                        } else {
                                            n_co.clone()
                                        };
                                        ws_co.update(cx, |this, cx| {
                                            if let Some(r) = r {
                                                this.checkout_branch(r, n, cx);
                                            }
                                        });
                                    },
                                ))
                                .item(PopupMenuItem::new("合并到当前分支").on_click(
                                    move |_ev, _window, cx| {
                                        let (r, n) = (r_mg.clone(), n_mg.clone());
                                        ws_mg.update(cx, |this, cx| {
                                            if let Some(r) = r {
                                                this.merge_branch(r, n, cx);
                                            }
                                        });
                                    },
                                ))
                                .separator()
                                .item(
                                    PopupMenuItem::new(if remote {
                                        "删除远端分支…"
                                    } else {
                                        "删除分支…"
                                    })
                                    .on_click(move |_ev, _window, cx| {
                                        let (r, n) = (r_del.clone(), n_del.clone());
                                        ws_del.update(cx, |this, cx| {
                                            if let Some(r) = r {
                                                this.start_delete_branch(r, n, remote, cx);
                                            }
                                        });
                                    }),
                                );
                            menu
                        }
                    })
            }))
    };

    let head_label = match head_branch.as_deref() {
        Some(b) => format!("当前分支（{b}）"),
        None => "当前分支".to_string(),
    };
    v_flex()
        .size_full()
        .min_h_0()
        .min_w_0()
        .p_1()
        .child(fixed(
            "git-log-head",
            head_label,
            LogScope::Head,
            *scope == LogScope::Head,
            ws.clone(),
            root.clone(),
        ))
        .child(fixed(
            "git-log-all-branches",
            "全部分支".into(),
            LogScope::All,
            *scope == LogScope::All,
            ws.clone(),
            root.clone(),
        ))
        .child(group(
            "本地",
            local,
            scope.clone(),
            head_branch.clone(),
            ws.clone(),
            root.clone(),
            false,
        ))
        .child(group("远程", remote, scope.clone(), head_branch, ws, root, true))
}
