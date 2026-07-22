//! 建房 + WebSocket 信令（host）。

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, info, warn};

use crate::rtc;
use crate::Config;

#[derive(Debug, Deserialize)]
pub struct RoomCreated {
    pub room: String,
    pub secret: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IceServerJson {
    pub urls: IceUrls,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub credential: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum IceUrls {
    One(String),
    Many(Vec<String>),
}

pub async fn create_room(signal_http: &str) -> Result<RoomCreated> {
    let url = format!("{signal_http}/v1/rooms");
    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("create room HTTP {status}: {body}");
    }
    Ok(resp.json().await?)
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum WireIn {
    HelloOk {
        ice_servers: Vec<IceServerJson>,
    },
    PeerJoined {
        role: String,
    },
    PeerLeft {
        role: String,
    },
    Signal {
        from: String,
        payload: Value,
    },
    Err {
        msg: String,
    },
    Ping,
    Pong,
}

pub async fn run_host(cfg: Arc<Config>, room: String, secret: String) -> Result<()> {
    let (ws, _) = connect_async(&cfg.signal_ws)
        .await
        .with_context(|| format!("connect {}", cfg.signal_ws))?;
    let (mut sink, mut stream) = ws.split();

    let hello = serde_json::json!({
        "op": "hello",
        "role": "host",
        "room": room,
        "secret": secret,
    });
    sink.send(Message::Text(hello.to_string().into()))
        .await
        .context("send hello")?;

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        while let Some(text) = out_rx.recv().await {
            if sink.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });

    let mut ice_servers: Vec<IceServerJson> = Vec::new();
    let mut peer: Option<rtc::HostPeer> = None;

    while let Some(msg) = stream.next().await {
        let msg = msg.context("ws read")?;
        let text = match msg {
            Message::Text(t) => t.to_string(),
            Message::Ping(_) => continue,
            Message::Close(_) => break,
            _ => continue,
        };
        let parsed: WireIn = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                warn!(%e, "bad signal json");
                continue;
            }
        };
        match parsed {
            WireIn::HelloOk { ice_servers: ice } => {
                ice_servers = ice;
                info!(n = ice_servers.len(), "hello_ok");
            }
            WireIn::PeerJoined { role } => {
                info!(%role, "peer_joined");
                if role == "client" {
                    // 手机重连：必须新 PC，不能复用上一轮
                    peer = recreate_peer(
                        peer,
                        cfg.clone(),
                        ice_servers.clone(),
                        out_tx.clone(),
                        "peer_joined",
                    )
                    .await?;
                }
            }
            WireIn::PeerLeft { role } => {
                info!(%role, "peer_left");
                if role == "client" {
                    if let Some(old) = peer.take() {
                        old.close().await;
                    }
                    info!("cleared host peer after client left");
                }
            }
            WireIn::Signal { from, payload } => {
                if from != "client" {
                    continue;
                }
                let kind = payload
                    .get("kind")
                    .and_then(|k| k.as_str())
                    .unwrap_or("")
                    .to_string();
                let is_restart = payload
                    .get("restart")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                // 新 offer：默认永远新建 PeerConnection（WebRTC 单轮协商）；
                // 但 restart=true（网络抖动后客户端原地 ICE restart）要在同一个
                // pc/DataChannel 上重协商，不能重建——否则正在用的会话直接断线，
                // 跟只是想续上连接的初衷矛盾。
                if kind == "offer" && !(is_restart && peer.is_some()) {
                    peer = recreate_peer(
                        peer,
                        cfg.clone(),
                        ice_servers.clone(),
                        out_tx.clone(),
                        "offer",
                    )
                    .await?;
                } else if peer.is_none() {
                    peer = Some(
                        rtc::HostPeer::new(cfg.clone(), ice_servers.clone(), out_tx.clone())
                            .await
                            .context("create host peer on signal")?,
                    );
                }
                if let Some(p) = peer.as_mut() {
                    if let Err(e) = p.handle_signal(payload).await {
                        warn!(%e, "handle signal");
                        // offer 失败则丢弃 PC，等下次
                        if kind == "offer" {
                            if let Some(old) = peer.take() {
                                old.close().await;
                            }
                        }
                    }
                }
            }
            WireIn::Err { msg } => {
                warn!(%msg, "signal err");
                // replaced 等可忽略
                if msg.contains("replaced") {
                    continue;
                }
                bail!("signaling error: {msg}");
            }
            WireIn::Ping => {
                let _ = out_tx.send(r#"{"op":"pong"}"#.into());
            }
            WireIn::Pong => debug!("pong"),
        }
    }

    if let Some(old) = peer.take() {
        old.close().await;
    }
    drop(out_tx);
    let _ = writer.await;
    info!("signaling closed");
    Ok(())
}

async fn recreate_peer(
    old: Option<rtc::HostPeer>,
    cfg: Arc<Config>,
    ice: Vec<IceServerJson>,
    out_tx: mpsc::UnboundedSender<String>,
    reason: &str,
) -> Result<Option<rtc::HostPeer>> {
    if let Some(p) = old {
        info!(%reason, "closing previous host peer for reconnect");
        p.close().await;
    }
    let p = rtc::HostPeer::new(cfg, ice, out_tx)
        .await
        .with_context(|| format!("create host peer ({reason})"))?;
    info!(%reason, "new host peer ready");
    Ok(Some(p))
}
