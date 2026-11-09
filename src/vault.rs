//! The local vault: a folder of Markdown files under git, plus local-only `.hippo/` metadata.
//!
//! ```text
//! <vault>/
//!   memories/*.md     canonical memories (one per file)
//!   media/            binaries, committed as-is
//!   .gitignore        ignores .hippo/
//!   .git/             history; hippo's own git config only
//!   .hippo/           local-only: vault.toml, state.json, index.sqlite, outbox/, merges/
//! ```

use crate::config::VaultConfig;
use crate::crypto::Key;
use crate::git::Git;
use crate::memory::{Memory, slugify};
use crate::util::{atomic_write, random_hex};
use anyhow::{Context, Result, bail};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

pub const FORMAT_VERSION: u32 = 1;
const GITIGNORE: &str = "# hippo: local-only metadata, never committed or uploaded\n.hippo/\n";
const README: &str = "# My memory\n\nThis folder is a [hippo](https://github.com/McPatate/hippo) vault: plain Markdown files you own.\n\n- `memories/` — one memory per file, with a small frontmatter header.\n- `media/` — images, audio and other files.\n\nIt is a regular git repository, so `git log` shows the full history.\n";

pub struct Vault {
    pub root: PathBuf,
    pub config: VaultConfig,
    pub git: Git,
}

