//! 本机 remote_gateway HTTP/WS 客户端。

use anyhow::{bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::warn;

use crate::Config;

pub async fn fetch_sessions(cfg: &Config) -> Result<Value> {
    let url = format!(
        "{}/sessions?token={}",
        cfg.gateway_base,
        urlencoding_token(&cfg.gateway_token)
    );
    let resp = reqwest::Client::new()
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        bail!("sessions HTTP {}", resp.status());
    }
    Ok(resp.json().await?)
}

pub async fn post_json(cfg: &Config, path: &str, body: Value) -> Result<Value> {
    let url = format!(
        "{}{}{}token={}",
        cfg.gateway_base,
        path,
        if path.contains('?') { "&" } else { "?" },
        urlencoding_token(&cfg.gateway_token)
    );
    let resp = reqwest::Client::new()
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

fn urlencoding_token(s: &str) -> String {
    // minimal encode for token in query
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            _ => format!("%{:02X}", c as u8),
        })
        .collect()
}
