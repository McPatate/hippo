//! Isolated git runner.
//!
//! hippo shells out to the `git` CLI (bundles + merges are first-class there), but the user's
//! git configuration must never leak in: no global/system config, no hooks, no signing, no
//! inherited `GIT_*` environment. Everything hippo needs is passed explicitly below.
//! If we later want to re-allow some user settings, add them to an explicit allowlist here.

use anyhow::{Context, Result, bail};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

#[cfg(unix)]
const NULL_DEVICE: &str = "/dev/null";
#[cfg(windows)]
const NULL_DEVICE: &str = "NUL";

/// Config forced on every git invocation. Order matters only for readability.
const FORCED_CONFIG: &[&str] = &[
    "commit.gpgsign=false",
    "tag.gpgsign=false",
    "core.hooksPath=/dev/null",
    "init.defaultBranch=main",
    "merge.conflictStyle=diff3",
    "core.autocrlf=false",
    "core.quotePath=false",
    "core.fsmonitor=false",
    "core.pager=cat",
    "advice.detachedHead=false",
    "user.useConfigOnly=false",
];

#[derive(Debug, Clone)]
pub struct Git {
    dir: PathBuf,
    author: String,
    email: String,
}

/// Result of a git command that is allowed to fail (e.g. a merge with conflicts).
pub struct GitOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

