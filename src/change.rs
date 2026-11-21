//! Changes: what an agent proposes as one decision (a merge, a split, a rename, a rescope, an
//! edit, a removal). The user takes or leaves a change as a whole: in Claude Code, through a
//! dialog hippo shows itself, or in `hippo review`.
//!
//! A change waiting for review lives in its files (`change: <id>` in each one's frontmatter) and
//! in `changes.toml` (its title and why), both committed so other devices see it too.

use crate::memory::{Memory, Visibility, slugify};
use crate::vault::Vault;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub const FILE: &str = "changes.toml";

#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    /// Create or update a memory (its full new version).
    Write(Memory),
    /// The same memory under a new name.
    Rename { from: String, to: String },
    /// Who can see it: `all`, `private` or `project:<key>`.
    Scope { name: String, visibility: String },
    /// Delete it; `into` is the memory that now holds its content, if any.
    Remove { name: String, into: Option<String> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub title: String,
    pub why: String,
    pub ops: Vec<Op>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Meta {
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub why: String,
    #[serde(default)]
    pub by: String,
    #[serde(default)]
    pub at: String,
}

fn load(vault: &Path) -> Result<BTreeMap<String, Meta>> {
    match std::fs::read_to_string(vault.join(FILE)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(e.into()),
        Ok(t) => toml::from_str(&t).with_context(|| format!("{FILE} in your vault is unreadable (an unresolved merge?)\n  → Fix it by hand, then retry")),
    }
}

/// Write `changes.toml` without the changes no file belongs to anymore; `None` if unchanged.
fn save(vault: &Vault, mut ledger: BTreeMap<String, Meta>) -> Result<Option<String>> {
    let live: BTreeSet<String> = vault.list()?.iter().filter_map(|m| m.extra.get("change").cloned()).collect();
    ledger.retain(|id, _| live.contains(id));
    let path = vault.root.join(FILE);
    if ledger == load(&vault.root)? {
        return Ok(None);
    }
    if ledger.is_empty() {
        let _ = std::fs::remove_file(&path);
    } else {
        crate::util::atomic_write(&path, toml::to_string(&ledger)?.as_bytes())?;
    }
    Ok(Some(FILE.to_string()))
}

fn check_visibility(v: &str) -> Result<String> {
    match v.trim() {
        "all" | "private" => Ok(v.trim().to_string()),
        p if p.starts_with("project:") && p.len() > 8 => Ok(format!("project:{}", slugify(&p[8..]))),
        other => bail!("visibility `{other}`: use all, private or project:<key>"),
    }
}

fn set_visibility(m: &mut Memory, v: &str) {
    match v {
        "all" => drop(m.extra.remove("visibility")),
        v => drop(m.extra.insert("visibility".into(), v.into())),
    }
}

/// One line per memory a change touches, written by hippo from the files (never from the agent's
/// words), so it is what a dialog or `hippo review` can trust. A `pending` change is compared
/// with what the user last approved, a proposal with the files as they are.
pub fn describe(vault: &Vault, ops: &[Op], pending: bool) -> Vec<String> {
    let before = |name: &str| -> Option<Memory> {
        if pending {
            vault.approved_version(name).ok().flatten().map(|t| Memory::parse(&slugify(name), &t))
        } else {
            vault.read_memory(name).ok()
        }
    };
    let vis = |m: &Memory| match m.visibility() {
        Visibility::All => "everywhere".to_string(),
        Visibility::Private => "private".to_string(),
        Visibility::Project(p) => p,
    };
    ops.iter()
        .map(|op| match op {
            Op::Write(m) => match before(&m.name) {
                None => format!("+ new {} [{}], seen {}", slugify(&m.name), m.kind, vis(m)),
                Some(old) => {
                    let mut what = vec![];
                    if old.body.trim() != m.body.trim() || old.description != m.description || old.kind != m.kind {
                        what.push("content".to_string());
                    }
                    if old.visibility() != m.visibility() {
                        what.push(format!("seen {} → {}", vis(&old), vis(m)));
                    }
                    format!("✎ edit {} ({})", old.name, if what.is_empty() { "unchanged".into() } else { what.join(", ") })
                }
            },
            Op::Rename { from, to } => format!("→ rename {} → {} (same content)", slugify(from), slugify(to)),
            Op::Scope { name, visibility } => {
                let old = vault.read_memory(name).map(|m| vis(&m)).unwrap_or_default();
                let mut m = Memory::default();
                set_visibility(&mut m, visibility);
                format!("◎ {}: seen {} → {}", slugify(name), old, vis(&m))
            }
            Op::Remove { name, into: Some(into) } => format!("✗ delete {} (its content is in {})", slugify(name), slugify(into)),
            Op::Remove { name, into: None } => format!("✗ delete {}", slugify(name)),
        })
        .collect()
}

/// `describe` lines counted by kind, e.g. "10 renames · 9 deletions (kept in history)": short
/// enough for a dialog, which shows only its first lines.
pub fn tally(lines: &[String]) -> String {
    let kind = |l: &String| match l.chars().next() {
        Some('→') => "rename",
        Some('◎') => "rescope",
        Some('✗') => "deletion",
        Some('+') => "new",
        _ if l.contains("(seen ") => "rescope",
        _ => "edit",
    };
    let mut out = vec![];
    for k in ["rename", "rescope", "edit", "new", "deletion"] {
        match lines.iter().filter(|l| kind(l) == k).count() {
            0 => {}
            n => out.push(format!("{n} {}{}", if k == "new" { "new" } else { k }, if n > 1 && k != "new" { "s" } else { "" })),
        }
    }
    let mut t = out.join(" · ");
    if lines.iter().any(|l| kind(l) == "deletion") {
        t += " (kept in history)";
    }
    t
}

impl Vault {
    /// Apply `changes` in one commit: as drafts for the user to review, or `approved`, when the
    /// user said yes in a dialog hippo showed them. Checks every change before touching a file.
    pub fn apply_changes(&self, changes: &[Change], agent: &str, approved: bool) -> Result<Option<String>> {
        let _lock = self.lock()?;
        let exists = |n: &str| self.memory_path(n).exists();
        let mut touched = BTreeSet::new();
        let mut written = BTreeSet::new();
        for op in changes.iter().flat_map(|c| &c.ops) {
            let names = match op {
                Op::Write(m) => {
                    written.insert(slugify(&m.name));
                    vec![m.name.clone()]
                }
                Op::Rename { from, to } => {
                    if exists(to) {
                        bail!("cannot rename {from} to {to}: {to} already exists (merge them instead)");
                    }
                    written.insert(slugify(to));
                    vec![from.clone(), to.clone()]
                }
                Op::Scope { name, visibility } => {
                    check_visibility(visibility)?;
                    vec![name.clone()]
                }
                Op::Remove { name, .. } => vec![name.clone()],
            };
            for n in names {
                if !touched.insert(slugify(&n)) {
                    bail!("{n} appears in more than one change; each memory can only be changed once at a time");
                }
            }
            match op {
                Op::Rename { from: n, .. } | Op::Scope { name: n, .. } | Op::Remove { name: n, .. } if !exists(n) => bail!("no memory named `{n}`"),
                Op::Remove { into: Some(i), .. } if !exists(i) && !written.contains(&slugify(i)) => bail!("no memory named `{i}` to hold the content"),
                _ => {}
            }
        }

        let mut ledger = load(&self.root)?;
        let mut paths = vec![];
        let mut put = |m: &mut Memory, id: &str, review: &[(&str, &str)]| -> Result<()> {
            if approved {
                m.settle();
            } else {
                m.extra.insert("change".into(), id.into());
                for (k, v) in review {
                    m.extra.insert(k.to_string(), v.to_string());
                }
            }
            paths.push(self.rel(&self.write_memory(m)?));
            Ok(())
        };
        let mut gone = vec![];
        for c in changes {
            let id = crate::util::random_hex(3);
            if !approved {
                ledger.insert(id.clone(), Meta { title: c.title.clone(), why: c.why.clone(), by: agent.into(), at: crate::util::now().to_rfc3339() });
            }
            for op in &c.ops {
                match op {
                    Op::Write(m) => {
                        let mut m = Memory { name: slugify(&m.name), ..m.clone() };
                        let prev = self.read_memory(&m.name).ok();
                        m.stamp(prev.as_ref(), Some(agent));
                        put(&mut m, &id, &[])?;
                    }
                    Op::Rename { from, to } => {
                        let mut old = self.read_memory(from)?;
                        let mut new = Memory { name: slugify(to), ..old.clone() };
                        new.stamp(None, Some(agent));
                        put(&mut new, &id, &[("renamed_from", &old.name)])?;
                        if approved {
                            gone.push(old.name.clone());
                        } else {
                            old.extra.insert("by".into(), agent.into());
                            put(&mut old, &id, &[("status", "forget"), ("into", &new.name)])?;
                        }
                    }
                    Op::Scope { name, visibility } => {
                        let prev = self.read_memory(name)?;
                        let mut m = prev.clone();
                        set_visibility(&mut m, &check_visibility(visibility)?);
                        m.stamp(Some(&prev), Some(agent));
                        put(&mut m, &id, &[])?;
                    }
                    Op::Remove { name, into } => {
                        let mut m = self.read_memory(name)?;
                        if approved {
                            gone.push(m.name.clone());
                        } else {
                            m.extra.insert("by".into(), agent.into());
                            let into = into.as_deref().map(slugify);
                            let mut review = vec![("status", "forget")];
                            review.extend(into.as_deref().map(|i| ("into", i)));
                            put(&mut m, &id, &review)?;
                        }
                    }
                }
            }
        }
        for n in gone {
            self.delete_memory(&n)?;
            paths.push(self.rel(&self.memory_path(&n)));
        }
        paths.extend(save(self, ledger)?);
        let titles: Vec<&str> = changes.iter().map(|c| c.title.as_str()).collect();
        let msg = format!("change: {}{}", titles.join("; "), if approved { " (approved in a dialog)" } else { " (pending review)" });
        self.commit_paths_locked(&paths, &msg)
    }

    /// Changes waiting for the user, oldest first. Drafts written outside a change (plain
    /// `remember`/`forget`) are one change each, except a removal and a new memory with the same
    /// content, shown as the rename it was.
    pub fn pending_changes(&self) -> Result<Vec<Pending>> {
        self.group_pending(true)
    }

    /// How many changes wait for the user, without the git lookups that title them.
    pub fn pending_count(&self) -> Result<usize> {
        Ok(self.group_pending(false)?.len())
    }

    fn group_pending(&self, titled: bool) -> Result<Vec<Pending>> {
        let ledger = load(&self.root)?;
        let mut grouped: BTreeMap<String, Vec<Memory>> = BTreeMap::new();
        let mut loose = vec![];
        for m in self.pending_review()? {
            match m.extra.get("change").filter(|id| ledger.contains_key(*id)) {
                Some(id) => grouped.entry(id.clone()).or_default().push(m),
                None => loose.push(m),
            }
        }
        let mut out: Vec<Pending> = grouped.into_iter().map(|(id, members)| Pending { meta: ledger[&id].clone(), id: Some(id), members }).collect();
        let mut paired = BTreeSet::new();
        for old in loose.iter().filter(|m| m.status() == Some("forget")) {
            let twin = loose.iter().find(|n| n.status() == Some("draft") && !paired.contains(&n.name) && n.body.trim() == old.body.trim() && n.name != old.name);
            if let Some(new) = twin {
                paired.extend([old.name.clone(), new.name.clone()]);
                let mut new = new.clone();
                new.extra.insert("renamed_from".into(), old.name.clone());
                let mut old = old.clone();
                old.extra.insert("into".into(), new.name.clone());
                let meta = Meta { title: format!("rename {} → {}", old.name, new.name), by: new.extra.get("by").cloned().unwrap_or_default(), ..Default::default() };
                out.push(Pending { id: None, meta, members: vec![new, old] });
            }
        }
        for m in loose.into_iter().filter(|m| !paired.contains(&m.name)) {
            let title = match m.status() {
                Some("forget") => format!("delete {}", m.name),
                _ if !titled => format!("change {}", m.name),
                _ if self.approved_version(&m.name)?.is_some() => format!("edit {}", m.name),
                _ => format!("new {}", m.name),
            };
            let meta = Meta { title, by: m.extra.get("by").cloned().unwrap_or_default(), ..Default::default() };
            out.push(Pending { id: None, meta, members: vec![m] });
        }
        out.sort_by_key(|p| p.members.iter().filter_map(Memory::updated).min());
        Ok(out)
    }

    /// A pending change as a git diff against what the user last approved: renames show as
    /// renames, new and deleted memories as such, a rescope as its `visibility` line.
    pub fn diff_change(&self, p: &Pending, color: bool) -> Result<String> {
        let dir = self.meta_dir().join("review");
        let _ = std::fs::remove_dir_all(&dir);
        for m in &p.members {
            // A removal shows what goes, even a draft that was never approved.
            let before = match self.approved_version(&m.name)? {
                Some(t) => Some(Memory::parse(&m.name, &t)),
                None if m.status() == Some("forget") => Some(m.clone()),
                None => None,
            };
            if let Some(b) = before {
                let path = dir.join("a/memories").join(format!("{}.md", m.name));
                std::fs::create_dir_all(path.parent().unwrap())?;
                std::fs::write(path, b.content().render())?;
            }
            if m.status() != Some("forget") {
                let path = dir.join("b/memories").join(format!("{}.md", m.name));
                std::fs::create_dir_all(path.parent().unwrap())?;
                std::fs::write(path, m.content().render())?;
            }
        }
        for side in ["a", "b"] {
            std::fs::create_dir_all(dir.join(side))?;
        }
        let out = std::process::Command::new("git")
            .current_dir(&dir)
            .args(["diff", "--no-index", "--no-prefix", "-M", if color { "--color=always" } else { "--color=never" }, "a", "b"])
            .output()?;
        let _ = std::fs::remove_dir_all(&dir);
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Take a pending change as a whole (one commit).
    pub fn approve_change(&self, p: &Pending) -> Result<Option<String>> {
        self.decide(p, true)
    }

    /// Undo a pending change as a whole: every memory goes back to how the user last approved it.
    pub fn reject_change(&self, p: &Pending) -> Result<Option<String>> {
        self.decide(p, false)
    }

    fn decide(&self, p: &Pending, yes: bool) -> Result<Option<String>> {
        let approved: Vec<Option<String>> = if yes { vec![] } else { p.members.iter().map(|m| self.approved_version(&m.name)).collect::<Result<_>>()? };
        let _lock = self.lock()?;
        let mut paths = vec![];
        for (i, m) in p.members.iter().enumerate() {
            let path = self.memory_path(&m.name);
            let now = self.read_memory(&m.name)?;
            if now.extra.get("status") != m.extra.get("status") || now.body != m.body {
                bail!("{} changed while you were reviewing; nothing was applied (run the review again)", m.name);
            }
            match (yes, m.status()) {
                (true, Some("forget")) => std::fs::remove_file(&path)?,
                (true, _) => {
                    let mut now = now;
                    now.settle();
                    self.write_memory(&now)?;
                }
                (false, _) => match &approved[i] {
                    Some(text) => crate::util::atomic_write(&path, text.as_bytes())?,
                    None => std::fs::remove_file(&path)?,
                },
            }
            paths.push(self.rel(&path));
        }
        paths.extend(save(self, load(&self.root)?)?);
        self.commit_paths_locked(&paths, &format!("{}: {}", if yes { "approve" } else { "reject" }, p.meta.title))
    }
}

/// A change waiting for review, with the files it touches.
#[derive(Debug, Clone)]
pub struct Pending {
    /// `None` for drafts written outside a change.
    pub id: Option<String>,
    pub meta: Meta,
    pub members: Vec<Memory>,
}

impl Pending {
    /// Reviewable from `scope`: each memory is seen there now or was before the change, so a
    /// memory moved to another project can still be decided where it came from.
    pub fn visible_in(&self, vault: &Vault, scope: &crate::project::Scope) -> bool {
        self.members.iter().all(|m| {
            m.visible_in(scope) || vault.approved_version(&m.name).ok().flatten().is_some_and(|t| Memory::parse(&m.name, &t).visible_in(scope))
        })
    }

    /// Where its memories are seen: project keys, `everywhere` or `private`.
    pub fn places(&self) -> BTreeSet<String> {
        self.members
            .iter()
            .map(|m| match m.visibility() {
                Visibility::All => "everywhere".into(),
                Visibility::Private => "private".into(),
                Visibility::Project(p) => p,
            })
            .collect()
    }

    /// The change as operations, rebuilt from its files (for `describe`).
    pub fn ops(&self) -> Vec<Op> {
        let mut ops = vec![];
        for m in &self.members {
            let renamed = m.extra.get("renamed_from");
            match (m.status(), renamed, m.extra.get("into")) {
                (_, Some(from), _) => ops.push(Op::Rename { from: from.clone(), to: m.name.clone() }),
                (Some("forget"), _, Some(into)) if self.members.iter().any(|n| n.extra.get("renamed_from") == Some(&m.name) && &n.name == into) => {}
                (Some("forget"), _, into) => ops.push(Op::Remove { name: m.name.clone(), into: into.cloned() }),
                _ => ops.push(Op::Write(m.clone())),
            }
        }
        ops
    }
}
