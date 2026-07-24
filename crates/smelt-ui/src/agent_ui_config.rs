//! 审批通知 / ACP 命令等 agent UI 偏好（`~/.smelt/agent_ui.json`）。数据模型 +
//! 持久化住在这（需要 `gpui::Global`，所以不能挪进不许引 GPUI 的 smelt-core），
//! 设置页怎么渲染这些字段仍在主 crate 的 settings.rs——UI 和数据分层。

use gpui::{App, Global};

use smelt_core::agent_kind::{
    default_acp_cmd, default_acp_codex_cmd, default_acp_copilot_cmd, default_acp_grok_cmd,
    AcpAgentKind, AcpProfile,
};

fn default_true() -> bool {
    true
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct AgentUiConfig {
    /// 状态通道进入「等你批准 / 等你输入」时用 Notification 组件弹出。
    #[serde(default = "default_true")]
    pub notify_awaiting: bool,
    /// Claude ACP 会话的 agent 启动命令（空白分词）。默认 Claude 官方适配器；权限门
    /// 保留——结构化审批正是这条通道的卖点，别在这里加 bypass 类参数。
    ///
    /// 字段名没跟着 `AcpAgentKind` 改成 `acp_claude_cmd`：老配置文件里就叫这个，
    /// 改名等于把用户自定义过的命令悄悄重置回默认。
    #[serde(default = "default_acp_cmd")]
    pub acp_cmd: String,
    /// GitHub Copilot ACP 会话的启动命令。
    #[serde(default = "default_acp_copilot_cmd")]
    pub acp_copilot_cmd: String,
    /// Codex ACP 会话的启动命令。
    #[serde(default = "default_acp_codex_cmd")]
    pub acp_codex_cmd: String,
    /// Grok ACP 会话的启动命令。
    #[serde(default = "default_acp_grok_cmd")]
    pub acp_grok_cmd: String,
    /// 手动添加的 workspace（同一家 agent 可以有好几个，比如 Claude 的默认
    /// `.claude` 和自定义的 `.claude-quant` 并存）。四个基础 agent 槽位不变、
    /// 走各自默认路径；这里只装"额外"的。
    #[serde(default)]
    pub profiles: Vec<AcpProfile>,
}

impl AgentUiConfig {
    /// 某个 agent 种类当前生效的启动命令。
    pub fn acp_cmd_for(&self, agent: AcpAgentKind) -> String {
        match agent {
            AcpAgentKind::Claude => self.acp_cmd.clone(),
            AcpAgentKind::Copilot => self.acp_copilot_cmd.clone(),
            AcpAgentKind::Codex => self.acp_codex_cmd.clone(),
            AcpAgentKind::Grok => self.acp_grok_cmd.clone(),
        }
    }

    /// 改某个 agent 的启动命令（设置页三条输入框共用）。
    pub fn set_acp_cmd_for(&mut self, agent: AcpAgentKind, cmd: String) {
        match agent {
            AcpAgentKind::Claude => self.acp_cmd = cmd,
            AcpAgentKind::Copilot => self.acp_copilot_cmd = cmd,
            AcpAgentKind::Codex => self.acp_codex_cmd = cmd,
            AcpAgentKind::Grok => self.acp_grok_cmd = cmd,
        }
    }

    pub fn find_profile(&self, id: &str) -> Option<&AcpProfile> {
        self.profiles.iter().find(|p| p.id == id)
    }
}

impl Default for AgentUiConfig {
    fn default() -> Self {
        Self {
            notify_awaiting: true,
            acp_cmd: default_acp_cmd(),
            acp_copilot_cmd: default_acp_copilot_cmd(),
            acp_codex_cmd: default_acp_codex_cmd(),
            acp_grok_cmd: default_acp_grok_cmd(),
            profiles: Vec::new(),
        }
    }
}

impl Global for AgentUiConfig {}

fn agent_ui_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".smelt").join("agent_ui.json"))
}

pub fn load_agent_ui_config() -> AgentUiConfig {
    smelt_core::json_store::load_json(agent_ui_path())
}

fn save_agent_ui_config(c: &AgentUiConfig) {
    smelt_core::json_store::save_json(agent_ui_path(), c);
}

pub fn apply_agent_ui(f: impl FnOnce(&mut AgentUiConfig), cx: &mut App) {
    let mut c = cx.global::<AgentUiConfig>().clone();
    f(&mut c);
    save_agent_ui_config(&c);
    cx.set_global(c);
}
