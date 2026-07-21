//! ACP（Agent Client Protocol）会话后端：第二种会话类型的连接层。
//!
//! 职责边界（见 docs/project-report.md 第 5 节与 plans 里的接入方案）：
//! - 每个 ACP 会话一条专用 OS 线程 `smol::block_on` 驱动整个连接（spawn 子进程、
//!   JSON-RPC over stdio、事件翻译），与全库「专用线程 + smol::channel + UI 线程
//!   drain」的惯用法一致；
//! - 本模块**不许引 gpui**——未来 smeltd 托管 ACP 子进程时要原样下沉 smelt-core，
//!   GPUI Entity/渲染都圈在 acp_view.rs；
//! - 一切失败（找不到命令 / 握手失败 / 子进程退出）都以 `AcpEvent::Fatal` 从事件
//!   通道出来，`spawn_acp` 本身永不阻塞、永不 panic 调用方。

use std::sync::{Arc, Mutex, OnceLock};

use std::collections::BTreeMap;

use agent_client_protocol::schema::v1::{
    CancelNotification, ClientCapabilities, ContentBlock, CreateElicitationRequest,
    CreateElicitationResponse, ElicitationAcceptAction, ElicitationAction,
    ElicitationCapabilities, ElicitationContentValue, ElicitationFormCapabilities,
    ElicitationMode, ElicitationPropertySchema, ElicitationSchema, InitializeRequest,
    LoadSessionRequest, MultiSelectItems, NewSessionResponse, PermissionOption,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionId, SessionNotification, SessionUpdate, StopReason,
    ToolCall, ToolCallUpdate,
};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::util::MatchDispatch;
use agent_client_protocol::{
    ActiveSession, AcpAgent, Agent, Client, ConnectionTo, LineDirection, SessionMessage,
};

/// 一次 ACP 会话的启动参数。
pub struct AcpLaunch {
    /// 启动命令（空白分词；MVP 不支持带引号的参数）。默认见 settings::acp_cmd。
    pub cmd: String,
    /// 会话工作目录（newSession 的 cwd）；None 用进程当前目录。
    pub cwd: Option<String>,
    /// GUI 侧会话 id，约定 `acp-` 前缀——DaemonStates 全局 map 里靠这个前缀
    /// 与 smeltd 会话共存（见 main.rs 状态转发循环的 retain）。
    pub sid: String,
    /// 上一次连接的 agent 侧 session id：有就先尝试 `session/load` 真正续接
    /// （agent 记得之前聊了什么），adapter 不支持该能力或 load 失败则自动
    /// 退回 `session/new`（全新会话，见 AcpEvent::Ready 的 resumed 字段）。
    pub resume_session_id: Option<SessionId>,
}

/// UI → 连接线程的指令。
pub enum AcpCommand {
    /// 发一轮 prompt（agent 空闲时才该发；UI 侧负责在 turn 进行中排队/禁用）。
    Prompt(String),
    /// 取消当前 turn（session/cancel 通知）。
    Cancel,
    /// 关闭会话：退出连接循环，随连接 drop 杀掉子进程。
    Shutdown,
}

/// 连接线程 → UI 的事件。schema 类型（ToolCall 等）原样透传，不造平行模型。
pub enum AcpEvent {
    /// 启动阶段的进度文案（下载运行时 / 拉取适配器等），Starting 横幅显示。
    Status(String),
    /// 握手完成，可以发 prompt 了。`resumed` = 这次是不是真的接上了旧会话
    /// （`session/load` 成功）——UI 据此决定清空本地历史（真续接，agent 会
    /// 重放全部）还是保留历史并插分割线（这其实是一次全新会话）。
    Ready { session_id: SessionId, resumed: bool },
    /// assistant 正文 / 思考块的流式增量（content 已文本化）。
    AgentChunk { thought: bool, text: String },
    ToolCall(ToolCall),
    ToolCallUpdate(ToolCallUpdate),
    /// agent 请求权限：UI 渲染按钮，凭 responder 直接回 RPC。
    Permission {
        /// 请求摘要（tool call 标题，没有就用工具 id）。
        question: String,
        pub_options: Vec<PermissionOption>,
        responder: PermissionResponder,
    },
    /// 用户消息的回显：`session/load` 重放历史时，agent 会把旧的用户提问也
    /// 当一条更新发回来（这是 entries 里 User 记录在 replay 场景下唯一的来源，
    /// 我们没有替它们手动 push 过）。正常 live 对话是否也会收到这个事件目前
    /// 没有把握确认，UI 侧用「等回声」状态机兼容两种可能，见 acp_view.rs。
    UserChunk(String),
    /// agent 的选择题 / 表单（AskUserQuestion 类）：UI 渲染字段，凭 responder 回填。
    Elicitation {
        message: String,
        fields: Vec<ElicitField>,
        responder: ElicitationResponder,
    },
    /// 一轮 prompt 结束（含被取消）。
    TurnEnded(StopReason),
    /// 连接不可恢复地结束：启动失败 / 协议错误 / 子进程退出。带 stderr 尾巴。
    Fatal(String),
}

