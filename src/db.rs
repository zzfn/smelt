//! SQLite 持久层：初始化数据库与 instincts 表的读写。

use crate::model::{Instinct, Scope};
use anyhow::Result;
use rusqlite::Connection;
use std::path::PathBuf;

pub fn smelt_dir() -> Result<PathBuf> {
    let dir = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("无法定位 home 目录"))?
        .join(".smelt");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn open() -> Result<Connection> {
    let path = smelt_dir()?.join("smelt.db");
    let conn = Connection::open(path)?;
    init(&conn)?;
    Ok(conn)
}

fn init(conn: &Connection) -> Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS instincts (
            id             TEXT PRIMARY KEY,
            content        TEXT NOT NULL,
            confidence     REAL NOT NULL,
            domain         TEXT NOT NULL,
            evidence_count INTEGER NOT NULL,
            last_seen      TEXT NOT NULL,
            scope          TEXT NOT NULL
        )",
        [],
    )?;
    Ok(())
}

/// 插入或更新一条 instinct（按 id upsert，命中则累加 evidence_count）。
pub fn upsert(conn: &Connection, it: &Instinct) -> Result<()> {
    let domain = serde_json::to_string(&it.domain)?;
    conn.execute(
        "INSERT INTO instincts
            (id, content, confidence, domain, evidence_count, last_seen, scope)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(id) DO UPDATE SET
            content=excluded.content,
            confidence=excluded.confidence,
            domain=excluded.domain,
            evidence_count=instincts.evidence_count + 1,
            last_seen=excluded.last_seen,
            scope=excluded.scope",
        rusqlite::params![
            it.id, it.content, it.confidence, domain,
            it.evidence_count, it.last_seen, it.scope.as_str(),
        ],
    )?;
    Ok(())
}

/// 清空并重建整张表（去重合并后的整体替换，事务保证原子）。
pub fn replace_all(conn: &Connection, items: &[Instinct]) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute("DELETE FROM instincts", [])?;
    for it in items {
        let domain = serde_json::to_string(&it.domain)?;
        tx.execute(
            "INSERT OR REPLACE INTO instincts
                (id, content, confidence, domain, evidence_count, last_seen, scope)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                it.id, it.content, it.confidence, domain,
                it.evidence_count, it.last_seen, it.scope.as_str(),
            ],
        )?;
    }
    tx.commit()?;
    Ok(())
}

pub fn list_by_confidence(conn: &Connection) -> Result<Vec<Instinct>> {
    let mut stmt = conn.prepare(
        "SELECT id, content, confidence, domain, evidence_count, last_seen, scope
         FROM instincts ORDER BY confidence DESC",
    )?;
    let rows = stmt.query_map([], |row| {
        let domain_json: String = row.get(3)?;
        let domain: Vec<String> = serde_json::from_str(&domain_json).unwrap_or_default();
        let scope_str: String = row.get(6)?;
        Ok(Instinct {
            id: row.get(0)?,
            content: row.get(1)?,
            confidence: row.get(2)?,
            domain,
            evidence_count: row.get(4)?,
            last_seen: row.get(5)?,
            scope: Scope::from_db(&scope_str),
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}
