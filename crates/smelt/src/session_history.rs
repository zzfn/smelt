//! 历史会话浏览：列出某个项目下 Claude Code 本地保存的历史会话
//! （`~/.claude/projects/<项目目录>/*.jsonl`），点开能看完整对话内容（只读浏览，
//! 不支持 resume）。跟 usage_stats.rs 读的是同一份数据源，但目的不同——那边统计
//! 聚合数字，这里还原对话本身。

use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// 项目目录编码规则：Claude Code 把项目路径里的 `/` 和 `.` 都换成 `-`
/// （已经拿 codux 的实现 `project_path.replace('/', '-').replace('.', '-')` 印证过，
/// 跟本机实测的编码目录名完全对得上）。
fn project_dir(cwd: &str) -> String {
    cwd.replace('/', "-").replace('.', "-")
}

fn projects_root() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp")).join(".claude").join("projects")
}

/// 某个项目的记忆目录（`<项目目录>/memory`）。编码规则只有这一份，claude_memory.rs
/// 从这里取，别再复制一遍 project_dir——规则一旦变，两处会悄悄不一致。
pub(crate) fn memory_dir(cwd: &str) -> PathBuf {
    projects_root().join(project_dir(cwd)).join("memory")
}

/// 一份历史会话的概览（列表用）。
#[derive(Clone)]
pub struct SessionSummary {
    pub path: PathBuf,
    /// 首条用户消息文本（截断），取不到就回退用 session id（文件名去掉扩展名）。
    pub title: String,
    pub started_at: Option<DateTime<Utc>>,
    pub last_active_at: Option<DateTime<Utc>>,
    /// user + assistant 消息总数（不含被跳过的 tool_result / 内部记录）。
    pub message_count: usize,
    /// 本份会话消耗的 token 总量（input+output+两种 cache 相加，算法跟 usage_stats
    /// 一致），供总览卡片展示「当前会话」口径的用量——跟用量页的整项目累计口径不同。
    pub total_tokens: u64,
    /// 最近一次工具调用名（按文件行序，最后一个 tool_use 块），供总览卡片展示。
    pub last_tool: Option<String>,
}

/// 一轮对话：用户发言 / Claude 回复（含它这轮调用了哪些工具）。
pub struct Turn {
    pub is_user: bool,
    pub timestamp: Option<DateTime<Utc>>,
    pub text: String,
    /// 这轮里 assistant 调用的工具名（user 轮恒为空）。
    pub tools: Vec<String>,
}

pub struct SessionDetail {
    pub turns: Vec<Turn>,
}

/// 列出某个项目目录下的所有历史会话，按最近活跃时间降序。
/// 只读扫描，可能要几十毫秒（视会话数量），调用方应放后台线程跑。
pub fn list_sessions(cwd: &str) -> Vec<SessionSummary> {
    let dir = projects_root().join(project_dir(cwd));
    let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut out: Vec<SessionSummary> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .filter_map(|path| summarize_session(&path))
        .collect();
    out.sort_by(|a, b| b.last_active_at.cmp(&a.last_active_at));
    out
}

