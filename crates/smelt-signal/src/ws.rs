//! WebSocket `/ws`：hello / hello_ok / peer_joined / signal / ping-pong。

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::protocol::{ClientMsg, Role, ServerMsg};
use crate::state::{AppState, Outbound};

pub async fn ws_upgrade(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

fn send_json(tx: &Outbound, msg: ServerMsg) {
    let _ = tx.send(msg.to_json());
}

async fn handle_socket(socket: WebSocket, state: AppState) {
    let (mut sink, mut stream) = socket.split();
    // 房间 relay 与本连接共用同一条出站 channel（JSON text）
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    // WS 控制帧（Pong）单独通道，避免污染 JSON 协议
    let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded_channel::<Message>();

    let writer = tokio::spawn(async move {
        loop {
            tokio::select! {
                text = out_rx.recv() => {
                    let Some(text) = text else { break; };
                    if sink.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                ctrl = ctrl_rx.recv() => {
                    let Some(msg) = ctrl else { break; };
                    if sink.send(msg).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    let mut joined: Option<(String, Role)> = None;

    while let Some(Ok(msg)) = stream.next().await {
        let text = match msg {
            Message::Text(t) => t.to_string(),
            Message::Ping(p) => {
                let _ = ctrl_tx.send(Message::Pong(p));
                continue;
            }
            Message::Close(_) => break,
            Message::Binary(_) | Message::Pong(_) => continue,
        };

        let parsed: ClientMsg = match serde_json::from_str(&text) {
            Ok(m) => m,
            Err(e) => {
                // text 里可能带 secret，不整条打；len 足够定位是不是被截断/编码坏了
                warn!(%e, len = text.len(), "bad signaling json");
                send_json(&out_tx, ServerMsg::err("bad json"));
                continue;
            }
        };

        match parsed {
            ClientMsg::Hello { role, room, secret } => {
                if joined.is_some() {
                    warn!(room = %room, role = role.as_str(), "hello rejected: already joined on this connection");
                    send_json(&out_tx, ServerMsg::err("already joined"));
                    continue;
                }
                match state.join(&room, &secret, role, out_tx.clone()) {
                    Ok(ok) => {
                        joined = Some((room.clone(), role));
                        send_json(
                            &out_tx,
                            ServerMsg::HelloOk {
                                ice_servers: state.ice_servers_for_hello(),
                            },
                        );
                        if ok.peer_online {
                            // 告诉新来的：对端已在
                            send_json(
                                &out_tx,
                                ServerMsg::PeerJoined {
                                    role: role.other(),
                                },
                            );
                            // 告诉对端：新角色上线
                            state.relay_to_other(
                                &room,
                                role,
                                &ServerMsg::PeerJoined { role },
                            );
                        }
                        // 特意用 info：这是排查"某个用户连不上"最关键的一条——出问题时
                        // 先看这条有没有出现，没出现说明 hello 压根没到，问题在更早的
                        // 网络/DNS/证书层；出现了但后面没有 signal 转发，问题在双方
                        // WebRTC 协商本身。
                        info!(room = %room, role = role.as_str(), peer_online = ok.peer_online, "hello ok");
                    }
                    Err(e) => {
                        // 同样特意用 info：这条直接说明是"密钥不对/房间过期/房间不存在"
                        // 里的哪一种，比让用户口头描述"连不上"准得多。
                        info!(room = %room, role = role.as_str(), reason = e.msg(), "hello rejected");
                        send_json(&out_tx, ServerMsg::err(e.msg()));
                    }
                }
            }
            ClientMsg::Signal { from, payload } => {
                let Some((ref room, role)) = joined else {
                    send_json(&out_tx, ServerMsg::err("not joined"));
                    continue;
                };
                if from != role {
                    send_json(&out_tx, ServerMsg::err("from role mismatch"));
                    continue;
                }
                let kind = payload.get("kind").and_then(|k| k.as_str()).unwrap_or("");
                if !state.relay_to_other(room, role, &ServerMsg::Signal { from, payload: payload.clone() }) {
                    // 对端不在线：offer/answer/ice 全部有去无回，客户端会一直等，
                    // 表现就是"卡在正在建立跨网连接"。这条能直接告诉你是不是这个原因。
                    info!(room = %room, role = role.as_str(), kind, "signal relay dropped: peer not online");
                }
            }
            ClientMsg::RefreshIce => {
                if joined.is_none() {
                    send_json(&out_tx, ServerMsg::err("not joined"));
                    continue;
                }
                send_json(
                    &out_tx,
                    ServerMsg::IceServers {
                        ice_servers: state.ice_servers_for_hello(),
                    },
                );
            }
            ClientMsg::Ping => {
                send_json(&out_tx, ServerMsg::Pong);
            }
            ClientMsg::Pong => {
                // ignore
            }
        }
    }

    if let Some((room, role)) = joined {
        state.leave(&room, role);
        info!(room = %room, role = role.as_str(), "ws closed, left room");
    }
    drop(out_tx);
    drop(ctrl_tx);
    let _ = writer.await;
}
