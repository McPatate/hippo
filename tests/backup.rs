//! End-to-end backup / restore / sync tests against a directory remote, with fault injection.

use hippo::backup::{self, RestoreOptions, VERIFIED_REF};
use hippo::config::RemoteConfig;
use hippo::crypto::Key;
use hippo::memory::Memory;
use hippo::remote::Remote;
use hippo::state::State;
use hippo::vault::Vault;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

struct Env {
    _tmp: TempDir,
    root: PathBuf,
    key: Key,
    rc: RemoteConfig,
    remote: Remote,
}

fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let key = Key::generate(&root.join("key.txt")).unwrap();
    let rc = RemoteConfig::Dir { path: root.join("bucket") };
    let remote = Remote::open(&rc).unwrap();
    Env { _tmp: tmp, root, key, rc, remote }
}

fn mem(name: &str, body: &str) -> Memory {
    Memory { name: name.into(), kind: "note".into(), body: body.into(), ..Default::default() }
}

impl Env {
    fn vault(&self, name: &str) -> Vault {
        let mut v = Vault::init(&self.root.join(name), &self.key).unwrap();
        v.config.remote = Some(self.rc.clone());
        v.config.save(&v.root).unwrap();
        v
    }
    async fn restore_to(&self, name: &str) -> anyhow::Result<Vault> {
        let r = backup::restore(
            &self.remote,
            &self.rc,
            &self.key,
            &self.root.join(name),
            RestoreOptions { vault_id: None, device: None },
        )
        .await?;
        Ok(r.vault)
    }
    fn bundles(&self) -> Vec<PathBuf> {
        let mut out = vec![];
        fn walk(d: &Path, out: &mut Vec<PathBuf>) {
            if let Ok(rd) = std::fs::read_dir(d) {
                for e in rd.flatten() {
                    let p = e.path();
                    if p.is_dir() { walk(&p, out) } else if p.to_string_lossy().ends_with(".bundle.age") { out.push(p) }
                }
            }
        }
        walk(&self.root.join("bucket"), &mut out);
        out.sort();
        out
    }
}

fn body(v: &Vault, name: &str) -> String {
    v.read_memory(name).unwrap().body
}

#[tokio::test]
async fn roundtrip_backup_restore() {
    let e = env();
    let v = e.vault("a");
    v.remember(&mem("likes-rust", "Prefers Rust.")).unwrap();
    v.remember(&mem("hippo", "Loves hippos 🦛")).unwrap();
    let r = backup::backup(&v, &e.key, &e.remote).await.unwrap();
    assert_eq!(r.seq, Some(1));

    let s = backup::status(&v).unwrap();
    assert_eq!((s.dirty_files, s.unverified_commits, s.state.verified_seq), (0, 0, 1));

    let b = e.restore_to("b").await.unwrap();
    assert_eq!(body(&b, "likes-rust"), "Prefers Rust.\n");
    assert_eq!(body(&b, "hippo"), "Loves hippos 🦛\n");
    assert_ne!(b.config.device_id, v.config.device_id, "restored copy must be a new device");
    assert_eq!(b.config.vault_id, v.config.vault_id);
}

#[tokio::test]
async fn incremental_and_idempotent() {
    let e = env();
    let v = e.vault("a");
    v.remember(&mem("one", "1")).unwrap();
    let first = backup::backup(&v, &e.key, &e.remote).await.unwrap();
    // Nothing new → no upload.
    assert_eq!(backup::backup(&v, &e.key, &e.remote).await.unwrap().seq, None);
    v.remember(&mem("two", "2")).unwrap();
    let second = backup::backup(&v, &e.key, &e.remote).await.unwrap();
    assert_eq!(second.seq, Some(2));
    assert!(second.bytes < first.bytes, "incremental bundle should be smaller than the full one");
    // Uncommitted edits are saved by backup.
    std::fs::write(v.memory_path("three"), "---\nname: three\n---\n3\n").unwrap();
    assert_eq!(backup::backup(&v, &e.key, &e.remote).await.unwrap().seq, Some(3));
    assert_eq!(e.bundles().len(), 3);
    let b = e.restore_to("b").await.unwrap();
    assert_eq!(b.list().unwrap().len(), 3);
}

