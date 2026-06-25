//! digest：读取 shell 历史，调用 DeepSeek API 提炼 instinct 并入库，最后刷新 global.md。

use crate::db;
use crate::model::{Instinct, Scope};
use crate::render;
use anyhow::{Context, Result};
use serde::Deserialize;

const MODEL: &str = "deepseek-chat";
const API_URL: &str = "https://api.deepseek.com/chat/completions";
const MAX_TOKENS: u32 = 2048;

#[derive(Debug, Deserialize)]
struct RawInstinct {
    content: String,
    confidence: f32,
    domain: Vec<String>,
}

fn read_history(n: usize) -> Result<String> {
    let path = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("无法定位 home 目录"))?
        .join(".zsh_history");
    let bytes = std::fs::read(&path).with_context(|| format!("读取 {:?} 失败", path))?;
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    Ok(lines[start..].join("\n"))
}

/// 读取 API key（供 digest / merge 复用）。
pub(crate) fn api_key() -> Result<String> {
    if let Ok(k) = std::env::var("DEEPSEEK_API_KEY") {
        if !k.is_empty() {
            return Ok(k);
        }
    }
    let cfg = db::smelt_dir()?.join("config.toml");
    let text = std::fs::read_to_string(&cfg)
        .with_context(|| format!("读取 {:?} 失败，且未设置 DEEPSEEK_API_KEY", cfg))?;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("DEEPSEEK_API_KEY") {
            if let Some(eq) = rest.split('=').nth(1) {
                return Ok(eq.trim().trim_matches('"').to_string());
            }
        }
    }
    anyhow::bail!("config.toml 中未找到 DEEPSEEK_API_KEY")
}

/// 基于内容生成稳定 id（FNV-1a，供 digest / merge 复用）。
pub(crate) fn stable_id(content: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in content.bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{:016x}", hash)
}

/// 通用聊天补全（供 digest / merge 复用）。
pub(crate) async fn chat(key: &str, prompt: &str) -> Result<String> {
    let body = serde_json::json!({
        "model": MODEL,
        "max_tokens": MAX_TOKENS,
        "stream": false,
        "messages": [{ "role": "user", "content": prompt }]
    });
    let client = reqwest::Client::new();
    let resp = client
        .post(API_URL)
        .bearer_auth(key)
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .context("请求 DeepSeek API 失败")?;
    let status = resp.status();
    let json: serde_json::Value = resp.json().await.context("解析 API 响应失败")?;
    if !status.is_success() {
        anyhow::bail!("DeepSeek API 返回错误 {}: {}", status, json);
    }
    let text = json["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("API 响应缺少 message.content 字段: {}", json))?;
    Ok(text.to_string())
}

pub async fn run() -> Result<()> {
    let history = read_history(200)?;
    if history.trim().is_empty() {
        println!("shell 历史为空，跳过。");
        return Ok(());
    }
    let key = api_key()?;
    let prompt = format!(
        "下面是我最近的 shell 命令历史。请提炼出 3-5 条关于我编码 / 工作习惯的 instinct。\n\
         每条要具体、可操作。只返回 JSON 数组，每个元素形如 \
         {{\"content\": \"...\", \"confidence\": 0.3-0.9 的小数, \"domain\": [\"领域标签\"]}}。\n\
         不要输出 JSON 以外的任何内容。\n\n\
         === shell 历史 ===\n{history}"
    );
    let text = chat(&key, &prompt).await?;
    let raws = parse_instincts(&text)?;
    println!("提炼出 {} 条 instinct。", raws.len());

    let conn = db::open()?;
    let now = chrono::Utc::now().to_rfc3339();
    for r in &raws {
        let confidence = r.confidence.clamp(0.3, 0.9);
        let id = stable_id(&r.content);
        let it = Instinct {
            id,
            content: r.content.clone(),
            confidence,
            domain: r.domain.clone(),
            evidence_count: 1,
            last_seen: now.clone(),
            scope: Scope::Global,
        };
        db::upsert(&conn, &it)?;
        println!("  [{:.2}] {}", confidence, r.content);
    }
    let path = render::write_global()?;
    println!("已更新 {:?}", path);
    Ok(())
}

fn parse_instincts(text: &str) -> Result<Vec<RawInstinct>> {
    let start = text.find('[').ok_or_else(|| anyhow::anyhow!("响应中无 JSON 数组: {text}"))?;
    let end = text.rfind(']').ok_or_else(|| anyhow::anyhow!("响应中无 JSON 数组: {text}"))?;
    let slice = &text[start..=end];
    let raws: Vec<RawInstinct> =
        serde_json::from_str(slice).with_context(|| format!("解析 instinct JSON 失败: {slice}"))?;
    Ok(raws)
}
