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
                if role == "client" && peer.is_none() {
                    peer = Some(
                        rtc::HostPeer::new(cfg.clone(), ice_servers.clone(), out_tx.clone())
                            .await
                            .context("create host peer")?,
                    );
                }
            }
            WireIn::Signal { from, payload } => {
                if from != "client" {
                    continue;
                }
                if peer.is_none() {
                    peer = Some(
                        rtc::HostPeer::new(cfg.clone(), ice_servers.clone(), out_tx.clone())
                            .await
                            .context("create host peer on signal")?,
                    );
                }
                if let Some(p) = peer.as_mut() {
                    if let Err(e) = p.handle_signal(payload).await {
                        warn!(%e, "handle signal");
                    }
                }
            }
            WireIn::Err { msg } => {
                warn!(%msg, "signal err");
                bail!("signaling error: {msg}");
            }
            WireIn::Ping => {
                let _ = out_tx.send(r#"{"op":"pong"}"#.into());
            }
            WireIn::Pong => debug!("pong"),
        }
    }

    drop(out_tx);
    let _ = writer.await;
    info!("signaling closed");
    Ok(())
}
