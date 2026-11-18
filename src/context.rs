//! The memory index an agent gets when a session starts, and again after its context is
//! compacted: what it can `recall` here, without having to think of searching first.

use crate::memory::{Memory, Visibility};
use crate::project::Scope;

/// Characters, header included. Claude Code moves hook output over 10k to a file.
pub const BUDGET: usize = 8000;

/// This project's memories first, then how the user wants agents to work, then the rest;
/// newest first within each group. Lines are cut once `budget` is reached.
pub fn index(memories: &[Memory], scope: &Scope, budget: usize) -> String {
    let mut seen: Vec<&Memory> = memories.iter().filter(|m| m.status() != Some("forget") && m.visible_in(scope)).collect();
    let rank = |m: &Memory| match (m.visibility(), m.kind.as_str()) {
        (Visibility::Project(_), _) => 0,
        (_, "feedback" | "user") => 1,
        _ => 2,
    };
    seen.sort_by(|a, b| rank(a).cmp(&rank(b)).then(b.updated().cmp(&a.updated())).then(a.name.cmp(&b.name)));
    if seen.is_empty() {
        return String::new();
    }
    let mut out = format!(
        "🦛 hippo: {} memories for this session (name [type] description). `recall` the ones relevant \
         to the task, `search_memories` for anything else. They are general guidance from past \
         sessions and can be outdated; what the user says now wins.\n",
        seen.len()
    );
    for (i, m) in seen.iter().enumerate() {
        let line = format!("- {} [{}]{} {}\n", m.name, m.kind, if m.needs_review() { " (unconfirmed)" } else { "" }, m.description);
        if out.len() + line.len() > budget {
            out += &format!("- … {} more: `list_memories` or `search_memories`\n", seen.len() - i);
            break;
        }
        out += &line;
    }
    out
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
    fn stays_within_budget() {
        let ms: Vec<Memory> = (0..100).map(|i| mem(&format!("m{i}"), "note", None)).collect();
        let idx = index(&ms, &Scope::default(), 1000);
        assert!(idx.len() < 1100 && idx.trim_end().ends_with("`search_memories`"));
        assert!(index(&[], &Scope::default(), BUDGET).is_empty());
    }
}
