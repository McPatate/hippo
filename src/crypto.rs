//! Vault encryption key: a standard age X25519 identity.
//!
//! The key file is a regular age identity file, so `age -d -i key.txt` can decrypt any hippo
//! backup object without hippo. It is never stored inside a vault and never uploaded.

use age::secrecy::ExposeSecret;
use age::x25519;
use anyhow::{Context, Result, bail};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;

pub struct Key {
    identity: x25519::Identity,
    path: PathBuf,
}

impl Key {
    /// Generate a new key and write it to `path`. Never overwrites an existing key.
    pub fn generate(path: &Path) -> Result<Self> {
        if path.exists() {
            bail!(
                "a key already exists at {} — refusing to overwrite it",
                path.display()
            );
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let identity = x25519::Identity::generate();
        let contents = format!(
            "# hippo vault key — created {}\n# public key: {}\n# KEEP THIS SECRET. Anyone with this file can read your backups.\n# Losing it means losing access to your backups.\n{}\n",
            crate::util::now().to_rfc3339(),
            identity.to_public(),
            identity.to_string().expose_secret()
        );
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts
            .open(path)
            .with_context(|| format!("cannot create key file {}", path.display()))?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
        crate::util::fsync_dir(path.parent().unwrap_or(Path::new(".")));
        Ok(Self {
            identity,
            path: path.to_path_buf(),
        })
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| {
            format!(
                "cannot read the vault key at {}\n  → If you moved it, set HIPPO_KEY=/path/to/key.txt",
                path.display()
            )
        })?;
        Self::parse(&text, path)
    }

    pub fn parse(text: &str, path: &Path) -> Result<Self> {
        let line = text
            .lines()
            .map(str::trim)
            .find(|l| l.starts_with("AGE-SECRET-KEY-"))
            .context("no AGE-SECRET-KEY line found in key file")?;
        let identity = x25519::Identity::from_str(line)
            .map_err(|e| anyhow::anyhow!("invalid key file {}: {e}", path.display()))?;
        Ok(Self {
            identity,
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Public half of the key, e.g. `age1...`. Safe to share and store anywhere.
    pub fn recipient(&self) -> String {
        self.identity.to_public().to_string()
    }

    pub fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        age::encrypt(&self.identity.to_public(), plaintext).context("encryption failed")
    }

    pub fn decrypt(&self, ciphertext: &[u8]) -> Result<Vec<u8>> {
        age::decrypt(&self.identity, ciphertext)
            .context("decryption failed: wrong key, or the object is corrupted")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_no_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("key.txt");
        let k = Key::generate(&p).unwrap();
        let c = k.encrypt(b"hello hippo").unwrap();
        assert_ne!(&c[..], b"hello hippo");
        let k2 = Key::load(&p).unwrap();
        assert_eq!(k2.decrypt(&c).unwrap(), b"hello hippo");
        assert!(Key::generate(&p).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn wrong_key_fails() {
        let dir = tempfile::tempdir().unwrap();
        let a = Key::generate(&dir.path().join("a")).unwrap();
        let b = Key::generate(&dir.path().join("b")).unwrap();
        let c = a.encrypt(b"secret").unwrap();
        assert!(b.decrypt(&c).is_err());
    }
}
