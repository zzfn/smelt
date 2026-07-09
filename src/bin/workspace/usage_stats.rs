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
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp")).join(".claude").join("projects")
}

/// 扫描 `~/.claude/projects/` 下所有会话 transcript，聚合成用量 + 工具调用事件。
/// 只读，视本地历史量可能要几十到几百毫秒——调用方应放后台线程跑，不要在 render 里同步调。
pub fn scan() -> UsageData {
    scan_root(&projects_root())
}

fn scan_root(root: &Path) -> UsageData {
    let mut data = UsageData::default();
    let Ok(project_dirs) = std::fs::read_dir(root) else { return data };
    for entry in project_dirs.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let project_key = entry.file_name().to_string_lossy().into_owned();
        let mut project_label = project_key.clone();
        let Ok(files) = std::fs::read_dir(&path) else { continue };
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
fn parse_transcript(path: &Path, project_key: &str, project_label: &mut String, out: &mut UsageData) {
    let Ok(text) = std::fs::read_to_string(path) else { return };
    let mut seen_uuids: HashSet<String> = HashSet::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(raw) = serde_json::from_str::<RawLine>(line) else { continue };
        if raw.kind.as_deref() != Some("assistant") {
            continue;
        }
        if let Some(uuid) = &raw.uuid {
            if !seen_uuids.insert(uuid.clone()) {
                continue;
            }
        }
        let Some(ts) = raw.timestamp.as_deref().and_then(|s| DateTime::parse_from_rfc3339(s).ok())
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
                    out.tools.push(ToolEvent { project_label: project_label.clone(), tool: name.to_string() });
                }
            }
        }
        if let (Some(model), Some(usage)) = (msg.model, msg.usage) {
            let tokens = usage.input_tokens
                + usage.output_tokens
                + usage.cache_creation_input_tokens
                + usage.cache_read_input_tokens;
            if tokens > 0 {
                out.events.push(UsageEvent { ts, project_label: project_label.clone(), model, tokens });
            }
        }
    }
}

/// 按模型聚合 token 总量，按用量降序；`project_label` 为 None 表示不筛选（全局口径），
/// 传 `Some(cwd)`（如 `session.cwd(cx)`）筛选「当前项目」。
pub fn by_model(events: &[UsageEvent], project_label: Option<&str>) -> Vec<(String, u64)> {
    let mut m: HashMap<String, u64> = HashMap::new();
    for e in events.iter().filter(|e| project_label.map_or(true, |p| e.project_label == p)) {
        *m.entry(e.model.clone()).or_default() += e.tokens;
    }
    sorted_desc(m)
}

/// 按工具调用次数聚合，按次数降序。
pub fn by_tool(tools: &[ToolEvent], project_label: Option<&str>) -> Vec<(String, u64)> {
    let mut m: HashMap<String, u64> = HashMap::new();
    for t in tools.iter().filter(|t| project_label.map_or(true, |p| t.project_label == p)) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn write_transcript(dir: &Path, name: &str, lines: &[&str]) {
        std::fs::write(dir.join(name), lines.join("\n")).unwrap();
    }

    fn assistant_line(ts: &str, cwd: &str, model: &str, tool: Option<&str>, tokens: (u64, u64)) -> String {
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
                &assistant_line("2026-07-08T01:00:00Z", "/Users/c.chen/dev/proj-a", "claude-sonnet-5", Some("Read"), (10, 5)),
                &assistant_line("2026-07-08T01:05:00Z", "/Users/c.chen/dev/proj-a", "claude-sonnet-5", Some("Edit"), (20, 8)),
                "not json",
            ],
        );
        write_transcript(
            &proj_b,
            "s1.jsonl",
            &[&assistant_line("2026-07-08T02:00:00Z", "/Users/c.chen/dev/proj-b", "claude-opus-4-8", Some("Bash"), (100, 50))],
        );

        let data = scan_root(&tmp);
        std::fs::remove_dir_all(&tmp).unwrap();

        assert_eq!(data.events.len(), 3);
        assert_eq!(data.tools.len(), 3);

        let by_model_global = by_model(&data.events, None);
        assert_eq!(by_model_global.iter().find(|(m, _)| m == "claude-opus-4-8").map(|(_, v)| *v), Some(150));
        assert_eq!(by_model_global.iter().find(|(m, _)| m == "claude-sonnet-5").map(|(_, v)| *v), Some(43));

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
            by_proj.iter().find(|(p, _)| p == "/Users/c.chen/dev/proj-b").map(|(_, v)| *v),
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
        let events = vec![
            UsageEvent {
                ts: Local::now().with_timezone(&Utc),
                project_label: "p".to_string(),
                model: "m".to_string(),
                tokens: 7,
            },
        ];
        let heat = daily_heatmap(&events, 2);
        assert_eq!(heat.len(), 14);
        assert_eq!(heat.last().map(|(_, v)| *v), Some(7));
    }
}
