//! 远程操作网关的核心逻辑（路由 + handler + HTML 模板），供两个地方 `#[path]` 引入：
//! - `src/bin/gateway.rs`：独立进程，命令行启动，自己管一个 `--bind`/`--port`
//! - `src/bin/smeltd.rs`：内嵌进守护，靠 `remote_start`/`remote_stop` op 按需开关
//!
//! 两边共用同一份 handler，避免同一套鉴权/转义/协议逻辑复制两次（CLAUDE.md 明令
//! 别复制）。这个模块本身**不碰 smeltd 主协议**：所有跟 smeltd 的交互都是走
//! `sock_path()` 连它自己的 unix socket，用既有的 `list`/`watch` op——不管是从独立
//! 进程调用还是从 smeltd 内部的这个模块调用，走的都是同一条路径，行为完全一致。
//!
//! 见 docs/remote-ops-roadmap.md（Phase 1/2）、docs/collaboration.md（安全底线）。

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;

const REFERENCE_PAGE: &str = include_str!("remote_gateway_page.html");
const LIST_PAGE: &str = include_str!("remote_gateway_list_page.html");
const CONSOLE_PAGE: &str = include_str!("remote_gateway_console_page.html");

/// Preact 远程 H5 构建产物目录（`remote-web/dist`）。编译期锚定仓库根；
/// 未 build 前端时回退到旧 HTML 模板，开发不断档。
fn remote_web_dist() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("remote-web").join("dist")
}

fn spa_index_path() -> PathBuf {
    remote_web_dist().join("index.html")
}

fn spa_ready() -> bool {
    spa_index_path().is_file()
}

pub fn sock_path() -> std::path::PathBuf {
    let dir = dirs::home_dir().unwrap_or_else(|| "/tmp".into()).join(".smelt");
    dir.join("smeltd.sock")
}

#[derive(Clone)]
struct AppState {
    token: Arc<String>,
    /// 这个 token 是否有写权限（approve/deny/reply）。链接分享出去那一刻就是
    /// 授权动作，这里不再加一层"每次点击都要主人当面确认"——见
    /// smeltd.rs「远程操控」一节的授权模型说明。开没开由生成链接时的 GUI 开关
    /// 决定，`build_router` 只是如实转达。
    write_enabled: bool,
}

#[derive(Deserialize)]
struct AuthQuery {
    token: String,
}

#[derive(Deserialize)]
struct ActionBody {
    kind: String,
    #[serde(default)]
    text: Option<String>,
}

/// 原始输入：UTF-8 字符串，控制字符用 JSON `\u00xx`（xterm onData 直接 stringify 即可）。
#[derive(Deserialize)]
struct InputBody {
    data: String,
}

#[derive(Deserialize)]
struct ResizeBody {
    cols: u16,
    rows: u16,
    #[serde(default)]
    cell_w: u16,
    #[serde(default)]
    cell_h: u16,
}

/// 组好整个网关的路由，鉴权用这一个 token（见 collaboration.md：一个网关/token 管
/// 这台机器上的全部活会话，泄漏一条链接的代价是明确的，不是没想到的疏漏）。
///
/// 前端：优先托管 `remote-web/dist`（Preact + Tailwind + xterm 的 CLI 面板）。
/// 未构建时回退内嵌 HTML（list / console / xterm）。
pub fn build_router(token: String, write_enabled: bool) -> Router {
    let state = AppState { token: Arc::new(token), write_enabled };
    let mut r = Router::new()
        .route("/sessions", get(sessions_json_handler))
        .route("/s/{id}/stream", get(stream_handler))
        .route("/s/{id}/state-stream", get(state_stream_handler))
        .route("/s/{id}/action", axum::routing::post(action_handler))
        .route("/s/{id}/input", axum::routing::post(input_handler))
        .route("/s/{id}/resize", axum::routing::post(resize_handler));

    if spa_ready() {
        // SPA：/ 与 /s/:id 都回 index.html（注入 write meta）；静态资源 /assets/*
        r = r
            .route("/", get(spa_index_handler))
            .route("/s/{id}", get(spa_index_handler_with_id))
            .route("/s/{id}/console", get(spa_index_handler_with_id))
            .route("/assets/{*path}", get(spa_asset_handler));
    } else {
        r = r
            .route("/", get(list_page_handler))
            .route("/s/{id}", get(page_handler))
            .route("/s/{id}/console", get(console_handler));
    }

    r.with_state(state)
}

