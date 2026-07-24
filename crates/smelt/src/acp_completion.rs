//! ACP 对话输入框的 `@` / `/` 补全**候选源**（弹层本身在 acp_view.rs）。
//!
//! 为什么不用 gpui-component 输入框自带的 `CompletionProvider`：那套菜单的定位
//! 写死在光标**下方**（`completion_menu.rs` 的 `origin()` 固定加一个
//! `line_height`，没有任何窗口下边界检测/翻转），而我们的输入框贴着窗口底边，
//! 菜单必然画到窗口外被裁掉——实测只能看见两行残影。组件是按「编辑器占满窗口、
//! 光标在中间」设计的，我们这个形态它没覆盖。所以候选照产，弹层自己画在输入框
//! 上方。
//!
//! 为什么 `@` 插的是纯文本路径而不是协议的 `ResourceLink` block：三家 agent 都
//! 自带读文件的工具，给一段路径它们一定会用；ResourceLink 各家支持程度没实测过，
//! 赌不起「引了没反应」。等有实测再升级。

use std::rc::Rc;

/// 一条补全候选。
pub struct Candidate {
    /// 菜单里显示的主文本（`@src/main.rs` / `/compact`）。
    pub label: String,
    /// 选中后替换掉触发 token 的文本（尾部带空格，接着打字不会粘住）。
    pub insert: String,
    /// 右侧灰色说明（命令描述；文件没有就空着）。
    pub hint: String,
}

/// 触发字符决定补哪一类。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `@` → 文件路径。
    At,
    /// `/` → agent 报上来的斜杠命令。
    Slash,
}

/// 候选数上限：菜单只有那么高，全量塞进去纯属浪费。
const MAX_ITEMS: usize = 50;

/// 光标前那段正在输入的补全 token。
pub struct Trigger {
    pub kind: Kind,
    /// token 在文本中的起始字节位置（含 `@`/`/` 本身），插入时按它替换。
    pub start: usize,
    /// 触发符之后已经打的部分（过滤词，已转小写）。
    pub needle: String,
}

/// 从「光标前的文本」里认出补全 token。
///
/// 规则刻意收紧，免得正常打字时菜单乱弹：
/// - 触发符必须在行首或空白之后（邮箱 `a@b.com`、路径 `a/b` 里的符号不算）；
/// - 触发符之后不能有空白（打了空格就算这段结束了）。
pub fn detect_trigger(before_cursor: &str) -> Option<Trigger> {
    // 从光标往前扫，遇到空白就说明这个词里没有触发符。
    // 注意不能「找到最近的触发符就下结论」：`@src/ma` 里最近的是那个 `/`，
    // 但它前面是字母不算触发，真正的触发符是更前面的 `@`——扫到不合格的
    // 要继续往前找，不能直接放弃（这条被测试逮到过）。
    for (ix, ch) in before_cursor.char_indices().rev() {
        if ch.is_whitespace() {
            break;
        }
        if ch != '@' && ch != '/' {
            continue;
        }
        // 触发符前面必须是行首或空白，否则是词中间的符号（邮箱 / 路径）。
        if before_cursor[..ix]
            .chars()
            .next_back()
            .is_none_or(|c| c.is_whitespace())
        {
            return Some(Trigger {
                kind: if ch == '@' { Kind::At } else { Kind::Slash },
                start: ix,
                needle: before_cursor[ix + ch.len_utf8()..].to_lowercase(),
            });
        }
    }
    None
}

