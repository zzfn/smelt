//! Claude Code 用量统计：读本地会话 transcript（`~/.claude/projects/**/*.jsonl`，
//! Claude Code 自己写的完整历史），按 assistant 消息聚合 token 用量 + 工具调用次数，
//! 供「用量」页画按工具 / 按模型 / 按项目拆分 + 今日走势 + 活动热力图。
//! 只读本地已有文件，不需要额外 hook；只看 token 数量，不折算价格（单价表会过时）。

use chrono::{DateTime, Local, NaiveDate, Utc};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// 一条 assistant 消息的用量（input+output+两种 cache token 相加）。
#[derive(Clone, Debug)]
pub struct UsageEvent {
    pub ts: DateTime<Utc>,
    /// 项目路径展示 / 分组 key（该项目任一消息的 cwd；取不到就回退用编码目录名）。
    pub project_label: String,
    pub model: String,
    pub tokens: u64,
}

/// 一次工具调用（没有独立 token 成本，只统计次数）。
#[derive(Clone, Debug)]
pub struct ToolEvent {
    /// 同 UsageEvent::project_label——用真实 cwd 而非编码目录名，方便调用方直接拿
    /// `session.cwd(cx)` 筛选「当前项目」，不用反向还原 Claude Code 的目录编码规则。
    pub project_label: String,
    pub tool: String,
}

#[derive(Default)]
pub struct UsageData {
    pub events: Vec<UsageEvent>,
    pub tools: Vec<ToolEvent>,
}

#[derive(Deserialize)]
struct RawLine {
    #[serde(rename = "type")]
    kind: Option<String>,
    message: Option<RawMessage>,
    timestamp: Option<String>,
    cwd: Option<String>,
    uuid: Option<String>,
}

#[derive(Deserialize)]
struct RawMessage {
    model: Option<String>,
    usage: Option<RawUsage>,
    #[serde(default)]
    content: Vec<serde_json::Value>,
}

#[derive(Deserialize, Default)]
struct RawUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
}

pub fn projects_root() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".claude")
        .join("projects")
}

/// 扫描 `~/.claude/projects/` 下所有会话 transcript，聚合成用量 + 工具调用事件。
/// 只读，视本地历史量可能要几十到几百毫秒——调用方应放后台线程跑，不要在 render 里同步调。
pub fn scan() -> UsageData {
    scan_root(&projects_root())
}

fn scan_root(root: &Path) -> UsageData {
    let mut data = UsageData::default();
    let Ok(project_dirs) = std::fs::read_dir(root) else {
        return data;
    };
    for entry in project_dirs.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let project_key = entry.file_name().to_string_lossy().into_owned();
        let mut project_label = project_key.clone();
        let Ok(files) = std::fs::read_dir(&path) else {
            continue;
        };
        for file in files.flatten() {
            let fpath = file.path();
            if fpath.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            parse_transcript(&fpath, &project_key, &mut project_label, &mut data);
        }
    }
    data
}

/// 解析单个会话 transcript，把 assistant 消息里的 tool_use / usage 追加进 `out`。
/// 单行解析失败就跳过，不中断整份文件；`project_label` 取该项目任一消息的 cwd
/// （只在还没取到、即仍等于 project_key 时更新一次）。
///
/// 不按 isSidechain 过滤子代理消息——子代理（Task 工具派生）产生的 token 是真实
/// 消耗，该算进用量；真正的子代理转录文件存在独立的 `subagents/` 子目录里，
/// scan_root 用非递归 read_dir 已经天然跳过了那些文件（参考 codux 同款处理）。
/// 按 `uuid` 去重，防止同一条消息因日志重写/追加异常被重复计数。
fn parse_transcript(
    path: &Path,
    project_key: &str,
    project_label: &mut String,
    out: &mut UsageData,
) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let mut seen_uuids: HashSet<String> = HashSet::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(raw) = serde_json::from_str::<RawLine>(line) else {
            continue;
        };
        if raw.kind.as_deref() != Some("assistant") {
            continue;
        }
        if let Some(uuid) = &raw.uuid {
            if !seen_uuids.insert(uuid.clone()) {
                continue;
            }
        }
        let Some(ts) = raw
            .timestamp
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        else {
            continue;
        };
        let ts = ts.with_timezone(&Utc);
        if let Some(cwd) = &raw.cwd {
            if project_label == project_key {
                *project_label = cwd.clone();
            }
        }
        let Some(msg) = raw.message else { continue };
        for block in &msg.content {
            if block.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                if let Some(name) = block.get("name").and_then(|v| v.as_str()) {
                    out.tools.push(ToolEvent {
                        project_label: project_label.clone(),
                        tool: name.to_string(),
                    });
                }
            }
        }
        if let (Some(model), Some(usage)) = (msg.model, msg.usage) {
            let tokens = usage.input_tokens
                + usage.output_tokens
                + usage.cache_creation_input_tokens
                + usage.cache_read_input_tokens;
            if tokens > 0 {
                out.events.push(UsageEvent {
                    ts,
                    project_label: project_label.clone(),
                    model,
                    tokens,
                });
            }
        }
    }
}

