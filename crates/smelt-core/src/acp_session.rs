//! ACP 会话的无 GPUI 状态机：`AcpEvent` → 可展示状态的归约逻辑，原来长在
//! `crates/smelt/src/acp_view.rs`（GPUI 绑定），现在挪到这——smeltd 要托管 ACP
//! 会话（GUI 退出不中断），谁持有连接谁就得跑这份归约（见 `AcpEvent::Permission`/
//! `Elicitation` 带的 responder：绑在连接线程上的一次性回执，没法跨进程传，
//! 所以「谁接手连接」这件事没有选择余地，只能是 smeltd）。
//!
//! 分两层类型：
//! - `AcpSessionState`：服务端（smeltd）持有的完整活体状态，permission/
//!   elicitation 待办卡片里揣着真正的 responder，只能在本进程内消费。
//! - `AcpSnapshot`：`AcpSessionState` 去掉 responder 之后能序列化的镜像，是
//!   smeltd → GUI（以后是 → web/mobile）那条 wire 的唯一内容。GUI 侧只认这份
//!   快照，再也不碰 `agent_client_protocol` 的 schema 类型。
//!
//! 回中的动作走反方向：GUI 发 `AcpUserAction`（纯数据，无 responder），smeltd
//! 收到后要么转发进连接线程的 `AcpCommand`（Prompt/Cancel/SetModel/Shutdown），
//! 要么直接消费自己攥着的 responder（PermissionSelect/Elicitation*）。

use std::collections::BTreeMap;

use agent_client_protocol::schema::v1::{
    ElicitationContentValue, Plan, PlanEntryStatus, PermissionOptionKind,
};

use crate::acp_chat::AcpEntry;
use crate::acp_conn::{
    AcpEvent, ElicitField, ElicitFieldKind, ElicitationResponder, ModelState, PermissionResponder,
    PromptImage, ReadyKind,
};

// ===================== wire 快照类型（无 agent_client_protocol 依赖） =====================

