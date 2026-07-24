//! 设置面板：外观 / 桌面宠物 / 启动参数 / 更新 四个分组，含嵌入式设置页
//! （主窗口右上角齿轮）和独立设置窗口共用的渲染逻辑。
//!
//! 跟 git_panel.rs / file_tree.rs 同一个套路：从 main.rs 拆出来的 `impl Workspace`
//! 方法 + 独立类型/函数，字段仍然声明在 main.rs 的 `Workspace` struct 里。
//!
//! 自动更新（`update_status`/`check_for_update`/`upgrade_daemon_seamless` 等）**不在
//! 这里**——那是应用级生命周期状态，不属于任何一个面板，仍留在 main.rs；这里的
//! 「更新」SettingPage 只是读它、展示它、提供按钮触发它。

use std::time::{Duration, Instant};

use gpui::*;
use gpui::prelude::FluentBuilder;
use gpui::InteractiveElement;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::color_picker::{ColorPicker, ColorPickerEvent, ColorPickerState};
use gpui_component::input::Input;
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use gpui_component::notification::Notification;
use gpui_component::progress::Progress;
use gpui_component::radio::{Radio, RadioGroup};
use gpui_component::setting::{
    SelectIndex, Settings, SettingField, SettingGroup, SettingItem, SettingPage,
};
use gpui_component::slider::{Slider, SliderEvent, SliderState, SliderValue};
use gpui_component::*;

use crate::{agent, pet, terminal, terminal_view, updater, Workspace};

// ===================== 外观 / 启动 配置类型 =====================

fn default_theme_mode() -> ThemeMode {
    ThemeMode::Dark
}

/// 老版本 appearance.json 没有 font_px 字段时的回退，跟 terminal_view::FONT_PX_ATOM
/// 的出厂默认值保持一致。
fn default_font_px() -> u32 {
    13
}

/// `bg_color` 从未被用户改过时的出厂值——终端背景层要不要跟着主题模式自动换色，
/// 就看当前值是不是还等于这个（见 `Appearance::bg_color_is_default`）。
const DEFAULT_BG_COLOR: u32 = 0x1a1b26;

/// 终端外观设置（全局单例，供所有终端渲染读取；存 ~/.smelt/appearance.json）。
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Appearance {
    /// 终端底色（0xRRGGBB）。
    pub bg_color: u32,
    /// 背景图片绝对路径（None = 无）。
    pub bg_image: Option<String>,
    /// 不透明度 0.3–1.0；<1 时窗口转透明/模糊，桌面透出。
    pub opacity: f32,
    /// 毛玻璃模糊（macOS vibrancy，配合透明使用）。
    pub blur: bool,
    /// 明暗主题模式。
    #[serde(default = "default_theme_mode")]
    pub theme_mode: ThemeMode,
    /// 终端字号（px）。
    #[serde(default = "default_font_px")]
    pub font_px: u32,
    /// 终端字体族。空 = 出厂默认（terminal_view::DEFAULT_FONT_FAMILY）；填了但机器上
    /// 没装时，渲染/测量会一致地落到 Menlo 兜底（见 terminal_view::terminal_font）。
    #[serde(default)]
    pub font_family: String,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            bg_color: DEFAULT_BG_COLOR,
            bg_image: None,
            opacity: 1.0,
            blur: false,
            theme_mode: ThemeMode::Dark,
            font_px: default_font_px(),
            font_family: String::new(),
        }
    }
}

impl Global for Appearance {}

impl Appearance {
    /// 据当前设置推导窗口背景外观。
    pub fn window_bg(&self) -> WindowBackgroundAppearance {
        if self.blur {
            WindowBackgroundAppearance::Blurred
        } else if self.opacity < 1.0 {
            WindowBackgroundAppearance::Transparent
        } else {
            WindowBackgroundAppearance::Opaque
        }
    }

    /// `bg_color` 是否还是没被用户碰过的出厂值。是的话终端背景层该跟主题模式自动
    /// 切换（见 terminal_view.rs 的 bg_layer）；用户显式选过颜色后就不再跟随，
    /// 保留其选择（深浅色模式来回切也不丢）。
    pub fn bg_color_is_default(&self) -> bool {
        self.bg_color == DEFAULT_BG_COLOR
    }
}

/// 把主题模式落到所有吃颜色的层：gpui-component 部件、自绘 UI 语义色板、终端调色板。
/// **唯一入口**——三处必须同时切，漏一处就是「面板变浅了但终端还是黑的」这种半吊子。
/// 只改全局态不重绘，调用方自己决定什么时候 `cx.refresh_windows()`
/// （启动时还没有窗口，切换时才需要）。
pub fn apply_theme_mode(mode: ThemeMode, cx: &mut App) {
    Theme::change(mode, None, cx);
    crate::ui_theme::set_light(!mode.is_dark());
    terminal::set_dark_mode(mode.is_dark());
    // Theme::change 装的是组件库自带色板，跟 ui_theme 是两套值——同屏里
    // `t.border` 和 `ui_theme::border_mid()` 挨着出现就会差一档。这里按语义位
    // 把组件库主题覆写成 ui_theme 的值，色真源收敛成一个。
    // 覆写必须在 Theme::change 之后：它会整套 apply_config 覆盖回默认。
    crate::ui_theme::apply_to_component_theme(cx);
}

/// 外观设置文件路径：~/.smelt/appearance.json。
fn appearance_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".smelt").join("appearance.json"))
}

/// 读取外观设置；缺失/损坏回退默认。
pub fn load_appearance() -> Appearance {
    crate::json_store::load_json(appearance_path())
}

/// 写回外观设置（失败静默忽略）。
fn save_appearance(a: &Appearance) {
    crate::json_store::save_json(appearance_path(), a)
}

/// 项目行「+」下拉菜单里的一条可配置启动项：显示名 + shell 启动命令。
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct LaunchEntry {
    pub label: String,
    pub command: String,
}

/// 出厂默认启动项：与当前常用配置对齐（各 agent 默认带全权限参数）。
/// 用户可在设置里增删改；需要更保守时把参数删掉即可。
/// 「继续上次」不放默认里，需要的人自己在设置里加。
pub fn default_launch_entries() -> Vec<LaunchEntry> {
    vec![
        LaunchEntry {
            label: "Claude Code".into(),
            command: "claude --dangerously-skip-permissions".into(),
        },
        LaunchEntry {
            label: "Codex".into(),
            command: "codex --dangerously-bypass-approvals-and-sandbox".into(),
        },
        LaunchEntry {
            label: "Copilot".into(),
            command: "copilot --allow-all".into(),
        },
    ]
}

/// 项目行「+」可配置启动项列表（全局单例，存 ~/.smelt/launch.json）。
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct LaunchConfig {
    /// 除固定的「新建终端」「新建 Worktree…」外，下拉菜单里的启动项。
    pub entries: Vec<LaunchEntry>,
}

impl Default for LaunchConfig {
    fn default() -> Self {
        Self {
            entries: default_launch_entries(),
        }
    }
}

/// 按命令前缀猜侧栏/菜单图标（自定义 agent 走通用终端图标）。
pub fn icon_for_launch_command(command: &str) -> IconName {
    let cmd = command.trim();
    if cmd.starts_with("claude") {
        IconName::Asterisk
    } else if cmd.starts_with("codex") {
        IconName::Bot
    } else if cmd.starts_with("copilot") {
        IconName::Github
    } else {
        IconName::SquareTerminal
    }
}

/// 过滤出可展示的启动项（名/命令非空）。
pub fn active_launch_entries(cx: &App) -> Vec<LaunchEntry> {
    cx.global::<LaunchConfig>()
        .entries
        .iter()
        .filter(|e| !e.label.trim().is_empty() && !e.command.trim().is_empty())
        .cloned()
        .collect()
}

impl Global for LaunchConfig {}

fn launch_config_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".smelt").join("launch.json"))
}

/// 磁盘上的原始形状：兼容旧版「全权限」三开关，也兼容新版 `entries` 列表。
/// `entries: None` 表示文件里没写这个键（旧格式）→ 迁到出厂默认并回写；
/// `Some([])` 表示用户清空了列表，照用。
#[derive(serde::Deserialize)]
struct LaunchConfigFile {
    #[serde(default)]
    entries: Option<Vec<LaunchEntry>>,
}

/// 读取启动配置；缺失/损坏/旧格式（无 `entries`）回退出厂默认并写成新格式。
pub fn load_launch_config() -> LaunchConfig {
    let Some(path) = launch_config_path() else {
        return LaunchConfig::default();
    };
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return LaunchConfig::default();
    };
    let Ok(file) = serde_json::from_str::<LaunchConfigFile>(&raw) else {
        return LaunchConfig::default();
    };
    match file.entries {
        Some(entries) => LaunchConfig { entries },
        None => {
            // 旧版只有全权限开关：直接用出厂默认（已含全权限参数）并回写。
            let c = LaunchConfig::default();
            save_launch_config(&c);
            c
        }
    }
}

/// 写回启动配置（失败静默忽略）。
fn save_launch_config(c: &LaunchConfig) {
    crate::json_store::save_json(launch_config_path(), c)
}

/// 改启动配置全局 + 存盘，不触发 view 重绘，用法同 [`apply_appearance`]。
fn apply_launch_config(f: impl FnOnce(&mut LaunchConfig), cx: &mut App) {
    let mut c = cx.global::<LaunchConfig>().clone();
    f(&mut c);
    save_launch_config(&c);
    cx.set_global(c);
}

// ===================== Agent UI / Claude hooks（B 路线） =====================
//
// AcpAgentKind / AcpProfile 搬进 smelt-core（本身不需要 GPUI），AgentUiConfig
// （需要 `gpui::Global`）搬进 smelt-ui——都是 acp_view.rs 独立成 smelt-acp-view
// crate 之后要跨 crate 共用的数据模型。这里重导出成原来的裸名字，本文件剩下
// 的 UI 渲染代码（acp_cmd_setting_item、手动添加 workspace 的编辑器等）不用
// 逐处改路径。
pub use smelt_core::agent_kind::{AcpAgentKind, AcpProfile};
pub use smelt_ui::agent_ui_config::{apply_agent_ui, load_agent_ui_config, AgentUiConfig};

/// 全局配置里某个 agent 的启动命令；配置还没装载就退回出厂值。
pub fn acp_cmd_for(agent: AcpAgentKind, cx: &App) -> String {
    cx.try_global::<AgentUiConfig>()
        .map(|c| c.acp_cmd_for(agent))
        .unwrap_or_else(|| agent.default_cmd())
}

/// 设置页「Agent 集成」里每个 agent 一条启动命令输入框（从枚举派生，加一家
/// agent 不用回来抄第四遍）。
fn acp_cmd_setting_item(agent: AcpAgentKind) -> SettingItem {
    SettingItem::new(
        format!("{} 启动命令", agent.label()),
        SettingField::input(
            move |cx: &App| acp_cmd_for(agent, cx).into(),
            move |v: SharedString, cx: &mut App| {
                let v = v.trim().to_string();
                // 留空 = 恢复该 agent 的出厂命令（不是清成空串跑不起来）。
                let cmd = if v.is_empty() { agent.default_cmd() } else { v };
                apply_agent_ui(move |c| c.set_acp_cmd_for(agent, cmd), cx);
            },
        ),
    )
    .description(format!(
        "「{}」对话会话的 agent 启动命令（ACP 协议，空白分词）。\
         留空恢复默认；改动只影响之后新建的会话。",
        agent.label()
    ))
    .keywords(["acp", "对话", "agent", agent.id()])
}

/// smelt-notify 安装路径（与 package/安装脚本约定一致）。
pub fn smelt_notify_path() -> std::path::PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| "/tmp".into())
        .join(".smelt")
        .join("bin")
        .join("smelt-notify")
}

fn claude_settings_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("settings.json"))
}

const SMELT_HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "PermissionRequest",
    "Notification",
    "UserPromptSubmit",
    "SubagentStart",
    "SubagentStop",
    "Stop",
    "StopFailure",
    "SessionEnd",
];

/// Claude hooks 是否已装上 smelt-notify（任一事件含该 command 即视为已装）。
pub fn claude_hooks_installed() -> bool {
    let Some(path) = claude_settings_path() else {
        return false;
    };
    let Ok(raw) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return false;
    };
    let notify = smelt_notify_path();
    let notify_s = notify.to_string_lossy();
    let Some(hooks) = v.get("hooks").and_then(|h| h.as_object()) else {
        return false;
    };
    for ev in SMELT_HOOK_EVENTS {
        let Some(arr) = hooks.get(*ev).and_then(|x| x.as_array()) else {
            continue;
        };
        for m in arr {
            let Some(hs) = m.get("hooks").and_then(|x| x.as_array()) else {
                continue;
            };
            for h in hs {
                if h.get("command")
                    .and_then(|c| c.as_str())
                    .is_some_and(|c| c == notify_s || c.ends_with("/smelt-notify"))
                {
                    return true;
                }
            }
        }
    }
    false
}

/// 把 smelt-notify 写入 ~/.claude/settings.json（幂等）；成功返回 Ok。
pub fn install_claude_hooks() -> Result<(), String> {
    let notify = smelt_notify_path();
    if !notify.is_file() {
        return Err(format!(
            "找不到 {}，请先编译安装 smelt-notify",
            notify.display()
        ));
    }
    let path = claude_settings_path().ok_or_else(|| "无 home 目录".to_string())?;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut root: serde_json::Value = if path.is_file() {
        let raw = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        serde_json::from_str(&raw).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };
    let hooks = root
        .as_object_mut()
        .ok_or_else(|| "settings.json 根不是对象".to_string())?
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}));
    let hooks_obj = hooks
        .as_object_mut()
        .ok_or_else(|| "hooks 不是对象".to_string())?;
    let cmd = notify.to_string_lossy().to_string();
    let entry = serde_json::json!({
        "type": "command",
        "command": cmd,
    });
    for ev in SMELT_HOOK_EVENTS {
        let arr = hooks_obj
            .entry(*ev)
            .or_insert_with(|| serde_json::json!([]));
        let list = arr
            .as_array_mut()
            .ok_or_else(|| format!("hooks.{ev} 不是数组"))?;
        // 已有 smelt-notify 则跳过
        let mut found = false;
        for m in list.iter_mut() {
            if let Some(hs) = m.get_mut("hooks").and_then(|h| h.as_array_mut()) {
                if hs.iter().any(|h| {
                    h.get("command")
                        .and_then(|c| c.as_str())
                        .is_some_and(|c| c.ends_with("smelt-notify"))
                }) {
                    found = true;
                    break;
                }
            }
        }
        if !found {
            list.push(serde_json::json!({
                "matcher": "",
                "hooks": [entry.clone()],
            }));
        }
    }
    let out = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
    std::fs::write(&path, out + "\n").map_err(|e| e.to_string())?;
    Ok(())
}

/// 从 Claude settings 移除 smelt-notify hooks（其它 hook 保留）。
pub fn uninstall_claude_hooks() -> Result<(), String> {
    let path = claude_settings_path().ok_or_else(|| "无 home 目录".to_string())?;
    if !path.is_file() {
        return Ok(());
    }
    let raw = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let mut root: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| e.to_string())?;
    let Some(hooks) = root.get_mut("hooks").and_then(|h| h.as_object_mut()) else {
        return Ok(());
    };
    for ev in SMELT_HOOK_EVENTS {
        let Some(arr) = hooks.get_mut(*ev).and_then(|x| x.as_array_mut()) else {
            continue;
        };
        arr.retain_mut(|m| {
            let Some(hs) = m.get_mut("hooks").and_then(|h| h.as_array_mut()) else {
                return true;
            };
            hs.retain(|h| {
                !h.get("command")
                    .and_then(|c| c.as_str())
                    .is_some_and(|c| c.ends_with("smelt-notify"))
            });
            // 空 matcher 且 hooks 空则整段删
            !(hs.is_empty()
                && m.get("matcher")
                    .and_then(|x| x.as_str())
                    .is_some_and(|s| s.is_empty()))
        });
        if arr.is_empty() {
            hooks.remove(*ev);
        }
    }
    let out = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
    std::fs::write(&path, out + "\n").map_err(|e| e.to_string())?;
    Ok(())
}