/// 按模型聚合 token 总量，按用量降序；`project_label` 为 None 表示不筛选（全局口径），
/// 传 `Some(cwd)`（如 `session.cwd(cx)`）筛选「当前项目」。
pub fn by_model(events: &[UsageEvent], project_label: Option<&str>) -> Vec<(String, u64)> {
    let mut m: HashMap<String, u64> = HashMap::new();
    for e in events
        .iter()
        .filter(|e| project_label.map_or(true, |p| e.project_label == p))
    {
        *m.entry(e.model.clone()).or_default() += e.tokens;
    }
    sorted_desc(m)
}

/// 按工具调用次数聚合，按次数降序。
pub fn by_tool(tools: &[ToolEvent], project_label: Option<&str>) -> Vec<(String, u64)> {
    let mut m: HashMap<String, u64> = HashMap::new();
    for t in tools
        .iter()
        .filter(|t| project_label.map_or(true, |p| t.project_label == p))
    {
        *m.entry(t.tool.clone()).or_default() += 1;
    }
    sorted_desc(m)
}

/// 按项目聚合 token 总量（全局汇总用），按用量降序，用更友好的 project_label 展示。
pub fn by_project(events: &[UsageEvent]) -> Vec<(String, u64)> {
    let mut m: HashMap<String, u64> = HashMap::new();
    for e in events {
        *m.entry(e.project_label.clone()).or_default() += e.tokens;
    }
    sorted_desc(m)
}

fn sorted_desc(m: HashMap<String, u64>) -> Vec<(String, u64)> {
    let mut v: Vec<_> = m.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v
}

/// 过去 `weeks` 周每天的 token 总量（本地时区，含今天），按日期升序；供活动热力图用。
/// 全局口径，不分项目。
pub fn daily_heatmap(events: &[UsageEvent], weeks: i64) -> Vec<(NaiveDate, u64)> {
    let today = Local::now().date_naive();
    let start = today - chrono::Duration::days(weeks * 7 - 1);
    let mut m: HashMap<NaiveDate, u64> = HashMap::new();
    for e in events {
        let d = e.ts.with_timezone(&Local).date_naive();
        if d >= start && d <= today {
            *m.entry(d).or_default() += e.tokens;
        }
    }
    let mut days = Vec::new();
    let mut d = start;
    while d <= today {
        days.push((d, m.get(&d).copied().unwrap_or(0)));
        d += chrono::Duration::days(1);
    }
    days
}

// ===================== GPUI 面板 =====================
//
// 以上是纯逻辑（无 GPUI 依赖，好单测）；以下是从 main.rs 拆过来的面板部分——
// `impl Workspace` 方法 + 渲染函数，字段仍然声明在 main.rs 的 `Workspace` struct 里。

use chrono::Datelike;
use gpui::*;
use gpui_component::chart::BarChart;
use gpui_component::*;
use std::rc::Rc;
use std::time::Instant;

use crate::{Workspace, placeholder_view};

