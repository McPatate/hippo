use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;

/// Crash-safe file replacement: write to a temp file in the same directory, fsync it,
/// rename over the target, then fsync the directory. Readers see either the old or the new
/// content, never a torn write.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("path has no parent directory")?;
    fs::create_dir_all(dir)?;
    let name = path.file_name().context("path has no file name")?.to_string_lossy();
    let tmp = dir.join(format!(".{name}.tmp-{}", random_hex(6)));
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .with_context(|| format!("cannot create {}", tmp.display()))?;
        f.write_all(bytes)
            .with_context(|| format!("cannot write {} (disk full?)", tmp.display()))?;
        sync_file(&f)?;
    }
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("cannot replace {}", path.display()));
    }
    fsync_dir(dir);
    Ok(())
}

/// Flush a file to stable storage. On macOS `sync_all` issues F_FULLFSYNC (~5 ms, flushes the
/// drive cache); like SQLite and git we use plain fsync unless HIPPO_FULLFSYNC=1. Writes stay
/// atomic (temp + rename) either way; verified remote backups are the power-loss safety net.
pub fn sync_file(f: &File) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        if std::env::var_os("HIPPO_FULLFSYNC").is_none() {
            use std::os::fd::AsRawFd;
            return if unsafe { libc::fsync(f.as_raw_fd()) } == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) };
        }
    }
    f.sync_all()
}

pub fn fsync_dir(dir: &Path) {
    if let Ok(d) = File::open(dir) {
        let _ = sync_file(&d);
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

pub fn random_hex(n_bytes: usize) -> String {
    let bytes: Vec<u8> = (0..n_bytes).map(|_| rand::random::<u8>()).collect();
    hex::encode(bytes)
}

pub fn now() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now()
}

/// "3 min ago" style formatting for status output.
pub fn ago(t: chrono::DateTime<chrono::Utc>) -> String {
    let s = (now() - t).num_seconds().max(0);
    match s {
        0..=9 => "just now".into(),
        10..=59 => format!("{s}s ago"),
        60..=3599 => format!("{} min ago", s / 60),
        3600..=86399 => format!("{} h ago", s / 3600),
        _ => format!("{} days ago", s / 86400),
    }
}

/// Replace $HOME prefix by `~` for display.
pub fn pretty_path(p: &Path) -> String {
    if let Some(home) = dirs::home_dir()
        && let Ok(rest) = p.strip_prefix(&home) {
            return format!("~/{}", rest.display());
        }
    p.display().to_string()
}

pub fn expand_tilde(s: &str) -> std::path::PathBuf {
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    std::path::PathBuf::from(s)
}
