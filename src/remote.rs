//! Dumb object storage for encrypted backup objects.
//!
//! Only three operations: create-only put, get, list. There is deliberately no delete here;
//! destructive maintenance lives in `hippo compact` behind a human-only guard.
//!
//! Remote layout (everything except `vault.json` is age-encrypted):
//! ```text
//! hippo/v1/<vault_id>/vault.json                                   write-once, public metadata
//! hippo/v1/<vault_id>/devices/<device_id>/<seq:08>-<sha16>.bundle.age   immutable
//! ```

use crate::config::RemoteConfig;
use crate::util::{atomic_write, sha256_hex};
use anyhow::{Context, Result, bail};
use futures::StreamExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub const ROOT: &str = "hippo/v1";

#[derive(Debug, Clone)]
pub struct Entry {
    pub key: String,
    pub size: u64,
}

/// Failure injection for tests: each flag fires once (on bundle objects), then resets.
#[derive(Debug, Default)]
pub struct Faults {
    /// Store the object, then report a failure (lost response).
    pub put_fails_after_store: bool,
    /// Report a failure without storing anything.
    pub put_fails_before_store: bool,
    /// Flip a byte in the next object returned by `get`.
    pub corrupt_next_get: bool,
    /// Fail the next `get`.
    pub get_fails: bool,
}

#[derive(Clone)]
pub struct Remote {
    backend: Backend,
    pub faults: Arc<Mutex<Faults>>,
}

#[derive(Clone)]
enum Backend {
    Dir(PathBuf),
    Hf(hf_hub::HFBucket),
}

impl Remote {
    pub fn open(cfg: &RemoteConfig) -> Result<Self> {
        let backend = match cfg {
            RemoteConfig::Dir { path } => Backend::Dir(path.clone()),
            RemoteConfig::Hf { bucket } => {
                let (owner, name) = bucket.split_once('/').context("bucket must be owner/name")?;
                let client = crate::auth::hf_client()?;
                Backend::Hf(client.bucket(owner, name))
            }
        };
        Ok(Self {
            backend,
            faults: Arc::default(),
        })
    }

    fn take_fault(&self, key: &str, f: impl FnOnce(&mut Faults) -> &mut bool) -> bool {
        if !key.ends_with(".bundle.age") {
            return false; // faults target backup objects only
        }
        let mut faults = self.faults.lock().unwrap();
        std::mem::take(f(&mut faults))
    }

    /// Store `bytes` at `key` unless something is already there.
    /// If an identical object already exists (a retried upload), this succeeds.
    /// If a *different* object exists, this fails: hippo never overwrites remote data.
    pub async fn put_new(&self, key: &str, bytes: &[u8]) -> Result<()> {
        validate_key(key)?;
        if self.exists(key).await? {
            let existing = self.get(key).await?;
            if sha256_hex(&existing) == sha256_hex(bytes) {
                return Ok(());
            }
            bail!("remote object {key} already exists with different content — refusing to overwrite");
        }
        if self.take_fault(key, |f| &mut f.put_fails_before_store) {
            bail!("injected fault: upload failed before storing");
        }
        match &self.backend {
            Backend::Dir(root) => {
                let path = root.join(key);
                atomic_write(&path, bytes)?;
            }
            Backend::Hf(bucket) => {
                bucket
                    .upload_source_files()
                    .files(vec![(
                        key.to_string(),
                        hf_hub::repository::AddSource::bytes(bytes.to_vec()),
                    )])
                    .send()
                    .await
                    .map_err(|e| anyhow::anyhow!("upload to Hugging Face failed: {e}"))?;
            }
        }
        if self.take_fault(key, |f| &mut f.put_fails_after_store) {
            bail!("injected fault: upload response lost after storing");
        }
        Ok(())
    }

