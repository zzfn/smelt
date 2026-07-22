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
/// 拖拽跟手用的高帧率时钟间隔（~60fps）：拖动时窗口位置追踪单独用这个更快的
/// 时钟驱动，不跟随上面 20fps 的慢时钟，跟手不卡顿；不拖时立刻判负返回，开销
/// 可忽略，不影响空闲功耗。
const DRAG_TICK: Duration = Duration::from_millis(16);
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
    /// 上次拖拽落点：原生窗口坐标 (x, y)（macOS AppKit 坐标系，左下角原点、y 向上），
    /// 跟 `cursor_and_window_origin`/`set_window_origin` 用的是同一套坐标，直接互通、
    /// 不用换算。None = 从未拖动过，回退默认的屏幕右下角。
    #[serde(default)]
    pub pos: Option<(f32, f32)>,
}

impl Default for PetConfig {
    fn default() -> Self {
        Self { enabled: true, notify: true, color: 0x6be3c9, scale: 1.0, pos: None }
    }
}

impl Global for PetConfig {}

fn pet_config_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".smelt").join("pet.json"))
}

/// 读取宠物配置；缺失 / 损坏回退默认。
pub fn load_pet_config() -> PetConfig {
    crate::json_store::load_json(pet_config_path())
}

/// 写回宠物配置（失败静默忽略）。
pub fn save_pet_config(c: &PetConfig) {
    crate::json_store::save_json(pet_config_path(), c)
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
    /// 点击互动余韵：按下时置 1.0，逐帧衰减到 0，期间宠物向上蹦。
    bounce: f32,
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
    /// 是否已确认进入真实拖动（按下后移动超过阈值才置真；跟"按下但没动=轻点"区分）。
    native_drag: bool,
    /// 手动拖动的锚点：(按下瞬间的全局光标 x/y, 按下瞬间的窗口原点 x/y)。按下时
    /// 记录、松手时清空；mouse_move 里用它算出「这次该把窗口挪到哪」，不靠 AppKit
    /// 的 `performWindowDragWithEvent`（那个会在拖动全程冻结 app 自己的重绘，见
    /// mouse_down/mouse_move 的注释）。
    drag_start: Option<(f32, f32, f32, f32)>,
    /// 当前所在屏幕的视觉大小补偿系数（见 `screen_scale_compensation`），乘到
    /// `PetConfig.scale` 上；首帧 + 每次拖拽落下后重新计算一次。
    display_scale: f32,
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

        // 拖拽跟手时钟：单独用更高帧率（DRAG_TICK）驱动位置追踪 + 滚动相位，
        // 不跟随上面那个 20fps 的慢时钟——见 handle_drag_move 和 DRAG_TICK 的注释。
        // 没在拖时这里立刻判负返回，几乎零开销，不影响空闲功耗。
        cx.spawn(async move |this, cx| loop {
            Timer::after(DRAG_TICK).await;
            let alive = this
                .update_in(cx, |this, window, cx| {
                    if this.drag_start.is_some() {
                        this.handle_drag_move(window, cx);
                    }
                })
                .is_ok();
            if !alive {
                break;
            }
        })
        .detach();

        Self {
            phase: 0.0,
            bounce: 0.0,
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
            native_drag: false,
            drag_start: None,
            display_scale: 1.0,
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

    /// 手动拖拽的移动处理：算位移、判定「是否已经算真实拖动」、挪窗口、累计滚动
    /// 路程。由 DRAG_TICK 高帧率时钟轮询调用（固定节奏，不挂在原始 mouse move
    /// 事件上——见 DRAG_TICK 的注释），没有正在按住（drag_start 为空）就什么都不做。
    fn handle_drag_move(&mut self, window: &Window, cx: &mut Context<Self>) {
        let Some((sx, sy, swx, swy)) = self.drag_start else { return };
        let (mx, my, _, _) = cursor_and_window_origin(window);
        let (dx, dy) = (mx - sx, my - sy);
        if !self.native_drag && (dx.abs() > 2.0 || dy.abs() > 2.0) {
            self.native_drag = true;
            self.proactive_say(
                "主人一把抓起你、正在挪动位置，惊呼一声，别超过 10 字。".into(),
                "哇！要去哪呀～",
                cx,
            );
        }
        if self.native_drag {
            // 自己接住拖拽、手动挪窗口（而非交给会冻结 app 重绘的
            // performWindowDragWithEvent），拖动全程 render() 都能正常跑。
            set_window_origin(window, swx + dx, swy + dy);
        }
        cx.notify();
    }

    /// 手动拖拽的松手处理：区分「拖完放下」和「按下没动=轻点」两种收尾，清掉拖拽
    /// 状态。由窗口级 MouseUpEvent 监听调用，drag_start 为空（这次 mouse up 跟我们
    /// 的拖拽无关）就直接忽略。
    fn handle_drag_up(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.drag_start.is_none() {
            return;
        }
        if self.native_drag {
            self.native_drag = false;
            self.bounce = 1.0;
            // 持久化这次的落点，下次开窗直接恢复到这——不然宠物永远焊在屏幕右下角。
            let (_, _, wx, wy) = cursor_and_window_origin(window);
            let mut c = cx.global::<PetConfig>().clone();
            c.pos = Some((wx, wy));
            save_pet_config(&c);
            cx.set_global(c);
            // 可能被拖到了另一块屏幕：重新算一次视觉大小补偿。
            self.display_scale = screen_scale_compensation(window);
            self.proactive_say(
                "刚被主人放下、安顿在新位置，俏皮地喘口气，别超过 10 字。".into(),
                "稳啦～",
                cx,
            );
        } else {
            self.bounce = 1.0;
            if Self::agent_on(cx) {
                // 开了大脑 → LLM 即兴回应（带上当前 app 上下文），替代写死台词。
                let ctx = self.app_ctx();
                self.ask_agent(format!("主人戳了戳你{ctx}，俏皮地回应一句。"), cx);
            } else {
                self.line_idx = (self.line_idx + 1) % LINES.len();
                self.say(LINES[self.line_idx]);
            }
        }
        self.drag_start = None;
        cx.notify();
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
        let r#gen = self.req_gen;
        self.say("…"); // 思考中
        cx.spawn(async move |this, cx| {
            // 网络请求放后台执行器，别卡 UI；block_on_tokio 负责给 reqwest 一个 reactor，
            // 顺带兜住内部 panic。
            let reply = cx
                .background_executor()
                .spawn(async move {
                    smelt_core::block_on::block_on_tokio(crate::agent::complete(cfg, user))
                        .and_then(|r| r)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.req_gen != r#gen {
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
            // 开窗时默认停在右下角（见 open_pet_window），这里首帧恢复上次拖拽的落点；
            // 但落点可能绑定着已经失效的屏幕拓扑（比如当时拖到过外接显示器、现在拔掉
            // 了），这种情况不恢复，保留默认右下角位置，避免宠物摆到看不见的地方。
            if let Some((x, y)) = cfg.pos {
                if pos_on_any_screen(x, y) {
                    set_window_origin(window, x, y);
                }
            }
            // 首帧按当前所在屏幕算一次视觉大小补偿（见 screen_scale_compensation）。
            self.display_scale = screen_scale_compensation(window);
        }
        // 显示开关变化时，order 原生窗口显隐（关掉时窗口彻底不挡点击）。
        if self.visible_applied != Some(cfg.enabled) {
            set_window_visible(window, cfg.enabled);
            self.visible_applied = Some(cfg.enabled);
        }
        if !cfg.enabled {
            return div().size_full().into_any_element();
        }

        // 缩放：配置缩放 × 跨屏视觉大小补偿，乘到所有像素尺寸上。
        let s = cfg.scale * self.display_scale;
        let p = |v: f32| px(v * s);

        // 一次原生取值：眼睛朝鼠标的目标偏移 + 是否落在身体内（点击穿透判定用）。
        let (eye_tx, eye_ty, over_pet) = mouse_state(window, s);
        // 缓动：当前瞳孔偏移逐帧逼近目标，避免瞬移的生硬感（≈0.2/帧，约 0.5s 到位）。
        self.eye_x += (eye_tx - self.eye_x) * 0.2;
        self.eye_y += (eye_ty - self.eye_y) * 0.2;
        let (eye_ox, eye_oy) = (self.eye_x, self.eye_y);
        // 身体外 → 窗口点击穿透（四周透明区放行点击）；身体上、或鼠标按着正在拖 →
        // 接管交互。拖动时强制不穿透：窗口快速移动时命中测试可能有一帧判到"光标
        // 已经不在新位置的身体范围内"，这时如果切成穿透，会直接丢失后续鼠标事件、
        // 拖拽卡死在半路——所以只要按着就无条件保持可交互。
        let interactive = over_pet || self.drag_start.is_some();
        if self.click_through != Some(!interactive) {
            set_click_through(window, !interactive);
            self.click_through = Some(!interactive);
        }

        // 用户离开检测：光标长时间不动 → 关心一句；一动就复位。
        let (mx, my, _, _) = cursor_and_window_origin(window);
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
        // 史莱姆身体：呼吸时宽高反向形变（squash & stretch）。抓起 / 拖动不再有
        // 额外的形变动画——试过挤压拉伸、变球滚动几版都不理想，索性去掉，拖动时
        // 身体保持原样跟着窗口走就好。
        let body_w = p(88.0 + breathe * -4.0);
        let body_h = p(78.0 + breathe * 4.0);
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
            // 悬停时张开手（可抓起来），实际拖动时合上手，跟系统拖拽的 grab/grabbing
            // 语义一致。
            .cursor(if self.native_drag { CursorStyle::ClosedHand } else { CursorStyle::OpenHand })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    let (mx, my, wx, wy) = cursor_and_window_origin(window);
                    this.drag_start = Some((mx, my, wx, wy));
                    cx.stop_propagation();
                }),
            )
            // 松手改在下面的 canvas 里，用窗口级监听接住（见那里的注释）；移动改由
            // 独立的 DRAG_TICK 高帧率时钟轮询处理（见 handle_drag_move），不再挂在
            // 鼠标移动事件上——原始 mouse move 事件频率不稳（可能远高于/参差于渲染
            // 需要的节奏），直接拿来驱动位置 + 滚动相位会导致「滚着走」在采样点之间
            // 跳变一大截，看起来忽高忽低不连贯；改成固定节奏轮询后就稳定了。
            .child({
                let view = cx.entity();
                canvas(
                    |_, _, _| {},
                    move |_bounds, _, window, _cx| {
                        // 用 window.on_mouse_event 而不是 on_mouse_up 构建器：后者靠
                        // hitbox.is_hovered() 判断要不要触发，而 hitbox id 是每帧重新
                        // 分配的单调计数器，mouse_down 时 capture 的旧 id 下一帧就跟
                        // 当前帧的 hitbox 对不上号——这正是"松手了还在跟着鼠标走"那个
                        // bug 的根因（capture_pointer 只在同一帧内有效）。窗口级监听
                        // 不依赖 hitbox，稳定收得到松手事件；参考 gpui-component 里
                        // resizable 面板拖拽同款写法（ResizeHandle::paint 里的
                        // window.on_mouse_event）。每帧重新注册，跟 GPUI 的预期用法
                        // 一致（"next frame" 会自动清掉）。
                        let view_up = view.clone();
                        window.on_mouse_event(move |_ev: &MouseUpEvent, phase, window, cx| {
                            if phase != DispatchPhase::Bubble {
                                return;
                            }
                            view_up.update(cx, |this, cx| this.handle_drag_up(window, cx));
                        });
                    },
                )
                .absolute()
                .inset_0()
            });

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

/// 去掉 GPUI 在 macOS 上强加的原生窗口外框：改成无边框、无阴影、`clearColor` 背景；
/// 顺带把窗口层级从 `WindowKind::PopUp` 默认的 `NSPopUpWindowLevel`（101，系统弹出
/// 菜单/提示条用的层级）降到 `NSFloatingWindowLevel`（3）。
///
/// GPUI 的 mac 后端对 `titlebar: None` 仍会建成 Titled 窗口（必带圆角边框 + 阴影 + 一层
/// 窗口材质），且没有任何 WindowOptions 能关掉。于是直接落到 AppKit 层改。首帧调一次。
///
/// 层级这一改是修一个「切到 smelt 时闪一下又跳回原来那个 app」的 bug：宠物这扇窗口是
/// `NonactivatingPanel`（本来就设计成永远抢不了焦点），常驻在 `NSPopUpWindowLevel`——
/// 全局层级里数一数二高，跟正常窗口的 0 差了两个数量级。用户切到 smelt 时，AppKit 会把
/// 「这个进程层级最高的窗口」当成待激活对象，选中的偏偏是这扇既最靠前、又永远当不了
/// key window 的面板：进程短暂被激活（这就是那一下「闪」），但没有任何窗口真正拿到
/// key 状态，系统于是又把前台还给了原来的 app。降到 `NSFloatingWindowLevel` 后依然
/// 浮在所有普通窗口（层级 0）之上，只是不再抢那个专供瞬时弹出内容用的极端层级，主窗口
/// 的正常激活流程就不会被它截胡了。
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
        let ns_floating_window_level: isize = 3;
        let _: () = msg_send![ns_window, setLevel: ns_floating_window_level];
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
/// 返回 `(瞳孔x偏移, 瞳孔y偏移, 是否在身体内)`。窗口原点不在这返回了——手动拖拽
/// 改用 `cursor_and_window_origin` 单独读，这个函数只服务眼神 + 点击穿透判定。
#[cfg(target_os = "macos")]
fn mouse_state(window: &Window, s: f32) -> (f32, f32, bool) {
    use objc::{class, msg_send, sel, sel_impl};

    let ns_window = ns_window_of(window);
    if ns_window.is_null() {
        return (0.0, 0.0, true);
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

        (ox, oy, inside)
    }
}

#[cfg(not(target_os = "macos"))]
fn mouse_state(_window: &Window, _s: f32) -> (f32, f32, bool) {
    (0.0, 0.0, true)
}

/// 全局光标坐标 + 窗口当前原点（都是屏幕坐标，y 向上）。手动拖拽用它算「这次要把
/// 窗口挪到哪」，不依赖缩放，故跟 mouse_state 分开。
#[cfg(target_os = "macos")]
fn cursor_and_window_origin(window: &Window) -> (f32, f32, f32, f32) {
    use objc::{class, msg_send, sel, sel_impl};

    let ns_window = ns_window_of(window);
    if ns_window.is_null() {
        return (0.0, 0.0, 0.0, 0.0);
    }
    unsafe {
        let frame: NSRect = msg_send![ns_window, frame];
        let mouse: NSPoint = msg_send![class!(NSEvent), mouseLocation];
        (mouse.x as f32, mouse.y as f32, frame.origin.x as f32, frame.origin.y as f32)
    }
}

#[cfg(not(target_os = "macos"))]
fn cursor_and_window_origin(_window: &Window) -> (f32, f32, f32, f32) {
    (0.0, 0.0, 0.0, 0.0)
}

/// 基准屏幕高度（points）：宠物窗口固定 point 尺寸，在这个高度的屏幕上视觉大小按
/// 「所见即所得」处理，其余屏幕按高度比例反向补偿。
const BASELINE_SCREEN_HEIGHT: f32 = 1080.0;

/// 跨屏视觉大小补偿系数：不同屏幕的「看起来的分辨率」（points，跟 Retina/物理像素
/// 无关）不一样——同样 260pt 宽的宠物窗口，在 point 分辨率更高的屏幕（比如 4K 屏
/// 开原生 1:1、point 数很大）上占的视觉比例更小，看着就像变小了；反之在 point 分
/// 辨率低的屏幕上看着变大。用宠物当前所在屏幕（`NSWindow.screen`）的高度相对基准
/// 高度算一个系数乘到 `PetConfig.scale` 上抵消这个差异，让跨屏拖拽视觉大小基本一致。
/// clamp 到 [0.6, 1.6] 防止极端屏幕配置把宠物缩得看不见或大到离谱。
#[cfg(target_os = "macos")]
fn screen_scale_compensation(window: &Window) -> f32 {
    use objc::runtime::Object;
    use objc::{msg_send, sel, sel_impl};

    let ns_window = ns_window_of(window);
    if ns_window.is_null() {
        return 1.0;
    }
    unsafe {
        let screen: *mut Object = msg_send![ns_window, screen];
        if screen.is_null() {
            return 1.0;
        }
        let frame: NSRect = msg_send![screen, frame];
        let h = frame.size.height as f32;
        if h <= 0.0 {
            return 1.0;
        }
        (BASELINE_SCREEN_HEIGHT / h).clamp(0.6, 1.6)
    }
}

#[cfg(not(target_os = "macos"))]
fn screen_scale_compensation(_window: &Window) -> f32 {
    1.0
}

/// 检查一个「上次拖拽落点」（AppKit 全局坐标）此刻是否还落在当前任意一块屏幕范围内。
///
/// `PetConfig.pos` 存的是全局桌面坐标，跟当时的屏幕拓扑绑定：比如把宠物拖到过外接
/// 显示器上，拔掉外接显示器后虚拟桌面收缩，这个坐标很可能落在现在根本没有物理屏幕
/// 覆盖的区域——原生窗口会被摆过去，但用户什么都看不见。所以恢复前先跟当前
/// `NSScreen.screens` 逐个比对，窗口矩形只要跟其中一块有重叠就当作「看得见」。
#[cfg(target_os = "macos")]
fn pos_on_any_screen(x: f32, y: f32) -> bool {
    use objc::runtime::Object;
    use objc::{class, msg_send, sel, sel_impl};

    unsafe {
        let screens: *mut Object = msg_send![class!(NSScreen), screens];
        let count: usize = msg_send![screens, count];
        for i in 0..count {
            let screen: *mut Object = msg_send![screens, objectAtIndex: i];
            let frame: NSRect = msg_send![screen, frame];
            let left = frame.origin.x as f32;
            let bottom = frame.origin.y as f32;
            let right = left + frame.size.width as f32;
            let top = bottom + frame.size.height as f32;
            if x < right && x + WIN_W > left && y < top && y + WIN_H > bottom {
                return true;
            }
        }
        false
    }
}

#[cfg(not(target_os = "macos"))]
fn pos_on_any_screen(_x: f32, _y: f32) -> bool {
    true
}

/// 手动挪窗口到给定的屏幕原点坐标——用来实现「自己接住拖拽」而不是把整个拖动过程
/// 甩给会冻结重绘的 `performWindowDragWithEvent`（见 mouse_down/mouse_move 的注释）。
#[cfg(target_os = "macos")]
fn set_window_origin(window: &Window, x: f32, y: f32) {
    use objc::{msg_send, sel, sel_impl};

    let ns_window = ns_window_of(window);
    if ns_window.is_null() {
        return;
    }
    let point = NSPoint { x: x as f64, y: y as f64 };
    unsafe {
        let _: () = msg_send![ns_window, setFrameOrigin: point];
    }
}

#[cfg(not(target_os = "macos"))]
fn set_window_origin(_window: &Window, _x: f32, _y: f32) {}

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