/// 大数字加 K/M 单位（千/百万才简化，保留一位小数），token 数 / 调用数这类展示都拿它过一遍。
pub fn format_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// 只保留输入里的头部 N 项，其余合并成一项"其他"（调用方传入的 data 已经按值降序，
/// 效果就是「前 N 大 + 其余汇总」）。用来防止种类过多时柱状图 x 轴标签挤在一起。
fn cap_top_n(mut data: Vec<(String, u64)>, n: usize) -> Vec<(String, u64)> {
    if data.len() <= n {
        return data;
    }
    let rest: u64 = data.split_off(n).into_iter().map(|(_, v)| v).sum();
    if rest > 0 {
        data.push(("其他".to_string(), rest));
    }
    data
}

/// 热力格颜色：无数据用极淡的底色描边，有数据按 `sqrt(占比)` 映射透明度——避免线性
/// 映射下小数值都挤成看不出深浅的一片。
fn heat_cell_color(v: Option<u64>, max: u64, base: Hsla) -> Hsla {
    match v {
        None => base.opacity(0.0),
        Some(0) => base.opacity(0.08),
        Some(v) => {
            let t = (v as f32 / max as f32).sqrt().clamp(0.25, 1.0);
            base.opacity(t)
        }
    }
}

/// 用量页统一的卡片外壳：标题 + 右侧小字 caption + 内容。
fn usage_section(title: &str, caption: &str, muted: Hsla, border: Hsla, body: AnyElement) -> Div {
    v_flex()
        .flex_1()
        .gap_2()
        .p_3()
        .border_1()
        .border_color(border)
        .rounded(px(8.))
        .child(
            h_flex()
                .justify_between()
                .items_baseline()
                .child(div().font_semibold().child(title.to_string()))
                .child(div().text_xs().text_color(muted).child(caption.to_string())),
        )
        .child(body)
}

/// 用量页的一个「按 X 拆分」柱状图区块；data 为空时显示「无数据」占位。
fn bar_section(
    title: &str,
    muted: Hsla,
    border: Hsla,
    color: Hsla,
    data: Vec<(String, u64)>,
) -> Div {
    // 种类一多（尤其工具名，含各种 mcp__xxx__yyy 前缀）柱子会挤成一团、x 轴标签
    // 叠在一起看不清，只画头部几项，其余合并成一根"其他"柱子。
    let data = cap_top_n(data, 6);
    let total: u64 = data.iter().map(|(_, v)| *v).sum();
    let body = if data.is_empty() {
        div()
            .h(px(180.))
            .flex()
            .items_center()
            .justify_center()
            .text_color(muted)
            .text_sm()
            .child("无数据")
    } else {
        div().h(px(180.)).child(
            BarChart::new(data)
                .band(|d: &(String, u64)| d.0.clone())
                .value(|d: &(String, u64)| d.1 as f64)
                .fill(move |_, _, _, _| color)
                .tick_margin(1),
        )
    };
    usage_section(
        title,
        &format!("共 {}", format_count(total)),
        muted,
        border,
        body.into_any_element(),
    )
}

