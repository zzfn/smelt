//! 本地任务：侧栏统一查看与开跑，**全部走交互终端**（不 `-p` 无头批跑）。
//!
//! - 总览只做会话监控；任务列表在左侧「任务」分组
//! - 开跑 = 新开侧栏终端 + `launch "首包"`（CLI 启动参数，**不**模拟粘贴/回车）
//! - 已有会话续聊才用 paste + 裸 `\r`（见 `send_text_and_submit`）
//! - 见 docs/local-tasks.md

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use gpui::*;
use gpui::prelude::FluentBuilder;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputState};
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use gpui_component::*;
use serde::{Deserialize, Serialize};

use crate::settings::{active_launch_entries, icon_for_launch_command};
use crate::terminal_view::TerminalView;
use crate::{new_sid, Workspace};

// ===================== 模型 =====================

/// 任务列。UI 只暴露三态：**待办 / 执行中 / 完成**。
/// `ready` / `waiting` 仍可从旧 tasks.json 读入，展示时归并到待办 / 执行中。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TaskColumn {
    #[default]
    Backlog,
    Ready,
    Running,
    Waiting,
    Done,
}

impl TaskColumn {
    /// 展示用三态标签。
    pub fn label(self) -> &'static str {
        match self {
            Self::Backlog | Self::Ready => "待办",
            Self::Running | Self::Waiting => "执行中",
            Self::Done => "完成",
        }
    }

    pub fn color(self) -> u32 {
        match self {
            Self::Running | Self::Waiting => 0x4a9eff,
            Self::Backlog | Self::Ready => 0x8b93a7,
            Self::Done => 0x22c55e,
        }
    }

    /// 侧栏 / 总览排序（越小越靠前）。
    pub fn sidebar_rank(self) -> u8 {
        match self {
            Self::Running | Self::Waiting => 0,
            Self::Backlog | Self::Ready => 1,
            Self::Done => 2,
        }
    }

    /// 是否算「执行中」（含旧 waiting）。
    pub fn is_active(self) -> bool {
        matches!(self, Self::Running | Self::Waiting)
    }

    /// 是否算「待办」（含旧 ready）。
    pub fn is_todo(self) -> bool {
        matches!(self, Self::Backlog | Self::Ready)
    }

    /// 状态下拉可选的三态（写入 store 用规范化值）。
    pub fn ui_choices() -> [TaskColumn; 3] {
        [Self::Backlog, Self::Running, Self::Done]
    }
}

/// 任务类型：普通（手动运行）/ 单次定时（到点自动 `run_task`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    /// 手动「运行」才开跑。
    #[default]
    Once,
    /// 到 `run_at` 后由 Workspace 扫描器自动开跑（单次，不循环）。
    Scheduled,
}

impl TaskKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Once => "普通",
            Self::Scheduled => "定时",
        }
    }
}

/// 本地任务。
///
/// 字段分工（给 UI / agent / 自循环时别混）：
/// - `title`：**给人看**的侧栏名；可空，创建时用首包首行生成
/// - `body`：**给 agent 的首包**（唯一写入 launch 启动参数的内容）
/// - `project_cwd`：在哪个项目目录开终端
/// - `launch`：base 启动命令（不含首包拼接）
/// - `session_id`：执行体（smeltd 会话）
/// - `kind` / `run_at`：普通 vs 单次定时
/// - `auto_run`：是否允许系统自动开跑（完成续跑 / 定时扫描）；手动点「运行」始终可以
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    /// 侧栏展示名（人类可读）。
    pub title: String,
    /// Agent 首包 prompt（开跑时进 CLI 参数；不是标题的复述）。
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub column: TaskColumn,
    /// 绑定的项目目录（绝对路径）。
    pub project_cwd: String,
    /// 已开终端的 smeltd session id。
    #[serde(default)]
    pub session_id: Option<String>,
    /// 快捷启动 base 命令（如 `claude --dangerously-skip-permissions`）。
    #[serde(default)]
    pub launch: Option<String>,
    /// 普通 / 单次定时。缺省 = 普通（兼容旧 tasks.json）。
    #[serde(default)]
    pub kind: TaskKind,
    /// 计划开跑时间（Unix 秒，本地语义写入）。仅 `kind = Scheduled` 有意义。
    #[serde(default)]
    pub run_at: Option<u64>,
    /// 是否可被系统自动执行（完成边沿续跑、定时扫描）。
    /// `false` = 只等人点「运行」。缺省 true（兼容旧数据与排队续跑预期）。
    #[serde(default = "default_true")]
    pub auto_run: bool,
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub updated_at: u64,
}

fn default_true() -> bool {
    true
}

impl Task {
    pub fn new(project_cwd: String, title: String, body: String, launch: Option<String>) -> Self {
        let now = now_secs();
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            title,
            body,
            column: TaskColumn::Backlog,
            project_cwd,
            session_id: None,
            launch,
            kind: TaskKind::Once,
            run_at: None,
            auto_run: true,
            created_at: now,
            updated_at: now,
        }
    }

    /// 定时任务是否已到点（`run_at <= now`）且允许自动执行。
    pub fn is_due(&self, now: u64) -> bool {
        self.auto_run
            && self.kind == TaskKind::Scheduled
            && self.column.is_todo()
            && self.run_at.map(|at| at <= now).unwrap_or(false)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TaskFile {
    #[serde(default)]
    pub tasks: Vec<Task>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 解析本地时间字符串 → Unix 秒。支持 `YYYY-MM-DD HH:MM` / `YYYY-MM-DD HH:MM:SS`。
pub fn parse_local_datetime(s: &str) -> Option<u64> {
    use chrono::{Local, NaiveDateTime, TimeZone};
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let naive = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M")
        .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S"))
        .ok()?;
    let local = Local.from_local_datetime(&naive).single()?;
    Some(local.timestamp().max(0) as u64)
}

/// 展示用短时间（本地）：`7/15 18:30`。
pub fn format_run_at_short(secs: u64) -> String {
    use chrono::{Local, TimeZone};
    Local
        .timestamp_opt(secs as i64, 0)
        .single()
        .map(|dt| dt.format("%m/%d %H:%M").to_string())
        .unwrap_or_else(|| secs.to_string())
}

/// 输入框默认值：约一小时后（本地 `YYYY-MM-DD HH:MM`）。
pub fn default_run_at_input() -> String {
    use chrono::{Duration, Local};
    (Local::now() + Duration::hours(1))
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

fn tasks_global_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".smelt").join("tasks.json"))
}

/// 开跑时交给 agent 的首包：**只用 body**；旧数据 body 空时才回退 title。
/// 不再把 title 拼进 prompt（标题是侧栏标签，不是指令）。
pub fn task_prompt(task: &Task) -> String {
    let body = task.body.trim();
    if !body.is_empty() {
        body.to_string()
    } else {
        task.title.trim().to_string()
    }
}

/// 从首包生成侧栏标题：首行非空，最长 40 字。
pub fn title_from_prompt(prompt: &str) -> String {
    let first = prompt
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("未命名任务");
    if first.chars().count() > 40 {
        format!("{}…", first.chars().take(40).collect::<String>())
    } else {
        first.to_string()
    }
}