/// 权限回执守卫：UI 点按钮时消费；**被 drop（视图关闭、卡片被弃置）自动回
/// Cancelled**，保证 agent 侧永远等得到答案、不会挂起。
pub struct PermissionResponder(
    Option<agent_client_protocol::Responder<RequestPermissionResponse>>,
);

impl PermissionResponder {
    /// 选中某个选项（allow / reject 都是「选中」，语义在 option.kind 里）。
    pub fn select(mut self, option_id: agent_client_protocol::schema::v1::PermissionOptionId) {
        if let Some(r) = self.0.take() {
            let _ = r.respond(RequestPermissionResponse::new(
                RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option_id)),
            ));
        }
    }
}

impl Drop for PermissionResponder {
    fn drop(&mut self) {
        if let Some(r) = self.0.take() {
            let _ = r.respond(RequestPermissionResponse::new(
                RequestPermissionOutcome::Cancelled,
            ));
        }
    }
}

/// 表单字段的 UI 无关简化模型（schema 细节收在本模块，视图只见这个）。
pub struct ElicitField {
    /// accept 回填时的 key（schema properties 的键名）。
    pub key: String,
    pub title: String,
    pub kind: ElicitFieldKind,
}

pub enum ElicitFieldKind {
    /// 单选：点一个按钮。布尔字段也翻译成 是/否 两个选项。
    Select(Vec<ElicitOption>),
    /// 多选：可切换多个再提交。
    MultiSelect(Vec<ElicitOption>),
}

pub struct ElicitOption {
    pub value: ElicitationContentValue,
    pub label: String,
}

/// 表单回执守卫：accept/decline 消费；**被 drop 自动回 Cancel**，agent 不会挂起。
pub struct ElicitationResponder(
    Option<agent_client_protocol::Responder<CreateElicitationResponse>>,
);

impl ElicitationResponder {
    pub fn accept(mut self, content: BTreeMap<String, ElicitationContentValue>) {
        if let Some(r) = self.0.take() {
            let _ = r.respond(CreateElicitationResponse::new(ElicitationAction::Accept(
                ElicitationAcceptAction::new().content(content),
            )));
        }
    }
}

impl Drop for ElicitationResponder {
    fn drop(&mut self) {
        if let Some(r) = self.0.take() {
            let _ = r.respond(CreateElicitationResponse::new(ElicitationAction::Cancel));
        }
    }
}

