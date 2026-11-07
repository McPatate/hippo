//! Derived full-text index (SQLite FTS5) at `<vault>/.hippo/index.sqlite`.
//!
//! Never canonical: delete it any time and it is rebuilt from the Markdown files. Refreshing is
//! incremental (mtime + size per file), so a search on an unchanged vault touches no file content.

use crate::memory::Memory;
use anyhow::Result;
use rusqlite::{Connection, params};
use std::collections::HashMap;
use std::path::Path;

pub struct Index {
    conn: Connection,
}

#[derive(Debug, Clone)]
pub struct Hit {
    pub name: String,
    pub description: String,
    pub snippet: String,
}

impl Index {
    pub fn open(vault_root: &Path) -> Result<Self> {
        let path = vault_root.join(".hippo").join("index.sqlite");
        let conn = Connection::open(&path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS files(name TEXT PRIMARY KEY, mtime INTEGER, size INTEGER);
             CREATE VIRTUAL TABLE IF NOT EXISTS docs USING fts5(
                name, description, tags, body, tokenize='unicode61 remove_diacritics 2');",
        )?;
        Ok(Self { conn })
    }

    /// Bring the index in line with `memories/`. Returns the number of re-indexed files.
    pub fn refresh(&mut self, memories_dir: &Path) -> Result<usize> {
        let mut known: HashMap<String, (i64, i64)> = HashMap::new();
        {
            let mut stmt = self.conn.prepare("SELECT name, mtime, size FROM files")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))?;
            for row in rows {
                let (n, v) = row?;
                known.insert(n, v);
            }
        }
        let tx = self.conn.transaction()?;
        let mut changed = 0;
        if memories_dir.exists() {
            for entry in std::fs::read_dir(memories_dir)? {
                let p = entry?.path();
                if p.extension().is_none_or(|e| e != "md") {
                    continue;
                }
                let name = p.file_stem().unwrap().to_string_lossy().to_string();
                let meta = std::fs::metadata(&p)?;
                let mtime = meta
                    .modified()?
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as i64)
                    .unwrap_or(0);
                let size = meta.len() as i64;
                if known.remove(&name) == Some((mtime, size)) {
                    continue;
                }
                let m = Memory::parse(&name, &std::fs::read_to_string(&p).unwrap_or_default());
                tx.execute("DELETE FROM docs WHERE name = ?1", params![name])?;
                tx.execute(
                    "INSERT INTO docs(name, description, tags, body) VALUES (?1, ?2, ?3, ?4)",
                    params![name, m.description, m.tags.join(" "), m.body],
                )?;
                tx.execute(
                    "INSERT OR REPLACE INTO files(name, mtime, size) VALUES (?1, ?2, ?3)",
                    params![name, mtime, size],
                )?;
                changed += 1;
            }
        }
        for gone in known.keys() {
            tx.execute("DELETE FROM docs WHERE name = ?1", params![gone])?;
            tx.execute("DELETE FROM files WHERE name = ?1", params![gone])?;
            changed += 1;
        }
        tx.commit()?;
        Ok(changed)
    }

    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>> {
        let terms: Vec<String> = query
            .split_whitespace()
            .map(|t| t.chars().filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_').collect::<String>())
            .filter(|t| !t.is_empty())
            .map(|t| format!("\"{t}\"*"))
            .collect();
        if terms.is_empty() {
            return Ok(vec![]);
        }
        // All terms first; fall back to any term.
        for joiner in [" ", " OR "] {
            let q = terms.join(joiner);
            let mut stmt = self.conn.prepare(
                "SELECT name, description, snippet(docs, 3, '«', '»', '…', 12)
                 FROM docs WHERE docs MATCH ?1
                 ORDER BY bm25(docs, 10.0, 5.0, 3.0, 1.0) LIMIT ?2",
            )?;
            let hits: Vec<Hit> = stmt
                .query_map(params![q, limit as i64], |r| {
                    Ok(Hit {
                        name: r.get(0)?,
                        description: r.get(1)?,
                        snippet: r.get(2)?,
                    })
                })?
                .collect::<Result<_, _>>()?;
            if !hits.is_empty() {
                return Ok(hits);
            }
        }
        Ok(vec![])
    }
}