/// shell 单引号包裹（路径 / 内联短 prompt）。
pub fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn tasks_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".smelt").join("tasks"))
}

/// 把首包 prompt 落到磁盘，供 `$(cat …)` 塞进 launch（多行/引号安全）。
fn write_prompt_file(task_id: &str, prompt: &str) -> Option<PathBuf> {
    let dir = tasks_dir()?.join("prompts");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("{task_id}.txt"));
    std::fs::write(&path, prompt).ok()?;
    Some(path)
}

/// 交互启动：在 base launch 后追加 `"$(cat prompt)"` 作为 **CLI 首包参数**。
///
/// 对齐 vibeyard `pendingPromptTrigger: 'startup-arg'`（`claude "…"`），
/// **不是** `claude -p` 无头批跑。agent 起来即带第一条用户消息，无需 PTY 回车。
pub fn build_launch_with_prompt(base_launch: &str, prompt_path: &Path) -> String {
    let cat = format!(
        "\"$(cat {})\"",
        shell_single_quote(&prompt_path.display().to_string())
    );
    let base = base_launch.trim();
    if base.is_empty() {
        format!("claude {cat}")
    } else {
        format!("{base} {cat}")
    }
}

fn project_label(cwd: &str) -> String {
    cwd.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(cwd)
        .to_string()
}

/// 终端右键「新建任务」时写入；`open_new_task_modal` 消费后清空。
#[derive(Default, Clone)]
pub struct NewTaskPrefill {
    pub session_id: Option<String>,
    pub cwd: Option<String>,
}

impl Global for NewTaskPrefill {}

// ===================== TaskStore（全局）=====================

pub struct TaskStore;

impl TaskStore {
    pub fn load() -> TaskFile {
        crate::json_store::load_json(tasks_global_path())
    }

    pub fn save(file: &TaskFile) {
        crate::json_store::save_json(tasks_global_path(), file);
    }

    pub fn upsert(task: Task) {
        let mut file = Self::load();
        if let Some(slot) = file.tasks.iter_mut().find(|t| t.id == task.id) {
            *slot = task;
        } else {
            file.tasks.insert(0, task);
        }
        Self::save(&file);
    }

    pub fn remove(id: &str) {
        let mut file = Self::load();
        file.tasks.retain(|t| t.id != id);
        Self::save(&file);
    }

    pub fn get(id: &str) -> Option<Task> {
        Self::load().tasks.into_iter().find(|t| t.id == id)
    }

    pub fn update<F: FnOnce(&mut Task)>(id: &str, f: F) -> Option<Task> {
        let mut file = Self::load();
        let task = file.tasks.iter_mut().find(|t| t.id == id)?;
        f(task);
        task.updated_at = now_secs();
        let out = task.clone();
        Self::save(&file);
        Some(out)
    }

    /// 终端 agent 停转（完成一轮）时：把绑了该 session 且仍在执行/等待的任务标 Done。
    /// 返回 `Some(project_cwd)` 表示确实收尾了至少一条任务（用于触发自动续跑）。
    pub fn mark_session_done(session_id: &str) -> Option<String> {
        let mut file = Self::load();
        let mut done_cwd: Option<String> = None;
        let now = now_secs();
        for t in &mut file.tasks {
            if t.session_id.as_deref() != Some(session_id) {
                continue;
            }
            if matches!(t.column, TaskColumn::Running | TaskColumn::Waiting) {
                t.column = TaskColumn::Done;
                t.updated_at = now;
                if done_cwd.is_none() {
                    done_cwd = Some(t.project_cwd.clone());
                }
            }
        }
        if done_cwd.is_some() {
            Self::save(&file);
        }
        done_cwd
    }

    /// 该项目是否已有执行中任务（同 cwd 串行 worker）。
    pub fn has_running_for_cwd(cwd: &str) -> bool {
        let cwd = cwd.trim_end_matches('/');
        Self::load().tasks.iter().any(|t| {
            t.column.is_active() && t.project_cwd.trim_end_matches('/') == cwd
        })
    }

    /// 任务此刻是否可被**系统**自动取跑（待办 + `auto_run`；定时须已到期）。
    /// 人手点「运行」不走此判断。
    fn is_auto_runnable(t: &Task, now: u64) -> bool {
        if !t.auto_run || !t.column.is_todo() {
            return false;
        }
        match t.kind {
            TaskKind::Once => true,
            TaskKind::Scheduled => t.run_at.map(|at| at <= now).unwrap_or(false),
        }
    }

    /// 原子领取下一条**可自动执行**的待办：标 Running 后返回 id。
    ///
    /// - 只取 `prefer_cwd` 同项目（串行续跑）
    /// - 仅 `auto_run == true` 且可跑
    /// - 该 cwd 已有 Running/Waiting 则不领
    /// - FIFO：`created_at` 升序
    pub fn claim_next_runnable(prefer_cwd: &str) -> Option<String> {
        let prefer = prefer_cwd.trim_end_matches('/');
        if prefer.is_empty() {
            return None;
        }
        let mut file = Self::load();
        let now = now_secs();
        if file.tasks.iter().any(|t| {
            t.column.is_active() && t.project_cwd.trim_end_matches('/') == prefer
        }) {
            return None;
        }
        let mut idxs: Vec<usize> = file
            .tasks
            .iter()
            .enumerate()
            .filter(|(_, t)| {
                t.project_cwd.trim_end_matches('/') == prefer && Self::is_auto_runnable(t, now)
            })
            .map(|(i, _)| i)
            .collect();
        idxs.sort_by_key(|&i| file.tasks[i].created_at);
        let idx = *idxs.first()?;
        file.tasks[idx].column = TaskColumn::Running;
        file.tasks[idx].updated_at = now;
        let id = file.tasks[idx].id.clone();
        Self::save(&file);
        Some(id)
    }

    /// 已到期、可自动执行的单次定时任务 id（按 `run_at` 升序）。
    pub fn due_scheduled_ids() -> Vec<String> {
        let now = now_secs();
        let mut due: Vec<(u64, String)> = Self::load()
            .tasks
            .into_iter()
            .filter(|t| t.is_due(now))
            .map(|t| (t.run_at.unwrap_or(0), t.id))
            .collect();
        due.sort_by_key(|(at, _)| *at);
        due.into_iter().map(|(_, id)| id).collect()
    }
}

// ===================== Workspace =====================

impl Workspace {
    /// 侧栏会话里出现过的项目 cwd 列表（去重，保序）。
    pub fn known_project_cwds(&self, cx: &App) -> Vec<String> {
        let mut out = Vec::new();
        for s in &self.sessions {
            if let Some(c) = s.cwd(cx) {
                if !out.iter().any(|x| x == &c) {
                    out.push(c);
                }
            }
        }
        if out.is_empty() {
            if let Ok(p) = std::env::current_dir() {
                out.push(p.display().to_string());
            }
        }
        out
    }

    /// 新建任务时绑定的项目；无则取第一个 known。
    pub fn task_bind_cwd(&self, cx: &App) -> Option<String> {
        if let Some(c) = &self.task_bind_project {
            if !c.is_empty() {
                return Some(c.clone());
            }
        }
        self.known_project_cwds(cx).into_iter().next()
    }