/// schema → 简化字段模型。宽容策略：
/// - 按钮化不了的**可选**字段（自由文本、数字等——如 AskUserQuestion 给每题附带的
///   "Other" 自由回答框）直接跳过，不提交即等于没填；
/// - 按钮化不了的**必填**字段 → 返回 None，调用方整表 Decline，agent 退回纯文本问
///   （不能提交一份缺必填项的表单）；
/// - 一个可按钮化字段都没有 → None。
fn parse_elicit_fields(schema: &ElicitationSchema) -> Option<Vec<ElicitField>> {
    let required = schema.required.clone().unwrap_or_default();
    let mut fields = Vec::new();
    for (key, prop) in &schema.properties {
        let buttonized = match prop {
            ElicitationPropertySchema::String(s) => {
                let options: Vec<ElicitOption> = if let Some(one_of) = &s.one_of {
                    one_of
                        .iter()
                        .map(|o| ElicitOption {
                            value: ElicitationContentValue::String(o.value.clone()),
                            label: o.title.clone(),
                        })
                        .collect()
                } else if let Some(values) = &s.enum_values {
                    values
                        .iter()
                        .map(|v| ElicitOption {
                            value: ElicitationContentValue::String(v.clone()),
                            label: v.clone(),
                        })
                        .collect()
                } else {
                    Vec::new() // 自由文本：按钮化不了
                };
                (!options.is_empty()).then(|| ElicitField {
                    key: key.clone(),
                    title: s.title.clone().unwrap_or_else(|| key.clone()),
                    kind: ElicitFieldKind::Select(options),
                })
            }
            ElicitationPropertySchema::Boolean(b) => Some(ElicitField {
                key: key.clone(),
                title: b.title.clone().unwrap_or_else(|| key.clone()),
                kind: ElicitFieldKind::Select(vec![
                    ElicitOption { value: ElicitationContentValue::Boolean(true), label: "是".into() },
                    ElicitOption { value: ElicitationContentValue::Boolean(false), label: "否".into() },
                ]),
            }),
            ElicitationPropertySchema::Array(a) => {
                let options: Vec<ElicitOption> = match &a.items {
                    MultiSelectItems::String(items) => items
                        .values
                        .iter()
                        .map(|v| ElicitOption {
                            value: ElicitationContentValue::String(v.clone()),
                            label: v.clone(),
                        })
                        .collect(),
                    MultiSelectItems::Titled(items) => items
                        .options
                        .iter()
                        .map(|o| ElicitOption {
                            value: ElicitationContentValue::String(o.value.clone()),
                            label: o.title.clone(),
                        })
                        .collect(),
                    _ => Vec::new(),
                };
                (!options.is_empty()).then(|| ElicitField {
                    key: key.clone(),
                    title: a.title.clone().unwrap_or_else(|| key.clone()),
                    kind: ElicitFieldKind::MultiSelect(options),
                })
            }
            _ => None, // Number/Integer/未知：MVP 不按钮化
        };
        match buttonized {
            Some(field) => fields.push(field),
            None if required.iter().any(|r| r == key) => return None,
            None => {} // 可选且按钮化不了：跳过
        }
    }
    if fields.is_empty() { None } else { Some(fields) }
}

/// UI 侧持有的会话句柄。drop cmd_tx（整个句柄）即请求连接收摊。
pub struct AcpHandle {
    pub cmd_tx: smol::channel::Sender<AcpCommand>,
    pub event_rx: smol::channel::Receiver<AcpEvent>,
}

/// 起一条专用线程跑 ACP 连接，立即返回句柄。
pub fn spawn_acp(launch: AcpLaunch) -> AcpHandle {
    let (cmd_tx, cmd_rx) = smol::channel::unbounded::<AcpCommand>();
    let (event_tx, event_rx) = smol::channel::unbounded::<AcpEvent>();
    let thread_name = format!("smelt-acp-{}", &launch.sid[..launch.sid.len().min(12)]);
    std::thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            // stderr 尾巴：环形保尾部若干行，Fatal 时拼进诊断（npx 找不到包/装包
            // 失败的真实原因都在 stderr 里，别让用户猜）。
            let stderr_tail: Arc<Mutex<Vec<String>>> = Arc::default();
            // 先解析运行时（bunx → 受管 bun，可能触发首次下载），再进连接循环。
            let cmd = {
                let tx = event_tx.clone();
                match resolve_runtime_command(&launch.cmd, &|msg| {
                    let _ = tx.try_send(AcpEvent::Status(msg.to_string()));
                }) {
                    Ok(cmd) => cmd,
                    Err(e) => {
                        let _ = event_tx.try_send(AcpEvent::Fatal(e));
                        return;
                    }
                }
            };
            let launch = AcpLaunch { cmd, ..launch };
            let result = smol::block_on(run_connection(
                &launch,
                cmd_rx,
                event_tx.clone(),
                stderr_tail.clone(),
            ));
            if let Err(e) = result {
                let tail = stderr_tail.lock().unwrap().join("\n");
                let msg = if tail.is_empty() {
                    format!("{e}")
                } else {
                    format!("{e}\n--- agent stderr ---\n{tail}")
                };
                let _ = event_tx.try_send(AcpEvent::Fatal(msg));
            }
            // Ok 结束（Shutdown）不发 Fatal——UI 主动关的，没必要再报。
        })
        .expect("spawn smelt-acp thread");
    AcpHandle { cmd_tx, event_rx }
}

