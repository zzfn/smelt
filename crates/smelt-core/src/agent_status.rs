//! 会话里 agent 的状态：总览页状态徽章、侧栏会话行状态点共用同一份枚举。
//! 借鉴 codex 的 ThreadStatus 细分：「需要处理」不再一锅烩，等审批和一般等待
//! 是不同等级的行动召唤。排列顺序即优先级（值越小越靠前 / 越紧急）。

/// 值 GPUI 无关，纯状态判断——UI 层（`ui_theme`/侧栏）按它上色，不掺渲染逻辑。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AgentStatus {
    /// Claude 等你批准操作（通知文本含 permission/权限等）→ 最高优先，红色。
    WaitingApproval,
    /// 其他需要处理：等输入 / 响铃 / 自定义通知 → 橙色。
    NeedsAttention,
    /// 标题以 Braille spinner 开头 → 运行中，蓝色。
    Running,
    /// 任务刚完成、你还没回应过 → 「有结果可看」，绿色。
    Done,
    /// 其余 → 空闲，灰色。
    Idle,
}

impl AgentStatus {
    /// 优先级序（越小越紧急），与声明序一致：排序、聚合（项目 rail 的组内
    /// 最高优先级状态点）共用。
    pub fn rank(self) -> u8 {
        match self {
            AgentStatus::WaitingApproval => 0,
            AgentStatus::NeedsAttention => 1,
            AgentStatus::Running => 2,
            AgentStatus::Done => 3,
            AgentStatus::Idle => 4,
        }
    }
}