    /// 新建任务选用的 launch；无则取启动项第一项。
    pub fn task_bind_launch_cmd(&self, cx: &App) -> Option<String> {
        if let Some(c) = &self.task_bind_launch {
            if !c.trim().is_empty() {
                return Some(c.clone());
            }
        }
        active_launch_entries(cx)
            .first()
            .map(|e| e.command.clone())
    }

    pub fn set_task_bind_project(&mut self, cwd: String, cx: &mut Context<Self>) {
        self.task_bind_project = Some(cwd);
        cx.notify();
    }

    pub fn set_task_bind_launch(&mut self, command: String, cx: &mut Context<Self>) {
        self.task_bind_launch = Some(command);
        // 手动选 Agent 时改回「新开终端」
        self.task_bind_session = None;
        cx.notify();
    }

    /// 从指定终端打开新建任务：项目/session 预填，开跑时注入该终端（保留上下文）。
    pub fn open_new_task_for_terminal(
        &mut self,
        pane: &Entity<TerminalView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let sid = pane.read(cx).session_id().to_string();
        let cwd = pane.read(cx).cwd();
        self.task_bind_session = Some(sid);
        if let Some(c) = cwd {
            self.task_bind_project = Some(c);
        }
        // 已有终端路径不强制 Agent 启动项
        self.open_new_task_modal(window, cx);
    }

