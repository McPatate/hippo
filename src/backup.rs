//! Backup, restore and device sync.
//!
//! A backup is an incremental git bundle (`<last verified>..main`), age-encrypted and uploaded
//! as a new immutable object. It only counts once it has been downloaded again, hashed,
//! decrypted and checked with `git bundle verify`. Each device appends to its own prefix, so
//! devices never write the same object. See `docs/format.md` for the full format.
//!
//! Crash safety: before uploading, the encrypted bundle is written to `.hippo/outbox/` and the
//! intent recorded in `state.pending`. A crash at any point is resumed by the next backup,
//! which re-sends exactly the same bytes (idempotent) and verifies them.

use crate::config::{RemoteConfig, VaultConfig};
use crate::crypto::Key;
use crate::remote::{ROOT, Remote};
use crate::state::{Pending, State};
use crate::util::{atomic_write, now, random_hex, sha256_hex};
use crate::vault::Vault;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

pub const VERIFIED_REF: &str = "refs/hippo/verified";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultManifest {
    pub format: String,
    pub vault_id: String,
    pub recipient: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

pub fn vault_prefix(vault_id: &str) -> String {
    format!("{ROOT}/{vault_id}")
}

fn device_prefix(vault_id: &str, device: &str) -> String {
    format!("{ROOT}/{vault_id}/devices/{device}/")
}

/// A bundle object name: `<seq:08>-<sha16>.bundle.age`.
#[derive(Debug, Clone, PartialEq)]
pub struct BundleName {
    pub device: String,
    pub seq: u64,
    pub sha16: String,
    pub key: String,
}

pub fn parse_bundle_key(key: &str) -> Option<BundleName> {
    let mut parts = key.rsplitn(3, '/');
    let file = parts.next()?;
    let device = parts.next()?.to_string();
    let stem = file.strip_suffix(".bundle.age")?;
    let (seq, sha16) = stem.split_once('-')?;
    if seq.len() != 8 || sha16.len() != 16 || !sha16.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(BundleName {
        device,
        seq: seq.parse().ok()?,
        sha16: sha16.to_string(),
        key: key.to_string(),
    })
}

/// All bundles of a vault, grouped by device and sorted by sequence.
pub async fn list_bundles(remote: &Remote, vault_id: &str) -> Result<BTreeMap<String, Vec<BundleName>>> {
    let mut by_device: BTreeMap<String, Vec<BundleName>> = BTreeMap::new();
    for e in remote.list(&format!("{}/devices/", vault_prefix(vault_id))).await? {
        if let Some(b) = parse_bundle_key(&e.key) {
            by_device.entry(b.device.clone()).or_default().push(b);
        }
    }
    for v in by_device.values_mut() {
        v.sort_by_key(|b| b.seq);
    }
    Ok(by_device)
}

/// Ensure the bundles form an unbroken 1..=n chain. Returns an error naming the first gap.
fn check_chain(device: &str, bundles: &[BundleName]) -> Result<()> {
    for (i, b) in bundles.iter().enumerate() {
        let expected = i as u64 + 1;
        if b.seq != expected {
            bail!(
                "backup chain of device {device} is broken: expected bundle #{expected}, found #{} \
                 (duplicate or missing object). Refusing to restore partial data.",
                b.seq
            );
        }
    }
    Ok(())
}

#[derive(Debug)]
pub struct BackupReport {
    pub seq: Option<u64>,
    pub bytes: usize,
    pub resumed: bool,
    pub committed: Option<String>,
}

/// Write the write-once vault manifest if absent; check it matches if present.
pub async fn ensure_manifest(remote: &Remote, cfg: &VaultConfig) -> Result<()> {
    let key = format!("{}/vault.json", vault_prefix(&cfg.vault_id));
    if remote.exists(&key).await? {
        let m: VaultManifest = serde_json::from_slice(&remote.get(&key).await?)
            .context("remote vault.json is malformed")?;
        if m.recipient != cfg.recipient {
            bail!(
                "the remote vault {} is encrypted to a different key ({}) than this vault ({}).\n  → Refusing to mix keys in one backup.",
                cfg.vault_id,
                m.recipient,
                cfg.recipient
            );
        }
        return Ok(());
    }
    let m = VaultManifest {
        format: "hippo-v1".into(),
        vault_id: cfg.vault_id.clone(),
        recipient: cfg.recipient.clone(),
        created_at: now(),
    };
    remote.put_new(&key, &serde_json::to_vec_pretty(&m)?).await
}

/// Save local changes, then upload and verify everything not yet verified.
pub async fn backup(vault: &Vault, key: &Key, remote: &Remote) -> Result<BackupReport> {
    let backup_lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(vault.meta_dir().join("backup.lock"))?;
    // Wait briefly: a concurrent backup may be finishing, and a child process forked by another
    // thread can hold a copy of the lock fd for a moment before it execs.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while backup_lock.try_lock().is_err() {
        if std::time::Instant::now() > deadline {
            bail!("another backup of this vault is still running after 15 s\n  → Retry in a moment");
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let mut state = State::load(&vault.root)?;
    let result = backup_inner(vault, key, remote, &mut state).await;
    state.last_error = result.as_ref().err().map(|e| format!("{e:#}"));
    state.save(&vault.root)?;
    result
}

async fn backup_inner(vault: &Vault, key: &Key, remote: &Remote, state: &mut State) -> Result<BackupReport> {
    let cfg = &vault.config;
    if key.recipient() != cfg.recipient {
        bail!("the loaded key does not match this vault's key ({})", cfg.recipient);
    }
    // 1. Commit pending local edits (skipped while a device merge is unresolved).
    let committed = if vault.unresolved_conflicts()?.is_empty() {
        vault.save("save")?
    } else {
        None
    };
    ensure_manifest(remote, cfg).await?;

    // 2. Guard: nobody else may be writing under our device prefix.
    let ours = list_bundles(remote, &cfg.vault_id)
        .await?
        .remove(&cfg.device_id)
        .unwrap_or_default();
    let remote_max = ours.last().map(|b| b.seq).unwrap_or(0);
    let allowed_max = state.pending.as_ref().map(|p| p.seq).unwrap_or(state.verified_seq);
    if remote_max > allowed_max {
        bail!(
            "the bucket has bundle #{remote_max} for this device but we only know up to #{allowed_max}.\n  \
             Another copy of this vault (same device id) may be backing up — e.g. a copied vault folder.\n  \
             → Nothing was uploaded. Run `hippo doctor` for details."
        );
    }

    // 3. Resume an interrupted backup first.
    let mut resumed = false;
    if let Some(p) = state.pending.clone() {
        let bytes = fs::read(outbox_path(vault, &p))
            .with_context(|| format!("pending backup #{} lost its outbox file", p.seq))?;
        upload_and_verify(vault, key, remote, state, &p, &bytes).await?;
        resumed = true;
    }

    // 4. Bundle whatever is new since the last verified commit.
    let tip = vault.git.rev_parse("main")?.context("vault has no commits")?;
    let verified = vault.git.rev_parse(VERIFIED_REF)?;
    if verified.as_deref() == Some(tip.as_str()) {
        return Ok(BackupReport { seq: None, bytes: 0, resumed, committed });
    }
    let range = match &verified {
        Some(v) if vault.git.is_ancestor(v, &tip)? => format!("{v}..main"),
        _ => "main".to_string(), // first backup of this device, or history diverged: full bundle
    };
    let plain_path = vault.meta_dir().join(format!("tmp-{}.bundle", random_hex(4)));
    vault
        .git
        .run(["bundle", "create", "-q", &plain_path.to_string_lossy(), &range])?;
    let plain = fs::read(&plain_path)?;
    let _ = fs::remove_file(&plain_path);
    let cipher = key.encrypt(&plain)?;
    let sha = sha256_hex(&cipher);
    let seq = state.verified_seq + 1;
    let p = Pending {
        seq,
        key: format!("{}{seq:08}-{}.bundle.age", device_prefix(&cfg.vault_id, &cfg.device_id), &sha[..16]),
        sha256: sha,
        tip,
        created_at: now(),
    };
    atomic_write(&outbox_path(vault, &p), &cipher)?;
    state.pending = Some(p.clone());
    state.save(&vault.root)?;

    // Deep secret scan of the new commits (~0.8 s, offline), detached: never delays the backup.
    {
        let root = vault.root.clone();
        let since = verified.clone();
        tokio::task::spawn_blocking(move || {
            if let Ok(Some(found)) = crate::secrets::trufflehog_git(&root, since.as_deref()) {
                let _ = crate::secrets::record_findings(&root, found);
            }
        });
    }
    upload_and_verify(vault, key, remote, state, &p, &cipher).await?;
    crate::log::record(&vault.root, "backup", serde_json::json!({"seq": seq, "bytes": cipher.len(), "tip": &p.tip[..12], "verified": true}));
    Ok(BackupReport { seq: Some(seq), bytes: cipher.len(), resumed, committed })
}

fn outbox_path(vault: &Vault, p: &Pending) -> PathBuf {
    vault.meta_dir().join("outbox").join(format!("{:08}.bundle.age", p.seq))
}

async fn upload_and_verify(
    vault: &Vault,
    key: &Key,
    remote: &Remote,
    state: &mut State,
    p: &Pending,
    cipher: &[u8],
) -> Result<()> {
    if sha256_hex(cipher) != p.sha256 {
        bail!("outbox file for backup #{} is corrupted locally", p.seq);
    }
    remote.put_new(&p.key, cipher).await?;
    state.uploaded_seq = p.seq;
    state.uploaded_at = Some(now());
    state.save(&vault.root)?;

    // Verify from the remote copy, not from memory.
    let back = remote.get(&p.key).await.context("could not read the backup back for verification")?;
    if sha256_hex(&back) != p.sha256 {
        bail!("verification failed: backup #{} in the bucket does not match what was uploaded", p.seq);
    }
    let plain = key.decrypt(&back)?;
    let check = vault.meta_dir().join(format!("verify-{}.bundle", random_hex(4)));
    atomic_write(&check, &plain)?;
    let ok = vault.git.try_run(["bundle", "verify", "-q", &check.to_string_lossy()])?;
    let heads = vault.git.try_run(["bundle", "list-heads", &check.to_string_lossy()])?;
    let _ = fs::remove_file(&check);
    if !ok.success {
        bail!("verification failed: backup #{} is not a valid git bundle: {}", p.seq, ok.stderr.trim());
    }
    if !heads.stdout.contains(&p.tip) {
        bail!("verification failed: backup #{} does not contain commit {}", p.seq, &p.tip[..12]);
    }

    // Promote: this is now a verified recovery point.
    vault.git.update_ref(VERIFIED_REF, &p.tip)?;
    state.verified_seq = p.seq;
    state.verified_at = Some(now());
    state.pending = None;
    state.save(&vault.root)?;
    let _ = fs::remove_file(outbox_path(vault, p));
    Ok(())
}

// ───────────────────────────── restore ─────────────────────────────

pub struct RestoreOptions {
    pub vault_id: Option<String>,
    pub device: Option<String>,
}

pub struct RestoreReport {
    pub vault: Vault,
    pub from_device: String,
    pub bundles: usize,
}

/// Vault ids present in a remote.
pub async fn list_vaults(remote: &Remote) -> Result<Vec<VaultManifest>> {
    let mut out = vec![];
    for e in remote.list(&format!("{ROOT}/")).await? {
        if e.key.ends_with("/vault.json") && e.key.matches('/').count() == 3
            && let Ok(m) = serde_json::from_slice::<VaultManifest>(&remote.get(&e.key).await?) {
                out.push(m);
            }
    }
    Ok(out)
}

/// Rebuild a vault from the remote into `dest` (which must not exist or be empty).
/// Builds in a temporary sibling directory and renames into place only when complete.
pub async fn restore(
    remote: &Remote,
    remote_cfg: &RemoteConfig,
    key: &Key,
    dest: &Path,
    opts: RestoreOptions,
) -> Result<RestoreReport> {
    crate::git::Git::ensure_available()?;
    if dest.exists() && fs::read_dir(dest)?.next().is_some() {
        bail!(
            "{} already exists and is not empty — restore never overwrites existing data.\n  → Choose an empty destination",
            dest.display()
        );
    }
    let vaults = list_vaults(remote).await?;
    let manifest = match &opts.vault_id {
        Some(id) => vaults.into_iter().find(|m| &m.vault_id == id).context("no vault with that id in this bucket")?,
        None => {
            let mine: Vec<_> = vaults.into_iter().filter(|m| m.recipient == key.recipient()).collect();
            match mine.len() {
                0 => bail!("no vault encrypted to your key ({}) was found in {}", key.recipient(), remote_cfg.display()),
                1 => mine.into_iter().next().unwrap(),
                _ => bail!(
                    "several vaults use your key: {} — pick one with --vault-id",
                    mine.iter().map(|m| m.vault_id.as_str()).collect::<Vec<_>>().join(", ")
                ),
            }
        }
    };
    if manifest.recipient != key.recipient() {
        bail!("this vault is encrypted to {}, but your key is {}", manifest.recipient, key.recipient());
    }
    let mut chains = list_bundles(remote, &manifest.vault_id).await?;
    let device = match opts.device {
        Some(d) => d,
        None => {
            // Default: the device with the longest chain (most backups). Ties: alphabetical.
            chains
                .iter()
                .max_by_key(|(_, v)| v.len())
                .map(|(d, _)| d.clone())
                .context("this vault has no backups yet")?
        }
    };
    let chain = chains.remove(&device).context("no backups for that device")?;
    check_chain(&device, &chain)?;

    let parent = dest.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let staging = parent.join(format!(
        ".{}.restoring-{}",
        dest.file_name().unwrap_or_default().to_string_lossy(),
        random_hex(4)
    ));
    let cfg = VaultConfig {
        format: crate::vault::FORMAT_VERSION,
        vault_id: manifest.vault_id.clone(),
        device_id: random_hex(6), // a restored copy is a new device
        recipient: manifest.recipient.clone(),
        remote: Some(remote_cfg.clone()),
    };
    let result = async {
        let git = crate::git::Git::new(&staging, &cfg.device_id);
        git.init()?;
        let vault = Vault::create_layout(&staging, cfg.clone(), false)?;
        let tip = fetch_chain(&vault, key, remote, &device, &chain).await?;
        vault.git.run(["checkout", "-q", "-B", "main", &tip])?;
        let mut state = State::default();
        state.peers.insert(device.clone(), crate::state::PeerState { fetched_seq: chain.len() as u64, tip: Some(tip) });
        state.save(&staging)?;
        anyhow::Ok(())
    }
    .await;
    if let Err(e) = result {
        bail!("restore failed, nothing was written to {} (partial data kept in {} for inspection): {e:#}", dest.display(), staging.display());
    }
    if dest.exists() {
        fs::remove_dir(dest)?; // empty, checked above
    }
    fs::rename(&staging, dest)?;
    crate::log::record(dest, "restore", serde_json::json!({"from_device": &device, "bundles": chain.len(), "remote": remote_cfg.display()}));
    crate::util::fsync_dir(parent);
    Ok(RestoreReport { vault: Vault::open(dest)?, from_device: device, bundles: chain.len() })
}

/// Download, verify and fetch a run of bundles into `refs/hippo/peers/<device>/main`.
/// Returns the resulting tip commit.
async fn fetch_chain(vault: &Vault, key: &Key, remote: &Remote, device: &str, bundles: &[BundleName]) -> Result<String> {
    let peer_ref = format!("refs/hippo/peers/{device}/main");
    for b in bundles {
        let cipher = remote.get(&b.key).await?;
        if !sha256_hex(&cipher).starts_with(&b.sha16) {
            bail!("bundle {} is corrupted in the bucket (hash mismatch)", b.key);
        }
        let plain = key.decrypt(&cipher).with_context(|| format!("cannot decrypt {}", b.key))?;
        let tmp = vault.meta_dir().join(format!("fetch-{}.bundle", random_hex(4)));
        atomic_write(&tmp, &plain)?;
        let tmp_s = tmp.to_string_lossy().to_string();
        let verify = vault.git.try_run(["bundle", "verify", "-q", &tmp_s])?;
        if !verify.success {
            let _ = fs::remove_file(&tmp);
            bail!("bundle {} failed git verification: {}", b.key, verify.stderr.trim());
        }
        let fetched = vault.git.try_run(["fetch", "-q", &tmp_s, &format!("+refs/heads/main:{peer_ref}")]);
        let _ = fs::remove_file(&tmp);
        let fetched = fetched?;
        if !fetched.success {
            bail!("could not apply bundle {}: {}", b.key, fetched.stderr.trim());
        }
    }
    vault.git.rev_parse(&peer_ref)?.context("restored chain has no main branch")
}

// ───────────────────────────── device sync ─────────────────────────────

#[derive(Debug, Default)]
pub struct SyncReport {
    pub fetched: Vec<(String, usize)>,
    pub merged: Vec<String>,
    pub conflicts: Vec<String>,
    pub trace: Option<PathBuf>,
}

/// Fetch other devices' new bundles and merge them into `main`.
/// Conflicts are left in the files (diff3 markers) and reported; nothing is dropped.
pub async fn sync(vault: &Vault, key: &Key, remote: &Remote) -> Result<SyncReport> {
    let mut report = SyncReport::default();
    if !vault.unresolved_conflicts()?.is_empty() {
        report.conflicts = vault.unresolved_conflicts()?;
        return Ok(report);
    }
    let mut state = State::load(&vault.root)?;
    let chains = list_bundles(remote, &vault.config.vault_id).await?;
    for (device, chain) in chains {
        if device == vault.config.device_id {
            continue;
        }
        check_chain(&device, &chain)?;
        let peer = state.peers.entry(device.clone()).or_default();
        let new: Vec<BundleName> = chain.into_iter().filter(|b| b.seq > peer.fetched_seq).collect();
        if new.is_empty() {
            continue;
        }
        let tip = fetch_chain(vault, key, remote, &device, &new).await?;
        peer.fetched_seq = new.last().unwrap().seq;
        peer.tip = Some(tip);
        report.fetched.push((device.clone(), new.len()));
        crate::log::record(&vault.root, "fetch", serde_json::json!({"device": &device, "bundles": new.len()}));
    }
    state.save(&vault.root)?;

    let _lock = vault.lock()?;
    vault.save_locked("save before device merge")?;
    for (device, peer) in &state.peers {
        let Some(tip) = &peer.tip else { continue };
        if vault.git.is_ancestor(tip, "main")? {
            continue; // already merged
        }
        let ours = vault.git.rev_parse("main")?.context("no main branch")?;
        let trace = write_merge_trace(vault, device, &ours, tip)?;
        let out = vault.git.try_run([
            "merge",
            "--no-ff",
            "--no-edit",
            "-m",
            &format!("merge memories from device {device}"),
            tip,
        ])?;
        if out.success {
            report.merged.push(device.clone());
            crate::log::record(&vault.root, "merge", serde_json::json!({"device": device}));
        } else {
            report.conflicts = vault.git.conflicted_paths()?;
            crate::log::record(&vault.root, "merge-conflict", serde_json::json!({"device": device, "files": &report.conflicts, "trace": trace.display().to_string()}));
            report.trace = Some(trace);
            if report.conflicts.is_empty() {
                bail!("merge with device {device} failed: {}", out.stderr.trim());
            }
            break; // resolve one device merge at a time
        }
    }
    Ok(report)
}

/// Forensics: before merging, record both sides and the full diff between them.
fn write_merge_trace(vault: &Vault, device: &str, ours: &str, theirs: &str) -> Result<PathBuf> {
    let stamp = now().format("%Y%m%dT%H%M%SZ").to_string();
    let base = vault.git.run(["merge-base", ours, theirs]).unwrap_or_default();
    let base = base.trim();
    vault.git.update_ref(&format!("refs/hippo/premerge/{stamp}/ours"), ours)?;
    vault.git.update_ref(&format!("refs/hippo/premerge/{stamp}/theirs"), theirs)?;
    let diff = vault.git.run(["diff", ours, theirs]).unwrap_or_default();
    let body = format!(
        "# hippo merge trace\n# when:   {stamp}\n# device: {device}\n# ours:   {ours}\n# theirs: {theirs}\n# base:   {base}\n# refs:   refs/hippo/premerge/{stamp}/{{ours,theirs}}\n\n{diff}"
    );
    let path = vault.meta_dir().join("merges").join(format!("{stamp}-{device}.diff"));
    atomic_write(&path, body.as_bytes())?;
    Ok(path)
}

// ───────────────────────────── status ─────────────────────────────

#[derive(Debug)]
pub struct Status {
    pub dirty_files: usize,
    pub unverified_commits: usize,
    pub state: State,
    pub conflicts: Vec<String>,
    pub remote: Option<RemoteConfig>,
}

pub fn status(vault: &Vault) -> Result<Status> {
    let state = State::load(&vault.root)?;
    let dirty_files = vault.git.dirty_count()?;
    let unverified_commits = match vault.git.rev_parse(VERIFIED_REF)? {
        Some(v) => vault.git.count_commits(&format!("{v}..main"))?,
        None => vault.git.count_commits("main")?,
    };
    Ok(Status {
        dirty_files,
        unverified_commits,
        state,
        conflicts: vault.unresolved_conflicts()?,
        remote: vault.config.remote.clone(),
    })
}
