//! Skills 面板数据源：扫 `~/.claude/skills/*/SKILL.md`（用户级）与
//! `<项目>/.claude/skills/*/SKILL.md`（项目级），读 YAML frontmatter 的
//! `name` / `description`。
//!
//! **只读**：Claude Code 没有「启用/停用某个 skill」的开关机制（settings.json 里
//! 的 `enabledPlugins` 管的是插件，不是 skill），所以这里不做开关——放一个拨了
//! 不生效的开关比不放更糟。面板的价值在「看清有哪些能力 + 一键把 /name 填进
//! 当前会话」。
//!
//! 跟 claude_memory.rs 同一个套路：纯数据函数，后台线程扫盘，render 只读缓存。

use std::path::PathBuf;
use std::rc::Rc;

/// 一条 skill。
#[derive(Clone)]
pub struct SkillEntry {
    pub name: String,
    pub description: String,
    /// true = 项目级（`<项目>/.claude/skills`），false = 用户级（`~/.claude/skills`）。
    pub project_scope: bool,
}

/// 扫描用户级 + 项目级 skills（阻塞读盘，调用方放后台线程）。
pub fn scan_skills(project_cwd: Option<&str>) -> Vec<SkillEntry> {
    let mut out = Vec::new();
    if let Some(home) = dirs::home_dir() {
        collect_dir(&home.join(".claude/skills"), false, &mut out);
    }
    if let Some(cwd) = project_cwd {
        collect_dir(&PathBuf::from(cwd).join(".claude/skills"), true, &mut out);
    }
    // 项目级在前（更贴近手头的活），组内按名字排。
    out.sort_by(|a, b| {
        b.project_scope
            .cmp(&a.project_scope)
            .then_with(|| a.name.cmp(&b.name))
    });
    out
}

fn collect_dir(dir: &PathBuf, project_scope: bool, out: &mut Vec<SkillEntry>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let md = path.join("SKILL.md");
        let Ok(text) = std::fs::read_to_string(&md) else {
            continue;
        };
        let (name, description) = parse_frontmatter(&text);
        // frontmatter 缺 name 就退回目录名——目录名本来就是 skill 的调用名。
        let name = name.unwrap_or_else(|| {
            path.file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        });
        if name.is_empty() {
            continue;
        }
        out.push(SkillEntry {
            name,
            description: description.unwrap_or_default(),
            project_scope,
        });
    }
}

/// 从 YAML frontmatter 里取 `name` / `description`，只认最简形态
/// （`key: value`，值可带引号、可跨行缩进续行）——skill 的 frontmatter 就这几个
/// 标量字段，引全套 YAML 解析器不值当。
fn parse_frontmatter(text: &str) -> (Option<String>, Option<String>) {
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return (None, None);
    }
    let (mut name, mut description) = (None, None);
    // 当前正在续行的字段（YAML 折叠行：下一行有缩进即为上一行的续写）。
    let mut pending: Option<&'static str> = None;
    let mut buf = String::new();
    for line in lines {
        if line.trim() == "---" {
            break;
        }
        let indented = line.starts_with(' ') || line.starts_with('\t');
        if indented {
            if pending.is_some() {
                buf.push(' ');
                buf.push_str(line.trim());
            }
            continue;
        }
        // 新字段开始：先把上一段收尾。
        if let Some(key) = pending.take() {
            let v = unquote(buf.trim());
            match key {
                "name" => name = Some(v),
                _ => description = Some(v),
            }
            buf.clear();
        }
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        match k.trim() {
            "name" => {
                pending = Some("name");
                buf.push_str(v.trim());
            }
            "description" => {
                pending = Some("description");
                buf.push_str(v.trim());
            }
            _ => {}
        }
    }
    if let Some(key) = pending {
        let v = unquote(buf.trim());
        match key {
            "name" => name = Some(v),
            _ => description = Some(v),
        }
    }
    (name, description)
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    let bytes = s.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        return s[1..s.len() - 1].to_string();
    }
    s.to_string()
}

/// render 侧缓存类型别名（与 usage_cache 等同款：(取得时刻, 数据)）。
pub type SkillsCache = Option<(std::time::Instant, Rc<Vec<SkillEntry>>)>;

impl crate::Workspace {
    /// SKILLS 面板：确保缓存新鲜（>30s 或换了项目就后台重扫）。
    /// 跟 ensure_memory_list 同一套模板——读盘绝不放在 render 里同步做。
    pub(crate) fn ensure_skills(&mut self, cwd: Option<String>, cx: &mut gpui::Context<Self>) {
        use std::time::{Duration, Instant};
        let fresh = self
            .skills_cache
            .as_ref()
            .is_some_and(|(t, _)| t.elapsed() < Duration::from_secs(30))
            && self.skills_cache_cwd == cwd;
        if fresh || self.skills_inflight {
            return;
        }
        self.skills_inflight = true;
        let scan_cwd = cwd.clone();
        cx.spawn(async move |this, cx| {
            let c = scan_cwd.clone();
            let list = cx
                .background_executor()
                .spawn(async move { scan_skills(c.as_deref()) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.skills_inflight = false;
                this.skills_cache_cwd = scan_cwd;
                this.skills_cache = Some((Instant::now(), Rc::new(list)));
                cx.notify();
            });
        })
        .detach();
    }

    /// 把 `/skill-name` 送进当前会话：终端直接敲进去（不回车，留给人补参数）；
    /// ACP 会话填进输入框并聚焦。没有活动会话就什么都不做。
    pub(crate) fn send_skill_to_session(
        &mut self,
        cmd: &str,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(sess) = self.sessions.get(self.active_session) else {
            return;
        };
        match &sess.kind {
            crate::SessionKind::Acp(view) => {
                let view = view.clone();
                let text = cmd.to_string();
                view.update(cx, |v, cx| v.insert_prompt_text(&text, window, cx));
            }
            crate::SessionKind::Term { active, .. } => {
                let pane = active.clone();
                let text = cmd.to_string();
                pane.update(cx, |tv, cx| tv.type_text(&text, cx));
                self.focus_active(window, cx);
            }
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_quoted_and_folded_frontmatter() {
        let text = "---\nname: commit-work\ndescription: \"Create commits:\n  split into logical commits.\"\n---\n\n# body\n";
        let (name, desc) = super::parse_frontmatter(text);
        assert_eq!(name.as_deref(), Some("commit-work"));
        assert_eq!(
            desc.as_deref(),
            Some("Create commits: split into logical commits.")
        );
    }

    #[test]
    fn ignores_files_without_frontmatter() {
        let (name, desc) = super::parse_frontmatter("# 没有 frontmatter\n");
        assert!(name.is_none() && desc.is_none());
    }
}
