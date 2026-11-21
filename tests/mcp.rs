//! The MCP server over stdio: hippo's own dialog decides proposed changes.

use hippo::crypto::Key;
use hippo::memory::Memory;
use hippo::vault::Vault;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

struct Client {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Client {
    fn start(vault: &Vault, home: &std::path::Path, dialogs: bool) -> Client {
        let mut child = Command::new(env!("CARGO_BIN_EXE_hippo"))
            .current_dir(home)
            .env("HIPPO_HOME", home)
            .env("CLAUDE_CODE_SESSION_ID", "5e55101d-0000-0000-0000-000000000000")
            .arg("--vault")
            .arg(&vault.root)
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let (stdin, stdout) = (
            child.stdin.take().unwrap(),
            BufReader::new(child.stdout.take().unwrap()),
        );
        let mut c = Client {
            child,
            stdin,
            stdout,
        };
        let caps = if dialogs {
            json!({"elicitation": {"form": {}}})
        } else {
            json!({})
        };
        c.send(json!({"jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {"protocolVersion": "2025-11-25", "capabilities": caps, "clientInfo": {"name": "test", "version": "0"}}}));
        c.read();
        c.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        c
    }
    fn send(&mut self, v: Value) {
        writeln!(self.stdin, "{v}").unwrap();
    }
    fn read(&mut self) -> Value {
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }
    /// Call a tool; answer hippo's dialog with `action` if it shows one. Returns (dialog text, result text).
    fn call(&mut self, tool: &str, args: Value, action: &str) -> (Option<String>, String) {
        self.send(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": tool, "arguments": args}}));
        let mut shown = None;
        loop {
            let m = self.read();
            if m["method"] == "elicitation/create" {
                shown = m["params"]["message"].as_str().map(String::from);
                self.send(json!({"jsonrpc": "2.0", "id": m["id"], "result": {"action": action, "content": {}}}));
            } else if m["id"] == 1 {
                return (
                    shown,
                    m["result"]["content"][0]["text"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string()
                        + m["error"]["message"].as_str().unwrap_or_default(),
                );
            }
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

fn setup() -> (tempfile::TempDir, Vault) {
    let tmp = tempfile::tempdir().unwrap();
    let key = Key::generate(&tmp.path().join("key.txt")).unwrap();
    let v = Vault::init(&tmp.path().join("v"), &key).unwrap();
    for (n, b) in [
        ("pr-style", "Conventional commits."),
        ("atomic-prs", "One intent per PR."),
    ] {
        v.remember(&Memory {
            name: n.into(),
            kind: "feedback".into(),
            body: b.into(),
            ..Default::default()
        })
        .unwrap();
    }
    (tmp, v)
}

fn merge() -> Value {
    json!({"changes": [{"title": "merge into pr-style", "why": "same topic", "ops": [
        {"op": "write", "name": "pr-style", "content": "Conventional commits. One intent per PR."},
        {"op": "remove", "name": "atomic-prs", "into": "pr-style"}]}]})
}

#[test]
fn proposed_changes_apply_only_when_the_user_says_yes() {
    let (tmp, v) = setup();
    let mut c = Client::start(&v, tmp.path(), true);
    let (shown, out) = c.call("propose_changes", merge(), "decline");
    let shown = shown.expect("hippo shows a dialog");
    assert_eq!(shown, "🦛 Apply 1 change to your memory?\n1 edit · 1 deletion (kept in history)\nTo see more first: decline and ask.");
    assert!(out.contains("nothing was applied"), "{out}");
    assert!(v.read_memory("atomic-prs").is_ok() && v.pending_changes().unwrap().is_empty());

    let (_, out) = c.call("propose_changes", merge(), "accept");
    assert!(out.contains("applied 1 change"), "{out}");
    assert!(v.read_memory("atomic-prs").is_err());
    let pr = v.read_memory("pr-style").unwrap();
    assert!(!pr.needs_review() && pr.body.contains("One intent"));
}

#[test]
fn without_dialogs_changes_wait_for_hippo_review() {
    let (tmp, v) = setup();
    let mut c = Client::start(&v, tmp.path(), false);
    let (shown, out) = c.call("propose_changes", merge(), "accept");
    assert!(shown.is_none() && out.contains("as drafts"), "{out}");
    assert_eq!(v.pending_changes().unwrap().len(), 1);
}

#[test]
fn pending_changes_are_decided_in_one_dialog() {
    let (tmp, v) = setup();
    v.remember_by(
        &Memory {
            name: "concise".into(),
            kind: "feedback".into(),
            body: "Be brief.".into(),
            ..Default::default()
        },
        Some("claude-code"),
    )
    .unwrap();
    let mut c = Client::start(&v, tmp.path(), true);
    let (_, list) = c.call("pending_changes", json!({}), "accept");
    assert!(
        list.contains("new concise") && list.contains("handle `concise`"),
        "{list}"
    );
    let (shown, out) = c.call("decide_changes", json!({"approve": ["concise"]}), "accept");
    assert_eq!(shown.unwrap(), "🦛 Decide these changes to your memory?\nApply 1: 1 new\nTo see more first: decline and ask.");
    assert!(out.contains("1 applied"), "{out}");
    assert!(!v.read_memory("concise").unwrap().needs_review());
}

#[test]
fn changes_in_other_projects_are_approved_by_count_only() {
    let (tmp, v) = setup();
    let mut m = Memory { name: "elsewhere-only".into(), kind: "project".into(), body: "Secret plan.".into(), ..Default::default() };
    m.extra.insert("visibility".into(), "project:other".into());
    v.remember_by(&m, Some("claude-code")).unwrap();
    let mut c = Client::start(&v, tmp.path(), true);
    let (_, list) = c.call("pending_changes", json!({}), "accept");
    assert!(list.contains("1 more in other projects") && !list.contains("Secret plan"), "{list}");
    let (shown, out) = c.call("decide_changes", json!({"elsewhere": true}), "accept");
    assert_eq!(shown.unwrap(), "🦛 Decide these changes to your memory?\nApply 1 in other projects: other 1 (1 new)\nTo see more first: decline and ask.");
    assert!(out.contains("1 applied"), "{out}");
    assert!(!v.read_memory("elsewhere-only").unwrap().needs_review());
}

#[test]
fn project_writes_say_where_they_are_seen_and_refuse_unknown_projects() {
    let (tmp, v) = setup();
    let mut c = Client::start(&v, tmp.path(), false);
    let note = |name: &str, vis: &str| json!({"name": name, "content": "Engine notes.", "visibility": vis});
    let (_, out) = c.call("remember", note("here-notes", "project"), "accept");
    let here = hippo::project::Registry::load(&v.root).unwrap().projects.into_keys().next().unwrap();
    assert!(out.contains(&format!("only in project {here}")), "{out}");

    let (_, out) = c.call("remember", note("engine-notes", "project:safetensors"), "accept");
    assert!(out.contains(&format!("no project `safetensors` (this folder is `{here}`); known: {here}")), "{out}");
    assert!(v.read_memory("engine-notes").is_err());

    let log = std::fs::read_to_string(v.root.join(".hippo/log.jsonl")).unwrap();
    let line = log.lines().find(|l| l.contains("\"remember\"")).unwrap();
    assert!(line.contains("\"session\":\"5e55101d\"") && line.contains(&format!("\"visibility\":\"project:{here}\"")), "{line}");
}

#[test]
fn sessions_without_initialize_are_logged_once() {
    let (tmp, v) = setup();
    let mut child = Command::new(env!("CARGO_BIN_EXE_hippo"))
        .current_dir(tmp.path())
        .env("HIPPO_HOME", tmp.path())
        .args(["--vault".as_ref(), v.root.as_os_str(), "mcp".as_ref()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let (mut stdin, mut stdout) = (child.stdin.take().unwrap(), BufReader::new(child.stdout.take().unwrap()));
    let meta = json!({"io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": {"name": "claude-code", "version": "2.1"},
        "io.modelcontextprotocol/clientCapabilities": {"elicitation": {"form": {}}}});
    for id in 1..=2 {
        let req = json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": "list_memories", "arguments": {}, "_meta": meta}});
        writeln!(stdin, "{req}").unwrap();
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        assert!(line.contains("pr-style"), "{line}");
    }
    let _ = child.kill();
    let log = std::fs::read_to_string(v.root.join(".hippo/log.jsonl")).unwrap();
    let sessions: Vec<&str> = log.lines().filter(|l| l.contains("\"action\":\"session\"")).collect();
    assert_eq!(sessions.len(), 1, "{log}");
    assert!(sessions[0].contains("\"client\":\"claude-code\"") && sessions[0].contains("\"protocol\":\"2026-07-28\"") && sessions[0].contains("\"dialogs\":true"), "{}", sessions[0]);
}

#[test]
fn rules_saved_elsewhere_reach_a_running_session_once() {
    let (tmp, v) = setup();
    let mut c = Client::start(&v, tmp.path(), false);
    let (_, out) = c.call("list_memories", json!({}), "accept");
    assert!(!out.contains("new since"), "{out}");
    let rule = |name: &str, kind: &str| Memory { name: name.into(), kind: kind.into(), body: "Use tabs.".into(), ..Default::default() };
    v.remember(&rule("tabs", "feedback")).unwrap();
    v.remember(&rule("trip-notes", "note")).unwrap();
    let (_, out) = c.call("remember", json!({"name": "my-rule", "content": "Mine.", "type": "feedback"}), "accept");
    let lines: Vec<&str> = out.lines().collect();
    assert!(lines[0].starts_with("🦛 hippo remembered `my-rule`"), "{out}");
    assert_eq!(lines[1], "🦛 new since this session started: tabs. `recall` them: they may change how you should work.");
    v.remember(&rule("tabs", "feedback")).unwrap();
    v.remember(&rule("tabs-too", "user")).unwrap();
    let (_, out) = c.call("list_memories", json!({}), "accept");
    assert!(out.lines().nth(1).unwrap().contains("new since this session started: tabs-too."), "{out}");
    let (_, out) = c.call("list_memories", json!({}), "accept");
    assert!(!out.contains("new since"), "{out}");
}

#[test]
fn handoffs_replace_the_last_one_on_their_topic_without_review() {
    let (tmp, v) = setup();
    let mut c = Client::start(&v, tmp.path(), false);
    let (_, out) = c.call("handoff", json!({"topic": "0.9 release", "content": "Tagging next."}), "accept");
    let key = hippo::project::Registry::load(&v.root).unwrap().projects.into_keys().next().unwrap();
    let name = format!("{key}-handoff-0-9-release");
    assert!(out.starts_with(&format!("🦛 hippo handoff `{name}` saved · only in project {key} · ")) && !out.contains("replaces"), "{out}");
    let m = v.read_memory(&name).unwrap();
    assert_eq!((m.kind.as_str(), m.description.as_str(), m.status()), ("handoff", "0.9 release", None));
    assert_eq!(m.extra.get("visibility"), Some(&format!("project:{key}")));
    assert_eq!(m.extra.get("by").map(String::as_str), Some("test"));

    let (_, out) = c.call("handoff", json!({"topic": "0.9 release", "content": "Tagged; publish wheels."}), "accept");
    assert!(out.contains("replaces the last one on this topic"), "{out}");
    assert_eq!(v.read_memory(&name).unwrap().body.trim(), "Tagged; publish wheels.");
    let log = std::fs::read_to_string(v.root.join(".hippo/log.jsonl")).unwrap();
    assert!(log.lines().rfind(|l| l.contains("\"action\":\"handoff\"")).unwrap().contains("\"new\":false"), "{log}");
    let (_, out) = c.call("pending_changes", json!({}), "accept");
    assert!(out.starts_with("🦛 hippo 0 change(s) waiting"), "{out}");

    let (_, out) = c.call("remember", json!({"name": "release-state", "content": "Tagging.", "type": "handoff"}), "accept");
    assert!(out.contains("use the `handoff` tool") && v.read_memory("release-state").is_err(), "{out}");
    let (_, out) = c.call("handoff", json!({"topic": "x", "content": "y", "project": "nope"}), "accept");
    assert!(out.contains(&format!("no project `nope` here; this folder's: {key}")), "{out}");
}

#[test]
fn a_folder_in_several_projects_names_the_handoff_project() {
    let (tmp, v) = setup();
    let dir = tmp.path().canonicalize().unwrap();
    let git = |args: &[&str]| {
        let out = Command::new("git").arg("-C").arg(&dir).args(args).env("GIT_CONFIG_GLOBAL", "/dev/null").output().unwrap();
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    };
    git(&["init", "-q"]);
    git(&["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false", "commit", "-q", "--allow-empty", "-m", "root"]);
    let id = hippo::project::Ident { dir: dir.clone(), toplevel: Some(dir.clone()), roots: vec![git(&["rev-parse", "HEAD"])] };
    v.update_registry("link", |r| r.learn("engine", &id) | r.learn("bindings", &id)).unwrap();
    let mut c = Client::start(&v, tmp.path(), false);
    let (_, out) = c.call("handoff", json!({"topic": "ci", "content": "Fixing the macOS job."}), "accept");
    assert!(out.contains("this folder is in several projects (bindings, engine): say which with `project`"), "{out}");
    let (_, out) = c.call("handoff", json!({"topic": "ci", "content": "Fixing the macOS job.", "project": "engine"}), "accept");
    assert!(out.contains("handoff `engine-handoff-ci` saved · only in project engine"), "{out}");
}

#[test]
fn a_rename_and_an_edit_go_in_one_change() {
    let ops = json!({"changes": [{"title": "rename and tighten", "ops": [
        {"op": "rename", "from": "pr-style", "to": "commit-style"},
        {"op": "write", "name": "commit-style", "content": "Conventional commits, short subjects."}]}]});
    for dialogs in [true, false] {
        let (tmp, v) = setup();
        let mut c = Client::start(&v, tmp.path(), dialogs);
        let (shown, out) = c.call("propose_changes", ops.clone(), "accept");
        assert_eq!(shown.is_some(), dialogs, "{out}");
        let m = v.read_memory("commit-style").unwrap();
        assert_eq!((m.kind.as_str(), m.body.trim(), m.needs_review()), ("feedback", "Conventional commits, short subjects.", !dialogs), "{out}");
        assert_eq!(v.read_memory("pr-style").is_ok(), !dialogs, "pending removal until reviewed");
    }
}
