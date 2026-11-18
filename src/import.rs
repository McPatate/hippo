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
    /// Looks like it holds a secret: left out of the vault.
    Secret,
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

/// What each Claude Code file looked like when hippo last took it (`imported.toml`, synced).
/// A file is only imported again once Claude changes it, so what the user edits, merges or
/// deletes in hippo stays that way.
pub const LEDGER: &str = "imported.toml";

#[derive(Debug, Default, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct Ledger {
    /// `<claude project slug>/<file stem>` → hash of the file as imported (hippo's history holds it).
    #[serde(default)]
    claude_code: BTreeMap<String, String>,
    /// Seen but not imported: declined by the user, or looked like a secret.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    claude_code_skipped: BTreeMap<String, String>,
}

impl Ledger {
    fn load(vault: &Path) -> Result<Option<Ledger>> {
        match std::fs::read_to_string(vault.join(LEDGER)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
            Ok(t) => Ok(Some(toml::from_str(&t).with_context(|| format!("{LEDGER} in your vault is unreadable (an unresolved merge?)\n  → Fix it by hand, then retry"))?)),
        }
    }

    fn seen(&self, id: &str) -> Option<&String> {
        self.claude_code.get(id).or_else(|| self.claude_code_skipped.get(id))
    }

    fn save(&self, vault: &Path) -> Result<()> {
        crate::util::atomic_write(&vault.join(LEDGER), toml::to_string(self)?.as_bytes())
    }
}

/// Was this Claude file imported, then deleted in hippo? (Vaults from before the ledger.)
fn deleted_after_import(vault: &Vault, slug: &str, stem: &str) -> bool {
    [slugify(stem), slugify(&format!("{}-{stem}", short_project(slug)))].iter().any(|name| {
        let file = format!("memories/{name}.md");
        let Ok(c) = vault.git.run(["log", "-1", "--format=%H", "--diff-filter=D", "--", &file]) else { return false };
        let c = c.trim();
        !c.is_empty() && vault.git.run(["show", &format!("{c}^:{file}")]).is_ok_and(|t| t.contains(&format!("claude_project: {slug}")))
    })
}

/// Previously imported memories, keyed by (claude project, slugified source file stem).
fn imported_map(existing: &[Memory]) -> HashMap<(String, String), Memory> {
    let mut imported: HashMap<(String, String), Memory> = HashMap::new();
    for m in existing {
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
    imported
}

fn hash(text: &str) -> String {
    use sha2::Digest;
    hex::encode(&sha2::Sha256::digest(text.as_bytes())[..8])
}

/// When the vault last imported from Claude Code, for vaults that did so before the ledger.
fn last_import(vault: &Vault) -> Option<std::time::SystemTime> {
    let t: u64 = vault.git.run(["log", "-1", "--format=%ct", "--grep=^import"]).ok()?.trim().parse().ok()?;
    Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(t))
}

/// Mark Claude Code's current memories as seen without importing them (the user declined).
pub fn skip_claude(vault: &Vault, from: &Path) -> Result<()> {
    let _lock = vault.lock()?;
    let mut ledger = Ledger::load(&vault.root)?.unwrap_or_default();
    for (slug, path) in sources(from)? {
        let stem = path.file_stem().unwrap().to_string_lossy().to_string();
        ledger.claude_code_skipped.insert(format!("{slug}/{stem}"), hash(&std::fs::read_to_string(&path)?));
    }
    ledger.save(&vault.root)?;
    vault.commit_paths_locked(&[LEDGER.into()], "import: skip Claude Code's existing memories")?;
    Ok(())
}

/// What [`write_back_claude`] changed in Claude Code's memory folders.
#[derive(Debug, Default, serde::Serialize)]
pub struct WriteBack {
    pub updated: Vec<String>,
    pub removed: Vec<String>,
}

