//! Changes: an agent's merge, rename, rescope or removal, taken or left as a whole.

use hippo::change::{Change, Op, describe};
use hippo::crypto::Key;
use hippo::memory::Memory;
use hippo::vault::Vault;

fn mem(name: &str, body: &str) -> Memory {
    Memory { name: name.into(), kind: "feedback".into(), description: format!("about {name}"), body: body.into(), ..Default::default() }
}

fn vault() -> (tempfile::TempDir, Vault) {
    let tmp = tempfile::tempdir().unwrap();
    let key = Key::generate(&tmp.path().join("key.txt")).unwrap();
    let v = Vault::init(&tmp.path().join("v"), &key).unwrap();
    for (n, b) in [("pr-style", "Conventional commits."), ("atomic-prs", "One intent per PR."), ("feedback-concise", "Be brief.")] {
        v.remember(&mem(n, b)).unwrap();
    }
    (tmp, v)
}

fn merge() -> Change {
    Change {
        title: "merge into pr-style".into(),
        why: "they say the same thing".into(),
        ops: vec![
            Op::Write(mem("pr-style", "Conventional commits. One intent per PR.")),
            Op::Remove { name: "atomic-prs".into(), into: Some("pr-style".into()) },
        ],
    }
}

fn rename() -> Change {
    Change { title: "drop the prefix".into(), why: String::new(), ops: vec![Op::Rename { from: "feedback-concise".into(), to: "concise".into() }] }
}

#[test]
fn a_pending_change_is_one_decision() {
    let (_t, v) = vault();
    v.apply_changes(&[merge(), rename()], "claude-code", false).unwrap();
    let pending = v.pending_changes().unwrap();
    assert_eq!(pending.len(), 2);
    let m = pending.iter().find(|p| p.meta.title == "merge into pr-style").unwrap();
    assert_eq!(m.meta.why, "they say the same thing");
    let lines = describe(&v, &m.ops(), true);
    assert!(lines.contains(&"✎ edit pr-style (content)".to_string()), "{lines:?}");
    assert!(lines.contains(&"✗ delete atomic-prs (its content is in pr-style)".to_string()), "{lines:?}");
    let r = pending.iter().find(|p| p.meta.title == "drop the prefix").unwrap();
    assert_eq!(describe(&v, &r.ops(), true), ["→ rename feedback-concise → concise (same content)"]);

    v.approve_change(m).unwrap();
    assert!(v.read_memory("atomic-prs").is_err());
    let pr = v.read_memory("pr-style").unwrap();
    assert!(!pr.needs_review() && !pr.extra.contains_key("change"));
    v.reject_change(r).unwrap();
    assert!(v.read_memory("concise").is_err());
    let back = v.read_memory("feedback-concise").unwrap();
    assert!(!back.needs_review() && !back.extra.contains_key("into"));
    assert!(v.pending_changes().unwrap().is_empty());
    assert!(!v.root.join("changes.toml").exists(), "settled changes leave no record");
}

#[test]
fn an_approved_change_lands_in_one_commit() {
    let (_t, v) = vault();
    let mut linked = v.read_memory("feedback-concise").unwrap();
    linked.extra.insert("claude_file".into(), "feedback_concise".into());
    v.remember(&linked).unwrap();
    let before = v.git.run(["rev-list", "--count", "HEAD"]).unwrap();
    let scope = Change { title: "scope".into(), why: String::new(), ops: vec![Op::Scope { name: "pr-style".into(), visibility: "project:hippo".into() }] };
    v.apply_changes(&[rename(), scope], "claude-code", true).unwrap();
    let after = v.git.run(["rev-list", "--count", "HEAD"]).unwrap();
    assert_eq!(after.trim().parse::<u32>().unwrap(), before.trim().parse::<u32>().unwrap() + 1);
    assert!(v.read_memory("feedback-concise").is_err());
    let c = v.read_memory("concise").unwrap();
    assert!(!c.needs_review() && c.body.trim() == "Be brief.", "{c:?}");
    assert_eq!(c.extra.get("claude_file").map(String::as_str), Some("feedback_concise"), "Claude's file follows the new name");
    assert_eq!(v.read_memory("pr-style").unwrap().extra.get("visibility").map(String::as_str), Some("project:hippo"));
    assert!(v.pending_changes().unwrap().is_empty());
}

#[test]
fn a_bad_change_touches_nothing() {
    let (_t, v) = vault();
    let head = v.git.head_fast().unwrap();
    let onto = Change { title: "x".into(), why: String::new(), ops: vec![Op::Rename { from: "atomic-prs".into(), to: "pr-style".into() }] };
    assert!(v.apply_changes(&[merge(), onto], "claude-code", false).is_err());
    let missing = Change { title: "x".into(), why: String::new(), ops: vec![Op::Remove { name: "nope".into(), into: None }] };
    assert!(v.apply_changes(&[missing], "claude-code", false).is_err());
    assert_eq!(v.git.head_fast().unwrap(), head);
    assert!(v.pending_changes().unwrap().is_empty());
}

#[test]
fn drafts_from_before_changes_show_renames_as_renames() {
    let (_t, v) = vault();
    v.remember_by(&mem("concise", "Be brief."), Some("claude-code")).unwrap();
    v.request_forget("feedback-concise", "claude-code").unwrap();
    v.remember_by(&mem("pr-style", "Edited."), Some("claude-code")).unwrap();
    let pending = v.pending_changes().unwrap();
    let titles: Vec<&str> = pending.iter().map(|p| p.meta.title.as_str()).collect();
    assert_eq!(titles, ["rename feedback-concise → concise", "edit pr-style"]);
    v.approve_change(&pending[0]).unwrap();
    assert!(v.read_memory("feedback-concise").is_err() && !v.read_memory("concise").unwrap().needs_review());
}

#[test]
fn a_pending_change_reads_as_a_git_diff() {
    let (_t, v) = vault();
    let scope = Change { title: "scope".into(), why: String::new(), ops: vec![Op::Scope { name: "pr-style".into(), visibility: "private".into() }] };
    v.apply_changes(&[rename(), scope], "claude-code", false).unwrap();
    let pending = v.pending_changes().unwrap();
    let diff = |t: &str| v.diff_change(pending.iter().find(|p| p.meta.title == t).unwrap(), false).unwrap();
    let r = diff("drop the prefix");
    assert!(r.contains("rename from a/memories/feedback-concise.md") && r.contains("rename to b/memories/concise.md"), "{r}");
    assert!(diff("scope").contains("+visibility: private"));
}