/// 读 dist/index.html，注入 write 权限 meta + 当前 token 提示（token 仍走 query）。
fn spa_index_html(write_enabled: bool) -> Response {
    let path = spa_index_path();
    let Ok(mut raw) = std::fs::read_to_string(&path) else {
        return (StatusCode::SERVICE_UNAVAILABLE, "remote-web 未构建：cd remote-web && npm run build")
            .into_response();
    };
    let meta = format!(
        r#"<meta name="smelt-write" content="{}" />"#,
        if write_enabled { "true" } else { "false" }
    );
    if raw.contains("</head>") {
        raw = raw.replacen("</head>", &format!("{meta}
</head>"), 1);
    } else {
        raw = format!("{meta}
{raw}");
    }
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            // 开发迭代频繁：禁止缓存 index，避免手机仍加载旧 JS 哈希
            (header::CACHE_CONTROL, "no-store, max-age=0"),
        ],
        raw,
    )
        .into_response()
}

async fn spa_index_handler(
    Query(q): Query<AuthQuery>,
    State(state): State<AppState>,
) -> Response {
    if q.token != *state.token {
        return (StatusCode::FORBIDDEN, "token 不对").into_response();
    }
    spa_index_html(state.write_enabled)
}

async fn spa_index_handler_with_id(
    Path(_id): Path<String>,
    Query(q): Query<AuthQuery>,
    State(state): State<AppState>,
) -> Response {
    if q.token != *state.token {
        return (StatusCode::FORBIDDEN, "token 不对").into_response();
    }
    spa_index_html(state.write_enabled)
}

/// 托管 Vite 产物：/assets/...
async fn spa_asset_handler(Path(path): Path<String>) -> Response {
    // 防目录穿越
    if path.contains("..") || path.starts_with('/') {
        return (StatusCode::BAD_REQUEST, "bad path").into_response();
    }
    let full = remote_web_dist().join("assets").join(&path);
    serve_file(&full)
}

fn serve_file(full: &FsPath) -> Response {
    let Ok(bytes) = std::fs::read(full) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let ct = match full.extension().and_then(|e| e.to_str()) {
        Some("js") => "application/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("woff2") => "font/woff2",
        Some("map") => "application/json",
        _ => "application/octet-stream",
    };
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, ct),
            // 带 content-hash 的文件名已可长期缓存；仍给短 max-age，方便迭代
            (header::CACHE_CONTROL, "public, max-age=120"),
        ],
        bytes,
    )
        .into_response()
}

/// 把字符串安全地嵌进内联 `<script>` 里的 JS 字符串字面量：JSON 转义处理引号/
/// 反斜杠，额外把尖括号转成 Unicode 转义序列——防止 id/token 里带 `</script>`
/// 提前把这段脚本切断（HTML 解析器找 `</script` 是纯文本匹配，不管有没有在字符串里）。
fn js_string_literal(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string()).replace('<', "\\u003c")
}

/// 把字符串安全地嵌进 HTML 正文/属性：转义 `& < > "`。会话列表页用它嵌 session id——
/// 现在 id 都是 GUI 用 `uuid::Uuid::new_v4()` 生成的（见 workspace/main.rs），字符集
/// 天然安全，这里是防御性的，防止以后 id 格式变了变成新的注入面。
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// 远程列表里一条可 attach 的终端（smeltd 会话）。展示名优先走 GUI 的
/// `~/.smelt/workspace.json`（用户重命名 / 快捷启动标签），跟 PC 侧栏一致；
/// 没有 GUI 元数据时再回退 smeltd 的 title / launch / cwd 末段。
#[derive(Clone, serde::Serialize)]
struct SessionInfo {
    id: String,
    phase: String,
    pending_question: Option<String>,
    /// 列表主标题（名称，不是 uuid）。
    name: String,
    /// 项目分组名：cwd 目录末段（与 workspace 侧栏 `project_name_for_cwd` 同规则）。
    project: String,
    /// 多 pane 会话的父会话名（如 "services"）；单 pane 为 None，直接挂在项目下。
    parent_session: Option<String>,
    cwd: Option<String>,
}

/// GUI 侧一个叶子终端的展示元数据（从 workspace.json 扫出来）。
#[derive(Clone, Default)]
struct GuiLeafMeta {
    /// 会话级 custom_title（侧栏上那一行的名字，如 "services" / "claude-quant"）。
    session_title: Option<String>,
    /// 叶子级 custom_title 或 launch_label（嵌套时显示成子项，如 "frontend"）。
    pane_title: Option<String>,
    cwd: Option<String>,
    /// 同一 GUI 会话里有多个叶子 → 列表要嵌套在 session_title 下。
    multi_pane: bool,
    /// workspace.json 里的会话顺序，用来保持跟 PC 侧栏一致。
    session_ord: usize,
    leaf_ord: usize,
}

fn workspace_json_path() -> std::path::PathBuf {
    dirs::home_dir().unwrap_or_else(|| "/tmp".into()).join(".smelt").join("workspace.json")
}

/// cwd → 项目分组名：取目录末段（跟 workspace/main.rs 的 `project_name_for_cwd` 对齐）。
fn project_name_for_cwd(cwd: &str) -> String {
    cwd.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("项目")
        .to_string()
}

/// 递归扫 workspace.json 的 layout 树，把每个有 id 的叶子记下来。
fn collect_gui_leaves(
    pane: &serde_json::Value,
    session_title: Option<&str>,
    multi_pane: bool,
    session_ord: usize,
    leaf_counter: &mut usize,
    out: &mut std::collections::HashMap<String, GuiLeafMeta>,
) {
    if let Some(leaf) = pane.get("Leaf") {
        let id = match leaf.get("id").and_then(|v| v.as_str()) {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => return,
        };
        let pane_title = leaf
            .get("custom_title")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from)
            .or_else(|| {
                leaf.get("launch_label")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(String::from)
            });
        let cwd = leaf.get("cwd").and_then(|v| v.as_str()).map(String::from);
        let leaf_ord = *leaf_counter;
        *leaf_counter += 1;
        out.insert(
            id,
            GuiLeafMeta {
                session_title: session_title.map(String::from),
                pane_title,
                cwd,
                multi_pane,
                session_ord,
                leaf_ord,
            },
        );
        return;
    }
    if let Some(split) = pane.get("Split") {
        if let Some(children) = split.get("children").and_then(|c| c.as_array()) {
            for child in children {
                collect_gui_leaves(child, session_title, multi_pane, session_ord, leaf_counter, out);
            }
        }
    }
}

