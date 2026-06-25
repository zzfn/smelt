//! render：把 DB 中的 instincts 渲染为 ~/.smelt/global.md，供 Claude Code 读取。

use crate::db;
use crate::model::Scope;
use anyhow::Result;
use std::path::PathBuf;

/// 把所有 Global 作用域的 instinct 渲染成 markdown 写入 ~/.smelt/global.md。
pub fn write_global() -> Result<PathBuf> {
    let conn = db::open()?;
    let items = db::list_by_confidence(&conn)?;

    let mut md = String::from(
        "# Smelt Instincts\n\n> 本文件由 smelt 自动生成，请勿手动编辑。\n\n",
    );

    let globals: Vec<_> = items
        .into_iter()
        .filter(|it| it.scope == Scope::Global)
        .collect();

    if globals.is_empty() {
        md.push_str("_（暂无）_\n");
    } else {
        for it in &globals {
            md.push_str(&format!("- **[{:.2}]** {}", it.confidence, it.content));
            if !it.domain.is_empty() {
                md.push_str(&format!(" `{}`", it.domain.join("/")));
            }
            md.push_str(&format!(" _(×{})_\n", it.evidence_count));
        }
    }

    let path = db::smelt_dir()?.join("global.md");
    std::fs::write(&path, md)?;
    Ok(path)
}