/// Claude's file with hippo's description and body. Untouched when they already say the same
/// (as imported: links renamed to hippo's names).
/// `links`: how Claude refers to each memory of the project (file stem, or the frontmatter name
/// it had) → (hippo name, file stem).
fn patch_claude(text: &str, m: &Memory, links: &HashMap<String, (String, String)>) -> String {
    let (front, old_body) = match text.strip_prefix("---\n").and_then(|r| r.find("\n---\n").map(|e| e + 4 + 5)) {
        Some(end) => text.split_at(end),
        None => ("", text),
    };
    let ours = Memory::parse("", text);
    let mut as_imported = ours.body.clone();
    for (theirs, (name, _)) in links {
        as_imported = as_imported.replace(&format!("[[{theirs}]]"), &format!("[[{name}]]"));
    }
    let body = if as_imported.trim() == m.body.trim() {
        old_body.to_string()
    } else {
        let mut b = m.body.trim().to_string();
        for (name, stem) in links.values() {
            b = b.replace(&format!("[[{name}]]"), &format!("[[{stem}]]"));
        }
        let lead = &old_body[..old_body.len() - old_body.trim_start().len()];
        format!("{lead}{b}\n")
    };
    let mut front = front.to_string();
    if ours.description != m.description && !front.is_empty() {
        let desc = serde_json::to_string(&m.description.replace('\n', " ")).unwrap_or_default(); // a YAML string
        front = front.lines().map(|l| if l.starts_with("description:") { format!("description: {desc}") } else { l.to_string() }).collect::<Vec<_>>().join("\n") + "\n";
    }
    format!("{front}{body}")
}

/// Keep Claude Code's memory files in line with what the user approved in hippo, for the
/// memories hippo took from them: an edited, merged or rejected memory is rewritten, a forgotten
/// one removed along with its `MEMORY.md` line. Only files Claude hasn't changed since hippo
/// took them are touched (a newer change is imported for review first), so nothing of Claude's
/// is lost: hippo's history holds every version.
pub fn write_back_claude(vault: &Vault, from: &Path) -> Result<WriteBack> {
    let mut done = WriteBack::default();
    let _lock = vault.lock()?;
    let Some(before) = Ledger::load(&vault.root)? else { return Ok(done) };
    let existing = vault.list()?;
    let imported = imported_map(&existing);
    let names: std::collections::HashSet<&str> = existing.iter().map(|m| m.name.as_str()).collect();
    let mut ledger = before.clone();
    // Per Claude project, how its files link to each other, to compare and turn [[links]] back.
    let mut links: HashMap<String, HashMap<String, (String, String)>> = HashMap::new();
    for id in before.claude_code.keys() {
        let Some((slug, stem)) = id.split_once('/') else { continue };
        if let Some(m) = imported.get(&(slug.to_string(), slugify(stem))) {
            let l = links.entry(slug.to_string()).or_default();
            let target = (m.name.clone(), stem.to_string());
            if let Ok(t) = std::fs::read_to_string(from.join(slug).join("memory").join(format!("{stem}.md"))) {
                l.insert(Memory::parse(stem, &t).name, target.clone());
            }
            l.insert(stem.to_string(), target);
        }
    }
    for (id, h) in &before.claude_code {
        let Some((slug, stem)) = id.split_once('/') else { continue };
        let dir = from.join(slug).join("memory");
        let path = dir.join(format!("{stem}.md"));
        let Ok(text) = std::fs::read_to_string(&path) else { continue }; // another machine's, or gone
        if hash(&text) != *h {
            continue; // Claude changed it since: imported for review first
        }
        match imported.get(&(slug.to_string(), slugify(stem))) {
            Some(m) if m.needs_review() => {}
            Some(m) => {
                let next = patch_claude(&text, m, &links[slug]);
                if next != text {
                    crate::util::atomic_write(&path, next.as_bytes())?;
                    ledger.claude_code.insert(id.clone(), hash(&next));
                    done.updated.push(path.display().to_string());
                }
            }
            // Not linked any more but a memory has its name: leave it alone.
            None if names.contains(slugify(stem).as_str()) => {}
            None => {
                std::fs::remove_file(&path)?;
                let index = dir.join("MEMORY.md");
                if let Ok(t) = std::fs::read_to_string(&index) {
                    let kept: Vec<&str> = t.lines().filter(|l| !l.contains(&format!("({stem}.md)"))).collect();
                    let next = kept.join("\n") + if t.ends_with('\n') { "\n" } else { "" };
                    if next != t {
                        crate::util::atomic_write(&index, next.as_bytes())?;
                    }
                }
                ledger.claude_code.remove(id);
                done.removed.push(path.display().to_string());
            }
        }
    }
    if ledger != before {
        ledger.save(&vault.root)?;
        vault.commit_paths_locked(&[LEDGER.into()], "import: Claude Code's files follow hippo")?;
    }
    Ok(done)
}

pub fn count(plans: &[Planned], s: Status) -> usize {
    plans.iter().filter(|p| p.status == s).count()
}

