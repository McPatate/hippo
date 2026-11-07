use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use hippo::backup::{self, RestoreOptions};
use hippo::config::{self, GlobalConfig, RemoteConfig};
use hippo::crypto::Key;
use hippo::index::Index;
use hippo::memory::{KINDS, Memory};
use hippo::remote::Remote;
use hippo::ui::{self, bold, dim, green, red, yellow};
use hippo::util::{expand_tilde, pretty_path};
use hippo::vault::Vault;
use serde_json::{Value, json};
use std::io::{BufRead, IsTerminal, Read, Write};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "hippo", version, about = "🦛 Your AI memory as plain files you own — encrypted, backed up, portable.")]
struct Cli {
    /// Vault folder (default: from ~/.hippo/config.toml, else ~/.hippo/memory)
    #[arg(long, global = true, env = "HIPPO_VAULT")]
    vault: Option<PathBuf>,
    /// Machine-readable mode: JSON on stdout, no prompts, no colors.
    /// Auto-enabled when an AI agent environment is detected (AI_AGENT, CLAUDECODE, …).
    #[arg(long, global = true, env = "HIPPO_AGENT_MODE")]
    agent: bool,
    /// Force human mode even inside an agent environment
    #[arg(long, global = true, conflicts_with = "agent")]
    human: bool,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a new vault (and its key, and optionally a private HF bucket)
    Init {
        #[arg(long)]
        path: Option<PathBuf>,
        /// Back up to this bucket (`username/name`) or folder
        #[arg(long)]
        remote: Option<String>,
        /// Do not configure backups
        #[arg(long, conflicts_with = "remote")]
        no_remote: bool,
        /// Accept defaults, no prompts
        #[arg(short, long)]
        yes: bool,
    },
    /// Show local / uploaded / verified state
    Status,
    /// Save or update a memory (content from -m, stdin, or $EDITOR)
    Add {
        name: String,
        #[arg(short, long)]
        message: Option<String>,
        #[arg(short, long)]
        description: Option<String>,
        #[arg(short = 't', long = "type", default_value = "note")]
        kind: String,
        #[arg(long, value_delimiter = ',')]
        tags: Vec<String>,
        /// Save even if it looks like it contains a secret (humans only)
        #[arg(long)]
        allow_secret: bool,
    },
    /// Scan all memories and history for secrets (heuristic + trufflehog if installed, offline)
    Scan,
    /// Recent actions on this vault (saves, backups, restores, merges…)
    Log {
        #[arg(short, long, default_value_t = 20)]
        n: usize,
    },
    /// List memories
    Ls {
        #[arg(short = 't', long = "type")]
        kind: Option<String>,
    },
    /// Print a memory
    Show { name: String },
    /// Full-text search
    Search {
        query: Vec<String>,
        #[arg(short, long, default_value_t = 10)]
        limit: usize,
    },
    /// Remove a memory (kept in history and existing backups)
    Rm { name: String },
    /// Commit edits made directly to the files
    Save {
        #[arg(short, long, default_value = "save")]
        message: String,
    },
    /// Save, encrypt and upload, then verify the upload
    Backup,
    /// Pull other devices' changes, merge, then back up
    Sync,
    /// Rebuild a vault from a backup (new machine, lost disk…)
    Restore {
        /// Bucket (`username/name`) or folder to restore from
        from: String,
        /// Destination folder (default: ~/.hippo/memory)
        #[arg(long)]
        to: Option<PathBuf>,
        #[arg(long)]
        vault_id: Option<String>,
        #[arg(long)]
        device: Option<String>,
    },
    /// Configure where backups go
    Remote {
        #[command(subcommand)]
        cmd: RemoteCmd,
    },
    /// Show the key's location and how to back it up
    Key {
        /// Print the secret key (human-only, to copy into a password manager)
        #[arg(long)]
        reveal: bool,
    },
    /// Import memories from another agent (read-only on the source; nothing is deleted)
    Import {
        #[command(subcommand)]
        source: ImportSource,
    },
    /// Sign in with Hugging Face (browser; --device for SSH/headless)
    Login {
        #[arg(long)]
        device: bool,
    },
    /// Forget hippo's Hugging Face sign-in (does not touch `hf auth login`)
    Logout,
    /// One-line status for harnesses (Claude Code `statusLine`): last memory action + backup state
    Statusline {
        /// Run your existing status line command (gets the same stdin) and append hippo to its last line
        #[arg(long)]
        wrap: Option<String>,
        #[arg(long, hide = true, env = "HIPPO_PIXEL")]
        pixel: bool,
    },
    /// Connect hippo to an agent harness (MCP + memory instructions + optional status line)
    Connect {
        #[command(subcommand)]
        harness: Harness,
    },
    /// Undo `hippo connect` (restores anything it changed)
    Disconnect {
        #[command(subcommand)]
        harness: Harness,
    },
    /// Meet your hippo buddy: name it, pick a color (shown by `statusline --pixel`)
    Buddy {
        #[arg(long)]
        name: Option<String>,
        /// lavender, pink, gray, blue
        #[arg(long)]
        color: Option<String>,
    },
    /// Run the MCP server on stdio (Claude, ChatGPT desktop, Cursor…)
    Mcp,
    /// Show backup storage usage (pruning: human-only, not implemented yet)
    Compact {
        #[arg(long)]
        prune: bool,
    },
}

