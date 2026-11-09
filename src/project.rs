//! Projects: a stable key (`safetensors`) for `visibility: project:<key>`, so scoped memories
//! survive renames, moves, clones and other devices.
//!
//! `projects.toml` (in the vault, synced) maps each key to how it is recognized: git root commits
//! first (the same on every clone, whatever the folder is called), then folder paths (for
//! folders that aren't repos). Whenever one matches, the others are learned, so it self-heals
//! after a `mv`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const FILE: &str = "projects.toml";

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roots: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub projects: BTreeMap<String, Project>,
}

/// What a folder looks like: its repo's top level and root commits, if it is in a git repo.
#[derive(Debug, Default, Clone)]
pub struct Ident {
    pub dir: PathBuf,
    pub toplevel: Option<PathBuf>,
    pub roots: Vec<String>,
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

impl Ident {
    pub fn of(dir: &Path) -> Ident {
        let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        let toplevel = git(&dir, &["rev-parse", "--show-toplevel"]).map(PathBuf::from);
        let roots = match toplevel {
            Some(_) => git(&dir, &["rev-list", "--max-parents=0", "HEAD"]).map(|s| s.lines().map(str::to_string).collect()).unwrap_or_default(),
            None => vec![],
        };
        Ident { dir, toplevel, roots }
    }

    /// The folder that stands for the project: the repo's top level, else the folder itself.
    pub fn home(&self) -> &Path {
        self.toplevel.as_deref().unwrap_or(&self.dir)
    }

    /// A key suggestion: the project folder's name.
    pub fn suggested_key(&self) -> String {
        crate::memory::slugify(&self.home().file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default())
    }
}

impl Registry {
    /// Fails on an unreadable file rather than starting over, which would drop every link.
    pub fn load(vault: &Path) -> Result<Registry> {
        match std::fs::read_to_string(vault.join(FILE)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Registry::default()),
            Err(e) => Err(e.into()),
            Ok(t) => toml::from_str(&t)
                .with_context(|| format!("{FILE} in your vault is unreadable (an unresolved merge?)\n  → Fix it by hand, then retry")),
        }
    }

    /// Fold in what another copy learned (e.g. one loaded before a long import).
    pub fn merge(&mut self, other: &Registry) {
        for (k, o) in &other.projects {
            let p = self.projects.entry(k.clone()).or_default();
            for r in &o.roots {
                if !p.roots.contains(r) {
                    p.roots.push(r.clone());
                }
            }
            for q in &o.paths {
                if !p.paths.contains(q) {
                    p.paths.push(q.clone());
                }
            }
        }
    }

    /// Matches `id` by root commit or by its exact folder (not just a parent folder), or has no
    /// matcher at all yet: safe to learn `id`'s identities into it.
    fn owns(&self, key: &str, id: &Ident) -> bool {
        let home = id.home().to_string_lossy();
        self.projects.get(key).is_some_and(|p| {
            p.roots.iter().any(|r| id.roots.contains(r)) || p.paths.iter().any(|q| *q == home) || (p.roots.is_empty() && p.paths.is_empty())
        })
    }

    pub fn save(&self, vault: &Path) -> Result<()> {
        crate::util::atomic_write(&vault.join(FILE), toml::to_string(self)?.as_bytes())
    }

    /// Keys this folder belongs to: by root commit, else by path (longest prefix wins).
    pub fn resolve(&self, id: &Ident) -> Vec<String> {
        let by_root: Vec<String> =
            self.projects.iter().filter(|(_, p)| p.roots.iter().any(|r| id.roots.contains(r))).map(|(k, _)| k.clone()).collect();
        if !by_root.is_empty() {
            return by_root;
        }
        let best = self
            .projects
            .iter()
            .flat_map(|(k, p)| p.paths.iter().map(move |path| (k, path)))
            .filter(|(_, path)| id.dir.starts_with(path))
            .max_by_key(|(_, path)| path.len());
        best.map(|(k, _)| vec![k.clone()]).unwrap_or_default()
    }

    /// The project this folder belongs to, registering it under its folder name if it's new.
    /// A key that exists but matches nothing (e.g. from an old `project:<name>`) is adopted.
    pub fn key_for(&mut self, id: &Ident) -> String {
        if let Some(k) = self.resolve(id).into_iter().next() {
            return k;
        }
        let base = id.suggested_key();
        let mut key = base.clone();
        for i in 2.. {
            match self.projects.get(&key) {
                Some(p) if !p.roots.is_empty() || !p.paths.is_empty() => key = format!("{base}-{i}"),
                _ => break,
            }
        }
        self.learn(&key, id);
        key
    }

    /// Remember how to recognize `key` from this folder. Returns whether anything changed.
    pub fn learn(&mut self, key: &str, id: &Ident) -> bool {
        let p = self.projects.entry(key.to_string()).or_default();
        let before = p.clone();
        for r in &id.roots {
            if !p.roots.contains(r) {
                p.roots.push(r.clone());
            }
        }
        let home = id.home().to_string_lossy().to_string();
        // Old paths stay: they may be another device's.
        if !p.paths.contains(&home) {
            p.paths.push(home);
        }
        *p != before
    }
}

/// Where an agent is working, and which projects that is.
#[derive(Debug, Default, Clone)]
pub struct Scope {
    pub dir: PathBuf,
    pub keys: Vec<String>,
}