/// Claude Code stores memories in `~/.claude/projects/<project-slug>/memory/*.md`
/// (one fact per file, YAML frontmatter, plus a `MEMORY.md` index we skip).
pub fn default_claude_dir() -> PathBuf {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(d) => PathBuf::from(d).join("projects"),
        None => dirs::home_dir().unwrap_or_default().join(".claude").join("projects"),
    }
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

/// `agent`: who runs the import, if not the user (then imported memories are drafts to review).
pub fn plan_and_import_claude(vault: &Vault, from: &Path, dry_run: bool, agent: Option<&str>) -> Result<(Vec<Planned>, Option<String>)> {
    let existing = vault.list()?;
    let imported = imported_map(&existing);
    let taken_by_other: HashMap<String, ()> = existing
        .iter()
        .filter(|m| m.extra.get("source").map(String::as_str) != Some("claude-code"))
        .map(|m| (m.name.clone(), ()))
        .collect();

    let mut used: HashMap<String, String> = HashMap::new(); // name → project slug
    let mut plans = vec![];
    let mut memories = vec![];
    let mut renames: BTreeMap<String, HashMap<String, String>> = BTreeMap::new();
    let mut reg = Registry::load(&vault.root)?;
    let before = Ledger::load(&vault.root)?;
    // Before the ledger, a file Claude hasn't touched since the last import is already in, or
    // was removed on purpose.
    let cutoff = if before.is_none() && !imported.is_empty() { last_import(vault) } else { None };
    let mut ledger = before.clone().unwrap_or_default();
    let mut taken = vec![]; // per source: already imported (or skipped) in this version
    let mut ids = vec![];

    for (slug, path) in sources(from)? {
        let text = std::fs::read_to_string(&path)?;
        let stem = path.file_stem().unwrap().to_string_lossy().to_string();
        let id = format!("{slug}/{stem}");
        let h = hash(&text);
        taken.push(match ledger.seen(&id) {
            Some(prev) => *prev == h,
            // Commit times are in whole seconds.
            None => cutoff.is_some_and(|t| std::fs::metadata(&path).and_then(|m| m.modified()).is_ok_and(|m| m < t + std::time::Duration::from_secs(1))),
        });
        ids.push((id, h, slug.clone(), stem.clone()));
        let secret = !taken[taken.len() - 1] && !crate::secrets::heuristic(&text).is_empty();
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
        let status = if secret { Status::Secret } else { Status::New };
        plans.push(Planned { source: path.clone(), name: name.clone(), kind: m.kind.clone(), project, renamed: name != base, status });
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
            _ if taken[i] => Status::Unchanged,
            _ if plans[i].status == Status::Secret => Status::Secret,
            None => Status::New,
            Some(p) if p.body == m.body && p.description == m.description && p.kind == m.kind && p.extra.get("visibility") == m.extra.get("visibility") => Status::Unchanged,
            Some(_) => Status::Updated,
        };
    }

    for (i, (id, h, slug, stem)) in ids.into_iter().enumerate() {
        let was_imported = match (plans[i].status, taken[i]) {
            (Status::Secret, _) => false,
            (_, false) => true,
            // Unchanged since last time: where it was, or for pre-ledger vaults, whether it's in hippo.
            (_, true) if ledger.claude_code.contains_key(&id) => true,
            (_, true) if ledger.claude_code_skipped.contains_key(&id) => false,
            (_, true) => memories[i].2.is_some() || deleted_after_import(vault, &slug, &stem),
        };
        if was_imported {
            ledger.claude_code_skipped.remove(&id);
            ledger.claude_code.insert(id, h);
        } else {
            ledger.claude_code.remove(&id);
            ledger.claude_code_skipped.insert(id, h);
        }
    }
    let fresh_ledger = before.as_ref() != Some(&ledger);
    let writes = plans.iter().any(|p| matches!(p.status, Status::New | Status::Updated));
    if dry_run || (!writes && !fresh_ledger) {
        return Ok((plans, None));
    }
    let _lock = vault.lock()?;
    ledger.save(&vault.root)?;
    if !writes {
        return Ok((plans, vault.commit_paths_locked(&[LEDGER.into()], "import: note what Claude Code memories hippo has")?));
    }
    // Merge with the registry as it is now: it may have changed since the start of the import.
    let mut fresh = Registry::load(&vault.root)?;
    fresh.merge(&reg);
    fresh.save(&vault.root)?;
    for ((_, m, prior), p) in memories.iter().zip(&plans) {
        if matches!(p.status, Status::New | Status::Updated) {
            let mut m = m.clone();
            m.stamp(prior.as_ref(), agent);
            vault.write_memory(&m)?;
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
