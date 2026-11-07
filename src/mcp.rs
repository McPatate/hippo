//! MCP server (stdio): agents read and update the vault through these tools.
//!
//! Agents are clients, not owners: every write is a normal git commit in the vault, followed by
//! a background encrypted backup. There is no tool that deletes remote data.
//!
//! If a device merge left conflicts, *every* tool response starts with a notice so the agent
//! deals with it (or asks the user) before relying on memory.

use crate::backup;
use crate::crypto::Key;
use crate::index::Index;
use crate::memory::{KINDS, Memory};
use crate::remote::Remote;
use crate::vault::{Vault, has_conflict_markers};
use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    schemars, tool, tool_handler, tool_router,
};
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const INSTRUCTIONS: &str = "hippo is the user's personal, private, long-term memory. \
It is a folder of Markdown files the user owns, encrypted and backed up by hippo.\n\
- Call `search_memories` / `list_memories` at the start of a task when personal context could help.\n\
- Use `remember` to save durable facts about the user, their preferences, projects and feedback \
(one fact or topic per memory, short kebab-case names, update an existing memory instead of duplicating).\n\
- Never store secrets (passwords, API keys, tokens) in memory.\n\
- After saving, updating or forgetting a memory, say so in one short line in your reply, e.g. `🦛 remembered prefers-rust`, so the user can see what changed.\n\
- If a response starts with `⚠️ MEMORY MERGE CONFLICT`, resolve it with `resolve_conflict` before \
continuing: keep information from both sides when they are compatible; if they genuinely disagree, \
ask the user which version is right.";

