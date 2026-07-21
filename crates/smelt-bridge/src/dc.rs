//! DataChannel 业务帧 ↔ 本机 gateway。
//!
//! 每个 DC 连接一份 [`DcSession`]：必须先 hello 鉴权，否则拒绝业务帧。

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use base64::Engine;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex;
use tracing::{info, warn};
use webrtc::data_channel::RTCDataChannel;

use crate::gateway::{self, PtyFrame};
use crate::Config;

/// 单条 DataChannel 上的连接态（鉴权 + 任务句柄）。
pub struct DcSession {
    pub authed: bool,
    pub write: bool,
    /// session_id → PTY watch task
    watches: HashMap<String, tokio::task::JoinHandle<()>>,
    /// session_id → state-stream task
    state_watches: HashMap<String, tokio::task::JoinHandle<()>>,
}

impl DcSession {
    pub fn new() -> Self {
        Self {
            authed: false,
            write: false,
            watches: HashMap::new(),
            state_watches: HashMap::new(),
        }
    }

    pub fn abort_all(&mut self) {
        for (_, h) in self.watches.drain() {
            h.abort();
        }
        for (_, h) in self.state_watches.drain() {
            h.abort();
        }
    }
}

impl Default for DcSession {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Deserialize)]
struct Frame {
    t: String,
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    write: Option<bool>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    data: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    cols: Option<u16>,
    #[serde(default)]
    rows: Option<u16>,
    #[serde(default)]
    cell_w: Option<u16>,
    #[serde(default)]
    cell_h: Option<u16>,
}