#[tokio::test]
async fn crash_after_upload_before_ack_resumes() {
    let e = env();
    let v = e.vault("a");
    v.remember(&mem("x", "x")).unwrap();
    e.remote.faults.lock().unwrap().put_fails_after_store = true;
    assert!(backup::backup(&v, &e.key, &e.remote).await.is_err());
    let st = State::load(&v.root).unwrap();
    assert!(st.pending.is_some());
    assert_eq!((st.uploaded_seq, st.verified_seq), (0, 0));
    assert!(v.git.rev_parse(VERIFIED_REF).unwrap().is_none(), "must not claim verified");
    assert!(st.last_error.is_some());

    let r = backup::backup(&v, &e.key, &e.remote).await.unwrap();
    assert!(r.resumed);
    let st = State::load(&v.root).unwrap();
    assert_eq!((st.verified_seq, st.pending.is_none()), (1, true));
    assert_eq!(e.bundles().len(), 1, "retry must not create a duplicate object");
    assert!(e.restore_to("b").await.is_ok());
}

#[tokio::test]
async fn upload_failure_before_store_retries() {
    let e = env();
    let v = e.vault("a");
    v.remember(&mem("x", "x")).unwrap();
    e.remote.faults.lock().unwrap().put_fails_before_store = true;
    assert!(backup::backup(&v, &e.key, &e.remote).await.is_err());
    assert_eq!(e.bundles().len(), 0);
    // A new edit while the previous upload is pending: the pending one goes first, then the new one.
    v.remember(&mem("y", "y")).unwrap();
    let r = backup::backup(&v, &e.key, &e.remote).await.unwrap();
    assert!(r.resumed);
    assert_eq!(r.seq, Some(2));
    let b = e.restore_to("b").await.unwrap();
    assert_eq!(b.list().unwrap().len(), 2);
}

#[tokio::test]
async fn corrupted_read_back_is_not_verified() {
    let e = env();
    let v = e.vault("a");
    v.remember(&mem("x", "x")).unwrap();
    e.remote.faults.lock().unwrap().corrupt_next_get = true;
    let err = backup::backup(&v, &e.key, &e.remote).await.unwrap_err();
    assert!(format!("{err:#}").contains("verification failed"), "{err:#}");
    let st = State::load(&v.root).unwrap();
    assert_eq!((st.uploaded_seq, st.verified_seq), (1, 0), "uploaded but NOT verified");
    // Next run re-verifies the same object and promotes it.
    assert!(backup::backup(&v, &e.key, &e.remote).await.unwrap().resumed);
    assert_eq!(State::load(&v.root).unwrap().verified_seq, 1);
}

#[tokio::test]
async fn restore_detects_tampering_and_writes_nothing() {
    let e = env();
    let v = e.vault("a");
    v.remember(&mem("x", "x")).unwrap();
    backup::backup(&v, &e.key, &e.remote).await.unwrap();
    let f = &e.bundles()[0];
    let mut bytes = std::fs::read(f).unwrap();
    let n = bytes.len();
    bytes[n - 5] ^= 1;
    std::fs::write(f, bytes).unwrap();
    let err = e.restore_to("b").await.err().unwrap();
    assert!(format!("{err:#}").contains("corrupted"), "{err:#}");
    assert!(!e.root.join("b").exists(), "failed restore must not create the destination");
}

#[tokio::test]
async fn restore_detects_missing_bundle() {
    let e = env();
    let v = e.vault("a");
    for i in 0..3 {
        v.remember(&mem(&format!("m{i}"), "x")).unwrap();
        backup::backup(&v, &e.key, &e.remote).await.unwrap();
    }
    std::fs::remove_file(&e.bundles()[1]).unwrap();
    let err = e.restore_to("b").await.err().unwrap();
    assert!(format!("{err:#}").contains("broken"), "{err:#}");
}

