//! 桌面宠物 —— 独立的透明置顶浮窗，陪伴用户。
//!
//! 与主工作台完全解耦：单开一个 NSPanel（`WindowKind::PopUp`）——无边框、透明背景、
//! 浮在所有窗口之上并跟随所有 Space、不抢焦点。宠物本体用 GPUI 图元手绘（无需图片
//! 资源），smol 定时器逐帧驱动待机动画（呼吸 + 上下浮动 + 眨眼）。
//!
//! 交互：
//! - 按住宠物拖动 → `start_window_move` 让整窗走遍全屏；轻点一下 → 蹦跳 + 换句台词。
//! - 眼睛始终看向鼠标（用 `[NSEvent mouseLocation]` 取全屏光标）。
//! - 主程序通过 [`push_pet_message`] 投递事件，宠物主动「说」出来（气泡）。
//!
//! 配置见 [`PetConfig`]（显示开关 / 通知播报 / 颜色 / 大小），存 ~/.smelt/pet.json，
//! 由主窗口的设置面板编辑，跨窗口经全局单例共享。

use std::collections::VecDeque;
use std::f32::consts::TAU;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Timelike;
use gpui::*;
use smol::Timer;

/// 动画时钟间隔：约 20fps，足够顺滑且开销极低。
const FRAME: Duration = Duration::from_millis(50);
/// 每帧相位步进（弧度）；一个完整呼吸周期约 TAU / 0.12 ≈ 52 帧 ≈ 2.6s。
const PHASE_STEP: f32 = 0.12;
/// 宠物窗口宽度：给气泡 + 连点鼓大的形变留足空间（周围透明区已点击穿透，不挡操作）。
const WIN_W: f32 = 260.0;
/// 宠物窗口高度：下半安放宠物，上半留给头顶的说话气泡 + 鼓大余量。
const WIN_H: f32 = 300.0;
/// 一句话默认停留时长（帧）：约 20fps × 4.5s。
const SPEECH_FRAMES: f32 = 90.0;
/// 瞳孔朝鼠标偏移的最大像素。
const EYE_LOOK_MAX: f32 = 4.0;
/// 空闲多少帧后主动搭话（20fps × ~160s）。
const IDLE_CHAT_FRAMES: f32 = 20.0 * 160.0;
/// 光标静止多少帧判为「用户离开」（20fps × ~600s = 10 分钟）。
const AFK_FRAMES: f32 = 20.0 * 600.0;
/// 切换 app 后再评论的冷却帧数（20fps × ~75s）：频繁切 app 时不啰嗦。
const APP_SWITCH_COOLDOWN: f32 = 20.0 * 75.0;

/// 「忙碌值」超过此值判定情绪为 Busy。
const BUSY_THRESHOLD: f32 = 2.2;
/// 忙碌值每帧衰减系数（约 35s 半衰期）。
const ENERGY_DECAY: f32 = 0.999;

/// 点一下宠物轮换的台词。
const LINES: &[&str] = &[
    "在忙什么呀？",
    "喝口水吧 💧",
    "陪着你哦～",
    "要加油鸭！",
    "摸摸我呀",
    "今天也辛苦啦",
    "歇一会儿吧～",
    "我一直都在 ✨",
];

// ===================== 配置（跨窗口全局单例，存 ~/.smelt/pet.json） =====================

/// 桌面宠物配置。
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct PetConfig {
    /// 显示开关：关了就隐藏浮窗。
    pub enabled: bool,
    /// 通知播报开关：是否让宠物主动说出 agent 事件。
    pub notify: bool,
    /// 史莱姆身体颜色（0xRRGGBB）。
    pub color: u32,
    /// 整体缩放（0.8 小 / 1.0 中 / 1.25 大）。
    pub scale: f32,
}

impl Default for PetConfig {
    fn default() -> Self {
        Self { enabled: true, notify: true, color: 0x6be3c9, scale: 1.0 }
    }
}

impl Global for PetConfig {}

fn pet_config_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".smelt").join("pet.json"))
}

