//! merge：用 DeepSeek 对已有 instincts 做语义去重合并。
//! LLM 只负责归并分组；confidence/evidence_count 在本地计算，保证准确。

use crate::db;
use crate::digest;
use crate::model::{Instinct, Scope};
use crate::render;
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
struct MergeInput<'a> {
    index: usize,
    content: &'a str,
    domain: &'a [String],
}

#[derive(Deserialize)]
struct MergeGroup {
    content: String,
    #[serde(default)]
    domain: Vec<String>,
    members: Vec<usize>,
}

pub async fn run() -> Result<()> {
    let conn = db::open()?;
    let items = db::list_by_confidence(&conn)?;
    if items.len() < 2 {
        println!("instinct 不足 2 条，无需合并。");
        return Ok(());
    }

    let key = digest::api_key()?;
    let groups = call_merge(&key, &items).await?;

    let mut merged: Vec<Instinct> = Vec::new();
    let mut seen = vec![false; items.len()];

    for g in groups {
        let members: Vec<usize> = g
            .members
            .into_iter()
            .filter(|&i| i < items.len() && !seen[i])
            .collect();
        if members.is_empty() {
            continue;
        }
        let confidence = members
            .iter()
            .map(|&i| items[i].confidence)
            .fold(0.0_f32, f32::max)
            .clamp(0.3, 0.9);
        let evidence_count: u32 = members.iter().map(|&i| items[i].evidence_count).sum();
        let last_seen = members
            .iter()
            .map(|&i| items[i].last_seen.clone())
            .max()
            .unwrap_or_default();
        for &i in &members {
            seen[i] = true;
        }
        merged.push(Instinct {
            id: digest::stable_id(&g.content),
            content: g.content,
            confidence,
            domain: g.domain,
            evidence_count,
            last_seen,
            scope: Scope::Global,
        });
    }

    // LLM 漏掉的条目原样保留，绝不丢数据。
    for (i, s) in seen.iter().enumerate() {
        if !*s {
            merged.push(items[i].clone());
        }
    }

    db::replace_all(&conn, &merged)?;
    let path = render::write_global()?;
    println!(
        "合并完成：{} 条 → {} 条，已更新 {:?}",
        items.len(),
        merged.len(),
        path
    );
    Ok(())
}

async fn call_merge(key: &str, items: &[Instinct]) -> Result<Vec<MergeGroup>> {
    let inputs: Vec<MergeInput> = items
        .iter()
        .enumerate()
        .map(|(i, it)| MergeInput {
            index: i,
            content: &it.content,
            domain: &it.domain,
        })
        .collect();
    let payload = serde_json::to_string(&inputs)?;

    let prompt = format!(
        "下面是已收集的编码习惯 instinct 列表（JSON，每条带 index）。\
         请把语义相同或高度重叠的条目归并为一组。输出合并后的分组 JSON 数组，\
         每个元素形如 {{\"content\": \"该组最具代表性的中文表述\", \"domain\": [\"合并去重后的领域标签\"], \"members\": [该组包含的 index 整数]}}。\
         要求：每个输入 index 必须且只能出现在一个组中；语义不重复的条目各自单独成组；content 用简洁准确的中文重写。\
         只返回 JSON 数组，不要任何其他内容。\n\n=== 输入 ===\n{payload}"
    );

    let text = digest::chat(key, &prompt).await?;
    let start = text.find('[').ok_or_else(|| anyhow!("响应中无 JSON 数组: {text}"))?;
    let end = text.rfind(']').ok_or_else(|| anyhow!("响应中无 JSON 数组: {text}"))?;
    let groups: Vec<MergeGroup> = serde_json::from_str(&text[start..=end])
        .with_context(|| format!("解析合并分组失败: {}", &text[start..=end]))?;
    Ok(groups)
}
