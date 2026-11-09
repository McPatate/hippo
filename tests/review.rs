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
    assert!(Registry::load(&v.root).projects["bucket-memory"].roots.is_empty(), "kept, unlinked");

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
