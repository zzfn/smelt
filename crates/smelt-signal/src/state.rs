//! 房间状态：内存 map，短时效；信令不存 PTY。

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use rand::RngCore;
use tokio::sync::mpsc;
use tracing::{debug, info};

use crate::protocol::{IceServerConfig, Role, ServerMsg};

pub type Outbound = mpsc::UnboundedSender<String>;

#[derive(Clone)]
pub struct AppState {
    pub rooms: Arc<DashMap<String, Room>>,
    pub ice_servers: Arc<Vec<IceServerConfig>>,
    pub default_ttl: Duration,
}

pub struct Room {
    pub secret: String,
    pub expires_at: Instant,
    pub host: Option<PeerSlot>,
    pub client: Option<PeerSlot>,
}

pub struct PeerSlot {
    pub tx: Outbound,
}

impl AppState {
    pub fn new(ice_servers: Vec<IceServerConfig>, default_ttl: Duration) -> Self {
        Self {
            rooms: Arc::new(DashMap::new()),
            ice_servers: Arc::new(ice_servers),
            default_ttl,
        }
    }

    pub fn create_room(&self, ttl: Option<Duration>) -> CreateRoomResult {
        self.gc_expired();
        let ttl = ttl.unwrap_or(self.default_ttl);
        let room_id = gen_room_id();
        let secret = gen_secret();
        let now = Instant::now();
        let created_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let expires_unix = created_unix + ttl.as_secs();

        self.rooms.insert(
            room_id.clone(),
            Room {
                secret: secret.clone(),
                expires_at: now + ttl,
                host: None,
                client: None,
            },
        );
        info!(room = %room_id, ttl_secs = ttl.as_secs(), "room created");
        CreateRoomResult {
            room: room_id,
            secret,
            expires_at: expires_unix,
            ttl_secs: ttl.as_secs(),
        }
    }

    /// 校验 secret 并注册 peer；成功返回是否已有对端在线。
    pub fn join(
        &self,
        room_id: &str,
        secret: &str,
        role: Role,
        tx: Outbound,
    ) -> Result<JoinOk, JoinErr> {
        self.gc_expired();
        let mut entry = self
            .rooms
            .get_mut(room_id)
            .ok_or(JoinErr::NotFound)?;

        if Instant::now() >= entry.expires_at {
            drop(entry);
            self.rooms.remove(room_id);
            return Err(JoinErr::Expired);
        }
        if entry.secret != secret {
            return Err(JoinErr::BadSecret);
        }

        let slot = match role {
            Role::Host => &mut entry.host,
            Role::Client => &mut entry.client,
        };
        if slot.is_some() {
            return Err(JoinErr::RoleTaken);
        }
        *slot = Some(PeerSlot { tx });

        let peer_online = match role.other() {
            Role::Host => entry.host.is_some(),
            Role::Client => entry.client.is_some(),
        };
        debug!(room = %room_id, role = role.as_str(), peer_online, "peer joined");
        Ok(JoinOk { peer_online })
    }

    /// 把消息投递给房间内另一角色。
    pub fn relay_to_other(&self, room_id: &str, from: Role, msg: &ServerMsg) {
        let Some(entry) = self.rooms.get(room_id) else {
            return;
        };
        let target = match from.other() {
            Role::Host => entry.host.as_ref(),
            Role::Client => entry.client.as_ref(),
        };
        if let Some(peer) = target {
            let json = msg.to_json();
            let _ = peer.tx.send(json);
        }
    }

    pub fn leave(&self, room_id: &str, role: Role) {
        let Some(mut entry) = self.rooms.get_mut(room_id) else {
            return;
        };
        match role {
            Role::Host => entry.host = None,
            Role::Client => entry.client = None,
        };
        let empty = entry.host.is_none() && entry.client.is_none();
        drop(entry);
        if empty {
            // 房间保留到 TTL，便于断线重连；不立刻删
            debug!(room = %room_id, role = role.as_str(), "peer left (room kept until ttl)");
        } else {
            debug!(room = %room_id, role = role.as_str(), "peer left");
        }
    }

    pub fn room_count(&self) -> usize {
        self.gc_expired();
        self.rooms.len()
    }

    fn gc_expired(&self) {
        let now = Instant::now();
        self.rooms.retain(|id, room| {
            let keep = now < room.expires_at;
            if !keep {
                info!(room = %id, "room expired, removed");
            }
            keep
        });
    }
}

pub struct CreateRoomResult {
    pub room: String,
    pub secret: String,
    pub expires_at: u64,
    pub ttl_secs: u64,
}

pub struct JoinOk {
    pub peer_online: bool,
}

#[derive(Debug)]
pub enum JoinErr {
    NotFound,
    Expired,
    BadSecret,
    RoleTaken,
}

impl JoinErr {
    pub fn msg(&self) -> &'static str {
        match self {
            JoinErr::NotFound => "room not found",
            JoinErr::Expired => "room expired",
            JoinErr::BadSecret => "invalid secret",
            JoinErr::RoleTaken => "role already connected",
        }
    }
}

/// 12 字符短 id，便于 URL
fn gen_room_id() -> String {
    const ALPH: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::thread_rng();
    let mut buf = [0u8; 12];
    rng.fill_bytes(&mut buf);
    buf.iter()
        .map(|b| ALPH[(*b as usize) % ALPH.len()] as char)
        .collect()
}

/// 24 字节 → base64url（无 padding），高熵 secret
fn gen_secret() -> String {
    use base64::Engine;
    let mut buf = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut buf);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}