// ===================== 远程操作网关（见 docs/remote-ops-roadmap.md） =====================

/// 远程操作网关的持久化开关（全局单例，存 ~/.smelt/collab.json）。只记「用户希望
/// 它是开是关」这一件事——具体的 token/绑定地址是运行时状态，不落盘（见
/// [`RemoteRuntimeState`]），GUI 启动时按这个字段决定要不要主动 remote_start。
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct RemoteConfig {
    pub enabled: bool,
    /// Cloudflare Tunnel 开关（Phase 3，见 smeltd.rs）。`#[serde(default)]`：
    /// 这个字段比 `enabled` 晚加，旧的 collab.json 里没有这个键，缺省按关闭处理。
    #[serde(default)]
    pub tunnel_enabled: bool,
    /// 跨网主路径：WebRTC + 自营信令（docs/webrtc-edge.md）。
    #[serde(default)]
    pub webrtc_enabled: bool,
    /// 公网信令 HTTP 根（SPA 同域），如 `https://signal.example.com`。
    /// **无内置默认域名**——须用户在设置里填写自己部署的 smelt-signal。
    #[serde(default)]
    pub signal_http: String,
    /// 这条链接是否允许 approve/deny/reply（Phase 6，见 smeltd.rs「远程操控」）。
    /// `#[serde(default)]`：比前两个字段更晚加，旧配置缺省按只读处理——不能让
    /// 老用户的配置在升级后突然变成可写。链接分享出去本身就是授权，这里没有
    /// 额外的"当面确认"一说，开这个开关前的取舍由用户自己判断。
    #[serde(default)]
    pub write_enabled: bool,
}

impl Default for RemoteConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            tunnel_enabled: false,
            webrtc_enabled: false,
            signal_http: String::new(),
            write_enabled: false,
        }
    }
}

/// 规范化用户填的信令地址：去空白、去尾 `/`。空字符串表示未配置。
pub fn normalize_signal_http(raw: &str) -> String {
    raw.trim().trim_end_matches('/').to_string()
}

/// 是否是可用来建房的 http(s) 信令根。
fn signal_http_ok(url: &str) -> bool {
    let u = normalize_signal_http(url);
    !u.is_empty() && (u.starts_with("https://") || u.starts_with("http://"))
}

impl Global for RemoteConfig {}

fn remote_config_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".smelt").join("collab.json"))
}

/// 读取远程网关开关；缺失/损坏回退默认（关闭）。
pub fn load_remote_config() -> RemoteConfig {
    crate::json_store::load_json(remote_config_path())
}

fn save_remote_config(c: &RemoteConfig) {
    crate::json_store::save_json(remote_config_path(), c)
}

/// 内嵌远程网关的运行时状态（不落盘，纯展示用）：token/绑定地址是当次 remote_start
/// 成功后守护回的实际值；error 是启动失败时的原因（比如端口被占）。
#[derive(Clone, Default)]
pub struct RemoteRuntimeState {
    pub token: Option<String>,
    pub addr: Option<String>,
    pub error: Option<String>,
    /// 当前这条链接是否可写——来自守护对 `remote_start`/`remote_status` 的实际
    /// 回执，不是直接照抄 [`RemoteConfig::write_enabled`]（写权限是烤进 token
    /// 里的，配置改了但网关还没重开时，这里应该继续显示"旧链接"的真实权限）。
    pub write: bool,
}

impl Global for RemoteRuntimeState {}

fn set_remote_from_start_result(result: Result<terminal::RemoteStatus, String>, cx: &mut App) {
    match result {
        Ok(s) => cx.set_global(RemoteRuntimeState {
            token: s.token,
            addr: s.addr,
            write: s.write,
            error: None,
        }),
        Err(e) => cx.set_global(RemoteRuntimeState {
            token: None,
            addr: None,
            write: false,
            error: Some(e),
        }),
    }
}

/// 异步拉起 / 刷新 Cloudflare 隧道，并在结束后用守护现状回填远程 token。
/// 失败时写进 TunnelRuntimeState.error，**不**要求用户记「先关后开」步骤——
/// 设置页会给一键「重试」。
fn spawn_tunnel_start(write: bool, cx: &mut App) {
    cx.set_global(TunnelRuntimeState {
        connecting: true,
        url: None,
        error: None,
        write: false,
    });
    cx.spawn(async move |cx| {
        let result = cx
            .background_executor()
            .spawn(async move { terminal::tunnel_start(write) })
            .await;
        let _ = cx.update(|cx| {
            let remote = terminal::remote_status();
            let has_token = remote.token.as_ref().is_some_and(|t| !t.is_empty());
            cx.set_global(RemoteRuntimeState {
                token: remote.token.clone(),
                addr: remote.addr,
                write: remote.write,
                error: None,
            });
            let rt = match result {
                Ok(status) if has_token => TunnelRuntimeState {
                    connecting: false,
                    url: status.url,
                    error: None,
                    write: status.write,
                },
                Ok(_) => TunnelRuntimeState {
                    connecting: false,
                    url: None,
                    error: Some("外网通道建好了，但分享密钥还没就绪，点下方重试即可".into()),
                    write: false,
                },
                Err(e) => TunnelRuntimeState {
                    connecting: false,
                    url: None,
                    error: Some(e),
                    write: false,
                },
            };
            cx.set_global(rt);
        });
    })
    .detach();
}

/// 总开关：开启远程。关掉时自动拆掉隧道 / WebRTC 桥，用户不必先关外网再关远程。
pub fn apply_remote_toggle(enabled: bool, cx: &mut App) {
    if enabled {
        let c = cx.global::<RemoteConfig>().clone();
        let write = c.write_enabled;
        let want_tunnel = c.tunnel_enabled;
        let want_webrtc = c.webrtc_enabled;
        set_remote_from_start_result(terminal::remote_start("127.0.0.1", write), cx);
        let mut c = cx.global::<RemoteConfig>().clone();
        c.enabled = true;
        save_remote_config(&c);
        cx.set_global(c);
        // 若用户是点「手机/外网」间接打开的，want_tunnel 已是 true，这里补上隧道。
        if want_tunnel {
            spawn_tunnel_start(write, cx);
        }
        if want_webrtc {
            spawn_webrtc_start(cx);
        }
    } else {
        stop_webrtc_bridge(cx);
        terminal::tunnel_stop();
        terminal::remote_stop();
        cx.set_global(TunnelRuntimeState::default());
        cx.set_global(RemoteRuntimeState::default());
        cx.set_global(WebrtcRuntimeState::default());
        let mut c = cx.global::<RemoteConfig>().clone();
        c.enabled = false;
        // 总开关关掉 = 停止分享。外网开关一并熄灭，避免「远程关了但手机访问还亮着」
        // 的误解；写入偏好保留，下次再开远程仍按原权限。
        c.tunnel_enabled = false;
        c.webrtc_enabled = false;
        save_remote_config(&c);
        cx.set_global(c);
    }
}

// ===================== WebRTC 跨网（smelt-bridge + 公网信令） =====================

/// WebRTC 跨网运行时（不落盘）：bridge 子进程 + 分享 URL + 二维码 PNG。
#[derive(Clone, Default)]
pub struct WebrtcRuntimeState {
    pub connecting: bool,
    pub share_url: Option<String>,
    pub error: Option<String>,
    /// bridge 进程 pid，关掉开关时 SIGKILL（GUI 拉起的子进程实测不响应
    /// SIGTERM，大概率继承了后台执行器线程的阻塞信号掩码；SIGKILL 谁都挡不住）
    pub bridge_pid: Option<u32>,
    /// 分享链接的 QR（PNG 字节），URL 变了才重算
    pub qr_png: Option<Vec<u8>>,
    /// 每次 spawn_webrtc_start/stop_webrtc_bridge 递增。后台任务落地结果前先
    /// 核对自己出发时捕获的世代还是不是当前——不是就说明中途被另一次调用取代
    /// 了（比如 stop 提前把 connecting 清成 false，重入锁没拦住），这次的结果
    /// （包括刚拉起来的 bridge 子进程）要整个丢弃，不能再注册成"当前"状态。
    pub generation: u64,
}

impl Global for WebrtcRuntimeState {}

/// 信令地址「探测连通」结果（不落盘，设置页展示）。
#[derive(Clone, Default)]
pub struct SignalProbeState {
    pub probing: bool,
    /// 最近一次探测的目标 URL（与当前输入对比时可提示）
    #[allow(dead_code)]
    pub url: Option<String>,
    pub ok: Option<bool>,
    pub message: Option<String>,
}

impl Global for SignalProbeState {}

/// 从输入框读出并规范化信令地址；空则 `None`。
fn signal_http_from_input(
    input: Option<&Entity<gpui_component::input::InputState>>,
    cx: &App,
) -> String {
    if let Some(s) = input {
        return normalize_signal_http(&s.read(cx).value());
    }
    normalize_signal_http(
        &cx.try_global::<RemoteConfig>()
            .map(|c| c.signal_http.clone())
            .unwrap_or_default(),
    )
}

/// 保存信令地址到配置；若跨网已开且地址变化，可选重启 bridge。
fn apply_signal_http(url: String, restart_if_webrtc: bool, cx: &mut App) {
    let url = normalize_signal_http(&url);
    let mut c = cx.global::<RemoteConfig>().clone();
    let changed = c.signal_http != url;
    c.signal_http = url.clone();
    save_remote_config(&c);
    cx.set_global(c.clone());
    // 清掉旧探测结果（目标变了）
    if changed {
        cx.set_global(SignalProbeState::default());
    }
    if restart_if_webrtc && changed && c.webrtc_enabled && signal_http_ok(&url) {
        stop_webrtc_bridge(cx);
        spawn_webrtc_start(cx);
    }
    cx.refresh_windows();
}

/// 后台 GET `{url}/health`，更新 [`SignalProbeState`]。
fn probe_signal_http(url: String, cx: &mut App) {
    let url = normalize_signal_http(&url);
    if !signal_http_ok(&url) {
        cx.set_global(SignalProbeState {
            probing: false,
            url: Some(url),
            ok: Some(false),
            message: Some("请填写以 http:// 或 https:// 开头的地址".into()),
        });
        cx.refresh_windows();
        return;
    }
    cx.set_global(SignalProbeState {
        probing: true,
        url: Some(url.clone()),
        ok: None,
        message: Some("探测中…".into()),
    });
    cx.refresh_windows();
    cx.spawn(async move |cx| {
        let probe_url = url.clone();
        let result: Result<String, String> = cx
            .background_executor()
            .spawn(async move {
                // block_on_tokio 的 T 是 Future::Output；若 Output 本身是 Result 会套两层。
                match smelt_core::block_on::block_on_tokio(async move {
                    let client = reqwest::Client::builder()
                        .timeout(Duration::from_secs(8))
                        .build()
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                    let health = format!("{probe_url}/health");
                    let resp = client
                        .get(&health)
                        .send()
                        .await
                        .map_err(|e| anyhow::anyhow!("连不上：{e}"))?;
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    if !status.is_success() {
                        anyhow::bail!(
                            "HTTP {status}：{}",
                            body.chars().take(80).collect::<String>()
                        );
                    }
                    // 兼容 { ok: true, rooms: n }
                    if body.contains("\"ok\"") && body.contains("true") {
                        Ok::<String, anyhow::Error>(format!("连通正常 · {body}"))
                    } else {
                        Ok(format!(
                            "已响应 HTTP {status} · {}",
                            body.chars().take(100).collect::<String>()
                        ))
                    }
                }) {
                    Ok(Ok(msg)) => Ok(msg),
                    Ok(Err(e)) => Err(e.to_string()),
                    Err(e) => Err(e.to_string()),
                }
            })
            .await;
        let _ = cx.update(|cx| {
            match result {
                Ok(msg) => {
                    cx.set_global(SignalProbeState {
                        probing: false,
                        url: Some(url),
                        ok: Some(true),
                        message: Some(msg),
                    });
                }
                Err(e) => {
                    cx.set_global(SignalProbeState {
                        probing: false,
                        url: Some(url),
                        ok: Some(false),
                        message: Some(e),
                    });
                }
            }
            cx.refresh_windows();
        });
    })
    .detach();
}

fn resolve_smelt_bridge() -> Option<std::path::PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("smelt-bridge");
            if p.is_file() {
                return Some(p);
            }
            // 开发：…/target/debug/smelt 旁
            let p2 = dir.join("smelt-bridge");
            if p2.is_file() {
                return Some(p2);
            }
        }
    }
    let dev = std::path::PathBuf::from("target/debug/smelt-bridge");
    if dev.is_file() {
        return Some(dev);
    }
    let dev_rel = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug/smelt-bridge");
    if dev_rel.is_file() {
        return Some(dev_rel);
    }
    None
}

/// `~/.smelt/smelt-bridge.log`，每次 spawn 前截断重开。拿不到 home 目录时返回
/// `None`，调用方回退到 `/dev/null`（不让日志问题阻塞跨网功能本身）。
fn bridge_log_file() -> Option<std::fs::File> {
    let path = dirs::home_dir()?.join(".smelt").join("smelt-bridge.log");
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::File::create(&path).ok()
}

/// 生成 RGB PNG（避免 L8 灰度图在 GPUI/Metal 解码时 abort）。
fn qr_png_for_url(url: &str) -> Option<Vec<u8>> {
    use qrcode::QrCode;
    let code = QrCode::new(url.as_bytes()).ok()?;
    let luma = code
        .render::<image::Luma<u8>>()
        .dark_color(image::Luma([0u8]))
        .light_color(image::Luma([255u8]))
        .min_dimensions(160, 160)
        .quiet_zone(true)
        .build();
    let rgb = image::DynamicImage::ImageLuma8(luma).into_rgb8();
    let mut buf = Vec::new();
    rgb.write_to(
        &mut std::io::Cursor::new(&mut buf),
        image::ImageFormat::Png,
    )
    .ok()?;
    Some(buf)
}

/// 下一个世代号：任何要让"正在飞的 spawn_webrtc_start"作废的地方都调这个。
fn next_webrtc_generation(cx: &App) -> u64 {
    cx.try_global::<WebrtcRuntimeState>()
        .map(|s| s.generation)
        .unwrap_or(0)
        .wrapping_add(1)
}

fn stop_webrtc_bridge(cx: &mut App) {
    let next_gen = next_webrtc_generation(cx);
    if let Some(pid) = cx
        .try_global::<WebrtcRuntimeState>()
        .and_then(|s| s.bridge_pid)
    {
        // 只杀我们拉起的 bridge，勿误伤自己
        let self_pid = std::process::id();
        if pid != 0 && pid != self_pid {
            #[cfg(unix)]
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
        }
    }
    // 世代 +1：正在飞的 spawn_webrtc_start（如果有）落地时会发现自己的世代
    // 过期，把结果（包括它刚拉起来的 bridge 进程）整个丢弃，不会跟这次 stop
    // 打架。
    cx.set_global(WebrtcRuntimeState {
        generation: next_gen,
        ..Default::default()
    });
}