/// Held while mutating the working tree / git state. Released on drop.
pub struct VaultLock(#[allow(dead_code)] File);

impl Vault {
    /// Create a brand-new vault at `root`. Refuses to touch a non-empty directory.
    pub fn init(root: &Path, key: &Key) -> Result<Self> {
        Git::ensure_available()?;
        if root.exists() && fs::read_dir(root)?.next().is_some() {
            bail!(
                "{} already exists and is not empty — refusing to create a vault there.\n  → Pick another path, or open the existing vault with `hippo --vault {}`",
                root.display(),
                root.display()
            );
        }
        let config = VaultConfig {
            format: FORMAT_VERSION,
            vault_id: random_hex(8),
            device_id: random_hex(6),
            recipient: key.recipient(),
            remote: None,
        };
        Self::create_layout(root, config, true)
    }

    /// Lay out a vault directory around an existing (or new) git repo.
    pub(crate) fn create_layout(root: &Path, config: VaultConfig, fresh: bool) -> Result<Self> {
        fs::create_dir_all(root.join(".hippo"))?;
        let git = Git::new(root, &config.device_id);
        if fresh {
            git.init()?;
            fs::create_dir_all(root.join("memories"))?;
            fs::create_dir_all(root.join("media"))?;
            atomic_write(&root.join(".gitignore"), GITIGNORE.as_bytes())?;
            atomic_write(&root.join("README.md"), README.as_bytes())?;
            atomic_write(&root.join("memories").join(".keep"), b"")?;
            atomic_write(&root.join("media").join(".keep"), b"")?;
        }
        config.save(root)?;
        let v = Self {
            root: root.to_path_buf(),
            config,
            git,
        };
        if fresh {
            v.git.commit_all("hippo: create vault")?;
        }
        Ok(v)
    }

    pub fn open(root: &Path) -> Result<Self> {
        let config = VaultConfig::load(root)?;
        if config.format > FORMAT_VERSION {
            bail!(
                "this vault uses format v{} but this hippo only understands v{FORMAT_VERSION}\n  → Upgrade hippo",
                config.format
            );
        }
        let git = Git::new(root, &config.device_id);
        Ok(Self {
            root: root.to_path_buf(),
            config,
            git,
        })
    }

    pub fn meta_dir(&self) -> PathBuf {
        self.root.join(".hippo")
    }

    pub fn memories_dir(&self) -> PathBuf {
        self.root.join("memories")
    }

    /// Exclusive lock on the working tree. Blocks until available.
    pub fn lock(&self) -> Result<VaultLock> {
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.meta_dir().join("vault.lock"))?;
        f.lock().context("cannot lock the vault")?;
        Ok(VaultLock(f))
    }

    pub fn memory_path(&self, name: &str) -> PathBuf {
        let stem = name.strip_suffix(".md").unwrap_or(name);
        self.memories_dir().join(format!("{}.md", slugify(stem)))
    }

    /// Write a memory file (atomically). Does not commit. Returns the file path.
    pub fn write_memory(&self, m: &Memory) -> Result<PathBuf> {
        let path = self.memory_path(&m.name);
        let mut m = m.clone();
        m.name = slugify(&m.name);
        atomic_write(&path, m.render().as_bytes())?;
        Ok(path)
    }

    pub fn read_memory(&self, name: &str) -> Result<Memory> {
        let path = self.memory_path(name);
        if !path.exists() {
            bail!("no memory named `{name}`\n  → `hippo ls` lists memories");
        }
        let text = fs::read_to_string(&path)?;
        Ok(Memory::parse(&slugify(name.strip_suffix(".md").unwrap_or(name)), &text))
    }

    pub fn list(&self) -> Result<Vec<Memory>> {
        let mut out = vec![];
        let dir = self.memories_dir();
        if !dir.exists() {
            return Ok(out);
        }
        for entry in fs::read_dir(dir)? {
            let p = entry?.path();
            if p.extension().is_some_and(|e| e == "md") {
                let stem = p.file_stem().unwrap().to_string_lossy().to_string();
                let text = fs::read_to_string(&p).unwrap_or_default();
                out.push(Memory::parse(&stem, &text));
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// Remove a memory from the working tree. History keeps it; nothing remote is touched.
    pub fn delete_memory(&self, name: &str) -> Result<bool> {
        let p = self.memory_path(name);
        if !p.exists() {
            return Ok(false);
        }
        fs::remove_file(&p)?;
        Ok(true)
    }

    /// Files that still contain conflict markers from an unfinished device merge.
    pub fn unresolved_conflicts(&self) -> Result<Vec<String>> {
        if !self.git.merge_in_progress() {
            return Ok(vec![]);
        }
        let mut out = vec![];
        for p in self.git.conflicted_paths()? {
            let text = fs::read_to_string(self.root.join(&p)).unwrap_or_default();
            if has_conflict_markers(&text) {
                out.push(p);
            }
        }
        Ok(out)
    }

    /// Commit all working-tree changes. Refuses to commit unresolved conflict markers.
    pub fn save(&self, message: &str) -> Result<Option<String>> {
        let _lock = self.lock()?;
        self.save_locked(message)
    }

    pub(crate) fn save_locked(&self, message: &str) -> Result<Option<String>> {
        let unresolved = self.unresolved_conflicts()?;
        if !unresolved.is_empty() {
            bail!(
                "cannot save: {} file(s) still have merge conflict markers: {}\n  → Edit them to keep the right content (remove the <<<<<<< / ======= / >>>>>>> lines), then save again",
                unresolved.len(),
                unresolved.join(", ")
            );
        }
        let msg = if self.git.merge_in_progress() {
            format!("{message} (resolve device merge)")
        } else {
            message.to_string()
        };
        let commit = self.git.commit_all(&crate::safety::with_agent_trailer(&msg))?;
        if let Some(c) = &commit {
            crate::log::record(&self.root, "commit", serde_json::json!({"message": msg, "commit": &c[..12]}));
        }
        Ok(commit)
    }

    /// Commit only `rel` (fast in-process path); full `save_locked` during a device merge.
    fn commit_one_locked(&self, rel: &str, message: &str) -> Result<Option<String>> {
        if self.git.merge_in_progress() {
            return self.save_locked(message);
        }
        let commit = self.git.commit_paths(&[rel.to_string()], &crate::safety::with_agent_trailer(message))?;
        if let Some(c) = &commit {
            crate::log::record(&self.root, "commit", serde_json::json!({"message": message, "commit": &c[..12]}));
        }
        Ok(commit)
    }

    fn rel(&self, p: &Path) -> String {
        p.strip_prefix(&self.root).unwrap_or(p).to_string_lossy().replace('\\', "/")
    }

    /// Write + commit in one go, as the user.
    pub fn remember(&self, m: &Memory) -> Result<Option<String>> {
        self.remember_by(m, None)
    }

    /// Write + commit, stamped with who wrote it. An agent's write is a draft until approved.
    pub fn remember_by(&self, m: &Memory, agent: Option<&str>) -> Result<Option<String>> {
        let _lock = self.lock()?;
        let mut m = m.clone();
        m.stamp(self.read_memory(&m.name).ok().as_ref(), agent);
        let rel = self.rel(&self.write_memory(&m)?);
        self.commit_one_locked(&rel, &format!("remember: {rel}"))
    }

    pub fn forget(&self, name: &str) -> Result<bool> {
        let _lock = self.lock()?;
        let rel = self.rel(&self.memory_path(name));
        if !self.delete_memory(name)? {
            return Ok(false);
        }
        self.commit_one_locked(&rel, &format!("forget: {}", slugify(name)))?;
        Ok(true)
    }
}

pub fn has_conflict_markers(text: &str) -> bool {
    text.lines().any(|l| {
        l.starts_with("<<<<<<< ") || l.starts_with(">>>>>>> ") || l == "=======" || l.starts_with("||||||| ")
    })
}
