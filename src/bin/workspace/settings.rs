//! 设置面板：外观 / 桌面宠物 / 启动参数 / 更新 四个分组，含嵌入式设置页
//! （主窗口右上角齿轮）和独立设置窗口共用的渲染逻辑。
//!
//! 跟 git_panel.rs / file_tree.rs 同一个套路：从 main.rs 拆出来的 `impl Workspace`
//! 方法 + 独立类型/函数，字段仍然声明在 main.rs 的 `Workspace` struct 里。
//!
//! 自动更新（`update_status`/`check_for_update`/`upgrade_daemon_seamless` 等）**不在
//! 这里**——那是应用级生命周期状态，不属于任何一个面板，仍留在 main.rs；这里的
//! 「更新」SettingPage 只是读它、展示它、提供按钮触发它。

use gpui::*;
use gpui::prelude::FluentBuilder;
use gpui::InteractiveElement;
use gpui_component::color_picker::{ColorPicker, ColorPickerEvent, ColorPickerState};
use gpui_component::input::Input;
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

/// 独立设置窗口的根 view：只是个薄壳，真正状态都还在传进来的 Workspace 实体上，
/// 每次渲染转手调 `render_settings_content`。
///
/// 但「转手调」不等于「跟着刷新」：`cx.notify()` 标脏的是 Workspace，设置窗口不在它
/// 的观察者名单里，不会因此重绘。所以得显式 observe 一把，否则后台改的状态——更新
/// 下载进度、守护进程检测结果——在设置窗口里会一直停在打开那一刻的样子。
pub struct SettingsWindow {
    workspace: Entity<Workspace>,
    _observe_workspace: Subscription,
}

impl Render for SettingsWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.workspace.update(cx, |ws, cx| ws.render_settings_content(cx))
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
        let btn = move |id: &'static str, label: String| {
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
                .hover(|s| s.bg(border))
                .child(label)
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
                            Theme::change(mode, None, cx);
                            terminal::set_dark_mode(mode.is_dark());
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
                    btn("restart-update", "立即重启更新".into())
                        .text_color(rgb(0x8fc7ff))
                        .bg(Hsla::from(rgba(0x4a9eff24)))
                        .hover(|s| s.bg(Hsla::from(rgba(0x4a9eff40))))
                        .on_mouse_down(MouseButton::Left, move |_, _window, cx: &mut App| {
                            if let updater::UpdateStatus::ReadyToInstall { staged_app, .. } = &status {
                                if updater::finalize_pending_update(staged_app).is_ok() {
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
                    // 兜底：硬重启（旧守护不支持无缝升级时的唯一出路，会断会话）。
                    let restart_daemon_btn = (outdated == Some(true)).then(|| {
                        btn("restart-daemon", "重启守护进程".into())
                            .text_color(rgb(0xff8f8f))
                            .bg(Hsla::from(rgba(0xef444424)))
                            .hover(|s| s.bg(Hsla::from(rgba(0xef444440))))
                            .on_mouse_down(MouseButton::Left, move |_, _window, cx: &mut App| {
                                restart_entity.update(cx, |this, cx| {
                                    this.show_daemon_restart_confirm = true;
                                    cx.notify();
                                });
                            })
                    });
                    let status_text = match outdated {
                        Some(true) => "版本落后于当前安装包，升级守护后新功能/修复才生效。".to_string(),
                        Some(false) => "已是最新。".to_string(),
                        None => "检测中…".to_string(),
                    };

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
                                        .children(restart_daemon_btn),
                                ),
                        )
                        .child(div().text_xs().text_color(muted).child(status_text))
                        .children(upgrade_msg.map(|m| div().text_xs().text_color(muted).child(m)))
                        .children((outdated == Some(true)).then(|| {
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child("「无缝升级」保留所有会话不中断；「重启守护进程」会断开并终止当前所有终端会话（含正在跑的 agent），仅当无缝升级不可用时使用。")
                        }))
                })),
        );

        div().size_full().child(
            // id 里带 nonce：见 `settings_page_nonce`，用来强制跳到 settings_page_ix。
            Settings::new(("settings", self.settings_page_nonce))
                .default_selected_index(SelectIndex {
                    page_ix: self.settings_page_ix,
                    group_ix: None,
                })
                .pages(vec![appearance_page, pet_page, launch_page, update_page]),
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
                    window.set_rem_size(px(18.));
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
