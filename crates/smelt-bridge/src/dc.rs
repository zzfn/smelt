//! DataChannel 业务帧 ↔ 本机 gateway。

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

/// 每个 DC 连接一份：已 open 的 session watch 任务
type WatchMap = Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>>;

fn watches() -> WatchMap {
    use std::sync::OnceLock;
    static W: OnceLock<WatchMap> = OnceLock::new();
    W.get_or_init(|| Arc::new(Mutex::new(HashMap::new())))
        .clone()
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

pub async fn handle_frame(cfg: Arc<Config>, dc: Arc<RTCDataChannel>, raw: &str) -> Result<()> {
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
            // 停旧 watch
            {
                let w = watches();
                let mut map = w.lock().await;
                if let Some(h) = map.remove(&id) {
                    h.abort();
                }
            }
            send_json(&dc, &serde_json::json!({ "t": "open_ok", "id": id })).await;

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
                                // 用 state 附带尺寸可选；帧协议无 header，先忽略或 err 旁路
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
            watches().lock().await.insert(id, handle);
        }
        "close" => {
            if let Some(id) = f.id {
                if let Some(h) = watches().lock().await.remove(&id) {
                    h.abort();
                }
            }
        }
        "input" => {
            if !cfg.write {
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
            if let Err(e) = gateway::post_json(&cfg, &path, serde_json::json!({ "data": data })).await
            {
                warn!(%e, "input");
            }
        }
        "action" => {
            if !cfg.write {
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
            if let Err(e) = gateway::post_json(&cfg, &path, body).await {
                warn!(%e, "action");
            }
        }
        "resize" => {
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

async fn send_json(dc: &RTCDataChannel, v: &Value) {
    let s = v.to_string();
    if let Err(e) = dc.send_text(s).await {
        warn!(%e, "dc send");
    }
}
