//! Claude Code hook 脚本调用的小工具：把 hook 事件翻译成 smeltd 的 `state` op。
//! 见 docs/state-channel-plan.md「hook 链路」一节。
//!
//! stdin: Claude Code 传来的 hook JSON（`hook_event_name` / `tool_name` /
//!        `notification_type` / `message` 等，字段名见官方 hooks 文档）
//! env:   `SMELT_SESSION_ID`（smeltd 会话 id，spawn_session 时注入）、
//!        `SMELT_SOCK`（smeltd.sock 路径，同样是 spawn 时注入）
//! 出参:  连 `$SMELT_SOCK` 发一行 `{"op":"state","id":"..","phase":"..","question":".."}`
//!
//! **必须 exit 0、不打印任何 stdout**：Claude Code 把 hook 的 stdout 当成决策 JSON
//! 解析，非 0 退出码（尤其是 2）会阻塞工具执行——这个工具只负责上报状态，绝不能
//! 意外干扰 agent 的正常运行。任何失败（socket 连不上、JSON 解析不出来、字段
//! 缺失、env 没设置）都静默退出，不 panic、不报错、不阻塞。

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

fn main() {
    let _ = run();
}

fn run() -> Option<()> {
    // 没设这两个 env 说明不在 smelt 会话里跑（用户在别的终端直接用 Claude Code），
    // 静默退出——这不是错误，是正常情况。
    let session_id = std::env::var("SMELT_SESSION_ID").ok()?;
    let sock = std::env::var("SMELT_SOCK").ok()?;

    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).ok()?;
    let hook: serde_json::Value = serde_json::from_str(&input).ok()?;

    let (phase, question) = map_hook_event(&hook)?;

    let mut s = UnixStream::connect(&sock).ok()?;
    let payload = serde_json::json!({
        "op": "state",
        "id": session_id,
        "phase": phase,
        "question": question,
    });
    // 连不上/写失败都无所谓——状态通道本就是「有则更准，没有就继续猜」，
    // 不是必须成功的关键路径。
    let _ = writeln!(s, "{payload}");
    Some(())
}

