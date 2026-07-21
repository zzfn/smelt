//! 宠物 agent —— 给桌面宠物接一个可配置的 LLM「大脑」。
//!
//! 走 OpenAI 兼容的 `/chat/completions` 协议（和项目里 digest/chat 一致），所以 DeepSeek /
//! OpenAI / Kimi / 本地兼容服务都能接，只是换 `base_url` + `model` + `api_key`。
//!
//! 配置是跨窗口全局单例（存 ~/.smelt/llm.json），宠物窗口每帧读它决定是否用 LLM 说话。

use gpui::*;
use serde_json::json;

/// 宠物大脑配置（OpenAI 兼容）。
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct LlmConfig {
    /// 总开关：关掉就回退到写死台词 / 原样播报。
    pub enabled: bool,
    /// Chat Completions 完整 URL（含路径）。
    pub base_url: String,
    /// API Key；留空则回落到环境变量 `DEEPSEEK_API_KEY`。
    pub api_key: String,
    /// 模型名。
    pub model: String,
    /// 人设 system prompt。
    pub persona: String,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: "https://api.deepseek.com/chat/completions".into(),
            api_key: String::new(),
            model: "deepseek-chat".into(),
            persona: "你是用户桌面上的一只软萌史莱姆宠物，名叫 smelt。性格黏人、俏皮、话少。\
                永远用中文口语化回复，每次只说一句、不超过 20 字，可带一个 emoji。\
                不要解释、不要客套、不要引号。"
                .into(),
        }
    }
}

impl Global for LlmConfig {}

fn llm_config_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".smelt").join("llm.json"))
}

/// 读取宠物大脑配置；缺失 / 损坏回退默认。
pub fn load_llm_config() -> LlmConfig {
    crate::json_store::load_json(llm_config_path())
}

/// 写回宠物大脑配置（失败静默忽略）。
pub fn save_llm_config(c: &LlmConfig) {
    crate::json_store::save_json(llm_config_path(), c)
}

/// 解析出可用的 API key：配置优先 → 环境变量 `DEEPSEEK_API_KEY` → `~/.smelt/config.toml`
/// 里手写的 `DEEPSEEK_API_KEY = "..."` 行（复用 CLI 那套，配过就即插即用）。
fn resolve_key(cfg: &LlmConfig) -> Option<String> {
    if !cfg.api_key.trim().is_empty() {
        return Some(cfg.api_key.trim().to_string());
    }
    if let Ok(k) = std::env::var("DEEPSEEK_API_KEY") {
        if !k.is_empty() {
            return Some(k);
        }
    }
    let path = dirs::home_dir()?.join(".smelt").join("config.toml");
    let text = std::fs::read_to_string(path).ok()?;
    text.lines().find_map(|line| {
        line.trim()
            .strip_prefix("DEEPSEEK_API_KEY")?
            .trim_start()
            .strip_prefix('=')?
            .trim()
            .trim_matches('"')
            .to_string()
            .into()
    })
}

/// 一次性问一句，拿回宠物要说的话（走宠物人设 system prompt）。是 complete_with_system
/// 的宠物专用封装，system prompt 固定用 cfg.persona、max_tokens 固定给短回复用的 120。
pub async fn complete(cfg: LlmConfig, user: String) -> anyhow::Result<String> {
    let persona = cfg.persona.clone();
    complete_with_system(cfg, persona, user, 120).await
}

/// 一次性问一句，system prompt 由调用方指定（OpenAI 兼容 `/chat/completions`，非流式）。
/// 是 complete() 的通用版本：宠物聊天固定用 cfg.persona 这个人设，别的用途（比如生成
/// commit message）需要自己的 system prompt，不能被宠物人设污染，所以拆出这个函数。
///
/// 照抄 `digest::chat` 的请求 / 解析逻辑，只是把 URL / model / key 换成配置驱动。
pub async fn complete_with_system(
    cfg: LlmConfig,
    system: String,
    user: String,
    max_tokens: u32,
) -> anyhow::Result<String> {
    let key = resolve_key(&cfg).ok_or_else(|| anyhow::anyhow!("缺少 API key"))?;
    let body = json!({
        "model": cfg.model,
        "max_tokens": max_tokens,
        "stream": false,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user},
        ],
    });
    let resp = reqwest::Client::new()
        .post(&cfg.base_url)
        .bearer_auth(key)
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await?;
    let v: serde_json::Value = resp.json().await?;
    let text = v["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("")
        .trim()
        .trim_matches(|c| c == '"' || c == '「' || c == '」')
        .to_string();
    if text.is_empty() {
        anyhow::bail!("空回复");
    }
    Ok(text)
}

/// 有没有配好可用的 API key——不看 `enabled`（那是宠物聊天开关，跟"凭据配没配好"是
/// 两回事），别的功能（commit message 生成）只关心凭据本身。
pub fn has_credentials(cfg: &LlmConfig) -> bool {
    resolve_key(cfg).is_some()
}
