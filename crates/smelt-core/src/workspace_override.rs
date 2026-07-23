//! 启动命令里 shell 风格 `VAR=value` 前缀的解析/展开，供两处共用：
//! - `acp_conn::build_agent` 拿它决定"跳过这些 token 找真正的程序名"；
//! - 多 workspace 场景（同一家 agent 开好几个 profile，比如 `.claude` 和
//!   `.claude-quant` 并存）下，续接可行性预检（transcript 在不在）和历史会话
//!   浏览都要从同一条启动命令里读出「这个会话实际用的是哪个 workspace 目录」，
//!   不能只看进程当前的全局环境变量——那只反映"默认"那一个。

/// 判断 token 是不是 shell 风格的 `VAR=value` 赋值——规则跟
/// `agent_client_protocol` 自己的 `parse_env_var` 保持一致（变量名以字母/下划线
/// 开头，后续字符是字母数字下划线），两边判断不一致就会出现"这边当程序名去查
/// PATH，SDK 那边却当环境变量"的分歧。命中就拆成 `(name, value)`，没命中返回
/// `None`（当作真正的程序名）。
pub fn split_env_assignment(token: &str) -> Option<(&str, &str)> {
    let eq_pos = token.find('=')?;
    if eq_pos == 0 {
        return None;
    }
    let name = &token[..eq_pos];
    let mut chars = name.chars();
    let first = chars.next()?;
    if !first.is_ascii_alphabetic() && first != '_' {
        return None;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    Some((name, &token[eq_pos + 1..]))
}

/// 展开值开头的 `~`（`~` 或 `~/...`）成 home 目录——环境变量的值不会像 shell
/// 命令行那样被自动展开，子进程 `getenv()` 拿到的就是字面量 `~/.claude-quant`；
/// 用户在设置里手填 `CLAUDE_CONFIG_DIR=~/.claude-quant` 是期望它跟 shell 里
/// `export` 出来的效果一样，所以这里替用户展开一次，不能指望每家 agent 自己
/// 都做了防御性展开。
pub fn expand_tilde(value: &str) -> String {
    if let Some(rest) = value.strip_prefix('~') {
        if rest.is_empty() || rest.starts_with('/') {
            if let Some(home) = dirs::home_dir() {
                return format!("{}{}", home.display(), rest);
            }
        }
    }
    value.to_string()
}

/// 从启动命令字符串里挑出某个环境变量名对应的值（已展开 `~`）。只看开头连续
/// 的 `VAR=value` 前缀段——遇到第一个不是赋值形状的 token（真正的程序名）就
/// 停，不深入扫描后面的参数（`--flag=1` 这类不该被误判）。找不到该变量名的
/// 前缀返回 `None`，调用方按"这个 profile 没覆盖，走默认路径"处理。
pub fn env_override_from_cmd(cmd: &str, var_name: &str) -> Option<String> {
    for tok in cmd.split_whitespace() {
        match split_env_assignment(tok) {
            Some((name, value)) if name == var_name => return Some(expand_tilde(value)),
            Some(_) => continue,
            None => break,
        }
    }
    None
}

/// 各家 agent 自定义 workspace 目录用的环境变量名（`AcpAgentKind::id()` →
/// 变量名），跟 `login_env.rs` 头部注释是同一份调研结论。Copilot 有
/// `COPILOT_HOME`/`XDG_CONFIG_HOME` 两种，这里给手动添加 workspace 用的是
/// 官方推荐、整段替换的 `COPILOT_HOME`。
pub fn config_dir_env_var(kind_id: &str) -> Option<&'static str> {
    match kind_id {
        "claude" => Some("CLAUDE_CONFIG_DIR"),
        "codex" => Some("CODEX_HOME"),
        "grok" => Some("GROK_HOME"),
        "copilot" => Some("COPILOT_HOME"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_valid_assignment() {
        assert_eq!(split_env_assignment("CLAUDE_CONFIG_DIR=~/.claude-quant"), Some(("CLAUDE_CONFIG_DIR", "~/.claude-quant")));
        assert_eq!(split_env_assignment("_X=1"), Some(("_X", "1")));
        assert_eq!(split_env_assignment("A1_B=val=ue"), Some(("A1_B", "val=ue")));
    }

    #[test]
    fn rejects_non_assignment_tokens() {
        for tok in ["claude", "/usr/local/bin/claude", "--flag=1", "=leading-eq", "pkg@1.0=x", "1VAR=x"] {
            assert_eq!(split_env_assignment(tok), None, "{tok} 不该被当成 VAR=value");
        }
    }

    #[test]
    fn expands_leading_tilde_only() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(expand_tilde("~/.claude-quant"), format!("{}/.claude-quant", home.display()));
        assert_eq!(expand_tilde("~"), home.display().to_string());
        assert_eq!(expand_tilde("~foo/bar"), "~foo/bar");
        assert_eq!(expand_tilde("/already/absolute"), "/already/absolute");
    }

    #[test]
    fn extracts_matching_var_and_stops_at_program_name() {
        let cmd = "CLAUDE_CONFIG_DIR=~/.claude-quant claude --dangerously-skip-permissions";
        let home = dirs::home_dir().unwrap();
        assert_eq!(env_override_from_cmd(cmd, "CLAUDE_CONFIG_DIR"), Some(format!("{}/.claude-quant", home.display())));
        assert_eq!(env_override_from_cmd(cmd, "CODEX_HOME"), None);
        assert_eq!(env_override_from_cmd("claude --flag", "CLAUDE_CONFIG_DIR"), None);
    }
}
