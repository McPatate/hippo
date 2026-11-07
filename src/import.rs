//! Importers from other agents' memory stores. Read-only on the source: nothing is deleted.
//! Re-running an import is safe: already-imported memories are skipped, changed ones updated.

use crate::memory::{KINDS, Memory, slugify};
use crate::vault::Vault;
use anyhow::{Context, Result};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    New,
    Updated,
    Unchanged,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Planned {
    pub source: PathBuf,
    pub name: String,
    pub kind: String,
    pub project: String,
    pub renamed: bool,
    pub status: Status,
}

pub fn count(plans: &[Planned], s: Status) -> usize {
    plans.iter().filter(|p| p.status == s).count()
}

/// Claude Code stores memories in `~/.claude/projects/<project-slug>/memory/*.md`
/// (one fact per file, YAML frontmatter, plus a `MEMORY.md` index we skip).
pub fn default_claude_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".claude").join("projects")
}

/// `-Users-luc-Documents-work-safetensors` → `safetensors`, for tags and collision prefixes.
fn short_project(slug: &str) -> String {
    let home = dirs::home_dir().map(|h| h.to_string_lossy().replace(['/', '.'], "-")).unwrap_or_default();
    let mut s = slug.strip_prefix(&home).unwrap_or(slug).trim_start_matches('-').to_string();
    for p in ["Documents-", "work-", "side-projects-", "scratch-", "code-", "src-", "dev-", "projects-"] {
        if let Some(rest) = s.strip_prefix(p) {
            s = rest.to_string();
        }
    }
    slugify(&s)
}

fn sources(from: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut out = vec![];
    for proj in std::fs::read_dir(from).with_context(|| format!("cannot read {}", from.display()))? {
        let proj = proj?.path();
        let mem_dir = proj.join("memory");
        if !mem_dir.is_dir() {
            continue;
        }
        let slug = proj.file_name().unwrap().to_string_lossy().to_string();
        for e in std::fs::read_dir(&mem_dir)? {
            let p = e?.path();
            if p.extension().is_some_and(|e| e == "md") && p.file_name().is_some_and(|n| n != "MEMORY.md") {
                out.push((slug.clone(), p));
            }
        }
    }
    // Newest first: on a name collision the most recently modified memory keeps the plain name.
    out.sort_by_key(|(_, p)| std::cmp::Reverse(std::fs::metadata(p).and_then(|m| m.modified()).ok()));
    Ok(out)
}

pub fn plan_and_import_claude(vault: &Vault, from: &Path, dry_run: bool) -> Result<(Vec<Planned>, Option<String>)> {
    let existing = vault.list()?;
    // Previously imported memories, keyed by (claude project, source file stem).
    let mut imported: HashMap<(String, String), Memory> = HashMap::new();
    for m in &existing {
        if m.extra.get("source").map(String::as_str) != Some("claude-code") {
            continue;
        }
        let Some(proj) = m.extra.get("claude_project") else { continue };
        let stem = m.extra.get("claude_file").cloned().unwrap_or_else(|| {
            // Imports from before `claude_file` existed: name is the stem, maybe project-prefixed.
            let p = format!("{}-", short_project(proj));
            m.name.strip_prefix(&p).unwrap_or(&m.name).to_string()
        });
        imported.insert((proj.clone(), slugify(&stem)), m.clone());
    }
    let taken_by_other: HashMap<String, ()> = existing
        .iter()
        .filter(|m| m.extra.get("source").map(String::as_str) != Some("claude-code"))
        .map(|m| (m.name.clone(), ()))
        .collect();

    let mut used: HashMap<String, String> = HashMap::new(); // name → project slug
    let mut plans = vec![];
    let mut memories = vec![];
    let mut renames: BTreeMap<String, HashMap<String, String>> = BTreeMap::new();

    for (slug, path) in sources(from)? {
        let text = std::fs::read_to_string(&path)?;
        let stem = path.file_stem().unwrap().to_string_lossy().to_string();
        let mut m = Memory::parse(&stem, &text);
        let original_name = m.name.clone();
        // The file stem is Claude Code's stable id; frontmatter names are sometimes long titles.
        if m.description.is_empty() && original_name != stem {
            m.description = original_name.clone();
        }
        let project = short_project(&slug);
        if !KINDS.contains(&m.kind.as_str()) {
            m.kind = "note".into();
        }
        for k in ["metadata", "originSessionId", "node_type"] {
            m.extra.remove(k);
        }
        m.extra.insert("source".into(), "claude-code".into());
        m.extra.insert("claude_project".into(), slug.clone());
        m.extra.insert("claude_file".into(), stem.clone());
        if m.description.is_empty() {
            m.description = m.body.lines().map(|l| l.trim_start_matches('#').trim()).find(|l| !l.is_empty()).unwrap_or("").chars().take(120).collect();
        }
        for t in ["claude-code".to_string(), project.clone()] {
            if !m.tags.contains(&t) {
                m.tags.push(t);
            }
        }
        let prior = imported.get(&(slug.clone(), slugify(&stem)));
        let base = slugify(&stem);
        let name = match prior {
            Some(p) => p.name.clone(), // keep the name it was imported under
            None => {
                let clash = |n: &str| taken_by_other.contains_key(n) || used.get(n).is_some_and(|s| *s != slug) || imported.values().any(|p| p.name == n);
                let n = if clash(&base) { slugify(&format!("{project}-{base}")) } else { base.clone() };
                if clash(&n) { format!("{n}-{}", crate::util::random_hex(2)) } else { n }
            }
        };
        used.insert(name.clone(), slug.clone());
        let map = renames.entry(slug.clone()).or_default();
        map.insert(original_name, name.clone());
        map.insert(stem.clone(), name.clone());
        m.name = name.clone();
        plans.push(Planned { source: path.clone(), name: name.clone(), kind: m.kind.clone(), project, renamed: name != base, status: Status::New });
        memories.push((slug, m, prior.cloned()));
    }

    // Rewrite [[links]] to the new names, then compare with what's already in the vault.
    for (i, (slug, m, prior)) in memories.iter_mut().enumerate() {
        if let Some(map) = renames.get(slug.as_str()) {
            for (old, new) in map {
                if old != new {
                    m.body = m.body.replace(&format!("[[{old}]]"), &format!("[[{new}]]"));
                }
            }
        }
        plans[i].status = match prior {
            None => Status::New,
            Some(p) if p.body == m.body && p.description == m.description && p.kind == m.kind => Status::Unchanged,
            Some(_) => Status::Updated,
        };
    }

    if dry_run || plans.iter().all(|p| p.status == Status::Unchanged) {
        return Ok((plans, None));
    }
    let _lock = vault.lock()?;
    for ((_, m, _), p) in memories.iter().zip(&plans) {
        if p.status != Status::Unchanged {
            vault.write_memory(m)?;
        }
    }
    let msg = format!(
        "import from Claude Code: {} new, {} updated",
        count(&plans, Status::New),
        count(&plans, Status::Updated)
    );
    let commit = vault.save_locked(&msg)?;
    Ok((plans, commit))
}