#[derive(Subcommand)]
enum Harness {
    /// Claude Code
    Claude {
        /// Don't touch the status line
        #[arg(long)]
        no_statusline: bool,
        /// Status line with the pixel hippo buddy
        #[arg(long)]
        pixel: bool,
    },
}

#[derive(Subcommand)]
enum ImportSource {
    /// Claude Code file memories (~/.claude/projects/*/memory/*.md)
    ClaudeCode {
        #[arg(long)]
        from: Option<PathBuf>,
        /// Show what would be imported
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum RemoteCmd {
    /// `username/bucket` (HF, created private if missing) or a folder
    Set { target: String },
    Show,
}

/// Output sink: human (stderr/stdout, colors) or agent (one JSON object on stdout).
struct Out {
    agent: bool,
}

impl Out {
    /// Human message (suppressed in agent mode).
    fn say(&self, s: impl AsRef<str>) {
        if !self.agent {
            eprintln!("{}", s.as_ref());
        }
    }
    /// Final result: JSON in agent mode, `human` text otherwise.
    fn done(&self, data: Value, human: impl FnOnce() -> String) {
        if self.agent {
            let mut obj = json!({"ok": true});
            if let (Some(o), Value::Object(d)) = (obj.as_object_mut(), data) {
                o.extend(d);
            }
            println!("{obj}");
        } else {
            let h = human();
            if !h.is_empty() {
                print!("{h}");
                if !h.ends_with('\n') {
                    println!();
                }
            }
        }
    }
}

fn vault_path(cli: &Cli) -> Result<PathBuf> {
    if let Some(v) = &cli.vault {
        return Ok(v.clone());
    }
    Ok(GlobalConfig::load()?.default_vault.unwrap_or_else(config::default_vault_path))
}

fn open_vault(cli: &Cli) -> Result<Vault> {
    let p = vault_path(cli)?;
    if !p.exists() {
        bail!("no vault at {}\n  → Create one with `hippo init`, or restore one with `hippo restore <bucket>`", pretty_path(&p));
    }
    Vault::open(&p)
}

fn prompt(question: &str, default: &str) -> Result<String> {
    eprint!("{question} {} ", dim(&format!("[{default}]")));
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    let line = line.trim();
    Ok(if line.is_empty() { default.to_string() } else { line.to_string() })
}

fn confirm(question: &str) -> Result<bool> {
    let a = prompt(question, "Y/n")?.to_lowercase();
    Ok(!matches!(a.as_str(), "n" | "no"))
}

fn confirm_default_no(question: &str) -> Result<bool> {
    let a = prompt(question, "y/N")?.to_lowercase();
    Ok(matches!(a.as_str(), "y" | "yes"))
}

async fn hf_username() -> Result<String> {
    let client = hippo::auth::hf_client()?;
    let user = client.whoami().send().await.map_err(|e| {
        anyhow::anyhow!("not signed in to Hugging Face ({e})\n  → Run `hippo login` (or `hf auth login`), then retry")
    })?;
    Ok(user.username)
}

/// Make sure the target exists and is private; return its config.
async fn setup_remote(out: &Out, target: &str) -> Result<RemoteConfig> {
    let rc = RemoteConfig::parse(target)?;
    if let RemoteConfig::Hf { bucket } = &rc {
        let (owner, name) = bucket.split_once('/').unwrap();
        let client = hippo::auth::hf_client()?;
        client
            .create_bucket()
            .namespace(owner)
            .name(name)
            .private(true)
            .exist_ok(true)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("cannot create bucket {bucket}: {e}\n  → Check `hf auth login` uses a token with write access"))?;
        let info = client
            .bucket(owner, name)
            .info()
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("cannot read bucket {bucket}: {e}"))?;
        if !info.private {
            out.say(yellow(&format!(
                "⚠️  bucket {bucket} is PUBLIC. Data is encrypted, but make it private: `hf buckets settings {bucket} --private`"
            )));
        }
    }
    Ok(rc)
}

fn read_body(agent: bool, message: Option<String>, existing: Option<&str>) -> Result<String> {
    if let Some(m) = message {
        return Ok(m);
    }
    if !std::io::stdin().is_terminal() {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        return Ok(s);
    }
    if agent {
        bail!("no content given\n  → pass -m \"...\" or pipe the content on stdin");
    }
    let editor = std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR")).unwrap_or_else(|_| "vi".into());
    let tmp = std::env::temp_dir().join(format!("hippo-{}.md", hippo::util::random_hex(4)));
    std::fs::write(&tmp, existing.unwrap_or(""))?;
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("sh")
        .arg(&tmp)
        .status()?;
    let body = std::fs::read_to_string(&tmp)?;
    let _ = std::fs::remove_file(&tmp);
    if !status.success() {
        bail!("editor exited with an error; nothing saved");
    }
    Ok(body)
}

