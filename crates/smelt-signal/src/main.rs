//! smelt-signal：WebRTC 信令服务。
//!
//! 只做房间 + SDP/ICE 转发，**不碰 PTY**。协议见 `docs/webrtc-edge.md`
//! 与 `remote-web/src/transport/types.ts`。
//!
//! ## 环境变量
//! - `SMELT_SIGNAL_BIND` — 默认 `127.0.0.1:7878`
//! - `SMELT_ROOM_TTL_SECS` — 房间默认存活秒数，默认 `3600`
//! - `SMELT_ICE_SERVERS` — JSON 数组，形如
//!   `[{"urls":"stun:stun.qq.com:3478"},{"urls":"turn:turn.example.com:3478","username":"u","credential":"p"}]`
//!   缺省：腾讯 / 小米 / Cloudflare / Google 公共 STUN；纯 STUN 场景够用
//! - `SMELT_TURN_SECRET` / `SMELT_TURN_HOST` — 配了才会现算临时 TURN 凭证
//!   （coturn REST API / `use-auth-secret` 模式）追加进下发的 ice_servers；
//!   `SMELT_TURN_SECRET` 必须跟 `turnserver.conf` 的 `static-auth-secret=`
//!   完全一致，`SMELT_TURN_HOST` 是 `host:port`（如 `signal.example.com:3478`）。
//!   比静态长期凭证（旧式 `SMELT_ICE_SERVERS` 里直接写 TURN username/credential）
//!   更安全：凭证短时效自动过期，且两边永远不会分叉，见 `deploy/signal/coturn.md`
//! - `SMELT_TURN_TTL_SECS` — 临时 TURN 凭证有效期，默认跟 `SMELT_ROOM_TTL_SECS`
//!   一致（避免长会话中途 ICE restart 时凭证已过期）
//!
//! ## HTTP
//! - `GET  /health` → `{ ok, rooms }`
//! - `POST /v1/rooms` → `{ room, secret, expires_at, ttl_secs, signal_ws }`
//! - `GET  /ws` → WebSocket 信令
//! - `GET  /` `/s/*` `/assets/*` → remote-web SPA（跨网手机页）

mod protocol;
mod spa;
mod state;
mod turn_rest;
mod ws;

use std::env;
use std::net::SocketAddr;
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use protocol::{IceServerConfig, IceUrls};
use state::AppState;
use turn_rest::TurnRestConfig;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let bind: SocketAddr = env::var("SMELT_SIGNAL_BIND")
        .unwrap_or_else(|_| "127.0.0.1:7878".into())
        .parse()
        .expect("SMELT_SIGNAL_BIND must be host:port");

    let ttl_secs: u64 = env::var("SMELT_ROOM_TTL_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3600);

    let ice_servers = load_ice_servers();
    let turn_rest = load_turn_rest_config(ttl_secs);
    info!(
        ice_count = ice_servers.len(),
        turn_rest = turn_rest.is_some(),
        ttl_secs,
        %bind,
        "smelt-signal starting"
    );

    let state = AppState::new(ice_servers, turn_rest, Duration::from_secs(ttl_secs));
    let spa_ok = spa::spa_ready();
    info!(spa_embedded = spa_ok, "smelt-signal routes");

    // API / WS 优先；其余同域托管 SPA（nginx 仍可整站反代到本进程）
    let mut app = Router::new()
        .route("/health", get(health))
        .route("/v1/rooms", post(create_room))
        .route("/ws", get(ws::ws_upgrade));
    if spa_ok {
        app = app
            .route("/", get(spa::spa_index))
            .route("/s/{*rest}", get(spa::spa_index))
            .route("/assets/{*path}", get(spa::spa_asset));
    }
    let app = app
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .unwrap_or_else(|e| panic!("bind {bind}: {e}"));
    info!(%bind, "listening");
    axum::serve(listener, app)
        .await
        .expect("server error");
}

