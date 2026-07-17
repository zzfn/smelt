//! 权限菜单解析 —— **唯一真源**，GUI 与 smeltd 共用（`#[path]` 引入，同 title_spinner.rs）。
//!
//! 从终端可视区扫出 Claude Code 等 TUI 的编号选择菜单（`❯ 1. Yes` / `[1] Allow` …），
//! 供三处消费：
//!   - GUI：本地读 Term 网格 → 总览页 / 状态侧栏渲染审批按钮
//!   - smeltd：解析后随 SessionState 下发
//!   - 手机端：只渲染 smeltd 下发的结果，**不再自己解析**
//!
//! 历史教训：这套逻辑曾在 Rust（这里）和 TypeScript（remote-web）各写一份，同日诞生
//! 后各自演化，实测已漂移——同一段文本手机认得出、桌面认不出。认不出的代价不是少个
//! 按钮，而是界面退回硬编码兜底（批准=打 1 / 拒绝=打 3）后盲发，而真实菜单未必是那个
//! 顺序。所以这份必须保持唯一，别再在别处「顺手写一版」。
//!
//! 本模块**不依赖 GPUI**：smeltd 用得上，且能脱离 GUI 单测。

/// 权限菜单里的一个可选项（从终端网格解析）。
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct PermissionOption {
    /// 注入 PTY 的键（通常是 `"1"` / `"2"` / `"3"`）。
    pub key: String,
    /// 选项原文。
    pub label: String,
    pub kind: PermissionOptionKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOptionKind {
    Allow,
    Deny,
    Other,
}

/// 从可视区扫出的权限提示（Claude Code 等 TUI 数字菜单）。
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct PermissionPrompt {
    pub summary: Option<String>,
    pub options: Vec<PermissionOption>,
}

impl PermissionOption {
    /// 总览按钮短标签。
    pub fn button_label(&self) -> String {
        match self.kind {
            PermissionOptionKind::Allow => {
                let l = self.label.to_ascii_lowercase();
                if l == "yes" || l.starts_with("yes.") || l == "allow" || l == "approve" {
                    "允许".into()
                } else {
                    truncate_chars(&self.label, 16)
                }
            }
            PermissionOptionKind::Deny => {
                let l = self.label.to_ascii_lowercase();
                if l == "no"
                    || l.starts_with("no,")
                    || l.starts_with("no ")
                    || l == "deny"
                    || l == "reject"
                    || l == "cancel"
                    || self.label.starts_with('否')
                {
                    "拒绝".into()
                } else {
                    truncate_chars(&self.label, 16)
                }
            }
            PermissionOptionKind::Other => truncate_chars(&self.label, 16),
        }
    }

    /// 主按钮：第一个「允许」类且不是「不再询问」。
    pub fn is_primary(&self) -> bool {
        matches!(self.kind, PermissionOptionKind::Allow)
            && !self.label.to_ascii_lowercase().contains("don't ask")
            && !self.label.contains("不再")
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    let t = s.trim();
    if t.chars().count() <= max {
        t.to_string()
    } else {
        format!(
            "{}…",
            t.chars().take(max.saturating_sub(1)).collect::<String>()
        )
    }
}

fn classify_option_label(label: &str) -> PermissionOptionKind {
    let l = label.to_ascii_lowercase();
    if l == "no"
        || l.starts_with("no,")
        || l.starts_with("no ")
        || l.starts_with("no.")
        || l.contains("deny")
        || l.contains("reject")
        || l.contains("cancel")
        || l.contains("refuse")
        || label.contains("拒绝")
        || label.starts_with('否')
    {
        return PermissionOptionKind::Deny;
    }
    if l == "yes"
        || l.starts_with("yes")
        || l.contains("allow")
        || l.contains("approve")
        || l.contains("proceed")
        || l.contains("accept")
        || label.contains("允许")
        || label.contains("批准")
        || label.starts_with('是')
    {
        return PermissionOptionKind::Allow;
    }
    PermissionOptionKind::Other
}

/// 尝试把一行解析成 `1. label` / `1) label` / `[1] label`。
/// TUI 画边框用的竖线：选项行常常长成 `│ ❯ 1. Yes`，边框不剥掉就认不出选项。
const BORDER_CHARS: [char; 6] = ['│', '|', '┃', '║', ' ', '\t'];

/// 高亮指针字符集——真实 TUI 里远不止 `❯`。这份清单与手机端
/// `remote-web/src/lib/parseChoiceMenu.ts` 的 OPTION_RE 对齐：那边踩过
/// 「旧正则只认 `>`，高亮的第 1 项被漏掉、跑到标题槽位里」的线上问题并修好了，
/// 这边一直没拿到那个补丁，于是同一个菜单手机认得出、桌面认不出。
///
/// 认不出的代价不是少个按钮，而是误操作：扫不到菜单 → 界面落回硬编码兜底
/// （批准=打 1 / 拒绝=打 3）→ 盲发，而真实菜单未必是这个顺序。
const POINTER_CHARS: [char; 15] = [
    '❯', '>', '›', '▶', '►', '→', '➜', '•', '●', '◆', '*', '✦', '➢', '➤', ' ',
];

fn parse_numbered_option_line(raw: &str) -> Option<(String, String)> {
    let line = raw.trim();
    if line.is_empty() {
        return None;
    }
    // 先剥边框，再剥高亮指针（顺序不能反：指针画在边框里侧）。
    let line = line
        .trim_start_matches(BORDER_CHARS)
        .trim_start()
        .trim_start_matches(POINTER_CHARS)
        .trim_start();

    if let Some(rest) = line.strip_prefix('[') {
        let (n, after) = rest.split_once(']')?;
        let n = n.trim();
        if n.is_empty() || n.len() > 2 || !n.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let label = after
            .trim()
            .trim_start_matches(['.', ')', ':', '-', ' '])
            .trim();
        if label.is_empty() {
            return None;
        }
        return Some((n.to_string(), label.to_string()));
    }

    let digits: String = line.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() || digits.len() > 2 {
        return None;
    }
    let rest = line[digits.len()..].trim_start();
    // 必须有分隔符，避免把 `1foo` 当选项。含中文 TUI 常用的顿号与全角句点
    // （与手机端 OPTION_RE 的 [\.．、:)\]] 对齐）。
    let rest = rest
        .strip_prefix('.')
        .or_else(|| rest.strip_prefix('．'))
        .or_else(|| rest.strip_prefix('、'))
        .or_else(|| rest.strip_prefix(')'))
        .or_else(|| rest.strip_prefix(':'))?;
    let label = rest.trim();
    if label.is_empty() || label.starts_with('/') || label.starts_with("http") {
        return None;
    }
    Some((digits, label.to_string()))
}

/// 从终端末尾行解析权限数字菜单（Claude Code 等）。
///
/// 典型形态：
/// ```text
/// Do you want to proceed?
/// ❯ 1. Yes
///   2. Yes, and don't ask again …
///   3. No, and tell Claude what to do differently
/// ```
pub fn parse_permission_prompt(lines: &[String]) -> Option<PermissionPrompt> {
    let mut options: Vec<PermissionOption> = Vec::new();
    let mut option_line_idxs: Vec<usize> = Vec::new();

    for (i, raw) in lines.iter().enumerate() {
        let Some((key, label)) = parse_numbered_option_line(raw) else {
            continue;
        };
        options.push(PermissionOption {
            key,
            label: label.clone(),
            kind: classify_option_label(&label),
        });
        option_line_idxs.push(i);
    }

    if options.len() < 2 {
        return None;
    }

    let has_perm_word = options
        .iter()
        .any(|o| !matches!(o.kind, PermissionOptionKind::Other));
    let first = *option_line_idxs.first()?;
    let last = *option_line_idxs.last()?;
    let ctx_start = first.saturating_sub(4);
    let context_hint = lines[ctx_start..=last].iter().any(|l| {
        let t = l.to_ascii_lowercase();
        t.contains("permission")
            || t.contains("approv")
            || t.contains("proceed")
            || t.contains("allow")
            || t.contains("do you want")
            || t.contains("bash command")
            || t.contains("tool call")
            || t.contains("run this")
            || t.contains("权限")
            || t.contains("批准")
            || t.contains("是否")
            || t.contains("允许")
            || t.contains("esc to cancel")
            || t.contains("don't ask")
    });
    if !has_perm_word && !context_hint {
        return None;
    }

    let summary = lines[..first].iter().rev().find_map(|l| {
        let t = l.trim();
        if t.is_empty() || parse_numbered_option_line(t).is_some() {
            return None;
        }
        Some(truncate_chars(t, 120))
    });

    // 不截断：被截掉的选项在界面上永远够不着，而 Claude Code 的权限菜单确实会有
    // 5 项（Yes / Yes 别再问 / 仅此一次 / 先编辑命令 / No）。上游 parse 已经用
    // 「2~12 项 + 序号连续」把误报挡住了，这里再砍一刀只会让真菜单缺项。
    Some(PermissionPrompt { summary, options })
}

#[cfg(test)]
mod permission_prompt_tests {
    use super::*;

    fn lines(s: &str) -> Vec<String> {
        s.lines().map(|l| l.to_string()).collect()
    }

    #[test]
    fn parses_claude_style_numbered_menu() {
        let p = parse_permission_prompt(&lines(
            "Do you want to proceed?\n\
             ❯ 1. Yes\n\
               2. Yes, and don't ask again for bash commands\n\
               3. No, and tell Claude what to do differently\n\
             Esc to cancel",
        ))
        .expect("prompt");
        assert_eq!(p.summary.as_deref(), Some("Do you want to proceed?"));
        assert_eq!(p.options.len(), 3);
        assert_eq!(p.options[0].key, "1");
        assert_eq!(p.options[0].kind, PermissionOptionKind::Allow);
        assert!(p.options[0].is_primary());
        assert_eq!(p.options[1].kind, PermissionOptionKind::Allow);
        assert!(!p.options[1].is_primary());
        assert_eq!(p.options[2].key, "3");
        assert_eq!(p.options[2].kind, PermissionOptionKind::Deny);
        assert_eq!(p.options[0].button_label(), "允许");
        assert_eq!(p.options[2].button_label(), "拒绝");
    }

    #[test]
    fn rejects_plain_numbered_list_without_permission_context() {
        assert!(
            parse_permission_prompt(&lines("1. install deps\n2. run tests\n3. deploy")).is_none()
        );
    }

    #[test]
    fn accepts_bracket_style_with_permission_hint() {
        let p = parse_permission_prompt(&lines(
            "Permission required\n[1] Allow\n[2] Deny",
        ))
        .expect("prompt");
        assert_eq!(p.options[0].kind, PermissionOptionKind::Allow);
        assert_eq!(p.options[1].kind, PermissionOptionKind::Deny);
    }

    // 以下几组是手机端（remote-web/src/lib/parseChoiceMenu.ts）已经认、而这边一直
    // 认不出的真实 TUI 形态。两份解析器同日诞生后各自演化，手机那份陆续吃过线上
    // 补丁（它注释里记着「高亮项常用 ❯ / › / ▶ 等，旧正则只认 `>`，会把第 1 项漏掉」），
    // 这边从没拿到。
    //
    // 认不出的代价不是「少个按钮」而是**误操作**：扫不到菜单 → has_opts=false →
    // 界面落回硬编码兜底（main.rs 的「批准=打 1 / 拒绝=打 3」）→ 盲发 1/3，
    // 而真实菜单未必是这个顺序。

    /// TUI 常用 `│` 画边框，选项行前缀是边框而非空白。
    #[test]
    fn accepts_options_behind_box_drawing_border() {
        let p = parse_permission_prompt(&lines(
            "Do you want to proceed?\n\
             │ ❯ 1. Yes\n\
             │   2. No, tell Claude what to do differently",
        ))
        .expect("边框前缀的菜单也该认出来");
        assert_eq!(p.options.len(), 2);
        assert_eq!(p.options[0].key, "1");
        assert_eq!(p.options[0].kind, PermissionOptionKind::Allow);
        assert_eq!(p.options[1].kind, PermissionOptionKind::Deny);
    }

    /// 高亮指针不止 `❯`：`›`/`▶`/`→` 等都在真实 TUI 里出现过。
    #[test]
    fn accepts_alternate_highlight_pointers() {
        for ptr in ["›", "▶", "►", "→", "➜", "✦", "➢", "➤"] {
            let src = format!(
                "Do you want to proceed?\n{ptr} 1. Yes\n  2. No, cancel"
            );
            let p = parse_permission_prompt(&lines(&src))
                .unwrap_or_else(|| panic!("指针 {ptr} 开头的菜单该认出来"));
            assert_eq!(p.options.len(), 2, "指针 {ptr}");
            assert_eq!(p.options[0].key, "1", "指针 {ptr}");
        }
    }

    /// 中文 TUI 常用顿号或全角句点做分隔符。
    #[test]
    fn accepts_cjk_separators() {
        for sep in ["、", "．"] {
            let src = format!("是否继续执行？\n❯ 1{sep}允许\n  2{sep}拒绝");
            let p = parse_permission_prompt(&lines(&src))
                .unwrap_or_else(|| panic!("分隔符 {sep} 的菜单该认出来"));
            assert_eq!(p.options.len(), 2, "分隔符 {sep}");
            assert_eq!(p.options[0].kind, PermissionOptionKind::Allow, "分隔符 {sep}");
            assert_eq!(p.options[1].kind, PermissionOptionKind::Deny, "分隔符 {sep}");
        }
    }

    /// 超过 4 项不得静默截断——被截掉的选项在界面上永远够不着。
    #[test]
    fn keeps_all_options_beyond_four() {
        let p = parse_permission_prompt(&lines(
            "Do you want to proceed?\n\
             ❯ 1. Yes\n\
               2. Yes, and don't ask again\n\
               3. Yes, but only this once\n\
               4. Edit the command first\n\
               5. No, tell Claude what to do differently",
        ))
        .expect("prompt");
        assert_eq!(p.options.len(), 5, "5 项菜单不该被截断成 4 项");
        assert_eq!(p.options[4].key, "5");
        assert_eq!(p.options[4].kind, PermissionOptionKind::Deny);
    }

    /// 补齐字符集不能把语义闸门一起放开：纯编号列表仍须拒绝。
    /// （手机那份没有闸门，会把这种误判成权限菜单——收敛时别把这个 bug 一起吸收。）
    #[test]
    fn still_rejects_plain_lists_with_new_separators_and_pointers() {
        assert!(
            parse_permission_prompt(&lines("› 1、install deps\n  2、run tests\n  3、deploy"))
                .is_none(),
            "换了指针和顿号，纯待办列表仍然不是权限菜单"
        );
    }
}