/// 读 `~/.smelt/workspace.json`，建 id → 展示元数据。读失败 / 文件不存在 → 空表，
/// 列表仍可用 smeltd 自带字段兜底（名称会差一些，但不崩）。
fn load_gui_leaf_meta() -> std::collections::HashMap<String, GuiLeafMeta> {
    let Ok(raw) = std::fs::read_to_string(workspace_json_path()) else {
        return std::collections::HashMap::new();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return std::collections::HashMap::new();
    };
    let mut out = std::collections::HashMap::new();
    let Some(sessions) = v.get("sessions").and_then(|s| s.as_array()) else {
        return out;
    };
    for (session_ord, sess) in sessions.iter().enumerate() {
        let session_title = sess
            .get("custom_title")
            .and_then(|t| t.as_str())
            .filter(|s| !s.is_empty());
        let Some(layout) = sess.get("layout") else { continue };
        // 先数这个会话有几个带 id 的叶子，决定要不要嵌套显示。
        let mut count_ids = 0usize;
        fn count_leaves(pane: &serde_json::Value, n: &mut usize) {
            if let Some(leaf) = pane.get("Leaf") {
                if leaf.get("id").and_then(|v| v.as_str()).is_some_and(|s| !s.is_empty()) {
                    *n += 1;
                }
            } else if let Some(children) =
                pane.get("Split").and_then(|s| s.get("children")).and_then(|c| c.as_array())
            {
                for c in children {
                    count_leaves(c, n);
                }
            }
        }
        count_leaves(layout, &mut count_ids);
        let multi_pane = count_ids > 1;
        let mut leaf_counter = 0usize;
        collect_gui_leaves(
            layout,
            session_title,
            multi_pane,
            session_ord,
            &mut leaf_counter,
            &mut out,
        );
    }
    out
}

/// 从 OSC 标题里剥掉 spinner / 状态前缀，只留人类可读的短名；剥空了就当没有。
fn clean_agent_title(raw: &str) -> Option<String> {
    let t = raw.trim();
    if t.is_empty() {
        return None;
    }
    // 常见 agent 标题：`✳ Claude Code` / spinner 前缀 / `… — working`
    let stripped = t
        .trim_start_matches(|c: char| {
            c.is_whitespace()
                || c == '✳'
                || c == '*'
                || ('\u{2800}'..='\u{28FF}').contains(&c) // braille spinners
        })
        .trim();
    let stripped = stripped
        .split(['—', '|', '·'])
        .next()
        .unwrap_or(stripped)
        .trim();
    if stripped.is_empty() || stripped.len() > 48 {
        None
    } else {
        Some(stripped.to_string())
    }
}

/// 给一条 smeltd 会话挑展示名：GUI 元数据 > launch > title > cwd 末段 > 短 id。
fn resolve_display_name(
    id: &str,
    cwd: Option<&str>,
    title: Option<&str>,
    launch: Option<&str>,
    gui: Option<&GuiLeafMeta>,
) -> (String, Option<String>, String) {
    let project = gui
        .and_then(|g| g.cwd.as_deref())
        .or(cwd)
        .map(project_name_for_cwd)
        .unwrap_or_else(|| "其他".to_string());

    let parent_session = gui.and_then(|g| {
        if g.multi_pane {
            g.session_title.clone()
        } else {
            None
        }
    });

    // 嵌套 pane：优先叶子名；否则用 session 名 + 序号感的短 id 太丑，用 pane/title。
    let name = if let Some(g) = gui {
        if g.multi_pane {
            g.pane_title
                .clone()
                .or_else(|| title.and_then(clean_agent_title))
                .or_else(|| launch.map(|l| l.to_string()))
                .unwrap_or_else(|| short_id(id))
        } else {
            g.session_title
                .clone()
                .or_else(|| g.pane_title.clone())
                .or_else(|| title.and_then(clean_agent_title))
                .or_else(|| launch.map(|l| l.to_string()))
                .or_else(|| cwd.map(project_name_for_cwd))
                .unwrap_or_else(|| short_id(id))
        }
    } else {
        title
            .and_then(clean_agent_title)
            .or_else(|| launch.map(|l| l.to_string()))
            .or_else(|| cwd.map(project_name_for_cwd))
            .unwrap_or_else(|| short_id(id))
    };

    (name, parent_session, project)
}

fn short_id(id: &str) -> String {
    let s: String = id.chars().take(8).collect();
    if s.is_empty() { "会话".into() } else { s }
}

