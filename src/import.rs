//! Importers from other agents' memory stores. Read-only on the source: nothing is deleted.
//! Re-running an import is safe: already-imported memories are skipped, changed ones updated.

use crate::memory::{KINDS, Memory, slugify};
use crate::project::{Ident, Registry};
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

/// Claude's project dir names encode the path with `/` and `.` turned into `-`, which is lossy.
/// Find the real directory by trying `/`, `-` and `.` between the parts.
fn resolve_dir(slug: &str) -> Option<PathBuf> {
    fn walk(base: &Path, comp: &str, rest: &[&str]) -> Option<PathBuf> {
        let Some((tok, rest)) = rest.split_first() else {
            let p = base.join(comp);
            return p.is_dir().then_some(p);
        };
        if comp.is_empty() {
            return walk(base, tok, rest);
        }
        let dir = base.join(comp);
        if dir.is_dir() {
            if let Some(p) = walk(&dir, tok, rest) {
                return Some(p);
            }
        }
        walk(base, &format!("{comp}-{tok}"), rest).or_else(|| walk(base, &format!("{comp}.{tok}"), rest))
    }
    let parts: Vec<&str> = slug.trim_start_matches('-').split('-').collect();
    walk(Path::new("/"), "", &parts)
}

/// Preferences follow you everywhere; everything else stays in the project Claude scoped it to.
fn default_scope(reg: &mut Registry, kind: &str, slug: &str, short: &str) -> Option<String> {
    let key = match (kind, resolve_dir(slug)) {
        ("user" | "feedback", _) => return None,
        (_, Some(dir)) => reg.key_for(&Ident::of(&dir)),
        (_, None) => {
            reg.projects.entry(short.to_string()).or_default();
            short.to_string()
        }
    };
    Some(format!("project:{key}"))
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
        if let Some(stem) = m.extra.get("claude_file") {
            imported.insert((proj.clone(), slugify(stem)), m.clone());
        }
    }
    // Imports from before `claude_file` existed: the name is the stem, possibly prefixed with the
    // project on a collision. The plain name wins; the stripped form is only a fallback, so a stem
    // that itself starts with the project name (`picklescan_venv`) isn't mistaken for a new memory.
    let legacy: Vec<&Memory> = existing
        .iter()
        .filter(|m| m.extra.get("source").map(String::as_str) == Some("claude-code") && !m.extra.contains_key("claude_file"))
        .collect();
    for m in &legacy {
        let proj = &m.extra["claude_project"];
        imported.entry((proj.clone(), m.name.clone())).or_insert_with(|| (*m).clone());
    }
    for m in &legacy {
        let proj = &m.extra["claude_project"];
        if let Some(stripped) = m.name.strip_prefix(&format!("{}-", short_project(proj))) {
            imported.entry((proj.clone(), stripped.to_string())).or_insert_with(|| (*m).clone());
        }
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
    let mut reg = Registry::load(&vault.root);

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
        // Keep a visibility already set (by the user or a previous import); else Claude's scope.
        let scope = prior.and_then(|p| p.extra.get("visibility").cloned()).or_else(|| default_scope(&mut reg, &m.kind, &slug, &project));
        if let Some(v) = scope {
            m.extra.insert("visibility".into(), v);
        }
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
            Some(p) if p.body == m.body && p.description == m.description && p.kind == m.kind && p.extra.get("visibility") == m.extra.get("visibility") => Status::Unchanged,
            Some(_) => Status::Updated,
        };
    }

    if dry_run || plans.iter().all(|p| p.status == Status::Unchanged) {
        return Ok((plans, None));
    }
    let _lock = vault.lock()?;
    reg.save(&vault.root)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_lossy_claude_project_slugs() {
        let tmp = tempfile::Builder::new().prefix("hippo").tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for d in ["work/tokenizers/tokenizers", "side/le.site", "side/my-app"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        for d in ["work/tokenizers/tokenizers", "side/le.site", "side/my-app"] {
            let p = root.join(d);
            let slug = p.to_string_lossy().replace(['/', '.'], "-");
            assert_eq!(resolve_dir(&slug), Some(p.clone()), "{slug}");
        }
        assert_eq!(resolve_dir("-nope-not-here"), None);
    }
}
