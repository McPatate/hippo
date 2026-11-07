//! Local backup state, `<vault>/.hippo/state.json`. Written atomically after every transition.
//!
//! The authoritative "last verified commit" is the git ref `refs/hippo/verified`; this file
//! tracks sequence numbers, timestamps and the in-flight (pending) upload.

use crate::util::atomic_write;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    /// Highest bundle sequence number of this device that passed verification.
    pub verified_seq: u64,
    pub verified_at: Option<DateTime<Utc>>,
    /// Highest bundle sequence number whose upload call returned success.
    pub uploaded_seq: u64,
    pub uploaded_at: Option<DateTime<Utc>>,
    /// A bundle that was prepared but not yet verified. Its encrypted bytes live in the outbox,
    /// so a retry re-sends exactly the same object.
    pub pending: Option<Pending>,
    /// Last bundle fetched from every other device of this vault.
    #[serde(default)]
    pub peers: BTreeMap<String, PeerState>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Pending {
    pub seq: u64,
    pub key: String,
    pub sha256: String,
    pub tip: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PeerState {
    pub fetched_seq: u64,
    pub tip: Option<String>,
}

impl State {
    pub fn path(vault: &Path) -> PathBuf {
        vault.join(".hippo").join("state.json")
    }

    pub fn load(vault: &Path) -> Result<Self> {
        let p = Self::path(vault);
        if !p.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&p)?;
        serde_json::from_str(&text).with_context(|| format!("corrupted state file {}", p.display()))
    }

    pub fn save(&self, vault: &Path) -> Result<()> {
        atomic_write(&Self::path(vault), serde_json::to_string_pretty(self)?.as_bytes())
    }
}
