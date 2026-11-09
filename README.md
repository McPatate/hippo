# 🦛 hippo

Your AI memory as plain files you own, encrypted locally and backed up to a Hugging Face bucket, so every agent can use it and none can keep it.

```sh
curl -fsSL hf.co/buckets/mcpotato/hippo-releases/resolve/install.sh | sh
hippo init    # vault + key + private HF bucket + verified backup, connects Claude Code, imports its memories
```
Prebuilt for macOS/Linux; `hippo update` upgrades. From source: `cargo install --git https://github.com/McPatate/hippo`.
On a new machine, `hippo init` finds your vault in the bucket and restores it (put your key at `~/.hippo/key.txt` first).

## Why hippo?

There are plenty of memory tools for AI agents. hippo is built around one promise: **your memory belongs to you**, and you can always get it back.

- **Plain files you own.** Memories are Markdown in a folder on your machine. The search index is rebuilt from them at any time.
- **Encrypted before it leaves your machine,** into storage you control (a Hugging Face Storage Bucket). The provider sees ciphertext, not memories.
- **Verified checkpoints.** A backup only counts once it has been read back and checked. hippo always shows whether your latest changes are local, uploaded, or verified recoverable.
- **Recover without hippo.** Backups are standard age-encrypted git bundles: `hf buckets cp` + `age -d` + `git fetch` rebuilds your vault ([format](docs/format.md)).
- **Multi-device without silent data loss.** Devices merge through git. Conflicts are kept, never dropped, and reported to your agent so it can resolve them or ask you.
- **You approve what agents remember.** Agent writes are drafts until you review the diff, and every memory says who wrote it and when. Ask your agent for 🧹 cleanup (the `cleanup` prompt) to merge duplicates and fix stale notes together.
- **Any agent, no lock-in.** Claude, ChatGPT, local models: agents are replaceable clients over MCP.
- **One small binary** (plus `git`). No runtime, no dependency tree pulled at install time.

## Your key

`~/.hippo/key.txt`, separate from the vault. **Nothing backs up the key for you yet: lose it and your backups are unreadable.** Copy it into a password manager or onto paper (`hippo key --reveal`, run by you).

## Commands

| | |
|---|---|
| `hippo` / `hippo status` | local · uploaded · verified, at a glance |
| `hippo add <name> [-m …] [-t user\|feedback\|project\|reference\|note]` | save a memory (stdin / `$EDITOR` work too) |
| `hippo ls`, `show`, `search`, `rm` | browse |
| `hippo review` | approve or reject what agents changed (their writes are drafts until you do) |
| `hippo save` | commit edits made directly to the files |
| `hippo backup` · `hippo sync` | back up · pull other devices, merge, back up |
| `hippo restore <user>/<bucket>` | new machine: rebuild from backup + key |
| `hippo login [--device]` | sign in with Hugging Face (only if `hf auth login` isn't set up) |
| `hippo key` | where your key is and how to keep it safe |
| `hippo connect claude [--pixel]` · `hippo disconnect claude` | MCP + memory instructions + status line (wraps yours); fully reversible |
| `hippo import claude-code [--dry-run]` | import Claude Code memories (re-runnable, read-only on the source) |
| `hippo statusline [--wrap '<cmd>']` | `🦛 remembered prefers-rust 3s ago · ✓ backed up #12 · 73 memories` |
| `hippo buddy [--name …] [--color …]` | meet your hippo (the `--pixel` status line) |
| `hippo scan` · `hippo log` | secret scan (offline) · recent actions |
| `hippo mcp` | MCP server for agents |

**Agent mode:** in an agent environment (`AI_AGENT`, `CLAUDECODE`, …) or with `--agent`, every command prints one JSON object, never prompts, and errors are `{"ok":false,"error","hint"}`. Destructive commands refuse to run from agents.
