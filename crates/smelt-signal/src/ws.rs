//! WebSocket `/ws`：hello / hello_ok / peer_joined / signal / ping-pong。

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tracing::{debug, warn};

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
                warn!(%e, "bad signaling json");
                send_json(&out_tx, ServerMsg::err("bad json"));
                continue;
            }
        };

        match parsed {
            ClientMsg::Hello { role, room, secret } => {
                if joined.is_some() {
                    send_json(&out_tx, ServerMsg::err("already joined"));
                    continue;
                }
                match state.join(&room, &secret, role, out_tx.clone()) {
                    Ok(ok) => {
                        joined = Some((room.clone(), role));
                        send_json(
                            &out_tx,
                            ServerMsg::HelloOk {
                                ice_servers: (*state.ice_servers).clone(),
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
                        debug!(room = %room, role = role.as_str(), "hello ok");
                    }
                    Err(e) => {
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
                state.relay_to_other(
                    room,
                    role,
                    &ServerMsg::Signal { from, payload },
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
        debug!(room = %room, role = role.as_str(), "ws closed, left room");
    }
    drop(out_tx);
    drop(ctrl_tx);
    let _ = writer.await;
}
