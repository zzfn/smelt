//! 信令线协议，与 `remote-web/src/transport/types.ts` / docs/webrtc-edge.md 对齐。

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Client,
    Host,
}

impl Role {
    pub fn other(self) -> Self {
        match self {
            Role::Client => Role::Host,
            Role::Host => Role::Client,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Client => "client",
            Role::Host => "host",
        }
    }
}

/// ICE server 配置（下发给浏览器 / bridge 的 RTCPeerConnection）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IceServerConfig {
    pub urls: IceUrls,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum IceUrls {
    One(String),
    Many(Vec<String>),
}

/// 客户端 → 服务端 / 服务端 → 客户端 统一 envelope
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ClientMsg {
    Hello {
        role: Role,
        room: String,
        secret: String,
    },
    Signal {
        from: Role,
        payload: Value,
    },
    Ping,
    /// 浏览器可能回 pong；服务端也可发 ping
    Pong,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ServerMsg {
    HelloOk {
        ice_servers: Vec<IceServerConfig>,
    },
    PeerJoined {
        role: Role,
    },
    Signal {
        from: Role,
        payload: Value,
    },
    Err {
        msg: String,
    },
    Ping,
    Pong,
}

impl ServerMsg {
    pub fn err(msg: impl Into<String>) -> Self {
        ServerMsg::Err { msg: msg.into() }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| {
            r#"{"op":"err","msg":"encode failed"}"#.to_string()
        })
    }
}
