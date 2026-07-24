//! 语义色板（深色 / 浅色两套）——全局 UI 颜色的唯一出处。
//!
//! 色值取自 **Discord**：深色 `#313338`（主区）/ `#2b2d31`（侧栏）/ `#1e1f22`
//! （rail）那一套灰蓝，强调色是它的 blurple `#5865f2`；浅色对应 Discord light。
//! 两套语义位一一对应，调用方不需要关心当前是哪套。
//!
//! 上一版色值来自 claude.ai/design「桌面开发者客户端设计」定稿（冷调近黑 +
//! 橙强调）。换成 Discord 是整体换语言，不是调参：**表面层级的方向都反过来了**
//! （见 DARK 的注释），两套混用会四不像。
//!
//! 布局各层（项目 rail / 会话列表 / 会话舞台 / inspector / 状态栏 / toast / diff）
//! 统一从这里取色，别再在各处写裸 `rgb(0x...)`——写死的深色在浅色模式下会花。
//!
//! 用法：全是**函数**不是常量（`ui_theme::bg_rail()`），因为色值要跟着
//! `set_light()` 在运行时切换。切换点见 settings.rs 的「主题模式」开关和 main() 初始化，
//! 切完必须 `cx.refresh_windows()` 才会重绘。

// 改版分阶段落地，常量与 helper 会陆续被各阶段启用；收尾阶段拿掉这行。
#![allow(dead_code)]

use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, Ordering};

use gpui::{Rgba, rgb, rgba};

use crate::AgentStatus;

/// 一套完整语义色板。字段即语义位，深浅两套必须一一对应填满。
pub struct Palette {
    // ---- 底色（表面层级） ----
    /// 项目 rail / inspector 图标条（深色下最深，浅色下最灰）。
    pub bg_rail: u32,
    /// 会话舞台底。
    pub bg_panel: u32,
    /// 会话列表 / inspector 面板底。
    pub bg_elev: u32,
    /// 标题栏底。
    pub bg_bar: u32,
    /// 卡片 / 胶囊底。
    pub bg_card: u32,
    /// hover / toast / 输入胶囊底。
    pub bg_hover: u32,
    /// **列表行**专用 hover 底：必须明显弱于 `bg_selected`，否则「鼠标划过的行」
    /// 和「当前选中的行」同时是两块灰，一眼分不出哪个才是当前。
    /// 不能直接复用 `bg_hover`——那个还给按钮 / 输入胶囊用，压淡了控件就没手感。
    pub bg_row_hover: u32,
    /// 选中行底（会话行选中、文件行选中）。
    pub bg_selected: u32,
    /// 状态栏 / 终端底条。
    pub bg_status: u32,

    // ---- 边色（对比由弱到强） ----
    /// 大区块分界线（列与列之间）。
    pub border_dim: u32,
    /// 标题栏下沿。
    pub border: u32,
    /// 卡片 / 胶囊描边。
    pub border_mid: u32,
    /// 输入框 / 虚线块描边。
    pub border_loud: u32,
    /// hover / 焦点描边。
    pub border_focus: u32,
    /// 选中行描边。
    pub border_selected: u32,

    // ---- 文字（由强到弱） ----
    /// 标题 / 强调正文。
    pub text_bright: u32,
    /// 正文。
    pub text: u32,
    /// 次级正文 / 按钮文字。
    pub text_mid: u32,
    /// 弱化说明 / mono 副标题。
    pub text_muted: u32,
    /// 最弱（占位、时间戳、快捷键提示）。
    pub text_faint: u32,

