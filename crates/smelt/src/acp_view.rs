//! ACP 会话的消息流视图：第二种会话类型的 GPUI 皮肤。
//!
//! 与 acp.rs 的分工：那边是连接层（不许引 gpui），这边持 `AcpHandle` 消费事件、
//! 渲染消息流 + 权限审批卡片 + 输入框，并把 phase 翻译进 `DaemonStates` 全局——
//! 四档着色 / Dock 角标 / 应用内通知全部复用终端会话的既有链路，零新增。

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, App, AppContext, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, ParentElement, Render, StatefulInteractiveElement, Styled,
    Window,
};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{h_flex, v_flex, ActiveTheme, Icon, IconName, StyledExt};

use agent_client_protocol::schema::v1::{
    PermissionOption, PermissionOptionKind, SessionId, ToolCallId, ToolCallStatus, ToolKind,
};

use crate::acp::{
    spawn_acp, AcpCommand, AcpEvent, AcpHandle, AcpLaunch, ElicitField, ElicitFieldKind,
    ElicitationResponder, PermissionResponder,
};
use crate::terminal::{DaemonPhase, DaemonSessionState};

/// 消息流里的一条。落盘持久化（见 main.rs 的 AcpSaved），进程重启 / 会话
/// 「重新开始」都要保住历史，不能让 agent 一断线聊天记录就没了。
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) enum AcpEntry {
    User(String),
    /// assistant 正文或思考块（thought 弱化显示）；连续 chunk 就地追加。
    Assistant { text: String, thought: bool },
    ToolCall {
        id: ToolCallId,
        title: String,
        kind: ToolKind,
        status: ToolCallStatus,
        output: String,
    },
    /// 「重新开始」在旧对话和新对话之间插的分割线（不清空历史，只做标记）。
    Divider(String),
}

/// 会话相位（UI 侧状态机；翻译成 DaemonPhase 进全局）。
enum AcpPhase {
    /// spawn 到 Ready 之间。
    Starting,
    Idle,
    Running,
    AwaitingApproval,
    /// agent 出了选择题（elicitation），等用户点选。
    AwaitingChoice,
    /// 连接不可恢复地结束（Fatal / 占位恢复）。带原因文本。
    Ended(String),
}

/// 待审批卡片：responder 是 Option 因为 select 消费所有权（从 &mut self take）。
struct PermissionCard {
    question: String,
    options: Vec<PermissionOption>,
    responder: Option<PermissionResponder>,
}

/// 选择题卡片（elicitation form）。chosen 按字段存选中的 option 下标。
struct ElicitCard {
    message: String,
    fields: Vec<ElicitField>,
    chosen: std::collections::BTreeMap<usize, Vec<usize>>,
    responder: Option<ElicitationResponder>,
}

/// AcpView 对外发的唯一事件：内容有实质变化，该存盘了。main.rs 订阅它触发
/// `Workspace::save_state`（与侧栏 resize 订阅同一惯用法）。
pub enum AcpViewEvent {
    Changed,
}

impl EventEmitter<AcpViewEvent> for AcpView {}

pub struct AcpView {
    sid: String,
    cwd: Option<String>,
    entries: Vec<AcpEntry>,
    permission: Option<PermissionCard>,
    elicitation: Option<ElicitCard>,
    phase: AcpPhase,
    /// 回合结束且用户还没回应过 → Session 状态给绿点「有结果可看」。
    completed_unread: bool,
    /// 启动阶段的进度文案（下载运行时等），Starting 横幅显示。
    status_line: Option<String>,
    /// None = 已结束的占位视图（重开后才建；Ended 态没有输入框）。
    input: Option<Entity<InputState>>,
    handle: Option<AcpHandle>,
    /// 重启用的启动参数（placeholder / restart 共用）。
    cmd: String,
    /// agent 侧真实的 session id：握手成功后写入，`restart()` 拿它去尝试
    /// `session/load` 真续接；也存盘（main.rs AcpSaved），GUI 重开后同样能续。
    acp_session_id: Option<SessionId>,
    /// 「等自己刚发那条 prompt 的回声」——不确定 adapter 是否会在 live 对话时也
    /// 回显 UserMessageChunk（已确认的是 session/load 重放一定会发）；两种可能
    /// 都要处理对：send_prompt 已经手动 push 过一次，这段窗口内若真收到回声就
    /// 吞掉不重复；等到 agent 给出实质响应/turn 收尾就清掉，之后的 UserChunk
    /// （只可能来自 replay）正常追加显示。
    awaiting_user_echo: bool,
    scroll: gpui::ScrollHandle,
    focus_handle: FocusHandle,
    _input_sub: Option<gpui::Subscription>,
}

