//! smelt-signal：WebRTC 信令服务。
//!
//! 只做房间 + SDP/ICE 转发，**不碰 PTY**。协议见 `docs/webrtc-edge.md`
//! 与 `remote-web/src/transport/types.ts`。
//!
//! ## 环境变量
//! - `SMELT_SIGNAL_BIND` — 默认 `127.0.0.1:7878`
//! - `SMELT_ROOM_TTL_SECS` — 房间默认存活秒数，默认 `3600`
//! - `SMELT_ICE_SERVERS` — JSON 数组，形如
//!   `[{"urls":"stun:stun.l.google.com:19302"},{"urls":"turn:...","username":"u","credential":"p"}]`
//!   缺省仅 Google 公共 STUN（本地 dev）
//!
//! ## HTTP
//! - `GET  /health` → `{ ok, rooms }`
//! - `POST /v1/rooms` → `{ room, secret, expires_at, ttl_secs, signal_ws }`
//! - `GET  /ws` → WebSocket 信令

mod protocol;
mod state;
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
    info!(
        ice_count = ice_servers.len(),
        ttl_secs,
        %bind,
        "smelt-signal starting"
    );

    let state = AppState::new(ice_servers, Duration::from_secs(ttl_secs));

    let app = Router::new()
        .route("/health", get(health))
        .route("/v1/rooms", post(create_room))
        .route("/ws", get(ws::ws_upgrade))
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
    vec![IceServerConfig {
        urls: IceUrls::One("stun:stun.l.google.com:19302".into()),
        username: None,
        credential: None,
    }]
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
    let created = state.create_room(ttl.map(Duration::from_secs));
    Ok(Json(CreateRoomResp {
        room: created.room,
        secret: created.secret,
        expires_at: created.expires_at,
        ttl_secs: created.ttl_secs,
        signal_ws: "/ws",
    }))
}