/// 用量页：本地 Claude Code 会话用量统计——今日走势 + 活动热力图（全局口径），
/// 加当前项目 / 全局汇总各自的按模型、按工具拆分（全局汇总另加按项目拆分）。
/// 数据来自 usage_stats::scan（后台扫描 `~/.claude/projects/**/*.jsonl` 缓存的结果）。
pub fn usage_view(
    cur_project: Option<String>,
    data: Option<Rc<UsageData>>,
    cx: &mut Context<Workspace>,
) -> Div {
    let (muted, c_border, chart_1, chart_2) = {
        let t = cx.theme();
        (t.muted_foreground, t.border, t.chart_1, t.chart_2)
    };

    let Some(data) = data else {
        return placeholder_view("统计中…", muted);
    };
    if data.events.is_empty() {
        return placeholder_view(
            "没有找到本地 Claude Code 会话记录（~/.claude/projects）",
            muted,
        );
    }

    // 活动热力图：近 12 周，按周对齐成「列=周，行=周一到周日」的日历格（全局口径）。
    let heat = daily_heatmap(&data.events, 12);
    let heat_total: u64 = heat.iter().map(|(_, v)| *v).sum();
    let max_heat = heat.iter().map(|(_, v)| *v).max().unwrap_or(0).max(1);
    let lead = heat
        .first()
        .map(|(d, _)| d.weekday().num_days_from_monday())
        .unwrap_or(0) as usize;
    let mut cells: Vec<Option<u64>> = std::iter::repeat(None)
        .take(lead)
        .chain(heat.iter().map(|(_, v)| Some(*v)))
        .collect();
    while cells.len() % 7 != 0 {
        cells.push(None);
    }
    let week_columns: Vec<AnyElement> = cells
        .chunks(7)
        .map(|week| {
            v_flex()
                .gap(px(2.))
                .children(week.iter().map(|cell| {
                    div()
                        .size(px(11.))
                        .rounded(px(2.))
                        .bg(heat_cell_color(*cell, max_heat, chart_1))
                }))
                .into_any_element()
        })
        .collect();
    let heatmap_section = usage_section(
        "活动热力图（近 12 周）",
        &format!("共 {} tokens", format_count(heat_total)),
        muted,
        c_border,
        h_flex()
            .gap(px(2.))
            .p_2()
            .children(week_columns)
            .into_any_element(),
    );

    // 当前项目 / 全局汇总的按模型、按工具拆分（全局另加按项目拆分）。
    let cur_model = cur_project
        .as_deref()
        .map(|p| by_model(&data.events, Some(p)))
        .unwrap_or_default();
    let cur_tool = cur_project
        .as_deref()
        .map(|p| by_tool(&data.tools, Some(p)))
        .unwrap_or_default();
    let global_model = by_model(&data.events, None);
    let global_tool = by_tool(&data.tools, None);
    let global_project = by_project(&data.events);

    let cur_project_row = h_flex()
        .gap_3()
        .child(bar_section(
            "当前项目 · 按模型",
            muted,
            c_border,
            chart_1,
            cur_model,
        ))
        .child(bar_section(
            "当前项目 · 按工具",
            muted,
            c_border,
            chart_2,
            cur_tool,
        ));

    let global_row = h_flex()
        .gap_3()
        .child(bar_section(
            "全局 · 按模型",
            muted,
            c_border,
            chart_1,
            global_model,
        ))
        .child(bar_section(
            "全局 · 按工具",
            muted,
            c_border,
            chart_2,
            global_tool,
        ))
        .child(bar_section(
            "全局 · 按项目",
            muted,
            c_border,
            chart_1,
            global_project,
        ));

    div()
        .flex_1()
        .min_h_0()
        .overflow_hidden()
        .flex()
        .flex_col()
        .gap_3()
        .p_3()
        .child(heatmap_section)
        .child(cur_project_row)
        .child(global_row)
}

/// 独立用量窗口的根 view：同 [`crate::settings::SettingsWindow`]，薄壳转手调 `render_usage_page`。
pub struct UsageWindow {
    workspace: Entity<Workspace>,
}

impl Render for UsageWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.workspace.update(cx, |ws, cx| ws.render_usage_page(cx))
    }
}

/// 独立用量窗口的单例句柄，同 [`crate::settings::SettingsWindowHandle`]。
pub struct UsageWindowHandle(pub Option<WindowHandle<Root>>);
impl Global for UsageWindowHandle {}

impl Workspace {
    /// 确保用量数据缓存新鲜（>30s 或缺失就后台重新扫描全部本地 transcript）。
    pub fn ensure_usage_data(&mut self, cx: &mut Context<Self>) {
        let fresh = self
            .usage_cache
            .as_ref()
            .is_some_and(|(t, _)| t.elapsed() < std::time::Duration::from_secs(30));
        if fresh || self.usage_inflight {
            return;
        }
        self.usage_inflight = true;
        cx.spawn(async move |this, cx| {
            let data = cx.background_executor().spawn(async move { scan() }).await;
            let _ = this.update(cx, |this, cx| {
                this.usage_inflight = false;
                this.usage_cache = Some((Instant::now(), Rc::new(data)));
                cx.notify();
            });
        })
        .detach();
    }