fn load_ice_servers() -> Vec<IceServerConfig> {
    if let Ok(raw) = env::var("SMELT_ICE_SERVERS") {
        match serde_json::from_str::<Vec<IceServerConfig>>(&raw) {
            Ok(list) if !list.is_empty() => return list,
            Ok(_) => warn!("SMELT_ICE_SERVERS is empty array, using default STUN"),
            Err(e) => warn!(%e, "SMELT_ICE_SERVERS parse failed, using default STUN"),
        }
    }
    // 多源公共 STUN：国内优先 + 全球兜底。生产应挂 coturn TURN（SMELT_ICE_SERVERS）。
    // 顺序：腾讯 → 小米 → Cloudflare（免费无限 STUN）→ Google。
    default_public_stun_servers()
}

/// `SMELT_TURN_SECRET` 没配就返回 None——纯 STUN 或旧式静态 TURN
/// （`SMELT_ICE_SERVERS` 里直接写死 username/credential）照常工作，这个是
/// 可选的升级路径，不强制迁移。
fn load_turn_rest_config(room_ttl_secs: u64) -> Option<TurnRestConfig> {
    let secret = env::var("SMELT_TURN_SECRET").ok().filter(|s| !s.is_empty())?;
    let host = match env::var("SMELT_TURN_HOST").ok().filter(|s| !s.is_empty()) {
        Some(h) => h,
        None => {
            warn!("SMELT_TURN_SECRET 配了但没配 SMELT_TURN_HOST，跳过临时 TURN 凭证");
            return None;
        }
    };
    // 默认对齐 room TTL：只要 room 没过期，凭证也不会先过期，长会话中途 ICE
    // restart 不用担心凭证过期被拒。
    let ttl_secs: u64 = env::var("SMELT_TURN_TTL_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(room_ttl_secs);
    Some(TurnRestConfig {
        secret,
        host,
        ttl: Duration::from_secs(ttl_secs),
    })
}

/// 与 SPA / bridge 回退列表保持一致。
fn default_public_stun_servers() -> Vec<IceServerConfig> {
    const URLS: &[&str] = &[
        "stun:stun.qq.com:3478",
        "stun:stun.miwifi.com:3478",
        "stun:stun.cloudflare.com:3478",
        "stun:stun.l.google.com:19302",
    ];
    URLS.iter()
        .map(|u| IceServerConfig {
            urls: IceUrls::One((*u).into()),
            username: None,
            credential: None,
        })
        .collect()
}

#[derive(Serialize)]
struct HealthResp {
    ok: bool,
    rooms: usize,
}

async fn health(State(state): State<AppState>) -> Json<HealthResp> {
    Json(HealthResp {
        ok: true,
        rooms: state.room_count(),
    })
}

#[derive(Deserialize)]
struct CreateRoomReq {
    /// 可选覆盖默认 TTL（秒），上限 24h
    #[serde(default)]
    ttl_secs: Option<u64>,
}

#[derive(Serialize)]
struct CreateRoomResp {
    room: String,
    secret: String,
    expires_at: u64,
    ttl_secs: u64,
    /// 相对路径；客户端拼 host 成 ws(s)://...
    signal_ws: &'static str,
}

async fn create_room(
    State(state): State<AppState>,
    body: Option<Json<CreateRoomReq>>,
) -> Result<Json<CreateRoomResp>, (StatusCode, Json<serde_json::Value>)> {
    let ttl = body
        .and_then(|b| b.ttl_secs)
        .map(|s| s.min(86_400).max(60));
    let created = state
        .create_room(ttl.map(Duration::from_secs))
        .map_err(|e| {
            let code = StatusCode::from_u16(e.status()).unwrap_or(StatusCode::SERVICE_UNAVAILABLE);
            (
                code,
                Json(serde_json::json!({ "error": e.msg() })),
            )
        })?;
    Ok(Json(CreateRoomResp {
        room: created.room,
        secret: created.secret,
        expires_at: created.expires_at,
        ttl_secs: created.ttl_secs,
        signal_ws: "/ws",
    }))
}