/// 会话相位。GUI 舞台头胶囊 / 四色状态点都从这个派生。
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum AcpPhase {
    Starting,
    Idle,
    Running,
    AwaitingApproval,
    AwaitingChoice,
    /// 连接不可恢复地结束（Fatal / 占位恢复），带原因文本。
    Ended(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PermissionOptionKindView {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

impl PermissionOptionKindView {
    fn from_acp(k: PermissionOptionKind) -> Self {
        match k {
            PermissionOptionKind::AllowOnce => Self::AllowOnce,
            PermissionOptionKind::AllowAlways => Self::AllowAlways,
            PermissionOptionKind::RejectOnce => Self::RejectOnce,
            PermissionOptionKind::RejectAlways => Self::RejectAlways,
            // #[non_exhaustive]：协议以后加新分类先当「拒绝一次」——比默认允许安全。
            _ => Self::RejectOnce,
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PermissionOptionView {
    pub option_id: String,
    pub name: String,
    pub kind: PermissionOptionKindView,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PendingPermission {
    pub question: String,
    pub tool_call_id: String,
    pub options: Vec<PermissionOptionView>,
}

/// 选择题字段的展示形态——**不带** `ElicitationContentValue`：客户端只按
/// `(字段下标, 选项下标)` 回选，真正的协议值只在 smeltd 自己持有的
/// `AcpSessionState`（非快照那份）里，翻译成 `ElicitationContentValue` 是
/// `submit_elicitation` 收到 `AcpUserAction::ElicitationSubmit` 时才做的事。
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ElicitOptionView {
    pub label: String,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum ElicitFieldKindView {
    Select(Vec<ElicitOptionView>),
    MultiSelect(Vec<ElicitOptionView>),
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ElicitFieldView {
    pub key: String,
    pub title: String,
    pub kind: ElicitFieldKindView,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PendingElicitation {
    pub message: String,
    pub fields: Vec<ElicitFieldView>,
    /// 已选中的 (字段下标 → 选项下标列表)，跟旧版 `ElicitCard.chosen` 同一份
    /// 语义——GUI 要能画出「已经点了哪些」，不能重连一次就清空選択态。
    pub chosen: BTreeMap<usize, Vec<usize>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PlanEntryStatusView {
    Pending,
    InProgress,
    Completed,
}

impl PlanEntryStatusView {
    fn from_acp(s: PlanEntryStatus) -> Self {
        match s {
            PlanEntryStatus::Completed => Self::Completed,
            PlanEntryStatus::InProgress => Self::InProgress,
            _ => Self::Pending,
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PlanEntryView {
    pub content: String,
    pub status: PlanEntryStatusView,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PlanView {
    pub entries: Vec<PlanEntryView>,
}

fn plan_view_from_acp(p: &Plan) -> PlanView {
    PlanView {
        entries: p
            .entries
            .iter()
            .map(|e| PlanEntryView {
                content: e.content.clone(),
                status: PlanEntryStatusView::from_acp(e.status.clone()),
            })
            .collect(),
    }
}

/// smeltd → GUI 的完整快照：`acp_watch`/`acp_open` 接上时发一份，之后每次
/// `apply_event` 有实质变化再发一份（懒得做增量 diff——快照本身就不大，
/// 消息流几十条 + 几个标量字段，序列化成本远低于维护增量协议的复杂度）。
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AcpSnapshot {
    pub entries: Vec<AcpEntry>,
    pub phase: AcpPhase,
    pub pending_permission: Option<PendingPermission>,
    pub pending_elicitation: Option<PendingElicitation>,
    pub status_line: Option<String>,
    pub acp_session_id: Option<String>,
    pub supports_image: bool,
    pub available_commands: Vec<(String, String)>,
    pub usage: Option<(u64, u64)>,
    pub plan: Option<PlanView>,
    pub model: Option<ModelState>,
    /// 回合结束且没人看过 → 「有结果可看」绿点，跟旧版 `completed_unread` 同一
    /// 语义，只是现在从服务端算，客户端不用自己维护。
    pub completed_unread: bool,
    /// 这份快照值不值得触发一次落盘。**不是**"数据有没有变"（每次推送数据
    /// 都变了），是旧版 `apply_event` 里 `skip_persist` 那条线的服务端版本：
    /// 流式增量（AgentChunk/Plan/Model/Usage）推快照是为了实时画面，但不该
    /// 把每次落盘都变成写盘风暴——完整内容在 TurnEnded 时已经在 entries 里
    /// 了，那时候存一次就够。客户端拿这个字段决定要不要 `cx.emit(Changed)`，
    /// 不用自己在两次快照之间做增量判断。
    pub should_persist: bool,
}

// ===================== 服务端活体状态 =====================

/// 待审批卡片：`responder` 只在收到 `AcpUserAction::PermissionSelect` 时消费。
pub struct LivePermission {
    pub question: String,
    pub tool_call_id: String,
    pub options: Vec<PermissionOptionView>,
    pub responder: Option<PermissionResponder>,
    /// 这张卡对应请求的原始 JSON-RPC 行，smeltd 无缝升级时用来重放（见
    /// `AcpEvent::Permission` 同名字段）。
    pub raw_request_line: Option<String>,
}

/// 选择题卡片：字段原始形态保留在 `raw_fields`（翻译回
/// `ElicitationContentValue` 要用），`chosen` 是当前選択态。
pub struct LiveElicitation {
    pub message: String,
    pub raw_fields: Vec<ElicitField>,
    pub chosen: BTreeMap<usize, Vec<usize>>,
    pub responder: Option<ElicitationResponder>,
    /// 同 `LivePermission::raw_request_line`。
    pub raw_request_line: Option<String>,
}

/// smeltd 侧一份 ACP 会话的完整活体状态。`apply_event`/`apply_user_action` 是
/// 仅有的两个 mutator，跟旧版 `AcpView::apply_event`/`pick_permission` 等方法
/// 一一对应，去掉的只是 GPUI 相关的那半（`cx.notify()`/`sync_daemon_state`
/// 这些，见 `ApplyOutcome`）。
pub struct AcpSessionState {
    pub entries: Vec<AcpEntry>,
    pub phase: AcpPhase,
    pub permission: Option<LivePermission>,
    pub elicitation: Option<LiveElicitation>,
    pub completed_unread: bool,
    pub status_line: Option<String>,
    pub acp_session_id: Option<String>,
    pub supports_image: bool,
    /// 「等自己刚发那条 prompt 的回声」，见旧版字段同名注释——语义原样保留。
    pub awaiting_user_echo: bool,
    pub available_commands: Vec<(String, String)>,
    pub usage: Option<(u64, u64)>,
    pub plan: Option<PlanView>,
    pub model: Option<ModelState>,
}

impl Default for AcpSessionState {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            phase: AcpPhase::Starting,
            permission: None,
            elicitation: None,
            completed_unread: false,
            status_line: None,
            acp_session_id: None,
            supports_image: true,
            awaiting_user_echo: false,
            available_commands: Vec::new(),
            usage: None,
            plan: None,
            model: None,
        }
    }
}

impl AcpSessionState {
    /// 冷恢复占位：只有落盘的历史消息 + 上次的 agent session id，还没有
    /// 连接。跟旧版 `AcpView::placeholder` 的字段初始化一一对应。
    pub fn placeholder(entries: Vec<AcpEntry>, resume_session_id: Option<String>, reason: String) -> Self {
        Self { entries, phase: AcpPhase::Ended(reason), acp_session_id: resume_session_id, ..Self::default() }
    }

    /// 当前有没有一张卡（权限/选择题）正等着人处理，有就带上它原始请求那行
    /// ——smeltd 无缝升级时用来判断"这条会话要不要在交接文件里多带一行"
    /// 以及 resume 时重放这行，见 `resume_acp_from_fds`。同一时刻协议上只会
    /// 有一张卡挂起（agent 等到上一个请求有回应才会发下一个），不用管两者
    /// 都有值的情况。
    pub fn pending_raw_request_line(&self) -> Option<&str> {
        self.permission
            .as_ref()
            .and_then(|p| p.raw_request_line.as_deref())
            .or_else(|| self.elicitation.as_ref().and_then(|e| e.raw_request_line.as_deref()))
    }

    /// `should_persist` 不是从 `self` 能算出来的——它是"这次变化是怎么发生的"
    /// 这个上下文信息，调用方（smeltd 的事件循环）从 `apply_event` 的返回值
    /// 里拿，这里只负责原样塞进快照，见该字段注释。
    pub fn to_snapshot(&self, should_persist: bool) -> AcpSnapshot {
        AcpSnapshot {
            entries: self.entries.clone(),
            phase: self.phase.clone(),
            pending_permission: self.permission.as_ref().map(|p| PendingPermission {
                question: p.question.clone(),
                tool_call_id: p.tool_call_id.clone(),
                options: p.options.clone(),
            }),
            pending_elicitation: self.elicitation.as_ref().map(|e| PendingElicitation {
                message: e.message.clone(),
                fields: e.raw_fields.iter().map(elicit_field_view).collect(),
                chosen: e.chosen.clone(),
            }),
            status_line: self.status_line.clone(),
            acp_session_id: self.acp_session_id.clone(),
            supports_image: self.supports_image,
            available_commands: self.available_commands.clone(),
            usage: self.usage,
            plan: self.plan.clone(),
            model: self.model.clone(),
            completed_unread: self.completed_unread,
            should_persist,
        }
    }
}

fn elicit_field_view(f: &ElicitField) -> ElicitFieldView {
    ElicitFieldView {
        key: f.key.clone(),
        title: f.title.clone(),
        kind: match &f.kind {
            ElicitFieldKind::Select(opts) => ElicitFieldKindView::Select(
                opts.iter().map(|o| ElicitOptionView { label: o.label.clone() }).collect(),
            ),
            ElicitFieldKind::MultiSelect(opts) => ElicitFieldKindView::MultiSelect(
                opts.iter().map(|o| ElicitOptionView { label: o.label.clone() }).collect(),
            ),
        },
    }
}

/// `apply_event` 的旁路效果——旧版直接在 GPUI `Context` 上做（`cx.notify()`/
/// `cx.emit(Changed)`/推 `PendingAgentNotifs`），归约函数本身不该管这些，
/// 交给调用方（smeltd）根据这份结果自己决定广播/落盘/要不要弹通知。
#[derive(Default)]
pub struct ApplyOutcome {
    /// 值得持久化（entries 有实质变化，排除逐块流式增量）。
    pub should_persist: bool,
    /// 相位刚变成需要人处理 → (标题, 正文, is_approval)，调用方决定要不要弹
    /// 通知（GUI 按 `AgentUiConfig.notify_awaiting` 开关；这个决定权不下放到
    /// smeltd，因为那是纯 GUI 展示偏好，smeltd 不该知道）。
    pub notify: Option<(String, String, bool)>,
}

/// 事件归约：entries 合并 + phase 机。跟旧版 `AcpView::apply_event` 逐行对应，
/// 唯一的行为差异是旁路效果收进返回值而不是直接执行。
pub fn apply_event(state: &mut AcpSessionState, ev: AcpEvent) -> ApplyOutcome {
    let mut outcome = ApplyOutcome::default();

    let skip_persist = matches!(
        ev,
        AcpEvent::AgentChunk { .. } | AcpEvent::Plan(_) | AcpEvent::Model(_) | AcpEvent::Usage { .. }
    );
    if !matches!(
        ev,
        AcpEvent::UserChunk(_) | AcpEvent::Status(_) | AcpEvent::AvailableCommands(_) | AcpEvent::Usage { .. }
    ) {
        state.awaiting_user_echo = false;
    }

    match ev {
        AcpEvent::AvailableCommands(list) => {
            state.available_commands = list;
        }
        AcpEvent::Usage { used, size, .. } => {
            state.usage = (size > 0).then_some((used, size));
        }
        AcpEvent::Status(msg) => {
            state.status_line = Some(msg);
        }
        AcpEvent::UserChunk(text) => {
            if state.awaiting_user_echo {
                // 自己刚发那条的回声——本地已经在 apply_user_action(Prompt) 时显示过了。
            } else {
                match state.entries.last_mut() {
                    Some(AcpEntry::User(t)) => t.push_str(&text),
                    _ => state.entries.push(AcpEntry::User(text)),
                }
            }
        }
        AcpEvent::Ready { session_id, kind, supports_image } => {
            state.acp_session_id = Some(session_id.to_string());
            state.supports_image = supports_image;
            match kind {
                ReadyKind::ResumedWithReplay => state.entries.clear(),
                ReadyKind::ResumedKeepHistory => {}
                ReadyKind::Fresh if !state.entries.is_empty() => {
                    state.entries.push(AcpEntry::Divider(format!(
                        "新会话 · agent 不记得以上内容 · {}",
                        chrono::Local::now().format("%m-%d %H:%M")
                    )));
                }
                ReadyKind::Fresh => {}
            }
            state.phase = AcpPhase::Idle;
            state.status_line = None;
        }
        AcpEvent::AgentChunk { thought, text } => {
            match state.entries.last_mut() {
                Some(AcpEntry::Assistant { text: t, thought: th }) if *th == thought => {
                    t.push_str(&text);
                }
                _ => state.entries.push(AcpEntry::Assistant { text, thought }),
            }
            state.phase = AcpPhase::Running;
        }
        AcpEvent::ToolCall(tc) => {
            state.entries.push(AcpEntry::ToolCall {
                id: tc.tool_call_id.to_string(),
                title: tc.title,
                kind: crate::acp_conn::tool_kind_from_acp(tc.kind),
                status: crate::acp_conn::tool_status_from_acp(tc.status),
                output: crate::acp_conn::tool_content_parts(&tc.content),
            });
            state.phase = AcpPhase::Running;
        }
        AcpEvent::ToolCallUpdate(u) => {
            let update_id = u.tool_call_id.to_string();
            if let Some(AcpEntry::ToolCall { title, kind, status, output, .. }) = state
                .entries
                .iter_mut()
                .rev()
                .find(|e| matches!(e, AcpEntry::ToolCall { id, .. } if *id == update_id))
            {
                if let Some(t) = u.fields.title {
                    *title = t;
                }
                if let Some(k) = u.fields.kind {
                    *kind = crate::acp_conn::tool_kind_from_acp(k);
                }
                if let Some(s) = u.fields.status {
                    *status = crate::acp_conn::tool_status_from_acp(s);
                }
                if let Some(c) = u.fields.content {
                    *output = crate::acp_conn::tool_content_parts(&c);
                }
            }
        }
        AcpEvent::Model(m) => {
            state.model = Some(m);
        }
        AcpEvent::Plan(p) => {
            state.plan = Some(plan_view_from_acp(&p));
            state.phase = AcpPhase::Running;
        }
        AcpEvent::Permission { question, tool_call_id, pub_options, responder, raw_request_line } => {
            let options: Vec<PermissionOptionView> = pub_options
                .iter()
                .map(|o| PermissionOptionView {
                    option_id: o.option_id.to_string(),
                    name: o.name.clone(),
                    kind: PermissionOptionKindView::from_acp(o.kind),
                })
                .collect();
            state.permission = Some(LivePermission {
                question: question.clone(),
                tool_call_id: tool_call_id.to_string(),
                options,
                responder: Some(responder),
                raw_request_line,
            });
            state.phase = AcpPhase::AwaitingApproval;
            outcome.notify = Some(("等你批准".to_string(), question, true));
        }
        AcpEvent::Elicitation { message, fields, responder, raw_request_line } => {
            state.elicitation = Some(LiveElicitation {
                message: message.clone(),
                raw_fields: fields,
                chosen: Default::default(),
                responder: Some(responder),
                raw_request_line,
            });
            state.phase = AcpPhase::AwaitingChoice;
            outcome.notify = Some(("等你选择".to_string(), message, false));
        }
        AcpEvent::TurnEnded(reason) => {
            state.permission = None;
            state.elicitation = None;
            state.phase = AcpPhase::Idle;
            state.completed_unread = true;
            let _ = reason;
        }
        AcpEvent::Fatal(msg) => {
            state.permission = None;
            state.elicitation = None;
            state.phase = AcpPhase::Ended(msg);
        }
    }

    outcome.should_persist = !skip_persist;
    outcome
}

/// 「重新开始」/新建时的相位重置：跟旧版 `AcpView::restart` 里那几行对应（cmd/
/// spawn 那部分是 smeltd 的事，不在这个纯状态函数里）。
pub fn reset_for_restart(state: &mut AcpSessionState) {
    state.permission = None;
    state.elicitation = None;
    state.plan = None;
    state.model = None;
    state.usage = None;
    state.completed_unread = false;
    state.phase = AcpPhase::Starting;
}

/// 用户发的一条 prompt（本地立即回显 + 打开等回声窗口），跟旧版 `send_prompt`
/// 里非 I/O 的那部分对应（`h.cmd_tx.try_send` 由调用方在成功后自己做，因为
/// 这个函数不持有 `AcpHandle`）。
pub fn note_prompt_sent(state: &mut AcpSessionState, shown_text: String) {
    state.entries.push(AcpEntry::User(shown_text));
    state.awaiting_user_echo = true;
    state.phase = AcpPhase::Running;
    state.completed_unread = false;
}

/// 权限审批：`option_id` 对不上当前卡片的任何选项就什么都不做（客户端发的
/// action 可能是过期请求——卡片已经因为别的原因被清掉）。
pub fn select_permission(state: &mut AcpSessionState, option_id: &str) {
    let Some(card) = &mut state.permission else { return };
    if !card.options.iter().any(|o| o.option_id == option_id) {
        return;
    }
    if let Some(responder) = card.responder.take() {
        responder.select(agent_client_protocol::schema::v1::PermissionOptionId::new(
            option_id.to_string(),
        ));
    }
    state.permission = None;
    state.phase = AcpPhase::Running;
}

/// 选择题点选：单选替换，多选 toggle，跟旧版 `pick_elicit_option` 一致。
/// 返回 true 表示这是「整卡单字段单选」的快捷路径，调用方应该紧接着调用
/// `submit_elicitation`（旧版点了就直接提交，不用等再按一次「确定」）。
pub fn choose_elicitation(state: &mut AcpSessionState, field_ix: usize, opt_ix: usize) -> bool {
    let Some(card) = &mut state.elicitation else { return false };
    let Some(field) = card.raw_fields.get(field_ix) else { return false };
    match &field.kind {
        ElicitFieldKind::Select(_) => {
            card.chosen.insert(field_ix, vec![opt_ix]);
        }
        ElicitFieldKind::MultiSelect(_) => {
            let sel = card.chosen.entry(field_ix).or_default();
            if let Some(pos) = sel.iter().position(|&i| i == opt_ix) {
                sel.remove(pos);
            } else {
                sel.push(opt_ix);
            }
        }
    }
    card.raw_fields.len() == 1 && matches!(card.raw_fields[0].kind, ElicitFieldKind::Select(_))
}

/// 提交选择题：把 `chosen` 翻译回 `ElicitationContentValue` 传给 responder。
/// 跟旧版 `submit_elicitation` 一致；字段没有選択就跳过（agent 那边按 schema
/// 自己决定必填与否，这里不做客户端校验）。
pub fn submit_elicitation(state: &mut AcpSessionState) {
    let Some(mut card) = state.elicitation.take() else { return };
    let Some(responder) = card.responder.take() else { return };
    let mut content = BTreeMap::new();
    for (ix, field) in card.raw_fields.iter().enumerate() {
        let Some(sel) = card.chosen.get(&ix) else { continue };
        match &field.kind {
            ElicitFieldKind::Select(options) => {
                if let Some(opt) = sel.first().and_then(|&i| options.get(i)) {
                    content.insert(field.key.clone(), opt.value.clone());
                }
            }
            ElicitFieldKind::MultiSelect(options) => {
                let values: Vec<String> = sel
                    .iter()
                    .filter_map(|&i| options.get(i))
                    .filter_map(|o| match &o.value {
                        ElicitationContentValue::String(s) => Some(s.clone()),
                        _ => None,
                    })
                    .collect();
                content.insert(field.key.clone(), ElicitationContentValue::StringArray(values));
            }
        }
    }
    responder.accept(content);
    state.phase = AcpPhase::Running;
}

/// 「跳过」：丢卡片，responder Drop 自动回 Cancel（见 `ElicitationResponder`
/// 的 Drop 实现）。
pub fn dismiss_elicitation(state: &mut AcpSessionState) {
    state.elicitation = None;
    state.phase = AcpPhase::Running;
}

/// 一份 turn 结束/连接终止后要不要自动续接（冷恢复占位第一次被访问时）：
/// 有旧 session id 才值得——没有 id 只能开全新会话，交给用户手动决定。
pub fn should_auto_resume(state: &AcpSessionState) -> bool {
    matches!(state.phase, AcpPhase::Ended(_)) && state.acp_session_id.is_some()
}

/// GUI → smeltd 的用户动作，走 `acp_open` 连接的 JSON 行。prompt/取消/切模型
/// 三种转发进连接线程原有的 `AcpCommand`；权限/选择题四种直接消费
/// `AcpSessionState` 自己攥着的 responder，不经过连接线程（那几种压根不是发给
/// agent 的 JSON-RPC 请求，是在回上一条来自 agent 的请求）。`PromptImage`
/// 复用连接层已有的 wire 形状，没有另造一份。
///
/// 没有 `Shutdown`：关闭子进程是会话生命周期层面的事，走独立的 `acp_kill` op
/// （同终端会话的 `kill`），不是"在一条打开的连接里发的一个动作"——GUI 断开
/// `acp_open` 连接（切标签/关标签/退出 App）只是摘掉这条连接，会话照样在
/// smeltd 里活着，这正是这一整层要解决的问题。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum AcpUserAction {
    Prompt { text: String, images: Vec<PromptImage> },
    Cancel,
    SetModel(String),
    PermissionSelect { option_id: String },
    ElicitationChoose { field_ix: usize, opt_ix: usize },
    ElicitationSubmit,
    ElicitationDismiss,
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::StopReason;

    fn fresh_state() -> AcpSessionState {
        AcpSessionState::default()
    }

    #[test]
    fn agent_chunk_appends_and_merges_consecutive_same_kind() {
        let mut s = fresh_state();
        apply_event(&mut s, AcpEvent::AgentChunk { thought: false, text: "he".into() });
        apply_event(&mut s, AcpEvent::AgentChunk { thought: false, text: "llo".into() });
        assert_eq!(s.entries.len(), 1);
        assert!(matches!(&s.entries[0], AcpEntry::Assistant { text, thought: false } if text == "hello"));
        assert!(matches!(s.phase, AcpPhase::Running));
    }

    #[test]
    fn user_echo_suppressed_once_after_prompt_sent() {
        let mut s = fresh_state();
        note_prompt_sent(&mut s, "hi".into());
        assert!(s.awaiting_user_echo);
        // 回声窗口内收到 UserChunk：吞掉，不重复追加。
        apply_event(&mut s, AcpEvent::UserChunk("hi".into()));
        assert_eq!(s.entries.len(), 1);
        // 任何非 UserChunk/Status/AvailableCommands/Usage 事件都清掉等回声窗口。
        apply_event(&mut s, AcpEvent::AgentChunk { thought: false, text: "ok".into() });
        assert!(!s.awaiting_user_echo);
        // 窗口关闭后再来的 UserChunk 是重放历史，正常追加。
        apply_event(&mut s, AcpEvent::UserChunk("old question".into()));
        assert_eq!(s.entries.len(), 3);
        assert!(matches!(&s.entries[2], AcpEntry::User(t) if t == "old question"));
    }

    #[test]
    fn ready_resumed_with_replay_clears_local_entries() {
        let mut s = fresh_state();
        s.entries.push(AcpEntry::User("old".into()));
        apply_event(
            &mut s,
            AcpEvent::Ready {
                session_id: agent_client_protocol::schema::v1::SessionId::new("sid-1"),
                kind: ReadyKind::ResumedWithReplay,
                supports_image: true,
            },
        );
        assert!(s.entries.is_empty());
        assert_eq!(s.acp_session_id.as_deref(), Some("sid-1"));
    }

    #[test]
    fn ready_resumed_keep_history_preserves_local_entries() {
        let mut s = fresh_state();
        s.entries.push(AcpEntry::User("old".into()));
        apply_event(
            &mut s,
            AcpEvent::Ready {
                session_id: agent_client_protocol::schema::v1::SessionId::new("sid-1"),
                kind: ReadyKind::ResumedKeepHistory,
                supports_image: true,
            },
        );
        assert_eq!(s.entries.len(), 1);
    }

    #[test]
    fn ready_fresh_with_existing_history_inserts_divider() {
        let mut s = fresh_state();
        s.entries.push(AcpEntry::User("old".into()));
        apply_event(
            &mut s,
            AcpEvent::Ready {
                session_id: agent_client_protocol::schema::v1::SessionId::new("sid-1"),
                kind: ReadyKind::Fresh,
                supports_image: true,
            },
        );
        assert_eq!(s.entries.len(), 2);
        assert!(matches!(s.entries[1], AcpEntry::Divider(_)));
    }

    #[test]
    fn turn_ended_clears_pending_cards_and_marks_unread() {
        let mut s = fresh_state();
        s.phase = AcpPhase::Running;
        apply_event(&mut s, AcpEvent::TurnEnded(StopReason::EndTurn));
        assert!(matches!(s.phase, AcpPhase::Idle));
        assert!(s.completed_unread);
    }

    #[test]
    fn fatal_ends_session_and_keeps_reason() {
        let mut s = fresh_state();
        apply_event(&mut s, AcpEvent::Fatal("boom".into()));
        assert!(matches!(&s.phase, AcpPhase::Ended(reason) if reason == "boom"));
    }

    #[test]
    fn should_persist_excludes_streaming_and_ephemeral_events() {
        let mut s = fresh_state();
        let o = apply_event(&mut s, AcpEvent::AgentChunk { thought: false, text: "x".into() });
        assert!(!o.should_persist);
        let o = apply_event(&mut s, AcpEvent::TurnEnded(StopReason::EndTurn));
        assert!(o.should_persist);
    }

    #[test]
    fn choose_elicitation_single_select_signals_auto_submit() {
        use agent_client_protocol::schema::v1::ElicitationContentValue as V;
        let mut s = fresh_state();
        s.elicitation = Some(LiveElicitation {
            message: "pick one".into(),
            raw_fields: vec![ElicitField {
                key: "k".into(),
                title: "t".into(),
                kind: ElicitFieldKind::Select(vec![
                    crate::acp_conn::ElicitOption { value: V::String("a".into()), label: "A".into() },
                    crate::acp_conn::ElicitOption { value: V::String("b".into()), label: "B".into() },
                ]),
            }],
            chosen: Default::default(),
            responder: None,
            raw_request_line: None,
        });
        let auto_submit = choose_elicitation(&mut s, 0, 1);
        assert!(auto_submit);
        assert_eq!(s.elicitation.as_ref().unwrap().chosen.get(&0), Some(&vec![1]));
    }

    #[test]
    fn choose_elicitation_multi_select_toggles() {
        use agent_client_protocol::schema::v1::ElicitationContentValue as V;
        let mut s = fresh_state();
        s.elicitation = Some(LiveElicitation {
            message: "pick many".into(),
            raw_fields: vec![ElicitField {
                key: "k".into(),
                title: "t".into(),
                kind: ElicitFieldKind::MultiSelect(vec![
                    crate::acp_conn::ElicitOption { value: V::String("a".into()), label: "A".into() },
                    crate::acp_conn::ElicitOption { value: V::String("b".into()), label: "B".into() },
                ]),
            }],
            chosen: Default::default(),
            responder: None,
            raw_request_line: None,
        });
        let auto_submit = choose_elicitation(&mut s, 0, 0);
        assert!(!auto_submit); // multi-select 从不自动提交
        assert_eq!(s.elicitation.as_ref().unwrap().chosen.get(&0), Some(&vec![0]));
        choose_elicitation(&mut s, 0, 0); // 再点一次 = 取消
        assert_eq!(s.elicitation.as_ref().unwrap().chosen.get(&0), Some(&vec![]));
    }

    #[test]
    fn should_auto_resume_requires_ended_and_known_session_id() {
        let mut s = fresh_state();
        s.phase = AcpPhase::Ended("gone".into());
        assert!(!should_auto_resume(&s)); // 没有旧 session id
        s.acp_session_id = Some("sid-1".into());
        assert!(should_auto_resume(&s));
        s.phase = AcpPhase::Idle;
        assert!(!should_auto_resume(&s)); // 还活着，用不上「自动续接」
    }
}
