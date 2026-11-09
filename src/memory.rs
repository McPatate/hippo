//! Memory file format: Markdown with a small YAML-ish frontmatter.
//!
//! ```text
//! ---
//! name: likes-rust
//! description: Prefers Rust for side projects
//! type: user
//! tags: rust, languages
//! updated: 2026-11-07T01:00:00Z
//! ---
//! Free-form markdown body.
//! ```
//!
//! Parsing is deliberately lenient: unknown keys are preserved, files without frontmatter are
//! valid memories (the whole file is the body). Only `key: value` lines are understood.

use std::collections::BTreeMap;

pub const KINDS: &[&str] = &["user", "feedback", "project", "reference", "note"];

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Memory {
    pub name: String,
    pub description: String,
    pub kind: String,
    pub tags: Vec<String>,
    pub extra: BTreeMap<String, String>,
    pub body: String,
}

impl Memory {
    pub fn parse(name_hint: &str, text: &str) -> Self {
        let mut m = Memory {
            name: name_hint.to_string(),
            kind: "note".into(),
            ..Default::default()
        };
        let normalized = text.replace("\r\n", "\n");
        let Some(rest) = normalized.strip_prefix("---\n") else {
            m.body = normalized;
            return m;
        };
        let Some(end) = rest.find("\n---\n").map(|i| (i, i + 5)).or_else(|| {
            rest.strip_suffix("\n---").map(|r| (r.len(), rest.len()))
        }) else {
            m.body = normalized;
            return m;
        };
        let (front, body) = (&rest[..end.0], &rest[end.1..]);
        for line in front.lines() {
            let Some((k, v)) = line.split_once(':') else { continue };
            let (k, v) = (k.trim(), v.trim().trim_matches('"').to_string());
            match k {
                "name" if !v.is_empty() => m.name = v,
                "description" => m.description = v,
                "type" | "kind" => m.kind = v,
                "tags" => {
                    m.tags = v
                        .trim_matches(|c| c == '[' || c == ']')
                        .split(',')
                        .map(|t| t.trim().trim_matches('"').to_string())
                        .filter(|t| !t.is_empty())
                        .collect()
                }
                _ => {
                    m.extra.insert(k.to_string(), v);
                }
            }
        }
        m.body = body.trim_start_matches('\n').to_string();
        m
    }

    pub fn render(&self) -> String {
        let mut out = String::from("---\n");
        out.push_str(&format!("name: {}\n", one_line(&self.name)));
        if !self.description.is_empty() {
            out.push_str(&format!("description: {}\n", one_line(&self.description)));
        }
        out.push_str(&format!("type: {}\n", one_line(&self.kind)));
        if !self.tags.is_empty() {
            out.push_str(&format!("tags: {}\n", self.tags.join(", ")));
        }
        for (k, v) in &self.extra {
            out.push_str(&format!("{k}: {}\n", one_line(v)));
        }
        out.push_str("---\n");
        out.push_str(self.body.trim_end());
        out.push('\n');
        out
    }
}

fn one_line(s: &str) -> String {
    s.replace(['\n', '\r'], " ").trim().to_string()
}

/// Who may see a memory: frontmatter `visibility: all | private | project:<name>` (default `all`).
/// `private` memories never leave hippo over MCP; `project:<name>` ones only reach agents whose
/// working folder is named `<name>`, or is inside `<name>` when it is an absolute path.
#[derive(Debug, Clone, PartialEq)]
pub enum Visibility {
    All,
    Private,
    Project(String),
}

impl Memory {
    pub fn visibility(&self) -> Visibility {
        match self.extra.get("visibility").map(|v| v.trim()) {
            Some("private") => Visibility::Private,
            Some(v) if v.starts_with("project:") => Visibility::Project(v["project:".len()..].trim().to_string()),
            _ => Visibility::All,
        }
    }

    /// Visible to an agent working in `roots`?
    pub fn visible_in(&self, roots: &[std::path::PathBuf]) -> bool {
        match self.visibility() {
            Visibility::All => true,
            Visibility::Private => false,
            Visibility::Project(p) => roots.iter().any(|r| {
                if p.starts_with('/') {
                    r.starts_with(&p)
                } else {
                    r.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(&p))
                }
            }),
        }
    }
}

/// Turn a free-form name into a safe file stem: lowercase, `[a-z0-9-]`, no path tricks.
pub fn slugify(name: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in name.trim().chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    let out = out.trim_end_matches('-').to_string();
    let out: String = out.chars().take(80).collect();
    if out.is_empty() { "untitled".into() } else { out }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let m = Memory {
            name: "likes-rust".into(),
            description: "Prefers Rust".into(),
            kind: "user".into(),
            tags: vec!["rust".into(), "lang".into()],
            extra: BTreeMap::from([("source".into(), "claude".into())]),
            body: "Uses Rust for side projects.\n\nAnd hippos.\n".into(),
        };
        let parsed = Memory::parse("x", &m.render());
        assert_eq!(parsed, m);
    }

    #[test]
    fn no_frontmatter() {
        let m = Memory::parse("plain", "# Hello\nworld\n");
        assert_eq!(m.name, "plain");
        assert_eq!(m.body, "# Hello\nworld\n");
    }

    #[test]
    fn visibility_scopes() {
        use std::path::PathBuf;
        let mut m = Memory::default();
        let here = [PathBuf::from("/Users/x/work/safetensors")];
        assert!(m.visible_in(&here));
        m.extra.insert("visibility".into(), "project:safetensors".into());
        assert!(m.visible_in(&here));
        assert!(!m.visible_in(&[PathBuf::from("/Users/x/work/hippo")]));
        m.extra.insert("visibility".into(), "project:/Users/x/work/safetensors".into());
        assert!(m.visible_in(&here));
        assert!(m.visible_in(&[PathBuf::from("/Users/x/work/safetensors/bindings/python")]));
        assert!(!m.visible_in(&[PathBuf::from("/Users/x/work/safetensors-old")]));
        m.extra.insert("visibility".into(), "private".into());
        assert!(!m.visible_in(&here));
    }

    #[test]
    fn slugs() {
        assert_eq!(slugify("Likes Rust!"), "likes-rust");
        assert_eq!(slugify("../../etc/passwd"), "etc-passwd");
        assert_eq!(slugify("   "), "untitled");
        assert_eq!(slugify("Café au lait"), "café-au-lait");
    }
}