impl AcpView {
    /// 建视图并立即起连接线程（spawn_acp 非阻塞，握手结果以事件回来）。
    pub fn start(
        window: &mut Window,
        cx: &mut Context<Self>,
        cmd: String,
        cwd: Option<String>,
    ) -> Self {
        let mut this = Self::placeholder(cx, cmd, cwd, String::new(), Vec::new(), None);
        this.phase = AcpPhase::Starting;
        this.init_input(window, cx);
        let handle = spawn_acp(AcpLaunch {
            cmd: this.cmd.clone(),
            cwd: this.cwd.clone(),
            sid: this.sid.clone(),
            resume_session_id: None, // 第一次开，没有旧会话可续
        });
        this.attach_handle(handle, cx);
        this
    }

    /// 冷启动恢复用的占位：不起进程、没有输入框，显示「已结束」+「重新开始」。
    /// `entries` 是上次落盘的历史消息（`Vec::new()` = 首次创建，还没有历史）；
    /// `resume_session_id` 是上次握手成功后 agent 分配的 session id，有它才有
    /// 机会在「重新开始」时真续接。
    pub fn placeholder(
        cx: &mut Context<Self>,
        cmd: String,
        cwd: Option<String>,
        reason: String,
        entries: Vec<AcpEntry>,
        resume_session_id: Option<SessionId>,
    ) -> Self {
        Self {
            sid: format!("acp-{}", uuid::Uuid::new_v4()),
            cwd,
            entries,
            permission: None,
            elicitation: None,
            status_line: None,
            phase: AcpPhase::Ended(reason),
            completed_unread: false,
            input: None,
            handle: None,
            cmd,
            acp_session_id: resume_session_id,
            awaiting_user_echo: false,
            scroll: gpui::ScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            _input_sub: None,
        }
    }

    /// 「重新开始」：带着上次的 session id（如果有）尝试 `session/load` 真续接
    /// ——agent 记得之前聊了什么，会重放完整历史（此时 apply_event 收到
    /// `Ready{resumed:true}` 会清空本地历史，让 replay 重建，避免重复）。
    /// adapter 不支持该能力、或旧会话已失效，自动退回全新会话：本地历史保留在
    /// 分割线上方（`Ready{resumed:false}` 触发），不会丢。
    fn restart(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.permission = None;
        self.elicitation = None;
        self.completed_unread = false;
        self.phase = AcpPhase::Starting;
        self.init_input(window, cx);
        let handle = spawn_acp(AcpLaunch {
            cmd: self.cmd.clone(),
            cwd: self.cwd.clone(),
            sid: self.sid.clone(),
            resume_session_id: self.acp_session_id.clone(),
        });
        self.attach_handle(handle, cx);
        cx.notify();
    }

