//! Local, append-only action log: `<vault>/.hippo/log.jsonl`, one JSON object per line.
//! Informational (what happened, when, from where) — never canonical, never uploaded.
//! Best-effort: logging must never make a user action fail.

use serde_json::{Value, json};
use std::io::Write;
use std::path::Path;

pub fn record(vault: &Path, action: &str, details: Value) {
    let mut entry = json!({"at": crate::util::now().to_rfc3339(), "action": action});
    if let (Some(e), Value::Object(d)) = (entry.as_object_mut(), details) {
        e.extend(d);
    }
    if let Some(agent) = crate::safety::detected_agent() {
        entry["via"] = json!(crate::safety::agent_name().unwrap_or(agent));
    }
    let path = vault.join(".hippo").join("log.jsonl");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{entry}");
    }
}

/// Last `n` entries, oldest first.
pub fn tail(vault: &Path, n: usize) -> Vec<Value> {
    let text = std::fs::read_to_string(vault.join(".hippo").join("log.jsonl")).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..]
        .iter()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}
