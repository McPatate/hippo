//! Wire hippo into an agent harness (and back out). Claude Code for now:
//! - MCP server registered at **user** scope (every project), via the `claude` CLI;
//! - a delimited memory-instructions block in `~/.claude/CLAUDE.md`;
//! - optionally the status line, wrapping any existing one (restored on disconnect).
//!
//! Everything is reversible with `hippo disconnect claude`.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::Command;

const START: &str = "<!-- hippo:start (managed by `hippo connect claude`; `hippo disconnect claude` removes it) -->";
const END: &str = "<!-- hippo:end -->";
const BLOCK: &str = "# Memory

Use the **hippo** MCP server as long-term memory (the user's own encrypted, portable vault):
- At the start of a task, `search_memories` / `list_memories` for relevant context.
- Save durable facts, preferences and feedback with `remember` (one topic per memory, short kebab-case names; update instead of duplicating).
- Do not write file-based memories under `~/.claude/projects/*/memory/`; hippo replaces them.
- Never store secrets in memory.
- After saving, updating or forgetting a memory, mention it in one short line (e.g. `🦛 remembered prefers-rust`).";
/// What an early hippo version wrote to CLAUDE.md without markers.
const LEGACY_PREFIX: &str = "# Memory\n\nUse the **hippo** MCP server as long-term memory";

fn claude_dir() -> PathBuf {
    if let Ok(d) = std::env::var("CLAUDE_CONFIG_DIR") {
        return PathBuf::from(d);
    }
    dirs::home_dir().unwrap_or_default().join(".claude")
}

pub fn claude_available() -> bool {
    Command::new("claude").arg("--version").output().is_ok_and(|o| o.status.success())
}

fn exe() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "hippo".into())
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[derive(Debug, Default, serde::Serialize)]
pub struct Report {
    pub mcp: String,
    pub instructions: String,
    pub statusline: String,
}

/// Is hippo registered as a user-scope MCP server?
pub fn mcp_connected() -> bool {
    Command::new("claude")
        .args(["mcp", "get", "hippo"])
        .output()
        .is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).contains("User"))
}

