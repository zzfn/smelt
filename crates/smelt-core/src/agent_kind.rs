//! ACP 会话可接的 agent 种类 + 手动添加的 workspace profile：GUI（设置页 UI、
//! 会话标题、历史会话页）和未来的 acp-view 渲染层都要认同一份身份标识，放在
//! smelt-core 避免各自复制一份判断逻辑（本身也不需要 GPUI）。

/// ACP 会话可接的 agent 种类。**新增一种 agent = 这个枚举加一条**，命令、显示名、
/// 设置项、新建菜单都从这里派生，别再散着写死。
///
/// 序列化用 `id()` 那串小写标识（存进 workspace.json 的 ACP 会话存档），不用
/// serde 派生——枚举变体名将来改了不该炸存档。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AcpAgentKind {
    Claude,
    Copilot,
    Codex,
    Grok,
}

impl AcpAgentKind {
    /// 新建菜单 / 设置页的排列顺序。
    pub const ALL: [Self; 4] = [Self::Claude, Self::Copilot, Self::Codex, Self::Grok];

    /// 存档标识（稳定，别改）。
    pub fn id(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Copilot => "copilot",
            Self::Codex => "codex",
            Self::Grok => "grok",
        }
    }

    /// 存档标识 → 种类；认不出（旧存档 / 手改坏了）返回 None，调用方自己兜底。
    pub fn from_id(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.id() == s)
    }

    /// 给人看的 agent 名（会话标题、启动横幅、菜单项共用）。
    pub fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Copilot => "GitHub Copilot",
            Self::Codex => "Codex",
            Self::Grok => "Grok",
        }
    }

    /// 短名：会话标题这种窄地方用（「Copilot 对话 · smelt」）。
    pub fn short_label(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Copilot => "Copilot",
            Self::Codex => "Codex",
            Self::Grok => "Grok",
        }
    }

    /// 出厂启动命令。
    pub fn default_cmd(self) -> String {
        match self {
            Self::Claude => default_acp_cmd(),
            Self::Copilot => default_acp_copilot_cmd(),
            Self::Codex => default_acp_codex_cmd(),
            Self::Grok => default_acp_grok_cmd(),
        }
    }
}

pub fn default_acp_cmd() -> String {
    // bunx 由 smelt 解析到受管 bun（~/.smelt/runtime，首次自动下载，见 acp_conn.rs）；
    // 适配器锁版本——方言适配与回归测试都对着这个版本做，升级是主动行为。
    //
    // --bun：强制 bunx 用 bun 自己的运行时执行，不 fallback 到系统 Node——实测
    // 发现这个适配器声明了 `engines.node >= 22`，bunx 默认会尊重这个声明主动
    // 切到系统 Node 去跑（哪怕我们准备了受管 bun），「受管运行时不依赖系统装了
    // 什么」这条设计承诺不加这个 flag 就不成立。见 bunx --help 的官方说明。
    //
    // 锁 0.59.0 不锁最新（0.60.0）：0.60.0 依赖的 zod 4.x 那份 `zod/v4` 子目录
    // 缺 index 文件（exports 声明了 `./v4` 但目录里只有 classic/core/locales/
    // mini 几个子模块，没有入口文件），Node 和 Bun 的解析器都拒绝——是上游
    // （@agentclientprotocol/sdk 的 zod 依赖）的 bug，不是我们能绕开的。0.59.0
    // 依赖的 sdk@1.2.1 自带完整的 zod 4.4.3（`zod/v4/package.json` 齐全），
    // 实测 initialize 握手正常返回、`agentCapabilities.loadSession: true` 也在。
    "bunx --bun @agentclientprotocol/claude-agent-acp@0.59.0".to_string()
}

pub fn default_acp_copilot_cmd() -> String {
    // Copilot CLI 自带 ACP 服务端，不需要适配器：`copilot --help` 里明写
    // `--acp  Start as Agent Client Protocol server`（实测 1.0.73）。
    // 代价是得先装 CLI（`brew install copilot` / npm `@github/copilot`）并
    // `copilot` 登录过——找不到命令时 Fatal 会带上 stderr 说明。
    "copilot --acp".to_string()
}

pub fn default_acp_codex_cmd() -> String {
    // Codex CLI 自己**没有** ACP 入口（`codex --help` 只有 mcp / mcp-server），
    // 走 Zed 维护的适配器包；它按平台分发原生二进制（optionalDependencies），
    // bunx 会自动取对应架构那份。同样锁版本，理由见 default_acp_cmd。
    // 登录态复用 codex CLI 的 `~/.codex`。
    "bunx --bun @zed-industries/codex-acp@0.16.0".to_string()
}

pub fn default_acp_grok_cmd() -> String {
    // Grok CLI 自带 ACP：`grok agent stdio`（help 里只写「Run the agent over
    // stdio」没提协议名，实测发 initialize 能正常握手，agentCapabilities 齐全）。
    // 需先装 grok CLI 并登录（凭据在 ~/.grok/auth.json）。
    //
    // 注意它 `promptCapabilities.image = false`——四家里唯一不收图的，粘贴图片
    // 对 Grok 会话没用。
    "grok agent stdio".to_string()
}

/// 手动添加的一个 workspace：底层还是四家基础 agent 之一，只是换了个数据目录
/// （比如 Claude 的 `CLAUDE_CONFIG_DIR`）。命令不用手填——按 `kind` 的出厂命令
/// 加一段 `ENV=workspace_dir` 前缀自动拼出来（见 `command()`），用户只需要选
/// agent 类型 + 填目录，不用记环境变量名和 shell 语法。
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct AcpProfile {
    /// 稳定 id（新建时生成，不随 label/kind 改变），历史会话页拿它当 tab key、
    /// 区分"同一个 kind 下的哪一个 workspace"。
    pub id: String,
    /// 底层 agent 种类，存 `AcpAgentKind::id()`（跟 `AcpSaved.agent` 同一份规则，
    /// 认不出就回退 Claude）。
    pub kind_id: String,
    /// 设置页 / 历史页 tab 上显示的名字，比如「Claude Quant」。
    pub label: String,
    /// workspace 目录，允许 `~` 开头（展开逻辑跟 build_agent 共用一份，见
    /// `crate::workspace_override`）。
    pub workspace_dir: String,
}

impl AcpProfile {
    pub fn kind(&self) -> AcpAgentKind {
        AcpAgentKind::from_id(&self.kind_id).unwrap_or(AcpAgentKind::Claude)
    }

    /// 该 profile 底层 agent 用来覆盖数据目录的环境变量名。
    pub fn env_var(&self) -> &'static str {
        crate::workspace_override::config_dir_env_var(&self.kind_id).unwrap_or("CLAUDE_CONFIG_DIR")
    }

    /// 自动拼出的完整启动命令：`ENV=workspace_dir <该 kind 的出厂命令>`。
    /// 不持久化——`kind` 的出厂命令以后升级版本号，已存在的 profile 也跟着变，
    /// 不用用户手动同步。
    pub fn command(&self) -> String {
        format!("{}={} {}", self.env_var(), self.workspace_dir, self.kind().default_cmd())
    }
}