/// 连接主体：spawn agent 子进程 → initialize → newSession → 双源 loop
/// （UI 指令 / agent 更新流）。返回 Ok 表示用户主动 Shutdown。
async fn run_connection(
    launch: &AcpLaunch,
    cmd_rx: smol::channel::Receiver<AcpCommand>,
    event_tx: smol::channel::Sender<AcpEvent>,
    stderr_tail: Arc<Mutex<Vec<String>>>,
) -> Result<(), agent_client_protocol::Error> {
    let agent = build_agent(&launch.cmd, stderr_tail)?;
    let cwd = launch
        .cwd
        .clone()
        .or_else(|| std::env::current_dir().ok().map(|p| p.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "/".to_string());

    let perm_tx = event_tx.clone();
    let elicit_tx = event_tx.clone();
    Client
        .builder()
        .name("smelt")
        // 权限请求：Responder 打包进事件甩给 UI，handler 立即返回不堵事件循环；
        // UI 弃置卡片时 PermissionResponder 的 Drop 兜底回 Cancelled。
        .on_receive_request(
            move |request: RequestPermissionRequest, responder, _connection| {
                let perm_tx = perm_tx.clone();
                async move {
                    let question = permission_question(&request);
                    let _ = perm_tx.try_send(AcpEvent::Permission {
                        question,
                        pub_options: request.options,
                        responder: PermissionResponder(Some(responder)),
                    });
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        // 选择题 / 表单：能按钮化的甩给 UI，按钮化不了的立即 Decline——agent 会退回
        // 纯文本问，跟不支持该能力时的行为一致，绝不让请求悬着。
        .on_receive_request(
            move |request: CreateElicitationRequest, responder, _connection| {
                let elicit_tx = elicit_tx.clone();
                async move {
                    let fields = match &request.mode {
                        ElicitationMode::Form(form) => parse_elicit_fields(&form.requested_schema),
                        _ => None, // Url / 未知模式不支持
                    };
                    match fields {
                        Some(fields) => {
                            let _ = elicit_tx.try_send(AcpEvent::Elicitation {
                                message: request.message,
                                fields,
                                responder: ElicitationResponder(Some(responder)),
                            });
                            Ok(())
                        }
                        None => responder.respond(CreateElicitationResponse::new(
                            ElicitationAction::Decline,
                        )),
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent, |connection: ConnectionTo<Agent>| async move {
            let init = connection
                .send_request(
                    InitializeRequest::new(ProtocolVersion::V1).client_capabilities(
                        ClientCapabilities::default().elicitation(
                            ElicitationCapabilities::default()
                                .form(ElicitationFormCapabilities::default()),
                        ),
                    ),
                )
                .block_task()
                .await?;

            // 有旧 session id 且 adapter 声明支持 load_session：先试 session/load
            // 真续接（agent 记得之前聊了什么，会重放完整历史）。SDK 的 retry 机制
            // 保证 load 期间提前到达的 session/update 通知不会在 attach 前丢失
            // （jsonrpc.rs 里 `Handled::No{retry:true}` 的排队重试就是为这个场景
            // 设计的，session/new 走的也是同一套）。
            if let Some(sid) = launch.resume_session_id.clone() {
                if init.agent_capabilities.load_session {
                    match connection
                        .send_request(LoadSessionRequest::new(sid.clone(), cwd.clone()))
                        .block_task()
                        .await
                    {
                        Ok(loaded) => {
                            let resp = NewSessionResponse::new(sid)
                                .modes(loaded.modes)
                                .config_options(loaded.config_options)
                                .meta(loaded.meta);
                            let session = connection.attach_session(resp, Default::default())?;
                            return drive_session(session, cmd_rx, event_tx, true).await;
                        }
                        Err(e) => {
                            // 旧会话可能已被清理/损坏——不是致命错误，退回全新会话，
                            // 只告知用户「这不是真续接」。
                            let _ = event_tx.try_send(AcpEvent::Status(format!(
                                "旧会话恢复失败，已开新对话（{e}）"
                            )));
                        }
                    }
                }
            }

            connection
                .build_session(&cwd)
                .block_task()
                .run_until(async |session| drive_session(session, cmd_rx, event_tx, false).await)
                .await
        })
        .await
}

/// 驱动一个已建立的会话：发 Ready → 双源 loop（UI 指令 / agent 更新流）。
/// `session/load`（attach_session）与 `session/new`（run_until）两条路径共用。
async fn drive_session<'r>(
    mut session: ActiveSession<'r, Agent>,
    cmd_rx: smol::channel::Receiver<AcpCommand>,
    event_tx: smol::channel::Sender<AcpEvent>,
    resumed: bool,
) -> Result<(), agent_client_protocol::Error> {
    let _ = event_tx.try_send(AcpEvent::Ready { session_id: session.session_id().clone(), resumed });
    loop {
        // 两个等待源合一：先构造 read_update future，race 决议后它
        // 即被 drop（消息未出队不会丢），借用随之结束——绕开
        // 「cmd 分支也要 &mut session」的借用冲突。
        enum Next {
            Cmd(Option<AcpCommand>),
            Update(Result<SessionMessage, agent_client_protocol::Error>),
        }
        let next = {
            let read = session.read_update();
            smol::future::race(
                async { Next::Cmd(cmd_rx.recv().await.ok()) },
                async move { Next::Update(read.await) },
            )
            .await
        };
        match next {
            // 通道关闭（UI 句柄 drop）等同 Shutdown。
            Next::Cmd(None) | Next::Cmd(Some(AcpCommand::Shutdown)) => {
                return Ok(());
            }
            Next::Cmd(Some(AcpCommand::Prompt(text))) => {
                session.send_prompt(text)?;
            }
            Next::Cmd(Some(AcpCommand::Cancel)) => {
                session
                    .connection()
                    .send_notification(CancelNotification::new(session.session_id().clone()))?;
            }
            Next::Update(update) => {
                translate_update(update?, &event_tx).await?;
            }
        }
    }
}

/// 把 agent 的一条更新翻译成 AcpEvent（不认识的一律忽略——协议会长新枝）。
async fn translate_update(
    message: SessionMessage,
    event_tx: &smol::channel::Sender<AcpEvent>,
) -> Result<(), agent_client_protocol::Error> {
    match message {
        SessionMessage::SessionMessage(dispatch) => {
            MatchDispatch::new(dispatch)
                .if_notification(async |notif: SessionNotification| {
                    let event = match notif.update {
                        SessionUpdate::AgentMessageChunk(chunk) => Some(AcpEvent::AgentChunk {
                            thought: false,
                            text: content_text(&chunk.content),
                        }),
                        SessionUpdate::AgentThoughtChunk(chunk) => Some(AcpEvent::AgentChunk {
                            thought: true,
                            text: content_text(&chunk.content),
                        }),
                        SessionUpdate::ToolCall(tc) => Some(AcpEvent::ToolCall(tc)),
                        SessionUpdate::ToolCallUpdate(u) => Some(AcpEvent::ToolCallUpdate(u)),
                        SessionUpdate::UserMessageChunk(chunk) => {
                            Some(AcpEvent::UserChunk(content_text(&chunk.content)))
                        }
                        // Plan / 模式 / 用量等 MVP 不渲染（见方案「已知不做」）。
                        _ => None,
                    };
                    if let Some(ev) = event {
                        let _ = event_tx.try_send(ev);
                    }
                    Ok(())
                })
                .await
                .otherwise_ignore()?;
        }
        SessionMessage::StopReason(reason) => {
            let _ = event_tx.try_send(AcpEvent::TurnEnded(reason));
        }
        _ => {} // SessionMessage #[non_exhaustive]
    }
    Ok(())
}

/// 组装 AcpAgent：命令按空白分词，注入 login shell 的 PATH（Finder 启动的 GUI
/// 进程 PATH 不含 nvm/homebrew，直接 spawn `npx` 会 ENOENT），stderr 逐行收进尾巴。
fn build_agent(
    cmd: &str,
    stderr_tail: Arc<Mutex<Vec<String>>>,
) -> Result<AcpAgent, agent_client_protocol::Error> {
    let path_env = format!("PATH={}", login_shell_path());
    let args = std::iter::once(path_env.as_str()).chain(cmd.split_whitespace());
    let agent = AcpAgent::from_args(args)?.with_debug(move |line, direction| {
        if matches!(direction, LineDirection::Stderr) {
            let mut tail = stderr_tail.lock().unwrap();
            if tail.len() >= 30 {
                tail.remove(0);
            }
            tail.push(line.to_string());
        }
    });
    Ok(agent)
}

/// 权限卡片的问题摘要：tool call 有标题用标题，否则退回工具 id。
fn permission_question(request: &RequestPermissionRequest) -> String {
    request
        .tool_call
        .fields
        .title
        .clone()
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| format!("工具调用 {}", request.tool_call.tool_call_id))
}

/// ContentBlock 文本化：MVP 只取文本，资源/图片降级为占位（方案「已知不做」）。
pub fn content_text(content: &ContentBlock) -> String {
    match content {
        ContentBlock::Text(t) => t.text.clone(),
        ContentBlock::Image(_) => "[图片]".to_string(),
        ContentBlock::Audio(_) => "[音频]".to_string(),
        ContentBlock::ResourceLink(l) => format!("[资源 {}]", l.uri),
        ContentBlock::Resource(_) => "[内嵌资源]".to_string(),
        _ => "[未知内容]".to_string(), // schema #[non_exhaustive]，协议会长新枝
    }
}

/// —— 受管 bun 运行时（Zed 式按需下载）——————————————————————————
///
/// 适配器是 npm 包，需要 JS 运行时；不依赖用户装 node/bun，smelt 自己按需下载
/// 一份锁定版本的 bun（单文件）到 ~/.smelt/runtime/bun-v<版本>/。升级 = 改下面
/// 常量（URL 与 sha256 成对锁死），旧版本目录留着不碍事。
/// agent 主体是 SDK 自带的原生 claude 二进制，bun 只跑适配器那层薄翻译。
const BUN_VERSION: &str = "1.3.14";
#[cfg(target_arch = "aarch64")]
const BUN_DOWNLOAD: (&str, &str) = (
    "https://github.com/oven-sh/bun/releases/download/bun-v1.3.14/bun-darwin-aarch64.zip",
    "d8b96221828ad6f97ac7ac0ab7e95872341af763001e8803e8267652c2652620",
);
#[cfg(target_arch = "x86_64")]
const BUN_DOWNLOAD: (&str, &str) = (
    "https://github.com/oven-sh/bun/releases/download/bun-v1.3.14/bun-darwin-x64.zip",
    "4183df3374623e5bab315c547cfa0974533cd457d86b73b639f7a87974cd6633",
);
#[cfg(target_arch = "aarch64")]
const BUN_ZIP_DIR: &str = "bun-darwin-aarch64";
#[cfg(target_arch = "x86_64")]
const BUN_ZIP_DIR: &str = "bun-darwin-x64";

fn managed_bun_path() -> Option<std::path::PathBuf> {
    Some(dirs::home_dir()?.join(".smelt/runtime").join(format!("bun-v{BUN_VERSION}")).join("bun"))
}

/// 确保受管 bun 就位（不在则下载 + sha256 校验 + 冒烟），返回可执行路径。
fn ensure_bun(status: &dyn Fn(&str)) -> Result<std::path::PathBuf, String> {
    let bun = managed_bun_path().ok_or("找不到 home 目录")?;
    if bun.is_file() {
        return Ok(bun);
    }
    let dir = bun.parent().unwrap();
    std::fs::create_dir_all(dir).map_err(|e| format!("建目录 {} 失败：{e}", dir.display()))?;
    let (url, want_sha) = BUN_DOWNLOAD;
    status("正在下载 Bun 运行时（约 22MB，仅首次）…");
    let zip = dir.join(".download.zip");
    let out = std::process::Command::new("curl")
        .args(["-fsSL", "--retry", "2", "-o"])
        .arg(&zip)
        .arg(url)
        .output()
        .map_err(|e| format!("无法执行 curl：{e}"))?;
    if !out.status.success() {
        return Err(format!(
            "下载 Bun 失败（可离线安装：brew install bun 后把命令改成系统 bunx）：{}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    status("校验并解压运行时…");
    let sum = std::process::Command::new("shasum")
        .args(["-a", "256"])
        .arg(&zip)
        .output()
        .map_err(|e| format!("无法执行 shasum：{e}"))?;
    let got = String::from_utf8_lossy(&sum.stdout);
    let got = got.split_whitespace().next().unwrap_or("");
    if got != want_sha {
        let _ = std::fs::remove_file(&zip);
        return Err(format!("Bun 下载校验失败（期望 {want_sha}，实际 {got}），已丢弃"));
    }
    let unzip = std::process::Command::new("unzip")
        .args(["-o", "-q"])
        .arg(&zip)
        .arg("-d")
        .arg(dir)
        .output()
        .map_err(|e| format!("无法执行 unzip：{e}"))?;
    if !unzip.status.success() {
        return Err(format!("解压 Bun 失败：{}", String::from_utf8_lossy(&unzip.stderr).trim()));
    }
    let _ = std::fs::remove_file(&zip);
    std::fs::rename(dir.join(BUN_ZIP_DIR).join("bun"), &bun)
        .map_err(|e| format!("安放 bun 失败：{e}"))?;
    let _ = std::fs::remove_dir_all(dir.join(BUN_ZIP_DIR));
    // 冒烟：能报版本才算装好（顺带触发 macOS 首次执行检查）。
    let ver = std::process::Command::new(&bun)
        .arg("--version")
        .output()
        .map_err(|e| format!("bun 无法执行：{e}"))?;
    if !ver.status.success() {
        return Err("bun 下载后无法运行".to_string());
    }
    Ok(bun)
}

/// 命令首词是 `bunx`/`bun` 时解析到受管 bun（必要时下载）；受管失败但系统 PATH
/// 里有同名可执行则原样放行（用户自己装的）；其他命令一律不动（npx / 绝对路径
/// 等逃生口）。
fn resolve_runtime_command(cmd: &str, status: &dyn Fn(&str)) -> Result<String, String> {
    let mut words = cmd.split_whitespace();
    let head = words.next().unwrap_or_default();
    if head != "bunx" && head != "bun" {
        return Ok(cmd.to_string());
    }
    let rest: Vec<&str> = words.collect();
    match ensure_bun(status) {
        Ok(bun) => {
            let bun = bun.to_string_lossy().into_owned();
            let mut parts = vec![bun];
            if head == "bunx" {
                parts.push("x".to_string());
            }
            parts.extend(rest.iter().map(|s| s.to_string()));
            Ok(parts.join(" "))
        }
        Err(e) => {
            // 受管失败：系统里用户自己装过 bun 就用系统的。
            let sys_has = std::env::split_paths(login_shell_path())
                .any(|p| p.join(head).is_file());
            if sys_has {
                Ok(cmd.to_string())
            } else {
                Err(e)
            }
        }
    }
}

/// login shell 的 PATH（跑一次缓存）。GUI 进程从 Finder 启动时 PATH 只有系统
/// 目录，nvm/homebrew 里的 npx 找不到；跟终端会话不同（那边 shell 由 smeltd 起，
/// 自带 login 环境），ACP 子进程是 GUI 直接 spawn 的，得自己补。
fn login_shell_path() -> &'static str {
    static PATH: OnceLock<String> = OnceLock::new();
    PATH.get_or_init(|| {
        std::process::Command::new("/bin/zsh")
            .args(["-lc", "echo -n $PATH"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| std::env::var("PATH").unwrap_or_default())
    })
}

#[cfg(test)]
mod elicit_parse_tests {
    use super::*;

    /// claude-agent-acp 对 AskUserQuestion 的真实 wire 形状：单选 `oneOf`+`const`，
    /// 每题附带一个**可选**自由文本 "Other" 字段。曾因 Other 字段整表返回 None →
    /// Decline，agent 收到「用户未作答」——选择卡片永远弹不出来（回归守护）。
    #[test]
    fn ask_user_question_shape_with_optional_custom_field_parses() {
        let schema: ElicitationSchema = serde_json::from_value(serde_json::json!({
            "type": "object",
            "properties": {
                "question_0": {
                    "type": "string",
                    "title": "水果",
                    "oneOf": [
                        { "const": "苹果", "title": "苹果", "description": "脆甜多汁" },
                        { "const": "香蕉", "title": "香蕉" }
                    ]
                },
                "question_0_custom": {
                    "type": "string",
                    "title": "Other",
                    "description": "Type your own answer (optional)."
                }
            }
        }))
        .expect("schema 反序列化");
        let fields = parse_elicit_fields(&schema).expect("可选自由文本字段应被跳过而非整表放弃");
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].key, "question_0");
        let ElicitFieldKind::Select(options) = &fields[0].kind else {
            panic!("单选题应解析为 Select");
        };
        assert_eq!(options.len(), 2);
        assert_eq!(options[0].label, "苹果");
        assert!(matches!(&options[0].value, ElicitationContentValue::String(s) if s == "苹果"));
    }

    /// 必填的自由文本字段没法按钮化 → 必须整表 None（Decline），不能提交缺必填的表单。
    #[test]
    fn required_free_text_field_declines_whole_form() {
        let schema: ElicitationSchema = serde_json::from_value(serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "title": "你的名字" }
            },
            "required": ["name"]
        }))
        .expect("schema 反序列化");
        assert!(parse_elicit_fields(&schema).is_none());
    }

    /// 多选题：`type: "array"` + `items.anyOf`（titled 枚举）。
    #[test]
    fn multi_select_anyof_shape_parses() {
        let schema: ElicitationSchema = serde_json::from_value(serde_json::json!({
            "type": "object",
            "properties": {
                "question_0": {
                    "type": "array",
                    "title": "运动",
                    "items": { "anyOf": [
                        { "const": "跑步", "title": "跑步" },
                        { "const": "游泳", "title": "游泳" }
                    ] }
                }
            }
        }))
        .expect("schema 反序列化");
        let fields = parse_elicit_fields(&schema).expect("anyOf 多选应可解析");
        assert!(matches!(&fields[0].kind, ElicitFieldKind::MultiSelect(o) if o.len() == 2));
    }
}

#[cfg(test)]
mod runtime_tests {
    use super::*;

    /// 非 bun 前缀的命令一律原样放行（npx / 绝对路径等逃生口不被劫持）。
    #[test]
    fn non_bun_commands_pass_through() {
        let noop = |_: &str| {};
        for cmd in ["npx -y foo@1", "/usr/local/bin/some-acp --flag", "node adapter.js"] {
            assert_eq!(resolve_runtime_command(cmd, &noop).unwrap(), cmd);
        }
    }

    /// 受管 bun 已就位时，bunx 前缀改写为 `<managed-bun> x …`。
    #[test]
    fn bunx_rewrites_to_managed_bun_when_present() {
        let Some(bun) = managed_bun_path() else { return };
        if !bun.is_file() {
            return; // 受管 bun 未安装的机器上跳过（真实下载见 manual_ensure_bun）
        }
        let noop = |_: &str| {};
        let out = resolve_runtime_command("bunx pkg@1 --flag", &noop).unwrap();
        assert_eq!(out, format!("{} x pkg@1 --flag", bun.to_string_lossy()));
    }

    /// 真实下载验证 + 预热（22MB，网络依赖）：`cargo test -- --ignored manual_ensure_bun`
    #[test]
    #[ignore]
    fn manual_ensure_bun() {
        let path = ensure_bun(&|msg| eprintln!("[status] {msg}")).expect("ensure_bun");
        assert!(path.is_file());
        let out = std::process::Command::new(&path).arg("--version").output().unwrap();
        assert!(out.status.success());
        eprintln!("bun @ {} → {}", path.display(), String::from_utf8_lossy(&out.stdout).trim());
    }
}