/// 开关「跨网 WebRTC」：只改配置 + 异步拉 bridge（不在 UI 线程阻塞/建房）。
pub fn apply_webrtc_toggle(enabled: bool, cx: &mut App) {
    let mut c = cx.global::<RemoteConfig>().clone();

    if enabled {
        let signal = normalize_signal_http(&c.signal_http);
        if !signal_http_ok(&signal) {
            // 未配置信令：不打开开关，提示用户先填地址
            c.webrtc_enabled = false;
            save_remote_config(&c);
            cx.set_global(c);
            let next_gen = next_webrtc_generation(cx);
            cx.set_global(WebrtcRuntimeState {
                connecting: false,
                error: Some(
                    "请先填写「信令服务地址」（你部署的 smelt-signal，如 https://signal.example.com）"
                        .into(),
                ),
                generation: next_gen,
                ..Default::default()
            });
            cx.refresh_windows();
            return;
        }
        c.signal_http = signal;
        c.webrtc_enabled = true;
        c.enabled = true;
    } else {
        c.webrtc_enabled = false;
    }
    save_remote_config(&c);
    cx.set_global(c);

    if !enabled {
        stop_webrtc_bridge(cx);
        cx.refresh_windows();
        return;
    }

    // 全部放到后台：remote_start / HTTP 建房 / spawn bridge，避免卡 UI 或跨 FFI panic
    spawn_webrtc_start(cx);
    cx.refresh_windows();
}

/// 供 main 启动恢复调用（网关 hydrate 之后）。
pub fn spawn_webrtc_start_public(cx: &mut App) {
    spawn_webrtc_start(cx);
}

/// 供 main 的 on_app_quit 钩子调用：App 直接 Cmd+Q 退出（没有先手动关开关）时，
/// bridge 子进程不会自己退出——之前只有「关开关」这条路径会杀它，直接退出 App
/// 完全没人管，子进程被系统收养成孤儿，一直占着信令服务器上的房间直到 TTL 到期。
pub fn stop_webrtc_bridge_on_quit(cx: &mut App) {
    stop_webrtc_bridge(cx);
}

fn spawn_webrtc_start(cx: &mut App) {
    // 重入防护：上一轮还在 connecting（后台 remote_start/建房/spawn bridge 没
    // 完成）时再来一次，不能再起一条独立的异步链——两条链各自读 existing_token
    // 时都可能看到"还没有"，各自去 remote_start，实测真的会各建一个网关、各拉
    // 一个 bridge，两边互相踩，表现为间歇性连不上本机网关。开关连点两下、或
    // app 启动恢复跟用户手动开关撞一起都可能触发。
    if cx
        .try_global::<WebrtcRuntimeState>()
        .is_some_and(|s| s.connecting)
    {
        eprintln!("[webrtc] spawn_webrtc_start: already connecting, skip duplicate call");
        return;
    }

    // 先停旧 bridge（清 pid）
    if let Some(old) = cx.try_global::<WebrtcRuntimeState>().cloned() {
        if let Some(pid) = old.bridge_pid {
            let self_pid = std::process::id();
            if pid != 0 && pid != self_pid {
                #[cfg(unix)]
                unsafe {
                    libc::kill(pid as i32, libc::SIGKILL);
                }
            }
        }
    }

    let cfg = cx.global::<RemoteConfig>().clone();
    let existing_token = cx
        .try_global::<RemoteRuntimeState>()
        .and_then(|r| r.token.clone())
        .filter(|t| !t.is_empty());
    let existing_addr = cx
        .try_global::<RemoteRuntimeState>()
        .and_then(|r| r.addr.clone());
    let write = cfg.write_enabled;
    let signal_http = normalize_signal_http(&cfg.signal_http);
    let bridge_bin = resolve_smelt_bridge();

    if !signal_http_ok(&signal_http) {
        let next_gen = next_webrtc_generation(cx);
        cx.set_global(WebrtcRuntimeState {
            connecting: false,
            error: Some(
                "未配置信令服务地址。请在设置 → 远程 填写你的 smelt-signal URL。".into(),
            ),
            generation: next_gen,
            ..Default::default()
        });
        return;
    }

    let my_gen = next_webrtc_generation(cx);
    cx.set_global(WebrtcRuntimeState {
        connecting: true,
        generation: my_gen,
        ..Default::default()
    });

    cx.spawn(async move |cx| {
        // webrtc_start_blocking 里也有同步的 remote_start/进程 spawn，不止 reqwest 那段，
        // 所以这里仍额外兜一层 catch_unwind（block_on_tokio 只包它内部的 reqwest 部分）。
        let result = cx
            .background_executor()
            .spawn(async move {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    webrtc_start_blocking(
                        existing_token,
                        existing_addr,
                        write,
                        signal_http,
                        bridge_bin,
                    )
                }))
                .unwrap_or_else(|payload| {
                    let msg = if let Some(s) = payload.downcast_ref::<&str>() {
                        (*s).to_string()
                    } else if let Some(s) = payload.downcast_ref::<String>() {
                        s.clone()
                    } else {
                        "WebRTC 启动过程中发生内部错误".into()
                    };
                    Err(format!("跨网启动崩溃（已拦截）：{msg}"))
                })
            })
            .await;

        let _ = cx.update(|cx| {
            // 落地前先核对世代：这段后台任务跑的这几秒里，如果又有人调用过
            // spawn_webrtc_start/stop_webrtc_bridge，世代会被推进，说明这次
            // 的结果已经过期，不能再当"当前状态"写回去——尤其是 Ok 分支，
            // 刚拉起来的 bridge 子进程也得直接杀掉，不然就是又一个孤儿进程、
            // 又一个没人管的本机网关。
            let current_gen = cx.global::<WebrtcRuntimeState>().generation;
            if current_gen != my_gen {
                if let Ok((_, pid, ..)) = &result {
                    let self_pid = std::process::id();
                    if *pid != 0 && *pid != self_pid {
                        #[cfg(unix)]
                        unsafe {
                            libc::kill(*pid as i32, libc::SIGKILL);
                        }
                    }
                }
                return;
            }
            match result {
                Ok((share, pid, qr, token, addr, write)) => {
                    // 回填网关状态（可能是这次才 remote_start 的）
                    cx.set_global(RemoteRuntimeState {
                        token: Some(token),
                        addr,
                        write,
                        error: None,
                    });
                    cx.set_global(WebrtcRuntimeState {
                        connecting: false,
                        share_url: Some(share),
                        error: None,
                        bridge_pid: Some(pid),
                        qr_png: qr,
                        generation: my_gen,
                    });
                }
                Err(e) => {
                    cx.set_global(WebrtcRuntimeState {
                        connecting: false,
                        share_url: None,
                        error: Some(e),
                        bridge_pid: None,
                        qr_png: None,
                        generation: my_gen,
                    });
                }
            }
            cx.refresh_windows();
        });
    })
    .detach();
}

/// 在后台线程同步跑完整条 WebRTC 启动链（网关 → 信令建房 → bridge → QR）。
/// reqwest 调用经 [`smelt_core::block_on::block_on_tokio`] 跑，避免「no reactor」。
fn webrtc_start_blocking(
    existing_token: Option<String>,
    existing_addr: Option<String>,
    write: bool,
    signal_http: String,
    bridge_bin: Option<std::path::PathBuf>,
) -> Result<(String, u32, Option<Vec<u8>>, String, Option<String>, bool), String> {
    // 0) 本机远程网关（阻塞 unix socket）
    let status = if let Some(t) = existing_token {
        let cur = terminal::remote_status();
        if cur.token.as_ref().is_some_and(|x| !x.is_empty()) {
            cur
        } else {
            terminal::RemoteStatus {
                running: true,
                token: Some(t),
                addr: existing_addr,
                write,
            }
        }
    } else {
        terminal::remote_start("127.0.0.1", write)
            .map_err(|e| format!("开启本机远程失败：{e}"))?
    };
    let token = status
        .token
        .filter(|t| !t.is_empty())
        .ok_or_else(|| "本机远程网关没有 token".to_string())?;
    let gateway_base = status
        .addr
        .as_ref()
        .map(|a| {
            if a.starts_with("http") {
                a.clone()
            } else {
                format!("http://{a}")
            }
        })
        .unwrap_or_else(|| "http://127.0.0.1:18765".into());

    let Some(bridge) = bridge_bin else {
        return Err(
            "找不到 smelt-bridge。请 make dist-build 安装，或 cargo build -p smelt-bridge。"
                .into(),
        );
    };

    if !signal_http_ok(&signal_http) {
        return Err("未配置信令服务地址".into());
    }

    // 1) 公网信令建房（reqwest 需要 tokio runtime）
    #[derive(serde::Deserialize)]
    struct Room {
        room: String,
        secret: String,
    }
    let room: Room = {
        let signal_http = signal_http.clone();
        smelt_core::block_on::block_on_tokio(async move {
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()
                .map_err(|e| e.to_string())?;
            let room_url = format!("{signal_http}/v1/rooms");
            let resp = client
                .post(&room_url)
                .header("content-type", "application/json")
                .body("{}")
                .send()
                .await
                .map_err(|e| format!("连信令失败（{signal_http}）：{e}"))?;
            if !resp.status().is_success() {
                return Err(format!(
                    "建房失败 HTTP {}：{}",
                    resp.status(),
                    resp.text().await.unwrap_or_default()
                ));
            }
            resp.json::<Room>().await.map_err(|e| e.to_string())
        })
        .map_err(|e| e.to_string())
        .and_then(|r| r)?
    };

    let signal_ws = {
        let u = signal_http
            .replacen("https://", "wss://", 1)
            .replacen("http://", "ws://", 1);
        format!("{u}/ws")
    };
    let share = format!(
        "{signal_http}/?room={}&k={}&signal={}&token={}",
        urlencoding_minimal(&room.room),
        urlencoding_minimal(&room.secret),
        urlencoding_minimal(&signal_ws),
        urlencoding_minimal(&token),
    );

    // 2) 拉起 bridge。stdout/stderr 落盘到 ~/.smelt/smelt-bridge.log（每次启动截断）——
    // 此前直接扔 /dev/null，ICE/DataChannel 连不上时完全没法从日志排查，只能看现象。
    let mut cmd = std::process::Command::new(&bridge);
    cmd.env("SMELT_SIGNAL_HTTP", &signal_http)
        .env("SMELT_SIGNAL_WS", &signal_ws)
        .env("SMELT_GATEWAY", &gateway_base)
        .env("SMELT_GATEWAY_TOKEN", &token)
        .env("SMELT_ROOM", &room.room)
        .env("SMELT_SECRET", &room.secret)
        .env("SMELT_WRITE", if write { "true" } else { "false" })
        .env("RUST_LOG", "info")
        .stdin(std::process::Stdio::null());
    match bridge_log_file() {
        Some(log) => {
            let err_log = log
                .try_clone()
                .map_err(|e| format!("无法复用 bridge 日志文件描述符：{e}"))?;
            cmd.stdout(log).stderr(err_log);
        }
        None => {
            cmd.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        }
    }
    let child = cmd
        .spawn()
        .map_err(|e| format!("启动 smelt-bridge 失败：{e}"))?;
    let pid = child.id();
    std::mem::forget(child);

    // 3) RGB 二维码（后台生成，勿在 UI 线程）
    let qr = qr_png_for_url(&share);
    Ok((share, pid, qr, token, status.addr, status.write))
}

fn urlencoding_minimal(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            _ => format!("%{:02X}", c as u32),
        })
        .collect()
}

/// Cloudflare Tunnel 运行时状态（不落盘）：`connecting` 是"cloudflared 起来了但
/// 还没等到结果"这个中间态——`tunnel_start` 可能要跑好几秒到 ~30s，UI 得显示
/// "连接中…"而不是看起来卡住没反应。
#[derive(Clone, Default)]
pub struct TunnelRuntimeState {
    pub connecting: bool,
    pub url: Option<String>,
    pub error: Option<String>,
    /// 同 [`RemoteRuntimeState::write`]：这条公网链接实际的写权限，来自守护回执。
    pub write: bool,
}

impl Global for TunnelRuntimeState {}

/// 复制按钮的短暂「已复制 ✓」状态（设置页读它改按钮文案）。
#[derive(Clone, Default)]
struct CopyFlash {
    id: String,
    until: Option<Instant>,
}

impl Global for CopyFlash {}

fn copy_btn_label(id: &str, idle: &str, cx: &App) -> String {
    if let Some(f) = cx.try_global::<CopyFlash>() {
        if f.id == id {
            if let Some(until) = f.until {
                if Instant::now() < until {
                    return "已复制 ✓".into();
                }
            }
        }
    }
    idle.into()
}

/// 写入剪贴板 + 成功 toast + 按钮文案闪「已复制 ✓」约 2 秒。
fn copy_with_feedback(
    text: String,
    btn_id: &'static str,
    toast: &'static str,
    window: &mut Window,
    cx: &mut App,
) {
    cx.write_to_clipboard(ClipboardItem::new_string(text));
    cx.set_global(CopyFlash {
        id: btn_id.into(),
        until: Some(Instant::now() + Duration::from_millis(2000)),
    });
    window.push_notification(Notification::success(toast), cx);
    cx.refresh_windows();

    let clear_id = btn_id.to_string();
    cx.spawn(async move |cx| {
        cx.background_executor()
            .timer(Duration::from_millis(2000))
            .await;
        let _ = cx.update(|cx| {
            let same = cx
                .try_global::<CopyFlash>()
                .map(|f| f.id == clear_id)
                .unwrap_or(false);
            if same {
                cx.set_global(CopyFlash::default());
                cx.refresh_windows();
            }
        });
    })
    .detach();
}

/// 开关「手机 / 外网可访问」。开 = 自动确保远程已开 + 拉隧道；关 = 只拆隧道，本机链接保留。
/// 用户不必知道「必须先开远程」——依赖由这里消化。
pub fn apply_tunnel_toggle(enabled: bool, cx: &mut App) {
    let mut c = cx.global::<RemoteConfig>().clone();
    c.tunnel_enabled = enabled;
    if enabled {
        c.enabled = true;
    }
    save_remote_config(&c);
    cx.set_global(c.clone());

    if !enabled {
        terminal::tunnel_stop();
        cx.set_global(TunnelRuntimeState::default());
        return;
    }

    let write = c.write_enabled;
    // 先保证本机网关有 token（同步，通常很快），再异步建隧道。
    if !cx
        .global::<RemoteRuntimeState>()
        .token
        .as_ref()
        .is_some_and(|t| !t.is_empty())
    {
        set_remote_from_start_result(terminal::remote_start("127.0.0.1", write), cx);
    }
    spawn_tunnel_start(write, cx);
}

/// 开关「允许写入」。只改偏好时不打扰；远程已开则在后台按新权限换新链接，
/// 状态卡会显示「正在更新…」，用户不用手动关开关。
pub fn apply_write_toggle(enabled: bool, cx: &mut App) {
    let mut c = cx.global::<RemoteConfig>().clone();
    c.write_enabled = enabled;
    save_remote_config(&c);
    cx.set_global(c.clone());

    if !c.enabled {
        // 远程没开：只记偏好，下次打开总开关时自动带上。
        return;
    }

    // 可同时开 WebRTC + CF：两边都要跟着换 token，不能 if/else 只走一路
    let need_restart = c.webrtc_enabled || c.tunnel_enabled;
    if need_restart {
        if c.webrtc_enabled {
            stop_webrtc_bridge(cx);
        }
        if c.tunnel_enabled {
            terminal::tunnel_stop();
        }
        terminal::remote_stop();
        set_remote_from_start_result(terminal::remote_start("127.0.0.1", enabled), cx);
        if c.tunnel_enabled {
            spawn_tunnel_start(enabled, cx);
        }
        if c.webrtc_enabled {
            spawn_webrtc_start(cx);
        }
    } else {
        terminal::remote_stop();
        set_remote_from_start_result(terminal::remote_start("127.0.0.1", enabled), cx);
    }
}