    /// 用量页内容：后台刷新扫描结果 + 取当前项目/全局数据拼视图。原先挂在
    /// `MainView::Usage` 分支里，拆成独立窗口（[`UsageWindow`]）后单独抽出来，
    /// 主窗口 render 循环里那个「用量页才刷新扫描」的判断也一并挪到这——独立窗口
    /// 自己每次渲染都会调，不需要再靠 self.view 门控。
    pub fn render_usage_page(&mut self, cx: &mut Context<Self>) -> Div {
        self.ensure_usage_data(cx);
        let cur_project = self.cur().and_then(|s| s.cwd(cx));
        let data = self.usage_cache.as_ref().map(|(_, d)| d.clone());
        // usage_view 内部用 flex_1/min_h_0 撑满，依赖外层是个 flex 容器（原先挂在
        // 主窗口的 flex_col 主区里），独立窗口里補上这层容器它才能正确撑满。
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(usage_view(cur_project, data, cx))
    }

    /// 打开独立用量窗口：已经开着就聚焦提到前台，不重复开第二扇。跟
    /// [`crate::settings::Workspace::open_settings_window`] 同一套薄壳模式——真正状态
    /// （usage_cache 等）还挂在这个 Workspace 实体上，薄壳每次渲染转手调回来。
    pub fn open_usage_window(&self, cx: &mut Context<Self>) {
        let workspace = cx.entity();
        cx.defer(move |cx| {
            if let Some(handle) = cx.try_global::<UsageWindowHandle>().and_then(|h| h.0) {
                if handle
                    .update(cx, |_, window, _| window.activate_window())
                    .is_ok()
                {
                    return;
                }
            }
            let bounds = WindowBounds::centered(size(px(900.), px(700.)), cx);
            let options = WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some("用量".into()),
                    ..Default::default()
                }),
                window_bounds: Some(bounds),
                ..Default::default()
            };
            let handle = cx
                .open_window(options, |window, cx| {
                    window.set_rem_size(px(19.));
                    let view = cx.new(|_cx| UsageWindow {
                        workspace: workspace.clone(),
                    });
                    cx.new(|cx| Root::new(view, window, cx))
                })
                .expect("打开用量窗口失败");
            cx.set_global(UsageWindowHandle(Some(handle)));
        });
    }
}

#[cfg(test)]
mod tests {
    // 不用 `use super::*;`：本文件后面会加入 gpui/gpui_component 的 glob 导入，
    // 带进这个测试模块会让 trait 解析图爆炸式增长，`cargo test` 编译期会撞
    // rustc 的递归限制甚至直接崩溃——只导入测试真正用到的几个名字就够了。
    use super::{UsageEvent, by_model, by_project, by_tool, daily_heatmap, scan_root};
    use chrono::{Local, Utc};
    use std::path::Path;

    fn write_transcript(dir: &Path, name: &str, lines: &[&str]) {
        std::fs::write(dir.join(name), lines.join("\n")).unwrap();
    }

