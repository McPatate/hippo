//! Agent changes are drafts until the user approves them.

use hippo::crypto::Key;
use hippo::memory::Memory;
use hippo::vault::Vault;

fn mem(name: &str, body: &str) -> Memory {
    Memory { name: name.into(), kind: "note".into(), body: body.into(), ..Default::default() }
}

fn vault() -> (tempfile::TempDir, Vault) {
    let tmp = tempfile::tempdir().unwrap();
    let key = Key::generate(&tmp.path().join("key.txt")).unwrap();
    let v = Vault::init(&tmp.path().join("v"), &key).unwrap();
    (tmp, v)
}

const AGENT: Option<&str> = Some("claude-code");

#[test]
fn approve_and_reject_agent_edits() {
    let (_t, v) = vault();
    v.remember(&mem("tone", "Be concise.")).unwrap();
    v.remember_by(&mem("tone", "Be verbose."), AGENT).unwrap();
    assert_eq!(v.pending_review().unwrap().len(), 1);
    assert!(v.approved_version("tone").unwrap().unwrap().contains("Be concise."));

    v.reject("tone").unwrap();
    let m = v.read_memory("tone").unwrap();
    assert_eq!((m.body.as_str(), m.status()), ("Be concise.\n", None));

    v.remember_by(&mem("tone", "Be concise, no emojis."), AGENT).unwrap();
    v.approve("tone").unwrap();
    let m = v.read_memory("tone").unwrap();
    assert_eq!((m.body.as_str(), m.status(), m.extra["by"].as_str()), ("Be concise, no emojis.\n", None, "claude-code"));
    assert!(v.pending_review().unwrap().is_empty());
}

#[test]
fn rejecting_a_new_draft_removes_it() {
    let (_t, v) = vault();
    v.remember_by(&mem("guess", "User likes Go."), AGENT).unwrap();
    assert_eq!(v.approved_version("guess").unwrap(), None);
    v.reject("guess").unwrap();
    assert!(v.read_memory("guess").is_err());
}

#[test]
fn agent_forget_waits_for_approval() {
    let (_t, v) = vault();
    v.remember(&mem("a", "keep")).unwrap();
    v.remember(&mem("b", "drop")).unwrap();
    v.request_forget("a", "claude-code").unwrap();
    v.request_forget("b", "claude-code").unwrap();
    assert_eq!(v.read_memory("a").unwrap().status(), Some("forget"));
    v.reject("a").unwrap();
    v.approve("b").unwrap();
    assert_eq!(v.read_memory("a").unwrap().status(), None);
    assert!(v.read_memory("b").is_err());
}

#[test]
fn migrates_folder_scopes_to_project_keys() {
    use hippo::project::{Registry, Scope, migrate};
    let (t, v) = vault();
    let repo = t.path().canonicalize().unwrap().join("code/safetensors");
    std::fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| assert!(std::process::Command::new("git").arg("-C").arg(&repo).args(args).env("GIT_CONFIG_GLOBAL", "/dev/null").output().unwrap().status.success());
    git(&["init", "-q"]);
    git(&["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false", "commit", "-q", "--allow-empty", "-m", "root"]);
    let scoped = |name: &str, vis: &str| {
        let mut m = mem(name, "x");
        m.extra.insert("visibility".into(), vis.into());
        v.remember(&m).unwrap();
    };
    scoped("abs", &format!("project:{}", repo.display()));
    scoped("dead", "project:bucket-memory");
    assert_eq!(migrate(&v).unwrap(), 1);
    assert_eq!(v.read_memory("abs").unwrap().extra["visibility"], "project:safetensors");
    assert!(Registry::load(&v.root).unwrap().projects["bucket-memory"].roots.is_empty(), "kept, unlinked");

    // The repo moves: still the same project.
    let moved = repo.with_file_name("st-renamed");
    std::fs::rename(&repo, &moved).unwrap();
    let here = Scope::here(&v, &moved);
    assert_eq!(here.keys, vec!["safetensors"]);
    assert!(v.read_memory("abs").unwrap().visible_in(&here));
    assert!(!v.read_memory("dead").unwrap().visible_in(&here));
}

#[test]
fn rewriting_the_same_memory_is_a_no_op() {
    let (_t, v) = vault();
    let m = Memory { name: "Prefers Rust".into(), kind: "note".into(), body: "Uses Rust.".into(), ..Default::default() };
    v.remember(&m).unwrap();
    assert_eq!(v.remember_by(&m, AGENT).unwrap(), None, "approved memory stays approved");
    assert_eq!(v.read_memory("prefers-rust").unwrap().status(), None);
}

