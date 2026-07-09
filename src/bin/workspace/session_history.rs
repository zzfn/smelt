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

#[cfg(test)]
mod tests {
    use super::*;

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
        std::env::set_var("HOME", &tmp);
        let sessions = list_sessions("/x/y");
        if let Some(h) = prev_home {
            std::env::set_var("HOME", h);
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