fn summarize_session(path: &Path) -> Option<SessionSummary> {
    let text = std::fs::read_to_string(path).ok()?;
    let session_id = path.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown").to_string();

    let mut title: Option<String> = None;
    let mut started_at: Option<DateTime<Utc>> = None;
    let mut last_active_at: Option<DateTime<Utc>> = None;
    let mut message_count = 0usize;
    let mut total_tokens = 0u64;
    let mut last_tool: Option<String> = None;
    let mut seen_uuids: HashSet<String> = HashSet::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(line) else { continue };
        let Some(kind) = row.get("type").and_then(|v| v.as_str()) else { continue };
        if kind != "user" && kind != "assistant" {
            continue;
        }
        let ts = row
            .get("timestamp")
            .and_then(|v| v.as_str())
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&Utc));
        if let Some(ts) = ts {
            started_at = Some(started_at.map_or(ts, |s: DateTime<Utc>| s.min(ts)));
            last_active_at = Some(last_active_at.map_or(ts, |l: DateTime<Utc>| l.max(ts)));
        }

        if kind == "user" {
            // content 是纯字符串才算真实用户发言；数组形态是 tool_result 回填，不计数、不当标题。
            if let Some(text) = row.get("message").and_then(|m| m.get("content")).and_then(|c| c.as_str()) {
                message_count += 1;
                if title.is_none() && !text.trim().is_empty() {
                    title = Some(truncate(text.trim(), 80));
                }
            }
        } else {
            // assistant：content 数组里只要有 text 块就算一条消息；同 uuid 只算一次
            // （日志重写/追加异常会重复），token 累加算法跟 usage_stats 保持一致。
            let dup = row
                .get("uuid")
                .and_then(|v| v.as_str())
                .is_some_and(|u| !seen_uuids.insert(u.to_string()));
            let blocks = row.get("message").and_then(|m| m.get("content")).and_then(|c| c.as_array());
            let has_text = blocks
                .is_some_and(|blocks| blocks.iter().any(|b| b.get("type").and_then(|t| t.as_str()) == Some("text")));
            if has_text {
                message_count += 1;
            }
            if let Some(blocks) = blocks {
                for b in blocks {
                    if b.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                        if let Some(name) = b.get("name").and_then(|n| n.as_str()) {
                            last_tool = Some(name.to_string());
                        }
                    }
                }
            }
            if !dup {
                if let Some(usage) = row.get("message").and_then(|m| m.get("usage")) {
                    let field = |k: &str| usage.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
                    total_tokens += field("input_tokens")
                        + field("output_tokens")
                        + field("cache_creation_input_tokens")
                        + field("cache_read_input_tokens");
                }
            }
        }
    }

    Some(SessionSummary {
        title: title.unwrap_or(session_id),
        path: path.to_path_buf(),
        started_at,
        last_active_at,
        message_count,
        total_tokens,
        last_tool,
    })
}

/// 读某一份会话 transcript，还原成 Turn 列表供浏览。
/// 跳过子代理（isSidechain）消息 —— 混进主线对话会话读起来很乱，先不做嵌套展示；
/// 也跳过纯 tool_result 的 user 消息（那是工具输出回填，不是真实用户发言，assistant
/// 轮次里的工具名已经能说明调用了什么）。
pub fn load_session_detail(path: &Path) -> Option<SessionDetail> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut turns = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(line) else { continue };
        if row.get("isSidechain").and_then(|v| v.as_bool()) == Some(true) {
            continue;
        }
        let Some(kind) = row.get("type").and_then(|v| v.as_str()) else { continue };
        if kind != "user" && kind != "assistant" {
            continue;
        }
        let timestamp = row
            .get("timestamp")
            .and_then(|v| v.as_str())
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&Utc));
        let content = row.get("message").and_then(|m| m.get("content"));

        if kind == "user" {
            let Some(text) = content.and_then(|c| c.as_str()) else { continue };
            if text.trim().is_empty() {
                continue;
            }
            turns.push(Turn { is_user: true, timestamp, text: text.to_string(), tools: Vec::new() });
        } else {
            let blocks = content.and_then(|c| c.as_array());
            let Some(blocks) = blocks else { continue };
            let text = blocks
                .iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n");
            let tools: Vec<String> = blocks
                .iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
                .filter_map(|b| b.get("name").and_then(|n| n.as_str()).map(str::to_string))
                .collect();
            if text.trim().is_empty() && tools.is_empty() {
                continue;
            }
            turns.push(Turn { is_user: false, timestamp, text, tools });
        }
    }

    Some(SessionDetail { turns })
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push('…');
    out
}

// ===================== GPUI 面板 =====================
//
// 以上是纯逻辑（无 GPUI 依赖，好单测）；以下是从 main.rs 拆过来的面板部分——
// `impl Workspace` 方法 + 渲染函数，字段仍然声明在 main.rs 的 `Workspace` struct 里。

use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::table::{Column, ColumnSort, DataTable, TableDelegate, TableEvent, TableState};
use gpui_component::text::TextView;
use gpui_component::*;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Instant;