    // ---- 语义色 ----
    /// 主强调橙（品牌色：进行中、主按钮、激活态）。
    pub accent: u32,
    /// 绿：运行正常 / 通过 / diff 新增侧。
    pub green: u32,
    /// 黄：阻塞 / 等审批。
    pub yellow: u32,
    /// 蓝：链接 / 读类工具 / queued。
    pub blue: u32,
    /// 紫：agent 会话标识 / 模型胶囊。
    pub purple: u32,
    /// 红：删除侧 / 拒绝 / 等审批（最高优先级状态）。
    pub red: u32,
    /// diff 新增行的文字色（深色下比 green 亮一档用于深绿底；浅色下反之压深）。
    pub diff_add_text: u32,
    /// 实心彩色按钮（橙/绿/黄底）上的反色文字。
    pub on_accent: u32,

    // ---- diff 视图（git_panel 并排/统一视图） ----
    /// 新增行：前景 / 整行底 / 左色条 / 行内变化片段的加深底。
    pub diff_add_fg: u32,
    pub diff_add_bg: u32,
    pub diff_add_bar: u32,
    pub diff_add_hl: u32,
    /// 删除行：同上四件套。
    pub diff_del_fg: u32,
    pub diff_del_bg: u32,
    pub diff_del_bar: u32,
    pub diff_del_hl: u32,
    /// 上下文行前景。
    pub diff_ctx_fg: u32,
    /// hunk 头前景 / 底 / 激活 hunk 的描边。
    pub diff_hunk_fg: u32,
    pub diff_hunk_bg: u32,
    /// diff 元信息行（文件头等）前景。
    pub diff_meta_fg: u32,
    /// 并排视图里「此侧无对应行」的空白底。
    pub diff_empty_bg: u32,
    /// 并排视图中缝分隔线。
    pub diff_gutter: u32,
}

/// 深色（Discord 风格）。
///
/// **表面层级的方向跟大多数深色 UI 相反，别顺手「修正」**：Discord 是
/// 「主区最亮、越往边缘越暗」（舞台 > 会话列表 > rail），靠边缘压暗把注意力
/// 推向中间的内容区；常见做法（也是本项目上一版）是反过来让侧栏浮在舞台之上。
/// 两种都自洽，但混用就会既不像 Discord 也不像原来那套。
pub const DARK: Palette = Palette {
    bg_rail: 0x191a1d,
    bg_panel: 0x313338,
    bg_elev: 0x232428,
    bg_bar: 0x2b2d31,
    bg_card: 0x3a3c42,
    bg_hover: 0x35373c,
    // 只比 bg_elev(0x232428) 高一档：划过是「浮起一点」，选中才是「亮起来」。
    bg_row_hover: 0x2b2d31,
    bg_selected: 0x45474f,
    bg_status: 0x191a1d,

    border_dim: 0x202226,
    border: 0x2b2d31,
    border_mid: 0x43454b,
    border_loud: 0x54575e,
    border_focus: 0x6d7079,
    // 选中描边直接用 blurple：Discord 的选中态从来不是「灰上加灰」。
    border_selected: 0x5865f2,

    text_bright: 0xf2f3f5,
    text: 0xdbdee1,
    text_mid: 0xb5bac1,
    text_muted: 0x949ba4,
    text_faint: 0x80848e,

    // accent 从原来的橙换成 Discord 的 blurple——它是这套设计的灵魂色，
    // 主按钮 / 激活态 / 进行中全用它。随之 on_accent 必须翻成白：blurple
    // 底上压深色文字读不出来。
    accent: 0x5865f2,
    green: 0x23a55a,
    yellow: 0xf0b232,
    // 链接蓝走 Discord 的青蓝，跟 blurple 拉开——两个都偏蓝紫会分不清。
    blue: 0x00a8fc,
    // agent 标识紫同理往粉里偏，避免和 blurple 撞。
    purple: 0xc78ef7,
    red: 0xf23f43,
    diff_add_text: 0x8fd6ac,
    on_accent: 0xffffff,

    diff_add_fg: 0xb5e08a,
    diff_add_bg: 0x16261a,
    diff_add_bar: 0x4ba14b,
    diff_add_hl: 0x2f6b34,
    diff_del_fg: 0xf7a3ae,
    diff_del_bg: 0x2a1620,
    diff_del_bar: 0xc75c6a,
    diff_del_hl: 0x7a2836,
    diff_ctx_fg: 0xc0caf5,
    diff_hunk_fg: 0x7dcfff,
    diff_hunk_bg: 0x16202e,
    diff_meta_fg: 0x565f89,
    diff_empty_bg: 0x101218,
    diff_gutter: 0x2a2e3d,
};