/// hook 事件 → (phase 字符串, pending_question)。不认识的事件返回 None（不上报，
/// 好过拿不确定的东西污染状态——这条原则贯穿整个状态通道设计）。
///
/// phase 字符串必须跟 smeltd.rs 里 `Phase` 枚举的 `#[serde(rename_all =
/// "snake_case")]` 对应（thinking / executing_tool / awaiting_approval /
/// waiting_for_user / idle / dead），两边不同源、靠字符串约定对齐。
fn map_hook_event(hook: &serde_json::Value) -> Option<(&'static str, Option<String>)> {
    let event = hook["hook_event_name"].as_str()?;
    match event {
        "UserPromptSubmit" => Some(("thinking", None)),
        "PreToolUse" => {
            // question 槽位复用为「当前工具」展示（结构面板 / 总览）。
            let tool = hook["tool_name"].as_str().map(|t| {
                // 尽量带一点路径/命令摘要
                let input = &hook["tool_input"];
                if let Some(cmd) = input["command"].as_str() {
                    // 按字符截断，别按字节：cmd.len() 是字节数，&cmd[..48] 也是按字节
                    // 切片——第 48 字节一旦落在中文/emoji 的多字节编码中间就会 panic
                    // （byte index is not a char boundary），把整个 hook 打挂。
                    let short = match cmd.char_indices().nth(48) {
                        Some((end, _)) => format!("{}…", &cmd[..end]),
                        None => cmd.to_string(),
                    };
                    format!("Bash: {short}")
                } else if let Some(p) = input["file_path"]
                    .as_str()
                    .or_else(|| input["path"].as_str())
                {
                    let name = p.rsplit('/').next().unwrap_or(p);
                    format!("{t}: {name}")
                } else {
                    t.to_string()
                }
            });
            Some(("executing_tool", tool))
        }
        "PostToolUse" => Some(("thinking", None)),
        // 独立的权限请求事件（比 Notification 更明确），见官方 hooks 文档。
        "PermissionRequest" => {
            let tool = hook["tool_name"].as_str().unwrap_or("");
            let q = format!("请求执行 {tool}");
            Some(("awaiting_approval", Some(q)))
        }
        "Notification" => {
            // Notification 不都是"等审批"——notification_type 区分子类型，
            // 认不出来的子类型不改 phase（比如 auth_success 这种跟"要不要
            // 批准/输入"无关的通知）。
            let message = hook["message"].as_str().map(String::from);
            match hook["notification_type"].as_str() {
                Some("permission_prompt") => Some(("awaiting_approval", message)),
                Some("idle_prompt") => Some(("waiting_for_user", message)),
                _ => None,
            }
        }
        "Stop" => Some(("waiting_for_user", None)),
        "SessionEnd" => Some(("dead", None)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn user_prompt_submit_maps_to_thinking() {
        let hook = json!({ "hook_event_name": "UserPromptSubmit" });
        assert_eq!(map_hook_event(&hook), Some(("thinking", None)));
    }

    #[test]
    fn pre_tool_use_carries_tool_name_as_question() {
        let hook = json!({ "hook_event_name": "PreToolUse", "tool_name": "Bash" });
        assert_eq!(map_hook_event(&hook), Some(("executing_tool", Some("Bash".to_string()))));
    }

    /// 命令摘要按**字符**截断，不能按字节切：`&cmd[..48]` 遇到第 48 字节落在多字节
    /// 字符中间会直接 panic（"byte index is not a char boundary"），把整个 hook 打挂
    /// ——中文命令一跑就中招，实际见过。
    #[test]
    fn long_cjk_command_does_not_panic_on_char_boundary() {
        // 第 48 字节落在「图」的三字节编码中间
        let cmd = "echo \"=== 卷内容（应见中文软链 + 卷图标）===\"; ls -la /tmp/x; echo done";
        assert!(!cmd.is_char_boundary(48), "用例前提：第 48 字节须落在字符中间");
        assert!(cmd.chars().count() > 48, "用例前提：字符数须超过截断阈值");

        let hook = json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": { "command": cmd },
        });
        let (phase, q) = map_hook_event(&hook).unwrap();
        assert_eq!(phase, "executing_tool");
        let q = q.expect("应带命令摘要");
        assert!(q.starts_with("Bash: echo"), "摘要应保留命令开头：{q}");
        assert!(q.ends_with('…'), "超长应截断并加省略号：{q}");
        assert_eq!(
            q.chars().count(),
            "Bash: ".chars().count() + 48 + 1,
            "截断按字符算：前缀 + 48 字符 + 省略号"
        );
    }

    /// 阈值按字符算而非字节：中文命令字节数很容易翻三倍越过阈值，但字符数并不多。
    /// 旧的 `cmd.len() > 48`（字节）会把这种并不长的命令误判成超长、砍掉尾巴。
    #[test]
    fn cjk_command_within_char_limit_is_not_truncated() {
        let cmd = "echo \"=== 卷内容（应见中文软链 + 卷图标）===\"; ls -la /tmp/x";
        assert!(cmd.len() > 48, "用例前提：字节数须超阈值");
        assert!(cmd.chars().count() <= 48, "用例前提：字符数须未超阈值");

        let hook = json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": { "command": cmd },
        });
        let (_, q) = map_hook_event(&hook).unwrap();
        assert_eq!(
            q.expect("应带命令摘要"),
            format!("Bash: {cmd}"),
            "字符数没超阈值就不该截断"
        );
    }

    #[test]
    fn permission_request_builds_a_question_from_tool_name() {
        let hook = json!({ "hook_event_name": "PermissionRequest", "tool_name": "Bash" });
        let (phase, q) = map_hook_event(&hook).unwrap();
        assert_eq!(phase, "awaiting_approval");
        assert_eq!(q.as_deref(), Some("请求执行 Bash"));
    }

    #[test]
    fn notification_permission_prompt_maps_to_awaiting_approval() {
        let hook = json!({
            "hook_event_name": "Notification",
            "notification_type": "permission_prompt",
            "message": "需要批准执行 rm 命令",
        });
        assert_eq!(
            map_hook_event(&hook),
            Some(("awaiting_approval", Some("需要批准执行 rm 命令".to_string())))
        );
    }

    #[test]
    fn notification_idle_prompt_maps_to_waiting_for_user() {
        let hook = json!({
            "hook_event_name": "Notification",
            "notification_type": "idle_prompt",
            "message": "等你继续",
        });
        assert_eq!(
            map_hook_event(&hook),
            Some(("waiting_for_user", Some("等你继续".to_string())))
        );
    }

    /// 认不出的 notification_type（比如 auth_success）不该反过来乱猜 phase。
    #[test]
    fn notification_unknown_subtype_is_ignored() {
        let hook = json!({
            "hook_event_name": "Notification",
            "notification_type": "auth_success",
            "message": "已登录",
        });
        assert_eq!(map_hook_event(&hook), None);
    }

    #[test]
    fn stop_maps_to_waiting_for_user() {
        let hook = json!({ "hook_event_name": "Stop" });
        assert_eq!(map_hook_event(&hook), Some(("waiting_for_user", None)));
    }

    #[test]
    fn session_end_maps_to_dead() {
        let hook = json!({ "hook_event_name": "SessionEnd" });
        assert_eq!(map_hook_event(&hook), Some(("dead", None)));
    }

    #[test]
    fn unknown_event_name_is_ignored() {
        let hook = json!({ "hook_event_name": "SomethingWeirdFromAFutureVersion" });
        assert_eq!(map_hook_event(&hook), None);
    }

    #[test]
    fn missing_hook_event_name_is_ignored() {
        assert_eq!(map_hook_event(&json!({})), None);
    }
}
