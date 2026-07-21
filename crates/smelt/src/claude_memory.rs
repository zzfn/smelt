//! Claude Code 记忆浏览：列出某个项目下 Claude Code 攒下的长期记忆
//! （`~/.claude/projects/<项目目录>/memory/*.md`），只读展示。
//!
//! 跟 session_history.rs / usage_stats.rs 同属「Claude Code 专属层」——读的是
//! Claude Code 的私有目录结构，不是什么终端通用协议。别把这里的假设漏到通用终端层去。
//!
//! 每条记忆是一个独立 md 文件：YAML frontmatter（name / description / metadata.type）
//! + markdown 正文。目录里还有一份 `MEMORY.md` 索引，内容是各条记忆的 description
//! 汇总，列表里显示它纯属重复，所以跳过。

use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime};

use gpui::Context;

use crate::session_history::memory_dir;
use crate::Workspace;

/// 一条记忆。
#[derive(Clone, Debug)]
pub struct MemoryEntry {
    /// frontmatter 的 `name`（kebab-case slug）；缺失时回退成文件名。
    pub name: String,
    /// frontmatter 的 `description`，一句话摘要，列表里当副标题。
    pub description: String,
    /// frontmatter 之后的 markdown 正文。
    pub body: String,
    pub modified: Option<SystemTime>,
}

/// 列出某个项目的全部记忆，最近写入的排在前面（跟历史会话「最近活跃在前」一致）。
/// 读不到目录就返回空 Vec——没有记忆是正常状态，不是错误。
pub fn list_memories(cwd: &str) -> Vec<MemoryEntry> {
    list_memories_in(&memory_dir(cwd))
}

/// 对着一个具体目录列记忆。跟 list_memories 分开只为让测试能绕开 `~/.claude`
/// 的路径推导，直接喂临时目录——测的仍是这条真实代码路径。
fn list_memories_in(dir: &Path) -> Vec<MemoryEntry> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<MemoryEntry> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "md"))
        // MEMORY.md 是索引，内容 = 各条 description 的汇总，列出来是重复信息。
        .filter(|p| p.file_name().is_some_and(|n| n != "MEMORY.md"))
        .filter_map(|p| parse_memory(&p))
        .collect();
    out.sort_by(|a, b| b.modified.cmp(&a.modified));
    out
}

/// 解析一个记忆文件。frontmatter 解析是「够用就行」的手写版：只认顶层的
/// `name:` / `description:`，不引入 YAML 依赖——记忆文件是 Claude Code 自己按固定
/// 模板写的，不会出现锚点、多行标量那些花样。没有 frontmatter 也不当错误：
/// 退化成「文件名当标题、全文当正文」，至少还能看。
fn parse_memory(path: &Path) -> Option<MemoryEntry> {
    let raw = std::fs::read_to_string(path).ok()?;
    let modified = std::fs::metadata(path).ok().and_then(|m| m.modified().ok());
    let fallback_name = path.file_stem()?.to_string_lossy().to_string();

    let (front, body) = split_frontmatter(&raw);
    let mut name = String::new();
    let mut description = String::new();
    for line in front.lines() {
        // 只取顶层键：缩进行属于 metadata 子表（node_type/type/originSessionId），跳过。
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        let Some((key, val)) = line.split_once(':') else { continue };
        let val = val.trim().trim_matches('"').to_string();
        match key.trim() {
            "name" => name = val,
            "description" => description = val,
            _ => {}
        }
    }

    Some(MemoryEntry {
        name: if name.is_empty() { fallback_name } else { name },
        description,
        body: body.trim().to_string(),
        modified,
    })
}

/// 切出 `---` 包起来的 frontmatter 和其后的正文。没有 frontmatter 时返回
/// （空 frontmatter, 全文）。
fn split_frontmatter(raw: &str) -> (&str, &str) {
    let rest = match raw.strip_prefix("---\n") {
        Some(r) => r,
        None => return ("", raw),
    };
    // 结束分隔符必须独占一行。
    match rest.find("\n---\n") {
        Some(ix) => (&rest[..ix], &rest[ix + 5..]),
        None => ("", raw),
    }
}

