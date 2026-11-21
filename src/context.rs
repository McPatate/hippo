//! The memory index an agent gets when a session starts, and again after its context is
//! compacted: what it can `recall` here, without having to think of searching first.

use crate::memory::{Memory, Visibility};
use crate::project::Scope;

/// Characters, header included: Claude Code moves hook output over 10,000 characters to a file
/// and shows the agent only a 2,000-character preview. Only the index is capped; memories
/// themselves can be any size.
pub const BUDGET: usize = 9500;

/// Handoffs not picked up for this long are marked stale.
const STALE_DAYS: i64 = 7;

/// Handoffs first (work to pick up), then this project's memories, then how the user wants
/// agents to work, then the rest; newest first within each group. Lines are cut once `budget` is reached.
pub fn index(memories: &[Memory], scope: &Scope, budget: usize) -> String {
    let mut seen: Vec<&Memory> = memories.iter().filter(|m| m.status() != Some("forget") && m.visible_in(scope)).collect();
    let rank = |m: &Memory| match (m.visibility(), m.kind.as_str()) {
        (_, "handoff") => 0,
        (Visibility::Project(_), _) => 1,
        (_, "feedback" | "user") => 2,
        _ => 3,
    };
    seen.sort_by(|a, b| rank(a).cmp(&rank(b)).then(b.updated().cmp(&a.updated())).then(a.name.cmp(&b.name)));
    if seen.is_empty() {
        return String::new();
    }
    let mut out = format!(
        "🦛 hippo: {} memories for this session{} (name [type] description). `recall` the ones relevant \
         to the task, `search_memories` for anything else. They are general guidance from past \
         sessions and can be outdated; what the user says now wins.\n",
        seen.len(),
        match scope.keys.as_slice() {
            [] => String::new(),
            [k] => format!(" in project `{k}`"),
            ks => format!(" in projects {}", ks.iter().map(|k| format!("`{k}`")).collect::<Vec<_>>().join(", ")),
        }
    );
    for (i, m) in seen.iter().enumerate() {
        let line = format!("- {} [{}]{} {}\n", m.name, label(m), if m.needs_review() { " (unconfirmed)" } else { "" }, m.description);
        if out.len() + line.len() > budget {
            out += &format!("- … {} more: `list_memories` or `search_memories`\n", seen.len() - i);
            break;
        }
        out += &line;
    }
    out
}

/// The type, and for a handoff its age: `handoff, 3h ago` (`, stale` once it sat for a week).
fn label(m: &Memory) -> String {
    let Some(t) = m.updated().filter(|_| m.kind == "handoff") else { return m.kind.clone() };
    let age = crate::util::now() - t;
    let ago = match (age.num_days(), age.num_hours(), age.num_minutes()) {
        (d, _, _) if d > 0 => format!("{d}d ago"),
        (_, h, _) if h > 0 => format!("{h}h ago"),
        (_, _, min) if min > 0 => format!("{min}min ago"),
        _ => "just now".into(),
    };
    format!("handoff, {ago}{}", if age.num_days() >= STALE_DAYS { ", stale" } else { "" })
}

/// Has the index for this project changed since the last session start? Remembers it locally
/// (`.hippo/context.json`), written only on a change.
pub fn changed(vault: &crate::vault::Vault, scope: &Scope, names: &[&str]) -> bool {
    use sha2::Digest;
    let path = vault.meta_dir().join("context.json");
    let mut seen: std::collections::BTreeMap<String, String> =
        std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    let key = scope.keys.first().cloned().unwrap_or_else(|| scope.dir.display().to_string());
    let hash = hex::encode(&sha2::Sha256::digest(names.join("\n").as_bytes())[..8]);
    if seen.get(&key) == Some(&hash) {
        return false;
    }
    seen.insert(key, hash);
    let _ = std::fs::write(&path, serde_json::to_string(&seen).unwrap_or_default());
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem(name: &str, kind: &str, vis: Option<&str>) -> Memory {
        let mut m = Memory { name: name.into(), kind: kind.into(), description: format!("about {name}"), ..Default::default() };
        if let Some(v) = vis {
            m.extra.insert("visibility".into(), v.into());
        }
        m
    }

    #[test]
    fn project_first_then_feedback_and_out_of_scope_hidden() {
        let scope = Scope { dir: "/w/st".into(), keys: vec!["st".into()] };
        let mut gone = mem("gone", "note", None);
        gone.extra.insert("status".into(), "forget".into());
        let ms = [
            mem("misc", "reference", None),
            mem("style", "feedback", None),
            mem("st-plan", "project", Some("project:st")),
            mem("other-plan", "project", Some("project:other")),
            mem("secret", "note", Some("private")),
            gone,
        ];
        let idx = index(&ms, &scope, BUDGET);
        let names: Vec<&str> = idx.lines().skip(1).map(|l| l[2..].split(' ').next().unwrap()).collect();
        assert_eq!(names, ["st-plan", "style", "misc"]);
        assert!(idx.starts_with("🦛 hippo: 3 memories"));
    }

    #[test]
    fn handoffs_first_with_their_age() {
        let scope = Scope { dir: "/w/st".into(), keys: vec!["st".into()] };
        let ago = |days: i64| (crate::util::now() - chrono::TimeDelta::days(days)).format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let mut fresh = mem("st-handoff-release", "handoff", Some("project:st"));
        fresh.extra.insert("updated".into(), (crate::util::now() - chrono::TimeDelta::hours(3)).format("%Y-%m-%dT%H:%M:%SZ").to_string());
        let mut old = mem("st-handoff-ci", "handoff", Some("project:st"));
        old.extra.insert("updated".into(), ago(9));
        let mut plan = mem("st-plan", "project", Some("project:st"));
        plan.extra.insert("updated".into(), ago(0));
        let idx = index(&[mem("style", "feedback", None), plan, old, fresh], &scope, BUDGET);
        let lines: Vec<&str> = idx.lines().skip(1).collect();
        assert_eq!(lines[0], "- st-handoff-release [handoff, 3h ago] about st-handoff-release");
        assert_eq!(lines[1], "- st-handoff-ci [handoff, 9d ago, stale] about st-handoff-ci");
        assert!(lines[2].starts_with("- st-plan [project] ") && lines[3].starts_with("- style [feedback] "), "{idx}");
    }

    #[test]
    fn stays_within_budget() {
        let ms: Vec<Memory> = (0..100).map(|i| mem(&format!("m{i}"), "note", None)).collect();
        let idx = index(&ms, &Scope::default(), 1000);
        assert!(idx.len() < 1100 && idx.trim_end().ends_with("`search_memories`"));
        assert!(index(&[], &Scope::default(), BUDGET).is_empty());
    }
}
