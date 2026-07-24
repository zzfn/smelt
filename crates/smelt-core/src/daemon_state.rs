//! 状态通道（见 docs/state-channel-plan.md）：GUI 侧订阅 smeltd 广播的会话状态
//! 镜像。这个模块本身不碰 GPUI，纯数据结构 + 阻塞的 socket 通信；GUI 那边的
//! `Entity`/`Global` 包装留在 main crate，跟 acp-view 未来渲染层要用同一份数据
//! 模型（会话相位翻译成这个结构），别再复制一遍判断逻辑。

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

/// smeltd 的 unix socket 路径（`~/.smelt/smeltd.sock`），用时顺手建好父目录。
pub fn smeltd_sock_path() -> PathBuf {
    let dir = dirs::home_dir().unwrap_or_else(|| "/tmp".into()).join(".smelt");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("smeltd.sock")
}

/// GUI 侧订阅状态通道的镜像（serde 反序列化；多出来的 JSON 字段自动忽略）。
/// 字段对齐 smeltd `SessionState` 广播——B 路线「语义面板」的数据源。
#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct DaemonSessionState {
    pub id: String,
    #[serde(default)]
    pub phase: DaemonPhase,
    /// hook 上报的问句 / 当前工具名（PreToolUse 时 question 常是 tool_name）。
    #[serde(default)]
    pub pending_question: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    /// 守护下发的协议字段（旧结构面板曾展示 launch 命令）；GUI 侧暂无读者，
    /// 保留以维持与 smeltd 状态通道的结构对齐。
    #[serde(default)]
    #[allow(dead_code)]
    pub launch: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    /// unix 秒，进入当前 phase 的时刻。
    #[serde(default)]
    pub phase_since: u64,
}

impl DaemonSessionState {
    /// 结构面板用：phase 中文标签。
    pub fn phase_label(&self) -> &'static str {
        match self.phase {
            DaemonPhase::Thinking => "思考中",
            DaemonPhase::ExecutingTool => "执行工具",
            DaemonPhase::AwaitingApproval => "等你批准",
            DaemonPhase::WaitingForUser => "等你输入",
            DaemonPhase::Idle => "空闲",
            DaemonPhase::Dead => "已结束",
        }
    }

    /// 结构面板副文案：工具名或审批问句。
    pub fn detail_line(&self) -> Option<String> {
        let q = self.pending_question.as_deref()?.trim();
        if q.is_empty() {
            return None;
        }
        Some(match self.phase {
            DaemonPhase::ExecutingTool => format!("🔧 {q}"),
            DaemonPhase::AwaitingApproval => format!("⚠ {q}"),
            DaemonPhase::WaitingForUser => format!("💬 {q}"),
            _ => q.to_string(),
        })
    }

    /// 进入当前 phase 多久（秒）；phase_since 为 0 则 None。
    pub fn phase_age_secs(&self) -> Option<u64> {
        if self.phase_since == 0 {
            return None;
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
        Some(now.saturating_sub(self.phase_since))
    }
}

/// 跟 smeltd.rs 的 `Phase` 对应，同样 `rename_all = "snake_case"`。
#[derive(Clone, Copy, PartialEq, Debug, Default, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonPhase {
    Thinking,
    ExecutingTool,
    AwaitingApproval,
    WaitingForUser,
    #[default]
    Idle,
    Dead,
}

/// `subscribe` 连接推来的一行，两种形状（见 smeltd.rs 的 handle_subscribe/state op）。
pub enum DaemonStateEvent {
    /// 首帧：全量快照。
    Snapshot(Vec<DaemonSessionState>),
    /// 之后每次：单个会话的变化。
    Update(DaemonSessionState),
}

/// 阻塞：连守护的 `subscribe`，逐行解析转发，直到连接断开（守护重启/没起来）才
/// 返回。调用方负责重连（这个函数本身不重试，一次连接的生命周期而已）。
pub fn subscribe_daemon_states_blocking(tx: &smol::channel::Sender<DaemonStateEvent>) {
    let Ok(mut s) = UnixStream::connect(smeltd_sock_path()) else { return };
    if writeln!(s, "{}", serde_json::json!({ "op": "subscribe" })).is_err() {
        return;
    }
    let reader = BufReader::new(s);
    for line in reader.lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
        if let Some(sessions) = v.get("sessions") {
            if let Ok(list) = serde_json::from_value::<Vec<DaemonSessionState>>(sessions.clone()) {
                if tx.try_send(DaemonStateEvent::Snapshot(list)).is_err() {
                    return; // 接收端（GUI 那边的转发任务）没了，没必要继续读
                }
            }
        } else if let Some(session) = v.get("session") {
            if let Ok(state) = serde_json::from_value::<DaemonSessionState>(session.clone()) {
                if tx.try_send(DaemonStateEvent::Update(state)).is_err() {
                    return;
                }
            }
        }
    }
}