#[tokio::test]
async fn restore_refuses_non_empty_destination_and_wrong_key() {
    let e = env();
    let v = e.vault("a");
    backup::backup(&v, &e.key, &e.remote).await.unwrap();
    std::fs::create_dir_all(e.root.join("busy")).unwrap();
    std::fs::write(e.root.join("busy/precious.txt"), "do not touch").unwrap();
    assert!(e.restore_to("busy").await.is_err());
    assert_eq!(std::fs::read_to_string(e.root.join("busy/precious.txt")).unwrap(), "do not touch");

    let other = Key::generate(&e.root.join("other.txt")).unwrap();
    let r = backup::restore(&e.remote, &e.rc, &other, &e.root.join("c"), RestoreOptions { vault_id: None, device: None }).await;
    assert!(r.is_err());
}

#[tokio::test]
async fn copied_vault_with_same_device_id_is_detected() {
    let e = env();
    let v = e.vault("a");
    backup::backup(&v, &e.key, &e.remote).await.unwrap();
    let status = std::process::Command::new("cp").arg("-R").arg(e.root.join("a")).arg(e.root.join("a-copy")).status().unwrap();
    assert!(status.success());
    let copy = Vault::open(&e.root.join("a-copy")).unwrap();
    copy.remember(&mem("from-copy", "c")).unwrap();
    backup::backup(&copy, &e.key, &e.remote).await.unwrap();
    v.remember(&mem("from-original", "o")).unwrap();
    let err = backup::backup(&v, &e.key, &e.remote).await.unwrap_err();
    assert!(format!("{err:#}").contains("same device id"), "{err:#}");
}

#[tokio::test]
async fn two_devices_merge_cleanly() {
    let e = env();
    let a = e.vault("a");
    a.remember(&mem("shared", "line 1\nline 2\nline 3")).unwrap();
    backup::backup(&a, &e.key, &e.remote).await.unwrap();
    let b = e.restore_to("b").await.unwrap();

    b.remember(&mem("from-b", "b")).unwrap();
    backup::backup(&b, &e.key, &e.remote).await.unwrap();
    a.remember(&mem("from-a", "a")).unwrap();
    backup::backup(&a, &e.key, &e.remote).await.unwrap();

    let r = backup::sync(&a, &e.key, &e.remote).await.unwrap();
    assert_eq!(r.merged.len(), 1);
    assert!(r.conflicts.is_empty());
    assert_eq!(a.list().unwrap().len(), 3);
    backup::backup(&a, &e.key, &e.remote).await.unwrap();
    let r = backup::sync(&b, &e.key, &e.remote).await.unwrap();
    assert!(r.conflicts.is_empty());
    assert_eq!(b.list().unwrap().len(), 3);
}

#[tokio::test]
async fn conflicting_edits_are_preserved_and_traced() {
    let e = env();
    let a = e.vault("a");
    a.remember(&mem("tone", "Be concise.")).unwrap();
    backup::backup(&a, &e.key, &e.remote).await.unwrap();
    let b = e.restore_to("b").await.unwrap();

    b.remember(&mem("tone", "Be concise and friendly.")).unwrap();
    backup::backup(&b, &e.key, &e.remote).await.unwrap();
    a.remember(&mem("tone", "Be concise, no emojis.")).unwrap();

    let r = backup::sync(&a, &e.key, &e.remote).await.unwrap();
    assert_eq!(r.conflicts, vec!["memories/tone.md".to_string()]);
    let trace = std::fs::read_to_string(r.trace.unwrap()).unwrap();
    assert!(trace.contains("friendly") && trace.contains("no emojis"));
    let text = std::fs::read_to_string(a.memory_path("tone")).unwrap();
    assert!(text.contains("friendly") && text.contains("no emojis"), "both sides kept");

    // Saving with markers is refused; backups still work (they bundle committed history only).
    assert!(a.save("oops").is_err());
    assert!(backup::backup(&a, &e.key, &e.remote).await.is_ok());

    // Resolve, save, back up, and the other device picks it up.
    std::fs::write(a.memory_path("tone"), "---\nname: tone\ntype: note\n---\nBe concise and friendly, no emojis.\n").unwrap();
    assert!(a.save("resolve").unwrap().is_some());
    assert!(a.unresolved_conflicts().unwrap().is_empty());
    backup::backup(&a, &e.key, &e.remote).await.unwrap();
    let r = backup::sync(&b, &e.key, &e.remote).await.unwrap();
    assert!(r.conflicts.is_empty());
    assert_eq!(body(&b, "tone"), "Be concise and friendly, no emojis.\n");
}

