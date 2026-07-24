//! ACP 会话的消息流视图：第二种会话类型的 GPUI 皮肤。
//!
//! 与 acp.rs 的分工：那边是连接层（不许引 gpui），这边持 `AcpHandle` 消费事件、
//! 渲染消息流 + 权限审批卡片 + 输入框，并把 phase 翻译进 `DaemonStates` 全局——
//! 四档着色 / Dock 角标 / 应用内通知全部复用终端会话的既有链路，零新增。

use gpui::prelude::FluentBuilder;
use gpui::{
    App, AppContext, Context, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement,
    IntoElement, ParentElement, Render, StatefulInteractiveElement, Styled, Window, div, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use gpui_component::spinner::Spinner;
use gpui_component::{ActiveTheme, Sizable, StyledExt, h_flex, v_flex};

use agent_client_protocol::schema::v1::{
    PermissionOption, PermissionOptionKind, Plan, PlanEntryStatus, SessionId, ToolCallId,
};

use crate::acp::{
    AcpCommand, AcpEvent, AcpHandle, AcpLaunch, ElicitField, ElicitFieldKind, ElicitationResponder,
    ModelState, PermissionResponder, ReadyKind, spawn_acp,
};
use crate::settings::AcpAgentKind;
use crate::terminal::{DaemonPhase, DaemonSessionState};
use crate::ui_theme;

/// 消息流数据模型（AcpEntry/ToolOutputPart/ToolKind/ToolCallStatus）与 diff/
/// markdown 围栏这批纯逻辑现在都活在 `smelt_core::acp_chat`——不依赖 GPUI 也不
/// 依赖 agent_client_protocol，未来 web/mobile 端渲染同一份对话时不用重新实现
/// 一遍「怎么把协议事件变成可展示内容」。这里整段 re-export，文件里大量既有的
/// 裸 `AcpEntry::...` 用法不用逐处改路径。
pub(crate) use smelt_core::acp_chat::{
    AcpEntry, DiffLineTag, ToolCallStatus, ToolKind, ToolOutputPart, diff_line_stats, diff_lines,
    is_interrupt_marker, strip_code_fence,
};

/// agent_client_protocol 的协议类型 → 共享模型类型。就近放这里：那边的 crate
/// 依赖只此一份（acp.rs / acp_view.rs），smelt_core 不许引它。
fn tool_kind_from_acp(k: agent_client_protocol::schema::v1::ToolKind) -> ToolKind {
    use agent_client_protocol::schema::v1::ToolKind as Acp;
    match k {
        Acp::Read => ToolKind::Read,
        Acp::Edit => ToolKind::Edit,
        Acp::Delete => ToolKind::Delete,
        Acp::Move => ToolKind::Move,
        Acp::Search => ToolKind::Search,
        Acp::Execute => ToolKind::Execute,
        Acp::Think => ToolKind::Think,
        Acp::Fetch => ToolKind::Fetch,
        Acp::SwitchMode => ToolKind::SwitchMode,
        _ => ToolKind::Other, // #[non_exhaustive]：协议以后加的新分类先归到这
    }
}

fn tool_status_from_acp(s: agent_client_protocol::schema::v1::ToolCallStatus) -> ToolCallStatus {
    use agent_client_protocol::schema::v1::ToolCallStatus as Acp;
    match s {
        Acp::Pending => ToolCallStatus::Pending,
        Acp::InProgress => ToolCallStatus::InProgress,
        Acp::Completed => ToolCallStatus::Completed,
        Acp::Failed => ToolCallStatus::Failed,
        _ => ToolCallStatus::Pending, // #[non_exhaustive]：协议以后加的新状态先当待定
    }
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

/// `@` / `/` 补全弹层的状态。回合态，不落盘。
struct CompletionPopup {
    /// 触发 token 在输入框文本里的字节范围（含 `@`/`/`），接受候选时按它替换。
    start: usize,
    end: usize,
    items: Vec<crate::acp_completion::Candidate>,
    selected: usize,
}

/// 待审批卡片：responder 是 Option 因为 select 消费所有权（从 &mut self take）。
struct PermissionCard {
    question: String,
    /// 关联的工具调用：消息流里有对应卡片时按钮内嵌进卡片底部，没有则独立渲染。
    tool_call_id: ToolCallId,
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
    /// 这条会话接的是哪个 agent（Claude / Copilot / Codex）：决定显示名，也决定
    /// 「重新开始」时该去全局配置的哪一条命令上取最新值。
    agent: AcpAgentKind,
    /// 已粘进来、等着随下一条 prompt 发出去的图片（缩略图条显示，发完清空）。
    /// 只在内存里待到发送为止：图片体积大，不进 workspace.json。
    pending_images: Vec<std::sync::Arc<gpui::Image>>,
    /// 本会话的 agent 是否收图（握手 Ready 带来）。握手前默认 true——那时还没
    /// 粘图的机会，先假设支持，Ready 到了再按实际能力修正（Grok = false）。
    supports_image: bool,
    /// 「这个 agent 不收图」的一次性提示：粘图被拦时置上，输入框上方显示一行，
    /// 用户下次一打字（Change）就清掉，不占定时器。
    paste_hint: Option<String>,
    /// `@` / `/` 补全弹层的当前状态；None = 没在补全。
    completion: Option<CompletionPopup>,
    /// cwd 下的文件清单缓存（`@` 的候选源）。每敲一个字符跑一次 git ls-files
    /// 会明显卡手，所以一次会话只列一次。
    file_cache: Option<std::rc::Rc<Vec<String>>>,
    /// agent 侧真实的 session id：握手成功后写入，`restart()` 拿它去尝试
    /// `session/load` 真续接；也存盘（main.rs AcpSaved），GUI 重开后同样能续。
    acp_session_id: Option<SessionId>,
    /// 「等自己刚发那条 prompt 的回声」——不确定 adapter 是否会在 live 对话时也
    /// 回显 UserMessageChunk（已确认的是 session/load 重放一定会发）；两种可能
    /// 都要处理对：send_prompt 已经手动 push 过一次，这段窗口内若真收到回声就
    /// 吞掉不重复；等到 agent 给出实质响应/turn 收尾就清掉，之后的 UserChunk
    /// （只可能来自 replay）正常追加显示。
    awaiting_user_echo: bool,
    /// 会话当前可用的斜杠命令 (名字, 说明)；空 = agent 没发过这个更新。
    /// 胶囊点开列出来、点一条填进输入框——只显示数量没有任何用处。
    available_commands: Vec<(String, String)>,
    /// 上下文用量：(已用 token, 窗口大小)。None = agent 没上报过，不显示。
    usage: Option<(u64, u64)>,
    /// 本次启动/续接的起点，用来在横幅上报「已等了几秒」。
    /// 实测 `session/new` 里 Claude Code 自身要约 10 秒（跟下载无关，同一适配器
    /// 进程建第二个会话一样慢），没有进度反馈会让人以为卡死了。
    starting_since: Option<std::time::Instant>,
    /// agent 最近一次上报的任务计划（每次全量覆盖）。回合态：不落盘，
    /// TurnEnded 保留最后一份供回看，「重新开始」清空。
    plan: Option<Plan>,
    /// PLAN 条折叠态（默认展开，跟设计稿一致）。
    plan_collapsed: bool,
    /// 模型状态：当前名 + 可切换的候选（协议给什么显示什么）；None = agent
    /// 没上报过，UI 就不显示模型胶囊，不拿适配器包名冒充。
    model: Option<ModelState>,
    /// 手动展开了完整输出的工具调用（key = tool_call_id）。长输出默认折叠成
    /// 前几行 + 「展开」，回合态不落盘。
    expanded_tools: std::collections::HashSet<String>,
    /// 冷恢复占位待自动续接：GUI 重启后第一次切到这个会话时自动 restart
    /// （协议级 session/load 续接），免去手点「重新开始」。只消费一次——
    /// 自动续接失败（Fatal → Ended）后回到手动，错误得让人看见，不能循环重试。
    auto_resume_pending: bool,
    scroll: gpui::ScrollHandle,
    focus_handle: FocusHandle,
    _input_sub: Option<gpui::Subscription>,
}

impl AcpView {
    /// 建视图并立即起连接线程（spawn_acp 非阻塞，握手结果以事件回来）。
    pub fn start(
        window: &mut Window,
        cx: &mut Context<Self>,
        agent: AcpAgentKind,
        cmd: String,
        cwd: Option<String>,
    ) -> Self {
        let mut this = Self::placeholder(cx, agent, cmd, cwd, String::new(), Vec::new(), None);
        this.phase = AcpPhase::Starting;
        this.starting_since = Some(std::time::Instant::now());
        this.init_input(window, cx);
        let handle = spawn_acp(AcpLaunch {
            cmd: this.cmd.clone(),
            cwd: this.cwd.clone(),
            sid: this.sid.clone(),
            resume_session_id: None, // 第一次开，没有旧会话可续
            resume_needs_transcript_check: matches!(agent, AcpAgentKind::Claude),
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
        agent: AcpAgentKind,
        cmd: String,
        cwd: Option<String>,
        reason: String,
        entries: Vec<AcpEntry>,
        resume_session_id: Option<SessionId>,
    ) -> Self {
        // 有旧 session id 的冷恢复占位才值得自动续接（没有 id 只能开全新会话，
        // 丢 agent 侧上下文，留给用户手动决定）。先算，下面 struct 初始化会 move。
        let auto_resume_pending = resume_session_id.is_some();
        Self {
            auto_resume_pending,
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
            agent,
            pending_images: Vec::new(),
            supports_image: true,
            paste_hint: None,
            completion: None,
            file_cache: None,
            acp_session_id: resume_session_id,
            awaiting_user_echo: false,
            available_commands: Vec::new(),
            usage: None,
            starting_since: None,
            plan: None,
            plan_collapsed: false,
            model: None,
            expanded_tools: std::collections::HashSet::new(),
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
        // 用当前全局配置刷新命令，不死守创建时固化的旧值——「重新开始」的直觉
        // 语义是「用现在认为对的命令再来一次」，不是「原样复刻可能早就被发现
        // 有 bug、用户已经在设置页改掉的旧配置」。真实教训：默认命令曾经指向
        // 一个有 zod 依赖 bug 的适配器版本，改了默认值后，已存在的旧会话点
        // 「重新开始」却因为这里死守 self.cmd 而继续用旧版本，看起来像「修复
        // 没生效」。
        // 取的是**本会话这个 agent** 那一条配置：多 agent 之后死拿 acp_cmd 会让
        // Copilot / Codex 会话一点「重新开始」就变成 Claude 会话。
        if let Some(cfg) = cx.try_global::<crate::settings::AgentUiConfig>() {
            self.cmd = cfg.acp_cmd_for(self.agent);
        }
        self.permission = None;
        self.elicitation = None;
        self.plan = None; // 计划是回合态，新会话不该带着上一段的进度条
        self.model = None; // 模型等新会话握手后重新上报
        self.usage = None; // 上下文用量属于旧会话，别带到新的上
        self.completed_unread = false;
        self.phase = AcpPhase::Starting;
        self.starting_since = Some(std::time::Instant::now());
        self.init_input(window, cx);
        let handle = spawn_acp(AcpLaunch {
            cmd: self.cmd.clone(),
            cwd: self.cwd.clone(),
            sid: self.sid.clone(),
            resume_session_id: self.acp_session_id.clone(),
            resume_needs_transcript_check: matches!(self.agent, AcpAgentKind::Claude),
        });
        self.attach_handle(handle, cx);
        cx.notify();
    }

    /// 舞台头状态胶囊用的相位文案 + 颜色。
    ///
    /// ACP 有自己的相位机，不能经 DaemonStates 那套五态绕一圈拿——`Starting`
    /// 和 `Ended` 在映射里都会塌成「空闲」，于是「正在启动」的横幅底下顶着一个
    /// 「空闲」胶囊，自相矛盾。
    pub(crate) fn phase_label(&self) -> (&'static str, u32) {
        match &self.phase {
            AcpPhase::Starting => ("启动中", ui_theme::blue()),
            AcpPhase::Idle => ("空闲", ui_theme::text_faint()),
            AcpPhase::Running => ("运行中", ui_theme::blue()),
            AcpPhase::AwaitingApproval => ("等你批准", ui_theme::yellow()),
            AcpPhase::AwaitingChoice => ("等你选择", ui_theme::yellow()),
            AcpPhase::Ended(_) => ("已结束", ui_theme::text_faint()),
        }
    }

    /// 切到本会话时的自动续接：冷恢复占位（Ended + 有旧 session id）第一次被
    /// 激活就 restart，像终端一样「点开就是活的」。只触发一次，见字段注释。
    pub(crate) fn maybe_auto_resume(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.auto_resume_pending {
            return;
        }
        self.auto_resume_pending = false;
        if matches!(self.phase, AcpPhase::Ended(_)) && self.handle.is_none() {
            self.restart(window, cx);
        }
    }

    fn init_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.input.is_some() {
            return;
        }
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("给 agent 的指令：@ 引文件，/ 用命令，Enter 发送，Shift+Enter 换行")
                .multi_line(true)
                .auto_grow(1, 8)
        });
        self._input_sub = Some(cx.subscribe_in(
            &input,
            window,
            |this: &mut Self, _input, ev: &InputEvent, window, cx| {
                match ev {
                    InputEvent::PressEnter { shift, .. } => {
                        if !shift {
                            this.submit_input(window, cx);
                        }
                    }
                    // 每次文本变化重算补全 token（打 `@`/`/` 就弹，打空格就收）。
                    InputEvent::Change => this.refresh_completion(cx),
                    InputEvent::Blur => this.completion = None,
                    _ => {}
                }
            },
        ));
        self.input = Some(input);
    }

    /// 启动期每秒重绘一次，让横幅上的「已 N 秒」真的在走。
    /// 相位离开 Starting 就自然停（不占常驻定时器）。
    fn tick_starting(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                smol::Timer::after(std::time::Duration::from_secs(1)).await;
                let keep = this
                    .update(cx, |v, cx| {
                        let starting = matches!(v.phase, AcpPhase::Starting);
                        if starting {
                            cx.notify();
                        }
                        starting
                    })
                    .unwrap_or(false);
                if !keep {
                    return;
                }
            }
        })
        .detach();
    }

    /// 挂上连接句柄并起事件 drain（start / restart 共用）。
    fn attach_handle(&mut self, handle: AcpHandle, cx: &mut Context<Self>) {
        let event_rx = handle.event_rx.clone();
        self.handle = Some(handle);
        self.tick_starting(cx);
        cx.spawn(async move |this, cx| {
            while let Ok(ev) = event_rx.recv().await {
                if this
                    .update(cx, |view, cx| view.apply_event(ev, cx))
                    .is_err()
                {
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

    /// 这条会话接的 agent 种类（存档 / 标题 / 舞台头胶囊用）。
    pub fn agent_kind(&self) -> AcpAgentKind {
        self.agent
    }

    /// cwd 下的文件清单（首次调用才真去列，之后走缓存）。
    fn file_list(&mut self) -> std::rc::Rc<Vec<String>> {
        if let Some(cached) = &self.file_cache {
            return cached.clone();
        }
        let list = self
            .cwd
            .as_deref()
            .map(crate::acp_completion::list_files)
            .unwrap_or_else(|| std::rc::Rc::new(Vec::new()));
        self.file_cache = Some(list.clone());
        list
    }

    /// 按输入框当前内容重算补全候选。
    fn refresh_completion(&mut self, cx: &mut Context<Self>) {
        // 一打字就把「不收图」提示撤了——它是针对上一次粘贴的，用户已经继续了。
        self.paste_hint = None;
        let Some(input) = self.input.clone() else {
            self.completion = None;
            return;
        };
        let (text, cursor) = {
            let s = input.read(cx);
            (s.value().to_string(), s.cursor())
        };
        // cursor 是字节偏移，可能落在多字节字符中间（中文输入过程中），
        // 切之前先确认是字符边界，否则 panic。
        let cursor = cursor.min(text.len());
        if !text.is_char_boundary(cursor) {
            return;
        }
        let Some(trigger) = crate::acp_completion::detect_trigger(&text[..cursor]) else {
            if self.completion.is_some() {
                self.completion = None;
                cx.notify();
            }
            return;
        };
        let files = match trigger.kind {
            crate::acp_completion::Kind::At => self.file_list(),
            crate::acp_completion::Kind::Slash => std::rc::Rc::new(Vec::new()),
        };
        let items = crate::acp_completion::candidates(&trigger, &files, &self.available_commands);
        self.completion = (!items.is_empty()).then(|| CompletionPopup {
            start: trigger.start,
            end: cursor,
            items,
            selected: 0,
        });
        cx.notify();
    }

    /// 上下移动补全选中项（返回 false = 当前没在补全，按键该交回输入框）。
    fn move_completion(&mut self, delta: i32, cx: &mut Context<Self>) -> bool {
        let Some(popup) = &mut self.completion else {
            return false;
        };
        let n = popup.items.len() as i32;
        popup.selected = (popup.selected as i32 + delta).rem_euclid(n) as usize;
        cx.notify();
        true
    }

    /// 把选中的候选替换进输入框（返回 false = 没在补全）。
    fn accept_completion(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(popup) = self.completion.take() else {
            return false;
        };
        let Some(input) = self.input.clone() else {
            return false;
        };
        let Some(item) = popup.items.get(popup.selected) else {
            return false;
        };
        let insert = item.insert.clone();
        input.update(cx, |s, cx| {
            let text = s.value().to_string();
            // 只换掉触发 token 那一段，光标后面的内容原样留着。
            if popup.start <= popup.end
                && popup.end <= text.len()
                && text.is_char_boundary(popup.start)
                && text.is_char_boundary(popup.end)
            {
                let merged = format!("{}{}{}", &text[..popup.start], insert, &text[popup.end..]);
                s.set_value(merged, window, cx);
            }
            s.focus(window, cx);
        });
        cx.notify();
        true
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
                AcpEntry::Assistant {
                    text,
                    thought: false,
                } => text.lines().last().unwrap_or_default().to_string(),
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

    /// 把一段文本塞进输入框并聚焦（SKILLS 面板点一条 skill 用）。
    /// 不自动发送——skill 后面常还要补一句话，发不发由人定。
    pub(crate) fn insert_prompt_text(
        &mut self,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(input) = self.input.clone() else {
            return;
        };
        input.update(cx, |s, cx| {
            let cur = s.value().to_string();
            let merged = if cur.trim().is_empty() {
                format!("{text} ")
            } else if cur.ends_with(' ') {
                format!("{cur}{text} ")
            } else {
                format!("{cur} {text} ")
            };
            s.set_value(merged, window, cx);
            s.focus(window, cx);
        });
        cx.notify();
    }

    /// 总览快捷回复直达（对齐终端会话的 send_key_to_session 语义）。
    /// 连带把已粘贴的待发图片一起发出去并清空。
    pub fn send_prompt(&mut self, text: String, cx: &mut Context<Self>) {
        // 光有图没有字也算一条有效 prompt（「这截图什么意思」式的用法）。
        if text.trim().is_empty() && self.pending_images.is_empty() {
            return;
        }
        let images: Vec<crate::acp::PromptImage> = self
            .pending_images
            .iter()
            .map(|im| crate::acp::PromptImage {
                mime: image_mime(im.format).to_string(),
                data_b64: base64_encode(&im.bytes),
            })
            .collect();
        let img_count = images.len();
        if let Some(h) = &self.handle {
            if h.cmd_tx
                .try_send(AcpCommand::Prompt {
                    text: text.clone(),
                    images,
                })
                .is_ok()
            {
                // 历史里只留「带了几张图」的标记：base64 动辄几 MB，进不得
                // workspace.json（那是每条消息都要落盘的存档）。
                let shown = match (text.is_empty(), img_count) {
                    (_, 0) => text,
                    (true, n) => format!("[{n} 张图片]"),
                    (false, n) => format!("{text}\n[{n} 张图片]"),
                };
                self.entries.push(AcpEntry::User(shown));
                self.pending_images.clear();
                self.awaiting_user_echo = true;
                self.phase = AcpPhase::Running;
                self.completed_unread = false;
                self.sync_daemon_state(cx);
                cx.notify();
            }
        }
    }

    /// 剪贴板里是图就收进待发列表（返回 true 表示这次粘贴被图片消费掉了，
    /// 调用方据此拦下事件，别再让输入框按文本粘一遍）。
    fn take_clipboard_image(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(item) = cx.read_from_clipboard() else {
            return false;
        };
        let has_image = item
            .entries()
            .iter()
            .any(|e| matches!(e, gpui::ClipboardEntry::Image(_)));
        if !has_image {
            return false;
        }
        // 能力门：agent 不收图就别收进来（Grok = false）。返回 true 照样吞掉这次
        // 粘贴——图片剪贴板里没有文本，放行给输入框也是白搭，只会漏个空。
        if !self.supports_image {
            self.paste_hint = Some(format!(
                "{} 不支持图片，已忽略粘贴",
                self.agent.short_label()
            ));
            cx.notify();
            return true;
        }
        for entry in item.into_entries() {
            if let gpui::ClipboardEntry::Image(image) = entry {
                self.pending_images.push(std::sync::Arc::new(image));
            }
        }
        self.paste_hint = None;
        cx.notify();
        true
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

    /// App 真退出前专用：发 Shutdown，但**不**在这里就地扔掉 handle——子进程
    /// 是不是真被杀掉，取决于连接线程有没有机会跑到收尾（内部 ChildGuard 在
    /// Drop 时才杀子进程，含整个进程组）。这里只发信号，`acp::wait_for_shutdown`
    /// 由调用方（main.rs 的 on_app_quit）异步等这份 handle 的 event 通道关闭，
    /// 确认线程真收尾了再放行退出——不然 Cmd+Q 直接杀掉整个 GUI 进程时，ACP
    /// 连接线程会被系统一起带走，根本没机会 Drop，子进程就变孤儿（真实教训：
    /// Copilot 这类 agent 的孤儿子进程还占着旧登录会话，下次「重新开始」新起
    /// 一个进程会撞上它，报出 Authentication required 这种看着不相关的错）。
    pub(crate) fn take_handle_for_quit(&mut self) -> Option<AcpHandle> {
        if let Some(h) = self.handle.as_ref() {
            let _ = h.cmd_tx.try_send(AcpCommand::Shutdown);
        }
        self.handle.take()
    }

    fn submit_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(input) = self.input.clone() else {
            return;
        };
        let text = input.read(cx).value().trim().to_string();
        // 只贴了图没打字也要能发。
        if text.is_empty() && self.pending_images.is_empty() {
            return;
        }
        input.update(cx, |s, cx| s.set_value("", window, cx));
        self.send_prompt(text, cx);
    }

    /// 事件应用：entries 合并 + phase 机 + 全局状态同步。
    fn apply_event(&mut self, ev: AcpEvent, cx: &mut Context<Self>) {
        // 持久化要排除的事件（match 会消费 ev，得在此之前先记下来）：
        // AgentChunk 是逐块流式增量，触发太密；Plan 本来就不落盘，触发没意义。
        let skip_persist = matches!(
            ev,
            AcpEvent::AgentChunk { .. }
                | AcpEvent::Plan(_)
                | AcpEvent::Model(_)
                | AcpEvent::Usage { .. }
        );
        // 除 UserChunk/Status/AvailableCommands 外的任何事件都代表「已经过了
        // 等自己那条 prompt 回声的窗口」——后两者是跟对话轮次推进无关的元数据
        // 更新，agent 给出实质响应或者 turn 收尾才算真的翻篇。
        if !matches!(
            ev,
            AcpEvent::UserChunk(_)
                | AcpEvent::Status(_)
                | AcpEvent::AvailableCommands(_)
                | AcpEvent::Usage { .. }
        ) {
            self.awaiting_user_echo = false;
        }
        match ev {
            AcpEvent::AvailableCommands(list) => {
                self.available_commands = list;
            }
            AcpEvent::Usage { used, size, .. } => {
                self.usage = (size > 0).then_some((used, size));
            }
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
            AcpEvent::Ready {
                session_id,
                kind,
                supports_image,
            } => {
                self.acp_session_id = Some(session_id);
                self.supports_image = supports_image;
                match kind {
                    // `session/load` 续接：agent 马上把完整历史重放一遍，本地
                    // 快照先清空避免变成两份（重放内容比本地快照权威）。
                    ReadyKind::ResumedWithReplay => self.entries.clear(),
                    // `session/resume` 续接：不重放，本地快照就是全部内容——
                    // 既不能清（清了就真没了），也别插分割线（对话是连着的）。
                    ReadyKind::ResumedKeepHistory => {}
                    // 全新会话，但本地还留着上一段——插分割线标明断点，
                    // 不能让人以为 agent 记得上面的内容。
                    ReadyKind::Fresh if !self.entries.is_empty() => {
                        self.entries.push(AcpEntry::Divider(format!(
                            "新会话 · agent 不记得以上内容 · {}",
                            chrono::Local::now().format("%m-%d %H:%M")
                        )));
                    }
                    ReadyKind::Fresh => {}
                }
                self.phase = AcpPhase::Idle;
                self.status_line = None;
            }
            AcpEvent::AgentChunk { thought, text } => {
                // 连续同类 chunk 并入最后一条，流式追加不炸条目数。
                match self.entries.last_mut() {
                    Some(AcpEntry::Assistant {
                        text: t,
                        thought: th,
                    }) if *th == thought => {
                        t.push_str(&text);
                    }
                    _ => self.entries.push(AcpEntry::Assistant { text, thought }),
                }
                self.phase = AcpPhase::Running;
            }
            AcpEvent::ToolCall(tc) => {
                self.entries.push(AcpEntry::ToolCall {
                    id: tc.tool_call_id.to_string(),
                    title: tc.title,
                    kind: tool_kind_from_acp(tc.kind),
                    status: tool_status_from_acp(tc.status),
                    output: tool_content_parts(&tc.content),
                });
                self.phase = AcpPhase::Running;
            }
            AcpEvent::ToolCallUpdate(u) => {
                let update_id = u.tool_call_id.to_string();
                if let Some(AcpEntry::ToolCall {
                    title,
                    kind,
                    status,
                    output,
                    ..
                }) = self
                    .entries
                    .iter_mut()
                    .rev()
                    .find(|e| matches!(e, AcpEntry::ToolCall { id, .. } if *id == update_id))
                {
                    if let Some(t) = u.fields.title {
                        *title = t;
                    }
                    if let Some(k) = u.fields.kind {
                        *kind = tool_kind_from_acp(k);
                    }
                    if let Some(s) = u.fields.status {
                        *status = tool_status_from_acp(s);
                    }
                    if let Some(c) = u.fields.content {
                        *output = tool_content_parts(&c);
                    }
                }
            }
            AcpEvent::Model(state) => {
                self.model = Some(state);
            }
            AcpEvent::Plan(p) => {
                // 每次全量覆盖：协议约定 Plan 更新携带完整清单，不做增量合并。
                self.plan = Some(p);
                self.phase = AcpPhase::Running;
            }
            AcpEvent::Permission {
                question,
                tool_call_id,
                pub_options,
                responder,
            } => {
                self.permission = Some(PermissionCard {
                    question: question.clone(),
                    tool_call_id,
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
                        p.0.lock()
                            .unwrap()
                            .push(("等你批准".to_string(), question, true));
                    }
                }
            }
            AcpEvent::Elicitation {
                message,
                fields,
                responder,
            } => {
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
                        p.0.lock()
                            .unwrap()
                            .push(("等你选择".to_string(), message, false));
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
        if !skip_persist {
            cx.emit(AcpViewEvent::Changed);
        }
        cx.notify();
    }

    /// 把当前相位写进 DaemonStates 全局（key = `acp-` 前缀 sid）——Session::status、
    /// Dock 角标、菜单栏、总览全部经既有链路自动点亮。
    fn sync_daemon_state(&self, cx: &mut App) {
        let Some(states) = cx.try_global::<crate::DaemonStates>() else {
            return;
        };
        let phase = match &self.phase {
            AcpPhase::Starting | AcpPhase::Idle => DaemonPhase::Idle,
            AcpPhase::Running => {
                // 有进行中的工具调用报「执行工具」，否则「思考中」。
                let executing = self.entries.iter().any(|e| {
                    matches!(
                        e,
                        AcpEntry::ToolCall {
                            status: ToolCallStatus::InProgress | ToolCallStatus::Pending,
                            ..
                        }
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
        let Some(card) = &mut self.elicitation else {
            return;
        };
        let Some(field) = card.fields.get(field_ix) else {
            return;
        };
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
        let single_select =
            card.fields.len() == 1 && matches!(card.fields[0].kind, ElicitFieldKind::Select(_));
        if single_select {
            self.submit_elicitation(cx);
        } else {
            cx.notify();
        }
    }

    /// 每个字段都有选择后才可提交（渲染侧按这个亮按钮）。
    fn elicit_ready(&self) -> bool {
        self.elicitation.as_ref().is_some_and(|card| {
            card.fields
                .iter()
                .enumerate()
                .all(|(ix, _)| card.chosen.get(&ix).is_some_and(|sel| !sel.is_empty()))
        })
    }

    fn submit_elicitation(&mut self, cx: &mut Context<Self>) {
        use agent_client_protocol::schema::v1::ElicitationContentValue as V;
        let Some(mut card) = self.elicitation.take() else {
            return;
        };
        let Some(responder) = card.responder.take() else {
            return;
        };
        let mut content = std::collections::BTreeMap::new();
        for (ix, field) in card.fields.iter().enumerate() {
            let Some(sel) = card.chosen.get(&ix) else {
                continue;
            };
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

    /// 当前模型的人类可读名（舞台头显示用）；None = agent 没上报过。
    pub(crate) fn model_name(&self) -> Option<String> {
        self.model.as_ref().map(|m| m.current_name.clone())
    }

    /// 切模型：值 id 来自协议给的候选列表。turn 跑着时也能切——协议允许，
    /// 生效范围由 agent 决定（通常下一轮起效），我们不替它加限制。
    fn set_model(&mut self, value_id: String) {
        if let Some(h) = &self.handle {
            let _ = h.cmd_tx.try_send(AcpCommand::SetModel(value_id));
        }
    }

    /// 输入栏 agent 胶囊的展示名：从启动命令里抠个可读的包名/程序名
    /// （`bunx @scope/claude-agent-acp@0.59.0` → `claude-agent-acp`，
    /// `copilot --acp` → `copilot`）。没有模型名数据源，不硬编。
    fn agent_label(&self) -> String {
        let tok = self
            .cmd
            .split_whitespace()
            .rev()
            .find(|t| !t.starts_with('-'))
            .unwrap_or("agent");
        let name = tok.rsplit('/').next().unwrap_or(tok);
        name.split('@')
            .find(|s| !s.is_empty())
            .unwrap_or(name)
            .to_string()
    }

    /// PLAN 条：agent 上报的任务计划 → 消息流上方的可折叠进度条。
    /// 折叠 = 一行摘要 + 进度条；展开 = 三态步骤清单（对齐设计稿）。
    /// 只借 `&Context`（listener 不需要可变借用），render 里跟 theme 引用共存。
    fn render_plan_bar(&self, cx: &Context<Self>) -> Option<gpui::AnyElement> {
        let plan = self.plan.as_ref()?;
        let total = plan.entries.len();
        if total == 0 {
            return None;
        }
        let done = plan
            .entries
            .iter()
            .filter(|e| matches!(e.status, PlanEntryStatus::Completed))
            .count();
        let in_progress = plan
            .entries
            .iter()
            .filter(|e| matches!(e.status, PlanEntryStatus::InProgress))
            .count();
        // 「第几步 of 总数」：正在跑的算当前步；全完成就是 n of n。
        let current = (done + in_progress).min(total);
        let (summary, summary_color) = if done == total {
            (
                format!("{total} of {total} · 完成"),
                gpui::rgb(ui_theme::green()),
            )
        } else if in_progress > 0 {
            (
                format!("{current} of {total} · 进行中"),
                gpui::rgb(ui_theme::accent()),
            )
        } else {
            (
                format!("{done} of {total}"),
                gpui::rgb(ui_theme::text_muted()),
            )
        };
        let progress = (done as f32 + in_progress as f32 * 0.5) / total as f32;

        let mut bar = gpui_component::v_flex()
            .border_b_1()
            .border_color(gpui::rgb(ui_theme::border_dim()))
            .bg(gpui::rgb(ui_theme::bg_status()))
            .child(
                h_flex()
                    .id("acp-plan-toggle")
                    .px_4()
                    .py_2()
                    .gap_2p5()
                    .items_center()
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _ev, _window, cx| {
                        this.plan_collapsed = !this.plan_collapsed;
                        cx.notify();
                    }))
                    .child(
                        div()
                            .w(px(10.))
                            .text_xs()
                            .text_color(gpui::rgb(ui_theme::text_muted()))
                            .child(if self.plan_collapsed { "▸" } else { "▾" }),
                    )
                    .child(
                        div()
                            .text_xs()
                            .font_semibold()
                            .text_color(gpui::rgb(ui_theme::text_muted()))
                            .child("PLAN"),
                    )
                    .child(div().text_xs().text_color(summary_color).child(summary))
                    .child(
                        div()
                            .flex_1()
                            .max_w(px(180.))
                            .h(px(5.))
                            .rounded_full()
                            .bg(gpui::rgb(ui_theme::border_dim()))
                            .overflow_hidden()
                            .child(
                                div()
                                    .w(gpui::relative(progress.clamp(0., 1.)))
                                    .h_full()
                                    .bg(gpui::rgb(ui_theme::accent())),
                            ),
                    ),
            );
        if !self.plan_collapsed {
            let mut steps = gpui_component::v_flex().px_4().pb_3().gap_0p5();
            for entry in &plan.entries {
                let row = h_flex().gap_2p5().items_center().py_0p5();
                let row = match entry.status {
                    PlanEntryStatus::Completed => row
                        .child(
                            div()
                                .flex_shrink_0()
                                .size(px(15.))
                                .rounded_sm()
                                .bg(gpui::rgb(ui_theme::green()))
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_xs()
                                .text_color(gpui::rgb(ui_theme::on_accent()))
                                .child("✓"),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(gpui::rgb(ui_theme::text_faint()))
                                .line_through()
                                .child(entry.content.clone()),
                        ),
                    PlanEntryStatus::InProgress => row
                        .child(
                            div()
                                .flex_shrink_0()
                                .size(px(15.))
                                .rounded_sm()
                                .border_1()
                                .border_color(gpui::rgb(ui_theme::accent()))
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(
                                    div()
                                        .size(px(7.))
                                        .rounded_xs()
                                        .bg(gpui::rgb(ui_theme::accent())),
                                ),
                        )
                        .child(
                            h_flex()
                                .gap_1p5()
                                .items_center()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_medium()
                                        .text_color(gpui::rgb(ui_theme::text_bright()))
                                        .child(entry.content.clone()),
                                )
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(gpui::rgb(ui_theme::accent()))
                                        .child("· 进行中"),
                                ),
                        ),
                    // Pending 与协议未来的新状态都按「待做」渲染。
                    _ => row
                        .child(
                            div()
                                .flex_shrink_0()
                                .size(px(15.))
                                .rounded_sm()
                                .border_1()
                                .border_color(gpui::rgb(ui_theme::border_focus())),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(gpui::rgb(ui_theme::text_mid()))
                                .child(entry.content.clone()),
                        ),
                };
                steps = steps.child(row);
            }
            bar = bar.child(steps);
        }
        Some(bar.into_any_element())
    }

    /// ⌘⏎ 快捷批准：选第一个 allow 类选项（跟绿色主按钮同一目标）。
    fn pick_permission_primary(&mut self, cx: &mut Context<Self>) {
        let Some(card) = &self.permission else { return };
        let Some(pix) = card.options.iter().position(|o| {
            matches!(
                o.kind,
                PermissionOptionKind::AllowOnce | PermissionOptionKind::AllowAlways
            )
        }) else {
            return;
        };
        self.pick_permission(pix, cx);
    }

    /// 审批按钮：消费 responder 回 RPC，卡片收起，相位回 Running（agent 会继续）。
    fn pick_permission(&mut self, option_ix: usize, cx: &mut Context<Self>) {
        let Some(card) = &mut self.permission else {
            return;
        };
        let Some(opt) = card.options.get(option_ix) else {
            return;
        };
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
                    .child(self.status_line.clone().unwrap_or_else(|| {
                        // 说实话：慢的是 Claude Code 自己建会话（实测约 10 秒），
                        // 不是「首次下载适配器」——那句每次都显示，是假的。
                        // 报出已等秒数，免得看着像卡死。
                        let waited = self
                            .starting_since
                            .map(|t| t.elapsed().as_secs())
                            .unwrap_or(0);
                        let what = if self.acp_session_id.is_some() {
                            "正在续接上次的会话".to_string()
                        } else {
                            format!("正在启动 {}", self.agent.label())
                        };
                        format!("{what}…（已 {waited} 秒，通常 10 秒左右）")
                    }))
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
                            .py_1p5()
                            .rounded_lg()
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

        // 权限审批按钮条：构建一次，优先内嵌进消息流里对应的工具卡片底部
        // （凭 tool_call_id 关联，对齐设计稿「diff 卡片自带 Approve/Reject」）；
        // 消息流里找不到对应卡片时退回独立卡片渲染——责任链只有一个出口，
        // responder 不会既没人展示又没人消费。
        let perm_target: Option<String> =
            self.permission.as_ref().map(|c| c.tool_call_id.to_string());
        let perm_embedded = perm_target.as_ref().is_some_and(|tid| {
            self.entries
                .iter()
                .any(|e| matches!(e, AcpEntry::ToolCall { id, .. } if id == tid))
        });
        let mut perm_buttons: Option<gpui::AnyElement> = self.permission.as_ref().map(|card| {
            let primary_ix = card.options.iter().position(|o| {
                matches!(
                    o.kind,
                    PermissionOptionKind::AllowOnce | PermissionOptionKind::AllowAlways
                )
            });
            let mut buttons = h_flex().gap_2().items_center().flex_wrap();
            if let Some(pix) = primary_ix {
                let name = card.options[pix].name.clone();
                buttons = buttons.child(
                    div()
                        .id(("acp-perm-primary", pix))
                        .px_4()
                        .py_2()
                        .rounded_lg()
                        .bg(gpui::rgb(ui_theme::green()))
                        .text_color(gpui::rgb(ui_theme::on_accent()))
                        .text_sm()
                        .font_semibold()
                        .cursor_pointer()
                        .hover(|d| d.opacity(0.9))
                        .child(format!("{name} ⌘⏎"))
                        .on_click(cx.listener(move |this, _ev, _window, cx| {
                            this.pick_permission(pix, cx);
                        })),
                );
            }
            for (ix, opt) in card.options.iter().enumerate() {
                if Some(ix) == primary_ix {
                    continue;
                }
                let danger = matches!(
                    opt.kind,
                    PermissionOptionKind::RejectOnce | PermissionOptionKind::RejectAlways
                );
                buttons = buttons.child(
                    div()
                        .id(("acp-perm-opt", ix))
                        .px_3()
                        .py_2()
                        .rounded_lg()
                        .border_1()
                        .border_color(t.border)
                        .text_sm()
                        .cursor_pointer()
                        .when(danger, |d| d.text_color(gpui::rgb(ui_theme::red())))
                        .hover(|d| d.opacity(0.85))
                        .child(opt.name.clone())
                        .on_click(cx.listener(move |this, _ev, _window, cx| {
                            this.pick_permission(ix, cx);
                        })),
                );
            }
            buttons.into_any_element()
        });

        // 消息流。
        let mut list = v_flex()
            .id("acp-entries")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .p_4()
            .gap_4();
        for (i, entry) in self.entries.iter().enumerate() {
            let el: gpui::AnyElement = match entry {
                // agent 回显的「中断」标记不是用户说的话，别套成气泡——
                // 那会读成「用户发了一条叫 [Request interrupted...] 的消息」。
                AcpEntry::User(text) if is_interrupt_marker(text) => h_flex()
                    .items_center()
                    .gap_2()
                    .my_1()
                    .child(div().flex_1().h(px(1.)).bg(t.border))
                    .child(div().text_xs().text_color(muted).child("已中断"))
                    .child(div().flex_1().h(px(1.)).bg(t.border))
                    .into_any_element(),
                // 用户气泡右对齐限宽（对齐设计稿）：整行铺满时跟 agent 正文
                // 混成一片，看不出谁在说话。
                AcpEntry::User(text) => h_flex()
                    .w_full()
                    .justify_end()
                    .child(
                        div()
                            .max_w(gpui::relative(0.72))
                            .px_4()
                            .py_2p5()
                            .rounded_lg()
                            .bg(t.muted)
                            .text_sm()
                            .child(crate::markdown_mermaid::markdown_view(
                                ("acp-user-md", i),
                                text.clone(),
                            )),
                    )
                    .into_any_element(),
                AcpEntry::Assistant { text, thought } => h_flex()
                    .items_start()
                    .gap_3()
                    .child(assistant_avatar())
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .pt(px(2.)) // 跟头像文字视觉基线对齐
                            .text_sm()
                            .when(*thought, |d| d.text_color(muted).italic())
                            .child(crate::markdown_mermaid::markdown_view(
                                ("acp-md", i),
                                text.clone(),
                            )),
                    )
                    .into_any_element(),
                AcpEntry::ToolCall {
                    id,
                    title,
                    kind,
                    status,
                    output,
                } => {
                    let accent = tool_accent_color(kind);
                    let (status_dot, status_label): (gpui::Hsla, &str) = match status {
                        ToolCallStatus::Pending => (t.muted_foreground, "待执行"),
                        ToolCallStatus::InProgress => {
                            (gpui::rgb(ui_theme::blue()).into(), "执行中")
                        }
                        ToolCallStatus::Completed => (gpui::rgb(ui_theme::green()).into(), "完成"),
                        ToolCallStatus::Failed => (gpui::rgb(ui_theme::red()).into(), "失败"),
                    };

                    // diff 汇总统计：头部摘要显示全部 diff 块加总的增删行数，
                    // 跟截图里 Edit 卡片右上角「+18 -4」的形态对齐。
                    let diff_totals: Vec<(usize, usize)> = output
                        .iter()
                        .filter_map(|p| match p {
                            ToolOutputPart::Diff {
                                old_text, new_text, ..
                            } => Some(diff_line_stats(old_text.as_deref().unwrap_or(""), new_text)),
                            _ => None,
                        })
                        .collect();
                    let has_diff = !diff_totals.is_empty();
                    let (total_added, total_removed) = diff_totals
                        .iter()
                        .fold((0usize, 0usize), |(a, r), (da, dr)| (a + da, r + dr));

                    let header_right: gpui::AnyElement = if has_diff {
                        h_flex()
                            .gap_2()
                            .text_xs()
                            .font_family("monospace")
                            .child(
                                div()
                                    .text_color(gpui::rgb(ui_theme::green()))
                                    .child(format!("+{total_added}")),
                            )
                            .child(
                                div()
                                    .text_color(gpui::rgb(ui_theme::red()))
                                    .child(format!("-{total_removed}")),
                            )
                            .into_any_element()
                    } else {
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(div().size_2().rounded_full().bg(status_dot))
                            .child(div().text_xs().text_color(muted).child(status_label))
                            .into_any_element()
                    };

                    let mut card = v_flex()
                        .rounded_lg()
                        .border_1()
                        .border_color(t.border)
                        .child(
                            h_flex()
                                .px_4()
                                .py_2p5()
                                .gap_2()
                                .items_center()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_semibold()
                                        .text_color(accent)
                                        .child(tool_kind_label(kind)),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .text_sm()
                                        .font_family("monospace")
                                        .text_color(muted)
                                        .truncate()
                                        .child(strip_kind_prefix(title, kind).to_string()),
                                )
                                .child(header_right),
                        );
                    for (part_ix, part) in output.iter().enumerate() {
                        card = match part {
                            ToolOutputPart::Diff {
                                path,
                                old_text,
                                new_text,
                            } => card.child(
                                v_flex()
                                    .px_4()
                                    .pb_3()
                                    .gap_1()
                                    .child(div().text_xs().text_color(muted).child(path.clone()))
                                    .child(render_diff_lines(
                                        old_text.as_deref().unwrap_or(""),
                                        new_text,
                                        (i, part_ix),
                                        t.border,
                                        t.muted_foreground,
                                    )),
                            ),
                            ToolOutputPart::Text(text) if !text.trim().is_empty() => {
                                // adapter 把工具输出包在 markdown 围栏里（```console…```），
                                // 当纯文本渲染会把 ``` 直接显示出来。剥掉再展示。
                                let body = strip_code_fence(text);
                                let lines: Vec<&str> = body.lines().collect();
                                let total = lines.len();
                                let key = id.to_string();
                                let expanded = self.expanded_tools.contains(&key);
                                // 默认只出前 8 行：以前是 max_h + overflow_hidden，
                                // 内容被硬切掉且没有任何展开入口，等于看不到全部。
                                let shown = if expanded || total <= TOOL_OUTPUT_PREVIEW_LINES {
                                    body.to_string()
                                } else {
                                    lines[..TOOL_OUTPUT_PREVIEW_LINES].join("\n")
                                };
                                let need_toggle = total > TOOL_OUTPUT_PREVIEW_LINES;
                                card.child(
                                    v_flex()
                                        .px_4()
                                        .pb_3()
                                        .gap_1()
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(muted)
                                                .font_family("monospace")
                                                .child(shown),
                                        )
                                        .when(need_toggle, |d| {
                                            let key = key.clone();
                                            d.child(
                                                div()
                                                    .id(("acp-tool-toggle", i * 100 + part_ix))
                                                    .text_xs()
                                                    .text_color(gpui::rgb(ui_theme::blue()))
                                                    .cursor_pointer()
                                                    .hover(|d| d.opacity(0.8))
                                                    .child(if expanded {
                                                        "收起".to_string()
                                                    } else {
                                                        format!("展开全部 {total} 行")
                                                    })
                                                    .on_click(cx.listener(
                                                        move |this, _ev, _window, cx| {
                                                            if !this.expanded_tools.remove(&key) {
                                                                this.expanded_tools
                                                                    .insert(key.clone());
                                                            }
                                                            cx.notify();
                                                        },
                                                    )),
                                            )
                                        }),
                                )
                            }
                            ToolOutputPart::Text(_) => card,
                        };
                    }
                    // 这条工具调用正在等审批 → 按钮条内嵌进卡片底部。
                    if perm_target.as_ref() == Some(id) {
                        if let Some(btns) = perm_buttons.take() {
                            card = card.child(
                                v_flex()
                                    .px_4()
                                    .py_2p5()
                                    .gap_2()
                                    .border_t_1()
                                    .border_color(t.border)
                                    .child(btns),
                            );
                        }
                    }
                    card.into_any_element()
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

        // 「正在思考」占位：回合在跑、但 agent 还没吐出正文（最后一条不是
        // assistant 气泡）时，消息流末尾必须有活的东西。否则从按下发送到首字
        // 落地之间是一整屏纯黑——Copilot 这类首字延迟长的 agent 上看着像卡死。
        // 用 Spinner 而不是「已 N 秒」：GPUI 没有定时重绘，秒数会僵在原地，
        // 反而更像死了；spinner 自带动画帧，转着就说明进程还在。
        if matches!(self.phase, AcpPhase::Running)
            && !matches!(self.entries.last(), Some(AcpEntry::Assistant { .. }))
        {
            list = list.child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(Spinner::new().xsmall().color(muted))
                    .child(
                        div()
                            .text_sm()
                            .text_color(muted)
                            .child(format!("{} 正在思考…", self.agent.short_label())),
                    ),
            );
        }

        // 独立权限卡片：仅当消息流里没有可内嵌的工具卡片时兜底渲染
        // （按钮条构建见循环前的 perm_buttons；内嵌成功时这里拿到的是 None）。
        let permission = (!perm_embedded)
            .then(|| {
                let card = self.permission.as_ref()?;
                let buttons = perm_buttons.take()?;
                Some(
                    v_flex()
                        .mx_4()
                        .mb_3()
                        .p_4()
                        .gap_3()
                        .rounded_lg()
                        .border_1()
                        .border_color(t.border)
                        .child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(div().text_sm().font_semibold().child("等你批准"))
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(muted)
                                        .child(card.question.clone()),
                                ),
                        )
                        .child(buttons),
                )
            })
            .flatten();

        // 选择题卡片：message + 逐字段按钮组；单字段单选点击即提交，
        // 其余选齐后亮「提交」；「跳过」丢卡（responder Drop 回 Cancel）。
        let elicitation = self.elicitation.as_ref().map(|card| {
            let ready = self.elicit_ready();
            let mut body = v_flex()
                .mx_4()
                .mb_3()
                .p_4()
                .gap_3()
                .rounded_lg()
                .border_1()
                .border_color(gpui::rgb(ui_theme::yellow()))
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(div().text_sm().font_semibold().child("等你选择"))
                        .child(
                            div()
                                .text_sm()
                                .text_color(muted)
                                .child(card.message.clone()),
                        ),
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
                            .rounded_lg()
                            .border_1()
                            .border_color(if selected {
                                gpui::rgb(ui_theme::yellow()).into()
                            } else {
                                t.border
                            })
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
                            d.child(div().text_xs().text_color(muted).child(format!(
                                "{}{}",
                                field.title.clone(),
                                if is_multi { "（可多选）" } else { "" }
                            )))
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
                                .rounded_lg()
                                .text_sm()
                                .when(ready, |d| {
                                    d.bg(gpui::rgb(ui_theme::green()))
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
                                .rounded_lg()
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

        // 胶囊优先显示真实模型名；协议没给就退回适配器名——但要让人看得出
        // 那是「适配器」不是模型，不能拿包名冒充模型。
        let (pill_text, pill_is_model) = match &self.model {
            Some(m) => (m.current_name.clone(), true),
            None => (self.agent_label(), false),
        };
        // 候选模型（协议给了才有）：胶囊变成可点下拉，点一项即切。
        let model_options: Vec<(String, String)> = self
            .model
            .as_ref()
            .map(|m| m.options.clone())
            .unwrap_or_default();
        let current_model = self.model.as_ref().map(|m| m.current_name.clone());
        // 补全弹层：画在输入框**上方**，而且是正常流式元素不是绝对定位浮层——
        // 输入框贴着窗口底边，往下开的菜单一定会被窗口边缘裁掉（组件自带那套
        // 就是这么废的，见 acp_completion.rs 文件头）。往上顶消息流反而符合
        // CLI 补全条的直觉。
        let completion_bar = self.completion.as_ref().map(|popup| {
            let mut list = v_flex()
                .id("acp-completion")
                .max_h(px(220.))
                .overflow_y_scroll()
                .border_t_1()
                .border_color(t.border)
                .bg(ui_theme::overlay(0x14));
            for (ix, item) in popup.items.iter().enumerate() {
                let selected = ix == popup.selected;
                list = list.child(
                    h_flex()
                        .id(("acp-completion-item", ix))
                        .px_3()
                        .py_1()
                        .gap_2()
                        .items_center()
                        .when(selected, |d| d.bg(ui_theme::overlay(0x28)))
                        .cursor_pointer()
                        .hover(|d| d.bg(ui_theme::overlay(0x20)))
                        .child(
                            div()
                                .flex_shrink_0()
                                .text_xs()
                                .font_family("monospace")
                                .text_color(if selected {
                                    gpui::rgb(ui_theme::accent())
                                } else {
                                    gpui::rgb(ui_theme::text_mid())
                                })
                                .child(item.label.clone()),
                        )
                        .when(!item.hint.is_empty(), |row| {
                            row.child(
                                div()
                                    .min_w_0()
                                    .text_xs()
                                    .text_color(muted)
                                    .truncate()
                                    .child(item.hint.clone()),
                            )
                        })
                        .on_click(cx.listener(move |this, _ev, window, cx| {
                            if let Some(popup) = &mut this.completion {
                                popup.selected = ix;
                            }
                            this.accept_completion(window, cx);
                        })),
                );
            }
            list.child(
                div()
                    .px_3()
                    .py_1()
                    .text_xs()
                    .text_color(muted)
                    .child("↑↓ 选择 · Enter/Tab 插入 · Esc 关闭"),
            )
        });

        let input_row = self.input.as_ref().map(|input| {
            v_flex()
                .border_t_1()
                .border_color(t.border)
                .child(
                    // 底部工具栏：上下文/命令数量/agent 名这类元信息用小胶囊展示，
                    // 跟输入框本体分层，不抢文字输入的视觉重量。
                    h_flex()
                        .px_3()
                        .pt_2()
                        .gap_2()
                        .items_center()
                        .child(
                            // 点一下就在输入框补个 `@` 并聚焦，补全菜单随即弹出
                            // ——这个胶囊以前是纯装饰，点不动。
                            div()
                                .id("acp-at-pill")
                                .px_2p5()
                                .py_0p5()
                                .rounded_full()
                                .bg(ui_theme::overlay(0x18))
                                .text_xs()
                                .text_color(gpui::rgb(ui_theme::text_muted()))
                                .cursor_pointer()
                                .hover(|d| d.opacity(0.8))
                                .child("@ 引用文件")
                                .on_click(cx.listener(|this, _ev, window, cx| {
                                    this.insert_prompt_text("@", window, cx);
                                })),
                        )
                        .when(!self.available_commands.is_empty(), |row| {
                            // 斜杠命令：点开列出来、点一条填进输入框。以前只显示
                            // 「N 条命令」这个数字——看得见、点不开、用不上。
                            let cmds = self.available_commands.clone();
                            let n = cmds.len();
                            let this = cx.entity();
                            row.child(
                                // 触发器必须是 Button：gpui-component 的
                                // DropdownMenu（左键）只对 Button 实现，挂在普通
                                // div 上只能用 context_menu，那是右键——等于点不动。
                                Button::new("acp-commands-pill")
                                    .ghost()
                                    .xsmall()
                                    .label(format!("/ {n} 条命令 ▾"))
                                    .text_color(gpui::rgb(ui_theme::text_muted()))
                                    .dropdown_menu(move |menu, _window, _cx| {
                                        let mut menu = menu.item(PopupMenuItem::label("斜杠命令"));
                                        // 菜单塞不下几十条，列前 20 条（够覆盖常用的）
                                        for (name, desc) in cmds.iter().take(20) {
                                            let label = if desc.trim().is_empty() {
                                                format!("/{name}")
                                            } else {
                                                format!("/{name} — {desc}")
                                            };
                                            let insert = format!("/{name}");
                                            let this = this.clone();
                                            menu = menu.item(PopupMenuItem::new(label).on_click(
                                                move |_ev, window, cx| {
                                                    let insert = insert.clone();
                                                    this.update(cx, |v, cx| {
                                                        v.insert_prompt_text(&insert, window, cx)
                                                    });
                                                },
                                            ));
                                        }
                                        menu
                                    }),
                            )
                        })
                        .children(self.usage.map(|(used, size)| {
                            // 上下文用量：接近用满时变色告警——「还剩多少」是决定
                            // 要不要 /compact 的依据，不该藏起来。
                            let pct = ((used as f64 / size as f64) * 100.0).round() as u32;
                            let color = if pct >= 90 {
                                ui_theme::red()
                            } else if pct >= 75 {
                                ui_theme::yellow()
                            } else {
                                ui_theme::text_muted()
                            };
                            div()
                                .px_2p5()
                                .py_0p5()
                                .rounded_full()
                                .bg(gpui::rgba(0x80808020))
                                .text_xs()
                                .text_color(gpui::rgb(color))
                                .child(format!("上下文 {pct}%"))
                        }))
                        .child({
                            // 模型胶囊：紫色。协议没上报模型时显示适配器名并加
                            // 「适配器」前缀，免得把包名读成模型名。
                            // 有候选就用 Button 触发左键下拉（见上面命令胶囊的注释）。
                            let label = if pill_is_model {
                                pill_text.clone()
                            } else {
                                format!("适配器 {pill_text}")
                            };
                            let color = if pill_is_model {
                                ui_theme::purple()
                            } else {
                                ui_theme::text_muted()
                            };
                            if model_options.len() > 1 {
                                let cur = current_model.clone();
                                let opts = model_options.clone();
                                let this = cx.entity();
                                Button::new("acp-model-pill")
                                    .ghost()
                                    .xsmall()
                                    .label(format!("{label} ▾"))
                                    .text_color(gpui::rgb(color))
                                    .dropdown_menu(move |menu, _window, _cx| {
                                        let mut menu = menu.item(PopupMenuItem::label("切换模型"));
                                        for (value, name) in &opts {
                                            let is_cur = cur.as_deref() == Some(name.as_str());
                                            let value = value.clone();
                                            let this = this.clone();
                                            // 用组件自带的 checked：勾选位由它统一
                                            // 预留，所有名字对齐。自己拿 "✓ " / "   "
                                            // 凑缩进对不齐——两者宽度并不相等。
                                            menu = menu.item(
                                                PopupMenuItem::new(name.clone())
                                                    .checked(is_cur)
                                                    .on_click(move |_ev, _window, cx| {
                                                        let value = value.clone();
                                                        this.update(cx, |v, _cx| {
                                                            v.set_model(value)
                                                        });
                                                    }),
                                            );
                                        }
                                        menu
                                    })
                                    .into_any_element()
                            } else {
                                div()
                                    .px_2p5()
                                    .py_0p5()
                                    .rounded_full()
                                    .bg(ui_theme::overlay(0x18))
                                    .text_xs()
                                    .text_color(gpui::rgb(color))
                                    .child(label)
                                    .into_any_element()
                            }
                        }),
                )
                // 待发图片的缩略图条：粘完得看得见「贴上了」，还得能反悔。
                .when(!self.pending_images.is_empty(), |col| {
                    let mut strip = h_flex().px_3().pt_2().gap_2().items_center().flex_wrap();
                    for (ix, im) in self.pending_images.iter().enumerate() {
                        strip = strip.child(
                            div()
                                .id(("acp-pending-img", ix))
                                .relative()
                                .child(
                                    gpui::img(im.clone())
                                        .h(px(56.))
                                        .max_w(px(96.))
                                        .rounded_md()
                                        .border_1()
                                        .border_color(t.border),
                                )
                                .child(
                                    // 右上角小 ×：点掉这张。
                                    div()
                                        .absolute()
                                        .top(px(-4.))
                                        .right(px(-4.))
                                        .size(px(16.))
                                        .rounded_full()
                                        .bg(ui_theme::overlay(0xcc))
                                        .text_xs()
                                        .text_color(gpui::rgb(ui_theme::text_mid()))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .cursor_pointer()
                                        .hover(|d| d.opacity(0.8))
                                        .child("×")
                                        .on_mouse_down(
                                            gpui::MouseButton::Left,
                                            cx.listener(move |this, _ev, _window, cx| {
                                                if ix < this.pending_images.len() {
                                                    this.pending_images.remove(ix);
                                                }
                                                cx.stop_propagation();
                                                cx.notify();
                                            }),
                                        ),
                                ),
                        );
                    }
                    col.child(strip)
                })
                .child(
                    h_flex()
                        .p_3()
                        .gap_2()
                        .items_end()
                        .child(div().flex_1().child(Input::new(input)))
                        .when(matches!(self.phase, AcpPhase::Running), |row| {
                            row.child(
                                div()
                                    .id("acp-stop")
                                    .px_2p5()
                                    .py_1()
                                    .rounded_lg()
                                    .border_1()
                                    .border_color(t.border)
                                    .text_xs()
                                    .text_color(muted)
                                    .cursor_pointer()
                                    .hover(|d| d.opacity(0.8))
                                    .child("停止")
                                    .on_click(
                                        cx.listener(|this, _ev, _window, _cx| this.cancel_turn()),
                                    ),
                            )
                        })
                        .child(
                            // 主发送按钮（橙实心，对齐设计稿 Send ⏎）。
                            div()
                                .id("acp-send")
                                .px_4()
                                .py_1p5()
                                .rounded_lg()
                                .bg(gpui::rgb(ui_theme::accent()))
                                .text_color(gpui::rgb(ui_theme::on_accent()))
                                .text_sm()
                                .font_semibold()
                                .cursor_pointer()
                                .hover(|d| d.opacity(0.9))
                                .child("发送 ⏎")
                                .on_click(cx.listener(|this, _ev, window, cx| {
                                    this.submit_input(window, cx);
                                })),
                        ),
                )
        });

        let plan_bar = self.render_plan_bar(cx);

        v_flex()
            .size_full()
            .track_focus(&self.focus_handle)
            // ⌘⏎ 快捷批准：有待审批卡片时等价于点绿色主按钮。挂在根上冒泡接收，
            // 输入框聚焦时也能生效（Input 只消费不带修饰键的 Enter）。
            .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _window, cx| {
                if ev.keystroke.modifiers.platform
                    && ev.keystroke.key == "enter"
                    && this.permission.is_some()
                {
                    this.pick_permission_primary(cx);
                    cx.stop_propagation();
                }
            }))
            // 补全弹层的键盘操作。同样只能走 **action 的 capture 阶段**：
            // 上/下/回车/Esc/Tab 在输入框里全都绑成了 action，冒泡阶段和
            // capture_key_down 都轮不到我们（见下面 ⌘V 那段的教训）。
            // 没在补全时一律不拦，按键原样交回输入框。
            .capture_action(
                cx.listener(|this, _: &gpui_component::input::MoveUp, _window, cx| {
                    if this.move_completion(-1, cx) {
                        cx.stop_propagation();
                    }
                }),
            )
            .capture_action(cx.listener(
                |this, _: &gpui_component::input::MoveDown, _window, cx| {
                    if this.move_completion(1, cx) {
                        cx.stop_propagation();
                    }
                },
            ))
            .capture_action(
                cx.listener(|this, _: &gpui_component::input::Enter, window, cx| {
                    // 补全开着时回车是「选中这条」，不是发送——否则永远选不上。
                    if this.accept_completion(window, cx) {
                        cx.stop_propagation();
                    }
                }),
            )
            .capture_action(cx.listener(
                |this, _: &gpui_component::input::IndentInline, window, cx| {
                    if this.accept_completion(window, cx) {
                        cx.stop_propagation();
                    }
                },
            ))
            .capture_action(
                cx.listener(|this, _: &gpui_component::input::Escape, _window, cx| {
                    if this.completion.take().is_some() {
                        cx.notify();
                        cx.stop_propagation();
                    }
                }),
            )
            // ⌘V 贴图（输入框聚焦时，也就是绝大多数情况）：必须拦 **Paste
            // action 的 capture 阶段**，不能拦 key_down。
            //
            // 真实教训：第一版挂的是 capture_key_down，实测完全没反应。GPUI 的
            // dispatch_key_event 顺序是「先派发 action bindings，binding 消费掉
            // 就直接 return」，capture 阶段的 key listener 排在那之后——输入框
            // 把 cmd-v 绑成了 Paste（gpui-component input/state.rs），于是这个
            // 事件永远轮不到我们。而 action 的 capture 阶段是从根往下走的，
            // 挂在这里就能抢在输入框（更深的节点）前面拿到。
            .capture_action(
                cx.listener(|this, _: &gpui_component::input::Paste, _window, cx| {
                    // 只有剪贴板真是图片才截胡；文本粘贴照样放行给输入框。
                    if this.take_clipboard_image(cx) {
                        cx.stop_propagation();
                    }
                }),
            )
            // 焦点不在输入框里（点了消息流等）时 Paste binding 不匹配，
            // action 那条路走不到——这条按 key_down 兜底。
            .capture_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _window, cx| {
                if ev.keystroke.modifiers.platform
                    && ev.keystroke.key == "v"
                    && this.take_clipboard_image(cx)
                {
                    cx.stop_propagation();
                }
            }))
            .bg(t.background)
            .children(banner)
            .children(plan_bar)
            .child(list)
            .children(permission)
            .children(elicitation)
            .children(completion_bar)
            .children(self.paste_hint.as_ref().map(|msg| {
                h_flex()
                    .items_center()
                    .gap_2()
                    .px_4()
                    .py_1p5()
                    .border_t_1()
                    .border_color(t.border)
                    .bg(ui_theme::tint(ui_theme::yellow(), 0x14))
                    .text_xs()
                    .text_color(gpui::rgb(ui_theme::yellow()))
                    .child(msg.clone())
            }))
            .children(input_row)
    }
}

/// GPUI 剪贴板图片格式 → 协议要的 MIME。
fn image_mime(format: gpui::ImageFormat) -> &'static str {
    match format {
        gpui::ImageFormat::Png => "image/png",
        gpui::ImageFormat::Jpeg => "image/jpeg",
        gpui::ImageFormat::Webp => "image/webp",
        gpui::ImageFormat::Gif => "image/gif",
        gpui::ImageFormat::Svg => "image/svg+xml",
        gpui::ImageFormat::Bmp => "image/bmp",
        gpui::ImageFormat::Tiff => "image/tiff",
        // 协议字段是必填的字符串，认不出的格式给个通用值让 agent 自己嗅探，
        // 总好过不发（ImageFormat 是 #[non_exhaustive]，会长新枝）。
        _ => "application/octet-stream",
    }
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// assistant 消息的发送方头像：主橙方块 + 首字母（设计稿色板 ACCENT），
/// 给消息流一个视觉锚点。
fn assistant_avatar() -> impl IntoElement {
    div()
        .flex_shrink_0()
        .w(px(24.))
        .h(px(24.))
        .rounded_md()
        .bg(gpui::rgb(ui_theme::accent()))
        .flex()
        .items_center()
        .justify_center()
        .text_color(gpui::rgb(ui_theme::on_accent()))
        .text_xs()
        .font_semibold()
        .child("C")
}

/// 工具输出默认只展开这么多行，其余折叠到「展开全部 N 行」后面。
const TOOL_OUTPUT_PREVIEW_LINES: usize = 8;

/// 工具标题里去掉与 kind 标签重复的前缀：adapter 常把标题写成
/// `Read crates/foo.rs`，而卡片左边已经有一个 `Read` 标签了。
fn strip_kind_prefix<'a>(title: &'a str, kind: &ToolKind) -> &'a str {
    let label = tool_kind_label(kind);
    title
        .strip_prefix(label)
        .map(|r| r.trim_start())
        .filter(|r| !r.is_empty())
        .unwrap_or(title)
}

/// ToolKind → 强调色：读类蓝、改类橙、执行类绿，一眼区分工具在干什么类型的事。
fn tool_accent_color(kind: &ToolKind) -> gpui::Rgba {
    match kind {
        ToolKind::Read | ToolKind::Search | ToolKind::Fetch => gpui::rgb(ui_theme::blue()),
        ToolKind::Edit | ToolKind::Delete | ToolKind::Move => gpui::rgb(ui_theme::accent()),
        ToolKind::Execute => gpui::rgb(ui_theme::green()),
        _ => gpui::rgb(ui_theme::text_muted()),
    }
}

/// ToolKind → 简短英文标签（跟工具本身在协议里的调用名对齐，比长句子扫得快）。
fn tool_kind_label(kind: &ToolKind) -> &'static str {
    match kind {
        ToolKind::Read => "Read",
        ToolKind::Edit => "Edit",
        ToolKind::Delete => "Delete",
        ToolKind::Move => "Move",
        ToolKind::Search => "Search",
        ToolKind::Execute => "Bash",
        ToolKind::Fetch => "Fetch",
        ToolKind::Think => "Think",
        ToolKind::SwitchMode => "Mode",
        _ => "Tool",
    }
}

/// 渲染一份 diff：逐行红（删）/绿（增）/灰（不变），等宽字体，滚动限高——大改动
/// 不能把整个消息流撑爆，超出部分滚动查看。`key` 保证同一条消息里多个 diff
/// 块各自有唯一 element id。行数据来自 `smelt_core::acp_chat::diff_lines`——
/// 跟头部「+N -M」摘要（`diff_line_stats`）共用同一次计算结果，数字不会对不上。
fn render_diff_lines(
    old: &str,
    new: &str,
    key: (usize, usize),
    border_color: gpui::Hsla,
    muted_color: gpui::Hsla,
) -> gpui::AnyElement {
    let mut rows = v_flex()
        .id(("acp-diff", key.0 * 10_000 + key.1))
        .max_h(px(320.))
        .overflow_y_scroll()
        .rounded_md()
        .border_1()
        .border_color(border_color)
        .font_family("monospace")
        .text_xs();
    for line in diff_lines(old, new) {
        let (bg, prefix, fg): (Option<gpui::Hsla>, &str, gpui::Hsla) = match line.tag {
            DiffLineTag::Removed => (
                Some(crate::ui_theme::tint(crate::ui_theme::red(), 0x22).into()),
                "-",
                gpui::rgb(crate::ui_theme::red()).into(),
            ),
            DiffLineTag::Added => (
                Some(crate::ui_theme::tint(crate::ui_theme::green(), 0x22).into()),
                "+",
                gpui::rgb(crate::ui_theme::diff_add_text()).into(),
            ),
            DiffLineTag::Context => (None, " ", muted_color),
        };
        let mut row = h_flex().px_2().gap_2();
        if let Some(bg) = bg {
            row = row.bg(bg);
        }
        rows = rows.child(
            row.child(
                div()
                    .w(px(12.))
                    .flex_shrink_0()
                    .text_color(fg)
                    .child(prefix.to_string()),
            )
            .child(div().flex_1().min_w_0().text_color(fg).child(line.text)),
        );
    }
    rows.into_any_element()
}

/// tool call content → 结构化的输出片段（文本走 acp.rs 的 ContentBlock 规则文本化；
/// diff 保留 old/new 原文，留给渲染层逐行算差异，不在这里压扁）。
fn tool_content_parts(
    content: &[agent_client_protocol::schema::v1::ToolCallContent],
) -> Vec<ToolOutputPart> {
    use agent_client_protocol::schema::v1::ToolCallContent;
    content
        .iter()
        .filter_map(|c| match c {
            ToolCallContent::Content(inner) => {
                let text = crate::acp::content_text(&inner.content);
                (!text.trim().is_empty()).then(|| ToolOutputPart::Text(text))
            }
            ToolCallContent::Diff(d) => Some(ToolOutputPart::Diff {
                path: d.path.display().to_string(),
                old_text: d.old_text.clone(),
                new_text: d.new_text.clone(),
            }),
            _ => None, // Terminal 等 MVP 不渲染
        })
        .collect()
}

// strip_code_fence / is_interrupt_marker 的单测随实现一起搬进了
// smelt_core::acp_chat（见该模块的 #[cfg(test)]），这里不再重复。
