//! Projects: a stable key (`safetensors`) for `visibility: project:<key>`, so scoped memories
//! survive renames, moves, clones and other devices.
//!
//! `projects.toml` (in the vault, synced) maps each key to how it is recognized: git root commits
//! first (the same on every clone, whatever the folder is called), then folder paths (for
//! folders that aren't repos). Whenever one matches, the others are learned, so it self-heals
//! after a `mv`.

use anyhow::Result;
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
    pub fn load(vault: &Path) -> Registry {
        std::fs::read_to_string(vault.join(FILE)).ok().and_then(|t| toml::from_str(&t).ok()).unwrap_or_default()
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
        let id = Ident::of(dir);
        let mut reg = Registry::load(&vault.root);
        let keys = reg.resolve(&id);
        let changed = keys.iter().fold(false, |c, k| reg.learn(k, &id) | c);
        if changed {
            let _ = vault.save_registry(&reg, "projects: learn where they live");
        }
        Scope { dir: id.dir, keys }
    }

    /// Does `project:<p>` include this scope? Keys first; legacy path/folder-name values still work.
    pub fn includes(&self, p: &str) -> bool {
        if self.keys.iter().any(|k| k.eq_ignore_ascii_case(p)) {
            return true;
        }
        if p.starts_with('/') {
            self.dir.starts_with(p)
        } else {
            self.dir.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(p))
        }
    }
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
        assert!(s.includes("st") && s.includes("/Users/x/work/safetensors") && s.includes("py"));
        assert!(!s.includes("safetensors") && !s.includes("other"));
    }
}
