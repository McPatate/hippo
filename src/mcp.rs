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
use crate::project::Scope;
use crate::remote::Remote;
use crate::vault::{Vault, has_conflict_markers};
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, ServiceExt,
    handler::server::{
        router::tool::ToolRouter,
        tool::{InputResponses, RequestState},
        wrapper::Parameters,
    },
    model::*,
    prompt, prompt_handler, prompt_router, schemars,
    service::{ElicitationMode, RequestContext},
    tool, tool_handler, tool_router,
};
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const INSTRUCTIONS: &str = "hippo is the user's personal, private, long-term memory. \
It is a folder of Markdown files the user owns, encrypted and backed up by hippo.\n\
- Call `search_memories` / `list_memories` at the start of a task when personal context could help. \
They return names and descriptions; `recall` only what you actually need.\n\
- Use `remember` to save durable facts about the user, their preferences, projects and feedback \
(one fact or topic per memory, short kebab-case names, update an existing memory instead of duplicating).\n\
- Memories are general guidance from past sessions: they can be outdated, and `unconfirmed` ones \
were written by an agent and not yet reviewed by the user. What the user says now wins.\n\
- Your writes are drafts until the user approves them (`/hippo review` in Claude Code, `hippo review` in a terminal). \
To change several memories at once with the user's agreement (merge, rename, rescope, retire), use `propose_changes`: \
hippo asks the user to confirm in a dialog.\n\
- Never store secrets (passwords, API keys, tokens) in memory.\n\
- Facts that only matter in one project: `remember` with `visibility: \"project\"` (the folder this session started in) \
or `\"project:<key>\"` when they belong to another project than that folder.\n\
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
    /// The agent's project (stdio servers start in its folder): scope for `project:` memories.
    /// (MCP roots would be the protocol answer, but they are deprecated by SEP-2577.)
    scope: Arc<std::sync::RwLock<Scope>>,
    /// The agent session this server runs for (Claude Code's id when it gives one, as the context hook logs it).
    session: String,
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
    #[allow(dead_code)]
    prompt_router: rmcp::handler::server::router::prompt::PromptRouter<Self>,
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
    /// `all` (default), `private` (never returned to agents), `project` (only in the project this session started in), or `project:<key>` (another project).
    pub visibility: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RecallArgs {
    /// Memory name (as shown by list_memories).
    pub name: String,
    /// True when reading for a memory review (`/hippo review`), not to use it: it isn't counted as a use.
    #[serde(default)]
    pub review: bool,
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

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ProposeArgs {
    /// Every change the user agreed to. hippo shows them all in one dialog.
    pub changes: Vec<ChangeArg>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ChangeArg {
    /// Short label, e.g. "merge PR conventions into pr-style".
    pub title: String,
    /// Why, in one line.
    pub why: Option<String>,
    /// What it does; all or nothing.
    pub ops: Vec<OpArg>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum OpArg {
    /// Create or update a memory (its full new content).
    Write {
        name: String,
        content: String,
        description: Option<String>,
        /// user, feedback, project, reference or note (default: unchanged, or note).
        #[serde(rename = "type")]
        kind: Option<String>,
        tags: Option<Vec<String>>,
        /// `all`, `private`, `project` (this one) or `project:<key>`.
        visibility: Option<String>,
    },
    /// The same memory under a new name.
    Rename { from: String, to: String },
    /// Change who sees it: `all`, `private`, `project` (this one) or `project:<key>` (an existing project).
    Scope { name: String, visibility: String },
    /// Delete it. `into`: the memory that now holds its content (a merge).
    Remove { name: String, into: Option<String> },
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DecideArgs {
    /// Handles (from `pending_changes`) of the changes the user approved.
    #[serde(default)]
    pub approve: Vec<String>,
    /// Handles of the changes the user rejected (each memory goes back to its last approved version).
    #[serde(default)]
    pub reject: Vec<String>,
    /// Also approve every change pending in other projects, which you can't see: the user confirms
    /// them by count per project in the dialog. Only when the user asked for it.
    #[serde(default)]
    pub elsewhere: bool,
}

/// What the user answered in a dialog hippo showed them.
enum Asked {
    Yes,
    No,
    /// The client can't show hippo's dialogs.
    Unavailable,
    /// Protocol 2026-07-28+: the client shows the dialog and calls the tool again with the answer.
    Pending(InputRequiredResult),
}

/// A dialog with no fields: the user's Accept or Decline is the answer.
fn dialog(message: String) -> ElicitRequestParams {
    ElicitRequestParams::FormElicitationParams { meta: None, message, requested_schema: ElicitationSchema::builder().build_unchecked() }
}

fn handle(p: &crate::change::Pending) -> String {
    p.id.clone().unwrap_or_else(|| p.members.iter().map(|m| m.name.as_str()).collect::<Vec<_>>().join("+"))
}

/// Agents never get to store secrets: memories are sent to model providers.
fn reject_secrets(vault: &Vault, text: &str) -> Result<(), McpError> {
    let found = crate::secrets::heuristic(text);
    if found.is_empty() {
        return Ok(());
    }
    crate::log::record(&vault.root, "refused", serde_json::json!({"why": "secret", "kind": crate::secrets::describe(&found)}));
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
        let cwd = std::env::current_dir().unwrap_or_default();
        let scope = match Vault::open(&vault_path) {
            Ok(v) => Scope::here(&v, &cwd),
            Err(_) => Scope { dir: cwd, keys: vec![] },
        };
        Self {
            vault_path,
            key_path,
            backup_running: Arc::default(),
            backup_again: Arc::default(),
            scope: Arc::new(std::sync::RwLock::new(scope)),
            session: std::env::var("CLAUDE_CODE_SESSION_ID").ok().and_then(|s| s.get(..8).map(String::from)).unwrap_or_else(|| crate::util::random_hex(4)),
            tool_router: Self::tool_router(),
            prompt_router: Self::prompt_router(),
        }
    }

    /// Log locally with where the agent works and its session (signals for the smart review).
    fn log(&self, vault: &Vault, action: &str, mut details: serde_json::Value) {
        details["project"] = serde_json::json!(self.scope.read().unwrap().keys.first());
        details["session"] = serde_json::json!(self.session);
        crate::log::record(&vault.root, action, details);
    }

    /// In scope, and not on its way out (an agent asked to forget it).
    fn visible(&self, m: &Memory) -> bool {
        m.status() != Some("forget") && m.visible_in(&self.scope.read().unwrap())
    }

    /// A memory this agent may see, or "not found" (never reveals out-of-scope names).
    fn read_visible(&self, vault: &Vault, name: &str) -> Result<Memory, McpError> {
        vault
            .read_memory(name)
            .ok()
            .filter(|m| self.visible(m))
            .ok_or_else(|| McpError::invalid_params(format!("no memory named `{name}`"), None))
    }

    /// The key for a project-scoped write. `None` means this project: its key, created from the
    /// folder name the first time. A key nobody uses yet names this folder, if it has no key yet.
    fn project_key(&self, vault: &Vault, key: Option<&str>) -> anyhow::Result<String> {
        let mut scope = self.scope.write().unwrap();
        if key.is_none() && !scope.keys.is_empty() {
            return Ok(scope.keys[0].clone());
        }
        if let Some(k) = key.map(crate::memory::slugify).filter(|_| !scope.keys.is_empty()) {
            let known: Vec<String> = crate::project::Registry::load(&vault.root)?.projects.into_keys().collect();
            anyhow::ensure!(known.contains(&k), "no project `{k}` (this folder is `{}`); known: {}", scope.keys[0], known.join(", "));
        }
        let id = crate::project::Ident::of(&scope.dir);
        let (key, here) = vault.update_registry("projects: link a folder", |reg| match key {
            Some(k) => {
                let k = crate::memory::slugify(k);
                let new = !reg.projects.contains_key(&k);
                if new {
                    reg.learn(&k, &id);
                }
                (k, new)
            }
            None => (reg.key_for(&id), true),
        })?;
        if here && !scope.keys.contains(&key) {
            scope.keys.push(key.clone());
        }
        Ok(key)
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
        description = "Save or update a memory about the user. Reusing a name replaces that memory (history is kept). Your writes are drafts until the user approves them.",
        annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn remember(&self, Parameters(a): Parameters<RememberArgs>) -> Result<CallToolResult, McpError> {
        let vault = self.vault()?;
        let kind = a.kind.unwrap_or_else(|| "note".into());
        if !KINDS.contains(&kind.as_str()) {
            return Err(McpError::invalid_params(format!("type must be one of {KINDS:?}"), None));
        }
        let existing = vault.read_memory(&a.name).ok();
        if existing.as_ref().is_some_and(|m| !m.visible_in(&self.scope.read().unwrap())) {
            return Err(McpError::invalid_params(format!("the name `{}` is taken outside this project; pick another", a.name), None));
        }
        let updated = existing.is_some();
        let m = Memory {
            name: a.name.clone(),
            description: a.description.or(existing.as_ref().map(|e| e.description.clone())).unwrap_or_default(),
            kind,
            tags: a.tags.or(existing.as_ref().map(|e| e.tags.clone())).unwrap_or_default(),
            extra: existing.map(|e| e.extra).unwrap_or_default(),
            body: a.content,
        };
        let mut m = m;
        match a.visibility.as_deref().map(str::trim) {
            None => {}
            Some("all") => {
                m.extra.remove("visibility");
            }
            Some("private") => {
                m.extra.insert("visibility".into(), "private".into());
            }
            Some(v) if v == "project" || (v.starts_with("project:") && v.len() > 8) => {
                let key = self.project_key(&vault, v.strip_prefix("project:").map(str::trim)).map_err(err)?;
                m.extra.insert("visibility".into(), format!("project:{key}"));
            }
            Some(v) => {
                return Err(McpError::invalid_params(format!("visibility `{v}`: use all, private, project (this one) or project:<key>"), None));
            }
        }
        reject_secrets(&vault, &m.render())?;
        let agent = crate::safety::agent_name().unwrap_or_else(|| "agent".into());
        let commit = vault.remember_by(&m, Some(&agent)).map_err(err)?;
        let name = crate::memory::slugify(&m.name);
        let receipt = match &commit {
            Some(c) => {
                let only = m.extra.get("visibility").cloned();
                self.log(&vault, "remember", serde_json::json!({"name": name, "new": !updated, "visibility": only}));
                self.schedule_backup();
                let what = if m.description.is_empty() { String::new() } else { format!(" — {}", m.description) };
                let seen = match only.as_deref().and_then(|v| v.strip_prefix("project:")) {
                    Some(k) => format!(" · only in project {k}"),
                    None if only.is_some() => " · private".into(),
                    None => String::new(),
                };
                format!(
                    "{} `{name}` [{}]{what}{seen} · draft until you approve (`hippo review`) · {} · {}",
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
    async fn recall(&self, Parameters(a): Parameters<RecallArgs>) -> Result<CallToolResult, McpError> {
        let t = std::time::Instant::now();
        let vault = self.vault()?;
        let m = self.read_visible(&vault, &a.name)?;
        let mut d = serde_json::json!({"name": &m.name, "shared": "full text", "ms": t.elapsed().as_millis()});
        if a.review {
            d["review"] = true.into();
        }
        self.log(&vault, "recall", d);
        let from = m.provenance(m.updated().is_none().then(|| vault.last_changed(&m.name)).flatten());
        let from = if from.is_empty() { String::new() } else { format!(" · {from}") };
        self.reply(&vault, format!("recalled `{}`{from}", m.name), m.render())
    }

    #[tool(
        title = "🦛 List memories",
        description = "List memories (name, type, description). Optionally filter by type.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn list_memories(&self, Parameters(a): Parameters<ListArgs>) -> Result<CallToolResult, McpError> {
        let t = std::time::Instant::now();
        let vault = self.vault()?;
        let list = vault.list().map_err(err)?;
        let lines: Vec<String> = list
            .iter()
            .filter(|m| a.kind.as_ref().is_none_or(|k| &m.kind == k) && self.visible(m))
            .map(|m| format!("- {} [{}]{} {}", m.name, m.kind, if m.needs_review() { " (unconfirmed)" } else { "" }, m.description))
            .collect();
        let filter = a.kind.as_ref().map(|k| format!(" ({k})")).unwrap_or_default();
        self.log(&vault, "list", serde_json::json!({"count": lines.len(), "shared": "descriptions", "ms": t.elapsed().as_millis()}));
        self.reply(&vault, format!("listed {} memories{filter}", lines.len()), lines.join("\n"))
    }

    #[tool(
        title = "🦛 Search memories",
        description = "Full-text search over all memories. Returns names and matching snippets.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn search_memories(&self, Parameters(a): Parameters<SearchArgs>) -> Result<CallToolResult, McpError> {
        let t = std::time::Instant::now();
        let vault = self.vault()?;
        let mut idx = Index::open(&vault.root).map_err(err)?;
        idx.refresh(&vault.memories_dir()).map_err(err)?;
        let limit = a.limit.unwrap_or(10);
        let hits: Vec<_> = idx
            .search(&a.query, limit * 4)
            .map_err(err)?
            .into_iter()
            .filter(|h| vault.read_memory(&h.name).is_ok_and(|m| self.visible(&m)))
            .take(limit)
            .collect();
        let names: Vec<&str> = hits.iter().take(3).map(|h| h.name.as_str()).collect();
        let snippets = crate::config::GlobalConfig::load().is_ok_and(|g| g.search_snippets);
        self.log(
            &vault,
            "search",
            serde_json::json!({
                "query": &a.query,
                "hits": hits.len(),
                "names": hits.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(),
                "shared": if snippets { "descriptions + snippets" } else { "descriptions" },
                "ms": t.elapsed().as_millis(),
            }),
        );
        let receipt = match hits.len() {
            0 => format!("searched “{}” → no matches", a.query),
            n => format!("searched “{}” → {n} match(es): {}{}", a.query, names.join(", "), if n > 3 { ", …" } else { "" }),
        };
        let body = hits
            .iter()
            .map(|h| {
                let draft = if vault.read_memory(&h.name).is_ok_and(|m| m.needs_review()) { " (unconfirmed)" } else { "" };
                match snippets {
                    true => format!("- {}{draft} — {}\n  {}", h.name, h.description, h.snippet.replace('\n', " ")),
                    false => format!("- {}{draft} — {}", h.name, h.description),
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        self.reply(&vault, receipt, body)
    }

    #[tool(
        title = "🦛 Forget",
        description = "Remove a memory. It is hidden from agents right away and deleted once the user approves; history keeps it.",
        annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn forget(&self, Parameters(a): Parameters<NameArgs>) -> Result<CallToolResult, McpError> {
        let vault = self.vault()?;
        let agent = crate::safety::agent_name().unwrap_or_else(|| "agent".into());
        let removed = self.read_visible(&vault, &a.name).is_ok() && vault.request_forget(&a.name, &agent).map_err(err)?;
        let receipt = if removed {
            self.log(&vault, "forget", serde_json::json!({"name": a.name}));
            self.schedule_backup();
            format!("forgot `{}` · removed once you approve (`hippo review`) · {}", a.name, self.backup_note(&vault))
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
        reject_secrets(&vault, &a.content)?;
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

    /// Ask the user in a dialog hippo writes itself; the answer comes back to hippo, never
    /// through the model. `key` binds the answer to exactly what was shown.
    async fn ask(&self, ctx: &RequestContext<RoleServer>, message: String, state: Option<String>, answers: Option<rmcp::model::InputResponses>) -> Asked {
        use sha2::Digest;
        let key = hex::encode(sha2::Sha256::digest(message.as_bytes()));
        let new_protocol = ctx.protocol_version().is_some_and(|v| v >= ProtocolVersion::V_2026_07_28);
        if new_protocol {
            if state.as_deref() == Some(key.as_str())
                && let Some(answer) = answers.and_then(|mut a| a.remove("confirm"))
            {
                let r: Option<ElicitResult> = serde_json::from_value(answer).ok();
                return if r.is_some_and(|r| matches!(r.action, ElicitationAction::Accept)) { Asked::Yes } else { Asked::No };
            }
            if ctx.client_capabilities().is_none_or(|c| c.elicitation.is_none()) {
                return Asked::Unavailable;
            }
            let req = InputRequests::from([("confirm".to_string(), InputRequest::Elicitation(ElicitRequest::new(dialog(message))))]);
            return Asked::Pending(InputRequiredResult::new(Some(req), Some(key)));
        }
        if !ctx.peer.supported_elicitation_modes().contains(&ElicitationMode::Form) {
            return Asked::Unavailable;
        }
        match ctx.peer.create_elicitation(dialog(message)).await {
            Ok(r) if matches!(r.action, ElicitationAction::Accept) => Asked::Yes,
            Ok(_) => Asked::No,
            Err(e) => {
                eprintln!("hippo: dialog failed: {e}");
                Asked::Unavailable
            }
        }
    }

    /// Pending changes reviewable here, and the rest (other projects: only counted, never shown).
    fn split_pending(&self, vault: &Vault) -> Result<(Vec<crate::change::Pending>, Vec<crate::change::Pending>), McpError> {
        let scope = self.scope.read().unwrap().clone();
        Ok(vault.pending_changes().map_err(err)?.into_iter().partition(|p| p.visible_in(vault, &scope)))
    }

    /// What a dialog asked and what the user answered (the funnel of the review).
    fn log_dialog(&self, vault: &Vault, tool: &str, message: &str, asked: &Asked, since: std::time::Instant) {
        let answer = match asked {
            Asked::Pending(_) => return,
            Asked::Yes => "yes",
            Asked::No => "no",
            Asked::Unavailable => "unavailable",
        };
        let summary: Vec<&str> = message.lines().skip(1).filter(|l| !l.starts_with("To see more")).collect();
        self.log(vault, "dialog", serde_json::json!({"tool": tool, "summary": summary.join(" | "), "answer": answer, "ms": since.elapsed().as_millis()}));
    }

    /// Memory names a change may touch: visible here (removals pending included, for review).
    fn reviewable(&self, m: &Memory) -> bool {
        m.visible_in(&self.scope.read().unwrap())
    }

    fn to_op(&self, vault: &Vault, op: OpArg) -> Result<crate::change::Op, McpError> {
        use crate::change::Op;
        let visibility = |v: &str| -> Result<String, McpError> {
            match v.trim() {
                "all" | "private" => Ok(v.trim().into()),
                "project" => Ok(format!("project:{}", self.project_key(vault, None).map_err(err)?)),
                p if p.starts_with("project:") => {
                    let k = crate::memory::slugify(&p[8..]);
                    let known = crate::project::Registry::load(&vault.root).map_err(err)?.projects.contains_key(&k);
                    if known {
                        Ok(format!("project:{k}"))
                    } else {
                        Err(McpError::invalid_params(format!("no project `{k}`: the user links it with `hippo project link {k} <dir>`"), None))
                    }
                }
                v => Err(McpError::invalid_params(format!("visibility `{v}`: use all, private, project or project:<key>"), None)),
            }
        };
        let seen = |n: &str| -> Result<Memory, McpError> { self.read_visible(vault, n) };
        Ok(match op {
            OpArg::Write { name, content, description, kind, tags, visibility: vis } => {
                let existing = vault.read_memory(&name).ok();
                if existing.as_ref().is_some_and(|m| !self.reviewable(m)) {
                    return Err(McpError::invalid_params(format!("the name `{name}` is taken outside this project; pick another"), None));
                }
                let kind = kind.or(existing.as_ref().map(|e| e.kind.clone())).unwrap_or_else(|| "note".into());
                if !KINDS.contains(&kind.as_str()) {
                    return Err(McpError::invalid_params(format!("type must be one of {KINDS:?}"), None));
                }
                let mut m = Memory {
                    name,
                    description: description.or(existing.as_ref().map(|e| e.description.clone())).unwrap_or_default(),
                    kind,
                    tags: tags.or(existing.as_ref().map(|e| e.tags.clone())).unwrap_or_default(),
                    extra: existing.map(|e| e.extra).unwrap_or_default(),
                    body: content,
                };
                match vis.as_deref().map(visibility).transpose()?.as_deref() {
                    None => {}
                    Some("all") => drop(m.extra.remove("visibility")),
                    Some(v) => drop(m.extra.insert("visibility".into(), v.into())),
                }
                reject_secrets(vault, &m.render())?;
                Op::Write(m)
            }
            OpArg::Rename { from, to } => Op::Rename { from: seen(&from)?.name, to },
            OpArg::Scope { name, visibility: v } => Op::Scope { name: seen(&name)?.name, visibility: visibility(&v)? },
            OpArg::Remove { name, into } => Op::Remove { name: seen(&name)?.name, into },
        })
    }

    #[tool(
        title = "🦛 Propose changes",
        description = "Apply changes to the memory that the user agreed to (merge, split, rename, rescope, edit, remove), \
                       grouped as one change per decision. hippo shows the user one dialog listing them all and applies \
                       them only if they say yes. If they decline, nothing changed: ask what they want instead.",
        annotations(read_only_hint = false, destructive_hint = true, idempotent_hint = false, open_world_hint = false)
    )]
    async fn propose_changes(
        &self,
        Parameters(a): Parameters<ProposeArgs>,
        RequestState(state): RequestState,
        InputResponses(answers): InputResponses,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let vault = self.vault()?;
        let mut changes = vec![];
        for c in a.changes {
            let ops = c.ops.into_iter().map(|o| self.to_op(&vault, o)).collect::<Result<Vec<_>, _>>()?;
            changes.push(crate::change::Change { title: c.title, why: c.why.unwrap_or_default(), ops });
        }
        if changes.is_empty() {
            return Err(McpError::invalid_params("no changes", None));
        }
        let lines: Vec<String> = changes.iter().flat_map(|c| crate::change::describe(&vault, &c.ops, false)).collect();
        let message = format!(
            "🦛 Apply {} change{} to your memory?\n{}\nTo see more first: decline and ask.",
            changes.len(),
            if changes.len() == 1 { "" } else { "s" },
            crate::change::tally(&lines)
        );
        let agent = crate::safety::agent_name().unwrap_or_else(|| "agent".into());
        let (t, shown) = (std::time::Instant::now(), message.clone());
        let asked = self.ask(&ctx, message, state, answers).await;
        self.log_dialog(&vault, "propose_changes", &shown, &asked, t);
        let receipt = match asked {
            Asked::Pending(r) => return Ok(CallToolResponse::InputRequired(r)),
            Asked::Yes => {
                vault.apply_changes(&changes, &agent, true).map_err(err)?;
                format!("applied {} change(s): the user approved them in hippo's dialog", changes.len())
            }
            Asked::No => "the user declined in hippo's dialog: nothing was applied. Ask what they want to change.".into(),
            Asked::Unavailable => {
                vault.apply_changes(&changes, &agent, false).map_err(err)?;
                format!(
                    "saved {} change(s) as drafts: this client can't show hippo's dialog, so the user approves them with `hippo review` in a terminal",
                    changes.len()
                )
            }
        };
        self.schedule_backup();
        Ok(CallToolResponse::Complete(self.reply(&vault, receipt, String::new())?))
    }

    #[tool(
        title = "🦛 Pending changes",
        description = "Changes to the memory waiting for the user's decision (agent drafts, removals, renames), grouped: \
                       hippo's description of each, and the content involved (also of memories to be deleted), so you \
                       can go through them with the user. Then `decide_changes`.",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn pending_changes(&self) -> Result<CallToolResult, McpError> {
        let vault = self.vault()?;
        let (pending, elsewhere) = self.split_pending(&vault)?;
        let mut body = vec![];
        for p in &pending {
            let mut t = format!("## {} · handle `{}` · by {}\n", p.meta.title, handle(p), p.meta.by);
            if !p.meta.why.is_empty() {
                t += &format!("why: {}\n", p.meta.why);
            }
            for l in crate::change::describe(&vault, &p.ops(), true) {
                t += &format!("- {l}\n");
            }
            for m in &p.members {
                let before = vault.approved_version(&m.name).map_err(err)?.map(|t| Memory::parse(&m.name, &t));
                match (m.status(), before) {
                    (Some("forget"), _) => t += &format!("\n### {} (to be deleted)\n{}\n", m.name, m.body.trim()),
                    (_, Some(b)) if b.body.trim() != m.body.trim() => {
                        t += &format!("\n### {} before\n{}\n\n### {} after\n{}\n", m.name, b.body.trim(), m.name, m.body.trim())
                    }
                    (_, Some(_)) => {}
                    (_, None) => t += &format!("\n### {} (new)\n{}\n", m.name, m.body.trim()),
                }
            }
            body.push(t);
        }
        let mut receipt = format!("{} change(s) waiting for the user", pending.len());
        if !elsewhere.is_empty() {
            receipt += &format!(
                "; {} more in other projects (not shown here): `decide_changes` with `elsewhere: true` approves them once the user confirms the counts",
                elsewhere.len()
            );
        }
        self.reply(&vault, receipt, body.join("\n"))
    }

    #[tool(
        title = "🦛 Decide changes",
        description = "Approve and reject pending changes the user decided on (handles from `pending_changes`). hippo \
                       shows the user one dialog to confirm; nothing happens if they decline.",
        annotations(read_only_hint = false, destructive_hint = true, idempotent_hint = false, open_world_hint = false)
    )]
    async fn decide_changes(
        &self,
        Parameters(a): Parameters<DecideArgs>,
        RequestState(state): RequestState,
        InputResponses(answers): InputResponses,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let vault = self.vault()?;
        let (pending, elsewhere) = self.split_pending(&vault)?;
        let find = |h: &String| {
            pending.iter().find(|p| &handle(p) == h).ok_or_else(|| McpError::invalid_params(format!("no pending change `{h}` (call `pending_changes`)"), None))
        };
        let mut yes = a.approve.iter().map(find).collect::<Result<Vec<_>, _>>()?;
        let no = a.reject.iter().map(find).collect::<Result<Vec<_>, _>>()?;
        let others: Vec<&crate::change::Pending> = if a.elsewhere { elsewhere.iter().collect() } else { vec![] };
        let lines = |list: &[&crate::change::Pending]| -> Vec<String> { list.iter().flat_map(|p| crate::change::describe(&vault, &p.ops(), true)).collect() };
        let mut places = std::collections::BTreeMap::<String, usize>::new();
        for p in &others {
            for k in p.places() {
                *places.entry(k).or_default() += 1;
            }
        }
        let others_line = format!(
            "\nApply {} in other projects: {} ({})",
            others.len(),
            places.iter().map(|(k, n)| format!("{k} {n}")).collect::<Vec<_>>().join(" · "),
            crate::change::tally(&lines(&others))
        );
        yes.extend(others.iter().copied());
        if yes.is_empty() && no.is_empty() {
            return Err(McpError::invalid_params("nothing to decide", None));
        }
        let mut message = "🦛 Decide these changes to your memory?".to_string();
        let here = &yes[..yes.len() - others.len()];
        if !here.is_empty() {
            message += &format!("\nApply {}: {}", here.len(), crate::change::tally(&lines(here)));
        }
        if !others.is_empty() {
            message += &others_line;
        }
        if !no.is_empty() {
            message += &format!("\nUndo {} (back to what you last approved): {}", no.len(), crate::change::tally(&lines(&no)));
        }
        message += "\nTo see more first: decline and ask.";
        let (t, shown) = (std::time::Instant::now(), message.clone());
        let asked = self.ask(&ctx, message, state, answers).await;
        self.log_dialog(&vault, "decide_changes", &shown, &asked, t);
        let receipt = match asked {
            Asked::Pending(r) => return Ok(CallToolResponse::InputRequired(r)),
            Asked::Yes => {
                let mut failed = vec![];
                for p in &yes {
                    if let Err(e) = vault.approve_change(p) {
                        failed.push(format!("{}: {e:#}", p.meta.title));
                    }
                }
                for p in &no {
                    if let Err(e) = vault.reject_change(p) {
                        failed.push(format!("{}: {e:#}", p.meta.title));
                    }
                }
                let _ = crate::import::write_back_claude(&vault, &crate::import::default_claude_dir());
                match failed.is_empty() {
                    true => format!("done: {} applied, {} undone (the user confirmed in hippo's dialog)", yes.len(), no.len()),
                    false => format!("partly done; not applied: {}", failed.join("; ")),
                }
            }
            Asked::No => "the user declined in hippo's dialog: nothing changed. Ask what they want instead.".into(),
            Asked::Unavailable => "this client can't show hippo's dialog: the user decides with `hippo review` in a terminal".into(),
        };
        self.schedule_backup();
        Ok(CallToolResponse::Complete(self.reply(&vault, receipt, String::new())?))
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

#[prompt_router]
impl HippoServer {
    /// 🧹 Cleanup time: a guided pass over the memory, decided with the user.
    #[prompt(name = "cleanup", description = "🧹 Tidy your memory with the agent: merge duplicates, fix stale or cryptic notes. You approve every change.")]
    async fn cleanup(&self) -> Result<Vec<PromptMessage>, McpError> {
        let vault = self.vault()?;
        let visible: Vec<Memory> = vault.list().map_err(err)?.into_iter().filter(|m| self.visible(m)).collect();
        let dupes: Vec<String> = crate::cleanup::likely_duplicates(&visible).iter().map(|(a, b, _)| format!("- `{a}` ~ `{b}`")).collect();
        let pending = visible.iter().filter(|m| m.needs_review()).count();
        let text = format!(
            "🧹 Cleanup time: help me tidy my hippo memory ({} memories here). hippo has no opinion on how \
             memories should be organized; we decide together.\n\n\
             1. `list_memories`, then `recall` the ones you need to compare.\n\
             2. Look for duplicates, overlapping topics, outdated facts, cryptic notes, and memories scoped too wide \
             or too narrow (`visibility`). Likely duplicates hippo spotted:\n{}\n\
             3. Propose a short plan, one line per change (merge / rewrite / rescope / forget) with why. \
             Ask me before changing anything: I may want a different organization.\n\
             4. Apply what I agree to with one `propose_changes` call, one change per decision (a merge is a `write` \
             plus a `remove` with `into` per absorbed memory). hippo asks me to confirm in a dialog{}.",
            visible.len(),
            if dupes.is_empty() { "- none".into() } else { dupes.join("\n") },
            if pending > 0 { format!("; {pending} older change(s) are waiting too: `pending_changes`, then `decide_changes`") } else { String::new() },
        );
        Ok(vec![PromptMessage::new_text(Role::User, text)])
    }
}

#[tool_handler]
#[prompt_handler]
impl ServerHandler for HippoServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().enable_prompts().build())
            .with_server_info(Implementation::new("hippo", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }

    async fn on_initialized(&self, context: rmcp::service::NotificationContext<rmcp::RoleServer>) {
        if let Some(info) = context.peer.peer_info() {
            crate::safety::set_agent_name(&info.client_info.name);
            if let Ok(vault) = self.vault() {
                let dialogs = context.peer.supported_elicitation_modes().contains(&ElicitationMode::Form);
                let (client, version) = (&info.client_info.name, &info.client_info.version);
                self.log(&vault, "session", serde_json::json!({"client": client, "version": version, "protocol": info.protocol_version, "dialogs": dialogs}));
            }
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
