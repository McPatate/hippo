//! Importers from other agents' memory stores. Read-only on the source: nothing is deleted.

use crate::memory::{KINDS, Memory, slugify};
use crate::vault::Vault;
use anyhow::{Context, Result};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, serde::Serialize)]
pub struct Planned {
    pub source: PathBuf,
    pub name: String,
    pub kind: String,
    pub project: String,
    pub renamed: bool,
}

/// Claude Code stores memories in `~/.claude/projects/<project-slug>/memory/*.md`
/// (one fact per file, YAML frontmatter, plus a `MEMORY.md` index we skip).
pub fn default_claude_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".claude").join("projects")
}

/// `-Users-luc-Documents-work-safetensors` → `safetensors`-ish short name for tags/prefixes.
fn short_project(slug: &str) -> String {
    let home = dirs::home_dir()
        .map(|h| h.to_string_lossy().replace(['/', '.'], "-"))
        .unwrap_or_default();
    let mut s = slug.strip_prefix(&home).unwrap_or(slug).trim_start_matches('-').to_string();
    for p in ["Documents-", "work-", "side-projects-", "scratch-", "code-", "src-", "dev-", "projects-"] {
        if let Some(rest) = s.strip_prefix(p) {
            s = rest.to_string();
        }
    }
    slugify(&s)
}

pub fn plan_and_import_claude(vault: &Vault, from: &Path, dry_run: bool) -> Result<(Vec<Planned>, Option<String>)> {
    let mut sources: Vec<(String, PathBuf)> = vec![];
    for proj in std::fs::read_dir(from).with_context(|| format!("cannot read {}", from.display()))? {
        let proj = proj?.path();
        let mem_dir = proj.join("memory");
        if !mem_dir.is_dir() {
            continue;
        }
        let slug = proj.file_name().unwrap().to_string_lossy().to_string();
        let mut files: Vec<PathBuf> = std::fs::read_dir(&mem_dir)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "md") && p.file_name().is_some_and(|n| n != "MEMORY.md"))
            .collect();
        files.sort();
        for f in files {
            sources.push((slug.clone(), f));
        }
    }
    // Newest first: on a name collision the most recently modified memory keeps the plain name.
    sources.sort_by_key(|(_, p)| std::cmp::Reverse(std::fs::metadata(p).and_then(|m| m.modified()).ok()));

    let existing: HashMap<String, String> = vault.list()?.into_iter().map(|m| (m.name.clone(), m.body)).collect();
    let mut used: HashMap<String, String> = HashMap::new(); // name → body
    let mut plans = vec![];
    let mut memories = vec![];
    // Per project: original link target → new name, for [[link]] rewriting.
    let mut renames: BTreeMap<String, HashMap<String, String>> = BTreeMap::new();

    for (slug, path) in &sources {
        let text = std::fs::read_to_string(path)?;
        let stem = path.file_stem().unwrap().to_string_lossy().to_string();
        let mut m = Memory::parse(&stem, &text);
        let original_name = m.name.clone();
        // The file stem is Claude Code's stable id; frontmatter names are sometimes long titles.
        if m.description.is_empty() && original_name != stem {
            m.description = original_name.clone();
        }
        m.name = stem.clone();
        let project = short_project(slug);
        if !KINDS.contains(&m.kind.as_str()) {
            m.kind = "note".into();
        }
        for k in ["metadata", "originSessionId", "node_type"] {
            m.extra.remove(k);
        }
        m.extra.insert("source".into(), "claude-code".into());
        m.extra.insert("claude_project".into(), slug.clone());
        if m.description.is_empty() {
            m.description = m.body.lines().map(|l| l.trim_start_matches('#').trim()).find(|l| !l.is_empty()).unwrap_or("").chars().take(120).collect();
        }
        for t in ["claude-code".to_string(), project.clone()] {
            if !m.tags.contains(&t) {
                m.tags.push(t);
            }
        }
        let base = slugify(&m.name);
        let taken = |n: &str| used.get(n).is_some_and(|b| *b != m.body) || existing.get(n).is_some_and(|b| *b != m.body);
        let name = if taken(&base) { slugify(&format!("{project}-{base}")) } else { base.clone() };
        let name = if taken(&name) { format!("{name}-{}", crate::util::random_hex(2)) } else { name };
        used.insert(name.clone(), m.body.clone());
        let map = renames.entry(slug.clone()).or_default();
        map.insert(original_name.clone(), name.clone());
        map.insert(stem.clone(), name.clone());
        m.name = name.clone();
        plans.push(Planned { source: path.clone(), name: name.clone(), kind: m.kind.clone(), project, renamed: name != base });
        memories.push((slug.clone(), m));
    }

    if dry_run {
        return Ok((plans, None));
    }
    let _lock = vault.lock()?;
    for (slug, mut m) in memories {
        if let Some(map) = renames.get(&slug) {
            for (old, new) in map {
                if old != new {
                    m.body = m.body.replace(&format!("[[{old}]]"), &format!("[[{new}]]"));
                }
            }
        }
        vault.write_memory(&m)?;
    }
    let commit = vault.save_locked(&format!("import {} memories from Claude Code", plans.len()))?;
    Ok((plans, commit))
}