/// 问 smeltd 要当前活会话列表 + 状态，再叠 workspace.json 的展示名。
/// 阻塞 IO，调用方需要丢进 `spawn_blocking`。
fn list_sessions_info() -> Vec<SessionInfo> {
    let Ok(conn) = UnixStream::connect(sock_path()) else { return Vec::new() };
    let Ok(mut writer) = conn.try_clone() else { return Vec::new() };
    if writeln!(writer, "{}", serde_json::json!({ "op": "list" })).is_err() {
        return Vec::new();
    }
    let mut reader = BufReader::new(conn);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return Vec::new();
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { return Vec::new() };
    let empty = Vec::new();
    let ids = v["sessions"].as_array().unwrap_or(&empty);
    let states = v["states"].as_array().unwrap_or(&empty);
    let gui = load_gui_leaf_meta();

    let mut infos: Vec<(SessionInfo, usize, usize)> = ids
        .iter()
        .zip(states.iter().map(Some).chain(std::iter::repeat(None)))
        .filter_map(|(id, state)| {
            let id = id.as_str()?.to_string();
            let phase = state.and_then(|s| s["phase"].as_str()).unwrap_or("idle").to_string();
            let pending_question =
                state.and_then(|s| s["pending_question"].as_str()).map(String::from);
            let cwd = state
                .and_then(|s| s["cwd"].as_str())
                .map(String::from)
                .or_else(|| gui.get(&id).and_then(|g| g.cwd.clone()));
            let title = state.and_then(|s| s["title"].as_str());
            let launch = state.and_then(|s| s["launch"].as_str());
            let g = gui.get(&id);
            let (name, parent_session, project) =
                resolve_display_name(&id, cwd.as_deref(), title, launch, g);
            let session_ord = g.map(|x| x.session_ord).unwrap_or(usize::MAX);
            let leaf_ord = g.map(|x| x.leaf_ord).unwrap_or(0);
            Some((
                SessionInfo {
                    id,
                    phase,
                    pending_question,
                    name,
                    project,
                    parent_session,
                    cwd,
                },
                session_ord,
                leaf_ord,
            ))
        })
        .collect();

    // 跟 PC 侧栏同一套：workspace 顺序优先，未入档的会话排在后面。
    infos.sort_by(|a, b| {
        a.1.cmp(&b.1)
            .then(a.2.cmp(&b.2))
            .then(a.0.project.cmp(&b.0.project))
            .then(a.0.name.cmp(&b.0.name))
    });
    infos.into_iter().map(|(info, _, _)| info).collect()
}

/// phase → (中文标签, 状态点颜色)，跟 remote_gateway_console_page.html 里 JS 那份
/// PHASE_LABEL 手动保持一致（一个是服务端渲染列表页用，一个是操作台页面
/// 实时刷新用，没法共用一份代码——不同语言）。
fn phase_label(phase: &str) -> (&'static str, &'static str) {
    match phase {
        "thinking" => ("思考中…", "#4a9eff"),
        "executing_tool" => ("执行工具中…", "#4a9eff"),
        "awaiting_approval" => ("等你批准", "#ef4444"),
        "waiting_for_user" => ("等你说话", "#f59e0b"),
        "dead" => ("已结束", "#666"),
        _ => ("空闲", "#666"),
    }
}

fn render_session_row(info: &SessionInfo, token: &str, nested: bool) -> String {
    let id = html_escape(&info.id);
    let token = html_escape(token);
    let name = html_escape(&info.name);
    let (label, color) = phase_label(&info.phase);
    let question = info
        .pending_question
        .as_deref()
        .map(|q| format!("<div class=\"question\">{}</div>", html_escape(q)))
        .unwrap_or_default();
    let nested_cls = if nested { " nested" } else { "" };
    format!(
        "<li class=\"session{nested_cls}\" data-phase=\"{phase}\">\
           <div class=\"row\">\
             <span class=\"dot\" style=\"background:{color}\"></span>\
             <a class=\"primary\" href=\"/s/{id}/console?token={token}\" title=\"{id}\">{name}</a>\
             <span class=\"label\">{label}</span>\
           </div>\
           {question}\
           <a class=\"secondary\" href=\"/s/{id}?token={token}\">完整终端 →</a>\
         </li>",
        phase = html_escape(&info.phase),
        nested_cls = nested_cls,
    )
}

/// 按「项目 →（可选）父会话 → 终端」渲染，形态对齐 PC 侧栏。
/// 组内顺序跟 `list_sessions_info` 一致（workspace.json 会话序），单 pane 与
/// 多 pane 组按首次出现交错，不会把所有单会话都堆到多 pane 前面。
fn render_session_list(infos: &[SessionInfo], token: &str) -> String {
    if infos.is_empty() {
        return LIST_PAGE.replace("__ROWS__", "<div class=\"empty\">目前没有活会话</div>");
    }

    let mut project_order: Vec<String> = Vec::new();
    for info in infos {
        if !project_order.iter().any(|p| p == &info.project) {
            project_order.push(info.project.clone());
        }
    }

    let mut html = String::new();
    for project in &project_order {
        let in_project: Vec<&SessionInfo> = infos.iter().filter(|i| &i.project == project).collect();
        html.push_str(&format!(
            "<section class=\"project\">\
               <div class=\"project-name\">📁 {}</div>\
               <ul class=\"session-list\">",
            html_escape(project)
        ));

        // 按 infos 顺序走一遍：遇到新 parent 开一组，遇到无 parent 直接出一行。
        let mut emitted_parents: Vec<String> = Vec::new();
        for info in &in_project {
            match &info.parent_session {
                None => {
                    html.push_str(&render_session_row(info, token, false));
                }
                Some(parent) => {
                    if emitted_parents.iter().any(|p| p == parent) {
                        continue; // 整组已经在首次遇到时画完
                    }
                    emitted_parents.push(parent.clone());
                    html.push_str(&format!(
                        "<li class=\"session-group\">\
                           <div class=\"group-name\">⊞ {}</div>\
                           <ul class=\"nested-list\">",
                        html_escape(parent)
                    ));
                    for child in &in_project {
                        if child.parent_session.as_deref() == Some(parent.as_str()) {
                            html.push_str(&render_session_row(child, token, true));
                        }
                    }
                    html.push_str("</ul></li>");
                }
            }
        }

        html.push_str("</ul></section>");
    }

    LIST_PAGE.replace("__ROWS__", &html)
}