    fn init_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.input.is_some() {
            return;
        }
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("给 agent 的指令，Enter 发送，Shift+Enter 换行")
                .multi_line(true)
                .auto_grow(1, 8)
        });
        self._input_sub = Some(cx.subscribe_in(
            &input,
            window,
            |this: &mut Self, _input, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { shift, .. } = ev {
                    if !shift {
                        this.submit_input(window, cx);
                    }
                }
            },
        ));
        self.input = Some(input);
    }

    /// 挂上连接句柄并起事件 drain（start / restart 共用）。
    fn attach_handle(&mut self, handle: AcpHandle, cx: &mut Context<Self>) {
        let event_rx = handle.event_rx.clone();
        self.handle = Some(handle);
        cx.spawn(async move |this, cx| {
            while let Ok(ev) = event_rx.recv().await {
                if this.update(cx, |view, cx| view.apply_event(ev, cx)).is_err() {
                    return; // 视图已销毁
                }
            }
        })
        .detach();
    }

    /// 全局状态表 / 持久化（Step 8）/ 远程透出（P2）都以它为 key。
    #[allow(dead_code)]
    pub fn session_id(&self) -> &str {
        &self.sid
    }

    /// 存档快照：main.rs 的 save_state 拿它写进 AcpSaved.entries。
    pub fn entries_for_save(&self) -> Vec<AcpEntry> {
        self.entries.clone()
    }

    /// 存档快照：写进 AcpSaved.resume_session_id，GUI 重开后「重新开始」
    /// 才有旧 session id 可用来尝试真续接。
    pub fn resume_session_id_for_save(&self) -> Option<SessionId> {
        self.acp_session_id.clone()
    }

    /// 停止当前 turn（session/cancel）。agent 会以 Cancelled 收尾，相位随 TurnEnded 回 Idle。
    fn cancel_turn(&mut self) {
        if let Some(h) = &self.handle {
            let _ = h.cmd_tx.try_send(AcpCommand::Cancel);
        }
    }

    pub fn cwd(&self) -> Option<String> {
        self.cwd.clone()
    }

    /// 启动命令（存档用：重开 GUI 后按它「重新开始」）。
    pub fn launch_cmd(&self) -> &str {
        &self.cmd
    }

    /// 末几条消息的纯文本（总览卡片迷你预览，对齐终端的 last_lines）。
    pub fn last_lines(&self, n: usize) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for e in self.entries.iter().rev() {
            if out.len() >= n {
                break;
            }
            let line = match e {
                AcpEntry::User(t) => format!("> {}", t.lines().next().unwrap_or_default()),
                AcpEntry::Assistant { text, thought: false } => {
                    text.lines().last().unwrap_or_default().to_string()
                }
                AcpEntry::Assistant { thought: true, .. } => continue,
                AcpEntry::ToolCall { title, .. } => format!("🔧 {title}"),
                AcpEntry::Divider(_) => continue,
            };
            if !line.trim().is_empty() {
                out.push(line);
            }
        }
        out.reverse();
        out
    }

    pub fn completed_unread(&self) -> bool {
        self.completed_unread
    }

    /// 会话被激活查看后清「有结果可看」。
    pub fn mark_read(&mut self) {
        self.completed_unread = false;
    }

    pub fn is_awaiting_approval(&self) -> bool {
        matches!(self.phase, AcpPhase::AwaitingApproval)
    }

    pub fn is_running(&self) -> bool {
        matches!(self.phase, AcpPhase::Running)
    }

    /// 出了选择题等用户点（四档色里归「需要处理」橙档）。
    pub fn is_awaiting_choice(&self) -> bool {
        matches!(self.phase, AcpPhase::AwaitingChoice)
    }

    pub fn focus_input(&self, window: &mut Window, cx: &mut App) {
        if let Some(input) = &self.input {
            input.update(cx, |s, cx| s.focus(window, cx));
        }
    }

    /// 总览快捷回复直达（对齐终端会话的 send_key_to_session 语义）。
    pub fn send_prompt(&mut self, text: String, cx: &mut Context<Self>) {
        if text.trim().is_empty() {
            return;
        }
        if let Some(h) = &self.handle {
            if h.cmd_tx.try_send(AcpCommand::Prompt(text.clone())).is_ok() {
                self.entries.push(AcpEntry::User(text));
                self.awaiting_user_echo = true;
                self.phase = AcpPhase::Running;
                self.completed_unread = false;
                self.sync_daemon_state(cx);
                cx.notify();
            }
        }
    }

    /// 关闭会话：让连接收摊（drop 子进程），并从全局状态表摘掉自己。
    pub fn shutdown(&mut self, cx: &mut App) {
        if let Some(h) = self.handle.take() {
            let _ = h.cmd_tx.try_send(AcpCommand::Shutdown);
        }
        if let Some(states) = cx.try_global::<crate::DaemonStates>() {
            states.0.lock().unwrap().remove(&self.sid);
        }
    }

    fn submit_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(input) = self.input.clone() else { return };
        let text = input.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        input.update(cx, |s, cx| s.set_value("", window, cx));
        self.send_prompt(text, cx);
    }

    /// 事件应用：entries 合并 + phase 机 + 全局状态同步。
    fn apply_event(&mut self, ev: AcpEvent, cx: &mut Context<Self>) {
        // 持久化要排除的高频事件（match 会消费 ev，得在此之前先记下来）。
        let is_agent_chunk = matches!(ev, AcpEvent::AgentChunk { .. });
        // 除 UserChunk/Status 外的任何事件都代表「已经过了等自己那条 prompt
        // 回声的窗口」——agent 给出实质响应或者 turn 收尾，这轮回声不会再来了。
        if !matches!(ev, AcpEvent::UserChunk(_) | AcpEvent::Status(_)) {
            self.awaiting_user_echo = false;
        }
        match ev {
            AcpEvent::Status(msg) => {
                self.status_line = Some(msg);
            }
            AcpEvent::UserChunk(text) => {
                if self.awaiting_user_echo {
                    // 自己刚发那条的回声——本地已经在 send_prompt 时显示过了。
                } else {
                    // 没有 pending 的自己发的消息，只可能是 session/load 重放
                    // 的历史用户提问——正常追加显示（连续 chunk 合并）。
                    match self.entries.last_mut() {
                        Some(AcpEntry::User(t)) => t.push_str(&text),
                        _ => self.entries.push(AcpEntry::User(text)),
                    }
                }
            }
            AcpEvent::Ready { session_id, resumed } => {
                self.acp_session_id = Some(session_id);
                if resumed {
                    // 真续接：agent 马上会把完整历史重放一遍（session/update
                    // 通知，走正常的 AgentChunk/ToolCall 分支），本地快照先清空
                    // 避免重复；重放内容比本地快照更权威。
                    self.entries.clear();
                } else if !self.entries.is_empty() {
                    // 这其实是一次全新会话（没有旧 id / adapter 不支持 load /
                    // load 失败），但本地还留着之前的历史——插分割线做标记，
                    // 不能让人以为对话凭空断了。
                    self.entries.push(AcpEntry::Divider(format!(
                        "新会话 · {}",
                        chrono::Local::now().format("%m-%d %H:%M")
                    )));
                }
                self.phase = AcpPhase::Idle;
                self.status_line = None;
            }
            AcpEvent::AgentChunk { thought, text } => {
                // 连续同类 chunk 并入最后一条，流式追加不炸条目数。
                match self.entries.last_mut() {
                    Some(AcpEntry::Assistant { text: t, thought: th }) if *th == thought => {
                        t.push_str(&text);
                    }
                    _ => self.entries.push(AcpEntry::Assistant { text, thought }),
                }
                self.phase = AcpPhase::Running;
            }
            AcpEvent::ToolCall(tc) => {
                self.entries.push(AcpEntry::ToolCall {
                    id: tc.tool_call_id,
                    title: tc.title,
                    kind: tc.kind,
                    status: tc.status,
                    output: tool_content_text(&tc.content),
                });
                self.phase = AcpPhase::Running;
            }
            AcpEvent::ToolCallUpdate(u) => {
                if let Some(AcpEntry::ToolCall { title, kind, status, output, .. }) =
                    self.entries.iter_mut().rev().find(|e| {
                        matches!(e, AcpEntry::ToolCall { id, .. } if *id == u.tool_call_id)
                    })
                {
                    if let Some(t) = u.fields.title {
                        *title = t;
                    }
                    if let Some(k) = u.fields.kind {
                        *kind = k;
                    }
                    if let Some(s) = u.fields.status {
                        *status = s;
                    }
                    if let Some(c) = u.fields.content {
                        *output = tool_content_text(&c);
                    }
                }
            }
            AcpEvent::Permission { question, pub_options, responder } => {
                self.permission = Some(PermissionCard {
                    question: question.clone(),
                    options: pub_options,
                    responder: Some(responder),
                });
                self.phase = AcpPhase::AwaitingApproval;
                // 应用内通知（镜像 main.rs 状态转发循环的推送；设置可关）。
                let notify_on = cx
                    .try_global::<crate::settings::AgentUiConfig>()
                    .map(|c| c.notify_awaiting)
                    .unwrap_or(true);
                if notify_on {
                    if let Some(p) = cx.try_global::<crate::PendingAgentNotifs>() {
                        p.0.lock().unwrap().push(("等你批准".to_string(), question, true));
                    }
                }
            }
            AcpEvent::Elicitation { message, fields, responder } => {
                self.elicitation = Some(ElicitCard {
                    message: message.clone(),
                    fields,
                    chosen: Default::default(),
                    responder: Some(responder),
                });
                self.phase = AcpPhase::AwaitingChoice;
                let notify_on = cx
                    .try_global::<crate::settings::AgentUiConfig>()
                    .map(|c| c.notify_awaiting)
                    .unwrap_or(true);
                if notify_on {
                    if let Some(p) = cx.try_global::<crate::PendingAgentNotifs>() {
                        p.0.lock().unwrap().push(("等你选择".to_string(), message, false));
                    }
                }
            }
            AcpEvent::TurnEnded(reason) => {
                // turn 收尾时卡片一并作废（responder Drop 兜底回 Cancelled/Cancel）
                self.permission = None;
                self.elicitation = None;
                self.phase = AcpPhase::Idle;
                self.completed_unread = true;
                let _ = reason;
            }
            AcpEvent::Fatal(msg) => {
                self.permission = None;
                self.elicitation = None;
                self.phase = AcpPhase::Ended(msg);
                self.handle = None;
            }
        }
        self.sync_daemon_state(cx);
        self.scroll.scroll_to_bottom(); // 消息流跟随最新内容
        // 持久化触发：排除 AgentChunk——那是逐块流式增量，触发太密会把每次落盘
        // 变成写盘风暴；完整文本在 TurnEnded 时已经在 self.entries 里了，那时存
        // 一次就够。真被强制退出打断在流式中间，最多丢当前这一轮还没打完的字，
        // 之前所有已完成的对话都在上一次 TurnEnded 落盘时保住了。
        if !is_agent_chunk {
            cx.emit(AcpViewEvent::Changed);
        }
        cx.notify();
    }

    /// 把当前相位写进 DaemonStates 全局（key = `acp-` 前缀 sid）——Session::status、
    /// Dock 角标、菜单栏、总览全部经既有链路自动点亮。
    fn sync_daemon_state(&self, cx: &mut App) {
        let Some(states) = cx.try_global::<crate::DaemonStates>() else { return };
        let phase = match &self.phase {
            AcpPhase::Starting | AcpPhase::Idle => DaemonPhase::Idle,
            AcpPhase::Running => {
                // 有进行中的工具调用报「执行工具」，否则「思考中」。
                let executing = self.entries.iter().any(|e| {
                    matches!(
                        e,
                        AcpEntry::ToolCall { status: ToolCallStatus::InProgress | ToolCallStatus::Pending, .. }
                    )
                });
                if executing {
                    DaemonPhase::ExecutingTool
                } else {
                    DaemonPhase::Thinking
                }
            }
            AcpPhase::AwaitingApproval => DaemonPhase::AwaitingApproval,
            AcpPhase::AwaitingChoice => DaemonPhase::WaitingForUser,
            AcpPhase::Ended(_) => DaemonPhase::Dead,
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        states.0.lock().unwrap().insert(
            self.sid.clone(),
            DaemonSessionState {
                id: self.sid.clone(),
                phase,
                pending_question: self
                    .permission
                    .as_ref()
                    .map(|p| p.question.clone())
                    .or_else(|| self.elicitation.as_ref().map(|e| e.message.clone())),
                title: None,
                launch: Some("claude".to_string()),
                cwd: self.cwd.clone(),
                phase_since: now,
            },
        );
    }

    /// 选择题点选：单选字段直接置换；多选字段 toggle。单字段单选点了立即提交。
    fn pick_elicit_option(&mut self, field_ix: usize, opt_ix: usize, cx: &mut Context<Self>) {
        let Some(card) = &mut self.elicitation else { return };
        let Some(field) = card.fields.get(field_ix) else { return };
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
        // 快捷路径：整卡只有一个单选字段，点了就是答案，不用再按提交。
        let single_select = card.fields.len() == 1
            && matches!(card.fields[0].kind, ElicitFieldKind::Select(_));
        if single_select {
            self.submit_elicitation(cx);
        } else {
            cx.notify();
        }
    }

    /// 每个字段都有选择后才可提交（渲染侧按这个亮按钮）。
    fn elicit_ready(&self) -> bool {
        self.elicitation.as_ref().is_some_and(|card| {
            card.fields.iter().enumerate().all(|(ix, _)| {
                card.chosen.get(&ix).is_some_and(|sel| !sel.is_empty())
            })
        })
    }

    fn submit_elicitation(&mut self, cx: &mut Context<Self>) {
        use agent_client_protocol::schema::v1::ElicitationContentValue as V;
        let Some(mut card) = self.elicitation.take() else { return };
        let Some(responder) = card.responder.take() else { return };
        let mut content = std::collections::BTreeMap::new();
        for (ix, field) in card.fields.iter().enumerate() {
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
                            V::String(s) => Some(s.clone()),
                            _ => None,
                        })
                        .collect();
                    content.insert(field.key.clone(), V::StringArray(values));
                }
            }
        }
        responder.accept(content);
        self.phase = AcpPhase::Running;
        self.sync_daemon_state(cx);
        cx.notify();
    }

    /// 「跳过」：丢弃卡片（responder Drop 自动回 Cancel），继续文本对话。
    fn dismiss_elicitation(&mut self, cx: &mut Context<Self>) {
        self.elicitation = None;
        self.phase = AcpPhase::Running;
        self.sync_daemon_state(cx);
        cx.notify();
    }

    /// 审批按钮：消费 responder 回 RPC，卡片收起，相位回 Running（agent 会继续）。
    fn pick_permission(&mut self, option_ix: usize, cx: &mut Context<Self>) {
        let Some(card) = &mut self.permission else { return };
        let Some(opt) = card.options.get(option_ix) else { return };
        let option_id = opt.option_id.clone();
        if let Some(responder) = card.responder.take() {
            responder.select(option_id);
        }
        self.permission = None;
        self.phase = AcpPhase::Running;
        self.sync_daemon_state(cx);
        cx.notify();
    }
}