/// 分享卡片上的「重试」：按当前配置把网关 / 隧道 / WebRTC 重新拉齐。
/// 与 [`apply_write_toggle`] 相同：WebRTC 与 CF 隧道可同时开，必须各自独立
/// 重启，不能 if/else 只走一路（否则另一路仍挂旧 token/端口）。
pub fn retry_remote_setup(cx: &mut App) {
    let mut c = cx.global::<RemoteConfig>().clone();
    if !c.enabled && !c.tunnel_enabled && !c.webrtc_enabled {
        return;
    }
    // 外网通道开着时确保总开关也开着（依赖由这里消化）
    if c.tunnel_enabled || c.webrtc_enabled {
        c.enabled = true;
        save_remote_config(&c);
        cx.set_global(c.clone());
    }
    let write = c.write_enabled;

    if c.webrtc_enabled {
        stop_webrtc_bridge(cx);
    }
    if c.tunnel_enabled {
        terminal::tunnel_stop();
    }
    // 网关先停再起，让 token/端口与两条外网通道对齐
    terminal::remote_stop();
    set_remote_from_start_result(terminal::remote_start("127.0.0.1", write), cx);
    if c.tunnel_enabled {
        spawn_tunnel_start(write, cx);
    }
    if c.webrtc_enabled {
        spawn_webrtc_start(cx);
    }
}

/// Copilot CLI 自己的配置文件路径（不是 smelt 的配置——这是 Copilot 全局设置，
/// 改了会影响你在任何地方用 copilot，不只是 smelt 里）。
fn copilot_settings_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".copilot").join("settings.json"))
}

/// 读 Copilot 的 `beep`（响铃提醒）开关；默认关闭，跟 Copilot 自己的默认值一致。
/// 每次都现读盘（不缓存）：这份文件可能被 Copilot CLI 自己或用户在别处改动。
fn read_copilot_beep() -> bool {
    copilot_settings_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("beep").and_then(|b| b.as_bool()))
        .unwrap_or(false)
}

/// 写 Copilot 的 `beep` 开关：只改这一个键，其余键（比如已有的 footer 配置）原样保留。
fn set_copilot_beep(enabled: bool) {
    let Some(path) = copilot_settings_path() else { return };
    let mut value: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    if !value.is_object() {
        value = serde_json::json!({});
    }
    value["beep"] = serde_json::Value::Bool(enabled);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string_pretty(&value) {
        let _ = std::fs::write(path, json);
    }
}

/// 改外观全局 + 存盘，不触发 view 重绘（调用方按需自己 notify/refresh）。
/// 供只有 `&mut App`（没有 `Context<Self>`）的场景用，比如设置页 SettingField 的 get/set 闭包。
fn apply_appearance(f: impl FnOnce(&mut Appearance), cx: &mut App) {
    let mut a = cx.global::<Appearance>().clone();
    f(&mut a);
    save_appearance(&a);
    cx.set_global(a);
}

/// 改桌面宠物配置全局 + 存盘 + 显隐同步，不触发 view 重绘，用法同 [`apply_appearance`]。
fn apply_pet_config(f: impl FnOnce(&mut pet::PetConfig), cx: &mut App) {
    let mut c = cx.global::<pet::PetConfig>().clone();
    let was_enabled = c.enabled;
    f(&mut c);
    pet::save_pet_config(&c);
    if c.enabled != was_enabled {
        pet::sync_pet_window_visibility(cx, c.enabled);
    }
    cx.set_global(c);
}

/// 改宠物大脑（LLM）配置全局 + 存盘，不触发 view 重绘，用法同 [`apply_appearance`]。
fn apply_llm_config(f: impl FnOnce(&mut agent::LlmConfig), cx: &mut App) {
    let mut c = cx.global::<agent::LlmConfig>().clone();
    f(&mut c);
    agent::save_llm_config(&c);
    cx.set_global(c);
}

/// Hsla → 0xRRGGBB（取色器回调把颜色写回 config 用）。
fn hsla_to_rgb(c: Hsla) -> u32 {
    let rgba = Rgba::from(c);
    let q = |f: f32| ((f.clamp(0.0, 1.0) * 255.0).round() as u32) & 0xff;
    (q(rgba.r) << 16) | (q(rgba.g) << 8) | q(rgba.b)
}

// ===================== 设置页专属类型 =====================

/// 宠物大脑配置的四个输入框（base_url / api_key / model / persona）。
#[derive(Clone)]
pub struct LlmInputs {
    base_url: Entity<gpui_component::input::InputState>,
    api_key: Entity<gpui_component::input::InputState>,
    model: Entity<gpui_component::input::InputState>,
    persona: Entity<gpui_component::input::InputState>,
}

/// 启动项列表编辑器：每项一对 label/command 输入框。
pub struct LaunchInputs {
    rows: Vec<(Entity<gpui_component::input::InputState>, Entity<gpui_component::input::InputState>)>,
    _subs: Vec<Subscription>,
}

/// 手动添加 workspace 列表编辑器：每项一对 label/workspace_dir 输入框；agent
/// 种类走下拉选择（离散值，不需要输入框），选完直接存盘不用另外的 InputState。
pub struct ProfileInputs {
    rows: Vec<(Entity<gpui_component::input::InputState>, Entity<gpui_component::input::InputState>)>,
    _subs: Vec<Subscription>,
}

/// 独立设置窗口的根 view：只是个薄壳，真正状态都还在传进来的 Workspace 实体上，
/// 每次渲染转手调 `render_settings_content`。
///
/// 但「转手调」不等于「跟着刷新」：`cx.notify()` 标脏的是 Workspace，设置窗口不在它
/// 的观察者名单里，不会因此重绘。所以得显式 observe 一把，否则后台改的状态——更新
/// 运行时长的人话格式：秒 → 「3 小时 12 分」。只保留两级单位，设置页那行不需要秒级精度。
fn fmt_uptime(secs: u64) -> String {
    let (d, h, m) = (secs / 86400, secs % 86400 / 3600, secs % 3600 / 60);
    match (d, h, m) {
        (0, 0, 0) => format!("{secs} 秒"),
        (0, 0, m) => format!("{m} 分钟"),
        (0, h, m) => format!("{h} 小时 {m} 分"),
        (d, h, _) => format!("{d} 天 {h} 小时"),
    }
}

/// 守护运行信息拼成一行：`v0.5.4 · PID 64954 · 启动于 07-16 20:38（已运行 3 小时 12 分）· 5 个会话`。
/// 老守护回不出的字段直接不显示——宁可少一段，也不摆「未知」占位。
fn daemon_info_line(info: &terminal::DaemonInfo) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(v) = &info.version {
        parts.push(format!("v{v}"));
    }
    if let Some(pid) = info.pid {
        parts.push(format!("PID {pid}"));
    }
    if let Some(started) = info.started_at {
        // 本地时区显示；秒数换算成人话时长跟在后面。
        let started_txt = chrono::DateTime::from_timestamp(started as i64, 0)
            .map(|t| {
                t.with_timezone(&chrono::Local)
                    .format("%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_else(|| "?".into());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // saturating：守护跟 GUI 之间时钟若有漂移，别算出个天文数字。
        parts.push(format!(
            "启动于 {started_txt}（已运行 {}）",
            fmt_uptime(now.saturating_sub(started))
        ));
    }
    if let Some(n) = info.session_count {
        parts.push(format!("{n} 个会话"));
    }
    parts.join(" · ")
}

/// 下载进度、守护进程检测结果——在设置窗口里会一直停在打开那一刻的样子。
pub struct SettingsWindow {
    workspace: Entity<Workspace>,
    _observe_workspace: Subscription,
}

impl Render for SettingsWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 设置内容 +（可选）重启守护确认层：弹层必须画在本窗，不能只改 Workspace 上的
        // flag 却在主窗口 render——用户点的是设置里的按钮，确认框却跑到主界面。
        self.workspace.update(cx, |ws, cx| {
            div()
                .relative()
                .size_full()
                .child(ws.render_settings_content(cx))
                .children(
                    ws.show_daemon_restart_confirm
                        .then(|| ws.render_daemon_restart_confirm(cx)),
                )
        })
    }
}

/// 独立设置窗口的单例句柄：已经开着就聚焦复用，避免重复开出好几扇一样的窗口。
pub struct SettingsWindowHandle(pub Option<WindowHandle<Root>>);
impl Global for SettingsWindowHandle {}

// ===================== Workspace 方法 =====================

impl Workspace {
    /// 懒创建宠物大脑配置的输入框（需要 window，故在首次渲染设置面板时调）。
    /// 每个框预填当前配置值，变更时写回 LlmConfig 并存盘。
    pub fn init_llm_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use gpui_component::input::{InputEvent, InputState};
        let lc = cx.global::<agent::LlmConfig>().clone();