use crate::claude_memory::MemoryEntry;
use crate::usage_stats::format_count;
use crate::{placeholder_view, Workspace};

/// 历史会话表格「时间」列文案：有明显跨度（>1 分钟）就顺带标一下这个会话跑了多久，
/// 纯单条消息的会话就只显示时间点，不必画蛇添足展示"0 分钟"。
fn session_when(s: &SessionSummary) -> String {
    match (s.started_at, s.last_active_at) {
        (Some(start), Some(last)) if (last - start).num_minutes() >= 1 => format!(
            "{} · 跑了 {} 分钟",
            last.with_timezone(&chrono::Local).format("%m-%d %H:%M"),
            (last - start).num_minutes()
        ),
        (_, Some(last)) => last.with_timezone(&chrono::Local).format("%m-%d %H:%M").to_string(),
        _ => String::new(),
    }
}

/// 历史会话表格的数据委托：持有当前项目的会话列表 + 列定义，渲染/排序都在这实现。
pub struct SessionHistoryDelegate {
    pub sessions: Rc<Vec<SessionSummary>>,
    columns: Vec<Column>,
}

impl SessionHistoryDelegate {
    fn new(sessions: Rc<Vec<SessionSummary>>) -> Self {
        Self {
            sessions,
            columns: vec![
                Column::new("title", "标题").width(px(260.)),
                Column::new("when", "时间").width(px(180.)).sortable(),
                Column::new("messages", "消息数").width(px(90.)).sortable(),
                Column::new("tokens", "Tokens").width(px(90.)).sortable(),
            ],
        }
    }
}

impl TableDelegate for SessionHistoryDelegate {
    fn columns_count(&self, _cx: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _cx: &App) -> usize {
        self.sessions.len()
    }

    fn column(&self, col_ix: usize, _cx: &App) -> Column {
        self.columns[col_ix].clone()
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let s = &self.sessions[row_ix];
        let (fg, muted) = {
            let t = cx.theme();
            (t.foreground, t.muted_foreground)
        };
        match self.columns[col_ix].key.as_ref() {
            "title" => div().text_color(fg).child(s.title.clone()).into_any_element(),
            "when" => div().text_color(muted).child(session_when(s)).into_any_element(),
            "messages" => {
                div().text_color(muted).child(s.message_count.to_string()).into_any_element()
            }
            "tokens" => div().text_color(muted).child(format_count(s.total_tokens)).into_any_element(),
            _ => Empty.into_any_element(),
        }
    }

    fn perform_sort(
        &mut self,
        col_ix: usize,
        sort: ColumnSort,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) {
        let key = self.columns[col_ix].key.clone();
        let rows = Rc::make_mut(&mut self.sessions);
        match (key.as_ref(), sort) {
            ("when", ColumnSort::Ascending) => rows.sort_by_key(|s| s.last_active_at),
            ("when", ColumnSort::Descending) => rows.sort_by_key(|s| std::cmp::Reverse(s.last_active_at)),
            ("messages", ColumnSort::Ascending) => rows.sort_by_key(|s| s.message_count),
            ("messages", ColumnSort::Descending) => rows.sort_by_key(|s| std::cmp::Reverse(s.message_count)),
            ("tokens", ColumnSort::Ascending) => rows.sort_by_key(|s| s.total_tokens),
            ("tokens", ColumnSort::Descending) => rows.sort_by_key(|s| std::cmp::Reverse(s.total_tokens)),
            // Default：不重排，维持 list_sessions 原始顺序（按时间新→旧）。
            _ => {}
        }
    }
}

/// 历史会话列表的三种状态：还没扫描完 / 扫描完但没有历史会话 / 拿到数据（表格 Entity
/// 已经就绪，见 Workspace::ensure_session_table）。
pub enum HistoryListState {
    Loading,
    Empty,
    Ready(Entity<TableState<SessionHistoryDelegate>>),
}

/// 历史会话页的两个子页，共用「左列表 + 右详情」的骨架：
/// - `Sessions`：Claude Code 存的历史对话（`*.jsonl`）
/// - `Memories`：Claude Code 攒的长期记忆（`memory/*.md`，见 claude_memory.rs）
///
/// 两者是同一个目录下的邻居数据，都属于「Claude Code 专属层」。
#[derive(Clone, Copy, PartialEq)]
pub enum HistoryPane {
    Sessions,
    Memories,
}

