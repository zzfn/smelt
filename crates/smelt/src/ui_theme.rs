//! 设计稿语义色板（深色 / 浅色两套）——全局 UI 颜色的唯一出处。
//!
//! 深色色值来自 claude.ai/design「桌面开发者客户端设计」的 Desktop Client 定稿；
//! 浅色是按同一套语义层级（表面由深到浅、边由暗到亮、文字由强到弱）对偶推导的，
//! 语义位一一对应，所以调用方不需要关心当前是哪套。
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

use gpui::{rgb, rgba, Rgba};

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

/// 深色（设计稿定稿值）。
pub const DARK: Palette = Palette {
    bg_rail: 0x0a0b0e,
    bg_panel: 0x0f1116,
    bg_elev: 0x12141a,
    bg_bar: 0x14161b,
    bg_card: 0x171a20,
    bg_hover: 0x1a1d24,
    bg_selected: 0x20232b,
    bg_status: 0x0c0d11,

    border_dim: 0x1c1f27,
    border: 0x23262e,
    border_mid: 0x262a33,
    border_loud: 0x2a2e37,
    border_focus: 0x3a3f4a,
    border_selected: 0x33373f,

    text_bright: 0xe6e8ec,
    text: 0xd7dae0,
    text_mid: 0xc2c6cf,
    text_muted: 0x8b909c,
    text_faint: 0x5a606c,

    accent: 0xd98a4f,
    green: 0x66bb8a,
    yellow: 0xfebc2e,
    blue: 0x6ea8fe,
    purple: 0xb98be0,
    red: 0xe0736e,
    diff_add_text: 0x8fd6ac,
    on_accent: 0x12141a,

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

/// 浅色（按深色的语义层级对偶推导）。
///
/// 两条不同于「把深色反过来」的地方，别顺手改平：
/// - 卡片底是纯白、比舞台底更亮，靠「更亮」浮起；深色里卡片靠「更浅的灰」浮起。
/// - 语义色整体压深一档（accent/green/blue…），否则在近白底上对比不足看不清。
pub const LIGHT: Palette = Palette {
    bg_rail: 0xe7e9ed,
    bg_panel: 0xfafbfc,
    bg_elev: 0xf2f4f7,
    bg_bar: 0xeef0f4,
    bg_card: 0xffffff,
    bg_hover: 0xe9ecf1,
    bg_selected: 0xdde2ea,
    bg_status: 0xe4e7ec,

    border_dim: 0xe3e6ec,
    border: 0xdadee5,
    border_mid: 0xd1d6de,
    border_loud: 0xc4cad4,
    border_focus: 0x9aa2b0,
    border_selected: 0xafb7c3,

    text_bright: 0x14161b,
    text: 0x2b303a,
    text_mid: 0x454b57,
    text_muted: 0x6a7280,
    text_faint: 0x99a1ae,

    accent: 0xb2662a,
    green: 0x2c8459,
    yellow: 0x9a6b0a,
    blue: 0x2563c4,
    purple: 0x7a4aa8,
    red: 0xc0392f,
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

/// 当前色板。
pub fn palette() -> &'static Palette {
    if is_light() {
        &LIGHT
    } else {
        &DARK
    }
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
