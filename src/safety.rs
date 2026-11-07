//! Guards for destructive commands (anything that deletes remote data).
//!
//! We cannot *prove* a human is at the keyboard, but we can make it deliberate:
//! - refuse when a known AI-agent environment is detected,
//! - refuse when stdin is not an interactive terminal (agents, scripts, pipes),
//! - require typing a confirmation phrase.
//!
//! This is a speed bump, not a security boundary: anything with shell access to your account
//! could fake a TTY. The real protection is that agents only get the MCP interface, which has
//! no destructive remote operation at all.

use anyhow::{Result, bail};
use std::io::{BufRead, IsTerminal, Write};

/// Environment variables set by coding agents / AI CLIs when they run commands.
const AGENT_ENV_VARS: &[&str] = &[
    "AI_AGENT",
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CODEX_SANDBOX",
    "CODEX_MANAGED_BY_NPM",
    "CURSOR_AGENT",
    "GEMINI_CLI",
    "AIDER_MODEL",
    "OPENCODE",
    "HIPPO_MCP",
];

pub fn detected_agent() -> Option<String> {
    AGENT_ENV_VARS
        .iter()
        .find(|v| std::env::var_os(v).is_some())
        .map(|v| v.to_string())
}

/// Ask a human to type `phrase` to proceed. Fails if called from an agent or non-interactively.
pub fn require_human(action: &str, why: &str, phrase: &str) -> Result<()> {
    if let Some(var) = detected_agent() {
        bail!(
            "`{action}` {why} and must be run by you, not by an AI agent \
             (detected ${var}).\n  → Run it yourself in a terminal."
        );
    }
    if !std::io::stdin().is_terminal() {
        bail!("`{action}` needs an interactive terminal to confirm.\n  → Run it yourself in a terminal.");
    }
    eprint!("⚠️  `{action}` {why}.\n   Type `{phrase}` to continue: ");
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    if line.trim() != phrase {
        bail!("confirmation did not match — aborted");
    }
    Ok(())
}

static AGENT_NAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Record the connected agent's name (from the MCP `initialize` handshake).
pub fn set_agent_name(name: &str) {
    let _ = AGENT_NAME.set(name.to_string());
}

/// Who is acting: the MCP client name if known, else a detected agent environment.
pub fn agent_name() -> Option<String> {
    if let Some(n) = AGENT_NAME.get() {
        return Some(n.clone());
    }
    if let Ok(v) = std::env::var("AI_AGENT") {
        return Some(v.split('_').next().unwrap_or(&v).to_string());
    }
    if std::env::var_os("CLAUDECODE").is_some() {
        return Some("claude-code".into());
    }
    detected_agent().map(|v| v.to_lowercase())
}

/// Append a git trailer naming the agent, if any: `Agent: claude-code`.
pub fn with_agent_trailer(message: &str) -> String {
    match agent_name() {
        Some(a) => format!("{message}\n\nAgent: {a}"),
        None => message.to_string(),
    }
}