    pub async fn exists(&self, key: &str) -> Result<bool> {
        validate_key(key)?;
        match &self.backend {
            Backend::Dir(root) => Ok(root.join(key).is_file()),
            Backend::Hf(bucket) => {
                let entries = bucket
                    .get_paths_info()
                    .paths(vec![key.to_string()])
                    .send()
                    .await
                    .map_err(|e| anyhow::anyhow!("cannot query Hugging Face bucket: {e}"))?;
                Ok(entries
                    .iter()
                    .any(|e| matches!(e, hf_hub::buckets::BucketTreeEntry::File { path, .. } if path == key)))
            }
        }
    }

    pub async fn get(&self, key: &str) -> Result<Vec<u8>> {
        validate_key(key)?;
        if self.take_fault(key, |f| &mut f.get_fails) {
            bail!("injected fault: download failed");
        }
        let mut bytes = match &self.backend {
            Backend::Dir(root) => std::fs::read(root.join(key))
                .with_context(|| format!("remote object {key} not found"))?,
            Backend::Hf(bucket) => {
                let (_, mut stream) = bucket
                    .download_file_stream()
                    .remote_path(key.to_string())
                    .send()
                    .await
                    .map_err(|e| anyhow::anyhow!("download of {key} failed: {e}"))?;
                let mut out = Vec::new();
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk.map_err(|e| anyhow::anyhow!("download of {key} failed: {e}"))?;
                    out.extend_from_slice(&chunk);
                }
                out
            }
        };
        if self.take_fault(key, |f| &mut f.corrupt_next_get) && !bytes.is_empty() {
            let mid = bytes.len() / 2;
            bytes[mid] ^= 0xff;
        }
        Ok(bytes)
    }

    /// List objects under `prefix` (recursive), sorted by key.
    pub async fn list(&self, prefix: &str) -> Result<Vec<Entry>> {
        let mut out = vec![];
        match &self.backend {
            Backend::Dir(root) => {
                let base = root.join(prefix);
                if base.is_dir() {
                    walk(root, &base, &mut out)?;
                }
            }
            Backend::Hf(bucket) => {
                let stream = bucket
                    .list_tree()
                    .prefix(prefix.to_string())
                    .recursive(true)
                    .send()
                    .map_err(|e| anyhow::anyhow!("cannot list Hugging Face bucket: {e}"))?;
                let mut stream = std::pin::pin!(stream);
                while let Some(entry) = stream.next().await {
                    match entry {
                        Ok(hf_hub::buckets::BucketTreeEntry::File { path, size, .. }) => {
                            out.push(Entry { key: path, size })
                        }
                        Ok(_) => {}
                        Err(hf_hub::HFError::BucketNotFound { .. }) => {
                            bail!("bucket not found — create it with `hippo remote set`")
                        }
                        Err(e) => {
                            // An empty prefix may 404; treat "nothing there" as empty.
                            let msg = e.to_string();
                            if msg.contains("404") || msg.to_lowercase().contains("not found") {
                                break;
                            }
                            bail!("cannot list Hugging Face bucket: {msg}")
                        }
                    }
                }
            }
        }
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }
}

fn walk(root: &std::path::Path, dir: &std::path::Path, out: &mut Vec<Entry>) -> Result<()> {
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue; // in-flight temp files
        }
        if p.is_dir() {
            walk(root, &p, out)?;
        } else {
            let key = p.strip_prefix(root)?.to_string_lossy().replace('\\', "/");
            out.push(Entry {
                key,
                size: e.metadata()?.len(),
            });
        }
    }
    Ok(())
}

/// Keys come from our own code, but listings come from the network: be strict.
pub fn validate_key(key: &str) -> Result<()> {
    let ok = !key.is_empty()
        && !key.starts_with('/')
        && !key.contains("//")
        && !key.contains('\\')
        && !key.contains('\0')
        && key.split('/').all(|seg| !seg.is_empty() && seg != "." && seg != "..")
        && key.chars().all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c));
    if !ok {
        bail!("invalid remote object key: {key:?}");
    }
    Ok(())
}