/// 历史会话页：左侧列出当前项目下 Claude Code 保存的历史会话，右侧显示选中会话的
/// 对话内容（只读浏览，不支持 resume）。数据来自 session_history 模块，跟「用量」
/// 页读的是同一份 `~/.claude/projects/**/*.jsonl`，但这里还原对话本身而非统计聚合。
pub fn history_view(
    pane: HistoryPane,
    list: HistoryListState,
    detail: &Option<(std::path::PathBuf, Rc<SessionDetail>)>,
    memories: Option<Rc<Vec<MemoryEntry>>>,
    memory_selected: Option<usize>,
    cx: &mut Context<Workspace>,
) -> Div {
    let (muted, fg, c_border, accent, secondary) = {
        let t = cx.theme();
        (t.muted_foreground, t.foreground, t.border, t.primary, t.secondary)
    };

    // 「会话 / 记忆」切换：两块数据是同一个项目的两种视角，共用下面的左右布局，
    // 所以做成页内切换而不是各占一个顶层 tab。
    let switcher = h_flex()
        .flex_none()
        .gap_1()
        .px_3()
        .py_2()
        .border_b_1()
        .border_color(c_border)
        .child(pane_button("会话", HistoryPane::Sessions, pane, accent, fg, muted, cx))
        .child(pane_button("记忆", HistoryPane::Memories, pane, accent, fg, muted, cx));

    if pane == HistoryPane::Memories {
        return v_flex()
            .flex_1()
            .min_h_0()
            .child(switcher)
            .child(memory_body(memories, memory_selected, muted, fg, c_border, accent, cx));
    }

    let list_body: AnyElement = match list {
        HistoryListState::Loading => placeholder_view("加载中…", muted).into_any_element(),
        HistoryListState::Empty => {
            placeholder_view("这个项目还没有本地保存的历史会话", muted).into_any_element()
        }
        HistoryListState::Ready(table) => {
            div().flex_1().min_h_0().child(DataTable::new(&table).stripe(true)).into_any_element()
        }
    };

    let detail_body: AnyElement = match detail {
        None => placeholder_view("← 选择一个历史会话查看内容", muted).into_any_element(),
        Some((_, d)) if d.turns.is_empty() => {
            placeholder_view("这份会话没有可展示的对话内容", muted).into_any_element()
        }
        Some((_, d)) => div()
            .id("session-detail")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_3()
            .p_3()
            .children(d.turns.iter().enumerate().map(|(i, t)| {
                let role = if t.is_user { "用户" } else { "Claude" };
                let role_color = if t.is_user { accent } else { fg };
                let bubble_bg = if t.is_user { accent.opacity(0.12) } else { secondary };
                // 工具名按出现顺序去重计数，多次调用同一工具合并成一行摘要
                // （比如连续 3 次 Bash 就显示"Bash ×3"），不然长会话里全是重复胶囊。
                let tool_summary = (!t.tools.is_empty()).then(|| {
                    let mut order: Vec<&String> = Vec::new();
                    let mut counts: HashMap<&String, usize> = HashMap::new();
                    for tool in &t.tools {
                        counts.entry(tool).and_modify(|c| *c += 1).or_insert_with(|| {
                            order.push(tool);
                            1
                        });
                    }
                    order
                        .into_iter()
                        .map(|name| {
                            let c = counts[name];
                            if c > 1 { format!("{name} ×{c}") } else { name.clone() }
                        })
                        .collect::<Vec<_>>()
                        .join(" · ")
                });
                v_flex()
                    .gap_1()
                    .px_3()
                    .py_2()
                    .rounded(px(8.))
                    .bg(bubble_bg)
                    .when(t.is_user, |el| el.max_w(px(560.)))
                    .child(
                        h_flex()
                            .gap_2()
                            .items_baseline()
                            .child(div().font_semibold().text_sm().text_color(role_color).child(role))
                            .children(t.timestamp.map(|ts| {
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .child(ts.with_timezone(&chrono::Local).format("%m-%d %H:%M").to_string())
                            })),
                    )
                    // 必须逐气泡给唯一 id：便捷函数 text::markdown() 拿调用处代码位置
                    // 当 id，循环里所有气泡会共享同一份 TextView 状态（文本互踩、高度
                    // 测量错乱，气泡整个叠在一起）。
                    .child(
                        div()
                            .text_sm()
                            .text_color(fg)
                            .child(TextView::markdown(("turn-md", i), t.text.clone())),
                    )
                    .children(tool_summary.map(|s| {
                        div().text_xs().text_color(muted).child(format!("🔧 {s}"))
                    }))
                    .into_any_element()
            }))
            .into_any_element(),
    };

    v_flex().flex_1().min_h_0().child(switcher).child(
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .child(
                div()
                    .w(px(280.))
                    .flex()
                    .flex_col()
                    .min_h_0()
                    .border_r_1()
                    .border_color(c_border)
                    .child(list_body),
            )
            .child(detail_body),
    )
}