async fn list_page_handler(Query(q): Query<AuthQuery>, State(state): State<AppState>) -> impl IntoResponse {
    if q.token != *state.token {
        return (StatusCode::FORBIDDEN, "token 不对").into_response();
    }
    let infos = tokio::task::spawn_blocking(list_sessions_info).await.unwrap_or_default();
    Html(render_session_list(&infos, &q.token)).into_response()
}

async fn sessions_json_handler(
    Query(q): Query<AuthQuery>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    if q.token != *state.token {
        return (StatusCode::FORBIDDEN, "token 不对").into_response();
    }
    let infos = tokio::task::spawn_blocking(list_sessions_info).await.unwrap_or_default();
    Json(serde_json::json!({ "sessions": infos })).into_response()
}

async fn page_handler(
    Path(id): Path<String>,
    Query(q): Query<AuthQuery>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    if q.token != *state.token {
        return (StatusCode::FORBIDDEN, "token 不对").into_response();
    }
    let page = REFERENCE_PAGE
        .replace("__ID_JSON__", &js_string_literal(&id))
        .replace("__TOKEN_JSON__", &js_string_literal(&q.token))
        .replace("__WRITE_ENABLED__", if state.write_enabled { "true" } else { "false" });
    Html(page).into_response()
}

async fn stream_handler(
    ws: WebSocketUpgrade,
    Path(id): Path<String>,
    Query(q): Query<AuthQuery>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    if q.token != *state.token {
        return (StatusCode::FORBIDDEN, "token 不对").into_response();
    }
    ws.on_upgrade(move |socket| pump_watch(socket, id)).into_response()
}

/// Phase 5+6：手机友好的"操作台"——大状态 + 问题文案，不嵌 xterm（roadmap 原则 3：
/// 「不绑死 xterm.js」）。`write_enabled` 决定页面要不要显示 approve/deny/reply
/// 按钮——纯布尔值，不是用户输入，直接拼字面量，不走 js_string_literal。
async fn console_handler(
    Path(id): Path<String>,
    Query(q): Query<AuthQuery>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    if q.token != *state.token {
        return (StatusCode::FORBIDDEN, "token 不对").into_response();
    }
    // 展示名从 workspace 元数据 / list 同源逻辑解析，避免操作台只显示丑陋 uuid。
    let id_for_meta = id.clone();
    let meta = tokio::task::spawn_blocking(move || {
        let gui = load_gui_leaf_meta();
        let infos = list_sessions_info();
        infos
            .into_iter()
            .find(|i| i.id == id_for_meta)
            .map(|i| (i.name, i.project, i.parent_session))
            .or_else(|| {
                let g = gui.get(&id_for_meta);
                let (name, parent, project) =
                    resolve_display_name(&id_for_meta, g.and_then(|x| x.cwd.as_deref()), None, None, g);
                Some((name, project, parent))
            })
            .unwrap_or_else(|| (short_id(&id_for_meta), "会话".into(), None))
    })
    .await
    .unwrap_or_else(|_| (short_id(&id), "会话".into(), None));

    let (name, project, parent) = meta;
    let subtitle = match parent {
        Some(p) if p != name => format!("{project} · {p}"),
        _ => project,
    };
    let page = CONSOLE_PAGE
        .replace("__ID_JSON__", &js_string_literal(&id))
        .replace("__TOKEN_JSON__", &js_string_literal(&q.token))
        .replace("__NAME_JSON__", &js_string_literal(&name))
        .replace("__SUBTITLE_JSON__", &js_string_literal(&subtitle))
        .replace("__WRITE_ENABLED__", if state.write_enabled { "true" } else { "false" });
    Html(page).into_response()
}

async fn state_stream_handler(
    ws: WebSocketUpgrade,
    Path(id): Path<String>,
    Query(q): Query<AuthQuery>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    if q.token != *state.token {
        return (StatusCode::FORBIDDEN, "token 不对").into_response();
    }
    ws.on_upgrade(move |socket| pump_state(socket, id)).into_response()
}

/// 操作台的状态流：连 smeltd 的 `subscribe`（全量订阅），按 id 过滤只转发这一个
/// 会话的变化。首帧快照里如果已经有这个 id，也转发一次，页面一打开就有内容，
/// 不用干等下一次状态变化。
async fn pump_state(mut socket: WebSocket, id: String) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<serde_json::Value>(16);
    let task = tokio::task::spawn_blocking(move || subscribe_and_forward(&id, tx));

    while let Some(state) = rx.recv().await {
        if socket.send(Message::Text(state.to_string().into())).await.is_err() {
            break;
        }
    }
    let _ = task.await;
    drop(socket);
}