#[test]
fn registry_is_never_overwritten_when_unreadable_or_learned_from_subfolders() {
    use hippo::project::{FILE, Ident, Registry, Scope};
    let (t, v) = vault();
    let work = t.path().canonicalize().unwrap().join("work");
    std::fs::create_dir_all(work.join("newrepo")).unwrap();
    v.update_registry("link", |r| r.learn("work", &Ident::of(&work))).unwrap();

    // A folder inside `work` is in `work`, but isn't added to it.
    assert_eq!(Scope::here(&v, &work.join("newrepo")).keys, vec!["work"]);
    assert_eq!(Registry::load(&v.root).unwrap().projects["work"].paths.len(), 1);

    // An unreadable registry: no project scope (fail closed), and the file is left alone.
    let conflicted = "<<<<<<< ours\n[projects.work]\n=======\n>>>>>>> theirs\n";
    std::fs::write(v.root.join(FILE), conflicted).unwrap();
    assert!(Scope::here(&v, &work).keys.is_empty());
    assert!(v.update_registry("x", |_| ()).is_err());
    assert_eq!(std::fs::read_to_string(v.root.join(FILE)).unwrap(), conflicted);
}

#[test]
fn claude_auto_memory_is_collected_once_per_change() {
    use hippo::import::{Status, count, plan_and_import_claude};
    let (t, v) = vault();
    let dir = t.path().join("claude/-Users-x-work-alpha/memory");
    std::fs::create_dir_all(&dir).unwrap();
    let write = |f: &str, body: &str| std::fs::write(dir.join(f), format!("---\nname: {f}\ndescription: d\ntype: feedback\n---\n{body}\n")).unwrap();
    let import = || plan_and_import_claude(&v, &t.path().join("claude"), false, AGENT).unwrap().0;
    write("tone.md", "Be concise.");
    write("gone.md", "Old rule.");
    write("key.md", "token hf_abcdefghijklmnopqrstuvwxyz0123456789");
    let p = import();
    assert_eq!((count(&p, Status::New), count(&p, Status::Secret)), (2, 1), "{p:?}");
    assert!(v.read_memory("key").is_err(), "secrets stay out");
    assert_eq!(v.read_memory("tone").unwrap().status(), Some("draft"));

    // The user edits one and deletes the other in hippo: Claude's unchanged files don't undo that.
    let mut tone = v.read_memory("tone").unwrap();
    tone.body = "Be concise, no emojis.\n".into();
    v.remember(&tone).unwrap();
    v.reject("gone").unwrap();
    assert!(import().iter().all(|p| p.status == Status::Unchanged));
    assert!(v.read_memory("tone").unwrap().body.contains("no emojis"));
    assert!(v.read_memory("gone").is_err());

    // Claude changes its file: that change comes in, as a draft to review.
    write("tone.md", "Be concise. Use French.");
    assert_eq!(count(&import(), Status::Updated), 1);
    let m = v.read_memory("tone").unwrap();
    assert!(m.body.contains("French") && m.needs_review());
}

#[test]
fn vaults_imported_before_the_ledger_skip_what_claude_has_not_touched() {
    use hippo::import::{LEDGER, Status, count, plan_and_import_claude};
    let (t, v) = vault();
    let from = t.path().join("claude");
    let dir = from.join("-Users-x-work-alpha/memory");
    std::fs::create_dir_all(&dir).unwrap();
    for f in ["kept.md", "deleted.md"] {
        std::fs::write(dir.join(f), format!("---\nname: {f}\ntype: feedback\n---\nx\n")).unwrap();
    }
    plan_and_import_claude(&v, &from, false, None).unwrap();
    // As if imported by an older hippo: no ledger. The user deleted one since.
    std::fs::remove_file(v.root.join(LEDGER)).unwrap();
    v.forget("deleted").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100)); // commit times are in seconds
    std::fs::write(dir.join("new.md"), "---\nname: new\ntype: feedback\n---\ny\n").unwrap();
    let p = plan_and_import_claude(&v, &from, false, None).unwrap().0;
    assert_eq!((count(&p, Status::New), count(&p, Status::Unchanged)), (1, 2), "{p:?}");
    assert!(v.read_memory("deleted").is_err());
}