/// 按 token 产候选。`files` 由调用方缓存后传进来（见 AcpView::file_list）。
pub fn candidates(
    trigger: &Trigger,
    files: &[String],
    commands: &[(String, String)],
) -> Vec<Candidate> {
    let needle = &trigger.needle;
    match trigger.kind {
        Kind::Slash => commands
            .iter()
            .filter(|(name, _)| needle.is_empty() || name.to_lowercase().contains(needle))
            .take(MAX_ITEMS)
            .map(|(name, desc)| Candidate {
                label: format!("/{name}"),
                insert: format!("/{name} "),
                hint: desc.clone(),
            })
            .collect(),
        Kind::At => files
            .iter()
            .filter(|p| needle.is_empty() || p.to_lowercase().contains(needle))
            .take(MAX_ITEMS)
            .map(|p| Candidate {
                label: format!("@{p}"),
                insert: format!("@{p} "),
                hint: String::new(),
            })
            .collect(),
    }
}

/// 列一个目录下的文件（相对路径）。优先 `git ls-files`：它天然尊重
/// `.gitignore`，不会把 `target/` 里几万个构建产物灌进补全菜单；非 git 目录
/// 退回浅层遍历，只走两层，够用且不会在大目录上卡住。
pub fn list_files(cwd: &str) -> Rc<Vec<String>> {
    if let Some(files) = git_ls_files(cwd) {
        return Rc::new(files);
    }
    let mut out = Vec::new();
    walk(
        std::path::Path::new(cwd),
        std::path::Path::new(cwd),
        0,
        &mut out,
    );
    out.sort();
    Rc::new(out)
}

fn git_ls_files(cwd: &str) -> Option<Vec<String>> {
    let out = std::process::Command::new("git")
        .args([
            "-C",
            cwd,
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let list: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    (!list.is_empty()).then_some(list)
}

fn walk(root: &std::path::Path, dir: &std::path::Path, depth: usize, out: &mut Vec<String>) {
    if depth > 2 || out.len() > 2000 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || name == "target" || name == "node_modules" {
            continue;
        }
        if path.is_dir() {
            walk(root, &path, depth + 1, out);
        } else if let Some(rel) = path.strip_prefix(root).ok().and_then(|p| p.to_str()) {
            out.push(rel.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `@` 的候选源必须真能列出文件——列空了菜单就是个空壳，而且不会报错。
    #[test]
    fn lists_files_under_cwd() {
        let cwd = env!("CARGO_MANIFEST_DIR");
        let files = list_files(cwd);
        assert!(!files.is_empty(), "本 crate 目录下不该一个文件都列不出来");
        assert!(
            files.iter().any(|f| f.ends_with("acp_completion.rs")),
            "应当列出本文件，实际前几条：{:?}",
            &files[..files.len().min(5)]
        );
        // git ls-files 尊重 .gitignore：构建产物不该混进补全菜单。
        assert!(
            !files.iter().any(|f| f.starts_with("target/")),
            "target/ 不该出现"
        );
    }

    #[test]
    fn detects_at_and_slash_at_word_start() {
        let t = detect_trigger("@src/ma").expect("行首 @ 该触发");
        assert!(matches!(t.kind, Kind::At));
        assert_eq!(t.start, 0);
        assert_eq!(t.needle, "src/ma");

        let t = detect_trigger("看下 /comp").expect("空白后的 / 该触发");
        assert!(matches!(t.kind, Kind::Slash));
        assert_eq!(t.needle, "comp");
    }

    /// 正常打字不该乱弹菜单：词中间的符号不算触发。
    #[test]
    fn ignores_symbols_inside_words() {
        assert!(
            detect_trigger("mail a@b.com").is_none(),
            "邮箱里的 @ 不该触发"
        );
        assert!(
            detect_trigger("path src/main").is_none(),
            "路径里的 / 不该触发"
        );
        assert!(
            detect_trigger("@src/main.rs 然后").is_none(),
            "打了空格就该收"
        );
        assert!(detect_trigger("").is_none());
    }

    /// 过滤词是子串匹配，且大小写无关。
    #[test]
    fn filters_case_insensitively() {
        let files = vec!["src/Main.rs".to_string(), "docs/readme.md".to_string()];
        let t = detect_trigger("@main").unwrap();
        let out = candidates(&t, &files, &[]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].insert, "@src/Main.rs ");
    }
}