fn status_json(v: &Vault, s: &backup::Status) -> Value {
    json!({
        "vault": v.root,
        "local": {"unsaved_files": s.dirty_files, "unbacked_commits": s.unverified_commits},
        "uploaded": {"seq": s.state.uploaded_seq, "at": s.state.uploaded_at},
        "verified": {"seq": s.state.verified_seq, "at": s.state.verified_at},
        "pending": s.state.pending.as_ref().map(|p| p.seq),
        "last_error": s.state.last_error,
        "conflicts": s.conflicts,
        "secret_findings": hippo::secrets::load_findings(&v.root),
        "remote": s.remote.as_ref().map(|r| r.display()),
        "backed_up": s.remote.is_some() && s.dirty_files == 0 && s.unverified_commits == 0 && s.state.pending.is_none(),
    })
}

#[derive(serde::Serialize)]
struct ImportReport {
    new: usize,
    updated: usize,
    unchanged: usize,
    commit: Option<String>,
    backup_seq: Option<u64>,
    memories: Vec<hippo::import::Planned>,
}

async fn import_claude(v: &Vault, from: &std::path::Path, dry_run: bool) -> Result<ImportReport> {
    use hippo::import::{Status, count};
    let (plans, commit) = hippo::import::plan_and_import_claude(v, from, dry_run)?;
    if commit.is_some() {
        hippo::log::record(&v.root, "import", json!({"source": "claude-code", "new": count(&plans, Status::New), "updated": count(&plans, Status::Updated)}));
    }
    let mut backup_seq = None;
    if commit.is_some() && v.config.remote.is_some() {
        let (key, remote, _) = remote_for(v)?;
        backup_seq = backup::backup(v, &key, &remote).await?.seq;
    }
    Ok(ImportReport {
        new: count(&plans, Status::New),
        updated: count(&plans, Status::Updated),
        unchanged: count(&plans, Status::Unchanged),
        commit,
        backup_seq,
        memories: plans,
    })
}

/// Vaults already in `target` (empty if the bucket doesn't exist yet or can't be listed).
async fn existing_vaults(target: &str) -> Vec<hippo::backup::VaultManifest> {
    let Ok(rc) = RemoteConfig::parse(target) else { return vec![] };
    let Ok(remote) = Remote::open(&rc) else { return vec![] };
    backup::list_vaults(&remote).await.unwrap_or_default()
}

