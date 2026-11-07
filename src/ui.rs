//! Terminal output helpers.

use crate::backup::Status;
use crate::util::{ago, pretty_path};
use crate::vault::Vault;
use std::io::IsTerminal;

pub fn color() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

fn paint(code: &str, s: &str, on: bool) -> String {
    if on { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
}
pub fn green(s: &str) -> String { paint("32", s, color()) }
pub fn yellow(s: &str) -> String { paint("33", s, color()) }
pub fn red(s: &str) -> String { paint("31", s, color()) }
pub fn dim(s: &str) -> String { paint("2", s, color()) }
pub fn bold(s: &str) -> String { paint("1", s, color()) }

/// The three states, at a glance.
pub fn status_text(vault: &Vault, s: &Status, colored: bool) -> String {
    let c = |code: &str, t: &str| paint(code, t, colored);
    let mut out = format!("🦛 hippo · {}\n", pretty_path(&vault.root));

    // 1. local
    let local = match (s.dirty_files, s.unverified_commits) {
        (0, 0) => c("32", "● local     all changes saved and backed up"),
        (d, u) => {
            let mut parts = vec![];
            if d > 0 { parts.push(format!("{d} unsaved file change(s)")); }
            if u > 0 { parts.push(format!("{u} saved change(s) not yet backed up")); }
            c("33", &format!("● local     {}", parts.join(", ")))
        }
    };
    out.push_str(&format!("  {local}\n"));

    // 2. uploaded / 3. verified
    let st = &s.state;
    match (&s.remote, st.uploaded_at) {
        (None, _) => out.push_str(&format!("  {}\n", c("31", "● backup    not configured — nothing leaves this machine  → `hippo remote set`"))),
        (Some(_), None) => out.push_str(&format!("  {}\n", c("33", "● uploaded  never"))),
        (Some(_), Some(t)) => out.push_str(&format!("  {}\n", c(if st.uploaded_seq == st.verified_seq { "32" } else { "33" }, &format!("● uploaded  #{} · {}", st.uploaded_seq, ago(t))))),
    }
    if s.remote.is_some() {
        match st.verified_at {
            Some(t) => out.push_str(&format!("  {}\n", c("32", &format!("● verified  #{} · {} ✓ recoverable", st.verified_seq, ago(t))))),
            None => out.push_str(&format!("  {}\n", c("31", "● verified  no verified recovery point yet"))),
        }
    }
    if let Some(p) = &st.pending {
        out.push_str(&format!("  {}\n", c("33", &format!("↻ backup #{} interrupted — will resume on next backup", p.seq))));
    }
    if let Some(e) = &st.last_error {
        out.push_str(&format!("  {}\n", c("31", &format!("✗ last backup failed: {}", e.lines().next().unwrap_or("")))));
    }
    let secrets = crate::secrets::load_findings(&vault.root);
    if !secrets.is_empty() {
        out.push_str(&format!("  {}\n", c("31", &format!("⚠ {} possible secret(s) in history — run `hippo scan`", secrets.len()))));
    }
    if !s.conflicts.is_empty() {
        out.push_str(&format!("  {}\n", c("31", &format!("⚠ merge conflict in: {}", s.conflicts.join(", ")))));
    }
    if let Some(r) = &s.remote {
        out.push_str(&c("2", &format!("  remote  {}\n", r.display())));
    }
    out.push_str(&c("2", &format!("  key     {}\n", pretty_path(&crate::config::key_path()))));
    out
}

pub const KEY_DISCLAIMER: &str = "\
⚠️  Your vault key lives only on this machine. Nothing is cloud-backed yet.
   If you lose this key, you lose access to your backups — nobody, including us,
   can recover them. Store a copy somewhere safe: a password manager (e.g. a
   1Password secure note) or printed on paper. `hippo key show` prints it.";
