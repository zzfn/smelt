//! coturn REST API 临时 TURN 凭证（`use-auth-secret` 模式）。
//!
//! 现在这套（改之前）是静态长期凭证：`turnserver.conf` 的 `user=` 和
//! `smelt-signal` 发给客户端的 `SMELT_ICE_SERVERS` 里的 credential 必须靠人肉
//! 保持一致——手动改一边、脚本半路失败、重复执行漏参数，都会让两边悄悄分叉，
//! 而且分叉之后两个服务都能正常启动、没有任何报错，只有真正要走 TURN 中继的
//! 用户会连不上，是那种"过一阵子才被人发现"的静默失败（这个坑真的踩过一次）。
//!
//! 这个模块换了个做法：`smelt-signal` 和 `coturn` 只共享**一份密钥**
//! （`static-auth-secret`），从不下发给客户端。每次 `hello_ok` 现算一个短时效
//! 的用户名/密码：
//!   username  = 过期时间戳（unix 秒）
//!   credential = base64(HMAC-SHA1(密钥, username))
//! coturn 收到 TURN 请求时用同一份密钥重新算一遍来验证，不用查任何用户表。
//! 好处：两边永远不会分叉（各自独立算，没有"手动同步"这个动作要做）；泄露的
//! 凭证会自动过期，不再是"泄露了能一直白嫖"。
//!
//! TTL 别设太短：ICE restart（网络抖动后原地重连）复用的是建连那一刻拿到的
//! iceServers，不会重新走一遍 hello 拿新凭证；TTL 短于会话实际时长的话，
//! restart 途中重新申请 TURN 会因为凭证已过期被拒。默认对齐 room 的默认寿命
//! （见 `SMELT_ROOM_TTL_SECS`），只要 room 没过期，凭证也不会先过期。

use base64::Engine;
use hmac::{Hmac, Mac};
use sha1::Sha1;

type HmacSha1 = Hmac<Sha1>;

#[derive(Clone)]
pub struct TurnRestConfig {
    /// 跟 turnserver.conf 里 `static-auth-secret=` 完全一致的那份密钥。
    pub secret: String,
    /// 直接拼进 `turn:` URL 的 host:port，如 `signal.example.com:3478`。
    pub host: String,
    /// 凭证有效期；建议 >= room 默认 TTL，避免长会话中途 ICE restart 时凭证
    /// 已经过期。
    pub ttl: std::time::Duration,
}

pub struct EphemeralTurnCred {
    pub username: String,
    pub credential: String,
}

/// 现算一份临时 TURN 凭证。每次调用时间戳都不同，天然不会跟别的调用撞。
pub fn mint(cfg: &TurnRestConfig) -> EphemeralTurnCred {
    let expiry = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        + cfg.ttl.as_secs();
    let username = expiry.to_string();

    // 密钥可以是任意长度，HMAC 本身接受任意 key size，这里不会失败。
    let mut mac = HmacSha1::new_from_slice(cfg.secret.as_bytes()).expect("hmac accepts any key size");
    mac.update(username.as_bytes());
    let sig = mac.finalize().into_bytes();
    let credential = base64::engine::general_purpose::STANDARD.encode(sig);

    EphemeralTurnCred { username, credential }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 跟 `printf '%s' "$USERNAME" | openssl dgst -sha1 -hmac "$SECRET" -binary |
    /// openssl base64`（以及独立用 Python hmac 模块）算出来的值对比，防止
    /// HMAC/base64 编码这种细节写错——写错了不会报错，就是 coturn 验证永远
    /// 失败，跟这次踩的静态凭证分叉一样是静默失败。
    #[test]
    fn matches_openssl_hmac_sha1() {
        let mut mac = HmacSha1::new_from_slice(b"testsecret").unwrap();
        mac.update(b"1784800000");
        let sig = mac.finalize().into_bytes();
        let credential = base64::engine::general_purpose::STANDARD.encode(sig);
        assert_eq!(credential, "PRCwArFvdkH132gQx5vlCpB/1FA=");
    }
}