    pub fn ensure_task_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.task_title_input.is_some() {
            return;
        }
        // body = 首包（主字段）；title = 可选侧栏名
        let body = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .auto_grow(4, 12)
                .placeholder("写给 agent 的第一条指令…")
        });
        let title = cx.new(|cx| {
            InputState::new(window, cx).placeholder("留空则用指令首行")
        });
        let run_at = cx.new(|cx| {
            InputState::new(window, cx).placeholder("YYYY-MM-DD HH:MM（本地时间）")
        });
        self._task_title_sub = None;
        self.task_body_input = Some(body);
        self.task_title_input = Some(title);
        self.task_run_at_input = Some(run_at);
    }

    pub fn set_task_kind(&mut self, kind: TaskKind, window: &mut Window, cx: &mut Context<Self>) {
        self.task_kind = kind;
        if kind == TaskKind::Scheduled {
            if let Some(input) = &self.task_run_at_input {
                let cur = input.read(cx).value().to_string();
                if cur.trim().is_empty() {
                    let def = default_run_at_input();
                    input.update(cx, |s, cx| s.set_value(def, window, cx));
                }
            }
        }
        cx.notify();
    }

    pub fn open_new_task_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ensure_task_inputs(window, cx);
        // 终端右键预填（若有）
        if let Some(pre) = cx.try_global::<NewTaskPrefill>() {
            let pre = pre.clone();
            if pre.session_id.is_some() || pre.cwd.is_some() {
                self.task_bind_session = pre.session_id;
                if let Some(c) = pre.cwd {
                    self.task_bind_project = Some(c);
                }
            }
            *cx.default_global::<NewTaskPrefill>() = NewTaskPrefill::default();
        }
        // 未预填 session 时：默认当前会话项目、新开终端。
        if self.task_bind_session.is_none() {
            if let Some(c) = self.cur().and_then(|s| s.cwd(cx)) {
                self.task_bind_project = Some(c);
            } else if self.task_bind_project.is_none() {
                self.task_bind_project = self.known_project_cwds(cx).into_iter().next();
            }
        }
        if self.task_bind_launch.is_none() {
            self.task_bind_launch = active_launch_entries(cx)
                .first()
                .map(|e| e.command.clone());
        }
        // 每次打开：默认普通 + 可自动执行，清空文案；焦点落在首包。
        self.task_kind = TaskKind::Once;
        self.task_auto_run = true;
        if let Some(input) = &self.task_body_input {
            input.update(cx, |s, cx| {
                s.set_value("", window, cx);
                s.focus(window, cx);
            });
        }
        if let Some(input) = &self.task_title_input {
            input.update(cx, |s, cx| s.set_value("", window, cx));
        }
        if let Some(input) = &self.task_run_at_input {
            input.update(cx, |s, cx| s.set_value(default_run_at_input(), window, cx));
        }
        self.show_new_task_modal = true;
        cx.notify();
    }

    pub fn close_new_task_modal(&mut self, cx: &mut Context<Self>) {
        self.show_new_task_modal = false;
        self.task_bind_session = None;
        self.task_kind = TaskKind::Once;
        self.task_auto_run = true;
        cx.notify();
    }

    /// 从弹窗创建任务。`run` 时：有 `task_bind_session` → 注入该终端；否则新开终端。
    /// 定时且 `run_at` 仍在未来：只入库，等扫描器到点再 `run_task`。
    pub fn create_task_from_inputs(
        &mut self,
        run: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(cwd) = self.task_bind_cwd(cx) else {
            return;
        };
        let body = self
            .task_body_input
            .as_ref()
            .map(|s| s.read(cx).value().to_string())
            .unwrap_or_default();
        let title_in = self
            .task_title_input
            .as_ref()
            .map(|s| s.read(cx).value().to_string())
            .unwrap_or_default()
            .trim()
            .to_string();
        // 必填：首包。标题可选。
        if body.trim().is_empty() {
            return;
        }
        let kind = self.task_kind;
        // 定时本身就是到点自动跑 → 强制 auto_run；普通任务跟弹窗开关。
        let auto_run = kind == TaskKind::Scheduled || self.task_auto_run;
        let run_at = if kind == TaskKind::Scheduled {
            let raw = self
                .task_run_at_input
                .as_ref()
                .map(|s| s.read(cx).value().to_string())
                .unwrap_or_default();
            let Some(at) = parse_local_datetime(&raw) else {
                // 时间非法：不创建（保持弹窗，用户改完再提交）
                return;
            };
            Some(at)
        } else {
            None
        };
        let title = if title_in.is_empty() {
            title_from_prompt(&body)
        } else {
            title_in
        };
        let launch = self.task_bind_launch_cmd(cx);
        // 清掉绑定，避免下次侧栏新建仍绑旧终端
        let sid = self.task_bind_session.take();
        let mut task = Task::new(cwd, title, body, launch);
        task.kind = kind;
        task.run_at = run_at;
        task.auto_run = auto_run;
        let id = task.id.clone();
        self.task_selected = Some(id.clone());
        TaskStore::upsert(task);
        if let Some(input) = &self.task_title_input {
            input.update(cx, |s, cx| s.set_value("", window, cx));
        }
        if let Some(input) = &self.task_body_input {
            input.update(cx, |s, cx| s.set_value("", window, cx));
        }
        self.show_new_task_modal = false;
        self.task_kind = TaskKind::Once;
        self.task_auto_run = true;

        // 定时且未到点：只创建，扫描器稍后开跑。
        let schedule_only = kind == TaskKind::Scheduled
            && run_at.map(|at| at > now_secs()).unwrap_or(true);
        let should_run = run && !schedule_only;

        if let Some(sid) = sid {
            self.assign_task_to_session(&id, &sid, should_run, window, cx);
        } else if should_run {
            self.run_task_in_terminal(&id, window, cx);
        } else {
            cx.notify();
        }
    }

    /// 后台扫描：到期定时任务 → 复用 [`Self::run_task`]。
    /// 同 cwd 已有执行中任务时跳过（串行，留给完成边沿续跑）。
    pub fn tick_scheduled_tasks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ids = TaskStore::due_scheduled_ids();
        if ids.is_empty() {
            return;
        }
        for id in ids {
            let Some(t) = TaskStore::get(&id) else {
                continue;
            };
            if !t.is_due(now_secs()) {
                continue;
            }
            if TaskStore::has_running_for_cwd(&t.project_cwd) {
                continue;
            }
            eprintln!(
                "[tasks] scheduled due id={} run_at={:?}",
                id, t.run_at
            );
            self.run_task(&id, window, cx);
        }
    }

    /// agent 会话刚从 Running→Idle 且收尾了绑定任务：
    /// 同项目 claim 下一条 **auto_run** 待办并 `run_task`（全局始终尝试，闸门在任务字段）。
    pub fn on_session_task_idle(
        &mut self,
        session_id: &str,
        done_cwd: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cwd = if done_cwd.trim().is_empty() {
            self.cwd_for_session(session_id, cx).unwrap_or_default()
        } else {
            done_cwd.to_string()
        };
        if cwd.trim().is_empty() {
            return;
        }
        let Some(id) = TaskStore::claim_next_runnable(&cwd) else {
            return;
        };
        eprintln!(
            "[tasks] auto_run after session={session_id} → next={id} cwd={cwd}"
        );
        self.run_task(&id, window, cx);
    }

    pub fn set_task_auto_run(&mut self, on: bool, cx: &mut Context<Self>) {
        self.task_auto_run = on;
        cx.notify();
    }

    /// 按 smeltd session id 查终端 cwd。
    fn cwd_for_session(&self, session_id: &str, cx: &App) -> Option<String> {
        for sess in &self.sessions {
            let leaves = sess.term_leaves();
            for leaf in leaves {
                if leaf.read(cx).session_id() == session_id {
                    return leaf.read(cx).cwd();
                }
            }
        }
        None
    }

    /// 新建任务弹窗。
    ///
    /// 默认新开终端；从终端右键进入时预绑该 session，开跑 = 键入+回车（沿用上下文）。
    pub fn render_new_task_modal(&self, cx: &mut Context<Self>) -> Div {
        let (fg, muted, border) = {
            let t = cx.theme();
            (t.foreground, t.muted_foreground, t.border)
        };
        let (neutral_bg, neutral_hover, tint, hover, accent_text) =
            Workspace::modal_accent_colors(false);

        let Some(title_in) = self.task_title_input.as_ref() else {
            return div();
        };
        let Some(body_in) = self.task_body_input.as_ref() else {
            return div();
        };

        let projects = self.known_project_cwds(cx);
        let cur_proj = self.task_bind_cwd(cx).unwrap_or_default();
        let proj_btn_label = if cur_proj.is_empty() {
            "当前 / 默认".into()
        } else {
            project_label(&cur_proj)
        };

        let launches = active_launch_entries(cx);
        let cur_launch_cmd = self.task_bind_launch_cmd(cx).unwrap_or_default();
        let agent_btn_label = launches
            .iter()
            .find(|e| e.command == cur_launch_cmd)
            .map(|e| e.label.clone())
            .unwrap_or_else(|| {
                if cur_launch_cmd.is_empty() {
                    "默认启动项".into()
                } else {
                    cur_launch_cmd.clone()
                }
            });
        let agent_icon = if cur_launch_cmd.is_empty() {
            IconName::Bot
        } else {
            icon_for_launch_command(&cur_launch_cmd)
        };

        let on_existing = self.task_bind_session.is_some();
        let is_scheduled = self.task_kind == TaskKind::Scheduled;
        let auto_run = self.task_auto_run || is_scheduled;
        let exec_hint = if is_scheduled {
            "到点后自动新开终端开跑（单次）；也可提前点「运行」"
        } else if auto_run {
            "可自动执行：前一条做完 / 队列有空时系统会接着跑；也可手动「运行」"
        } else if on_existing {
            "仅手动：不会被完成续跑取走；运行 = 键入指令并回车进当前终端"
        } else {
            "仅手动：点「运行」才开终端；不会被系统自动取走"
        };
        let primary_label = if is_scheduled {
            "创建定时"
        } else {
            "创建并运行"
        };

        let field_label = |text: &str| {
            div()
                .text_xs()
                .font_weight(FontWeight::MEDIUM)
                .text_color(muted)
                .child(text.to_string())
        };

        let e = cx.entity().clone();
        let e2 = e.clone();
        let context_row = h_flex()
            .gap_3()
            .items_end()
            .child(
                v_flex()
                    .gap_1()
                    .flex_1()
                    .min_w_0()
                    .child(field_label("项目 · 可选"))
                    .child(
                        Button::new("task-pick-project")
                            .label(proj_btn_label)
                            .icon(IconName::Folder)
                            .small()
                            .w_full()
                            .dropdown_menu({
                                let projects = projects.clone();
                                let e = e.clone();
                                move |menu, _window, _cx| {
                                    let mut menu = menu;
                                    if projects.is_empty() {
                                        return menu.item(
                                            PopupMenuItem::new("暂无项目（先打开终端）")
                                                .disabled(true),
                                        );
                                    }
                                    for p in &projects {
                                        let cwd = p.clone();
                                        let e = e.clone();
                                        let label = project_label(p);
                                        menu = menu.item(
                                            PopupMenuItem::new(label).on_click(move |_, _, cx| {
                                                let cwd = cwd.clone();
                                                e.update(cx, |ws, cx| {
                                                    ws.set_task_bind_project(cwd, cx);
                                                });
                                            }),
                                        );
                                    }
                                    menu
                                }
                            }),
                    ),
            )
            .child(
                v_flex()
                    .gap_1()
                    .flex_1()
                    .min_w_0()
                    .opacity(if on_existing { 0.45 } else { 1. })
                    .child(field_label(if on_existing {
                        "Agent · 当前终端时忽略"
                    } else {
                        "Agent · 可选"
                    }))
                    .child(
                        Button::new("task-pick-agent")
                            .label(agent_btn_label)
                            .icon(agent_icon)
                            .small()
                            .w_full()
                            .dropdown_menu({
                                let launches = launches.clone();
                                move |menu, _window, _cx| {
                                    let mut menu = menu;
                                    if launches.is_empty() {
                                        return menu.item(
                                            PopupMenuItem::new("设置里暂无启动项")
                                                .disabled(true),
                                        );
                                    }
                                    for entry in &launches {
                                        let label = entry.label.clone();
                                        let command = entry.command.clone();
                                        let e = e2.clone();
                                        let icon = icon_for_launch_command(&command);
                                        menu = menu.item(
                                            PopupMenuItem::new(label)
                                                .icon(icon)
                                                .on_click(move |_, _, cx| {
                                                    let command = command.clone();
                                                    e.update(cx, |ws, cx| {
                                                        ws.set_task_bind_launch(command, cx);
                                                    });
                                                }),
                                        );
                                    }
                                    menu
                                }
                            }),
                    ),
            );

        // 类型：普通 / 定时
        let kind_row = h_flex()
            .gap_2()
            .items_center()
            .child(
                Button::new("task-kind-once")
                    .label("普通")
                    .small()
                    .when(self.task_kind == TaskKind::Once, |b| b.primary())
                    .when(self.task_kind != TaskKind::Once, |b| b.ghost())
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.set_task_kind(TaskKind::Once, window, cx);
                    })),
            )
            .child(
                Button::new("task-kind-scheduled")
                    .label("定时")
                    .small()
                    .when(is_scheduled, |b| b.primary())
                    .when(!is_scheduled, |b| b.ghost())
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.set_task_kind(TaskKind::Scheduled, window, cx);
                    })),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(if is_scheduled {
                        "单次 · 到点自动开跑"
                    } else {
                        "普通待办"
                    }),
            );

        // 任务级：是否允许系统自动执行（定时强制开）
        let auto_row = h_flex()
            .gap_2()
            .items_center()
            .child(
                Button::new("task-auto-run-on")
                    .label("可自动执行")
                    .small()
                    .when(auto_run, |b| b.primary())
                    .when(!auto_run, |b| b.ghost())
                    .disabled(is_scheduled)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.set_task_auto_run(true, cx);
                    })),
            )
            .child(
                Button::new("task-auto-run-off")
                    .label("仅手动")
                    .small()
                    .when(!auto_run, |b| b.primary())
                    .when(auto_run, |b| b.ghost())
                    .disabled(is_scheduled)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.set_task_auto_run(false, cx);
                    })),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(if is_scheduled {
                        "定时任务默认自动"
                    } else if auto_run {
                        "完成续跑 / 队列会取它"
                    } else {
                        "只等人点运行"
                    }),
            );

        let content = v_flex()
            .gap_3()
            .child(
                div()
                    .font_bold()
                    .text_color(fg)
                    .text_lg()
                    .child(if on_existing {
                        "新建任务 · 当前终端"
                    } else {
                        "新建任务"
                    }),
            )
            .when(on_existing, |d| {
                d.child(
                    div()
                        .px_3()
                        .py_2()
                        .rounded_lg()
                        .bg(rgba(0x4a9eff18))
                        .text_xs()
                        .text_color(rgb(0x8fc7ff))
                        .child("已绑定侧栏选中的终端，运行会把指令发进该会话。"),
                )
            })
            .child(
                v_flex()
                    .gap_1()
                    .child(field_label("类型"))
                    .child(kind_row),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(field_label("自动执行"))
                    .child(auto_row),
            )
            .when(is_scheduled, |d| {
                let run_at_in = self.task_run_at_input.as_ref();
                d.child(
                    v_flex()
                        .gap_1()
                        .child(field_label("执行时间 · 本地（YYYY-MM-DD HH:MM）"))
                        .children(run_at_in.map(|i| Input::new(i))),
                )
            })
            .child(
                v_flex()
                    .gap_1()
                    .child(field_label("指令（必填 · 给 agent 的首包）"))
                    .child(Input::new(body_in)),
            )
            .child(context_row)
            .child(
                v_flex()
                    .gap_1()
                    .child(field_label("侧栏标题 · 可选"))
                    .child(Input::new(title_in)),
            )
            .child(div().text_xs().text_color(muted).child(exec_hint))
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .pt_1()
                    .border_t_1()
                    .border_color(border)
                    .child(Workspace::modal_button(
                        "cancel-new-task",
                        "取消",
                        neutral_bg,
                        neutral_hover,
                        fg,
                        |this, _, _, cx| this.close_new_task_modal(cx),
                        cx,
                    ))
                    .child(Workspace::modal_button(
                        "create-only-task",
                        "仅创建",
                        neutral_bg,
                        neutral_hover,
                        fg,
                        |this, _, window, cx| this.create_task_from_inputs(false, window, cx),
                        cx,
                    ))
                    .child(Workspace::modal_button(
                        "confirm-new-task",
                        primary_label,
                        tint,
                        hover,
                        accent_text,
                        |this, _, window, cx| this.create_task_from_inputs(true, window, cx),
                        cx,
                    )),
            );
        Workspace::modal_shell(500., false, content, cx)
    }

    pub fn delete_task(&mut self, id: &str, cx: &mut Context<Self>) {
        TaskStore::remove(id);
        if self.task_selected.as_deref() == Some(id) {
            self.task_selected = None;
        }
        cx.notify();
    }

    /// 直接设为指定列（任务卡片状态下拉用）。
    pub fn set_task_column(&mut self, id: &str, col: TaskColumn, cx: &mut Context<Self>) {
        TaskStore::update(id, |t| t.column = col);
        cx.notify();
    }

    /// 任务总览 pill：点同一状态再点一次回到「全部」。
    pub fn set_task_column_filter(&mut self, col: Option<TaskColumn>, cx: &mut Context<Self>) {
        self.task_column_filter = if self.task_column_filter == col {
            None
        } else {
            col
        };
        cx.notify();
    }

    /// 主区任务总览：会话总览同款气质，任务专属信息层级。
    pub fn render_tasks_page(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> Div {
        let (fg, muted, border) = {
            let t = cx.theme();
            (t.foreground, t.muted_foreground, t.border)
        };
        let soft_bg: Hsla = rgba(0xffffff0d).into();
        let card_bg = rgb(0x17181d);
        let card_border = rgba(0xffffff12);

        let mut all = TaskStore::load().tasks;
        all.sort_by_key(|t| (t.column.sidebar_rank(), std::cmp::Reverse(t.updated_at)));
        let n_all = all.len();
        let n_run = all.iter().filter(|t| t.column.is_active()).count();
        let n_todo = all.iter().filter(|t| t.column.is_todo()).count();
        let n_done = all.iter().filter(|t| t.column == TaskColumn::Done).count();
        if let Some(f) = self.task_column_filter {
            all.retain(|t| match f {
                TaskColumn::Running | TaskColumn::Waiting => t.column.is_active(),
                TaskColumn::Backlog | TaskColumn::Ready => t.column.is_todo(),
                TaskColumn::Done => t.column == TaskColumn::Done,
            });
        }

        let pill = |id: &'static str,
                    text: String,
                    col: Option<TaskColumn>,
                    color: Hsla,
                    bg: Hsla| {
            // 「全部」仅在无筛选时高亮
            let active = if col.is_none() {
                self.task_column_filter.is_none()
            } else {
                self.task_column_filter == col
            };
            div()
                .id(id)
                .px(px(12.))
                .py(px(5.))
                .rounded_full()
                .cursor_pointer()
                .border_1()
                .border_color(if active {
                    color
                } else {
                    Hsla::from(rgba(0x00000000))
                })
                .bg(if active { bg } else { soft_bg })
                .text_sm()
                .font_weight(if active {
                    FontWeight::SEMIBOLD
                } else {
                    FontWeight::NORMAL
                })
                .text_color(if active { color } else { muted })
                .hover(|s| s.bg(bg).text_color(color))
                .child(text)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        this.set_task_column_filter(col, cx);
                    }),
                )
        };

        let c_blue: Hsla = rgb(0x4a9eff).into();
        let c_gray: Hsla = rgb(0x8b93a7).into();
        let c_green: Hsla = rgb(0x22c55e).into();
        let blue_tint: Hsla = rgba(0x4a9eff28).into();
        let gray_tint: Hsla = rgba(0x8b93a728).into();
        let green_tint: Hsla = rgba(0x22c55e28).into();

        let summary = div()
            .flex()
            .items_center()
            .gap_2()
            .flex_wrap()
            .child(pill(
                "tp-all",
                format!("{n_all} 任务"),
                None,
                fg,
                soft_bg,
            ))
            .child(pill(
                "tp-run",
                format!("{n_run} 执行中"),
                Some(TaskColumn::Running),
                c_blue,
                blue_tint,
            ))
            .child(pill(
                "tp-todo",
                format!("{n_todo} 待办"),
                Some(TaskColumn::Backlog),
                c_gray,
                gray_tint,
            ))
            .child(pill(
                "tp-done",
                format!("{n_done} 完成"),
                Some(TaskColumn::Done),
                c_green,
                green_tint,
            ));

        let header = div()
            .px_6()
            .pt_5()
            .pb_4()
            .border_b_1()
            .border_color(border)
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .flex()
                    .items_start()
                    .justify_between()
                    .gap_3()
                    .child(
                        v_flex()
                            .gap_1()
                            .child(
                                div()
                                    .text_xl()
                                    .font_bold()
                                    .text_color(fg)
                                    .child("任务总览"),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .child("点状态徽章可改状态 · 终端右键可绑当前会话新建"),
                            ),
                    )
                    .child(
                        Button::new("tasks-page-new")
                            .label("新建任务")
                            .icon(IconName::Plus)
                            .small()
                            .primary()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_new_task_modal(window, cx);
                            })),
                    ),
            )
            .child(summary);

        let body = if all.is_empty() {
            div()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_4()
                .py_20()
                .child(
                    div()
                        .size(px(56.))
                        .rounded_full()
                        .bg(soft_bg)
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            Icon::new(IconName::Bot)
                                .size(px(28.))
                                .text_color(muted),
                        ),
                )
                .child(div().text_sm().text_color(fg).font_weight(FontWeight::MEDIUM).child(
                    if n_all == 0 {
                        "还没有任务"
                    } else {
                        "这个筛选下是空的"
                    },
                ))
                .child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(if n_all == 0 {
                            "新建一条，或在终端右键「新建任务」"
                        } else {
                            "换个状态 pill，或清除筛选看全部"
                        }),
                )
                .when(n_all == 0, |d| {
                    d.child(
                        Button::new("tasks-empty-new")
                            .label("新建任务")
                            .primary()
                            .icon(IconName::Plus)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_new_task_modal(window, cx);
                            })),
                    )
                })
        } else {
            let mut grid = div().flex().flex_wrap().gap_4();
            for task in &all {
                grid = grid.child(self.render_task_overview_card(
                    task,
                    card_bg,
                    card_border,
                    fg,
                    muted,
                    cx,
                ));
            }
            grid
        };

        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(header)
            .child(
                div()
                    .id("tasks-overview-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_6()
                    .py_5()
                    .child(body),
            )
    }

    fn render_task_overview_card(
        &self,
        task: &Task,
        card_bg: impl Into<Hsla>,
        card_border: impl Into<Hsla>,
        fg: Hsla,
        muted: Hsla,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let card_bg = card_bg.into();
        let card_border = card_border.into();
        let id = task.id.clone();
        let id_run = id.clone();
        let id_col = id.clone();
        let id_del = id.clone();
        let title = task.title.clone();
        let proj = project_label(&task.project_cwd);
        let col = task.column;
        let col_color: Hsla = rgb(col.color()).into();
        let col_tint: Hsla = match col {
            TaskColumn::Running | TaskColumn::Waiting => rgba(0x4a9eff22).into(),
            TaskColumn::Done => rgba(0x22c55e22).into(),
            _ => rgba(0x8b93a722).into(),
        };
        let body_prev = {
            let t = task.body.trim();
            if t.is_empty() {
                String::new()
            } else if t.chars().count() > 96 {
                format!("{}…", t.chars().take(96).collect::<String>())
            } else {
                t.to_string()
            }
        };
        let has_session = task.session_id.is_some();
        let primary: Option<&'static str> = if col.is_todo() {
            Some("运行")
        } else if has_session {
            Some("打开")
        } else if col.is_active() {
            Some("运行")
        } else {
            None
        };
        let schedule_label = if task.kind == TaskKind::Scheduled {
            task.run_at.map(|at| {
                let when = format_run_at_short(at);
                let kind = TaskKind::Scheduled.label();
                if col.is_todo() && at <= now_secs() {
                    format!("{kind} · 已到期 {when}")
                } else if col.is_todo() {
                    format!("{kind} · {when}")
                } else {
                    format!("{kind} · 计划 {when}")
                }
            })
        } else {
            None
        };
        // 待办且可自动执行时标一下（定时已有徽章可省略）
        let auto_label = if task.kind == TaskKind::Once && col.is_todo() {
            Some(if task.auto_run {
                "自动"
            } else {
                "手动"
            })
        } else {
            None
        };
        let e_status = cx.entity().clone();
        let id_status = id_col.clone();

        // 状态徽章可点：下拉改状态（不占操作行）
        let status_badge = Button::new(SharedString::from(format!("tc-st-{id}")))
            .label(col.label())
            .xsmall()
            .ghost()
            .dropdown_menu(move |menu, _window, _cx| {
                let mut menu = menu;
                for c in TaskColumn::ui_choices() {
                    let e = e_status.clone();
                    let tid = id_status.clone();
                    let label = c.label();
                    menu = menu.item(PopupMenuItem::new(label).on_click(move |_, _, cx| {
                        let tid = tid.clone();
                        e.update(cx, |ws, cx| {
                            ws.set_task_column(&tid, c, cx);
                        });
                    }));
                }
                menu
            });

        // 不画「选中描边」：task_selected 会让某张卡永久亮一圈边，像坏了一样。
        // 与会话总览一致，只靠 hover 反馈。
        div()
            .id(SharedString::from(format!("task-card-{id}")))
            .w(px(300.))
            .p_4()
            .rounded(px(18.))
            .border_1()
            .border_color(card_border)
            .bg(card_bg)
            .shadow_sm()
            .hover(|d| d.border_color(col_color).shadow_lg().bg(rgb(0x1c1e24)))
            .flex()
            .flex_col()
            .gap_3()
            // 标题：状态点 + 名（对齐会话总览）
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .min_w_0()
                    .child(
                        div()
                            .size(px(9.))
                            .rounded_full()
                            .bg(col_color)
                            .flex_shrink_0(),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_semibold()
                            .text_color(fg)
                            .child(title),
                    ),
            )
            // 元信息：状态徽章 · 项目
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .min_w_0()
                    .child(
                        // 包一层 tint 底，让 ghost 下拉看起来像徽章而不是灰按钮
                        div()
                            .rounded_full()
                            .bg(col_tint)
                            .child(status_badge),
                    )
                    .when(schedule_label.is_some(), |d| {
                        let lab = schedule_label.clone().unwrap_or_default();
                        d.child(
                            div()
                                .rounded_full()
                                .px_2()
                                .py_1()
                                .bg(rgba(0xa78bfa22))
                                .text_xs()
                                .text_color(rgb(0xc4b5fd))
                                .child(lab),
                        )
                    })
                    .when(auto_label.is_some(), |d| {
                        let lab = auto_label.unwrap_or("");
                        let (bg, fg): (Hsla, Hsla) = if task.auto_run {
                            (rgba(0x22c55e22).into(), rgb(0x86efac).into())
                        } else {
                            (rgba(0x8b93a722).into(), muted)
                        };
                        d.child(
                            div()
                                .rounded_full()
                                .px_2()
                                .py_1()
                                .bg(bg)
                                .text_xs()
                                .text_color(fg)
                                .child(lab),
                        )
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_xs()
                            .text_color(muted)
                            .truncate()
                            .child(if has_session {
                                format!("{proj} · 已绑会话")
                            } else {
                                proj
                            }),
                    ),
            )
            // 指令摘要（与会话预览同款深底）
            .when(!body_prev.is_empty(), |d| {
                d.child(
                    div()
                        .p_2()
                        .rounded_lg()
                        .bg(rgb(0x0d0d10))
                        .text_xs()
                        .text_color(muted)
                        .line_clamp(3)
                        .child(body_prev),
                )
            })
            // 操作：主操作 + 删除（不再把状态塞进这一行）
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .children(primary.map(|label| {
                        Button::new(SharedString::from(format!("tc-run-{id}")))
                            .label(label)
                            .small()
                            .primary()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.primary_task_action(&id_run, window, cx);
                            }))
                    }))
                    .child(
                        Button::new(SharedString::from(format!("tc-del-{id}")))
                            .label("删除")
                            .small()
                            .ghost()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.delete_task(&id_del, cx);
                            })),
                    ),
            )
    }

    /// 绑到指定终端；`inject` 时键入首包并回车（当前终端上下文执行）。
    pub fn assign_task_to_session(
        &mut self,
        id: &str,
        sid: &str,
        inject: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(task) = TaskStore::get(id) else {
            return;
        };
        let prompt = task_prompt(&task);

        let mut found: Option<(usize, Entity<TerminalView>)> = None;
        for i in 0..self.sessions.len() {
            for leaf in self.sessions[i].term_leaves() {
                if leaf.read(cx).session_id() == sid {
                    found = Some((i, leaf));
                    break;
                }
            }
            if found.is_some() {
                break;
            }
        }
        let Some((ix, leaf)) = found else {
            eprintln!("[tasks] assign: 找不到会话 {sid}，回退新开终端");
            if inject {
                self.run_task_in_terminal(id, window, cx);
            } else {
                cx.notify();
            }
            return;
        };

        let cwd = leaf.read(cx).cwd();
        TaskStore::update(id, |t| {
            t.session_id = Some(sid.to_string());
            if let Some(c) = cwd {
                t.project_cwd = c;
            }
            if inject {
                t.column = TaskColumn::Running;
            }
        });

        self.active_session = ix;
        self.sessions[ix].set_active_term(leaf.clone());
        self.view = crate::MainView::Terminal;

        if inject && !prompt.is_empty() {
            leaf.update(cx, |tv, cx| {
                tv.send_text_and_submit(&prompt, cx);
            });
        }

        self.focus_active(window, cx);
        cx.notify();
    }

    /// 按 session_id 聚焦已有侧栏终端；找到返回 true。
    pub fn focus_session_by_id(
        &mut self,
        sid: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        for i in 0..self.sessions.len() {
            for leaf in self.sessions[i].term_leaves() {
                if leaf.read(cx).session_id() == sid {
                    self.active_session = i;
                    self.view = crate::MainView::Terminal;
                    self.sessions[i].set_active_term(leaf);
                    self.focus_active(window, cx);
                    cx.notify();
                    return true;
                }
            }
        }
        false
    }

    /// 卡片主按钮：待办 → [`Self::run_task`]；已跑过 → 聚焦会话。
    pub fn primary_task_action(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.task_selected = Some(id.to_string());
        let Some(task) = TaskStore::get(id) else {
            return;
        };
        if task.column.is_todo() {
            self.run_task(id, window, cx);
            return;
        }
        if let Some(sid) = task.session_id.as_ref() {
            if self.focus_session_by_id(sid, window, cx) {
                return;
            }
        }
        // 执行中但会话已丢 → 再新开
        if task.column.is_active() {
            self.run_task_in_terminal(id, window, cx);
        }
        cx.notify();
    }

    /// 侧栏快捷：有会话则聚焦，待办/无会话则开跑。
    pub fn focus_or_run_task(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.task_selected = Some(id.to_string());
        let Some(task) = TaskStore::get(id) else {
            return;
        };
        if let Some(sid) = task.session_id.as_ref() {
            if !task.column.is_todo() && self.focus_session_by_id(sid, window, cx) {
                return;
            }
        }
        if task.column.is_todo() || task.column.is_active() {
            self.run_task(id, window, cx);
            return;
        }
        if let Some(sid) = task.session_id.as_ref() {
            let _ = self.focus_session_by_id(sid, window, cx);
        }
        cx.notify();
    }

    /// 执行任务：有绑定且仍存活的会话 → 注入该终端；否则新开终端。
    pub fn run_task(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(task) = TaskStore::get(id) else {
            return;
        };
        if let Some(sid) = task.session_id.clone() {
            // 会话还在：把首包打进该终端（右键新建仅创建后的「开跑」）
            let mut alive = false;
            for sess in &self.sessions {
                let leaves = sess.term_leaves();
                if leaves.iter().any(|l| l.read(cx).session_id() == sid) {
                    alive = true;
                    break;
                }
            }
            if alive {
                self.assign_task_to_session(id, &sid, true, window, cx);
                return;
            }
        }
        self.run_task_in_terminal(id, window, cx);
    }

    /// 在侧栏**新开终端**跑任务：`base_launch + "首包"` 编进 smeltd launch（startup-arg）。
    ///
    /// **不**往 PTY 粘贴/回车——agent 进程启动即带第一条用户消息（可自循环调度）。
    pub fn run_task_in_terminal(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(task) = TaskStore::get(id) else {
            return;
        };
        let cwd = if task.project_cwd.trim().is_empty() {
            None
        } else {
            Some(task.project_cwd.clone())
        };
        let base_launch = task
            .launch
            .clone()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                active_launch_entries(cx)
                    .first()
                    .map(|e| e.command.clone())
            })
            .unwrap_or_else(|| "claude".into());
        let label = task.title.clone();
        let prompt = task_prompt(&task);

        // 有首包 → 写文件后拼进 launch；无首包 → 只起空 agent。
        let launch_cmd = if prompt.trim().is_empty() {
            base_launch.clone()
        } else if let Some(path) = write_prompt_file(&task.id, &prompt) {
            build_launch_with_prompt(&base_launch, &path)
        } else {
            // 落盘失败：内联单引号（多行可能不完美，但强于静默失败）
            format!("{base_launch} {}", shell_single_quote(&prompt))
        };

        eprintln!("[tasks] run launch={launch_cmd}");

        let sid = new_sid();
        // 同 add_session_with_launch：FFI 回调栈上 panic = abort 整个 app，
        // spawn 失败就不起任务终端，留日志。
        let terminal = match crate::terminal::Terminal::spawn(
            24,
            80,
            cwd.as_deref(),
            &sid,
            Some(launch_cmd.as_str()),
        ) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("[tasks] 任务终端启动失败（{cwd:?}）：{e:#}");
                return;
            }
        };
        let view = cx.new(|cx| {
            TerminalView::from_terminal(
                cx,
                terminal,
                cwd,
                sid.clone(),
                Some(launch_cmd.as_str()),
                Some(label.as_str()),
            )
        });
        self.sessions.push(crate::Session::single(view));
        self.active_session = self.sessions.len() - 1;
        self.view = crate::MainView::Terminal;

        // 存 base（不含 prompt 拼接），再跑时重新拼首包。
        TaskStore::update(id, |t| {
            t.session_id = Some(sid);
            t.column = TaskColumn::Running;
            t.launch = Some(base_launch);
        });

        self.save_state(cx);
        self.focus_active(window, cx);
        cx.notify();
    }
}