        let base_url = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("https://api.deepseek.com/chat/completions")
                .default_value(lc.base_url.clone())
        });
        let api_key = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("sk-...（留空则用 config.toml/env）")
                .masked(true)
                .default_value(lc.api_key.clone())
        });
        let model = cx.new(|cx| {
            InputState::new(window, cx).placeholder("deepseek-chat").default_value(lc.model.clone())
        });
        let persona = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .auto_grow(2, 5)
                .placeholder("人设 / system prompt")
                .default_value(lc.persona.clone())
        });

        // 变更即写回对应字段（Change 覆盖键入，Blur 兜底）。
        let save_on = |ev: &InputEvent| matches!(ev, InputEvent::Change | InputEvent::Blur);
        self.llm_subs.clear();
        self.llm_subs.push(cx.subscribe(&base_url, move |this, s, ev: &InputEvent, cx| {
            if save_on(ev) {
                let v = s.read(cx).value().to_string();
                this.update_llm_config(|c| c.base_url = v, cx);
            }
        }));
        self.llm_subs.push(cx.subscribe(&api_key, move |this, s, ev: &InputEvent, cx| {
            if save_on(ev) {
                let v = s.read(cx).value().to_string();
                this.update_llm_config(|c| c.api_key = v, cx);
            }
        }));
        self.llm_subs.push(cx.subscribe(&model, move |this, s, ev: &InputEvent, cx| {
            if save_on(ev) {
                let v = s.read(cx).value().to_string();
                this.update_llm_config(|c| c.model = v, cx);
            }
        }));
        self.llm_subs.push(cx.subscribe(&persona, move |this, s, ev: &InputEvent, cx| {
            if save_on(ev) {
                let v = s.read(cx).value().to_string();
                this.update_llm_config(|c| c.persona = v, cx);
            }
        }));

        self.llm_inputs = Some(LlmInputs { base_url, api_key, model, persona });

        // —— 有状态组件：不透明度滑块 + 字体大小滑块 + 背景色 / 宠物色取色器 ——
        let ap = cx.global::<Appearance>().clone();
        let pc = cx.global::<pet::PetConfig>().clone();
        let opacity_slider = cx.new(|_| {
            SliderState::new().min(60.0).max(100.0).step(5.0).default_value(ap.opacity * 100.0)
        });
        let font_size_slider = cx.new(|_| {
            SliderState::new()
                .min(terminal_view::MIN_FONT_PX as f32)
                .max(terminal_view::MAX_FONT_PX as f32)
                .step(1.0)
                .default_value(ap.font_px as f32)
        });
        let bg_color_picker =
            cx.new(|cx| ColorPickerState::new(window, cx).default_value(rgb(ap.bg_color)));
        let pet_color_picker =
            cx.new(|cx| ColorPickerState::new(window, cx).default_value(rgb(pc.color)));

        self.settings_subs.clear();

        // 信令服务地址：无写死域名，用户填自己的 smelt-signal。故意不订阅 Change/Blur
        // 自动落盘——下面「保存」按钮走 apply_signal_http，靠 draft≠saved 判断是否要
        // 重启 bridge；这里要是也自动写 RemoteConfig，会在按「保存」之前就把值同步过去，
        // dirty 恒为 false，按钮和地址变更重启 bridge 的逻辑都会失效。
        let remote_sig = normalize_signal_http(&cx.global::<RemoteConfig>().signal_http);
        let signal_http_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("https://signal.example.com（你部署的 smelt-signal）")
                .default_value(remote_sig)
        });
        self.signal_http_input = Some(signal_http_input);

        self.settings_subs.push(cx.subscribe(
            &opacity_slider,
            |this, _s, ev: &SliderEvent, cx| {
                let (SliderEvent::Change(v) | SliderEvent::Release(v)) = ev;
                if let SliderValue::Single(x) = v {
                    let op = (*x / 100.0).clamp(0.3, 1.0);
                    this.set_appearance(move |a| a.opacity = op, cx);
                }
            },
        ));
        self.settings_subs.push(cx.subscribe(
            &font_size_slider,
            |this, _s, ev: &SliderEvent, cx| {
                let (SliderEvent::Change(v) | SliderEvent::Release(v)) = ev;
                if let SliderValue::Single(x) = v {
                    let size = x.round().clamp(
                        terminal_view::MIN_FONT_PX as f32,
                        terminal_view::MAX_FONT_PX as f32,
                    ) as u32;
                    terminal_view::set_font_px(size);
                    this.set_appearance(move |a| a.font_px = size, cx);
                }
            },
        ));
        self.settings_subs.push(cx.subscribe(
            &bg_color_picker,
            |this, _s, ev: &ColorPickerEvent, cx| {
                let ColorPickerEvent::Change(c) = ev;
                if let Some(hsla) = c {
                    let color = hsla_to_rgb(*hsla);
                    this.set_appearance(move |a| a.bg_color = color, cx);
                }
            },
        ));
        self.settings_subs.push(cx.subscribe(
            &pet_color_picker,
            |this, _s, ev: &ColorPickerEvent, cx| {
                let ColorPickerEvent::Change(c) = ev;
                if let Some(hsla) = c {
                    let color = hsla_to_rgb(*hsla);
                    this.update_pet_config(move |cfg| cfg.color = color, cx);
                }
            },
        ));
        self.opacity_slider = Some(opacity_slider);
        self.font_size_slider = Some(font_size_slider);
        self.bg_color_picker = Some(bg_color_picker);
        self.pet_color_picker = Some(pet_color_picker);
    }

    /// 无 window 版：改全局 + 存盘 + 重绘。窗口背景（透明/模糊）由 render 里的
    /// applied_window_bg 同步——供 slider/color_picker 的订阅回调用（它们拿不到 window）。
    pub fn set_appearance(&mut self, f: impl FnOnce(&mut Appearance), cx: &mut Context<Self>) {
        apply_appearance(f, cx);
        cx.notify();
    }

    /// 修改桌面宠物配置：改全局 + 存盘 + 触发重绘。宠物窗口每帧读该全局，改动 ≤50ms 生效。
    pub fn update_pet_config(&mut self, f: impl FnOnce(&mut pet::PetConfig), cx: &mut Context<Self>) {
        apply_pet_config(f, cx);
        cx.notify();
    }

    /// 修改宠物大脑（LLM）配置：改全局 + 存盘 + 重绘。
    pub fn update_llm_config(&mut self, f: impl FnOnce(&mut agent::LlmConfig), cx: &mut Context<Self>) {
        apply_llm_config(f, cx);
        cx.notify();
    }

    /// 启动项条数变了就重建输入框（增删后调用）。
    pub fn reset_launch_inputs(&mut self) {
        self.launch_inputs = None;
    }

    /// 懒创建启动项列表编辑器（需要 window）。
    pub fn ensure_launch_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let count = cx.global::<LaunchConfig>().entries.len();
        let stale = self.launch_inputs.as_ref().is_none_or(|i| i.rows.len() != count);
        if stale {
            self.init_launch_inputs(window, cx);
        }
    }

    fn init_launch_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use gpui_component::input::{InputEvent, InputState};

        let entries = cx.global::<LaunchConfig>().entries.clone();
        let save_on = |ev: &InputEvent| matches!(ev, InputEvent::Change | InputEvent::Blur);
        let mut rows = Vec::new();
        let mut subs = Vec::new();
        for (i, entry) in entries.iter().enumerate() {
            let label_input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("显示名称")
                    .default_value(entry.label.clone())
            });
            let command_input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("启动命令，如 claude")
                    .default_value(entry.command.clone())
            });
            subs.push(cx.subscribe(&label_input, move |_, s, ev: &InputEvent, cx| {
                if save_on(ev) {
                    let v = s.read(cx).value().to_string();
                    apply_launch_config(|c| {
                        if let Some(e) = c.entries.get_mut(i) {
                            e.label = v;
                        }
                    }, cx);
                }
            }));
            subs.push(cx.subscribe(&command_input, move |_, s, ev: &InputEvent, cx| {
                if save_on(ev) {
                    let v = s.read(cx).value().to_string();
                    apply_launch_config(|c| {
                        if let Some(e) = c.entries.get_mut(i) {
                            e.command = v;
                        }
                    }, cx);
                }
            }));
            rows.push((label_input, command_input));
        }
        self.launch_inputs = Some(LaunchInputs { rows, _subs: subs });
    }

    pub fn add_launch_entry(&mut self, cx: &mut Context<Self>) {
        apply_launch_config(|c| {
            c.entries.push(LaunchEntry {
                label: "新启动项".into(),
                command: String::new(),
            });
        }, cx);
        self.reset_launch_inputs();
        cx.notify();
    }

    pub fn remove_launch_entry(&mut self, index: usize, cx: &mut Context<Self>) {
        apply_launch_config(|c| {
            if index < c.entries.len() {
                c.entries.remove(index);
            }
        }, cx);
        self.reset_launch_inputs();
        cx.notify();
    }

    /// 手动添加 workspace 条数变了就重建输入框（增删后调用）。
    pub fn reset_profile_inputs(&mut self) {
        self.profile_inputs = None;
    }

    /// 懒创建 workspace 列表编辑器（需要 window）。
    pub fn ensure_profile_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let count = cx.global::<AgentUiConfig>().profiles.len();
        let stale = self.profile_inputs.as_ref().is_none_or(|i| i.rows.len() != count);
        if stale {
            self.init_profile_inputs(window, cx);
        }
    }

    fn init_profile_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use gpui_component::input::{InputEvent, InputState};

        let profiles = cx.global::<AgentUiConfig>().profiles.clone();
        let save_on = |ev: &InputEvent| matches!(ev, InputEvent::Change | InputEvent::Blur);
        let mut rows = Vec::new();
        let mut subs = Vec::new();
        for (i, p) in profiles.iter().enumerate() {
            let id = p.id.clone();
            let label_input = cx.new(|cx| {
                InputState::new(window, cx).placeholder("显示名称").default_value(p.label.clone())
            });
            let dir_input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("workspace 目录，如 ~/.claude-quant")
                    .default_value(p.workspace_dir.clone())
            });
            let id_for_label = id.clone();
            subs.push(cx.subscribe(&label_input, move |_, s, ev: &InputEvent, cx| {
                if save_on(ev) {
                    let v = s.read(cx).value().to_string();
                    let id = id_for_label.clone();
                    apply_agent_ui(move |c| {
                        if let Some(p) = c.profiles.iter_mut().find(|p| p.id == id) {
                            p.label = v;
                        }
                    }, cx);
                }
            }));
            let id_for_dir = id.clone();
            subs.push(cx.subscribe(&dir_input, move |_, s, ev: &InputEvent, cx| {
                if save_on(ev) {
                    let v = s.read(cx).value().to_string();
                    let id = id_for_dir.clone();
                    apply_agent_ui(move |c| {
                        if let Some(p) = c.profiles.iter_mut().find(|p| p.id == id) {
                            p.workspace_dir = v;
                        }
                    }, cx);
                }
            }));
            let _ = i;
            rows.push((label_input, dir_input));
        }
        self.profile_inputs = Some(ProfileInputs { rows, _subs: subs });
    }

    /// 新增一个手动 workspace：默认接 Claude（用户改目录之前就得选个 agent，
    /// Claude 是最常见的场景，跟「新建 ACP 对话」菜单的默认排位一致）。
    pub fn add_profile(&mut self, cx: &mut Context<Self>) {
        apply_agent_ui(|c| {
            c.profiles.push(AcpProfile {
                id: uuid::Uuid::new_v4().to_string(),
                kind_id: AcpAgentKind::Claude.id().to_string(),
                label: "新 workspace".into(),
                workspace_dir: String::new(),
            });
        }, cx);
        self.reset_profile_inputs();
        cx.notify();
    }

    pub fn remove_profile(&mut self, index: usize, cx: &mut Context<Self>) {
        apply_agent_ui(|c| {
            if index < c.profiles.len() {
                c.profiles.remove(index);
            }
        }, cx);
        self.reset_profile_inputs();
        cx.notify();
    }

    /// 改某个 workspace 接的 agent 种类（下拉菜单选中项回调）。
    pub fn set_profile_kind(&mut self, index: usize, kind: AcpAgentKind, cx: &mut Context<Self>) {
        apply_agent_ui(move |c| {
            if let Some(p) = c.profiles.get_mut(index) {
                p.kind_id = kind.id().to_string();
            }
        }, cx);
        cx.notify();
    }

    /// 设置 / 清除背景图（不影响窗口透明度，故无需 window）。
    pub fn set_bg_image(&mut self, path: Option<String>, cx: &mut Context<Self>) {
        apply_appearance(|a| a.bg_image = path, cx);
        cx.notify();
    }

    /// 弹原生选择框选一张背景图。
    pub fn pick_bg_image(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("选择背景图片".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                if let Some(p) =
                    paths.into_iter().next().and_then(|p| p.to_str().map(String::from))
                {
                    this.update(cx, |this, cx| this.set_bg_image(Some(p), cx)).ok();
                }
            }
        })
        .detach();
    }

    /// 渲染独立设置页面：铺满主区、居中限宽、支持滚动。
    /// 设置页主体：外观 / 桌面宠物 / 更新三个分组。供嵌入式设置页（主窗口右上角齿轮，
    /// 带「返回」头）和独立设置窗口（原生标题栏，无需「返回」）共用，各自决定外层怎么包。
    pub fn render_settings_content(&self, cx: &mut Context<Self>) -> Div {
        let (fg, muted, border, popover) = {
            let t = cx.theme();
            (t.foreground, t.muted_foreground, t.border, t.popover)
        };
        let entity = cx.entity();

        // 统一的小按钮：固定高度 + flex_none，避免被 flex 布局拉伸成大块。
        // move 闭包：捕获的四个颜色都是 Copy，闭包本身因此也是 Copy，可以放心
        // 塞进下面多个 SettingField::render 的 move 闭包里各用一份。
        //
        // 注意：GPUI 的 `.hover()` 只能挂一次（debug_assert「hover style already set」），
        // 所以默认 hover 写在这里；需要换 hover 色的按钮请用 `btn_hover`，别再链式 `.hover()`。
        let btn_base = move |id: &'static str, label: String| {
            div()
                .id(id)
                .h(px(26.))
                .px_3()
                .flex()
                .flex_none()
                .items_center()
                .rounded_md()
                .cursor_pointer()
                .text_xs()
                .text_color(fg)
                .bg(popover)
                .border_1()
                .border_color(border)
                .child(label)
        };
        let btn = move |id: &'static str, label: String| {
            btn_base(id, label).hover(|s| s.bg(border))
        };
        let btn_hover = move |id: &'static str, label: String, hover_bg: Hsla| {
            btn_base(id, label).hover(move |s| s.bg(hover_bg))
        };

        const PET_SIZES: [f32; 3] = [0.8, 1.0, 1.25];

        // —— 外观 ——
        let bg_color_picker = self.bg_color_picker.clone();
        let opacity_slider = self.opacity_slider.clone();
        let font_size_slider = self.font_size_slider.clone();
        let pick_entity = entity.clone();
        let clear_entity = entity.clone();
        // 终端字体下拉的选项：内嵌默认置顶（值为空 = 用默认），其后按字母序列出系统
        // 已装的全部字体族。不做等宽过滤——系统没有可靠的「是否等宽」元数据，漏判
        // 误判都更糟；选了非等宽的后果只是难看，fallback 链保证不会渲染错乱。
        //
        // 扫字体贵（见 `font_options` 字段注释），只在第一次渲染设置页时做一次。
        let font_options = self
            .font_options
            .get_or_init(|| {
                let mut names = cx.text_system().all_font_names();
                names.sort();
                names.dedup();
                // 选项 label 同时也是下拉按钮上的文字，而 Button 既不截断也不收缩，
                // 全名「JetBrainsMono Nerd Font Mono」会把按钮顶出设置页右边界。
                // 这里只取第一段，完整名字放在 description 里。
                let short = terminal_view::DEFAULT_FONT_FAMILY
                    .split_whitespace()
                    .next()
                    .unwrap_or(terminal_view::DEFAULT_FONT_FAMILY);
                std::iter::once((
                    SharedString::from(""),
                    SharedString::from(format!("默认（{short}）")),
                ))
                .chain(
                    names.into_iter().map(|n| (SharedString::from(n.clone()), SharedString::from(n))),
                )
                .collect()
            })
            .clone();
        let appearance_page = SettingPage::new("外观").default_open(true).group(
            SettingGroup::new().items(vec![
                SettingItem::new(
                    "主题模式",
                    SettingField::switch(
                        |cx: &App| cx.global::<Appearance>().theme_mode.is_dark(),
                        |v: bool, cx: &mut App| {
                            let mode = if v { ThemeMode::Dark } else { ThemeMode::Light };
                            apply_appearance(|a| a.theme_mode = mode, cx);
                            apply_theme_mode(mode, cx);
                            // 色板是进程级全局态（见 ui_theme），改完不重绘就还是旧色。
                            cx.refresh_windows();
                        },
                    )
                    .default_value(true),
                )
                .description("开启为深色主题，关闭为浅色主题"),
                SettingItem::new(
                    "字体大小",
                    SettingField::render(move |_, _, cx: &mut App| {
                        let size = cx.global::<Appearance>().font_px;
                        h_flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .w(px(200.))
                                    .children(font_size_slider.as_ref().map(Slider::new)),
                            )
                            .child(
                                div()
                                    .w(px(32.))
                                    .text_xs()
                                    .text_color(muted)
                                    .child(format!("{size}px")),
                            )
                    }),
                ),
                SettingItem::new(
                    "终端字体",
                    SettingField::scrollable_dropdown(
                        font_options,
                        |cx: &App| cx.global::<Appearance>().font_family.clone().into(),
                        |v: SharedString, cx: &mut App| {
                            let name = v.trim().to_string();
                            terminal_view::set_font_family(&name);
                            apply_appearance(move |a| a.font_family = name, cx);
                            cx.refresh_windows();
                        },
                    )
                    // 系统里总有名字长得离谱的字体，选中后同样会顶爆按钮，这里封顶兜住。
                    .max_w(px(220.))
                    .overflow_hidden(),
                )
                .description(concat!(
                    "终端使用的字体；建议选等宽字体，图标缺字自动回落内嵌默认（",
                    "JetBrainsMono Nerd Font Mono）",
                )),
                SettingItem::new(
                    "背景色",
                    SettingField::render(move |_, _, _| {
                        div().children(bg_color_picker.as_ref().map(|p| ColorPicker::new(p).small()))
                    }),
                ),
                SettingItem::new(
                    "背景图片",
                    SettingField::render(move |_, _, cx: &mut App| {
                        let img_name = cx
                            .global::<Appearance>()
                            .bg_image
                            .as_deref()
                            .and_then(|p| p.rsplit('/').next())
                            .unwrap_or("无")
                            .to_string();
                        let pick_entity = pick_entity.clone();
                        let clear_entity = clear_entity.clone();
                        h_flex()
                            .items_center()
                            .gap_2()
                            .child(
                                // 文件名长度不可控，必须自己封顶：SettingItem 外层是
                                // overflow_hidden，撑爆的部分不会换行，只会把右边的按钮
                                // 顶出可视区，导致「选择图片…／清除」点都点不到。
                                // 中间省略号保留开头和扩展名，比末尾截断更容易认出是哪张图。
                                div()
                                    .max_w(px(140.))
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis_middle()
                                    .text_xs()
                                    .text_color(muted)
                                    .child(img_name),
                            )
                            .child(btn("pick-img", "选择图片…".into()).flex_shrink_0().on_mouse_down(
                                MouseButton::Left,
                                move |_, _window, cx: &mut App| {
                                    pick_entity.update(cx, |this, cx| this.pick_bg_image(cx));
                                },
                            ))
                            .child(
                                btn("clear-img", "清除".into())
                                    .flex_shrink_0()
                                    .text_color(muted)
                                    .on_mouse_down(
                                    MouseButton::Left,
                                    move |_, _window, cx: &mut App| {
                                        clear_entity.update(cx, |this, cx| this.set_bg_image(None, cx));
                                    },
                                ),
                            )
                    }),
                ),
                SettingItem::new(
                    "不透明度",
                    SettingField::render(move |_, _, _| {
                        div().w(px(200.)).children(opacity_slider.as_ref().map(Slider::new))
                    }),
                ),
                SettingItem::new(
                    "背景模糊",
                    SettingField::switch(
                        |cx: &App| cx.global::<Appearance>().blur,
                        |v: bool, cx: &mut App| {
                            apply_appearance(|a| a.blur = v, cx);
                            cx.refresh_windows();
                        },
                    )
                    .default_value(false),
                ),
            ]),
        );

        // —— 桌面宠物 ——
        let pet_color_picker = self.pet_color_picker.clone();
        let llm_inputs = self.llm_inputs.clone();
        let pet_page = SettingPage::new("桌面宠物").group(
            SettingGroup::new().items(vec![
                SettingItem::new(
                    "显示宠物",
                    SettingField::switch(
                        |cx: &App| cx.global::<pet::PetConfig>().enabled,
                        |v: bool, cx: &mut App| apply_pet_config(|c| c.enabled = v, cx),
                    ),
                ),
                SettingItem::new(
                    "状态播报",
                    SettingField::switch(
                        |cx: &App| cx.global::<pet::PetConfig>().notify,
                        |v: bool, cx: &mut App| apply_pet_config(|c| c.notify = v, cx),
                    ),
                ),
                SettingItem::new(
                    "宠物大脑（LLM）",
                    SettingField::switch(
                        |cx: &App| cx.global::<agent::LlmConfig>().enabled,
                        |v: bool, cx: &mut App| apply_llm_config(|c| c.enabled = v, cx),
                    ),
                )
                .description("接入 OpenAI 兼容接口，点击或通知宠物时将调用 LLM 主动说话。"),
                SettingItem::render(move |_, _, _| {
                    let field = |label: &str, state: &Entity<gpui_component::input::InputState>| {
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(div().text_xs().text_color(muted).child(label.to_string()))
                            .child(Input::new(state).small())
                    };
                    div()
                        .w_full()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .children(llm_inputs.as_ref().map(|inp| {
                            div()
                                .flex()
                                .flex_col()
                                .gap_3()
                                .child(field("接口地址 base_url", &inp.base_url))
                                .child(field("API Key", &inp.api_key))
                                .child(field("模型 model", &inp.model))
                                .child(field("人设 persona", &inp.persona))
                        }))
                }),
                SettingItem::new(
                    "颜色",
                    SettingField::render(move |_, _, _| {
                        div().children(pet_color_picker.as_ref().map(|p| ColorPicker::new(p).small()))
                    }),
                ),
                SettingItem::new(
                    "大小",
                    SettingField::render(move |_, _, cx: &mut App| {
                        let scale = cx.global::<pet::PetConfig>().scale;
                        let size_ix = PET_SIZES.iter().position(|v| (scale - v).abs() < 0.01);
                        RadioGroup::horizontal("pet-size")
                            .selected_index(size_ix)
                            .on_click(|ix: &usize, _window, cx: &mut App| {
                                let val = PET_SIZES[*ix];
                                apply_pet_config(|c| c.scale = val, cx);
                            })
                            .children([
                                Radio::new("sz-s").label("小"),
                                Radio::new("sz-m").label("中"),
                                Radio::new("sz-l").label("大"),
                            ])
                    }),
                ),
            ]),
        );

        // —— 启动：项目「+」下拉菜单的可配置启动项 ——
        // Settings 的 list 测量项高度时，百分比宽度（w_full）经常解析不到确定父宽，
        // 卡片会缩成「内容宽」——输入框只露出几个字。这里用窗口视口算绝对像素宽。
        let launch_editor_entity = entity.clone();
        let launch_page = SettingPage::new("启动").group(
            SettingGroup::new()
                .item(
                    SettingItem::render(move |_, window, cx: &mut App| {
                        let muted = cx.theme().muted_foreground;
                        let border = cx.theme().border;
                        let fg = cx.theme().foreground;
                        let popover = cx.theme().popover;
                        let secondary = cx.theme().secondary;
                        let danger = cx.theme().danger;
                        let danger_fg = cx.theme().danger_foreground;
                        // 侧栏默认 250 + 左右 padding/滚动条余量；再夹到合理区间。
                        let field_w = {
                            let vw = f32::from(window.viewport_size().width);
                            let w = (vw - 250. - 80.).clamp(360., 720.);
                            px(w)
                        };
                        launch_editor_entity.update(cx, |ws, cx| {
                            ws.ensure_launch_inputs(window, cx);
                            let Some(inputs) = ws.launch_inputs.as_ref() else {
                                return div().into_any_element();
                            };
                            let mut col = v_flex()
                                .w(field_w)
                                .gap_3()
                                .child(
                                    v_flex()
                                        .w(field_w)
                                        .gap_1()
                                        .child(
                                            div()
                                                .text_sm()
                                                .font_semibold()
                                                .text_color(fg)
                                                .child("快捷启动项"),
                                        )
                                        .child(
                                            div().w(field_w).text_sm().text_color(muted).child(
                                                "项目行「+」菜单里除「新建终端」「新建 Worktree…」外的项。\
                                                 显示名会出现在菜单上；命令是在该项目目录下执行的 shell 命令\
                                                 （可含参数）。",
                                            ),
                                        ),
                                );
                            // 名称和命令并排成两列，而不是上下堆叠：之前两个输入框同宽同字体，
                            // 只靠上方一行小灰字区分，扫视时根本分不出哪个是哪个。改成
                            // 「窄名称列 + 宽命令列 + 命令用等宽字体」——列位置、宽度、字体三重
                            // 区分，比标签文字有效得多，顺带把每项从 4 行压到 1 行。
                            // 名称短（"Claude Code" 这种）、命令长（带一串参数），宽度按
                            // 信息量分：名称够放就行，剩下的全给命令。
                            let name_w = px(140.);
                            let del_w = px(28.);
                            // 容器 p_3（12*2）+ 行内两个 gap_2（8*2）。
                            let cmd_w = field_w - name_w - del_w - px(40.);
                            let mono = terminal_view::font_family();

                            let mut list = v_flex()
                                .w(field_w)
                                .gap_2()
                                .p_3()
                                .rounded_lg()
                                .border_1()
                                .border_color(border)
                                .bg(secondary)
                                // 列名只在表头出现一次，不必每项重复一遍「名称」「命令」。
                                .child(
                                    h_flex()
                                        .w_full()
                                        .gap_2()
                                        .items_center()
                                        .text_xs()
                                        .text_color(muted)
                                        .child(div().w(name_w).child("名称"))
                                        .child(div().w(cmd_w).child("命令"))
                                        // 占位：让表头两列跟下面的行严格对齐（删除按钮那一列）。
                                        .child(div().w(del_w)),
                                );
                            for (ix, (label, command)) in inputs.rows.iter().enumerate() {
                                let del_entity = launch_editor_entity.clone();
                                let row_ix = ix;
                                list = list.child(
                                    h_flex()
                                        .id(("launch-row", row_ix))
                                        .w_full()
                                        .gap_2()
                                        .items_center()
                                        .child(Input::new(label).w(name_w))
                                        // 命令是 shell 代码，用终端同款等宽字体——参数里的
                                        // `-`/`_` 对齐后好读，也一眼跟左边的显示名区分开。
                                        .child(
                                            Input::new(command)
                                                .w(cmd_w)
                                                .font_family(mono.clone()),
                                        )
                                        .child(
                                            div()
                                                .id(("del-launch", row_ix))
                                                .size(del_w)
                                                .flex()
                                                .flex_none()
                                                .items_center()
                                                .justify_center()
                                                .rounded_md()
                                                .cursor_pointer()
                                                .text_sm()
                                                .text_color(muted)
                                                // 删除是破坏性操作，hover 时给红底明示。
                                                .hover(|s| s.bg(danger).text_color(danger_fg))
                                                .child("×")
                                                .on_mouse_down(
                                                    MouseButton::Left,
                                                    move |_, _, cx: &mut App| {
                                                        del_entity.update(cx, |ws, cx| {
                                                            ws.remove_launch_entry(row_ix, cx);
                                                        });
                                                    },
                                                ),
                                        ),
                                );
                            }
                            col = col.child(list);
                            let add_entity = launch_editor_entity.clone();
                            col.child(
                                div()
                                    .id("add-launch")
                                    .h(px(36.))
                                    .w(field_w)
                                    .px_3()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded_lg()
                                    .cursor_pointer()
                                    .text_sm()
                                    .text_color(fg)
                                    .bg(popover)
                                    .border_1()
                                    .border_color(border)
                                    .hover(|s| s.bg(border))
                                    .child("+ 添加启动项")
                                    .on_mouse_down(MouseButton::Left, move |_, _, cx: &mut App| {
                                        add_entity.update(cx, |ws, cx| ws.add_launch_entry(cx));
                                    }),
                            )
                            .into_any_element()
                        })
                    })
                    .keywords(["快捷启动", "launch", "命令", "claude", "codex", "copilot"]),
                )
                .item(
                    SettingItem::new(
                        "Copilot 响铃通知",
                        SettingField::switch(
                            |_cx: &App| read_copilot_beep(),
                            |v: bool, _cx: &mut App| set_copilot_beep(v),
                        ),
                    )
                    .description(
                        "开启 Copilot CLI 自己的 beep 设置（默认关闭）：需要你确认或跑完一轮时\
                         发终端响铃，smelt 能借此点亮侧栏状态点/toast/角标——不开这个 Copilot \
                         不会主动发任何信号。改的是 ~/.copilot/settings.json，会影响你所有场景下\
                         用 Copilot，不止 smelt 里。",
                    ),
                ),
        );

        // —— 更新：检查/下载全自动静默，生效推迟到退出时 ——
        let update_entity = entity.clone();
        let daemon_entity = entity.clone();
        let update_page = SettingPage::new("更新").resettable(false).group(
            SettingGroup::new()
                .item(SettingItem::render(move |_, _, cx: &mut App| {
                let status = update_entity.read(cx).update_status.clone();
                // 字节数换算成 MB 展示，只在拿得到 Content-Length 时才有百分比。
                let mb = |b: u64| b as f64 / 1024.0 / 1024.0;
                let status_text = match &status {
                    updater::UpdateStatus::Idle => String::new(),
                    updater::UpdateStatus::Checking => "检查中…".to_string(),
                    updater::UpdateStatus::UpToDate => "已是最新版本".to_string(),
                    updater::UpdateStatus::Downloading { version, received, total } => match total {
                        Some(total) if *total > 0 => format!(
                            "正在下载 v{version}… {:.0}%（{:.1} / {:.1} MB）",
                            *received as f64 / *total as f64 * 100.0,
                            mb(*received),
                            mb(*total),
                        ),
                        _ => format!("正在下载 v{version}…（已下载 {:.1} MB）", mb(*received)),
                    },
                    updater::UpdateStatus::Installing { version } => {
                        format!("正在安装 v{version}…")
                    }
                    updater::UpdateStatus::ReadyToInstall { version, .. } => {
                        format!("新版本 v{version} 已就绪，下次启动生效")
                    }
                    updater::UpdateStatus::Failed(e) => format!("检查失败：{e}"),
                };
                // 进度条：能算出百分比就走确定进度，否则跑不确定的滑动动画。
                let progress_bar = match &status {
                    updater::UpdateStatus::Downloading { received, total: Some(total), .. }
                        if *total > 0 =>
                    {
                        Some(
                            Progress::new("update-progress")
                                .value(*received as f32 / *total as f32 * 100.0),
                        )
                    }
                    updater::UpdateStatus::Downloading { .. }
                    | updater::UpdateStatus::Installing { .. } => {
                        Some(Progress::new("update-progress").loading(true))
                    }
                    _ => None,
                };
                let busy = matches!(
                    status,
                    updater::UpdateStatus::Checking
                        | updater::UpdateStatus::Downloading { .. }
                        | updater::UpdateStatus::Installing { .. }
                );
                let ready = matches!(status, updater::UpdateStatus::ReadyToInstall { .. });

                let check_label: String = match &status {
                    updater::UpdateStatus::Checking => "检查中…".into(),
                    updater::UpdateStatus::Downloading { .. } => "下载中…".into(),
                    updater::UpdateStatus::Installing { .. } => "安装中…".into(),
                    _ => "检查更新".into(),
                };
                let check_entity = update_entity.clone();
                let check_btn = btn("check-update", check_label)
                    .text_color(if busy { muted } else { fg })
                    .on_mouse_down(MouseButton::Left, move |_, _window, cx: &mut App| {
                        check_entity.update(cx, |this, cx| {
                            if !matches!(
                                this.update_status,
                                updater::UpdateStatus::Checking
                                    | updater::UpdateStatus::Downloading { .. }
                                    | updater::UpdateStatus::Installing { .. }
                            ) {
                                this.check_for_update(false, cx);
                            }
                        });
                    });
                let restart_btn = ready.then(|| {
                    btn_hover(
                        "restart-update",
                        "立即重启更新".into(),
                        Hsla::from(crate::ui_theme::tint(crate::ui_theme::blue(), 0x40)),
                    )
                        .text_color(rgb(crate::ui_theme::blue()))
                        .bg(Hsla::from(crate::ui_theme::tint(crate::ui_theme::blue(), 0x24)))
                        .on_mouse_down(MouseButton::Left, move |_, _window, cx: &mut App| {
                            if let updater::UpdateStatus::ReadyToInstall { staged_app, .. } = &status {
                                // 先 handoff smeltd 再换 .app，避免会话全灭后对话被「重新初始化」。
                                if crate::terminal::install_app_preserving_sessions(staged_app)
                                    .is_ok()
                                {
                                    // 排好重启再退；拉不起来也只是退化成手动打开，不该拦着退出。
                                    let _ = updater::relaunch();
                                    cx.quit();
                                }
                            }
                        })
                });

                v_flex()
                    .w_full()
                    .gap_3()
                    .child(
                        h_flex()
                            .w_full()
                            .justify_between()
                            .items_center()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(fg)
                                            .child(concat!("当前版本 v", env!("CARGO_PKG_VERSION"))),
                                    )
                                    .child(
                                        div()
                                            .id("settings-github-link")
                                            .text_xs()
                                            .cursor_pointer()
                                            .text_color(muted)
                                            .hover(|s| s.text_color(fg))
                                            .child("GitHub ↗")
                                            .on_mouse_down(MouseButton::Left, |_, _window, cx| {
                                                cx.open_url("https://github.com/smelt-ai/smelt");
                                            }),
                                    )
                                    .child(
                                        div()
                                            .id("settings-report-issue-link")
                                            .text_xs()
                                            .cursor_pointer()
                                            .text_color(muted)
                                            .hover(|s| s.text_color(fg))
                                            .child("反馈问题 ↗")
                                            .on_mouse_down(MouseButton::Left, |_, _window, cx| {
                                                cx.open_url(
                                                    "https://github.com/smelt-ai/smelt/issues/new/choose",
                                                );
                                            }),
                                    ),
                            )
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .child(check_btn)
                                    .children(restart_btn),
                            ),
                    )
                    .children((!status_text.is_empty()).then(|| {
                        div().text_xs().text_color(muted).child(status_text)
                    }))
                    .children(progress_bar)
            }))
                .item(SettingItem::render(move |_, _, cx: &mut App| {
                    let outdated = daemon_entity.read(cx).daemon_outdated;
                    let upgrading = daemon_entity.read(cx).daemon_upgrading;
                    let upgrade_msg = daemon_entity.read(cx).daemon_upgrade_msg.clone();
                    let upgrade_entity = daemon_entity.clone();
                    let restart_entity = daemon_entity.clone();
                    // 首选：无缝升级（exec 交接，会话不中断）。
                    let upgrade_daemon_btn = (outdated == Some(true)).then(|| {
                        btn(
                            "upgrade-daemon",
                            if upgrading { "升级中…".into() } else { "无缝升级".into() },
                        )
                        .when(!upgrading, |b| {
                            b.on_mouse_down(MouseButton::Left, move |_, _window, cx: &mut App| {
                                upgrade_entity.update(cx, |this, cx| {
                                    this.upgrade_daemon_seamless(cx);
                                });
                            })
                        })
                    });
                    // 硬重启：常驻入口（守护卡死 / 想强制换二进制时用），会断会话。
                    // 不受版本是否落后限制；点击走二次确认弹窗兜底。
                    // 用 btn_hover：自定义 hover 色，避免在已有 hover 的 btn 上再链式 .hover() 崩。
                    let restart_daemon_btn = btn_hover(
                        "restart-daemon",
                        "重启守护进程".into(),
                        Hsla::from(crate::ui_theme::tint(crate::ui_theme::red(), 0x40)),
                    )
                        .text_color(rgb(crate::ui_theme::red()))
                        .bg(Hsla::from(crate::ui_theme::tint(crate::ui_theme::red(), 0x24)))
                        .on_mouse_down(MouseButton::Left, move |_, _window, cx: &mut App| {
                            restart_entity.update(cx, |this, cx| {
                                this.show_daemon_restart_confirm = true;
                                cx.notify();
                            });
                        });
                    let status_text = match outdated {
                        Some(true) => "版本落后于当前安装包，升级守护后新功能/修复才生效。".to_string(),
                        Some(false) => "已是最新。".to_string(),
                        None => "检测中…".to_string(),
                    };
                    // 运行信息：守护没起就明说，别留空白让人以为没加载出来。
                    let info = daemon_entity.read(cx).daemon_info.clone();
                    let info_text = match (&info, outdated) {
                        (Some(i), _) => Some(daemon_info_line(i)),
                        // outdated 已探测完但拿不到 info → 守护确实没跑。
                        (None, Some(_)) => Some("未在运行（新建终端时会自动拉起）".to_string()),
                        (None, None) => None,
                    };
                    // 「N 个会话」不只是个数字——守护持有的会话不全是侧栏认领的
                    // （测试跑出来的孤儿、忘了关的临时会话也计在内），点开能看
                    // 到明细并单独清理，不用被迫走「重启守护进程」那种连坐所有
                    // 会话的核选项。守护没起来就没什么可看的，不露这个入口。
                    let manage_sessions_entity = daemon_entity.clone();
                    let manage_sessions_link = info.is_some().then(|| {
                        div()
                            .text_xs()
                            .cursor_pointer()
                            .text_color(muted)
                            .hover(|s| s.text_color(fg))
                            .child("查看/清理会话 ›")
                            .on_mouse_down(MouseButton::Left, move |_, _window, cx| {
                                manage_sessions_entity.update(cx, |ws, cx| {
                                    ws.open_session_manager(cx);
                                });
                            })
                    });

                    v_flex()
                        .w_full()
                        .gap_3()
                        .child(
                            h_flex()
                                .w_full()
                                .justify_between()
                                .items_center()
                                .child(div().text_sm().text_color(fg).child("守护进程（smeltd）"))
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .items_center()
                                        .children(upgrade_daemon_btn)
                                        .child(restart_daemon_btn),
                                ),
                        )
                        .child(div().text_xs().text_color(muted).child(status_text))
                        .children(
                            info_text.map(|t| div().text_xs().text_color(muted).child(t)),
                        )
                        .children(manage_sessions_link)
                        .children(upgrade_msg.map(|m| div().text_xs().text_color(muted).child(m)))
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child("「重启守护进程」会断开并终止当前所有终端会话（含正在跑的 agent）；若只是版本落后，优先用会话不中断的「无缝升级」。"),
                        )
                })),
        );

        // —— Agent 集成：ACP 启动命令 + 审批通知 + Claude hooks 安装/还原 ——
        let agent_page = SettingPage::new("Agent 集成").group(
            SettingGroup::new()
                .item(
                    SettingItem::new(
                        "审批时弹出通知",
                        SettingField::switch(
                            |cx: &App| {
                                cx.try_global::<AgentUiConfig>()
                                    .map(|c| c.notify_awaiting)
                                    .unwrap_or(true)
                            },
                            |v: bool, cx: &mut App| {
                                apply_agent_ui(|c| c.notify_awaiting = v, cx);
                            },
                        ),
                    )
                    .description(
                        "状态通道进入「等你批准 / 等你输入」时，用应用内 Notification 弹出提示\
                         （不依赖系统横幅）。",
                    )
                    .keywords(["通知", "notification", "审批"]),
                )
                .item(acp_cmd_setting_item(AcpAgentKind::Claude))
                .item(acp_cmd_setting_item(AcpAgentKind::Copilot))
                .item(acp_cmd_setting_item(AcpAgentKind::Codex))
                .item(acp_cmd_setting_item(AcpAgentKind::Grok))
                .item(
                    SettingItem::render({
                        let profile_editor_entity = entity.clone();
                        move |_, window, cx: &mut App| {
                            let muted = cx.theme().muted_foreground;
                            let border = cx.theme().border;
                            let fg = cx.theme().foreground;
                            let popover = cx.theme().popover;
                            let secondary = cx.theme().secondary;
                            let danger = cx.theme().danger;
                            let danger_fg = cx.theme().danger_foreground;
                            let field_w = {
                                let vw = f32::from(window.viewport_size().width);
                                let w = (vw - 250. - 80.).clamp(360., 720.);
                                px(w)
                            };
                            profile_editor_entity.update(cx, |ws, cx| {
                                ws.ensure_profile_inputs(window, cx);
                                let Some(inputs) = ws.profile_inputs.as_ref() else {
                                    return div().into_any_element();
                                };
                                let mut col = v_flex()
                                    .w(field_w)
                                    .gap_3()
                                    .child(
                                        v_flex()
                                            .w(field_w)
                                            .gap_1()
                                            .child(
                                                div()
                                                    .text_sm()
                                                    .font_semibold()
                                                    .text_color(fg)
                                                    .child("手动添加 workspace"),
                                            )
                                            .child(
                                                div().w(field_w).text_sm().text_color(muted).child(
                                                    "同一家 agent 可以同时用好几个 workspace（比如 Claude \
                                                     默认的 ~/.claude 之外再开一个 ~/.claude-quant）。选好\
                                                     agent 类型、填上目录，启动命令自动拼好，不用自己写 \
                                                     shell 语法。「新建对话」菜单和历史会话页都会多出对应\
                                                     的入口。",
                                                ),
                                            ),
                                    );

                                let kind_w = px(120.);
                                let name_w = px(140.);
                                let del_w = px(28.);
                                let dir_w = field_w - kind_w - name_w - del_w - px(56.);
                                let mono = terminal_view::font_family();

                                let mut list = v_flex()
                                    .w(field_w)
                                    .gap_2()
                                    .p_3()
                                    .rounded_lg()
                                    .border_1()
                                    .border_color(border)
                                    .bg(secondary)
                                    .child(
                                        h_flex()
                                            .w_full()
                                            .gap_2()
                                            .items_center()
                                            .text_xs()
                                            .text_color(muted)
                                            .child(div().w(kind_w).child("Agent"))
                                            .child(div().w(name_w).child("名称"))
                                            .child(div().w(dir_w).child("Workspace 目录"))
                                            .child(div().w(del_w)),
                                    );

                                let profiles = cx.global::<AgentUiConfig>().profiles.clone();
                                for (ix, ((label, dir), p)) in
                                    inputs.rows.iter().zip(profiles.iter()).enumerate()
                                {
                                    let row_ix = ix;
                                    let kind_entity = profile_editor_entity.clone();
                                    let del_entity = profile_editor_entity.clone();
                                    let current_kind = p.kind();
                                    list = list.child(
                                        h_flex()
                                            .id(("profile-row", row_ix))
                                            .w_full()
                                            .gap_2()
                                            .items_center()
                                            .child(
                                                Button::new(("profile-kind", row_ix))
                                                    .ghost()
                                                    .small()
                                                    .w(kind_w)
                                                    .label(current_kind.short_label())
                                                    .dropdown_menu(move |mut menu, _window, _cx| {
                                                        for kind in AcpAgentKind::ALL {
                                                            let kind_entity = kind_entity.clone();
                                                            menu = menu.item(
                                                                PopupMenuItem::new(kind.label())
                                                                    .on_click(move |_ev, _window, cx| {
                                                                        kind_entity.update(cx, |ws, cx| {
                                                                            ws.set_profile_kind(
                                                                                row_ix, kind, cx,
                                                                            );
                                                                        });
                                                                    }),
                                                            );
                                                        }
                                                        menu
                                                    }),
                                            )
                                            .child(Input::new(label).w(name_w))
                                            .child(
                                                Input::new(dir).w(dir_w).font_family(mono.clone()),
                                            )
                                            .child(
                                                div()
                                                    .id(("del-profile", row_ix))
                                                    .size(del_w)
                                                    .flex()
                                                    .flex_none()
                                                    .items_center()
                                                    .justify_center()
                                                    .rounded_md()
                                                    .cursor_pointer()
                                                    .text_sm()
                                                    .text_color(muted)
                                                    .hover(|s| s.bg(danger).text_color(danger_fg))
                                                    .child("×")
                                                    .on_mouse_down(
                                                        MouseButton::Left,
                                                        move |_, _, cx: &mut App| {
                                                            del_entity.update(cx, |ws, cx| {
                                                                ws.remove_profile(row_ix, cx);
                                                            });
                                                        },
                                                    ),
                                            ),
                                    );
                                }
                                col = col.child(list);
                                let add_entity = profile_editor_entity.clone();
                                col.child(
                                    div()
                                        .id("add-profile")
                                        .h(px(36.))
                                        .w(field_w)
                                        .px_3()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded_lg()
                                        .cursor_pointer()
                                        .text_sm()
                                        .text_color(fg)
                                        .bg(popover)
                                        .border_1()
                                        .border_color(border)
                                        .hover(|s| s.bg(border))
                                        .child("+ 添加 workspace")
                                        .on_mouse_down(MouseButton::Left, move |_, _, cx: &mut App| {
                                            add_entity.update(cx, |ws, cx| ws.add_profile(cx));
                                        }),
                                )
                                .into_any_element()
                            })
                        }
                    })
                    .keywords(["workspace", "claude-quant", "config dir", "多工作区", "agent"]),
                )
                .item(SettingItem::render(move |_, _, cx: &mut App| {
                    let installed = claude_hooks_installed();
                    let (fg, muted, border) = {
                        let t = cx.theme();
                        (t.foreground, t.muted_foreground, t.border)
                    };
                    let status = if installed {
                        "已安装 smelt-notify → Claude hooks"
                    } else {
                        "未安装（结构面板只能靠标题猜测，hook 事实不会上报）"
                    };
                    let status_color: Hsla = if installed {
                        rgb(crate::ui_theme::green()).into()
                    } else {
                        muted
                    };
                    v_flex()
                        .gap_2()
                        .child(
                            div()
                                .text_sm()
                                .text_color(status_color)
                                .child(status),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child(format!(
                                    "路径：{}",
                                    smelt_notify_path().display()
                                )),
                        )
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    div()
                                        .id("install-claude-hooks")
                                        .px_3()
                                        .py(px(6.))
                                        .rounded_md()
                                        .cursor_pointer()
                                        .border_1()
                                        .border_color(border)
                                        .bg(crate::ui_theme::tint(crate::ui_theme::green(), 0x22))
                                        .text_sm()
                                        .text_color(rgb(crate::ui_theme::green()))
                                        .hover(|s| s.opacity(0.9))
                                        .child(if installed {
                                            "重新安装 hooks"
                                        } else {
                                            "安装 hooks"
                                        })
                                        .on_mouse_down(MouseButton::Left, move |_, _, cx: &mut App| {
                                            match install_claude_hooks() {
                                                Ok(()) => {
                                                    // 触发设置页重绘
                                                    cx.refresh_windows();
                                                }
                                                Err(e) => {
                                                    eprintln!("[workspace] 安装 hooks 失败：{e}");
                                                    cx.refresh_windows();
                                                }
                                            }
                                        }),
                                )
                                .child(
                                    div()
                                        .id("uninstall-claude-hooks")
                                        .px_3()
                                        .py(px(6.))
                                        .rounded_md()
                                        .cursor_pointer()
                                        .border_1()
                                        .border_color(border)
                                        .text_sm()
                                        .text_color(fg)
                                        .hover(|s| s.bg(border))
                                        .child("还原 hooks")
                                        .on_mouse_down(MouseButton::Left, move |_, _, cx: &mut App| {
                                            match uninstall_claude_hooks() {
                                                Ok(()) => cx.refresh_windows(),
                                                Err(e) => {
                                                    eprintln!("[workspace] 还原 hooks 失败：{e}");
                                                    cx.refresh_windows();
                                                }
                                            }
                                        }),
                                ),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child(
                                    "写入 ~/.claude/settings.json（仅增删 smelt-notify 条目，其它 hook 保留）。\
                                     还原 = 移除这些条目。改完后新开 Claude 会话生效。",
                                ),
                        )
                        .into_any_element()
                })),
        );

        // —— 远程：本机 → 跨网 WebRTC → 临时 CF → 写入 → 分享卡片（复制 + 扫码）——
        let signal_http_input = self.signal_http_input.clone();
        let remote_page = SettingPage::new("远程").group(
            SettingGroup::new().items(vec![
                SettingItem::new(
                    "开启远程",
                    SettingField::switch(
                        |cx: &App| cx.global::<RemoteConfig>().enabled,
                        |v: bool, cx: &mut App| apply_remote_toggle(v, cx),
                    ),
                )
                .description(
                    "打开后生成本机分享能力（局域网 / 跨网都依赖）。关掉会停止所有分享。",
                ),
                // 信令地址完整交互：输入 + 保存 + 探测 + 状态
                SettingItem::render({
                    let signal_http_input = signal_http_input.clone();
                    move |_, _, cx: &mut App| {
                        let muted = cx.theme().muted_foreground;
                        let fg = cx.theme().foreground;
                        let danger = cx.theme().danger;
                        let secondary = cx.theme().secondary;
                        let border = cx.theme().border;
                        let success = cx.theme().success;
                        let cfg = cx.global::<RemoteConfig>().clone();
                        let probe = cx
                            .try_global::<SignalProbeState>()
                            .cloned()
                            .unwrap_or_default();
                        let saved = normalize_signal_http(&cfg.signal_http);
                        let draft = signal_http_from_input(signal_http_input.as_ref(), cx);
                        let configured = signal_http_ok(&saved);
                        let draft_ok = signal_http_ok(&draft);
                        let dirty = draft != saved;

                        let status_line = if probe.probing {
                            ("探测中…".to_string(), muted)
                        } else if let Some(ok) = probe.ok {
                            let msg = probe.message.clone().unwrap_or_default();
                            if ok {
                                (format!("✓ {msg}"), success)
                            } else {
                                (format!("✗ {msg}"), danger)
                            }
                        } else if configured {
                            (format!("已保存：{saved}"), muted)
                        } else {
                            ("未配置 — 跨网 WebRTC 需要信令地址".into(), danger)
                        };

                        let input_entity = signal_http_input.clone();
                        let input_for_save = signal_http_input.clone();
                        let input_for_probe = signal_http_input.clone();
                        let input_for_clear = signal_http_input.clone();

                        v_flex()
                            .w_full()
                            .gap_2()
                            .p_3()
                            .rounded(px(8.))
                            .border_1()
                            .border_color(border)
                            .bg(secondary.opacity(0.35))
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(fg)
                                    .child("信令服务地址"),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .child(
                                        "自部署的 smelt-signal 根 URL（无内置默认）。\
                                         例：https://signal.example.com · 部署见 deploy/signal/",
                                    ),
                            )
                            .child(
                                h_flex()
                                    .w_full()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w(px(0.))
                                            .children(
                                                input_entity
                                                    .as_ref()
                                                    .map(|s| Input::new(s).small()),
                                            ),
                                    )
                                    .child({
                                        let can_save = draft_ok && dirty;
                                        let label = if dirty {
                                            "保存".to_string()
                                        } else {
                                            "已保存".to_string()
                                        };
                                        let b = btn("signal-save", label).flex_shrink_0();
                                        if can_save {
                                            b.on_mouse_down(
                                                MouseButton::Left,
                                                move |_, window, cx: &mut App| {
                                                    let v = signal_http_from_input(
                                                        input_for_save.as_ref(),
                                                        cx,
                                                    );
                                                    if !signal_http_ok(&v) {
                                                        window.push_notification(
                                                            Notification::error(
                                                                "请填写 https:// 或 http:// 开头的地址",
                                                            ),
                                                            cx,
                                                        );
                                                        return;
                                                    }
                                                    apply_signal_http(v, true, cx);
                                                    window.push_notification(
                                                        Notification::success("信令地址已保存"),
                                                        cx,
                                                    );
                                                },
                                            )
                                        } else {
                                            b.opacity(0.45)
                                        }
                                    })
                                    .child({
                                        let can_probe = draft_ok && !probe.probing;
                                        let label = if probe.probing {
                                            "探测中…".to_string()
                                        } else {
                                            "探测连通".to_string()
                                        };
                                        let b = btn("signal-probe", label).flex_shrink_0();
                                        if can_probe {
                                            b.on_mouse_down(
                                                MouseButton::Left,
                                                move |_, _window, cx: &mut App| {
                                                    let v = signal_http_from_input(
                                                        input_for_probe.as_ref(),
                                                        cx,
                                                    );
                                                    // 探测前先落盘，避免配置与探测目标不一致
                                                    if signal_http_ok(&v) {
                                                        apply_signal_http(v.clone(), false, cx);
                                                    }
                                                    probe_signal_http(v, cx);
                                                },
                                            )
                                        } else {
                                            b.opacity(0.45)
                                        }
                                    })
                                    .child(
                                        btn("signal-clear", "清除".into())
                                            .flex_shrink_0()
                                            .text_color(muted)
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                move |_, window, cx: &mut App| {
                                                    if let Some(inp) = input_for_clear.as_ref() {
                                                        inp.update(cx, |s, cx| {
                                                            s.set_value("", window, cx);
                                                        });
                                                    }
                                                    apply_signal_http(String::new(), true, cx);
                                                    cx.set_global(SignalProbeState::default());
                                                    window.push_notification(
                                                        Notification::success("已清除信令地址"),
                                                        cx,
                                                    );
                                                },
                                            ),
                                    ),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(status_line.1)
                                    .child(status_line.0),
                            )
                            .when(dirty && draft_ok, |el| {
                                el.child(
                                    div()
                                        .text_xs()
                                        .text_color(muted)
                                        .child("有未保存修改，点「保存」写入配置（跨网开启时会重启 bridge）。"),
                                )
                            })
                    }
                }),
                SettingItem::new(
                    "跨网（WebRTC）",
                    SettingField::switch(
                        |cx: &App| cx.global::<RemoteConfig>().webrtc_enabled,
                        |v: bool, cx: &mut App| apply_webrtc_toggle(v, cx),
                    ),
                )
                .description(
                    "推荐：手机蜂窝也能连。经上方信令握手，数据优先点对点到本机；\
                     打开后生成可复制 / 扫码的跨网链接。请先配置并保存信令地址。",
                ),
                SettingItem::new(
                    "临时 Cloudflare（高级）",
                    SettingField::switch(
                        |cx: &App| cx.global::<RemoteConfig>().tunnel_enabled,
                        |v: bool, cx: &mut App| apply_tunnel_toggle(v, cx),
                    ),
                )
                .description(
                    "Quick Tunnel 临时公网链接，不稳定时优先用上方「跨网 WebRTC」。\
                     需要本机 cloudflared；打开时会自动开启「远程」。",
                ),
                SettingItem::render(move |_, _, cx: &mut App| {
                    let muted = cx.theme().muted_foreground;
                    let fg = cx.theme().foreground;
                    let cmd = "brew install cloudflared";
                    let label = copy_btn_label("copy-brew-cloudflared", "复制", cx);
                    h_flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child("Cloudflare 未安装时："),
                        )
                        .child(
                            div()
                                .px_2()
                                .py_0p5()
                                .rounded(px(4.))
                                .bg(cx.theme().secondary)
                                .text_xs()
                                .font_family("Menlo")
                                .text_color(fg)
                                .child(cmd),
                        )
                        .child(
                            btn("copy-brew-cloudflared", label)
                                .flex_shrink_0()
                                .on_mouse_down(MouseButton::Left, move |_, window, cx: &mut App| {
                                    copy_with_feedback(
                                        "brew install cloudflared".into(),
                                        "copy-brew-cloudflared",
                                        "已复制安装命令",
                                        window,
                                        cx,
                                    );
                                }),
                        )
                }),
                SettingItem::new(
                    "允许远程写入",
                    SettingField::switch(
                        |cx: &App| cx.global::<RemoteConfig>().write_enabled,
                        |v: bool, cx: &mut App| apply_write_toggle(v, cx),
                    ),
                )
                .description(
                    "链接持有者可在手机上输入、批准/拒绝权限。分享即授权。\
                     切换后会自动换一条新链接（旧链接失效）。",
                ),
                // 分享卡片：WebRTC 优先 → CF → 本机；复制 + 二维码
                SettingItem::render(move |_, _, cx: &mut App| {
                    let cfg = cx.global::<RemoteConfig>().clone();
                    let remote = cx.global::<RemoteRuntimeState>().clone();
                    let tunnel = cx.global::<TunnelRuntimeState>().clone();
                    let webrtc = cx
                        .try_global::<WebrtcRuntimeState>()
                        .cloned()
                        .unwrap_or_default();
                    let danger = cx.theme().danger;
                    let muted = cx.theme().muted_foreground;
                    let fg = cx.theme().foreground;

                    if !cfg.enabled && !cfg.tunnel_enabled && !cfg.webrtc_enabled {
                        return div()
                            .text_xs()
                            .text_color(muted)
                            .child("打开「开启远程」或「跨网（WebRTC）」后，这里出现分享链接与二维码。");
                    }

                    // WebRTC 准备中
                    if cfg.webrtc_enabled && webrtc.connecting {
                        return div()
                            .text_xs()
                            .text_color(muted)
                            .child("正在准备跨网链接…（建房 + 启动 bridge）");
                    }

                    let preparing = tunnel.connecting
                        || (cfg.enabled
                            && !cfg.webrtc_enabled
                            && remote.error.is_none()
                            && !remote.token.as_ref().is_some_and(|t| !t.is_empty()));

                    if preparing {
                        let msg = if tunnel.connecting {
                            "正在准备 Cloudflare 链接…（最多约 30 秒）"
                        } else {
                            "正在准备分享链接…"
                        };
                        return div().text_xs().text_color(muted).child(msg);
                    }

                    if let Some(err) = webrtc
                        .error
                        .as_ref()
                        .or(remote.error.as_ref())
                        .or(tunnel.error.as_ref())
                    {
                        let need_cloudflared = err.contains("没找到 cloudflared")
                            || err.contains("brew install cloudflared")
                            || err.contains("SMELT_CLOUDFLARED");
                        let mut box_ = v_flex()
                            .gap_2()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(danger)
                                    .child(format!("出了点问题：{err}")),
                            )
                            .child(
                                btn("retry-remote", "重试".into()).on_mouse_down(
                                    MouseButton::Left,
                                    |_, _window, cx: &mut App| retry_remote_setup(cx),
                                ),
                            );
                        if need_cloudflared {
                            let err_label =
                                copy_btn_label("copy-brew-on-err", "复制安装命令", cx);
                            box_ = box_.child(
                                h_flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .px_2()
                                            .py_0p5()
                                            .rounded(px(4.))
                                            .bg(cx.theme().secondary)
                                            .text_xs()
                                            .font_family("Menlo")
                                            .text_color(fg)
                                            .child("brew install cloudflared"),
                                    )
                                    .child(
                                        btn("copy-brew-on-err", err_label)
                                            .flex_shrink_0()
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                move |_, window, cx: &mut App| {
                                                    copy_with_feedback(
                                                        "brew install cloudflared".into(),
                                                        "copy-brew-on-err",
                                                        "已复制安装命令",
                                                        window,
                                                        cx,
                                                    );
                                                },
                                            ),
                                    ),
                            );
                        }
                        return box_;
                    }

                    // 主链接优先级：WebRTC → CF → 本机
                    let webrtc_url = webrtc
                        .share_url
                        .clone()
                        .filter(|_| cfg.webrtc_enabled);
                    let token = remote.token.clone().filter(|t| !t.is_empty());
                    let public = tunnel
                        .url
                        .as_ref()
                        .filter(|_| cfg.tunnel_enabled)
                        .and_then(|u| token.as_ref().map(|t| format!("{u}/?token={t}")));
                    let local = remote
                        .addr
                        .as_ref()
                        .and_then(|a| token.as_ref().map(|t| format!("http://{a}/?token={t}")));

                    let primary = webrtc_url
                        .clone()
                        .or_else(|| public.clone())
                        .or_else(|| local.clone());

                    let Some(primary) = primary else {
                        return v_flex()
                            .gap_2()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .child("还没有可用的分享链接。"),
                            )
                            .child(
                                btn("retry-remote-empty", "重试".into()).on_mouse_down(
                                    MouseButton::Left,
                                    |_, _window, cx: &mut App| retry_remote_setup(cx),
                                ),
                            );
                    };

                    let (scope, mode) = if webrtc_url.is_some() {
                        (
                            "跨网 WebRTC（手机扫码 / 蜂窝可用）",
                            if remote.write {
                                "可写入"
                            } else {
                                "只读"
                            },
                        )
                    } else if public.is_some() {
                        (
                            "临时 Cloudflare 公网",
                            if tunnel.write { "可写入" } else { "只读" },
                        )
                    } else {
                        (
                            "仅本机 / 局域网",
                            if remote.write { "可写入" } else { "只读" },
                        )
                    };

                    let primary_copy = primary.clone();
                    // 仅展示后台预生成的 RGB 二维码（绝不在 UI 线程现算 QR）
                    let qr_png = webrtc.qr_png.clone().filter(|_| webrtc_url.is_some());

                    let mut card = v_flex().gap_2();

                    // 二维码 + 链接区
                    let mut row = h_flex().items_start().gap_3();
                    if let Some(png) = qr_png {
                        if !png.is_empty() {
                            row = row.child(
                                div()
                                    .p_2()
                                    .rounded(px(8.))
                                    // 二维码底必须是纯白，两种主题都一样：
                                    // 深色底上的二维码扫不出来。别跟着色板走。
                                    .bg(gpui::rgb(0xffffff))
                                    .child(
                                        img(std::sync::Arc::new(Image::from_bytes(
                                            ImageFormat::Png,
                                            png,
                                        )))
                                        .w(px(132.))
                                        .h(px(132.)),
                                    ),
                            );
                        }
                    }
                    row = row.child(
                        v_flex()
                            .gap_1p5()
                            .min_w(px(0.))
                            .flex_1()
                            .child(
                                div()
                                    .max_w(px(280.))
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis_middle()
                                    .text_xs()
                                    .text_color(fg)
                                    .child(primary.clone()),
                            )
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        btn(
                                            "copy-share-link",
                                            copy_btn_label("copy-share-link", "复制链接", cx),
                                        )
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            move |_, window, cx: &mut App| {
                                                copy_with_feedback(
                                                    primary_copy.clone(),
                                                    "copy-share-link",
                                                    "已复制分享链接",
                                                    window,
                                                    cx,
                                                );
                                            },
                                        ),
                                    ),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .child(format!("{scope} · {mode}")),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .child("手机扫码打开；或复制链接到浏览器。"),
                            ),
                    );
                    card = card.child(row);

                    // 次要：WebRTC 开启时仍可看本机链接
                    if webrtc_url.is_some() {
                        if let Some(local_link) = local.clone() {
                            let local_copy = local_link.clone();
                            card = card.child(
                                h_flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .max_w(px(260.))
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .text_ellipsis_middle()
                                            .text_xs()
                                            .text_color(muted)
                                            .child(format!("本机：{local_link}")),
                                    )
                                    .child(
                                        btn(
                                            "copy-local-link",
                                            copy_btn_label("copy-local-link", "复制本机", cx),
                                        )
                                        .flex_shrink_0()
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            move |_, window, cx: &mut App| {
                                                copy_with_feedback(
                                                    local_copy.clone(),
                                                    "copy-local-link",
                                                    "已复制本机链接",
                                                    window,
                                                    cx,
                                                );
                                            },
                                        ),
                                    ),
                            );
                        }
                    } else if let (Some(_), Some(local_link)) = (public, local) {
                        let local_copy = local_link.clone();
                        card = card.child(
                            h_flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .max_w(px(280.))
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis_middle()
                                        .text_xs()
                                        .text_color(muted)
                                        .child(format!("本机：{local_link}")),
                                )
                                .child(
                                    btn(
                                        "copy-local-link",
                                        copy_btn_label("copy-local-link", "复制本机", cx),
                                    )
                                    .flex_shrink_0()
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        move |_, window, cx: &mut App| {
                                            copy_with_feedback(
                                                local_copy.clone(),
                                                "copy-local-link",
                                                "已复制本机链接",
                                                window,
                                                cx,
                                            );
                                        },
                                    ),
                                ),
                        );
                    }

                    card
                }),
            ]),
        );

        div().size_full().child(
            // id 里带 nonce：见 `settings_page_nonce`，用来强制跳到 settings_page_ix。
            Settings::new(("settings", self.settings_page_nonce))
                .default_selected_index(SelectIndex {
                    page_ix: self.settings_page_ix,
                    group_ix: None,
                })
                .pages(vec![
                    appearance_page,
                    pet_page,
                    launch_page,
                    agent_page,
                    update_page,
                    remote_page,
                ]),
        )
    }

    /// 打开独立设置窗口：已经开着就聚焦提到前台，不重复开第二扇。窗口只是个薄壳
    /// （[`SettingsWindow`]），真正的状态（颜色选择器/LLM 输入框等）还挂在这个
    /// Workspace 实体上没挪窝，薄壳每次渲染都转手调回来，天然跟主窗口保持同步。
    ///
    /// 必须用 `cx.defer` 推迟到当前这轮 `Workspace::update` 彻底返回之后再开窗：
    /// 这里被点齿轮的 `cx.listener` 调用时，`Workspace` 这个 entity 正被 update
    /// 占着；若同步 `cx.open_window`，新窗口首帧 `SettingsWindow::render` 里会
    /// 马上又对同一个 `Workspace` entity 调 `update`，两层嵌套 update 撞上 GPUI
    /// 的重入保护直接 panic 崩溃（"cannot update ... while it is already being
    /// updated"）——这就是「点齿轮整个 app 崩溃」的真正原因。
    pub fn open_settings_window(&self, cx: &mut Context<Self>) {
        let workspace = cx.entity();
        cx.defer(move |cx| {
            if let Some(handle) = cx.try_global::<SettingsWindowHandle>().and_then(|h| h.0) {
                if handle.update(cx, |_, window, _| window.activate_window()).is_ok() {
                    return;
                }
            }
            // 启动项编辑需要较宽的命令输入区；侧栏约 250，内容区至少要能放下长命令。
            let bounds = WindowBounds::centered(size(px(900.), px(700.)), cx);
            let options = WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some("设置".into()),
                    ..Default::default()
                }),
                window_bounds: Some(bounds),
                ..Default::default()
            };
            let handle = cx
                .open_window(options, |window, cx| {
                    window.set_rem_size(px(19.));
                    let view = cx.new(|cx| SettingsWindow {
                        _observe_workspace: cx.observe(&workspace, |_, _, cx| cx.notify()),
                        workspace: workspace.clone(),
                    });
                    cx.new(|cx| Root::new(view, window, cx))
                })
                .expect("打开设置窗口失败");
            cx.set_global(SettingsWindowHandle(Some(handle)));
        });
    }
}