impl Scope {
    /// Resolve `dir` against the vault's registry, learning new paths/roots for known projects.
    pub fn here(vault: &crate::vault::Vault, dir: &Path) -> Scope {
        if let Err(e) = migrate(vault) {
            eprintln!("hippo: could not migrate project scopes: {e:#}");
        }
        let id = Ident::of(dir);
        let reg = match Registry::load(&vault.root) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("hippo: {e:#}");
                return Scope { dir: id.dir, keys: vec![] }; // fail closed: no project memories
            }
        };
        let mut keys = reg.resolve(&id);
        // A folder named like a project nobody has linked yet (an old `project:<name>`): adopt it.
        let name = id.suggested_key();
        if keys.is_empty() && reg.projects.get(&name).is_some_and(|p| p.roots.is_empty() && p.paths.is_empty()) {
            keys.push(name);
        }
        // Learn a moved repo's new path, or a folder's first commit. A folder that only sits
        // inside a project's path is not learned: it may be a different project.
        let learn = |reg: &mut Registry| {
            let owned: Vec<&String> = keys.iter().filter(|k| reg.owns(k, &id)).collect();
            owned.into_iter().fold(false, |c, k| reg.learn(k, &id) | c)
        };
        if learn(&mut reg.clone()) {
            let _ = vault.update_registry("projects: learn where they live", learn);
        }
        Scope { dir: id.dir, keys }
    }

    /// Does `project:<p>` include this scope? (`p` an absolute path: a not yet migrated value.)
    pub fn includes(&self, p: &str) -> bool {
        self.keys.iter().any(|k| k.eq_ignore_ascii_case(p)) || (p.starts_with('/') && self.dir.starts_with(p))
    }
}

/// Turn folder-based scopes (`project:/abs/path`, `project:<folder name>`) into project keys.
/// Returns how many memories changed.
pub fn migrate(vault: &crate::vault::Vault) -> Result<usize> {
    use crate::memory::{Visibility, slugify};
    let marker = vault.meta_dir().join("projects-migrated");
    if marker.exists() {
        return Ok(0);
    }
    let _lock = vault.lock()?;
    let before = Registry::load(&vault.root)?;
    let mut reg = before.clone();
    let mut changed = vec![];
    for mut m in vault.list()? {
        let Visibility::Project(p) = m.visibility() else { continue };
        let key = if p.starts_with('/') && Path::new(&p).exists() {
            reg.key_for(&Ident::of(Path::new(&p)))
        } else if p.starts_with('/') {
            // Gone from this disk (moved, or another device's path): keep it as a path matcher,
            // under its own key if a live project already has that name.
            let base = slugify(Path::new(&p).file_name().map(|n| n.to_string_lossy()).unwrap_or_default().as_ref());
            let mut key = base.clone();
            for i in 2.. {
                match reg.projects.get(&key) {
                    Some(q) if !q.roots.is_empty() || q.paths.iter().any(|q| Path::new(q).exists()) => key = format!("{base}-{i}"),
                    _ => break,
                }
            }
            let e = reg.projects.entry(key.clone()).or_default();
            if !e.paths.contains(&p) {
                e.paths.push(p.clone());
            }
            key
        } else {
            let key = slugify(&p);
            reg.projects.entry(key.clone()).or_default();
            key
        };
        if key != p {
            m.extra.insert("visibility".into(), format!("project:{key}"));
            let path = vault.write_memory(&m)?;
            changed.push(path.strip_prefix(&vault.root)?.to_string_lossy().to_string());
        }
    }
    if !changed.is_empty() || reg != before {
        reg.save(&vault.root)?;
        changed.push(FILE.into());
        vault.commit_paths_locked(&changed, "projects: scope memories by project key")?;
    }
    std::fs::write(&marker, "")?;
    Ok(changed.len().saturating_sub(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(dir: &str, roots: &[&str]) -> Ident {
        Ident { dir: dir.into(), toplevel: (!roots.is_empty()).then(|| dir.into()), roots: roots.iter().map(|s| s.to_string()).collect() }
    }

    #[test]
    fn root_commit_survives_a_move() {
        let mut reg = Registry::default();
        reg.learn("safetensors", &id("/old/safetensors", &["abc"]));
        let moved = id("/new/st-renamed", &["abc"]);
        assert_eq!(reg.resolve(&moved), vec!["safetensors"]);
        assert!(reg.learn("safetensors", &moved));
        assert_eq!(reg.projects["safetensors"].paths, vec!["/old/safetensors", "/new/st-renamed"]);
        assert!(!reg.learn("safetensors", &moved));
    }

    #[test]
    fn paths_for_plain_folders() {
        let mut reg = Registry::default();
        reg.learn("notes", &id("/home/me/notes", &[]));
        assert_eq!(reg.resolve(&Ident { dir: "/home/me/notes/sub".into(), ..Default::default() }), vec!["notes"]);
        assert!(reg.resolve(&Ident { dir: "/home/me/notes-old".into(), ..Default::default() }).is_empty());
    }

    #[test]
    fn scope_keeps_legacy_values_working() {
        let s = Scope { dir: "/Users/x/work/safetensors/py".into(), keys: vec!["st".into()] };
        assert!(s.includes("st") && s.includes("/Users/x/work/safetensors"));
        assert!(!s.includes("safetensors") && !s.includes("py") && !s.includes("/Users/x/work/other"));
    }
}