/// 阻塞线程里跑：连 smeltd 的 subscribe，逐行解析，只把匹配这个 id 的状态塞进
/// channel——subscribe 本身是全量订阅（见 smeltd.rs 的 Subscribers），过滤是
/// 网关自己做的，不改 smeltd 协议。
fn subscribe_and_forward(id: &str, tx: tokio::sync::mpsc::Sender<serde_json::Value>) {
    let Ok(conn) = UnixStream::connect(sock_path()) else { return };
    let Ok(mut writer) = conn.try_clone() else { return };
    if writeln!(writer, "{}", serde_json::json!({ "op": "subscribe" })).is_err() {
        return;
    }
    let reader = BufReader::new(conn);
    for line in reader.lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
        if let Some(sessions) = v.get("sessions").and_then(|s| s.as_array()) {
            if let Some(state) =
                sessions.iter().find(|s| s.get("id").and_then(|i| i.as_str()) == Some(id))
            {
                if tx.blocking_send(state.clone()).is_err() {
                    return;
                }
            }
        } else if let Some(session) = v.get("session") {
            if session.get("id").and_then(|i| i.as_str()) == Some(id) {
                if tx.blocking_send(session.clone()).is_err() {
                    return;
                }
            }
        }
    }
}

/// Phase 6：approve/deny/reply，转发给 smeltd 的 `action` op（门闩/字节映射都在
/// 那边，见 smeltd.rs「远程操控」一节）。这里只多做一层网关自己的授权检查——
/// 这个 token 有没有写权限，跟 phase 门闩是两件独立的事：没写权限直接 403，
/// 不去问 smeltd；有写权限但 phase 不对，由 smeltd 用 `{"ok":false,"err":..}`
/// 正常回复，原样透传给客户端。
async fn action_handler(
    Path(id): Path<String>,
    Query(q): Query<AuthQuery>,
    State(state): State<AppState>,
    Json(body): Json<ActionBody>,
) -> impl IntoResponse {
    if q.token != *state.token {
        return (StatusCode::FORBIDDEN, "token 不对").into_response();
    }
    if !state.write_enabled {
        return (StatusCode::FORBIDDEN, "这条链接没有写权限").into_response();
    }
    let result = tokio::task::spawn_blocking(move || send_action(&id, &body.kind, body.text.as_deref()))
        .await
        .unwrap_or_else(|_| serde_json::json!({ "ok": false, "err": "内部错误" }));
    Json(result).into_response()
}

/// 阻塞：连 smeltd 发一次 `action` op，读一行回执。
fn read_smeltd_reply(conn: UnixStream) -> serde_json::Value {
    let mut line = String::new();
    match BufReader::new(conn).read_line(&mut line) {
        Ok(0) | Err(_) => {
            // 老版本 smeltd 不认识 op 时直接关连接、不回一行——以前会变成含糊的「响应解析失败」
            return serde_json::json!({
                "ok": false,
                "err": "守护没有响应（多半版本偏旧，请在 Mac 设置里「无缝升级」smeltd 后再试）"
            });
        }
        Ok(_) => {}
    }
    let line = line.trim();
    if line.is_empty() {
        return serde_json::json!({
            "ok": false,
            "err": "守护返回空响应（多半版本偏旧，请无缝升级 smeltd）"
        });
    }
    serde_json::from_str(line).unwrap_or_else(|_| {
        serde_json::json!({ "ok": false, "err": format!("守护响应无法解析：{}", &line[..line.len().min(80)]) })
    })
}

fn send_action(id: &str, kind: &str, text: Option<&str>) -> serde_json::Value {
    let Ok(mut conn) = UnixStream::connect(sock_path()) else {
        return serde_json::json!({ "ok": false, "err": "连不上守护" });
    };
    let req = serde_json::json!({ "op": "action", "id": id, "kind": kind, "text": text });
    if writeln!(conn, "{req}").is_err() {
        return serde_json::json!({ "ok": false, "err": "发送失败" });
    }
    read_smeltd_reply(conn)
}

/// Phase 6 补齐：原始键盘/粘贴。`write_enabled` 与 action 同一把锁；**不做** phase
/// 门闩（工作延续：Ctrl+C / TUI 导航 / agent 忙时补一句都必须能进）。门闩只留给
/// action 防误点批准。
async fn input_handler(
    Path(id): Path<String>,
    Query(q): Query<AuthQuery>,
    State(state): State<AppState>,
    Json(body): Json<InputBody>,
) -> impl IntoResponse {
    if q.token != *state.token {
        return (StatusCode::FORBIDDEN, "token 不对").into_response();
    }
    if !state.write_enabled {
        return (StatusCode::FORBIDDEN, "这条链接没有写权限").into_response();
    }
    if body.data.is_empty() {
        return Json(serde_json::json!({ "ok": false, "err": "需要非空 data" })).into_response();
    }
    let result = tokio::task::spawn_blocking(move || send_input(&id, &body.data))
        .await
        .unwrap_or_else(|_| serde_json::json!({ "ok": false, "err": "内部错误" }));
    Json(result).into_response()
}

fn send_input(id: &str, data: &str) -> serde_json::Value {
    let Ok(mut conn) = UnixStream::connect(sock_path()) else {
        return serde_json::json!({ "ok": false, "err": "连不上守护" });
    };
    let req = serde_json::json!({ "op": "input", "id": id, "data": data });
    if writeln!(conn, "{req}").is_err() {
        return serde_json::json!({ "ok": false, "err": "发送失败" });
    }
    read_smeltd_reply(conn)
}