pub async fn handle_frame(
    cfg: Arc<Config>,
    dc: Arc<RTCDataChannel>,
    sess: Arc<Mutex<DcSession>>,
    raw: &str,
) -> Result<()> {
    let f: Frame = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => {
            send_json(
                &dc,
                &serde_json::json!({ "t": "err", "msg": "bad frame json" }),
            )
            .await;
            return Ok(());
        }
    };

    // 未 hello 只允许 hello
    if f.t != "hello" {
        let ok = sess.lock().await.authed;
        if !ok {
            send_json(
                &dc,
                &serde_json::json!({
                    "t": "err",
                    "msg": "not authenticated; send hello first",
                    "code": "auth"
                }),
            )
            .await;
            return Ok(());
        }
    }

    match f.t.as_str() {
        "hello" => {
            let tok = f.token.unwrap_or_default();
            if tok != cfg.gateway_token {
                send_json(
                    &dc,
                    &serde_json::json!({ "t": "err", "msg": "token mismatch", "code": "auth" }),
                )
                .await;
                return Ok(());
            }
            let write = cfg.write && f.write.unwrap_or(true);
            {
                let mut s = sess.lock().await;
                s.authed = true;
                s.write = write;
            }
            send_json(
                &dc,
                &serde_json::json!({ "t": "hello_ok", "write": write }),
            )
            .await;
            info!(write, "dc hello_ok");
        }
        "sessions" => match gateway::fetch_sessions(&cfg).await {
            Ok(body) => {
                let sessions = body.get("sessions").cloned().unwrap_or(Value::Array(vec![]));
                send_json(
                    &dc,
                    &serde_json::json!({ "t": "sessions_ok", "sessions": sessions }),
                )
                .await;
            }
            Err(e) => {
                send_json(
                    &dc,
                    &serde_json::json!({ "t": "err", "msg": format!("sessions: {e}") }),
                )
                .await;
            }
        },
        "open" => {
            let Some(id) = f.id.clone() else {
                send_json(&dc, &serde_json::json!({ "t": "err", "msg": "open needs id" })).await;
                return Ok(());
            };
            // 停旧 PTY + state watch
            {
                let mut s = sess.lock().await;
                if let Some(h) = s.watches.remove(&id) {
                    h.abort();
                }
                if let Some(h) = s.state_watches.remove(&id) {
                    h.abort();
                }
            }
            send_json(&dc, &serde_json::json!({ "t": "open_ok", "id": id })).await;

            // PTY 字节流
            let cfg_w = cfg.clone();
            let dc_w = Arc::clone(&dc);
            let id_w = id.clone();
            let handle = tokio::spawn(async move {
                let r = gateway::watch_pty(&cfg_w, &id_w, |frame| {
                    let dc = Arc::clone(&dc_w);
                    let id = id_w.clone();
                    async move {
                        match frame {
                            PtyFrame::Header { cols, rows } => {
                                let _ = (cols, rows);
                            }
                            PtyFrame::Bytes(b) => {
                                let data = base64::engine::general_purpose::STANDARD.encode(&b);
                                send_json(
                                    &dc,
                                    &serde_json::json!({ "t": "pty", "id": id, "data": data }),
                                )
                                .await;
                            }
                        }
                    }
                })
                .await;
                if let Err(e) = r {
                    warn!(%e, id = %id_w, "watch ended");
                    send_json(
                        &dc_w,
                        &serde_json::json!({ "t": "err", "msg": format!("watch: {e}") }),
                    )
                    .await;
                }
            });
            sess.lock().await.watches.insert(id.clone(), handle);

            // 状态流 → state 帧（phase / pending_question）
            let cfg_s = cfg.clone();
            let dc_s = Arc::clone(&dc);
            let id_s = id.clone();
            let state_h = tokio::spawn(async move {
                let r = gateway::watch_state(&cfg_s, &id_s, |v| {
                    let dc = Arc::clone(&dc_s);
                    let id = id_s.clone();
                    async move {
                        let phase = v.get("phase").cloned().unwrap_or(Value::Null);
                        let pending = v.get("pending_question").cloned().unwrap_or(Value::Null);
                        send_json(
                            &dc,
                            &serde_json::json!({
                                "t": "state",
                                "id": id,
                                "phase": phase,
                                "pending_question": pending,
                            }),
                        )
                        .await;
                    }
                })
                .await;
                if let Err(e) = r {
                    warn!(%e, id = %id_s, "state watch ended");
                }
            });
            sess.lock().await.state_watches.insert(id, state_h);
        }
        "close" => {
            if let Some(id) = f.id {
                let mut s = sess.lock().await;
                if let Some(h) = s.watches.remove(&id) {
                    h.abort();
                }
                if let Some(h) = s.state_watches.remove(&id) {
                    h.abort();
                }
            }
        }
        "menu" => {
            let Some(id) = f.id else {
                send_json(&dc, &serde_json::json!({ "t": "err", "msg": "menu needs id" })).await;
                return Ok(());
            };
            match gateway::fetch_menu(&cfg, &id).await {
                Ok(menu) => {
                    send_json(
                        &dc,
                        &serde_json::json!({ "t": "menu_ok", "id": id, "menu": menu }),
                    )
                    .await;
                }
                Err(e) => {
                    send_json(
                        &dc,
                        &serde_json::json!({ "t": "err", "msg": format!("menu: {e}") }),
                    )
                    .await;
                }
            }
        }
        "input" => {
            if !sess.lock().await.write {
                send_json(
                    &dc,
                    &serde_json::json!({ "t": "err", "msg": "read-only", "code": "readonly" }),
                )
                .await;
                return Ok(());
            }
            let (Some(id), Some(data)) = (f.id, f.data) else {
                return Ok(());
            };
            let path = format!("/s/{id}/input");
            match gateway::post_json(&cfg, &path, serde_json::json!({ "data": data })).await {
                Ok(_) => {
                    send_json(
                        &dc,
                        &serde_json::json!({ "t": "ack", "op": "input", "id": id, "ok": true }),
                    )
                    .await;
                }
                Err(e) => {
                    warn!(%e, "input");
                    send_json(
                        &dc,
                        &serde_json::json!({ "t": "ack", "op": "input", "id": id, "ok": false, "err": e.to_string() }),
                    )
                    .await;
                }
            }
        }
        "action" => {
            if !sess.lock().await.write {
                send_json(
                    &dc,
                    &serde_json::json!({ "t": "err", "msg": "read-only", "code": "readonly" }),
                )
                .await;
                return Ok(());
            }
            let (Some(id), Some(kind)) = (f.id, f.kind) else {
                return Ok(());
            };
            let path = format!("/s/{id}/action");
            let body = serde_json::json!({ "kind": kind, "text": f.text });
            match gateway::post_json(&cfg, &path, body).await {
                Ok(_) => {
                    send_json(
                        &dc,
                        &serde_json::json!({ "t": "ack", "op": "action", "id": id, "ok": true }),
                    )
                    .await;
                }
                Err(e) => {
                    warn!(%e, "action");
                    send_json(
                        &dc,
                        &serde_json::json!({ "t": "ack", "op": "action", "id": id, "ok": false, "err": e.to_string() }),
                    )
                    .await;
                }
            }
        }
        "resize" => {
            // 与 input/action 一致：只读连接不可改共享 PTY 尺寸
            if !sess.lock().await.write {
                send_json(
                    &dc,
                    &serde_json::json!({ "t": "err", "msg": "read-only", "code": "readonly" }),
                )
                .await;
                return Ok(());
            }
            let (Some(id), Some(cols), Some(rows)) = (f.id, f.cols, f.rows) else {
                return Ok(());
            };
            let path = format!("/s/{id}/resize");
            let body = serde_json::json!({
                "cols": cols,
                "rows": rows,
                "cell_w": f.cell_w.unwrap_or(0),
                "cell_h": f.cell_h.unwrap_or(0),
            });
            if let Err(e) = gateway::post_json(&cfg, &path, body).await {
                warn!(%e, "resize");
            }
        }
        other => {
            warn!(%other, "unknown frame t");
        }
    }
    Ok(())
}

/// DC 关闭时清理该连接上所有 watch 任务。
pub async fn on_dc_closed(sess: Arc<Mutex<DcSession>>) {
    sess.lock().await.abort_all();
    info!("dc closed, watches aborted");
}

pub async fn send_json(dc: &RTCDataChannel, v: &Value) {
    let s = v.to_string();
    if let Err(e) = dc.send_text(s).await {
        warn!(%e, "dc send");
    }
}