/// 读取宠物配置；缺失 / 损坏回退默认。
pub fn load_pet_config() -> PetConfig {
    pet_config_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// 写回宠物配置（失败静默忽略）。
pub fn save_pet_config(c: &PetConfig) {
    let Some(path) = pet_config_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string_pretty(c) {
        let _ = std::fs::write(path, json);
    }
}

// ===================== 播报邮箱（主窗口投递 → 宠物窗口取用） =====================

/// 跨窗口的宠物播报邮箱：主程序在通知触发点 push，宠物在 tick 里 poll。
#[derive(Clone, Default)]
pub struct PetMailbox(Arc<Mutex<VecDeque<SharedString>>>);

impl Global for PetMailbox {}

/// 给宠物投一句要说的话（供主程序在 agent 通知触发点调用）。
pub fn push_pet_message(cx: &App, text: impl Into<SharedString>) {
    if let Some(mb) = cx.try_global::<PetMailbox>() {
        if let Ok(mut q) = mb.0.lock() {
            // 防止积压：只保留最近若干条。
            while q.len() >= 8 {
                q.pop_front();
            }
            q.push_back(text.into());
        }
    }
}

// ===================== 视图 =====================

/// 宠物的「情绪」：由光标静止时长 + 最近切 app 频率推出，驱动步速/停留时长/说话语气。
#[derive(Clone, Copy, PartialEq)]
enum Mood {
    /// 光标长时间不动：悠闲放空，走得慢、歇得久。
    Chill,
    /// 默认状态：专注陪伴。
    Focused,
    /// 最近频繁切 app：走得快、歇得短，说话也带点紧张感。
    Busy,
}

pub struct PetView {
    /// 单调递增的动画相位（弧度），驱动呼吸 / 浮动的 sin；每帧到 TAU 归零防精度漂移。
    phase: f32,
    /// 拖拽标志：mouse_down 置真，首次 mouse_move 触发原生窗口拖动后复位。
    dragging: bool,
    /// 点击互动余韵：按下时置 1.0，逐帧衰减到 0，期间宠物向上蹦并被拉长。
    bounce: f32,
    /// 连点鼓大值：每次戳 +0.2（封顶 1.0）并缓慢衰减，驱动临时放大；停手会慢慢缩回。
    poke: f32,
    /// 眨眼计时：逐帧递增，按周期取模决定何时闭眼。
    blink: f32,
    /// 当前说的话；None 表示不显示气泡。
    speech: Option<SharedString>,
    /// 气泡剩余帧数，逐帧递减到 0 时自动收起气泡。
    speech_ttl: f32,
    /// 台词轮换游标，点一下 +1。
    line_idx: usize,
    /// 是否已剥离原生窗口外框（只需在首帧做一次）。
    chrome_stripped: bool,
    /// 上次应用过的「显示开关」，用于检测变化后 order 窗口。
    visible_applied: Option<bool>,
    /// 上次设置的「点击穿透」状态，避免每帧重复调 setIgnoresMouseEvents。
    click_through: Option<bool>,
    /// 瞳孔当前平滑偏移（每帧缓动逼近鼠标目标方向，避免瞬移的生硬感）。
    eye_x: f32,
    eye_y: f32,
    /// LLM 请求自增序号：只认最新一次的回复，丢弃过期结果（用户连点防竞态）。
    req_gen: u64,
    /// 空闲帧计数：说话 / 交互清零，累到阈值且开了大脑就主动搭话。
    idle_frames: f32,
    /// 上次报时的小时（整点报时用）。
    last_hour: Option<u32>,
    /// 上一帧全屏光标位置（检测久未移动 = 用户离开）。
    last_mouse: Option<(f32, f32)>,
    /// 光标静止累计帧数。
    still_frames: f32,
    /// 是否已就本次「离开」提醒过（光标一动就复位）。
    afk_notified: bool,
    /// 上次感知到的前台 app 名（用于检测「切换了 app」）。
    last_app: Option<String>,
    /// 切 app 评论的冷却计时（帧），>0 时不评论。
    app_switch_cd: f32,
    /// 「忙碌值」：每次切 app 累加，逐帧衰减，用于推出情绪状态。
    switch_energy: f32,
}

impl PetView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        // 动画时钟：照抄 TerminalView 的 smol::Timer 循环，逐帧推进状态并 notify 重绘。
        // this.update 返回 Err 表示视图已销毁，退出循环。
        cx.spawn(async move |this, cx| loop {
            Timer::after(FRAME).await;
            let alive = this
                .update(cx, |this, cx| {
                    // 忙碌值逐帧衰减；情绪越「忙碌」呼吸越快，越「悠闲」呼吸越慢——
                    // 不开大脑时也有这层无声的情绪表达。
                    this.switch_energy *= ENERGY_DECAY;
                    let phase_mult = match this.mood() {
                        Mood::Chill => 0.7,
                        Mood::Focused => 1.0,
                        Mood::Busy => 1.5,
                    };
                    this.phase += PHASE_STEP * phase_mult;
                    if this.phase > TAU {
                        this.phase -= TAU;
                    }
                    this.blink += 1.0;
                    if this.bounce > 0.0 {
                        this.bounce = (this.bounce - 0.06).max(0.0);
                    }
                    // 连点鼓大缓慢回落（停手就慢慢缩回原大小）。
                    if this.poke > 0.0 {
                        this.poke = (this.poke - 0.015).max(0.0);
                    }
                    // 气泡倒计时：到点收起。
                    if this.speech_ttl > 0.0 {
                        this.speech_ttl -= 1.0;
                        if this.speech_ttl <= 0.0 {
                            this.speech = None;
                        }
                    }
                    // 播报 / 主动搭话。
                    let agent_on = Self::agent_on(cx);
                    let notify = cx.try_global::<PetConfig>().map(|c| c.notify).unwrap_or(true);
                    if this.speech.is_none() && notify {
                        let next = cx
                            .try_global::<PetMailbox>()
                            .and_then(|mb| mb.0.lock().ok().and_then(|mut q| q.pop_front()));
                        if let Some(text) = next {
                            if agent_on {
                                // 开了大脑 → 用宠物口吻转述这条通知。
                                this.ask_agent(
                                    format!("主人收到一条开发通知：「{text}」。用你的口吻简短提醒他。"),
                                    cx,
                                );
                            } else {
                                this.say(text);
                            }
                        }
                    }
                    // 空闲主动搭话（需开播报 + 大脑；AI 生成）。
                    this.idle_frames += 1.0;
                    if notify
                        && agent_on
                        && this.speech.is_none()
                        && this.idle_frames > IDLE_CHAT_FRAMES
                    {
                        this.idle_frames = 0.0;
                        let ctx = this.app_ctx();
                        this.ask_agent(format!("你有点无聊了{ctx}，主动跟主人搭一句家常。"), cx);
                    }

                    // 感知前台 app：切到别的 app 时偶尔评论一句（带冷却，不啰嗦）。
                    if this.app_switch_cd > 0.0 {
                        this.app_switch_cd -= 1.0;
                    }
                    let front = frontmost_app();
                    if front != this.last_app {
                        let had_prev = this.last_app.is_some();
                        this.last_app = front.clone();
                        if had_prev {
                            // 切 app 记一笔「忙碌值」，不受评论冷却限制——即便不吭声，情绪也在累积。
                            this.switch_energy = (this.switch_energy + 1.0).min(6.0);
                        }
                        // 首次观测只记基线不评论；切到 smelt 自己也不评论。
                        if had_prev && this.app_switch_cd <= 0.0 {
                            if let Some(app) = front.filter(|a| a != "smelt" && a != "Smelt") {
                                this.app_switch_cd = APP_SWITCH_COOLDOWN;
                                this.proactive_say(
                                    format!(
                                        "主人刚切到「{app}」这个应用，用宠物口吻俏皮地评论一句，别超过 15 字。"
                                    ),
                                    format!("在用 {app} 呀～"),
                                    cx,
                                );
                            }
                        }
                    }

                    // 整点报时（9–22 点；跨过整点时报一次）。
                    let hour = chrono::Local::now().hour();
                    match this.last_hour {
                        Some(h) if h != hour => {
                            this.last_hour = Some(hour);
                            if (9..=22).contains(&hour) {
                                this.proactive_say(
                                    format!("现在{hour}点整了，愉快地跟主人报个时、提醒他注意节奏。"),
                                    format!("🕐 {hour} 点整啦"),
                                    cx,
                                );
                            }
                        }
                        None => this.last_hour = Some(hour),
                        _ => {}
                    }
                    cx.notify();
                })
                .is_ok();
            if !alive {
                break;
            }
        })
        .detach();

        Self {
            phase: 0.0,
            dragging: false,
            bounce: 0.0,
            poke: 0.0,
            blink: 0.0,
            // 启动先打个招呼。
            speech: Some(SharedString::from("嗨～我在这儿陪你！")),
            speech_ttl: SPEECH_FRAMES,
            line_idx: 0,
            chrome_stripped: false,
            visible_applied: None,
            click_through: None,
            eye_x: 0.0,
            eye_y: 0.0,
            req_gen: 0,
            idle_frames: 0.0,
            last_hour: None,
            last_mouse: None,
            still_frames: 0.0,
            afk_notified: false,
            last_app: None,
            app_switch_cd: 0.0,
            switch_energy: 0.0,
        }
    }

    /// 当前前台 app + 情绪的上下文串（喂给大脑，让搭话更贴合当下）。
    fn app_ctx(&self) -> String {
        let app_part = self
            .last_app
            .as_deref()
            .map(|a| format!("在用 {a}，"))
            .unwrap_or_default();
        format!("（主人现在{app_part}此刻状态偏「{}」）", Self::mood_label(self.mood()))
    }

    /// 推出当前情绪：光标静止久 → 悠闲放空；最近频繁切 app → 忙碌紧张；否则专注陪伴。
    fn mood(&self) -> Mood {
        if self.still_frames > AFK_FRAMES / 3.0 {
            Mood::Chill
        } else if self.switch_energy > BUSY_THRESHOLD {
            Mood::Busy
        } else {
            Mood::Focused
        }
    }

    /// 情绪的中文描述，喂给 LLM 让语气贴合当下状态。
    fn mood_label(mood: Mood) -> &'static str {
        match mood {
            Mood::Chill => "悠闲放空",
            Mood::Focused => "专注陪伴",
            Mood::Busy => "有点忙碌紧张",
        }
    }

    /// 主动开口（受「播报开关」总控，且当前没在说话时才说）：
    /// 开了大脑走 AI（`ai_prompt`），否则用固定话术 `canned`。
    fn proactive_say(
        &mut self,
        ai_prompt: String,
        canned: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        let notify = cx.try_global::<PetConfig>().map(|c| c.notify).unwrap_or(true);
        if !notify || self.speech.is_some() {
            return;
        }
        if Self::agent_on(cx) {
            self.ask_agent(ai_prompt, cx);
        } else {
            self.say(canned);
        }
    }

    /// 让宠物说一句话，气泡停留 `SPEECH_FRAMES` 帧；顺便清空闲计时。
    fn say(&mut self, text: impl Into<SharedString>) {
        self.speech = Some(text.into());
        self.speech_ttl = SPEECH_FRAMES;
        self.idle_frames = 0.0;
    }

    /// 是否开启了 LLM 大脑。
    fn agent_on(cx: &App) -> bool {
        cx.try_global::<crate::agent::LlmConfig>()
            .map(|c| c.enabled)
            .unwrap_or(false)
    }

    /// 让宠物「大脑」回一句：先显示「…」思考，异步拿到 LLM 回复后替换；过期结果丢弃。
    fn ask_agent(&mut self, user: String, cx: &mut Context<Self>) {
        let Some(cfg) = cx.try_global::<crate::agent::LlmConfig>().cloned() else {
            return;
        };
        if !cfg.enabled {
            return;
        }
        self.req_gen += 1;
        let gen = self.req_gen;
        self.say("…"); // 思考中
        cx.spawn(async move |this, cx| {
            // 网络请求放后台执行器，别卡 UI。注意：reqwest 依赖 tokio 运行时，而 GPUI 的
            // executor 不是 tokio，直接跑会 panic「no reactor running」。故在后台线程里起一个
            // 临时 current-thread 运行时 block_on 跑请求。
            let reply = cx
                .background_executor()
                .spawn(async move {
                    match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                        Ok(rt) => rt.block_on(crate::agent::complete(cfg, user)),
                        Err(e) => Err(anyhow::Error::from(e)),
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.req_gen != gen {
                    return; // 已有更新的请求，丢弃这次
                }
                match reply {
                    Ok(t) => this.say(t),
                    // 失败：收起「…」，不打扰。
                    Err(_) => {
                        if this.speech.as_deref() == Some("…") {
                            this.speech = None;
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
}

impl Render for PetView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let cfg = cx.try_global::<PetConfig>().cloned().unwrap_or_default();

        // 首帧剥离原生窗口外框（无边框 / 无阴影 / 透明），顺手把原生窗口指针存成
        // 全局单例——设置面板切换显示开关时要用它直接 order 显隐（见 sync_visibility 的注释）。
        if !self.chrome_stripped {
            strip_native_chrome(window);
            self.chrome_stripped = true;
            #[cfg(target_os = "macos")]
            cx.set_global(PetWindowHandle(ns_window_of(window)));
        }
        // 显示开关变化时，order 原生窗口显隐（关掉时窗口彻底不挡点击）。
        if self.visible_applied != Some(cfg.enabled) {
            set_window_visible(window, cfg.enabled);
            self.visible_applied = Some(cfg.enabled);
        }
        if !cfg.enabled {
            return div().size_full().into_any_element();
        }

        // 缩放：配置缩放 × 连点鼓大系数，乘到所有像素尺寸上。
        let s = cfg.scale * (1.0 + self.poke);
        let p = |v: f32| px(v * s);

        // 一次原生取值：眼睛朝鼠标的目标偏移 + 是否落在身体内（点击穿透）+ 全屏光标坐标。
        let (eye_tx, eye_ty, over_pet, mx, my) = mouse_state(window, s);
        // 缓动：当前瞳孔偏移逐帧逼近目标，避免瞬移的生硬感（≈0.2/帧，约 0.5s 到位）。
        self.eye_x += (eye_tx - self.eye_x) * 0.2;
        self.eye_y += (eye_ty - self.eye_y) * 0.2;
        let (eye_ox, eye_oy) = (self.eye_x, self.eye_y);
        // 身体外 → 窗口点击穿透（四周透明区放行点击）；身体上 → 接管交互。
        if self.click_through != Some(!over_pet) {
            set_click_through(window, !over_pet);
            self.click_through = Some(!over_pet);
        }

        // 用户离开检测：光标长时间不动 → 关心一句；一动就复位。
        let moved = self
            .last_mouse
            .map(|(lx, ly)| (mx - lx).abs() > 2.0 || (my - ly).abs() > 2.0)
            .unwrap_or(true);
        if moved {
            self.still_frames = 0.0;
            self.afk_notified = false;
        } else {
            self.still_frames += 1.0;
        }
        self.last_mouse = Some((mx, my));
        if self.still_frames > AFK_FRAMES && !self.afk_notified {
            self.afk_notified = true;
            self.proactive_say(
                "主人好像离开挺久了，温柔地提醒他回来时注意休息眼睛。".into(),
                "🍵 歇会儿，记得喝水呀～",
                cx,
            );
        }

        // 呼吸相位 [-1, 1]：正为「吸气」（拉高变窄），负为「呼气」（压扁变宽）。
        let breathe = self.phase.sin();
        // 垂直浮动：随呼吸上下漂 4px，叠加点击蹦跳的上冲（向上为负 margin）。
        let bob = (breathe * 4.0 - self.bounce * 22.0) * s;
        // 史莱姆身体：呼吸时宽高反向形变（squash & stretch），蹦跳时整体拉长。
        let body_w = p(88.0 + breathe * -4.0 + self.bounce * -6.0);
        let body_h = p(78.0 + breathe * 4.0 + self.bounce * 14.0);
        // 每约 68 帧（~3.4s）闭眼 4 帧。
        let eye_closed = (self.blink % 68.0) < 4.0;

        // 配色：身体色可配置，五官固定深色。
        let body_color = rgb(cfg.color);
        let ink = rgb(0x14322c);

        // 单只眼睛：睁眼是竖椭圆带高光，闭眼是一道横线。
        let eye = move |closed: bool| {
            if closed {
                div()
                    .w(px(11.0 * s))
                    .h(px(3.0 * s))
                    .rounded_full()
                    .bg(ink)
                    .into_any_element()
            } else {
                div()
                    .w(px(10.0 * s))
                    .h(px(14.0 * s))
                    .rounded_full()
                    .bg(ink)
                    .child(
                        // 眼球高光，让它更有神。
                        div()
                            .absolute()
                            .top(px(2.0 * s))
                            .left(px(2.0 * s))
                            .w(px(3.5 * s))
                            .h(px(3.5 * s))
                            .rounded_full()
                            .bg(rgb(0xffffff)),
                    )
                    .into_any_element()
            }
        };

        // 腮红。
        let blush = move || {
            div()
                .w(px(8.0 * s))
                .h(px(5.0 * s))
                .rounded_full()
                .bg(rgba(0xff8fa380))
        };

        // 身体：整体偏圆、底部略平的史莱姆轮廓，投影让它「浮」起来。
        let body = div()
            .relative()
            .w(body_w)
            .h(body_h)
            .rounded_t(p(56.0))
            .rounded_b(p(30.0))
            .bg(body_color)
            .shadow_lg()
            // 五官容器：绝对定位在身体上部居中。
            .child(
                div()
                    .absolute()
                    .top(p(24.0))
                    .left_0()
                    .right_0()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(p(4.0))
                    .child(
                        // 眼睛整体朝鼠标方向平移几像素 → 「看向」鼠标。
                        div()
                            .relative()
                            .left(px(eye_ox * s))
                            .top(px(eye_oy * s))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(p(16.0))
                            .child(eye(eye_closed))
                            .child(eye(eye_closed)),
                    )
                    .child(
                        // 小嘴：闭眼（眨眼）时张成 o 卖个萌。
                        div()
                            .w(px(if eye_closed { 8.0 } else { 10.0 } * s))
                            .h(px(if eye_closed { 7.0 } else { 4.0 } * s))
                            .rounded_full()
                            .bg(ink),
                    ),
            )
            // 两坨腮红，夹在嘴两侧。
            .child(
                div()
                    .absolute()
                    .top(p(38.0))
                    .left_0()
                    .right_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_center()
                    .gap(p(30.0))
                    .child(blush())
                    .child(blush()),
            )
            // —— 交互：按住拖动 / 轻点蹦跳说话 ——
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _window, cx| {
                    this.dragging = true;
                    cx.stop_propagation();
                }),
            )
            .on_mouse_move(cx.listener(|this, _, window, _cx| {
                // 按下后一旦移动即视为拖拽：交给原生窗口拖动，整个浮窗跟着鼠标走。
                if this.dragging {
                    this.dragging = false;
                    window.start_window_move();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _window, cx| {
                    // 未发生移动就抬起 = 一次轻点。
                    if this.dragging {
                        this.dragging = false;
                        // 连点鼓大：每戳一下鼓一点、封顶不鼓破，停手会慢慢缩回。
                        this.poke = (this.poke + 0.2).min(1.0);
                        this.bounce = 1.0;
                        if Self::agent_on(cx) {
                            // 开了大脑 → LLM 即兴回应（带上当前 app 上下文），替代写死台词。
                            let ctx = this.app_ctx();
                            this.ask_agent(format!("主人戳了戳你{ctx}，俏皮地回应一句。"), cx);
                        } else {
                            this.line_idx = (this.line_idx + 1) % LINES.len();
                            this.say(LINES[this.line_idx]);
                        }
                        cx.notify();
                    }
                }),
            );

        // 说话气泡：白底圆角卡片 + 一个朝下的小尾巴，浮在宠物头顶。
        let bubble = self.speech.clone().map(|text| {
            div()
                .flex()
                .flex_col()
                .items_center()
                .child(
                    div()
                        .max_w(px(180.0))
                        .px(px(11.0))
                        .py(px(6.0))
                        .rounded(px(13.0))
                        .bg(rgb(0xffffff))
                        .text_color(rgb(0x1f2937))
                        .text_size(px(12.5))
                        .shadow_lg()
                        .child(text),
                )
                // 尾巴：一颗小白点，把气泡和脑袋连起来。
                .child(
                    div()
                        .mt(px(-1.0))
                        .w(px(8.0))
                        .h(px(8.0))
                        .rounded_full()
                        .bg(rgb(0xffffff)),
                )
        });

        // 身体也朝鼠标方向倾（幅度比眼睛大 ≈1.8×）：div 不能旋转，用位移近似「歪头看你」。
        // 与眼睛偏移同向叠加 → 整只都在看鼠标，而非只有瞳孔动。
        let lean_x = eye_ox * 1.8 * s;
        let lean_y = eye_oy * 1.0 * s;

        // 整窗透明：不设背景色，只有宠物本体可见；上方气泡、下方宠物。
        let mut root = div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_end()
            .pb(px(12.0))
            .gap(px(3.0));
        if let Some(bubble) = bubble {
            root = root.child(bubble);
        }
        root.child(
            div()
                .relative()
                .left(px(lean_x))
                .top(px(bob + lean_y))
                .child(body),
        )
        .into_any_element()
    }
}

// ===================== 原生窗口操作（macOS） =====================

/// 从 `Window` 拿到底层 `NSWindow` 指针（失败返回 null）。
#[cfg(target_os = "macos")]
fn ns_window_of(window: &Window) -> *mut objc::runtime::Object {
    use objc::runtime::Object;
    use objc::{msg_send, sel, sel_impl};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let Ok(handle) = HasWindowHandle::window_handle(&*window) else {
        return std::ptr::null_mut();
    };
    let RawWindowHandle::AppKit(h) = handle.as_raw() else {
        return std::ptr::null_mut();
    };
    let ns_view = h.ns_view.as_ptr() as *mut Object;
    unsafe { msg_send![ns_view, window] }
}

/// 去掉 GPUI 在 macOS 上强加的原生窗口外框：改成无边框、无阴影、`clearColor` 背景。
///
/// GPUI 的 mac 后端对 `titlebar: None` 仍会建成 Titled 窗口（必带圆角边框 + 阴影 + 一层
/// 窗口材质），且没有任何 WindowOptions 能关掉。于是直接落到 AppKit 层改。首帧调一次。
#[cfg(target_os = "macos")]
fn strip_native_chrome(window: &Window) {
    use objc::runtime::{Object, NO};
    use objc::{class, msg_send, sel, sel_impl};

    let ns_window = ns_window_of(window);
    if ns_window.is_null() {
        return;
    }
    unsafe {
        // 仅保留「非激活面板」位（1<<7），去掉 Titled → 无边框、无系统圆角。
        let borderless_nonactivating: usize = 1 << 7;
        let _: () = msg_send![ns_window, setStyleMask: borderless_nonactivating];
        let _: () = msg_send![ns_window, setHasShadow: NO];
        let _: () = msg_send![ns_window, setOpaque: NO];
        let clear: *mut Object = msg_send![class!(NSColor), clearColor];
        let _: () = msg_send![ns_window, setBackgroundColor: clear];
    }
}

#[cfg(not(target_os = "macos"))]
fn strip_native_chrome(_window: &Window) {}

/// 对一个原生 NSWindow 指针发 order 消息（提出 / 收起），供 `set_window_visible`
/// 和 `sync_pet_window_visibility` 共用。
#[cfg(target_os = "macos")]
fn order_ns_window(ns_window: *mut objc::runtime::Object, visible: bool) {
    use objc::runtime::Object;
    use objc::{msg_send, sel, sel_impl};

    if ns_window.is_null() {
        return;
    }
    let nil: *mut Object = std::ptr::null_mut();
    unsafe {
        if visible {
            let _: () = msg_send![ns_window, orderFront: nil];
        } else {
            let _: () = msg_send![ns_window, orderOut: nil];
        }
    }
}

/// 显隐原生窗口（显示开关关掉时 orderOut，彻底不挡点击）。
#[cfg(target_os = "macos")]
fn set_window_visible(window: &Window, visible: bool) {
    order_ns_window(ns_window_of(window), visible);
}

/// 宠物原生窗口指针（macOS）。首帧渲染时缓存（见 `Render for PetView`），供
/// `sync_pet_window_visibility` 直接 order 显隐，绕开会被 GPUI 按窗口 occlusion
/// 状态整个停掉的渲染循环。
#[cfg(target_os = "macos")]
#[derive(Clone, Copy)]
struct PetWindowHandle(*mut objc::runtime::Object);

#[cfg(target_os = "macos")]
impl Global for PetWindowHandle {}

/// 设置面板切换宠物显示开关时，从 Workspace 侧直接调用，立即 order 显隐宠物窗口。
///
/// 不能指望宠物自己的 `render()` 去响应配置变化：GPUI 在 mac 上一旦窗口 occlusion
/// 状态变 hidden 就会把驱动渲染的 DisplayLink 整个停掉（`windowDidChangeOcclusionState:`），
/// `orderOut` 之后宠物窗口再也等不到下一次 render() 把自己重新 order 出来——先有鸡还是
/// 先有蛋。这里绕开渲染循环，直接对缓存的原生窗口指针发 order 消息，从而触发 occlusion
/// 状态变化、重新点亮 DisplayLink，宠物自己的动画循环下一帧就能接着正常跑，
/// 内部的 `visible_applied` 也会在那一帧自愈同步。
pub fn sync_pet_window_visibility(cx: &App, visible: bool) {
    #[cfg(target_os = "macos")]
    if let Some(handle) = cx.try_global::<PetWindowHandle>() {
        order_ns_window(handle.0, visible);
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (cx, visible);
}

#[cfg(not(target_os = "macos"))]
fn set_window_visible(_window: &Window, _visible: bool) {}

// AppKit 几何结构体（屏幕坐标，原点在左下、y 向上）。
#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy)]
struct NSPoint {
    x: f64,
    y: f64,
}
#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy)]
struct NSSize {
    width: f64,
    height: f64,
}
#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy)]
struct NSRect {
    origin: NSPoint,
    size: NSSize,
}

/// 一次性从全屏光标算出两件事：瞳孔朝鼠标的偏移 + 光标是否落在宠物身体范围内。
///
/// 宠物窗口很大且大半透明，`window.mouse_position()` 只在光标进窗时更新，无法追；且透明区
/// 会拦截点击。于是用 `[NSEvent mouseLocation]` 取全屏光标（屏幕坐标）+ `NSWindow.frame`：
/// - 眼睛偏移：屏幕坐标 y 向上、视图坐标 y 向下，故竖直分量取反。
/// - 命中测试：宠物底部锚定，身体中心在窗口底边上方 `12 + 39*s`，用椭圆判定是否在身体内。
///
/// 返回 `(瞳孔x偏移, 瞳孔y偏移, 是否在身体内)`。
#[cfg(target_os = "macos")]
fn mouse_state(window: &Window, s: f32) -> (f32, f32, bool, f32, f32) {
    use objc::{class, msg_send, sel, sel_impl};

    let ns_window = ns_window_of(window);
    if ns_window.is_null() {
        return (0.0, 0.0, true, 0.0, 0.0);
    }
    let s = s as f64;
    unsafe {
        let frame: NSRect = msg_send![ns_window, frame];
        let mouse: NSPoint = msg_send![class!(NSEvent), mouseLocation];
        let cx = frame.origin.x + frame.size.width / 2.0;

        // 眼睛看向鼠标：眼睛中心在身体中上部（底边上方 54*s）。
        let eye_y = frame.origin.y + 12.0 + 54.0 * s;
        let vx = mouse.x - cx;
        let vy = mouse.y - eye_y;
        let len = (vx * vx + vy * vy).sqrt();
        let (ox, oy) = if len < 1.0 {
            (0.0, 0.0)
        } else {
            ((vx / len) as f32 * EYE_LOOK_MAX, -(vy / len) as f32 * EYE_LOOK_MAX)
        };

        // 命中测试：身体椭圆（含少量外扩，便于抓取）。
        let body_cy = frame.origin.y + 12.0 + 39.0 * s;
        let rx = 44.0 * s + 10.0;
        let ry = 42.0 * s + 12.0;
        let hx = (mouse.x - cx) / rx;
        let hy = (mouse.y - body_cy) / ry;
        let inside = hx * hx + hy * hy <= 1.0;

        (ox, oy, inside, mouse.x as f32, mouse.y as f32)
    }
}

#[cfg(not(target_os = "macos"))]
fn mouse_state(_window: &Window, _s: f32) -> (f32, f32, bool, f32, f32) {
    (0.0, 0.0, true, 0.0, 0.0)
}

/// 当前前台 app 的名字（如 "Google Chrome" / "Xcode"）。
/// 用 `[[NSWorkspace sharedWorkspace] frontmostApplication].localizedName`——只拿 app 名
/// 无需任何权限（拿窗口标题 / 网址才要辅助功能权限，这里不碰）。
#[cfg(target_os = "macos")]
fn frontmost_app() -> Option<String> {
    use objc::runtime::Object;
    use objc::{class, msg_send, sel, sel_impl};

    unsafe {
        let ws: *mut Object = msg_send![class!(NSWorkspace), sharedWorkspace];
        if ws.is_null() {
            return None;
        }
        let app: *mut Object = msg_send![ws, frontmostApplication];
        if app.is_null() {
            return None;
        }
        let name: *mut Object = msg_send![app, localizedName];
        if name.is_null() {
            return None;
        }
        let utf8: *const std::os::raw::c_char = msg_send![name, UTF8String];
        if utf8.is_null() {
            return None;
        }
        let s = std::ffi::CStr::from_ptr(utf8).to_string_lossy().into_owned();
        (!s.is_empty()).then_some(s)
    }
}

#[cfg(not(target_os = "macos"))]
fn frontmost_app() -> Option<String> {
    None
}

/// 设置窗口是否「点击穿透」：`passthrough=true` 时整窗放行鼠标事件到下层。
#[cfg(target_os = "macos")]
fn set_click_through(window: &Window, passthrough: bool) {
    use objc::runtime::{NO, YES};
    use objc::{msg_send, sel, sel_impl};

    let ns_window = ns_window_of(window);
    if ns_window.is_null() {
        return;
    }
    let flag = if passthrough { YES } else { NO };
    unsafe {
        let _: () = msg_send![ns_window, setIgnoresMouseEvents: flag];
    }
}

#[cfg(not(target_os = "macos"))]
fn set_click_through(_window: &Window, _passthrough: bool) {}

// ===================== 开窗 =====================

/// 打开桌面宠物浮窗：透明、无标题栏、置顶（PopUp/NSPanel）、不抢焦点，
/// 初始停在主屏右下角（Dock 上方）。
pub fn open_pet_window(cx: &mut App) {
    // 主屏尺寸 → 右下角初始位置；取不到就退回一个常见分辨率。
    let screen = cx
        .primary_display()
        .map(|d| d.bounds())
        .unwrap_or_else(|| Bounds::new(point(px(0.0), px(0.0)), size(px(1440.0), px(900.0))));
    let margin = px(28.0);
    let x = screen.origin.x + screen.size.width - px(WIN_W) - margin;
    // 底部再抬高一点，避开 Dock。
    let y = screen.origin.y + screen.size.height - px(WIN_H) - px(90.0);
    let bounds = Bounds::new(point(x, y), size(px(WIN_W), px(WIN_H)));

    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: None,
        // 不抢焦点：宠物出现时不打断用户当前操作。
        focus: false,
        show: true,
        // PopUp → macOS 上是非激活 NSPanel，浮在所有窗口之上、跟随所有 Space。
        kind: WindowKind::PopUp,
        // 自定义拖拽（start_window_move），据官方建议关掉系统窗口移动。
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        // 透明背景：露出桌面，只显示宠物本体。
        window_background: WindowBackgroundAppearance::Transparent,
        ..Default::default()
    };

    // 注意：不包 gpui-component 的 Root —— Root 会给整窗填主题背景色（深色）并加圆角
    // 边框。宠物只用 GPUI 原生图元，直接渲染即可保持透明。
    cx.open_window(options, |_window, cx| cx.new(|cx| PetView::new(cx)))
        .expect("打开桌面宠物窗口失败");
}
