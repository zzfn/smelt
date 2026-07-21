//! webrtc-rs host：收 offer、回 answer、收 DataChannel。
//! 每次新 offer / 对端重连都新建 PeerConnection（不可复用已协商的 PC）。

use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::sync::mpsc;
use tracing::{info, warn};
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;

use crate::dc;
use crate::signal::{IceServerJson, IceUrls};
use crate::Config;

pub struct HostPeer {
    pc: Arc<RTCPeerConnection>,
    out_tx: mpsc::UnboundedSender<String>,
    remote_set: bool,
    pending_ice: Vec<RTCIceCandidateInit>,
    /// 已完成过一轮 offer/answer，再来 offer 必须新建 PC
    negotiated: bool,
}

impl HostPeer {
    pub async fn new(
        cfg: Arc<Config>,
        ice_servers: Vec<IceServerJson>,
        out_tx: mpsc::UnboundedSender<String>,
    ) -> Result<Self> {
        let mut m = MediaEngine::default();
        m.register_default_codecs()
            .context("register_default_codecs")?;
        let mut registry = Registry::new();
        registry = register_default_interceptors(registry, &mut m)
            .context("register_default_interceptors")?;
        let api = APIBuilder::new()
            .with_media_engine(m)
            .with_interceptor_registry(registry)
            .build();

        let ice: Vec<RTCIceServer> = ice_servers
            .into_iter()
            .map(|s| RTCIceServer {
                urls: match s.urls {
                    IceUrls::One(u) => vec![u],
                    IceUrls::Many(v) => v,
                },
                username: s.username.unwrap_or_default(),
                credential: s.credential.unwrap_or_default(),
                ..Default::default()
            })
            .collect();

        let config = RTCConfiguration {
            ice_servers: if ice.is_empty() {
                vec![RTCIceServer {
                    urls: vec!["stun:stun.l.google.com:19302".into()],
                    ..Default::default()
                }]
            } else {
                ice
            },
            ..Default::default()
        };

        let pc = Arc::new(
            api.new_peer_connection(config)
                .await
                .context("new_peer_connection")?,
        );

        {
            let out = out_tx.clone();
            pc.on_ice_candidate(Box::new(move |c| {
                let out = out.clone();
                Box::pin(async move {
                    let payload = if let Some(c) = c {
                        match c.to_json() {
                            Ok(init) => serde_json::json!({
                                "kind": "ice",
                                "candidate": {
                                    "candidate": init.candidate,
                                    "sdpMid": init.sdp_mid,
                                    "sdpMLineIndex": init.sdp_mline_index,
                                    "usernameFragment": init.username_fragment,
                                }
                            }),
                            Err(e) => {
                                warn!(%e, "ice to_json");
                                return;
                            }
                        }
                    } else {
                        serde_json::json!({ "kind": "ice", "candidate": null })
                    };
                    let msg = serde_json::json!({
                        "op": "signal",
                        "from": "host",
                        "payload": payload,
                    });
                    let _ = out.send(msg.to_string());
                })
            }));
        }

        {
            let cfg = cfg.clone();
            pc.on_data_channel(Box::new(move |d: Arc<RTCDataChannel>| {
                let cfg = cfg.clone();
                Box::pin(async move {
                    if d.label() != "smelt" {
                        warn!(label = %d.label(), "ignore dc");
                        return;
                    }
                    info!("datachannel smelt");
                    wire_dc(cfg, d);
                })
            }));
        }

        pc.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
            info!(?s, "pc state");
            Box::pin(async {})
        }));

        Ok(Self {
            pc,
            out_tx,
            remote_set: false,
            pending_ice: Vec::new(),
            negotiated: false,
        })
    }

    pub fn needs_fresh_pc_for_offer(&self) -> bool {
        self.negotiated || self.remote_set
    }

    pub async fn close(self) {
        info!("closing host peer connection");
        let _ = self.pc.close().await;
    }

    pub async fn handle_signal(&mut self, payload: Value) -> Result<()> {
        let kind = payload
            .get("kind")
            .and_then(|k| k.as_str())
            .unwrap_or("");
        match kind {
            "offer" => {
                if self.needs_fresh_pc_for_offer() {
                    // 调用方应先 close 再建新 PC；这里仅防御
                    bail_offer_reuse()?;
                }
                let sdp = payload
                    .get("sdp")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let offer = RTCSessionDescription::offer(sdp).context("parse offer")?;
                self.pc
                    .set_remote_description(offer)
                    .await
                    .context("set_remote offer")?;
                self.remote_set = true;
                self.flush_ice().await?;

                let answer = self
                    .pc
                    .create_answer(None)
                    .await
                    .context("create_answer")?;
                let mut gather_complete = self.pc.gathering_complete_promise().await;
                self.pc
                    .set_local_description(answer)
                    .await
                    .context("set_local answer")?;
                let _ = gather_complete.recv().await;

                let local = self
                    .pc
                    .local_description()
                    .await
                    .context("local_description")?;
                let msg = serde_json::json!({
                    "op": "signal",
                    "from": "host",
                    "payload": {
                        "kind": "answer",
                        "sdp": local.sdp,
                    }
                });
                let _ = self.out_tx.send(msg.to_string());
                self.negotiated = true;
                info!("sent answer");
            }
            "answer" => {
                warn!("host unexpected answer");
            }
            "ice" => {
                if let Some(c) = payload.get("candidate") {
                    if c.is_null() {
                        return Ok(());
                    }
                    let init = RTCIceCandidateInit {
                        candidate: c
                            .get("candidate")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        sdp_mid: c
                            .get("sdpMid")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string()),
                        sdp_mline_index: c
                            .get("sdpMLineIndex")
                            .and_then(|v| v.as_u64())
                            .map(|n| n as u16),
                        username_fragment: c
                            .get("usernameFragment")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string()),
                    };
                    if self.remote_set {
                        if let Err(e) = self.pc.add_ice_candidate(init).await {
                            warn!(%e, "add_ice");
                        }
                    } else {
                        self.pending_ice.push(init);
                    }
                }
            }
            _ => warn!(%kind, "unknown signal kind"),
        }
        Ok(())
    }

    async fn flush_ice(&mut self) -> Result<()> {
        for init in self.pending_ice.drain(..) {
            if let Err(e) = self.pc.add_ice_candidate(init).await {
                warn!(%e, "flush ice");
            }
        }
        Ok(())
    }
}

fn bail_offer_reuse() -> Result<()> {
    Err(anyhow::anyhow!("peer already negotiated; need new HostPeer"))
}

fn wire_dc(cfg: Arc<Config>, d: Arc<RTCDataChannel>) {
    let cfg2 = cfg.clone();
    let d2 = Arc::clone(&d);
    let sess = Arc::new(tokio::sync::Mutex::new(dc::DcSession::new()));
    let sess_msg = Arc::clone(&sess);
    let sess_close = Arc::clone(&sess);

    d.on_open(Box::new(move || {
        info!("dc open");
        Box::pin(async {})
    }));

    d.on_message(Box::new(move |msg: DataChannelMessage| {
        let cfg = cfg2.clone();
        let d = Arc::clone(&d2);
        let sess = Arc::clone(&sess_msg);
        Box::pin(async move {
            let text = String::from_utf8_lossy(&msg.data).to_string();
            if let Err(e) = dc::handle_frame(cfg, d, sess, &text).await {
                warn!(%e, "dc frame");
            }
        })
    }));

    d.on_close(Box::new(move || {
        let sess = Arc::clone(&sess_close);
        Box::pin(async move {
            dc::on_dc_closed(sess).await;
        })
    }));
}