// ===================== 测试 =====================

#[cfg(test)]
mod task_model_tests {
    use super::{
        build_launch_with_prompt, parse_local_datetime, shell_single_quote, Task, TaskColumn,
        TaskFile, TaskKind, TaskStore,
    };
    use std::path::Path;

    fn project_key(cwd: &str) -> String {
        let p = Path::new(cwd);
        let s = std::fs::canonicalize(p)
            .unwrap_or_else(|_| p.to_path_buf())
            .to_string_lossy()
            .into_owned();
        s.chars()
            .map(|c| match c {
                '/' | '\\' | ':' => '-',
                c if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' => c,
                _ => '_',
            })
            .collect()
    }

    #[test]
    fn project_key_is_filesystem_safe() {
        let k = project_key("/Users/foo/bar baz");
        assert!(!k.contains('/'));
        assert!(!k.is_empty());
    }

    #[test]
    fn task_file_json_roundtrip() {
        let mut t = Task::new(
            "/tmp/p".into(),
            "t1".into(),
            "body".into(),
            Some("claude".into()),
        );
        t.column = TaskColumn::Running;
        t.kind = TaskKind::Scheduled;
        t.run_at = Some(1_700_000_000);
        let file = TaskFile { tasks: vec![t] };
        let json = serde_json::to_string_pretty(&file).unwrap();
        let back: TaskFile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tasks[0].title, "t1");
        assert_eq!(back.tasks[0].kind, TaskKind::Scheduled);
        assert_eq!(back.tasks[0].run_at, Some(1_700_000_000));
        assert_eq!(TaskColumn::Ready.label(), "待办");
        assert_eq!(TaskColumn::Waiting.label(), "执行中");
    }

    #[test]
    fn old_json_defaults_kind_to_once() {
        let json = r#"{"tasks":[{"id":"a","title":"t","body":"b","project_cwd":"/x"}]}"#;
        let back: TaskFile = serde_json::from_str(json).unwrap();
        assert_eq!(back.tasks[0].kind, TaskKind::Once);
        assert!(back.tasks[0].run_at.is_none());
    }

    #[test]
    fn scheduled_is_due_when_past() {
        let mut t = Task::new("/x".into(), "t".into(), "b".into(), None);
        t.kind = TaskKind::Scheduled;
        t.run_at = Some(100);
        assert!(t.is_due(100));
        assert!(t.is_due(200));
        assert!(!t.is_due(99));
        t.column = TaskColumn::Running;
        assert!(!t.is_due(200));
    }

    #[test]
    fn parse_local_datetime_accepts_minute_precision() {
        let at = parse_local_datetime("2030-01-15 18:30").expect("parse");
        assert!(at > 1_800_000_000);
        assert!(parse_local_datetime("").is_none());
        assert!(parse_local_datetime("not-a-date").is_none());
    }

    #[test]
    fn is_auto_runnable_skips_future_scheduled_and_manual() {
        let now = 1_000u64;
        let once = Task::new("/p".into(), "a".into(), "b".into(), None);
        assert!(TaskStore::is_auto_runnable(&once, now));

        let mut manual = Task::new("/p".into(), "a".into(), "b".into(), None);
        manual.auto_run = false;
        assert!(!TaskStore::is_auto_runnable(&manual, now));

        let mut sched = Task::new("/p".into(), "a".into(), "b".into(), None);
        sched.kind = TaskKind::Scheduled;
        sched.run_at = Some(2_000);
        assert!(!TaskStore::is_auto_runnable(&sched, now));
        sched.run_at = Some(500);
        assert!(TaskStore::is_auto_runnable(&sched, now));
        sched.auto_run = false;
        assert!(!TaskStore::is_auto_runnable(&sched, now));
    }

    #[test]
    fn claim_next_is_fifo_same_cwd_auto_only() {
        let mut a = Task::new("/proj".into(), "1".into(), "b1".into(), None);
        a.created_at = 10;
        let mut b = Task::new("/proj".into(), "2".into(), "b2".into(), None);
        b.created_at = 5;
        b.auto_run = false; // 更早但不自动 → 跳过
        let mut c = Task::new("/proj".into(), "3".into(), "b3".into(), None);
        c.created_at = 20;
        let mut list = vec![a, b, c];
        list.retain(|t| t.project_cwd == "/proj" && TaskStore::is_auto_runnable(t, 999));
        list.sort_by_key(|t| t.created_at);
        assert_eq!(list[0].title, "1");
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn old_json_defaults_auto_run_true() {
        let json = r#"{"tasks":[{"id":"a","title":"t","body":"b","project_cwd":"/x"}]}"#;
        let back: TaskFile = serde_json::from_str(json).unwrap();
        assert!(back.tasks[0].auto_run);
    }

    #[test]
    fn task_prompt_is_body_only() {
        let t = Task::new("/x".into(), "侧栏标题".into(), "真正给 agent 的指令".into(), None);
        assert_eq!(super::task_prompt(&t), "真正给 agent 的指令");
    }

    #[test]
    fn task_prompt_falls_back_to_title_when_body_empty() {
        let t = Task::new("/x".into(), "only title".into(), String::new(), None);
        assert_eq!(super::task_prompt(&t), "only title");
    }

    #[test]
    fn title_from_prompt_takes_first_line() {
        assert_eq!(
            super::title_from_prompt("第一行\n第二行"),
            "第一行"
        );
    }

    #[test]
    fn shell_single_quote_escapes() {
        assert_eq!(shell_single_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn launch_with_prompt_appends_cat_not_dash_p() {
        let p = Path::new("/tmp/prompt.txt");
        let cmd = build_launch_with_prompt("claude --dangerously-skip-permissions", p);
        assert!(cmd.starts_with("claude --dangerously-skip-permissions "));
        assert!(cmd.contains("\"$(cat "));
        assert!(!cmd.contains(" -p "));
        assert!(!cmd.contains("-p "));
    }

    #[test]
    fn empty_base_defaults_to_claude() {
        let p = Path::new("/tmp/p.txt");
        let cmd = build_launch_with_prompt("", p);
        assert!(cmd.starts_with("claude "));
        assert!(cmd.contains("\"$(cat "));
    }
}