#[cfg(test)]
mod daemon_info_tests {
    use super::{daemon_info_line, fmt_uptime};
    use crate::terminal::DaemonInfo;

    #[test]
    fn fmt_uptime_picks_two_units() {
        assert_eq!(fmt_uptime(45), "45 秒");
        assert_eq!(fmt_uptime(600), "10 分钟");
        assert_eq!(fmt_uptime(3600 * 3 + 60 * 12), "3 小时 12 分");
        assert_eq!(fmt_uptime(86400 * 2 + 3600 * 5), "2 天 5 小时");
    }

    /// 老守护只回 version/exe_mtime：拿不到的字段整段省掉，不摆「未知」占位。
    #[test]
    fn old_daemon_without_new_fields_shows_only_version() {
        let info = DaemonInfo {
            version: Some("0.5.4".into()),
            ..Default::default()
        };
        assert_eq!(daemon_info_line(&info), "v0.5.4");
    }

    /// 全字段齐活：各段用 · 连起来，PID 和会话数都在。
    #[test]
    fn full_info_joins_all_parts() {
        let info = DaemonInfo {
            version: Some("0.5.4".into()),
            pid: Some(64954),
            started_at: Some(1_000_000),
            session_count: Some(5),
        };
        let line = daemon_info_line(&info);
        assert!(line.starts_with("v0.5.4 · PID 64954 · 启动于 "), "got {line}");
        assert!(line.contains("已运行 "), "got {line}");
        assert!(line.ends_with("· 5 个会话"), "got {line}");
    }

    /// 守护时钟比 GUI 快时不能算出天文数字（saturating_sub 兜底）。
    #[test]
    fn future_started_at_does_not_underflow() {
        let future = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 9999;
        let info = DaemonInfo {
            started_at: Some(future),
            ..Default::default()
        };
        assert!(daemon_info_line(&info).contains("已运行 0 秒"));
    }
}
