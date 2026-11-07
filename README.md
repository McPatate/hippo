# 🦛 hippo

> Your AI memory as plain files you own, encrypted locally and backed up to a Hugging Face bucket, so every agent can use it and none can keep it.

```sh
cargo install --path .
hippo init                          # vault + key + private HF bucket + first verified backup
claude mcp add hippo -- hippo mcp   # connect an agent
```

## Why hippo?

There are plenty of memory tools for AI agents. hippo is built around one promise: **your memory belongs to you**, and you can always get it back.

- **Plain files you own.** Memories are Markdown in a folder on your machine. The search index is rebuilt from them at any time.
- **Encrypted before it leaves your machine,** into storage you control (a Hugging Face Storage Bucket). The provider sees ciphertext, not memories.
- **Verified checkpoints.** A backup only counts once it has been read back and checked. hippo always shows whether your latest changes are local, uploaded, or verified recoverable.
- **Recover without hippo.** Backups are standard age-encrypted git bundles: `hf buckets cp` + `age -d` + `git fetch` rebuilds your vault ([format](docs/format.md)).
- **Multi-device without silent data loss.** Devices merge through git. Conflicts are kept, never dropped, and reported to your agent so it can resolve them or ask you.
- **Any agent, no lock-in.** Claude, ChatGPT, local models: agents are replaceable clients over MCP.
- **One small binary** (plus `git`). No runtime, no dependency tree pulled at install time.

## Commands

| | |
|---|---|
| `hippo` / `hippo status` | local · uploaded · verified, at a glance |
| `hippo add <name> [-m …] [-t user\|feedback\|project\|reference\|note]` | save a memory (stdin / `$EDITOR` work too) |
| `hippo ls`, `show`, `search`, `rm` | browse |
| `hippo save` | commit edits made directly to the files |
| `hippo backup` · `hippo sync` | back up · pull other devices, merge, back up |
| `hippo restore <user>/<bucket>` | new machine: rebuild from backup + key |
| `hippo login [--device]` | sign in with Hugging Face (only if `hf auth login` isn't set up) |
| `hippo key` | where your key is and how to keep it safe |
| `hippo mcp` | MCP server for agents |

**Agent mode:** in an agent environment (`AI_AGENT`, `CLAUDECODE`, …) or with `--agent`, every command prints one JSON object, never prompts, and errors are `{"ok":false,"error","hint"}`. Destructive commands refuse to run from agents.

## Your key

`~/.hippo/key.txt`, separate from the vault. **Nothing backs up the key for you yet: lose it and your backups are unreadable.** Copy it into a password manager or onto paper (`hippo key --reveal`, run by you).