async fn cmd_init(out: &Out, cli: &Cli, path: Option<PathBuf>, remote: Option<String>, no_remote: bool, yes: bool) -> Result<()> {
    let interactive = !yes && !out.agent && std::io::stdin().is_terminal();
    let ask = |q: &str| -> Result<bool> { if interactive { confirm(q) } else { Ok(true) } };
    out.say(format!("\n  {} — your memory, your files, your keys.\n", bold("🦛 hippo")));

    let default_path = path.or(cli.vault.clone()).unwrap_or_else(config::default_vault_path);
    let path = if interactive {
        expand_tilde(&prompt("Where should your vault live?", &pretty_path(&default_path))?)
    } else {
        default_path
    };
    let kp = config::key_path();
    let mut key_created = false;
    let mut outcome = "created";
    let mut first_backup = None;

    let vault = if hippo::config::VaultConfig::load(&path).is_ok() {
        // Re-running init on a set-up machine is fine: skip to the integration steps.
        outcome = "existing";
        out.say(format!("  {} vault already set up at {}", green("✓"), pretty_path(&path)));
        Vault::open(&path)?
    } else {
        // Where would backups go?
        let target = if no_remote {
            None
        } else if let Some(r) = remote {
            Some(r)
        } else {
            let mut who = hf_username().await;
            if who.is_err() && interactive && hippo::auth::client_id().is_some() && confirm("Sign in with Hugging Face to enable encrypted backups?")? {
                login(out, !hippo::auth::can_open_browser()).await?;
                who = hf_username().await;
            }
            match who {
                Ok(user) => Some(format!("{user}/hippo")),
                Err(e) => {
                    out.say(format!("  {} backups not configured: {}", yellow("!"), e.to_string().lines().next().unwrap_or("")));
                    out.say("    → later: `hf auth login` then `hippo remote set <username>/hippo`");
                    None
                }
            }
        };
        let found = match &target {
            Some(t) => existing_vaults(t).await,
            None => vec![],
        };
        let key = kp.exists().then(|| Key::load(&kp)).transpose()?;
        let mine = key.as_ref().and_then(|k| found.iter().find(|m| m.recipient == k.recipient()));

        if let (Some(t), Some(k), Some(m)) = (&target, &key, mine) {
            // Recovery: this key's vault is already in the bucket.
            out.say(format!("  {} found your vault in {} (created {})", green("✓"), RemoteConfig::parse(t)?.display(), m.created_at.format("%Y-%m-%d")));
            if !ask("Restore it here?")? {
                bail!("not restoring, nothing changed\n  → Pick another vault location, or `hippo init --no-remote` for a separate local vault");
            }
            let rc = RemoteConfig::parse(t)?;
            let r = backup::restore(&Remote::open(&rc)?, &rc, k, &path, backup::RestoreOptions { vault_id: Some(m.vault_id.clone()), device: None }).await?;
            outcome = "restored";
            out.say(format!("  {} restored {} memories from {} backup(s)", green("✓"), r.vault.list()?.len(), r.bundles));
            r.vault
        } else {
            if !found.is_empty() {
                let why = if key.is_some() { "a different key" } else { "a key that isn't on this machine" };
                out.say(yellow(&format!("  ⚠ {} has hippo backups encrypted to {why}.", target.as_deref().unwrap_or(""))));
                out.say(format!("    If you have that key, put it at {} and rerun `hippo init`.", pretty_path(&kp)));
                if !interactive && !yes {
                    bail!("refusing to create a second vault next to existing backups\n  → Restore your key first, or rerun with --yes to create a new vault (old backups stay untouched)");
                }
                if interactive && !confirm_default_no("Create a new vault anyway? Old backups stay untouched.")? {
                    bail!("nothing changed");
                }
            }
            let key = match key {
                Some(k) => k,
                None => {
                    key_created = true;
                    Key::generate(&kp)?
                }
            };
            let mut vault = Vault::init(&path, &key)?;
            out.say(format!("  {} vault created at {}", green("✓"), pretty_path(&path)));
            if key_created {
                out.say(format!("  {} key created at {}\n", green("✓"), pretty_path(&kp)));
                out.say(format!("{}\n", yellow(ui::KEY_DISCLAIMER)));
            } else {
                out.say(format!("  {} using existing key {}", green("✓"), pretty_path(&kp)));
            }
            if let Some(t) = target {
                if ask(&format!("Back up (encrypted) to a private Hugging Face bucket {}?", bold(&t)))? {
                    let rc = setup_remote(out, &t).await?;
                    vault.config.remote = Some(rc.clone());
                    vault.config.save(&vault.root)?;
                    let r = backup::backup(&vault, &key, &Remote::open(&rc)?).await?;
                    first_backup = r.seq;
                    out.say(format!("  {} first backup verified (#{}) in {}", green("✓"), r.seq.unwrap_or(0), rc.display()));
                }
            }
            vault
        }
    };
    let mut gc = GlobalConfig::load()?;
    if gc.default_vault.is_none() || interactive {
        gc.default_vault = Some(path.clone());
        gc.save()?;
    }

    // Agent integration + import.
    let mut connected = None;
    if hippo::connect::claude_available() {
        if hippo::connect::mcp_connected() {
            out.say(format!("  {} Claude Code already connected", green("✓")));
        } else if (interactive || yes) && ask("Connect Claude Code? (memory tools, memory instructions, status line)")? {
            let r = hippo::connect::connect_claude(true, false)?;
            out.say(format!("  {} Claude Code connected: MCP {} · instructions {} · status line {}", green("✓"), r.mcp, r.instructions, r.statusline));
            connected = Some(r);
        }
    }
    let mut imported = None;
    let from = hippo::import::default_claude_dir();
    if from.is_dir() {
        let plan = import_claude(&vault, &from, true).await?;
        let todo = plan.new + plan.updated;
        if todo > 0 && (interactive || yes) && ask(&format!("Import {todo} memories from Claude Code? (read-only: Claude's copies stay)"))? {
            let r = import_claude(&vault, &from, false).await?;
            out.say(format!("  {} imported {} new, {} updated", green("✓"), r.new, r.updated));
            imported = Some(r);
        }
    }

    out.done(
        json!({
            "vault": path, "outcome": outcome, "key": kp, "key_created": key_created,
            "remote": vault.config.remote.as_ref().map(|r| r.display()), "first_backup": first_backup,
            "claude": connected, "imported": imported.as_ref().map(|r| json!({"new": r.new, "updated": r.updated})),
        }),
        || {
            let s = backup::status(&vault).map(|s| ui::status_text(&vault, &s, ui::color())).unwrap_or_default();
            let next = if connected.is_some() || hippo::connect::mcp_connected() {
                "Restart Claude Code to load hippo, then ask it to remember something."
            } else {
                "Connect an agent: `hippo connect claude`"
            };
            // Fresh keys get the full warning above; otherwise a short nudge, every time.
            let nudge = if key_created {
                String::new()
            } else {
                format!("  🔑 {}\n", yellow(&format!("Don't forget to safekeep your key: {} (`hippo key`)", pretty_path(&kp))))
            };
            format!("\n{s}\n{nudge}  {next}\n")
        },
    );
    Ok(())
}

async fn login(out: &Out, device: bool) -> Result<String> {
    let say = |s: &str| out.say(format!("  {s}"));
    if device {
        hippo::auth::login_device(say).await?;
    } else {
        hippo::auth::login_browser(say).await?;
    }
    let user = hf_username().await?;
    out.say(format!("  {} signed in as {}", green("✓"), bold(&user)));
    Ok(user)
}