impl Focusable for AcpView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for AcpView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        let muted = t.muted_foreground;

        // 相位横幅：启动中 / 已结束（含失败原因）时显示；正常运行不占空间。
        let banner: Option<gpui::AnyElement> = match &self.phase {
            AcpPhase::Starting => Some(
                div()
                    .p_2()
                    .text_sm()
                    .text_color(muted)
                    .child(
                        self.status_line
                            .clone()
                            .unwrap_or_else(|| "正在启动 agent…（首次需下载适配器，可能要一会儿）".to_string()),
                    )
                    .into_any_element(),
            ),
            AcpPhase::Ended(msg) => Some(
                v_flex()
                    .p_2()
                    .gap_2()
                    .text_sm()
                    .child(div().text_color(t.danger).child("会话已结束"))
                    .when(!msg.is_empty(), |d| {
                        d.child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .font_family("monospace")
                                .child(msg.clone()),
                        )
                    })
                    .child(
                        div()
                            .id("acp-restart")
                            .w(px(120.))
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .border_1()
                            .border_color(t.border)
                            .text_sm()
                            .text_center()
                            .cursor_pointer()
                            .hover(|d| d.opacity(0.8))
                            .child("重新开始")
                            .on_click(cx.listener(|this, _ev, window, cx| {
                                this.restart(window, cx);
                            })),
                    )
                    .into_any_element(),
            ),
            _ => None,
        };

        // 消息流。
        let mut list = v_flex()
            .id("acp-entries")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .p_3()
            .gap_2();
        for (i, entry) in self.entries.iter().enumerate() {
            let el: gpui::AnyElement = match entry {
                AcpEntry::User(text) => h_flex()
                    .justify_end()
                    .child(
                        div()
                            .max_w_4_5()
                            .px_3()
                            .py_2()
                            .rounded_md()
                            .bg(t.muted)
                            .text_sm()
                            .child(crate::markdown_mermaid::markdown_view(("acp-user-md", i), text.clone())),
                    )
                    .into_any_element(),
                AcpEntry::Assistant { text, thought } => div()
                    .max_w_full()
                    .text_sm()
                    .when(*thought, |d| d.text_color(muted).italic())
                    .child(crate::markdown_mermaid::markdown_view(("acp-md", i), text.clone()))
                    .into_any_element(),
                AcpEntry::ToolCall { title, kind, status, output, .. } => {
                    let (dot, label) = match status {
                        ToolCallStatus::Pending => (t.muted_foreground, "待执行"),
                        ToolCallStatus::InProgress => (gpui::rgb(0x4a9eff).into(), "执行中"),
                        ToolCallStatus::Completed => (gpui::rgb(0x22c55e).into(), "完成"),
                        ToolCallStatus::Failed => (gpui::rgb(0xef4444).into(), "失败"),
                        _ => (t.muted_foreground, "…"), // schema #[non_exhaustive]
                    };
                    v_flex()
                        .p_2()
                        .gap_1()
                        .rounded_md()
                        .border_1()
                        .border_color(t.border)
                        .child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(Icon::new(tool_icon(kind)).size(px(13.)).text_color(muted))
                                .child(div().text_sm().flex_1().truncate().child(title.clone()))
                                .child(div().size_2().rounded_full().bg(dot))
                                .child(div().text_xs().text_color(muted).child(label)),
                        )
                        .when(!output.trim().is_empty(), |d| {
                            d.child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .font_family("monospace")
                                    .max_h(px(160.))
                                    .overflow_hidden()
                                    .child(output.clone()),
                            )
                        })
                        .into_any_element()
                }
                AcpEntry::Divider(label) => h_flex()
                    .items_center()
                    .gap_2()
                    .my_1()
                    .child(div().flex_1().h(px(1.)).bg(t.border))
                    .child(div().text_xs().text_color(muted).child(label.clone()))
                    .child(div().flex_1().h(px(1.)).bg(t.border))
                    .into_any_element(),
            };
            list = list.child(el);
        }

        // 权限审批卡片：原生按钮，点了直接回 RPC——这是 ACP 通道的核心卖点。
        let permission = self.permission.as_ref().map(|card| {
            let mut buttons = h_flex().gap_2().flex_wrap();
            for (ix, opt) in card.options.iter().enumerate() {
                let primary = matches!(
                    opt.kind,
                    PermissionOptionKind::AllowOnce | PermissionOptionKind::AllowAlways
                );
                let danger = matches!(
                    opt.kind,
                    PermissionOptionKind::RejectOnce | PermissionOptionKind::RejectAlways
                );
                buttons = buttons.child(
                    div()
                        .id(("acp-perm-opt", ix))
                        .px_3()
                        .py_1()
                        .rounded_md()
                        .border_1()
                        .border_color(t.border)
                        .text_sm()
                        .cursor_pointer()
                        .when(primary, |d| d.bg(gpui::rgb(0x22c55e)).text_color(gpui::white()))
                        .when(danger, |d| d.text_color(gpui::rgb(0xef4444)))
                        .hover(|d| d.opacity(0.85))
                        .child(opt.name.clone())
                        .on_click(cx.listener(move |this, _ev, _window, cx| {
                            this.pick_permission(ix, cx);
                        })),
                );
            }
            v_flex()
                .mx_3()
                .mb_2()
                .p_3()
                .gap_2()
                .rounded_md()
                .border_1()
                .border_color(gpui::rgb(0xef4444))
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(div().text_sm().font_semibold().child("等你批准"))
                        .child(div().text_sm().text_color(muted).child(card.question.clone())),
                )
                .child(buttons)
        });

        // 选择题卡片：message + 逐字段按钮组；单字段单选点击即提交，
        // 其余选齐后亮「提交」；「跳过」丢卡（responder Drop 回 Cancel）。
        let elicitation = self.elicitation.as_ref().map(|card| {
            let ready = self.elicit_ready();
            let mut body = v_flex()
                .mx_3()
                .mb_2()
                .p_3()
                .gap_2()
                .rounded_md()
                .border_1()
                .border_color(gpui::rgb(0xf59e0b))
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(div().text_sm().font_semibold().child("等你选择"))
                        .child(div().text_sm().text_color(muted).child(card.message.clone())),
                );
            let multi_field = card.fields.len() > 1
                || card
                    .fields
                    .first()
                    .is_some_and(|f| matches!(f.kind, ElicitFieldKind::MultiSelect(_)));
            for (fix, field) in card.fields.iter().enumerate() {
                let (options, is_multi) = match &field.kind {
                    ElicitFieldKind::Select(o) => (o, false),
                    ElicitFieldKind::MultiSelect(o) => (o, true),
                };
                let chosen = card.chosen.get(&fix).cloned().unwrap_or_default();
                let mut row = h_flex().gap_2().flex_wrap();
                for (oix, opt) in options.iter().enumerate() {
                    let selected = chosen.contains(&oix);
                    row = row.child(
                        div()
                            .id(("acp-elicit-opt", fix * 1000 + oix))
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .border_1()
                            .border_color(if selected { gpui::rgb(0xf59e0b).into() } else { t.border })
                            .when(selected, |d| d.bg(t.muted))
                            .text_sm()
                            .cursor_pointer()
                            .hover(|d| d.opacity(0.85))
                            .child(opt.label.clone())
                            .on_click(cx.listener(move |this, _ev, _window, cx| {
                                this.pick_elicit_option(fix, oix, cx);
                            })),
                    );
                }
                body = body.child(
                    v_flex()
                        .gap_1()
                        .when(multi_field, |d| {
                            d.child(
                                div().text_xs().text_color(muted).child(format!(
                                    "{}{}",
                                    field.title.clone(),
                                    if is_multi { "（可多选）" } else { "" }
                                )),
                            )
                        })
                        .child(row),
                );
            }
            if multi_field {
                body = body.child(
                    h_flex()
                        .gap_2()
                        .child(
                            div()
                                .id("acp-elicit-submit")
                                .px_3()
                                .py_1()
                                .rounded_md()
                                .text_sm()
                                .when(ready, |d| {
                                    d.bg(gpui::rgb(0x22c55e))
                                        .text_color(gpui::white())
                                        .cursor_pointer()
                                        .hover(|x| x.opacity(0.85))
                                })
                                .when(!ready, |d| {
                                    d.border_1().border_color(t.border).text_color(muted)
                                })
                                .child("提交")
                                .on_click(cx.listener(|this, _ev, _window, cx| {
                                    if this.elicit_ready() {
                                        this.submit_elicitation(cx);
                                    }
                                })),
                        )
                        .child(
                            div()
                                .id("acp-elicit-skip")
                                .px_3()
                                .py_1()
                                .rounded_md()
                                .text_sm()
                                .text_color(muted)
                                .cursor_pointer()
                                .hover(|d| d.opacity(0.8))
                                .child("跳过（改用文字回答）")
                                .on_click(cx.listener(|this, _ev, _window, cx| {
                                    this.dismiss_elicitation(cx);
                                })),
                        ),
                );
            }
            body
        });

        let input_row = self.input.as_ref().map(|input| {
            h_flex()
            .p_2()
            .gap_2()
            .items_end()
            .border_t_1()
            .border_color(t.border)
            .child(div().flex_1().child(Input::new(input)))
            .when(matches!(self.phase, AcpPhase::Running), |row| {
                row.child(
                    div()
                        .id("acp-stop")
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .border_1()
                        .border_color(t.border)
                        .text_xs()
                        .text_color(muted)
                        .cursor_pointer()
                        .hover(|d| d.opacity(0.8))
                        .child("停止")
                        .on_click(cx.listener(|this, _ev, _window, _cx| this.cancel_turn())),
                )
            })
        });

        v_flex()
            .size_full()
            .track_focus(&self.focus_handle)
            .bg(t.background)
            .children(banner)
            .child(list)
            .children(permission)
            .children(elicitation)
            .children(input_row)
    }
}

/// ToolKind → 图标（与终端侧 LaunchKind 图标风格对齐）。
fn tool_icon(kind: &ToolKind) -> IconName {
    match kind {
        ToolKind::Read | ToolKind::Search | ToolKind::Fetch => IconName::Search,
        ToolKind::Edit | ToolKind::Delete | ToolKind::Move => IconName::File,
        ToolKind::Execute => IconName::SquareTerminal,
        _ => IconName::Bot,
    }
}

/// tool call content 文本化（复用 acp.rs 的 ContentBlock 规则）。
fn tool_content_text(content: &[agent_client_protocol::schema::v1::ToolCallContent]) -> String {
    use agent_client_protocol::schema::v1::ToolCallContent;
    content
        .iter()
        .filter_map(|c| match c {
            ToolCallContent::Content(inner) => Some(crate::acp::content_text(&inner.content)),
            ToolCallContent::Diff(d) => Some(format!("diff: {}", d.path.display())),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}