#[derive(Clone)]
pub struct HippoServer {
    vault_path: PathBuf,
    key_path: PathBuf,
    backup_running: Arc<AtomicBool>,
    backup_again: Arc<AtomicBool>,
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RememberArgs {
    /// Short kebab-case name, e.g. `prefers-rust`. Reusing a name updates that memory.
    pub name: String,
    /// The memory content (Markdown).
    pub content: String,
    /// One-line summary used for listing and search.
    pub description: Option<String>,
    /// One of: user, feedback, project, reference, note.
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NameArgs {
    /// Memory name (as shown by list_memories).
    pub name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListArgs {
    /// Optional filter: user, feedback, project, reference, note.
    #[serde(rename = "type")]
    pub kind: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchArgs {
    pub query: String,
    /// Max results (default 10).
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ResolveArgs {
    /// Conflicted file path relative to the vault, e.g. `memories/prefers-rust.md`.
    pub path: String,
    /// The full resolved file content, without any conflict markers.
    pub content: String,
}

/// Agents never get to store secrets: memories are sent to model providers.
fn reject_secrets(text: &str) -> Result<(), McpError> {
    let found = crate::secrets::heuristic(text);
    if found.is_empty() {
        return Ok(());
    }
    Err(McpError::invalid_params(
        format!(
            "refused: this looks like it contains a secret ({}). Memories are sent to AI providers, so never store \
             credentials. Remove the secret (describe where it lives instead, e.g. \"token is in 1Password\") and retry. \
             If it is a false positive, rephrase the flagged string.",
            crate::secrets::describe(&found)
        ),
        None,
    ))
}

fn err(e: anyhow::Error) -> McpError {
    McpError::internal_error(format!("{e:#}"), None)
}

#[tool_router]
impl HippoServer {
    pub fn new(vault_path: PathBuf, key_path: PathBuf) -> Self {
        Self {
            vault_path,
            key_path,
            backup_running: Arc::default(),
            backup_again: Arc::default(),
            tool_router: Self::tool_router(),
        }
    }

    fn vault(&self) -> Result<Vault, McpError> {
        Vault::open(&self.vault_path).map_err(err)
    }

    /// Every reply starts with a one-line receipt (`🦛 hippo …`): harnesses keep it visible in the
    /// transcript after the call collapses. Then the conflict notice (if any), then the body.
    fn reply(&self, vault: &Vault, receipt: String, body: String) -> Result<CallToolResult, McpError> {
        let conflicts = vault.unresolved_conflicts().map_err(err)?;
        let mut text = format!("🦛 hippo {receipt}");
        if !conflicts.is_empty() {
            text += &format!(
                "\n\n⚠️ MEMORY MERGE CONFLICT — resolving memory device merge conflict…\n\
                 Two devices changed the same memories. Conflicted files: {}.\n\
                 Read each file (it contains <<<<<<< / ||||||| / ======= / >>>>>>> sections), then call \
                 `resolve_conflict` with the merged content. If the versions genuinely disagree, ask the user.",
                conflicts.join(", ")
            );
        }
        if !body.is_empty() {
            text += "\n\n";
            text += &body;
        }
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    fn backup_note(&self, vault: &Vault) -> &'static str {
        if vault.config.remote.is_some() { "encrypted backup queued" } else { "local only (no backup configured)" }
    }

    /// Run a backup in the background; coalesce bursts of writes into at most one extra run.
    fn schedule_backup(&self) {
        let vault = match Vault::open(&self.vault_path) {
            Ok(v) if v.config.remote.is_some() => v,
            _ => return,
        };
        if self.backup_running.swap(true, Ordering::SeqCst) {
            self.backup_again.store(true, Ordering::SeqCst);
            return;
        }
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                me.backup_again.store(false, Ordering::SeqCst);
                let res = async {
                    let key = Key::load(&me.key_path)?;
                    let remote = Remote::open(vault.config.remote.as_ref().unwrap())?;
                    backup::backup(&vault, &key, &remote).await
                }
                .await;
                if let Err(e) = res {
                    eprintln!("hippo: background backup failed: {e:#}");
                }
                if !me.backup_again.load(Ordering::SeqCst) {
                    break;
                }
            }
            me.backup_running.store(false, Ordering::SeqCst);
        });
    }

    #[tool(
        title = "🦛 Remember",
        description = "Save or update a memory about the user. Reusing a name replaces that memory (history is kept).",
        annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn remember(&self, Parameters(a): Parameters<RememberArgs>) -> Result<CallToolResult, McpError> {
        let vault = self.vault()?;
        let kind = a.kind.unwrap_or_else(|| "note".into());
        if !KINDS.contains(&kind.as_str()) {
            return Err(McpError::invalid_params(format!("type must be one of {KINDS:?}"), None));
        }
        let existing = vault.read_memory(&a.name).ok();
        let updated = existing.is_some();
        let m = Memory {
            name: a.name.clone(),
            description: a.description.or(existing.as_ref().map(|e| e.description.clone())).unwrap_or_default(),
            kind,
            tags: a.tags.or(existing.as_ref().map(|e| e.tags.clone())).unwrap_or_default(),
            extra: existing.map(|e| e.extra).unwrap_or_default(),
            body: a.content,
        };
        reject_secrets(&m.render())?;
        let commit = vault.remember(&m).map_err(err)?;
        let name = crate::memory::slugify(&m.name);
        let receipt = match &commit {
            Some(c) => {
                self.schedule_backup();
                let what = if m.description.is_empty() { String::new() } else { format!(" — {}", m.description) };
                format!(
                    "{} `{name}` [{}]{what} · {} · {}",
                    if updated { "updated" } else { "remembered" },
                    m.kind,
                    &c[..8],
                    self.backup_note(&vault)
                )
            }
            None => format!("already knew `{name}` (unchanged)"),
        };
        self.reply(&vault, receipt, String::new())
    }

    #[tool(title = "🦛 Recall", description = "Read one memory by name.", annotations(read_only_hint = true, open_world_hint = false))]
    async fn recall(&self, Parameters(a): Parameters<NameArgs>) -> Result<CallToolResult, McpError> {
        let vault = self.vault()?;
        let m = vault.read_memory(&a.name).map_err(err)?;
        crate::log::record(&vault.root, "recall", serde_json::json!({"name": &m.name}));
        self.reply(&vault, format!("recalled `{}`", m.name), m.render())
    }

    #[tool(
        title = "🦛 List memories",
        description = "List memories (name, type, description). Optionally filter by type.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn list_memories(&self, Parameters(a): Parameters<ListArgs>) -> Result<CallToolResult, McpError> {
        let vault = self.vault()?;
        let list = vault.list().map_err(err)?;
        let lines: Vec<String> = list
            .iter()
            .filter(|m| a.kind.as_ref().is_none_or(|k| &m.kind == k))
            .map(|m| format!("- {} [{}] {}", m.name, m.kind, m.description))
            .collect();
        let filter = a.kind.as_ref().map(|k| format!(" ({k})")).unwrap_or_default();
        self.reply(&vault, format!("listed {} memories{filter}", lines.len()), lines.join("\n"))
    }

    #[tool(
        title = "🦛 Search memories",
        description = "Full-text search over all memories. Returns names and matching snippets.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn search_memories(&self, Parameters(a): Parameters<SearchArgs>) -> Result<CallToolResult, McpError> {
        let vault = self.vault()?;
        let mut idx = Index::open(&vault.root).map_err(err)?;
        idx.refresh(&vault.memories_dir()).map_err(err)?;
        let hits = idx.search(&a.query, a.limit.unwrap_or(10)).map_err(err)?;
        let names: Vec<&str> = hits.iter().take(3).map(|h| h.name.as_str()).collect();
        crate::log::record(&vault.root, "search", serde_json::json!({"query": &a.query, "hits": hits.len()}));
        let receipt = match hits.len() {
            0 => format!("searched “{}” → no matches", a.query),
            n => format!("searched “{}” → {n} match(es): {}{}", a.query, names.join(", "), if n > 3 { ", …" } else { "" }),
        };
        let body = hits
            .iter()
            .map(|h| format!("- {} — {}\n  {}", h.name, h.description, h.snippet.replace('\n', " ")))
            .collect::<Vec<_>>()
            .join("\n");
        self.reply(&vault, receipt, body)
    }