fn remote_for(v: &Vault) -> Result<(Key, Remote, RemoteConfig)> {
    let rc = v.config.remote.clone().context("backups are not configured\n  → `hippo remote set <username>/hippo`")?;
    Ok((Key::load(&config::key_path())?, Remote::open(&rc)?, rc))
}

async fn run(cli: &Cli, out: &Out) -> Result<()> {
    let Some(cmd) = &cli.cmd else {
        return match open_vault(cli) {
            Ok(v) => {
                let s = backup::status(&v)?;
                out.done(status_json(&v, &s), || ui::status_text(&v, &s, ui::color()));
                Ok(())
            }
            Err(_) => {
                out.done(json!({"vault": null}), || {
                    "🦛 No vault yet. `hippo init` creates one; `hippo restore <bucket>` recovers one.".into()
                });
                Ok(())
            }
        };
    };
    match cmd {
        Cmd::Init { path, remote, no_remote, yes } => cmd_init(out, cli, path.clone(), remote.clone(), *no_remote, *yes).await,
        Cmd::Status => {
            let v = open_vault(cli)?;
            let s = backup::status(&v)?;
            out.done(status_json(&v, &s), || ui::status_text(&v, &s, ui::color()));
            Ok(())
        }
        Cmd::Statusline { wrap, pixel } => {
            // Harnesses pass session JSON on stdin: drain it, and forward it to a wrapped command.
            let mut input = Vec::new();
            if !std::io::stdin().is_terminal() {
                let _ = std::io::stdin().read_to_end(&mut input);
            }
            let color = std::env::var_os("NO_COLOR").is_none();
            let p = vault_path(cli).unwrap_or_else(|_| config::default_vault_path());
            let now = chrono::Utc::now();
            let theirs = wrap.as_ref().and_then(|cmd| {
                let mut child = std::process::Command::new("sh")
                    .args(["-c", cmd])
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .ok()?;
                child.stdin.take()?.write_all(&input).ok()?;
                let out = child.wait_with_output().ok()?;
                let s = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
                (!s.is_empty()).then_some(s)
            });
            let Some(snap) = hippo::statusline::snapshot(&p) else {
                println!("{}🦛 no vault · run `hippo init`", theirs.map(|t| format!("{t}  ")).unwrap_or_default());
                return Ok(());
            };
            if *pixel {
                // Their status line above, the buddy block below.
                if let Some(t) = theirs {
                    println!("{t}");
                }
                println!("{}", hippo::statusline::buddy(&snap, color, now));
            } else {
                // One row: their last line, then ours.
                let ours = hippo::statusline::line(&snap, color, now);
                match theirs {
                    Some(t) => println!("{t}  {ours}"),
                    None => println!("{ours}"),
                }
            }
            Ok(())
        }
        Cmd::Connect { harness: Harness::Claude { no_statusline, pixel } } => {
            let r = hippo::connect::connect_claude(!*no_statusline, *pixel)?;
            out.done(json!(r), || {
                format!(
                    "{} Claude Code connected\n  MCP          {}\n  instructions {}\n  status line  {}\n  → Restart Claude Code to load hippo. Undo anytime: `hippo disconnect claude`",
                    green("✓"), r.mcp, r.instructions, r.statusline
                )
            });
            Ok(())
        }
        Cmd::Disconnect { harness: Harness::Claude { .. } } => {
            let r = hippo::connect::disconnect_claude()?;
            out.done(json!(r), || {
                format!(
                    "{} Claude Code disconnected\n  MCP          {}\n  instructions {}\n  status line  {}\n  Your vault and backups are untouched.",
                    green("✓"), r.mcp, r.instructions, r.statusline
                )
            });
            Ok(())
        }
        Cmd::Buddy { name, color } => {
            let mut gc = GlobalConfig::load()?;
            if let Some(n) = name {
                gc.buddy.name = (!n.is_empty()).then(|| n.clone());
            }
            if let Some(c) = color {
                let c = c.to_lowercase();
                if !["lavender", "pink", "gray", "grey", "blue"].contains(&c.as_str()) {
                    bail!("color must be one of: lavender, pink, gray, blue");
                }
                gc.buddy.color = Some(c);
            }
            if name.is_some() || color.is_some() {
                gc.save()?;
            }
            let shade = gc.buddy.color.clone().unwrap_or_else(|| "lavender".into());
            out.done(json!({"name": gc.buddy.name, "color": shade}), || {
                format!(
                    "{}\n\n  name   {}\n  color  {shade}\n\n  Show it in Claude Code: \"statusLine\": {{\"type\": \"command\", \"command\": \"HIPPO_PIXEL=1 hippo statusline\", \"refreshInterval\": 1}}",
                    hippo::statusline::sheet(&shade, ui::color()),
                    gc.buddy.name.clone().unwrap_or_else(|| "hippo (set one with --name)".into())
                )
            });
            Ok(())
        }
        Cmd::Log { n } => {
            let v = open_vault(cli)?;
            let entries = hippo::log::tail(&v.root, *n);
            out.done(json!({"entries": entries}), || {
                entries
                    .iter()
                    .map(|e| {
                        let at = e["at"].as_str().unwrap_or("").get(..19).unwrap_or("").replace('T', " ");
                        let action = e["action"].as_str().unwrap_or("?");
                        let mut rest: Vec<String> = e
                            .as_object()
                            .map(|o| o.iter().filter(|(k, _)| !matches!(k.as_str(), "at" | "action")).map(|(k, v)| format!("{k}={}", v.as_str().map(str::to_string).unwrap_or(v.to_string()))).collect())
                            .unwrap_or_default();
                        rest.sort();
                        format!("{}  {:<15} {}\n", dim(&at), bold(action), rest.join("  "))
                    })
                    .collect()
            });
            Ok(())
        }
        Cmd::Scan => {
            let v = open_vault(cli)?;
            let mut findings = vec![];
            for m in v.list()? {
                for mut f in hippo::secrets::heuristic(&m.render()) {
                    f.file = Some(format!("memories/{}.md", m.name));
                    findings.push(f);
                }
            }
            let tf = hippo::secrets::trufflehog_git(&v.root, None)?;
            let tf_ran = tf.is_some();
            if tf_ran && tf.as_ref().is_some_and(|f| f.is_empty()) {
                hippo::secrets::clear_findings(&v.root)?;
            }
            findings.extend(tf.unwrap_or_default());
            out.done(json!({"findings": findings, "trufflehog": tf_ran}), || {
                let mut s = String::new();
                for f in &findings {
                    s += &format!("{} {:<22} {:<40} {}  {}\n", red("⚠"), f.kind, f.file.clone().unwrap_or_default(), f.preview, dim(&f.source));
                }
                s += &match (findings.is_empty(), tf_ran) {
                    (true, true) => format!("{} no secrets found (heuristic + trufflehog, offline)", green("✓")),
                    (true, false) => format!("{} no secrets found (heuristic; install trufflehog for a deeper scan)", green("✓")),
                    (false, _) => format!("{} finding(s). Remove them with `hippo add`/edit + `hippo save`; they stay in git history and backups until compaction.", findings.len()),
                };
                s
            });
            Ok(())
        }
        Cmd::Add { name, message, description, kind, tags, allow_secret } => {
            if !KINDS.contains(&kind.as_str()) {
                bail!("--type must be one of {}", KINDS.join(", "));
            }
            let v = open_vault(cli)?;
            let existing = v.read_memory(name).ok();
            let body = read_body(out.agent, message.clone(), existing.as_ref().map(|e| e.body.as_str()))?;
            let m = Memory {
                name: name.clone(),
                description: description.clone().or(existing.as_ref().map(|e| e.description.clone())).unwrap_or_default(),
                kind: kind.clone(),
                tags: if tags.is_empty() { existing.as_ref().map(|e| e.tags.clone()).unwrap_or_default() } else { tags.clone() },
                extra: existing.map(|e| e.extra).unwrap_or_default(),
                body,
            };
            let found = hippo::secrets::heuristic(&m.render());
            if !found.is_empty() {
                let what = hippo::secrets::describe(&found);
                if out.agent || hippo::safety::detected_agent().is_some() {
                    bail!("refused: looks like a secret ({what}); memories are sent to AI providers\n  → Remove the secret and describe where it lives instead (e.g. \"in 1Password\")");
                }
                let ok = *allow_secret
                    || (message.is_some() && std::io::stdin().is_terminal() && {
                        out.say(yellow(&format!("⚠️  this looks like a secret: {what}\n   Agents reading this memory will send it to AI providers.")));
                        prompt("Save anyway?", "y/N")?.eq_ignore_ascii_case("y")
                    });
                if !ok {
                    bail!("not saved: looks like a secret ({what})\n  → Remove it, or pass --allow-secret if you really mean it");
                }
            }
            let commit = v.remember(&m)?;
            let path = v.memory_path(name);
            out.done(json!({"path": path, "commit": commit, "changed": commit.is_some()}), || match &commit {
                Some(c) => format!("{} saved {} {}", green("✓"), pretty_path(&path), dim(&c[..8])),
                None => "unchanged".into(),
            });
            Ok(())
        }
        Cmd::Ls { kind } => {
            let v = open_vault(cli)?;
            let list: Vec<Memory> = v.list()?.into_iter().filter(|m| kind.as_ref().is_none_or(|k| &m.kind == k)).collect();
            out.done(
                json!({"memories": list.iter().map(|m| json!({"name": m.name, "type": m.kind, "description": m.description, "tags": m.tags})).collect::<Vec<_>>()}),
                || list.iter().map(|m| format!("{:<32} {:<10} {}\n", bold(&m.name), dim(&m.kind), m.description)).collect(),
            );
            Ok(())
        }
        Cmd::Show { name } => {
            let v = open_vault(cli)?;
            let m = v.read_memory(name)?;
            out.done(
                json!({"name": m.name, "type": m.kind, "description": m.description, "tags": m.tags, "body": m.body}),
                || m.render(),
            );
            Ok(())
        }
        Cmd::Search { query, limit } => {
            let v = open_vault(cli)?;
            let mut idx = Index::open(&v.root)?;
            idx.refresh(&v.memories_dir())?;
            let hits = idx.search(&query.join(" "), *limit)?;
            out.done(
                json!({"hits": hits.iter().map(|h| json!({"name": h.name, "description": h.description, "snippet": h.snippet})).collect::<Vec<_>>()}),
                || {
                    if hits.is_empty() {
                        return "no matches".into();
                    }
                    hits.iter()
                        .map(|h| format!("{}  {}\n  {}\n", bold(&h.name), dim(&h.description), h.snippet.replace('\n', " ")))
                        .collect()
                },
            );
            Ok(())
        }
        Cmd::Rm { name } => {
            let v = open_vault(cli)?;
            if !v.forget(name)? {
                bail!("no memory named `{name}`");
            }
            out.done(json!({"removed": name}), || {
                format!("{} removed {name} (still in history: `git -C {} log -- memories/`)", green("✓"), pretty_path(&v.root))
            });
            Ok(())
        }
        Cmd::Save { message } => {
            let v = open_vault(cli)?;
            let c = v.save(message)?;
            out.done(json!({"commit": c}), || match &c {
                Some(c) => format!("{} saved {}", green("✓"), dim(&c[..8])),
                None => "nothing to save".into(),
            });
            Ok(())
        }
        Cmd::Backup => {
            let v = open_vault(cli)?;
            let (key, remote, _) = remote_for(&v)?;
            let t = std::time::Instant::now();
            let r = backup::backup(&v, &key, &remote).await?;
            let secs = t.elapsed().as_secs_f64();
            out.done(json!({"seq": r.seq, "bytes": r.bytes, "resumed": r.resumed, "seconds": secs, "verified": true}), || match r.seq {
                Some(seq) => format!("{} backup #{seq} uploaded and verified ({} bytes, {secs:.1}s)", green("✓"), r.bytes),
                None if r.resumed => format!("{} resumed and verified the interrupted backup", green("✓")),
                None => format!("{} already backed up", green("✓")),
            });
            Ok(())
        }
        Cmd::Sync => {
            let v = open_vault(cli)?;
            let (key, remote, _) = remote_for(&v)?;
            let r = backup::sync(&v, &key, &remote).await?;
            let b = if r.conflicts.is_empty() { Some(backup::backup(&v, &key, &remote).await?) } else { None };
            out.done(
                json!({
                    "fetched": r.fetched.iter().map(|(d, n)| json!({"device": d, "bundles": n})).collect::<Vec<_>>(),
                    "merged": r.merged, "conflicts": r.conflicts, "merge_trace": r.trace,
                    "backup_seq": b.as_ref().and_then(|b| b.seq),
                }),
                || {
                    let mut s = String::new();
                    for (d, n) in &r.fetched { s += &format!("{} fetched {n} backup(s) from device {d}\n", green("↓")); }
                    for d in &r.merged { s += &format!("{} merged changes from device {d}\n", green("✓")); }
                    if !r.conflicts.is_empty() {
                        s += &format!("{} conflicts in: {}\n  Edit those files (keep the right lines, drop the <<<<<<< ======= >>>>>>> markers), then `hippo save`.\n", red("⚠"), r.conflicts.join(", "));
                        if let Some(t) = &r.trace { s += &format!("  Pre-merge diff kept at {}\n", pretty_path(t)); }
                    }
                    if let Some(seq) = b.as_ref().and_then(|b| b.seq) { s += &format!("{} backup #{seq} verified\n", green("✓")); }
                    if s.is_empty() { s = format!("{} up to date", green("✓")); }
                    s
                },
            );
            Ok(())
        }
        Cmd::Restore { from, to, vault_id, device } => {
            let rc = RemoteConfig::parse(from)?;
            let key = Key::load(&config::key_path()).context(
                "restoring needs your vault key at ~/.hippo/key.txt (or set HIPPO_KEY)\n  → Copy it from your password manager / paper backup first",
            )?;
            let dest = to.clone().or(cli.vault.clone()).unwrap_or_else(config::default_vault_path);
            let remote = Remote::open(&rc)?;
            let r = backup::restore(&remote, &rc, &key, &dest, RestoreOptions { vault_id: vault_id.clone(), device: device.clone() }).await?;
            let mut gc = GlobalConfig::load()?;
            if gc.default_vault.is_none() {
                gc.default_vault = Some(dest.clone());
                gc.save()?;
            }
            let n = r.vault.list()?.len();
            out.done(json!({"vault": dest, "memories": n, "bundles": r.bundles, "from_device": r.from_device}), || {
                format!(
                    "{} restored {n} memories from {} backup(s) of device {} into {}\n  This machine is a new device; its first `hippo backup` uploads a full checkpoint.",
                    green("✓"), r.bundles, r.from_device, pretty_path(&dest)
                )
            });
            Ok(())
        }
        Cmd::Remote { cmd } => {
            let mut v = open_vault(cli)?;
            match cmd {
                RemoteCmd::Show => {
                    let r = v.config.remote.as_ref().map(|r| r.display());
                    out.done(json!({"remote": r}), || r.clone().unwrap_or_else(|| "not configured".into()));
                }
                RemoteCmd::Set { target } => {
                    let rc = setup_remote(out, target).await?;
                    v.config.remote = Some(rc.clone());
                    v.config.save(&v.root)?;
                    out.done(json!({"remote": rc.display()}), || format!("{} backups go to {}  → run `hippo backup`", green("✓"), rc.display()));
                }
            }
            Ok(())
        }
        Cmd::Key { reveal } => {
            let kp = config::key_path();
            let key = Key::load(&kp)?;
            if *reveal {
                hippo::safety::require_human("hippo key --reveal", "prints your secret key", "reveal")?;
                print!("{}", std::fs::read_to_string(&kp)?);
                return Ok(());
            }
            out.done(json!({"key_file": kp, "public_key": key.recipient()}), || {
                format!("key file    {}\npublic key  {}\n\n{}", pretty_path(&kp), key.recipient(), yellow(ui::KEY_DISCLAIMER))
            });
            Ok(())
        }
        Cmd::Import { source: ImportSource::ClaudeCode { from, dry_run } } => {
            let v = open_vault(cli)?;
            let from = from.clone().unwrap_or_else(hippo::import::default_claude_dir);
            let r = import_claude(&v, &from, *dry_run).await?;
            out.done(json!(r), || {
                let mut s = String::new();
                for p in r.memories.iter().filter(|p| p.status != hippo::import::Status::Unchanged) {
                    let st = if p.status == hippo::import::Status::New { "new" } else { "updated" };
                    s += &format!("  {:<8} {:<44} {:<9} {}{}\n", st, p.name, p.kind, p.project, if p.renamed { "  (renamed: name collision)" } else { "" });
                }
                let verb = if *dry_run { "would import" } else { "imported" };
                s += &format!(
                    "{} {verb}: {} new, {} updated, {} already in hippo{}",
                    if *dry_run { dim("…") } else { green("✓") },
                    r.new,
                    r.updated,
                    r.unchanged,
                    r.backup_seq.map(|n| format!(" · backup #{n} verified")).unwrap_or_default()
                );
                s
            });
            Ok(())
        }
        Cmd::Login { device } => {
            if out.agent {
                bail!("signing in needs you at a browser\n  → Run `hippo login` yourself in a terminal");
            }
            let user = login(out, *device || !hippo::auth::can_open_browser()).await?;
            out.done(json!({"user": user}), String::new);
            Ok(())
        }
        Cmd::Logout => {
            let removed = hippo::auth::logout()?;
            out.done(json!({"removed": removed}), || {
                if removed { format!("{} signed out of hippo", green("✓")) } else { "not signed in via hippo".into() }
            });
            Ok(())
        }
        Cmd::Mcp => {
            let p = vault_path(cli)?;
            Vault::open(&p)?;
            hippo::mcp::serve(p, config::key_path()).await
        }
        Cmd::Compact { prune } => {
            let v = open_vault(cli)?;
            let (_, remote, rc) = remote_for(&v)?;
            if *prune {
                hippo::safety::require_human("hippo compact --prune", "permanently deletes backup data", &v.config.vault_id)?;
                bail!("pruning is not implemented yet — nothing was deleted");
            }
            let chains = backup::list_bundles(&remote, &v.config.vault_id).await?;
            let objects = remote.list(&backup::vault_prefix(&v.config.vault_id)).await?;
            let total: u64 = objects.iter().map(|e| e.size).sum();
            out.done(
                json!({"objects": objects.len(), "bytes": total, "devices": chains.iter().map(|(d, c)| json!({"device": d, "bundles": c.len(), "this": d == &v.config.device_id})).collect::<Vec<_>>()}),
                || {
                    let mut s = format!("{} objects, {total} bytes in {}\n", objects.len(), rc.display());
                    for (d, c) in &chains {
                        s += &format!("  device {d}: {} bundle(s){}\n", c.len(), if *d == v.config.device_id { " (this device)" } else { "" });
                    }
                    s
                },
            );
            Ok(())
        }
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let agent = cli.agent || (!cli.human && hippo::safety::detected_agent().is_some());
    let out = Out { agent };
    if let Err(e) = run(&cli, &out).await {
        let msg = format!("{e:#}");
        if agent {
            let (error, hint) = match msg.split_once("\n  → ") {
                Some((e, h)) => (e.trim().to_string(), Some(h.trim().to_string())),
                None => (msg.clone(), None),
            };
            println!("{}", json!({"ok": false, "error": error, "hint": hint}));
        } else {
            eprintln!("{} {msg}", red("✗"));
        }
        std::process::exit(1);
    }
}
