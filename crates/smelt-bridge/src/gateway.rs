//! 本机 remote_gateway HTTP/WS 客户端。

use std::sync::OnceLock;

use anyhow::{bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::warn;

use crate::Config;

/// 共享 client，且显式关掉系统代理。
///
/// gateway_base 永远是 127.0.0.1 回环地址，没有任何理由走代理；但 reqwest
/// 默认会读系统代理设置（macOS 上经 scutil），如果代理软件（Stash/Clash 类）
/// 的直连例外表里只写了 "localhost" 这个主机名、没写 127.0.0.1/8 这个网段，
/// 发往 127.0.0.1 的请求就会被那条代理接管——代理再回环连自己这一跳一旦不稳
/// 定，表现为间歇性 502 Bad Gateway，实测滚动时密集发 input 帧会成片触发。
/// 用 .no_proxy() 从根上避免流量绕道；顺带用共享 client 替掉每次新建（避免
/// 每条消息都重建连接池）。
fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("build local gateway http client")
    })
}

pub async fn fetch_sessions(cfg: &Config) -> Result<Value> {
    let url = format!(
        "{}/sessions?token={}",
        cfg.gateway_base,
        urlencoding_token(&cfg.gateway_token)
    );
    let resp = client()
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        bail!("sessions HTTP {}", resp.status());
    }
    Ok(resp.json().await?)
}

pub async fn fetch_menu(cfg: &Config, id: &str) -> Result<Value> {
    let url = format!(
        "{}/s/{}/menu?token={}",
        cfg.gateway_base,
        id,
        urlencoding_token(&cfg.gateway_token)
    );
    let resp = client()
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        bail!("menu HTTP {}", resp.status());
    }
    let body: Value = resp.json().await?;
    // gateway 返回 { menu: ... | null }
    Ok(body.get("menu").cloned().unwrap_or(Value::Null))
}

pub async fn post_json(cfg: &Config, path: &str, body: Value) -> Result<Value> {
    let url = format!(
        "{}{}{}token={}",
        cfg.gateway_base,
        path,
        if path.contains('?') { "&" } else { "?" },
        urlencoding_token(&cfg.gateway_token)
    );
    let resp = client()
        .post(&url)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("POST {path} HTTP {status}: {text}");
    }
    if text.is_empty() {
        return Ok(serde_json::json!({ "ok": true }));
    }
    Ok(serde_json::from_str(&text).unwrap_or_else(|_| serde_json::json!({ "ok": true, "raw": text })))
}

/// 订阅 PTY 流：header 文本 + binary 字节 → 回调。
pub async fn watch_pty<F, Fut>(cfg: &Config, id: &str, mut on_frame: F) -> Result<()>
where
    F: FnMut(PtyFrame) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let base = cfg
        .gateway_base
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1);
    let url = format!(
        "{base}/s/{id}/stream?token={}",
        urlencoding_token(&cfg.gateway_token)
    );
    let (ws, _) = connect_async(&url)
        .await
        .with_context(|| format!("watch {url}"))?;
    let (mut _sink, mut stream) = ws.split();
    while let Some(msg) = stream.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(e) => {
                warn!(%e, "watch ws");
                break;
            }
        };
        match msg {
            Message::Text(t) => {
                if let Ok(v) = serde_json::from_str::<Value>(&t) {
                    let cols = v.get("cols").and_then(|c| c.as_u64()).unwrap_or(80) as u16;
                    let rows = v.get("rows").and_then(|r| r.as_u64()).unwrap_or(24) as u16;
                    on_frame(PtyFrame::Header { cols, rows }).await;
                }
            }
            Message::Binary(b) => {
                on_frame(PtyFrame::Bytes(b.to_vec())).await;
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    // keep sink alive until end
    let _ = _sink.close().await;
    Ok(())
}

pub enum PtyFrame {
    Header { cols: u16, rows: u16 },
    Bytes(Vec<u8>),
}

/// 订阅会话 state-stream（phase / pending_question）。
pub async fn watch_state<F, Fut>(cfg: &Config, id: &str, mut on_state: F) -> Result<()>
where
    F: FnMut(Value) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let base = cfg
        .gateway_base
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1);
    let url = format!(
        "{base}/s/{id}/state-stream?token={}",
        urlencoding_token(&cfg.gateway_token)
    );
    let (ws, _) = connect_async(&url)
        .await
        .with_context(|| format!("state-stream {url}"))?;
    let (mut _sink, mut stream) = ws.split();
    while let Some(msg) = stream.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(e) => {
                warn!(%e, "state ws");
                break;
            }
        };
        match msg {
            Message::Text(t) => {
                if let Ok(v) = serde_json::from_str::<Value>(&t) {
                    on_state(v).await;
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    let _ = _sink.close().await;
    Ok(())
}

fn urlencoding_token(s: &str) -> String {
    // minimal encode for token in query
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            _ => format!("%{:02X}", c as u8),
        })
        .collect()
}