/// 手机按视口改 PTY 尺寸。不要求 write_enabled——只读观战也需要正确排版，
/// 否则桌面大窗口镜像过来底部永远空一截（不是 xterm 画坏了）。
async fn resize_handler(
    Path(id): Path<String>,
    Query(q): Query<AuthQuery>,
    State(state): State<AppState>,
    Json(body): Json<ResizeBody>,
) -> impl IntoResponse {
    if q.token != *state.token {
        return (StatusCode::FORBIDDEN, "token 不对").into_response();
    }
    if body.cols == 0 || body.rows == 0 {
        return Json(serde_json::json!({ "ok": false, "err": "cols/rows 必须 > 0" })).into_response();
    }
    // 防离谱尺寸
    let cols = body.cols.min(300);
    let rows = body.rows.min(200);
    let result = tokio::task::spawn_blocking(move || {
        send_resize(&id, cols, rows, body.cell_w, body.cell_h)
    })
    .await
    .unwrap_or_else(|_| serde_json::json!({ "ok": false, "err": "内部错误" }));
    Json(result).into_response()
}

fn send_resize(id: &str, cols: u16, rows: u16, cell_w: u16, cell_h: u16) -> serde_json::Value {
    let Ok(mut conn) = UnixStream::connect(sock_path()) else {
        return serde_json::json!({ "ok": false, "err": "连不上守护" });
    };
    let req = serde_json::json!({
        "op": "resize",
        "id": id,
        "cols": cols,
        "rows": rows,
        "cell_w": cell_w,
        "cell_h": cell_h,
    });
    if writeln!(conn, "{req}").is_err() {
        return serde_json::json!({ "ok": false, "err": "发送失败" });
    }
    read_smeltd_reply(conn)
}

/// 从阻塞的 smeltd watch 连接搬到这条 WS 上的一帧：Header 只在开头发一次，
/// 后面全是 Bytes——顺序必须保持（客户端先按 cols/rows 定尺寸，再写快照）。
enum Frame {
    Header { cols: u16, rows: u16 },
    Bytes(Vec<u8>),
}

/// 连 smeltd.sock 的只读 watch，把字节流转成 WS 二进制消息推给浏览器。
/// stream WS 本身只读画面；可写走独立的 `POST …/input`（见 input_handler），不混在这条
/// WS 上——避免和 fan-out 的只读旁观语义缠在一起。
async fn pump_watch(mut socket: WebSocket, id: String) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Frame>(64);
    // smeltd 那端是阻塞 IO，丢进阻塞线程池，不占用 tokio 的 async 执行器。
    let task = tokio::task::spawn_blocking(move || watch_and_forward(&id, tx));

    while let Some(frame) = rx.recv().await {
        let msg = match frame {
            Frame::Header { cols, rows } => {
                Message::Text(serde_json::json!({ "cols": cols, "rows": rows }).to_string().into())
            }
            Frame::Bytes(b) => Message::Binary(b.into()),
        };
        if socket.send(msg).await.is_err() {
            break;
        }
    }
    let _ = task.await;
    drop(socket); // WS 连接随 drop 关闭，不需要显式 close 帧
}