#[tokio::test]
async fn user_git_config_is_ignored() {
    let e = env();
    // A hostile global config: forced signing with a missing key and a failing hook.
    let hooks = e.root.join("hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    std::fs::write(hooks.join("pre-commit"), "#!/bin/sh\nexit 1\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(hooks.join("pre-commit"), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let cfg = e.root.join("gitconfig");
    std::fs::write(&cfg, format!("[commit]\n\tgpgsign = true\n[user]\n\tsigningkey = DEADBEEF\n[core]\n\thooksPath = {}\n", hooks.display())).unwrap();
    // SAFETY: test-only; hippo's git runner strips all GIT_* variables anyway.
    unsafe { std::env::set_var("GIT_CONFIG_GLOBAL", &cfg) };
    let v = e.vault("a");
    assert!(v.remember(&mem("x", "x")).unwrap().is_some());
    backup::backup(&v, &e.key, &e.remote).await.unwrap();
}

#[tokio::test]
async fn import_claude_code_memories() {
    let e = env();
    let v = e.vault("a");
    let src = e.root.join("claude");
    for (proj, file, text) in [
        ("-Users-x-work-alpha", "feedback_tone.md", "---\nname: Tone of voice\ndescription: be concise\nmetadata:\n  type: feedback\n  originSessionId: abc\n---\nBe concise. See [[user_profile]].\n"),
        ("-Users-x-work-alpha", "user_profile.md", "---\nname: user_profile\ndescription: who\ntype: user\n---\nRust dev.\n"),
        ("-Users-x-work-alpha", "MEMORY.md", "- [Tone](feedback_tone.md)\n"),
        ("-Users-x-work-beta", "user_profile.md", "---\nname: user_profile\ndescription: other\ntype: user\n---\nDifferent content.\n"),
    ] {
        let d = src.join(proj).join("memory");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(file), text).unwrap();
    }
    let (plans, commit) = hippo::import::plan_and_import_claude(&v, &src, false).unwrap();
    assert_eq!(plans.len(), 3, "MEMORY.md index is skipped");
    assert!(commit.is_some());
    let tone = v.read_memory("feedback-tone").unwrap();
    assert_eq!((tone.kind.as_str(), tone.description.as_str()), ("feedback", "be concise"));
    assert!(tone.tags.contains(&"claude-code".to_string()));
    assert!(!tone.extra.contains_key("originSessionId"));
    let names: Vec<String> = v.list().unwrap().into_iter().map(|m| m.name).collect();
    assert_eq!(names.iter().filter(|n| n.ends_with("user-profile")).count(), 2, "collision kept both: {names:?}");
    let link = tone.body.split("[[").nth(1).unwrap().split("]]").next().unwrap().to_string();
    assert!(names.contains(&link), "link rewritten to an existing memory: {link}");
    assert!(src.join("-Users-x-work-alpha/memory/feedback_tone.md").exists(), "source untouched");

    // Re-running is a no-op; an edited source updates the same memory in place.
    let (plans, commit) = hippo::import::plan_and_import_claude(&v, &src, false).unwrap();
    assert!(commit.is_none() && plans.iter().all(|p| p.status == hippo::import::Status::Unchanged), "{plans:?}");
    std::fs::write(src.join("-Users-x-work-alpha/memory/feedback_tone.md"), "---\nname: Tone of voice\ndescription: be concise\nmetadata:\n  type: feedback\n---\nBe very concise.\n").unwrap();
    let (plans, commit) = hippo::import::plan_and_import_claude(&v, &src, false).unwrap();
    assert!(commit.is_some());
    assert_eq!(hippo::import::count(&plans, hippo::import::Status::Updated), 1);
    assert_eq!(v.list().unwrap().len(), 3, "no duplicates");
    assert!(v.read_memory("feedback-tone").unwrap().body.contains("very concise"));
}

