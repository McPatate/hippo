//! Locations and configuration.
//!
//! ```text
//! ~/.hippo/                 HIPPO_HOME
//!   key.txt                 vault key (age identity), chmod 600 — HIPPO_KEY overrides
//!   config.toml             global config: default vault
//!   <vault>/                default vault location: ~/.hippo/memory
//! ```

use crate::util::atomic_write;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub fn hippo_home() -> PathBuf {
    if let Ok(p) = std::env::var("HIPPO_HOME") {
        return PathBuf::from(p);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".hippo")
}

pub fn key_path() -> PathBuf {
    if let Ok(p) = std::env::var("HIPPO_KEY") {
        return PathBuf::from(p);
    }
    hippo_home().join("key.txt")
}

pub fn default_vault_path() -> PathBuf {
    hippo_home().join("memory")
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct GlobalConfig {
    pub default_vault: Option<PathBuf>,
    /// Your hippo buddy (statusline `--pixel`).
    #[serde(default)]
    pub buddy: Buddy,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Buddy {
    pub name: Option<String>,
    /// lavender (default), pink, gray, blue
    pub color: Option<String>,
}

impl GlobalConfig {
    fn path() -> PathBuf {
        hippo_home().join("config.toml")
    }

    pub fn load() -> Result<Self> {
        let p = Self::path();
        if !p.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&p)?;
        toml::from_str(&text).with_context(|| format!("invalid config file {}", p.display()))
    }

    pub fn save(&self) -> Result<()> {
        atomic_write(&Self::path(), toml::to_string_pretty(self)?.as_bytes())
    }
}

/// Where backups go.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum RemoteConfig {
    /// A Hugging Face Storage Bucket, `owner/name`.
    Hf { bucket: String },
    /// A plain directory (external drive, NAS mount, tests).
    Dir { path: PathBuf },
}

impl RemoteConfig {
    pub fn parse(s: &str) -> Result<Self> {
        if let Some(rest) = s.strip_prefix("hf://buckets/").or_else(|| s.strip_prefix("hf://")) {
            return Self::hf(rest);
        }
        if let Some(rest) = s.strip_prefix("file://") {
            return Ok(Self::Dir { path: PathBuf::from(rest) });
        }
        if s.starts_with('/') || s.starts_with('.') || s.starts_with('~') {
            return Ok(Self::Dir { path: crate::util::expand_tilde(s) });
        }
        Self::hf(s)
    }

    fn hf(s: &str) -> Result<Self> {
        let s = s.trim_matches('/');
        let parts: Vec<&str> = s.split('/').collect();
        anyhow::ensure!(
            parts.len() == 2 && parts.iter().all(|p| !p.is_empty()),
            "expected a bucket like `username/hippo`, got `{s}`"
        );
        Ok(Self::Hf { bucket: s.to_string() })
    }

    pub fn display(&self) -> String {
        match self {
            Self::Hf { bucket } => format!("hf://buckets/{bucket}"),
            Self::Dir { path } => crate::util::pretty_path(path),
        }
    }
}

/// Per-vault config, stored in `<vault>/.hippo/vault.toml` (never committed, never uploaded).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultConfig {
    pub format: u32,
    /// Stable id shared by every device of this vault.
    pub vault_id: String,
    /// Random id of this device; never reused, never derived from the hostname.
    pub device_id: String,
    /// Public key backups are encrypted to.
    pub recipient: String,
    pub remote: Option<RemoteConfig>,
}

impl VaultConfig {
    pub fn path(vault: &Path) -> PathBuf {
        vault.join(".hippo").join("vault.toml")
    }

    pub fn load(vault: &Path) -> Result<Self> {
        let p = Self::path(vault);
        let text = std::fs::read_to_string(&p).with_context(|| {
            format!(
                "{} is not a hippo vault (missing .hippo/vault.toml)\n  → Create one with `hippo init`",
                vault.display()
            )
        })?;
        toml::from_str(&text).with_context(|| format!("invalid vault config {}", p.display()))
    }

    pub fn save(&self, vault: &Path) -> Result<()> {
        atomic_write(&Self::path(vault), toml::to_string_pretty(self)?.as_bytes())
    }
}