/// 切换条上的一个按钮。选中态用 accent 底色标出来。
#[allow(clippy::too_many_arguments)]
fn pane_button(
    label: &'static str,
    target: HistoryPane,
    current: HistoryPane,
    accent: Hsla,
    fg: Hsla,
    muted: Hsla,
    cx: &mut Context<Workspace>,
) -> Stateful<Div> {
    let selected = target == current;
    div()
        .id(label)
        .px_3()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .text_sm()
        .text_color(if selected { fg } else { muted })
        .when(selected, |d| d.bg(accent.opacity(0.18)))
        .when(!selected, |d| d.hover(|s| s.text_color(fg)))
        .child(label)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _, _, cx| {
                if this.history_pane != target {
                    this.history_pane = target;
                    // 换子页时清掉右边的选中项，免得显示上一个子页残留的详情。
                    this.memory_selected = None;
                    cx.notify();
                }
            }),
        )
}

/// 记忆子页：左列表（标题 + 一句话描述）+ 右详情（markdown 全文）。
#[allow(clippy::too_many_arguments)]
fn memory_body(
    memories: Option<Rc<Vec<MemoryEntry>>>,
    selected: Option<usize>,
    muted: Hsla,
    fg: Hsla,
    c_border: Hsla,
    accent: Hsla,
    cx: &mut Context<Workspace>,
) -> Div {
    let list_body: AnyElement = match &memories {
        None => placeholder_view("加载中…", muted).into_any_element(),
        Some(list) if list.is_empty() => placeholder_view(
            "这个项目还没有记忆。Claude Code 会把值得长期记住的事写进 ~/.claude 下的 memory 目录。",
            muted,
        )
        .into_any_element(),
        Some(list) => {
            let mut col = v_flex().id("memory-list").flex_1().min_h_0().overflow_y_scroll().p_2().gap_1();
            for (ix, m) in list.iter().enumerate() {
                let is_sel = selected == Some(ix);
                col = col.child(
                    v_flex()
                        .id(("memory-row", ix))
                        .w_full()
                        .gap_0p5()
                        .px_2()
                        .py_2()
                        .rounded_md()
                        .cursor_pointer()
                        .when(is_sel, |d| d.bg(accent.opacity(0.18)))
                        .when(!is_sel, |d| d.hover(|s| s.bg(c_border.opacity(0.5))))
                        .child(div().text_sm().text_color(fg).child(m.name.clone()))
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child(truncate(&m.description, 60)),
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, cx| {
                                this.memory_selected = Some(ix);
                                cx.notify();
                            }),
                        ),
                );
            }
            col.into_any_element()
        }
    };

    let detail_body: AnyElement = match memories.as_ref().and_then(|l| selected.and_then(|ix| l.get(ix))) {
        None => placeholder_view("← 选择一条记忆查看内容", muted).into_any_element(),
        Some(m) => v_flex()
            .id("memory-detail")
            .flex_1()
            .min_h_0()
            // min_w_0 不能省：flex item 的默认 min-width 是 auto，即「不收缩到比内容更窄」。
            // 少了它，这一栏会被记忆正文里最长的那行撑开，超出窗口的部分被直接裁掉，
            // 文本也永远不会换行（会话那边没踩到，是因为气泡上有 max_w 兜着）。
            .min_w_0()
            .overflow_y_scroll()
            .p_4()
            .gap_2()
            .child(div().text_lg().text_color(fg).child(m.name.clone()))
            .children((!m.description.is_empty()).then(|| {
                div().text_sm().text_color(muted).child(m.description.clone())
            }))
            // markdown 得给唯一 id，否则跟别处的 TextView 共享状态互踩（同 turn 气泡的坑）。
            // 外面这层 w_full + min_w_0 是给正文定死一个「可用宽度」，长行才会在这个宽度
            // 上折行；不设的话它按内容宽度铺开，撑破整栏被裁掉。
            .child(
                div()
                    .w_full()
                    .min_w_0()
                    .child(TextView::markdown("memory-md", m.body.clone())),
            )
            .into_any_element(),
    };

    div()
        .flex_1()
        .min_h_0()
        .flex()
        .child(
            div()
                .w(px(280.))
                .flex()
                .flex_col()
                .min_h_0()
                .border_r_1()
                .border_color(c_border)
                .child(list_body),
        )
        .child(detail_body)
}

