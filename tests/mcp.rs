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
    assert_eq!(shown.unwrap(), "🦛 Decide these changes to your memory?\nApply 1: 1 new\nEvery diff: decline, then run `hippo review` in a terminal.");
    assert!(out.contains("1 applied"), "{out}");
    assert!(!v.read_memory("concise").unwrap().needs_review());
}
