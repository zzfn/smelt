//! 房间状态：内存 map，短时效；信令不存 PTY。

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use rand::RngCore;
use tokio::sync::mpsc;
use tracing::{debug, info};

use crate::protocol::{IceServerConfig, IceUrls, Role, ServerMsg};
use crate::turn_rest::{self, TurnRestConfig};

pub type Outbound = mpsc::UnboundedSender<String>;

/// 全局房间上限，防止 POST /v1/rooms 刷爆内存
const MAX_ROOMS: usize = 2_000;
/// 全进程每分钟最多新建房间数
const MAX_CREATE_PER_MINUTE: u32 = 120;

#[derive(Clone)]
pub struct AppState {
    pub rooms: Arc<DashMap<String, Room>>,
    /// 静态 ICE 列表（公共 STUN，也可以是没配 SMELT_TURN_SECRET 时的旧式静态
    /// TURN）。配了 turn_rest 时，每次 hello_ok 会在这份静态列表之外再现算
    /// 追加一条临时 TURN。
    pub ice_servers: Arc<Vec<IceServerConfig>>,
    pub turn_rest: Option<Arc<TurnRestConfig>>,
    pub default_ttl: Duration,
    create_window: Arc<std::sync::Mutex<(Instant, u32)>>,
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
    pub fn new(
        ice_servers: Vec<IceServerConfig>,
        turn_rest: Option<TurnRestConfig>,
        default_ttl: Duration,
    ) -> Self {
        Self {
            rooms: Arc::new(DashMap::new()),
            ice_servers: Arc::new(ice_servers),
            turn_rest: turn_rest.map(Arc::new),
            default_ttl,
            create_window: Arc::new(std::sync::Mutex::new((Instant::now(), 0))),
        }
    }

    /// 每次 hello_ok 调用一次：静态列表 + （如果配了）现算的临时 TURN 凭证。
    /// 现算而不是缓存，是因为凭证本身带过期时间戳，缓存了就失去意义。
    pub fn ice_servers_for_hello(&self) -> Vec<IceServerConfig> {
        let mut list = (*self.ice_servers).clone();
        if let Some(cfg) = &self.turn_rest {
            let cred = turn_rest::mint(cfg);
            list.push(IceServerConfig {
                urls: IceUrls::Many(vec![
                    format!("turn:{}?transport=udp", cfg.host),
                    format!("turn:{}?transport=tcp", cfg.host),
                ]),
                username: Some(cred.username),
                credential: Some(cred.credential),
            });
        }
        list
    }

    pub fn create_room(&self, ttl: Option<Duration>) -> Result<CreateRoomResult, CreateRoomErr> {
        self.gc_expired();
        if self.rooms.len() >= MAX_ROOMS {
            return Err(CreateRoomErr::TooManyRooms);
        }
        {
            let mut w = self
                .create_window
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let now = Instant::now();
            if now.duration_since(w.0) > Duration::from_secs(60) {
                *w = (now, 0);
            }
            if w.1 >= MAX_CREATE_PER_MINUTE {
                return Err(CreateRoomErr::RateLimited);
            }
            w.1 += 1;
        }
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
        Ok(CreateRoomResult {
            room: room_id,
            secret,
            expires_at: expires_unix,
            ttl_secs: ttl.as_secs(),
        })
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
        // 同角色重复连（手机杀进程/二次打开）：踢掉旧连接，避免 RoleTaken 卡死
        if let Some(old) = slot.take() {
            let _ = old
                .tx
                .send(crate::protocol::ServerMsg::err("replaced by new connection").to_json());
            info!(room = %room_id, role = role.as_str(), "replaced stale peer for role");
        }
        *slot = Some(PeerSlot { tx });

        let peer_online = match role.other() {
            Role::Host => entry.host.is_some(),
            Role::Client => entry.client.is_some(),
        };
        debug!(room = %room_id, role = role.as_str(), peer_online, "peer joined");
        Ok(JoinOk { peer_online })
    }

    /// 把消息投递给房间内另一角色。返回 false 表示对端不在线（房间不存在也算），
    /// 调用方可以据此判断是不是"消息发出去了但没人收"这种排查线索。
    pub fn relay_to_other(&self, room_id: &str, from: Role, msg: &ServerMsg) -> bool {
        let Some(entry) = self.rooms.get(room_id) else {
            return false;
        };
        let target = match from.other() {
            Role::Host => entry.host.as_ref(),
            Role::Client => entry.client.as_ref(),
        };
        let Some(peer) = target else {
            return false;
        };
        let json = msg.to_json();
        peer.tx.send(json).is_ok()
    }

    pub fn leave(&self, room_id: &str, role: Role) {
        let other_tx = {
            let Some(mut entry) = self.rooms.get_mut(room_id) else {
                return;
            };
            let slot = match role {
                Role::Host => &mut entry.host,
                Role::Client => &mut entry.client,
            };
            *slot = None;
            // 通知对端拆 RTC
            let other = match role.other() {
                Role::Host => entry.host.as_ref(),
                Role::Client => entry.client.as_ref(),
            };
            other.map(|p| p.tx.clone())
        };
        if let Some(tx) = other_tx {
            let _ = tx.send(
                crate::protocol::ServerMsg::PeerLeft { role }.to_json(),
            );
        }
        debug!(room = %room_id, role = role.as_str(), "peer left");
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

#[derive(Debug)]
pub enum CreateRoomErr {
    TooManyRooms,
    RateLimited,
}

impl CreateRoomErr {
    pub fn msg(&self) -> &'static str {
        match self {
            CreateRoomErr::TooManyRooms => "too many rooms",
            CreateRoomErr::RateLimited => "create room rate limited",
        }
    }

    pub fn status(&self) -> u16 {
        match self {
            CreateRoomErr::TooManyRooms => 503,
            CreateRoomErr::RateLimited => 429,
        }
    }
}

pub struct JoinOk {
    pub peer_online: bool,
}

#[derive(Debug)]
pub enum JoinErr {
    NotFound,
    Expired,
    BadSecret,
}

impl JoinErr {
    pub fn msg(&self) -> &'static str {
        match self {
            JoinErr::NotFound => "room not found",
            JoinErr::Expired => "room expired",
            JoinErr::BadSecret => "invalid secret",
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