fn connect_mcp() -> Result<String> {
    if mcp_connected() {
        return Ok("already connected (user scope)".into());
    }
    let out = Command::new("claude")
        .args(["mcp", "add", "-s", "user", "hippo", "--", &exe(), "mcp"])
        .output()
        .context("could not run `claude`")?;
    if !out.status.success() {
        bail!("`claude mcp add` failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok("added (user scope: every project)".into())
}

fn disconnect_mcp() -> String {
    let mut removed = vec![];
    for scope in ["user", "local"] {
        let ok = Command::new("claude")
            .args(["mcp", "remove", "-s", scope, "hippo"])
            .output()
            .is_ok_and(|o| o.status.success());
        if ok {
            removed.push(scope);
        }
    }
    if removed.is_empty() { "was not registered".into() } else { format!("removed ({} scope)", removed.join(" + ")) }
}

fn write_instructions() -> Result<String> {
    let path = claude_dir().join("CLAUDE.md");
    let current = std::fs::read_to_string(&path).unwrap_or_default();
    let block = format!("{START}\n{BLOCK}\n{END}");
    let (next, what) = if let (Some(s), Some(e)) = (current.find(START), current.find(END)) {
        let next = format!("{}{block}{}", &current[..s], &current[e + END.len()..]);
        (next, if current[s..e + END.len()] == block { "already present" } else { "updated" })
    } else if current.starts_with(LEGACY_PREFIX) {
        // Replace the unmarked block an earlier version wrote (it was the whole file).
        (format!("{block}\n"), "migrated to a managed block")
    } else if current.trim().is_empty() {
        (format!("{block}\n"), "added")
    } else {
        (format!("{}\n\n{block}\n", current.trim_end()), "added")
    };
    if next != current {
        crate::util::atomic_write(&path, next.as_bytes())?;
    }
    Ok(format!("{what} in ~/.claude/CLAUDE.md"))
}

fn remove_instructions() -> Result<String> {
    let path = claude_dir().join("CLAUDE.md");
    let current = std::fs::read_to_string(&path).unwrap_or_default();
    let next = if let (Some(s), Some(e)) = (current.find(START), current.find(END)) {
        format!("{}{}", current[..s].trim_end(), &current[e + END.len()..]).trim().to_string()
    } else if current.starts_with(LEGACY_PREFIX) {
        String::new()
    } else {
        return Ok("not present".into());
    };
    if next.is_empty() {
        std::fs::remove_file(&path)?;
    } else {
        crate::util::atomic_write(&path, format!("{next}\n").as_bytes())?;
    }
    Ok("removed from ~/.claude/CLAUDE.md".into())
}

fn settings_path() -> PathBuf {
    claude_dir().join("settings.json")
}

fn load_settings() -> Result<Value> {
    let p = settings_path();
    if !p.exists() {
        return Ok(json!({}));
    }
    let text = std::fs::read_to_string(&p)?;
    serde_json::from_str(&text).with_context(|| format!("{} is not valid JSON — not touching it", p.display()))
}

fn save_settings(v: &Value) -> Result<()> {
    let p = settings_path();
    let backup = claude_dir().join("settings.json.hippo-backup");
    if p.exists() && !backup.exists() {
        std::fs::copy(&p, &backup)?; // one-time safety copy of the original
    }
    crate::util::atomic_write(&p, format!("{}\n", serde_json::to_string_pretty(v)?).as_bytes())
}

fn is_ours(cmd: &str) -> bool {
    cmd.contains("hippo statusline") || cmd.contains(&format!("{} statusline", exe()))
}

fn connect_statusline(pixel: bool) -> Result<String> {
    let mut settings = load_settings()?;
    let obj = settings.as_object_mut().context("settings.json is not a JSON object")?;
    let existing = obj.get("statusLine").cloned();
    let prior_cmd = existing.as_ref().and_then(|s| s["command"].as_str()).map(str::to_string);
    // Remember someone else's status line so disconnect can restore it exactly.
    let mut gc = crate::config::GlobalConfig::load()?;
    let wrapped = match &prior_cmd {
        Some(c) if is_ours(c) => gc.claude_statusline_backup.as_ref().and_then(|b| serde_json::from_str::<Value>(b).ok()).and_then(|v| v["command"].as_str().map(str::to_string)),
        Some(c) => {
            gc.claude_statusline_backup = Some(serde_json::to_string(existing.as_ref().unwrap())?);
            gc.save()?;
            Some(c.clone())
        }
        None => None,
    };
    let mut cmd = format!("{} statusline", exe());
    if pixel {
        cmd = format!("HIPPO_PIXEL=1 {cmd}");
    }
    if let Some(w) = &wrapped {
        cmd += &format!(" --wrap {}", sh_quote(w));
    }
    let mut line = json!({"type": "command", "command": cmd, "refreshInterval": if pixel { 1 } else { 2 }});
    if let Some(p) = existing.as_ref().and_then(|e| e.get("padding")) {
        line["padding"] = p.clone();
    }
    obj.insert("statusLine".into(), line);
    save_settings(&settings)?;
    Ok(match wrapped {
        Some(_) => "set (wrapping your existing status line)".into(),
        None => "set".into(),
    })
}

fn disconnect_statusline() -> Result<String> {
    let mut settings = load_settings()?;
    let Some(obj) = settings.as_object_mut() else { return Ok("untouched".into()) };
    let ours = obj.get("statusLine").and_then(|s| s["command"].as_str()).is_some_and(is_ours);
    if !ours {
        return Ok("not ours, untouched".into());
    }
    let mut gc = crate::config::GlobalConfig::load()?;
    let what = match gc.claude_statusline_backup.take() {
        Some(b) => {
            obj.insert("statusLine".into(), serde_json::from_str(&b)?);
            "restored your previous status line"
        }
        None => {
            obj.remove("statusLine");
            "removed"
        }
    };
    save_settings(&settings)?;
    gc.save()?;
    Ok(what.into())
}

pub fn connect_claude(statusline: bool, pixel: bool) -> Result<Report> {
    if !claude_available() {
        bail!("Claude Code CLI (`claude`) not found on PATH\n  → Install Claude Code, or add the MCP server manually: `hippo mcp`");
    }
    Ok(Report {
        mcp: connect_mcp()?,
        instructions: write_instructions()?,
        statusline: if statusline { connect_statusline(pixel)? } else { "skipped".into() },
    })
}

pub fn disconnect_claude() -> Result<Report> {
    Ok(Report {
        mcp: if claude_available() { disconnect_mcp() } else { "claude CLI not found".into() },
        instructions: remove_instructions()?,
        statusline: disconnect_statusline()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_survives_single_quotes() {
        assert_eq!(sh_quote("echo 'hi' there"), r"'echo '\''hi'\'' there'");
    }
}