#[tokio::test]
async fn fast_commit_keeps_git_consistent() {
    let e = env();
    let v = e.vault("a");
    let c1 = v.remember(&mem("a", "one")).unwrap().unwrap();
    let c2 = v.remember(&mem("a", "two")).unwrap().unwrap();
    assert_ne!(c1, c2);
    assert_eq!(v.remember(&mem("a", "two")).unwrap(), None, "identical content → no commit");
    v.remember(&mem("b", "bee")).unwrap();
    assert!(v.forget("b").unwrap());
    // Index and HEAD agree with the working tree; repository is sound.
    assert_eq!(v.git.run(["status", "--porcelain"]).unwrap(), "");
    assert!(v.git.try_run(["fsck", "--strict"]).unwrap().success);
    assert!(v.git.run(["show", "HEAD:memories/a.md"]).unwrap().contains("two"));
    assert!(!v.git.try_run(["cat-file", "-e", "HEAD:memories/b.md"]).unwrap().success);
    assert_eq!(v.git.count_commits("main").unwrap(), 5); // create + a + a + b + forget b
    // A later CLI commit builds the right tree (no stale cache-tree).
    std::fs::write(v.memory_path("c"), "---\nname: c\n---\nsee\n").unwrap();
    v.save("manual").unwrap();
    assert!(v.git.run(["show", "HEAD:memories/a.md"]).unwrap().contains("two"));
    assert!(v.git.run(["ls-tree", "-r", "--name-only", "HEAD"]).unwrap().contains("memories/c.md"));
    assert_eq!(v.git.run(["status", "--porcelain"]).unwrap(), "");
    // Backups and restores still round-trip.
    backup::backup(&v, &e.key, &e.remote).await.unwrap();
    let b = e.restore_to("b").await.unwrap();
    assert_eq!(body(&b, "a"), "two\n");
}

/// Regression: a legacy import (no `claude_file`) whose stem starts with the project name
/// (`picklescan_venv` in project `picklescan`) must be recognised, not re-imported as a duplicate.
#[tokio::test]
async fn import_recognises_legacy_project_named_stems() {
    let e = env();
    let v = e.vault("a");
    let src = e.root.join("claude");
    // Real project dirs start with the home path, which the importer strips (`-Users-luc-work-picklescan`).
    let proj = format!("{}-work-picklescan", dirs::home_dir().unwrap().to_string_lossy().replace(['/', '.'], "-"));
    let d = src.join(&proj).join("memory");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("picklescan_venv.md"), "---\nname: picklescan_venv\ndescription: venv\ntype: reference\n---\nUse ~/.venv/picklescan.\n").unwrap();
    std::fs::write(d.join("notes.md"), "---\nname: notes\ndescription: n\ntype: note\n---\nSee [[picklescan_venv]].\n").unwrap();
    // Simulate what the first importer version wrote: no `claude_file`.
    for (name, desc, kind, body) in [("picklescan-venv", "venv", "reference", "Use ~/.venv/picklescan.\n"), ("notes", "n", "note", "See [[picklescan-venv]].\n")] {
        let mut m = Memory { name: name.into(), description: desc.into(), kind: kind.into(), body: body.into(), ..Default::default() };
        m.tags = vec!["claude-code".into(), "picklescan".into()];
        m.extra.insert("source".into(), "claude-code".into());
        m.extra.insert("claude_project".into(), proj.clone());
        v.write_memory(&m).unwrap();
    }
    v.save("legacy import").unwrap();
    let (plans, _) = hippo::import::plan_and_import_claude(&v, &src, false).unwrap();
    assert_eq!(hippo::import::count(&plans, hippo::import::Status::New), 0, "{plans:?}");
    let names: Vec<String> = v.list().unwrap().into_iter().map(|m| m.name).collect();
    assert_eq!(names, ["notes", "picklescan-venv"], "matched in place, no duplicates");
}