    fn assistant_line(
        ts: &str,
        cwd: &str,
        model: &str,
        tool: Option<&str>,
        tokens: (u64, u64),
    ) -> String {
        let content = match tool {
            Some(t) => format!(r#"[{{"type":"tool_use","name":"{t}"}}]"#),
            None => "[]".to_string(),
        };
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","cwd":"{cwd}","isSidechain":false,"message":{{"model":"{model}","content":{content},"usage":{{"input_tokens":{},"output_tokens":{},"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}}}}"#,
            tokens.0, tokens.1
        )
    }

    #[test]
    fn scans_and_aggregates_by_model_tool_and_project() {
        let tmp = std::env::temp_dir().join("smelt-usage-stats-test-basic");
        let _ = std::fs::remove_dir_all(&tmp);
        let proj_a = tmp.join("-Users-c-chen-dev-proj-a");
        let proj_b = tmp.join("-Users-c-chen-dev-proj-b");
        std::fs::create_dir_all(&proj_a).unwrap();
        std::fs::create_dir_all(&proj_b).unwrap();

        write_transcript(
            &proj_a,
            "s1.jsonl",
            &[
                &assistant_line(
                    "2026-07-08T01:00:00Z",
                    "/Users/c.chen/dev/proj-a",
                    "claude-sonnet-5",
                    Some("Read"),
                    (10, 5),
                ),
                &assistant_line(
                    "2026-07-08T01:05:00Z",
                    "/Users/c.chen/dev/proj-a",
                    "claude-sonnet-5",
                    Some("Edit"),
                    (20, 8),
                ),
                "not json",
            ],
        );
        write_transcript(
            &proj_b,
            "s1.jsonl",
            &[&assistant_line(
                "2026-07-08T02:00:00Z",
                "/Users/c.chen/dev/proj-b",
                "claude-opus-4-8",
                Some("Bash"),
                (100, 50),
            )],
        );

        let data = scan_root(&tmp);
        std::fs::remove_dir_all(&tmp).unwrap();

        assert_eq!(data.events.len(), 3);
        assert_eq!(data.tools.len(), 3);

        let by_model_global = by_model(&data.events, None);
        assert_eq!(
            by_model_global
                .iter()
                .find(|(m, _)| m == "claude-opus-4-8")
                .map(|(_, v)| *v),
            Some(150)
        );
        assert_eq!(
            by_model_global
                .iter()
                .find(|(m, _)| m == "claude-sonnet-5")
                .map(|(_, v)| *v),
            Some(43)
        );

        let label_a = "/Users/c.chen/dev/proj-a".to_string();
        let by_model_a = by_model(&data.events, Some(&label_a));
        assert_eq!(by_model_a, vec![("claude-sonnet-5".to_string(), 43)]);

        let by_tool_a = by_tool(&data.tools, Some(&label_a));
        assert_eq!(by_tool_a.len(), 2);
        assert!(by_tool_a.contains(&("Read".to_string(), 1)));
        assert!(by_tool_a.contains(&("Edit".to_string(), 1)));

        let by_proj = by_project(&data.events);
        assert_eq!(by_proj.len(), 2);
        assert_eq!(
            by_proj
                .iter()
                .find(|(p, _)| p == "/Users/c.chen/dev/proj-b")
                .map(|(_, v)| *v),
            Some(150)
        );
    }

    #[test]
    fn sidechain_events_are_counted() {
        // 子代理（Task 工具派生）消息也是真实 token 消耗，不该被 isSidechain 挡掉——
        // 真正该排除的子代理转录文件本来就在独立的 subagents/ 子目录里。
        let tmp = std::env::temp_dir().join("smelt-usage-stats-test-sidechain");
        let _ = std::fs::remove_dir_all(&tmp);
        let proj = tmp.join("-proj");
        std::fs::create_dir_all(&proj).unwrap();
        let line = r#"{"type":"assistant","timestamp":"2026-07-08T01:00:00Z","cwd":"/x","isSidechain":true,"message":{"model":"claude-sonnet-5","content":[],"usage":{"input_tokens":10,"output_tokens":5,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#;
        write_transcript(&proj, "s1.jsonl", &[line]);

        let data = scan_root(&tmp);
        std::fs::remove_dir_all(&tmp).unwrap();
        assert_eq!(data.events.len(), 1);
        assert_eq!(data.events[0].tokens, 15);
    }

    #[test]
    fn duplicate_uuid_is_only_counted_once() {
        let tmp = std::env::temp_dir().join("smelt-usage-stats-test-dedup");
        let _ = std::fs::remove_dir_all(&tmp);
        let proj = tmp.join("-proj");
        std::fs::create_dir_all(&proj).unwrap();
        let line = r#"{"type":"assistant","uuid":"dup-1","timestamp":"2026-07-08T01:00:00Z","cwd":"/x","message":{"model":"claude-sonnet-5","content":[],"usage":{"input_tokens":10,"output_tokens":5,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#;
        // 同一条消息（同 uuid）在文件里出现两次，模拟日志重写/追加异常。
        write_transcript(&proj, "s1.jsonl", &[line, line]);

        let data = scan_root(&tmp);
        std::fs::remove_dir_all(&tmp).unwrap();
        assert_eq!(data.events.len(), 1);
    }

    #[test]
    fn daily_heatmap_covers_requested_week_span_including_today() {
        let events = vec![UsageEvent {
            ts: Local::now().with_timezone(&Utc),
            project_label: "p".to_string(),
            model: "m".to_string(),
            tokens: 7,
        }];
        let heat = daily_heatmap(&events, 2);
        assert_eq!(heat.len(), 14);
        assert_eq!(heat.last().map(|(_, v)| *v), Some(7));
    }
}