    #[tool(
        title = "🦛 Forget",
        description = "Remove a memory from the vault. It stays in local history and existing backups.",
        annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn forget(&self, Parameters(a): Parameters<NameArgs>) -> Result<CallToolResult, McpError> {
        let vault = self.vault()?;
        let removed = vault.forget(&a.name).map_err(err)?;
        let receipt = if removed {
            self.schedule_backup();
            format!("forgot `{}` · still in history · {}", a.name, self.backup_note(&vault))
        } else {
            format!("no memory named `{}`", a.name)
        };
        self.reply(&vault, receipt, String::new())
    }

    #[tool(
        title = "🦛 Resolve memory conflict",
        description = "Resolve a memory merge conflict by writing the final content of a conflicted file.",
        annotations(read_only_hint = false, destructive_hint = false, open_world_hint = false)
    )]
    async fn resolve_conflict(&self, Parameters(a): Parameters<ResolveArgs>) -> Result<CallToolResult, McpError> {
        let vault = self.vault()?;
        let conflicts = vault.unresolved_conflicts().map_err(err)?;
        if !conflicts.contains(&a.path) {
            return Err(McpError::invalid_params(
                format!("`{}` is not in conflict. Conflicted files: {:?}", a.path, conflicts),
                None,
            ));
        }
        if has_conflict_markers(&a.content) {
            return Err(McpError::invalid_params("content still contains conflict markers", None));
        }
        reject_secrets(&a.content)?;
        let path = vault.root.join(&a.path);
        crate::util::atomic_write(&path, a.content.as_bytes()).map_err(err)?;
        let remaining = vault.unresolved_conflicts().map_err(err)?;
        let receipt = if remaining.is_empty() {
            vault.save("resolve memory merge").map_err(err)?;
            self.schedule_backup();
            format!("resolved `{}` · all conflicts resolved · {}", a.path, self.backup_note(&vault))
        } else {
            format!("resolved `{}` · still conflicted: {}", a.path, remaining.join(", "))
        };
        self.reply(&vault, receipt, String::new())
    }

    #[tool(
        title = "🦛 Memory status",
        description = "Backup status of the memory vault: local changes, last upload, last verified recovery point.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn memory_status(&self) -> Result<CallToolResult, McpError> {
        let vault = self.vault()?;
        let s = backup::status(&vault).map_err(err)?;
        let receipt = match s.state.verified_at {
            Some(t) => format!("status · last verified backup #{} {}", s.state.verified_seq, crate::util::ago(t)),
            None => "status · no verified backup yet".into(),
        };
        self.reply(&vault, receipt, crate::ui::status_text(&vault, &s, false))
    }
}

#[tool_handler]
impl ServerHandler for HippoServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("hippo", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }

    async fn on_initialized(&self, context: rmcp::service::NotificationContext<rmcp::RoleServer>) {
        if let Some(info) = context.peer.peer_info() {
            crate::safety::set_agent_name(&info.client_info.name);
        }
    }
}

pub async fn serve(vault_path: PathBuf, key_path: PathBuf) -> anyhow::Result<()> {
    // Mark our own process so destructive CLI commands refuse to run from it.
    // SAFETY: called before any other thread is spawned by us.
    unsafe { std::env::set_var("HIPPO_MCP", "1") };
    let server = HippoServer::new(vault_path.clone(), key_path.clone());
    // Pull other devices' changes at startup so conflicts surface early.
    if let Ok(vault) = Vault::open(&vault_path)
        && let (Some(rc), Ok(key)) = (vault.config.remote.clone(), Key::load(&key_path)) {
            tokio::spawn(async move {
                if let Ok(remote) = Remote::open(&rc)
                    && let Err(e) = backup::sync(&vault, &key, &remote).await {
                        eprintln!("hippo: startup sync failed: {e:#}");
                    }
            });
        }
    let service = server.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