/// 浅色（Discord light）。
///
/// 两条别顺手改平：
/// - 层级方向与深色一致：**舞台是纯白（最亮），侧栏和卡片反而更暗**。所以
///   卡片不能再取纯白——那样就跟舞台糊成一片，浮不起来了（上一版靠纯白浮起，
///   因为那时舞台不是纯白）。
/// - 同深色一样，层级明度拉得比 Discord light 更开：它的侧栏/标题栏/卡片
///   原样都是 `#f2f3f5` 同一个值，在这里会糊成一片。
/// - blurple 深浅两套用同一个值：它是品牌色，跟着模式变色就不是那个牌子了；
///   其余语义色（绿/黄/红/蓝/紫）照例压深一档，否则近白底上对比不足。
pub const LIGHT: Palette = Palette {
    bg_rail: 0xdcdfe4,
    bg_panel: 0xffffff,
    bg_elev: 0xeff1f4,
    bg_bar: 0xf6f7f9,
    bg_card: 0xf2f3f5,
    bg_hover: 0xe6e9ee,
    // 浅色下同理：划过只比 bg_elev(0xeff1f4) 压深一点，选中才明显。
    bg_row_hover: 0xe8eaee,
    bg_selected: 0xdadee6,
    bg_status: 0xdcdfe4,

    border_dim: 0xe8eaee,
    border: 0xdde0e5,
    border_mid: 0xc9ced6,
    border_loud: 0xb4bac4,
    border_focus: 0x8e9297,
    border_selected: 0x5865f2,

    text_bright: 0x060607,
    text: 0x2e3338,
    text_mid: 0x4e5058,
    text_muted: 0x5c5e66,
    text_faint: 0x80848e,

    accent: 0x5865f2,
    green: 0x248046,
    yellow: 0xb8850b,
    blue: 0x006ce7,
    purple: 0x9b59d0,
    red: 0xd83a3e,
    diff_add_text: 0x1c7a4a,
    on_accent: 0xffffff,

    diff_add_fg: 0x1c6b3c,
    diff_add_bg: 0xe4f6ea,
    diff_add_bar: 0x3f9a4a,
    diff_add_hl: 0xb4e3c4,
    diff_del_fg: 0x9c2b2b,
    diff_del_bg: 0xfdebee,
    diff_del_bar: 0xc75c6a,
    diff_del_hl: 0xf6c2ca,
    diff_ctx_fg: 0x3b4252,
    diff_hunk_fg: 0x1a6485,
    diff_hunk_bg: 0xe3f0f8,
    diff_meta_fg: 0x8a93a8,
    diff_empty_bg: 0xeceef2,
    diff_gutter: 0xd1d6de,
};

/// 当前是不是浅色。进程级全局态：色板要在任意渲染函数里同步读到，
/// 走 GPUI 的 Global 就得把 `&App` 一路传进每个取色点，代价不成比例
/// （跟 terminal.rs 的 `DARK_MODE` 是同一路数）。
static LIGHT_MODE: AtomicBool = AtomicBool::new(false);

/// 切换色板。调用方切完必须 `cx.refresh_windows()`，否则已绘制的界面不会更新。
pub fn set_light(light: bool) {
    LIGHT_MODE.store(light, Ordering::Relaxed);
}

pub fn is_light() -> bool {
    LIGHT_MODE.load(Ordering::Relaxed)
}