impl Git {
    pub fn new(dir: impl Into<PathBuf>, device_id: &str) -> Self {
        Self {
            dir: dir.into(),
            author: "hippo".into(),
            email: format!("{device_id}@hippo.local"),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn ensure_available() -> Result<()> {
        let out = Command::new("git").arg("--version").output();
        match out {
            Ok(o) if o.status.success() => Ok(()),
            _ => bail!(
                "git is not installed or not on PATH.\n  hippo uses git to version your memories.\n  → Install it (macOS: `xcode-select --install`, Debian/Ubuntu: `apt install git`) and retry."
            ),
        }
    }

    fn command(&self) -> Command {
        let mut c = Command::new("git");
        c.current_dir(&self.dir);
        for (k, _) in std::env::vars_os() {
            if k.to_string_lossy().starts_with("GIT_") {
                c.env_remove(&k);
            }
        }
        c.env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", NULL_DEVICE)
            .env("GIT_CONFIG_SYSTEM", NULL_DEVICE)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_AUTHOR_NAME", &self.author)
            .env("GIT_AUTHOR_EMAIL", &self.email)
            .env("GIT_COMMITTER_NAME", &self.author)
            .env("GIT_COMMITTER_EMAIL", &self.email)
            .env("LC_ALL", "C")
            .stdin(Stdio::null());
        for cfg in FORCED_CONFIG {
            c.arg("-c").arg(cfg);
        }
        c
    }

    fn exec<I, S>(&self, args: I) -> Result<(Vec<String>, Output)>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<String> = args
            .into_iter()
            .map(|a| a.as_ref().to_string_lossy().into_owned())
            .collect();
        let out = self
            .command()
            .args(&args)
            .output()
            .with_context(|| format!("failed to run git {}", args.join(" ")))?;
        Ok((args, out))
    }

    /// Run git and fail with stderr on a non-zero exit.
    pub fn run<I, S>(&self, args: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let (args, out) = self.exec(args)?;
        if !out.status.success() {
            bail!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Run git, returning success/failure instead of erroring.
    pub fn try_run<I, S>(&self, args: I) -> Result<GitOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let (_, out) = self.exec(args)?;
        Ok(GitOutput {
            success: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    pub fn init(&self) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        self.run(["init", "-q", "-b", "main"])?;
        Ok(())
    }

    pub fn rev_parse(&self, rev: &str) -> Result<Option<String>> {
        let out = self.try_run(["rev-parse", "-q", "--verify", &format!("{rev}^{{commit}}")])?;
        Ok(out.success.then(|| out.stdout.trim().to_string()))
    }

    pub fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool> {
        Ok(self
            .try_run(["merge-base", "--is-ancestor", ancestor, descendant])?
            .success)
    }

    pub fn update_ref(&self, name: &str, commit: &str) -> Result<()> {
        self.run(["update-ref", name, commit])?;
        Ok(())
    }

    /// `git add -A` + commit. Returns the new commit id, or None if nothing changed.
    /// Two git spawns (add + commit); HEAD is read from the ref file when possible.
    pub fn commit_all(&self, message: &str) -> Result<Option<String>> {
        self.run(["add", "-A"])?;
        let out = self.try_run(["commit", "-q", "-m", message])?;
        if !out.success {
            let text = format!("{}{}", out.stdout, out.stderr);
            if self.head_fast()?.is_none() {
                self.run(["commit", "-q", "--allow-empty", "-m", message])?; // very first commit
            } else if text.contains("nothing to commit") || text.contains("nothing added to commit") {
                return Ok(None);
            } else {
                bail!("git commit failed: {}", text.trim());
            }
        }
        self.head_fast()
    }

    /// Fast in-process commit of specific paths (gix): ~1–3 ms instead of ~45 ms of git spawns.
    /// Commits only `paths` (repo-relative); missing files are recorded as deletions.
    /// Falls back to `commit_all` on anything unusual (merge in progress, unborn branch, errors).
    pub fn commit_paths(&self, paths: &[String], message: &str) -> Result<Option<String>> {
        if self.merge_in_progress() {
            return self.commit_all(message);
        }
        match self.commit_paths_gix(paths, message) {
            Ok(r) => Ok(r),
            Err(_) => self.commit_all(message),
        }
    }

    fn commit_paths_gix(&self, paths: &[String], message: &str) -> Result<Option<String>> {
        use gix::bstr::ByteSlice;
        use gix::objs::tree::EntryKind;
        let repo = gix::open_opts(&self.dir, gix::open::Options::isolated())?;
        let head = repo.head_commit()?;
        let head_tree = head.tree_id()?.detach();
        let mut editor = repo.edit_tree(head_tree)?;
        let mut index = repo.open_index()?;
        for rel in paths {
            let abs = self.dir.join(rel);
            let path = rel.as_bytes().as_bstr();
            if abs.is_file() {
                let blob = repo.write_blob(std::fs::read(&abs)?)?.detach();
                editor.upsert(rel.as_str(), EntryKind::Blob, blob)?;
                let stat = gix::index::entry::Stat::from_fs(&gix::index::fs::Metadata::from_path_no_follow(&abs)?)?;
                match index.entry_mut_by_path_and_stage(path, gix::index::entry::Stage::Unconflicted) {
                    Some(e) => {
                        e.id = blob;
                        e.stat = stat;
                    }
                    None => {
                        index.dangerously_push_entry(
                            stat,
                            blob,
                            gix::index::entry::Flags::empty(),
                            gix::index::entry::Mode::FILE,
                            path,
                        );
                        index.sort_entries();
                    }
                }
            } else {
                editor.remove(rel.as_str())?;
                index.remove_entries(|_, p, _| p == path);
            }
        }
        let tree = editor.write()?.detach();
        if tree == head_tree {
            return Ok(None);
        }
        let sig = gix::actor::Signature {
            name: self.author.as_str().into(),
            email: self.email.as_str().into(),
            time: gix::date::Time::now_local_or_utc(),
        };
        let mut buf = gix::date::parse::TimeBuf::default();
        let sig_ref = sig.to_ref(&mut buf);
        let id = repo.commit_as(sig_ref, sig_ref, "HEAD", message, tree, [head.id])?;
        // The cached tree extension is now stale; drop it so git rebuilds trees from entries.
        index.remove_tree();
        index.write(Default::default())?;
        Ok(Some(id.to_string()))
    }

    /// Resolve HEAD without spawning git when it is a loose ref; fall back to rev-parse.
    pub fn head_fast(&self) -> Result<Option<String>> {
        let git_dir = self.dir.join(".git");
        if let Ok(head) = std::fs::read_to_string(git_dir.join("HEAD"))
            && let Some(r) = head.trim().strip_prefix("ref: ")
                && let Ok(id) = std::fs::read_to_string(git_dir.join(r)) {
                    let id = id.trim();
                    if id.len() >= 40 && id.chars().all(|c| c.is_ascii_hexdigit()) {
                        return Ok(Some(id.to_string()));
                    }
                }
        self.rev_parse("HEAD")
    }

    /// Paths with unresolved merge conflicts.
    pub fn conflicted_paths(&self) -> Result<Vec<String>> {
        let out = self.run(["diff", "--name-only", "--diff-filter=U"])?;
        Ok(out.lines().map(str::to_string).filter(|l| !l.is_empty()).collect())
    }

    pub fn merge_in_progress(&self) -> bool {
        self.dir.join(".git").join("MERGE_HEAD").exists()
    }

    /// Number of working-tree entries that differ from HEAD (uncommitted changes).
    pub fn dirty_count(&self) -> Result<usize> {
        let out = self.run(["status", "--porcelain", "--untracked-files=all"])?;
        Ok(out.lines().filter(|l| !l.is_empty()).count())
    }

    pub fn count_commits(&self, range: &str) -> Result<usize> {
        let out = self.run(["rev-list", "--count", range])?;
        Ok(out.trim().parse().unwrap_or(0))
    }
}
