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
        // 会话刚起、hooks 链路第一次有机会发声——不上报的话，Idle 态和「hooks
        // 根本没装/装的是旧配置/socket 连不上」在 UI 上长得一模一样；有这一条，
        // DaemonStates 里出现记录本身就是「链路确认通了」的信号。
        "SessionStart" => Some(("idle", None)),
        "UserPromptSubmit" => Some(("thinking", None)),
        "PreToolUse" => {
            // question 槽位复用为「当前工具」展示（结构面板 / 总览）。
            Some(("executing_tool", describe_tool_call(hook)))
        }
        "PostToolUse" => Some(("thinking", None)),
        // 工具跑挂了：跟 PostToolUse 一样回到 thinking（agent 马上会看着错误决定
        // 下一步），但 question 带上失败标记——不然「刚才那个工具是不是炸了」在
        // UI 上完全看不出来，跟正常跑完长得一样。
        "PostToolUseFailure" => {
            let tool = hook["tool_name"].as_str().unwrap_or("工具");
            let err = first_present_str(hook, &["error", "error_message", "tool_error", "reason"]);
            let q = match err {
                Some(e) => format!("⚠ {tool} 执行失败：{}", truncate_chars(e, 60)),
                None => format!("⚠ {tool} 执行失败"),
            };
            Some(("thinking", Some(q)))
        }
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
        // agent 起了个子任务（比如 Task 工具）：这段时间之前完全是黑箱，只显示
        // 笼统的 executing_tool；agent_type 是官方 hooks 字段（"Explore" /
        // "security-reviewer" 这类），带上就知道具体在跑哪个子任务。
        "SubagentStart" => {
            let name = hook["agent_type"].as_str();
            let q = match name {
                Some(n) => format!("子任务：{n}"),
                None => "运行子任务".to_string(),
            };
            Some(("executing_tool", Some(q)))
        }
        // 子任务做完，主 agent 回去汇总/继续思考。
        "SubagentStop" => Some(("thinking", None)),
        "Stop" => Some(("waiting_for_user", None)),
        // 回合因 API 错误中断，跟正常「说完了等你」（Stop）语义不同——同样落
        // waiting_for_user（协议里没有更细的档位，见 status_color 只到五色），
        // 但 question 标出「出错」，detail_line 上能看出差别，不会跟正常收尾混淆。
        "StopFailure" => {
            let reason = first_present_str(hook, &["error", "error_type", "reason"]);
            let q = match reason {
                Some(r) => format!("⚠ 因错误中断：{r}"),
                None => "⚠ 因错误中断".to_string(),
            };
            Some(("waiting_for_user", Some(q)))
        }
        "SessionEnd" => Some(("dead", None)),
        _ => None,
    }
}

/// PreToolUse 的「当前工具」摘要：尽量带点路径/命令细节。
fn describe_tool_call(hook: &serde_json::Value) -> Option<String> {
    let tool = hook["tool_name"].as_str()?;
    let input = &hook["tool_input"];
    Some(if let Some(cmd) = input["command"].as_str() {
        format!("Bash: {}", truncate_chars(cmd, 48))
    } else if let Some(p) = input["file_path"].as_str().or_else(|| input["path"].as_str()) {
        let name = p.rsplit('/').next().unwrap_or(p);
        format!("{tool}: {name}")
    } else {
        tool.to_string()
    })
}

/// 按**字符**截断，不能按字节切：`&s[..n]` 是字节切片，第 n 字节一旦落在中文/
/// emoji 的多字节编码中间就会 panic（byte index is not a char boundary）。
fn truncate_chars(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        Some((end, _)) => format!("{}…", &s[..end]),
        None => s.to_string(),
    }
}

/// 依次尝试几个可能的字段名，返回第一个存在的字符串——官方文档没有给出
/// `PostToolUseFailure`/`StopFailure` 的精确错误字段名，宽容读取：拿不到具体
/// 错误文案就退化成通用提示，但「失败」这个事实本身来自 hook_event_name 自己，
/// 不依赖猜中字段名，不会因为猜错而整条不上报。
fn first_present_str<'a>(hook: &'a serde_json::Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| hook[*k].as_str())
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

    #[test]
    fn session_start_maps_to_idle() {
        let hook = json!({ "hook_event_name": "SessionStart" });
        assert_eq!(map_hook_event(&hook), Some(("idle", None)));
    }

    #[test]
    fn subagent_start_carries_agent_type() {
        let hook = json!({ "hook_event_name": "SubagentStart", "agent_type": "Explore" });
        assert_eq!(map_hook_event(&hook), Some(("executing_tool", Some("子任务：Explore".to_string()))));
    }

    #[test]
    fn subagent_start_without_agent_type_falls_back() {
        let hook = json!({ "hook_event_name": "SubagentStart" });
        assert_eq!(map_hook_event(&hook), Some(("executing_tool", Some("运行子任务".to_string()))));
    }

    #[test]
    fn subagent_stop_maps_to_thinking() {
        let hook = json!({ "hook_event_name": "SubagentStop" });
        assert_eq!(map_hook_event(&hook), Some(("thinking", None)));
    }

    #[test]
    fn post_tool_use_failure_carries_error_when_present() {
        let hook = json!({
            "hook_event_name": "PostToolUseFailure",
            "tool_name": "Bash",
            "error": "command not found",
        });
        let (phase, q) = map_hook_event(&hook).unwrap();
        assert_eq!(phase, "thinking");
        assert_eq!(q.as_deref(), Some("⚠ Bash 执行失败：command not found"));
    }

    /// 官方字段名没有精确文档，宽容读取：这里故意不给 `error`，只给
    /// `tool_error`，验证 first_present_str 会往下试。
    #[test]
    fn post_tool_use_failure_falls_back_to_alternate_field_name() {
        let hook = json!({
            "hook_event_name": "PostToolUseFailure",
            "tool_name": "Write",
            "tool_error": "permission denied",
        });
        let (_, q) = map_hook_event(&hook).unwrap();
        assert_eq!(q.as_deref(), Some("⚠ Write 执行失败：permission denied"));
    }

    /// 一个错误字段都拿不到也不能整条不上报——「失败」这个事实来自
    /// hook_event_name 本身，不依赖猜中字段名。
    #[test]
    fn post_tool_use_failure_without_any_known_field_still_reports() {
        let hook = json!({ "hook_event_name": "PostToolUseFailure", "tool_name": "Bash" });
        let (phase, q) = map_hook_event(&hook).unwrap();
        assert_eq!(phase, "thinking");
        assert_eq!(q.as_deref(), Some("⚠ Bash 执行失败"));
    }

    #[test]
    fn stop_failure_differs_from_plain_stop() {
        let hook = json!({ "hook_event_name": "StopFailure", "error_type": "rate_limit" });
        let (phase, q) = map_hook_event(&hook).unwrap();
        assert_eq!(phase, "waiting_for_user");
        assert_eq!(q.as_deref(), Some("⚠ 因错误中断：rate_limit"));
        // 跟正常 Stop 同一个 phase，但 question 不同——detail_line 上能看出区别，
        // 不会跟「说完了等你」混淆。
        assert_ne!(map_hook_event(&hook), map_hook_event(&json!({ "hook_event_name": "Stop" })));
    }

    #[test]
    fn stop_failure_without_reason_still_flags_error() {
        let hook = json!({ "hook_event_name": "StopFailure" });
        let (_, q) = map_hook_event(&hook).unwrap();
        assert_eq!(q.as_deref(), Some("⚠ 因错误中断"));
    }
}