impl Workspace {
    /// 历史会话页：确保当前项目的会话列表缓存新鲜（>10s 或缺失就后台重新扫描）。
    pub fn ensure_session_list(&mut self, cwd: String, cx: &mut Context<Self>) {
        let fresh = self
            .session_list
            .get(&cwd)
            .is_some_and(|(t, _)| t.elapsed() < std::time::Duration::from_secs(10));
        if fresh || self.session_list_inflight.contains(&cwd) {
            return;
        }
        self.session_list_inflight.insert(cwd.clone());
        cx.spawn(async move |this, cx| {
            let c = cwd.clone();
            let sessions =
                cx.background_executor().spawn(async move { list_sessions(&c) }).await;
            let _ = this.update(cx, |this, cx| {
                this.session_list_inflight.remove(&cwd);
                this.session_list.insert(cwd, (Instant::now(), Rc::new(sessions)));
                cx.notify();
            });
        })
        .detach();
    }

    /// 历史会话页：点开一份会话，后台解析成 Turn 列表。用自增 gen 丢弃过期结果
    /// （解析期间又点了别的会话）。
    pub fn open_session_detail(&mut self, path: std::path::PathBuf, cx: &mut Context<Self>) {
        self.session_detail_gen = self.session_detail_gen.wrapping_add(1);
        let r#gen = self.session_detail_gen;
        self.session_detail = None;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let p = path.clone();
            let detail =
                cx.background_executor().spawn(async move { load_session_detail(&p) }).await;
            let _ = this.update(cx, |this, cx| {
                if this.session_detail_gen != r#gen {
                    return;
                }
                if let Some(detail) = detail {
                    this.session_detail = Some((path, Rc::new(detail)));
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 历史会话表格懒建 / 刷新：同项目（key 不变）只换 delegate 里的数据（保留排序/
    /// 滚动/选中状态）；换项目（key 变）整个重建 Entity（重置这些状态，体感上是
    /// "进了一个新页面"）。
    pub fn ensure_session_table(
        &mut self,
        key: &str,
        sessions: Rc<Vec<SessionSummary>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<TableState<SessionHistoryDelegate>> {
        if self.session_table_key.as_deref() == Some(key) {
            if let Some(table) = &self.session_table {
                table.update(cx, |t, cx| {
                    t.delegate_mut().sessions = sessions;
                    t.refresh(cx);
                });
                return table.clone();
            }
        }
        let table = cx.new(|cx| TableState::new(SessionHistoryDelegate::new(sessions), window, cx));
        self.session_table_sub =
            Some(cx.subscribe_in(&table, window, |this, table, ev: &TableEvent, _window, cx| {
                if let TableEvent::SelectRow(ix) = ev {
                    if let Some(s) = table.read(cx).delegate().sessions.get(*ix) {
                        this.open_session_detail(s.path.clone(), cx);
                    }
                }
            }));
        self.session_table_key = Some(key.to_string());
        self.session_table = Some(table.clone());
        table
    }
}

#[cfg(test)]
mod tests {
    // 不用 `use super::*;`：本文件后半段引入了 gpui/gpui_component 的 glob 导入，
    // 带进这个测试模块会让 trait 解析图爆炸式增长，`cargo test` 编译期直接撞
    // rustc 的递归限制崩溃（甚至 SIGBUS）——只导入测试真正用到的几个名字就够了。
    use super::{list_sessions, load_session_detail, project_dir};
    use std::path::Path;

    fn write(dir: &Path, name: &str, lines: &[&str]) {
        std::fs::write(dir.join(name), lines.join("\n")).unwrap();
    }

    #[test]
    fn project_dir_replaces_slashes_and_dots() {
        assert_eq!(project_dir("/Users/c.chen/dev/smelt"), "-Users-c-chen-dev-smelt");
    }

    #[test]
    fn list_sessions_summarizes_title_and_counts_and_sorts_by_recency() {
        let tmp = std::env::temp_dir().join("smelt-session-history-test-list");
        let _ = std::fs::remove_dir_all(&tmp);
        let proj_root = tmp.join(".claude").join("projects").join(project_dir("/x/y"));
        std::fs::create_dir_all(&proj_root).unwrap();

        write(
            &proj_root,
            "older.jsonl",
            &[
                r#"{"type":"user","timestamp":"2026-07-01T00:00:00Z","message":{"content":"hello there"}}"#,
                r#"{"type":"assistant","timestamp":"2026-07-01T00:00:05Z","message":{"content":[{"type":"text","text":"hi"}]}}"#,
            ],
        );
        write(
            &proj_root,
            "newer.jsonl",
            &[r#"{"type":"user","timestamp":"2026-07-05T00:00:00Z","message":{"content":"second session"}}"#],
        );

        let prev_home = std::env::var_os("HOME");
        // Edition 2024：`set_var` 为 unsafe；测试串行跑、且只改本进程 HOME。
        unsafe {
            std::env::set_var("HOME", &tmp);
        }
        let sessions = list_sessions("/x/y");
        if let Some(h) = prev_home {
            unsafe {
                std::env::set_var("HOME", h);
            }
        }
        std::fs::remove_dir_all(&tmp).unwrap();

        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].path.file_stem().unwrap(), "newer");
        assert_eq!(sessions[0].title, "second session");
        assert_eq!(sessions[1].path.file_stem().unwrap(), "older");
        assert_eq!(sessions[1].message_count, 2);
    }

    #[test]
    fn load_session_detail_skips_tool_result_and_sidechain() {
        let tmp = std::env::temp_dir().join("smelt-session-history-test-detail.jsonl");
        write(
            &tmp.parent().unwrap().to_path_buf(),
            tmp.file_name().unwrap().to_str().unwrap(),
            &[
                r#"{"type":"user","timestamp":"2026-07-01T00:00:00Z","message":{"content":"do the thing"}}"#,
                r#"{"type":"user","timestamp":"2026-07-01T00:00:01Z","message":{"content":[{"type":"tool_result","content":"raw output"}]}}"#,
                r#"{"type":"assistant","timestamp":"2026-07-01T00:00:02Z","message":{"content":[{"type":"text","text":"done"},{"type":"tool_use","name":"Bash"}]}}"#,
                r#"{"type":"assistant","isSidechain":true,"timestamp":"2026-07-01T00:00:03Z","message":{"content":[{"type":"text","text":"subagent chatter"}]}}"#,
            ],
        );

        let detail = load_session_detail(&tmp).unwrap();
        std::fs::remove_file(&tmp).unwrap();

        assert_eq!(detail.turns.len(), 2);
        assert!(detail.turns[0].is_user);
        assert_eq!(detail.turns[0].text, "do the thing");
        assert!(!detail.turns[1].is_user);
        assert_eq!(detail.turns[1].text, "done");
        assert_eq!(detail.turns[1].tools, vec!["Bash".to_string()]);
    }
}