impl Workspace {
    /// 记忆页：确保当前项目的记忆列表缓存新鲜（>10s 或缺失就后台重扫）。
    /// 跟 ensure_session_list 同一套模板——读盘绝不放在 render 里同步做。
    pub fn ensure_memory_list(&mut self, cwd: String, cx: &mut Context<Self>) {
        let fresh = self
            .memory_list
            .get(&cwd)
            .is_some_and(|(t, _)| t.elapsed() < Duration::from_secs(10));
        if fresh || self.memory_list_inflight.contains(&cwd) {
            return;
        }
        self.memory_list_inflight.insert(cwd.clone());
        cx.spawn(async move |this, cx| {
            let c = cwd.clone();
            let memories = cx.background_executor().spawn(async move { list_memories(&c) }).await;
            let _ = this.update(cx, |this, cx| {
                this.memory_list_inflight.remove(&cwd);
                this.memory_list.insert(cwd, (Instant::now(), Rc::new(memories)));
                cx.notify();
            });
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(dir: &Path, name: &str, content: &str) {
        fs::write(dir.join(name), content).unwrap();
    }

    /// 标准格式：取 frontmatter 的 name/description，正文不含 frontmatter。
    #[test]
    fn parses_frontmatter_and_body() {
        let dir = std::env::temp_dir().join(format!("smelt-mem-{}-a", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        write(
            &dir,
            "gui-sandbox.md",
            "---\nname: gui-sandbox-no-window\ndescription: 沙箱跑不出 GUI 窗口\nmetadata:\n  type: project\n  originSessionId: abc\n---\n\n正文第一行\n\n正文第二行\n",
        );

        let m = parse_memory(&dir.join("gui-sandbox.md")).expect("应能解析");
        assert_eq!(m.name, "gui-sandbox-no-window");
        assert_eq!(m.description, "沙箱跑不出 GUI 窗口");
        assert_eq!(m.body, "正文第一行\n\n正文第二行");
        // metadata 子表的缩进键不该被当成顶层 name/description 覆盖掉。
        assert!(!m.description.contains("project"));

        let _ = fs::remove_dir_all(&dir);
    }

    /// 没有 frontmatter 也得能看：文件名当标题，全文当正文，而不是整条丢掉。
    #[test]
    fn falls_back_when_no_frontmatter() {
        let dir = std::env::temp_dir().join(format!("smelt-mem-{}-b", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        write(&dir, "raw-note.md", "就是一段裸 markdown\n");

        let m = parse_memory(&dir.join("raw-note.md")).expect("应能解析");
        assert_eq!(m.name, "raw-note");
        assert_eq!(m.description, "");
        assert_eq!(m.body, "就是一段裸 markdown");

        let _ = fs::remove_dir_all(&dir);
    }

    /// MEMORY.md 是索引不是记忆，列表里必须排除；其余按修改时间倒序。
    #[test]
    fn lists_skips_index_and_sorts_by_recency() {
        let dir = std::env::temp_dir().join(format!("smelt-mem-{}-c", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        write(&dir, "MEMORY.md", "- [老的](old.md) — 索引行\n");
        write(&dir, "old.md", "---\nname: old\ndescription: 旧的\n---\n\n旧正文\n");
        // 拉开修改时间，保证排序断言稳定（同秒写入时 mtime 可能相同）。
        std::thread::sleep(std::time::Duration::from_millis(1100));
        write(&dir, "fresh.md", "---\nname: fresh\ndescription: 新的\n---\n\n新正文\n");

        let list = list_memories_in(&dir);
        assert_eq!(list.len(), 2, "MEMORY.md 索引不该出现在列表里");
        assert_eq!(list[0].name, "fresh", "最近写入的排最前");
        assert_eq!(list[1].name, "old");

        let _ = fs::remove_dir_all(&dir);
    }

    /// 没有 memory 目录（大多数项目都没有）时安静返回空列表，不 panic。
    #[test]
    fn missing_dir_yields_empty_list() {
        let dir = std::env::temp_dir().join("smelt-mem-does-not-exist-at-all");
        let _ = fs::remove_dir_all(&dir);
        assert!(list_memories_in(&dir).is_empty());
    }
}