/// 把当前色板按语义位灌进 gpui-component 的主题，让组件（Input / Button /
/// Menu / 设置页 / 表格…）跟自绘部分同色。
///
/// 背景：全库有一百多处直接读 `cx.theme().border` 这类组件库色位，和自绘部分读
/// 的 `ui_theme::*` 是两套独立色值，同屏挨着就差一档。与其改掉那一百多处调用，
/// 不如在这里把组件库主题按语义对齐——色真源仍然只有本文件一个。
///
/// 只覆写「表面 / 边 / 文字 / 主按钮」这些会跟自绘部分并排出现的位；组件内部
/// 的细分态（各种 button_success_hover 之类）交给组件库自己推导，别越俎代庖。
pub fn apply_to_component_theme(cx: &mut gpui::App) {
    use gpui_component::{ActiveTheme as _, Theme};

    if !cx.has_global::<Theme>() {
        return;
    }
    let p = palette();
    let is_dark = cx.theme().mode.is_dark();
    let c = &mut cx.global_mut::<Theme>().colors;

    c.background = rgb(p.bg_panel).into();
    c.foreground = rgb(p.text).into();
    c.border = rgb(p.border_mid).into();
    c.muted = rgb(p.bg_card).into();
    c.muted_foreground = rgb(p.text_muted).into();
    c.popover = rgb(p.bg_card).into();
    c.popover_foreground = rgb(p.text).into();
    c.input = rgb(p.border_loud).into();
    c.ring = rgb(p.accent).into();
    c.selection = rgba((p.accent << 8) | 0x55).into();
    c.drop_target = rgba((p.accent << 8) | 0x33).into();

    // 主按钮 = blurple，次按钮 = 卡片底。深浅两套的前景色跟着 on_accent 走。
    c.primary = rgb(p.accent).into();
    c.primary_hover = rgb(shade(p.accent, if is_dark { 1.12 } else { 0.92 })).into();
    c.primary_active = rgb(shade(p.accent, if is_dark { 0.9 } else { 0.84 })).into();
    c.primary_foreground = rgb(p.on_accent).into();
    c.secondary = rgb(p.bg_card).into();
    c.secondary_hover = rgb(p.bg_hover).into();
    c.secondary_active = rgb(p.bg_selected).into();
    c.secondary_foreground = rgb(p.text).into();
    c.accent = rgb(p.bg_hover).into();
    c.accent_foreground = rgb(p.text_bright).into();
    c.danger = rgb(p.red).into();
    c.danger_foreground = rgb(p.on_accent).into();

    // 列表 / 侧栏：会话列表与文件树都在这一层，必须跟自绘的行底同色。
    c.list = rgb(p.bg_elev).into();
    c.list_hover = rgb(p.bg_hover).into();
    c.list_active = rgb(p.bg_selected).into();
    c.list_active_border = rgb(p.accent).into();
    c.list_even = rgb(p.bg_elev).into();
    c.list_head = rgb(p.bg_bar).into();
    c.sidebar = rgb(p.bg_elev).into();

    // colors 改完必须同步重算 tokens：组件内部有些位读的是 `tokens.*` 而不是
    // `colors.*`（如 checkbox 勾选态的方块填充用 tokens.primary）。不同步的话
    // 填充还是组件库默认的浅色，和被我们改成白的对勾（primary_foreground）撞成
    // 「白底 + 白对勾」——勾选了却看不见勾，就是这个回归。
    let theme = cx.global_mut::<Theme>();
    theme.tokens = (&theme.colors).into();
}

/// 把一个 RGB 按比例调亮（factor > 1）或压暗（< 1），逐通道饱和截断。
/// 只给上面的 hover / active 态推导用——手写十几个近似色不值当。
fn shade(color: u32, factor: f32) -> u32 {
    let ch = |shift: u32| {
        let v = ((color >> shift) & 0xff) as f32 * factor;
        (v.clamp(0.0, 255.0) as u32) << shift
    };
    ch(16) | ch(8) | ch(0)
}