/// 阻塞线程里跑：连 smeltd、发 watch、读 header、snapshot、后续实时字节，
/// 都塞进 channel 交给上面那个 async 循环转发。
fn watch_and_forward(id: &str, tx: tokio::sync::mpsc::Sender<Frame>) {
    let Ok(conn) = UnixStream::connect(sock_path()) else { return };
    let Ok(mut writer) = conn.try_clone() else { return };
    if writeln!(writer, "{}", serde_json::json!({ "op": "watch", "id": id })).is_err() {
        return;
    }
    let mut reader = BufReader::new(conn);

    let mut line = String::new();
    if reader.read_line(&mut line).is_err() || line.is_empty() {
        return; // 会话不存在：smeltd 直接关连接，什么都不发（见 handle_watch）
    }
    let Ok(header) = serde_json::from_str::<serde_json::Value>(&line) else { return };
    let cols = header["cols"].as_u64().unwrap_or(80) as u16;
    let rows = header["rows"].as_u64().unwrap_or(24) as u16;
    let replay_len = header["replay_len"].as_u64().unwrap_or(0) as usize;

    if tx.blocking_send(Frame::Header { cols, rows }).is_err() {
        return;
    }

    if replay_len > 0 {
        let mut snap = vec![0u8; replay_len];
        if reader.read_exact(&mut snap).is_err() {
            return;
        }
        if tx.blocking_send(Frame::Bytes(snap)).is_err() {
            return;
        }
    }

    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if tx.blocking_send(Frame::Bytes(buf[..n].to_vec())).is_err() {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 反射型 XSS 的核心防线：id/token 里带 `</script>` 不能提前把内联脚本切断。
    #[test]
    fn js_string_literal_escapes_script_breakout() {
        let evil = "</script><script>alert(1)</script>";
        let escaped = js_string_literal(evil);
        assert!(!escaped.contains("</script>"), "转义后仍含裸露的 </script>：{escaped}");
        assert!(escaped.contains("\\u003c"), "尖括号应被转成 \\u003c：{escaped}");
    }

    #[test]
    fn js_string_literal_escapes_quotes_and_backslashes() {
        let evil = "\"; alert(1); //\\";
        let escaped = js_string_literal(evil);
        // 必须是一个合法的、被双引号包住的 JS 字符串字面量。
        assert!(escaped.starts_with('"') && escaped.ends_with('"'));
        // 反序列化回来应该精确等于原字符串（转义没丢信息、没被破坏）。
        let roundtrip: String = serde_json::from_str(&escaped).unwrap();
        assert_eq!(roundtrip, evil);
    }

    /// 会话列表页把 id 嵌进 HTML 正文/属性——防的是 HTML 注入，不是 JS 字符串逃逸，
    /// 转义规则跟 js_string_literal 不一样，得单独测。
    #[test]
    fn html_escape_neutralizes_tag_breakout() {
        let evil = "<img src=x onerror=alert(1)>";
        let escaped = html_escape(evil);
        assert!(!escaped.contains('<') && !escaped.contains('>'), "尖括号应被转义：{escaped}");
    }

    #[test]
    fn render_session_list_escapes_ids_and_handles_empty() {
        let empty = render_session_list(&[], "tok");
        assert!(empty.contains("没有活会话"));

        let evil = SessionInfo {
            id: "<script>alert(1)</script>".to_string(),
            phase: "idle".to_string(),
            pending_question: Some("<b>问题</b>".to_string()),
            name: "<img onerror=1>".to_string(),
            project: "proj<script>".to_string(),
            parent_session: None,
            cwd: None,
        };
        let page = render_session_list(&[evil], "tok");
        assert!(!page.contains("<script>alert(1)</script>"), "未转义的 id 混进了列表页：{page}");
        assert!(!page.contains("<img onerror=1>"), "未转义的 name 混进了列表页：{page}");
        assert!(page.contains("&lt;img"), "转义后的 name 应该出现在列表里：{page}");
        assert!(!page.contains("<b>问题</b>"), "未转义的 pending_question 混进了列表页：{page}");
        // 列表主标题是 name，不是裸 uuid
        assert!(page.contains("primary"), "应有主链接：{page}");
    }

    #[test]
    fn project_name_for_cwd_takes_last_segment() {
        assert_eq!(project_name_for_cwd("/Users/x/Desktop/my/smelt"), "smelt");
        assert_eq!(project_name_for_cwd("/tmp/"), "tmp");
        assert_eq!(project_name_for_cwd(""), "项目");
    }

    #[test]
    fn resolve_display_name_prefers_gui_session_title() {
        let gui = GuiLeafMeta {
            session_title: Some("claude-quant".into()),
            pane_title: None,
            cwd: Some("/p/quant-above-all".into()),
            multi_pane: false,
            session_ord: 0,
            leaf_ord: 0,
        };
        let (name, parent, project) =
            resolve_display_name("uuid-here", Some("/p/quant-above-all"), None, None, Some(&gui));
        assert_eq!(name, "claude-quant");
        assert!(parent.is_none());
        assert_eq!(project, "quant-above-all");
    }

    #[test]
    fn resolve_display_name_nests_multi_pane_under_session() {
        let gui = GuiLeafMeta {
            session_title: Some("services".into()),
            pane_title: Some("frontend".into()),
            cwd: Some("/p/quant-above-all".into()),
            multi_pane: true,
            session_ord: 0,
            leaf_ord: 0,
        };
        let (name, parent, _) =
            resolve_display_name("uuid", Some("/p/quant-above-all"), None, None, Some(&gui));
        assert_eq!(name, "frontend");
        assert_eq!(parent.as_deref(), Some("services"));
    }

    #[test]
    fn render_session_list_groups_by_project_and_shows_names() {
        let infos = vec![
            SessionInfo {
                id: "id-a".into(),
                phase: "idle".into(),
                pending_question: None,
                name: "claude-quant".into(),
                project: "quant-above-all".into(),
                parent_session: None,
                cwd: None,
            },
            SessionInfo {
                id: "id-b".into(),
                phase: "idle".into(),
                pending_question: None,
                name: "frontend".into(),
                project: "quant-above-all".into(),
                parent_session: Some("services".into()),
                cwd: None,
            },
            SessionInfo {
                id: "id-c".into(),
                phase: "waiting_for_user".into(),
                pending_question: Some("继续吗？".into()),
                name: "grok".into(),
                project: "smelt".into(),
                parent_session: None,
                cwd: None,
            },
        ];
        let page = render_session_list(&infos, "tok");
        assert!(page.contains("quant-above-all"), "应有项目组：{page}");
        assert!(page.contains("claude-quant"), "应显示会话名：{page}");
        assert!(page.contains("services"), "应有多 pane 父会话：{page}");
        assert!(page.contains("frontend"), "应有嵌套 pane 名：{page}");
        assert!(page.contains("grok"), "应有另一项目会话：{page}");
        assert!(!page.contains(">id-a<"), "主链接不该是裸 id：{page}");
    }

    /// 未知 phase（比如以后 smeltd 加了新枚举值，网关还没更新）不该 panic，
    /// 退化成一个能看的默认值。
    #[test]
    fn phase_label_falls_back_on_unknown_phase() {
        let (label, _color) = phase_label("some_future_phase_we_dont_know_yet");
        assert!(!label.is_empty());
    }

    #[test]
    fn phase_label_covers_all_known_phases() {
        for phase in [
            "thinking",
            "executing_tool",
            "awaiting_approval",
            "waiting_for_user",
            "idle",
            "dead",
        ] {
            let (label, color) = phase_label(phase);
            assert!(!label.is_empty() && color.starts_with('#'), "phase={phase}");
        }
    }
}