/// 当前色板。
pub fn palette() -> &'static Palette {
    if is_light() { &LIGHT } else { &DARK }
}

/// 给每个语义位生成一个取当前色板的读取函数——调用点写 `ui_theme::bg_rail()`。
macro_rules! slots {
    ($($name:ident),* $(,)?) => {
        $(
            #[inline]
            pub fn $name() -> u32 {
                palette().$name
            }
        )*
    };
}

slots!(
    bg_rail,
    bg_panel,
    bg_elev,
    bg_bar,
    bg_card,
    bg_hover,
    bg_row_hover,
    bg_selected,
    bg_status,
    border_dim,
    border,
    border_mid,
    border_loud,
    border_focus,
    border_selected,
    text_bright,
    text,
    text_mid,
    text_muted,
    text_faint,
    accent,
    green,
    yellow,
    blue,
    purple,
    red,
    diff_add_text,
    on_accent,
    diff_add_fg,
    diff_add_bg,
    diff_add_bar,
    diff_add_hl,
    diff_del_fg,
    diff_del_bg,
    diff_del_bar,
    diff_del_hl,
    diff_ctx_fg,
    diff_hunk_fg,
    diff_hunk_bg,
    diff_meta_fg,
    diff_empty_bg,
    diff_gutter,
);

/// 给纯色叠低透明度，用于角标底、激活态背景等衍生色。
/// `alpha` 0–255；`tint(accent(), 0x22)` ≈ 设计稿的 rgba(217,138,79,.13)。
pub fn tint(color: u32, alpha: u8) -> Rgba {
    rgba((color << 8) | alpha as u32)
}

/// 「在底色上压一层薄纱」——深色下是白纱，浅色下是黑纱。
/// 收编原先散落的 `rgba(0xffffff0d)` 之类：那些在浅色底上是隐形的。
pub fn overlay(alpha: u8) -> Rgba {
    let base: u32 = if is_light() { 0x000000 } else { 0xffffff };
    rgba((base << 8) | alpha as u32)
}

/// Agent 五态状态色（等审批红 > 需处理黄 > 运行蓝 > 完成绿 > 空闲灰）。
/// 收敛自旧版散落的 0xef4444/0xf59e0b/0x4a9eff/0x22c55e 硬编码。
pub fn agent_status_color(status: AgentStatus) -> Rgba {
    rgb(agent_status_u32(status))
}

fn agent_status_u32(status: AgentStatus) -> u32 {
    match status {
        AgentStatus::WaitingApproval => red(),
        AgentStatus::NeedsAttention => yellow(),
        AgentStatus::Running => blue(),
        AgentStatus::Done => green(),
        AgentStatus::Idle => text_faint(),
    }
}

/// 同一套状态色的 (r, g, b) 形态，给 mac 菜单栏（status_item）用。
pub fn agent_status_rgb8(status: AgentStatus) -> (u8, u8, u8) {
    let c = agent_status_u32(status);
    ((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

/// 设计稿三态点（项目 rail / 会话行）：跑着 = 绿、阻塞 = 黄、空闲 = 灰。
/// 与五态 `agent_status_color` 是两套语义，别硬凑：这里「Running/Done」都算
/// 「活着且正常」（绿），两种等待都算「阻塞」（黄）。
pub fn session_dot_color(status: AgentStatus) -> Rgba {
    match status {
        AgentStatus::WaitingApproval | AgentStatus::NeedsAttention => rgb(yellow()),
        AgentStatus::Running | AgentStatus::Done => rgb(green()),
        AgentStatus::Idle => rgb(text_faint()),
    }
}

/// 项目名 → 稳定颜色：hash 到 6 色环，跟会话顺序无关。
pub fn project_color(name: &str) -> Rgba {
    let ring = [accent(), green(), blue(), purple(), yellow(), red()];
    let mut h = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut h);
    rgb(ring[(h.finish() % ring.len() as u64) as usize])
}
