# hippo format v1

Everything needed to read a hippo vault or backup **without hippo**.

## Vault (local)

```
<vault>/
  memories/<slug>.md     one memory per file
  projects.toml          project keys → how to recognize them (git root commits, folder paths)
  media/                 any files
  .git/                  plain git repo, branch `main`
  .hippo/                local-only (gitignored): vault.toml, state.json, index.sqlite, outbox/, merges/
```

Memory file: UTF-8 Markdown with optional frontmatter. Unknown keys must be preserved.

```markdown
---
name: prefers-rust          # slug, = file stem
description: one line
type: user | feedback | project | reference | note
tags: a, b
visibility: all | private | project:<key>   # who may read it (default all)
by: user | <agent>          # who wrote this version
updated: 2026-11-09T12:00:00Z
status: draft | forget      # an agent change the user hasn't reviewed (absent = confirmed)
---
Body (Markdown).
```

A project is a key in `projects.toml`, not a folder: an agent is in project `<key>` when its folder's git repo has one of the key's root commits (true for every clone, wherever it lives and whatever it is called), else when its folder is inside one of the key's paths. hippo learns new paths and root commits as they match, so moves and renames need nothing.

```toml
[projects.safetensors]
roots = ["9f1c…"]
paths = ["/Users/me/work/safetensors"]
```

Agent writes are drafts until the user approves them (`hippo review`). Rejecting restores the last version without `status`; an agent's `forget` hides the memory from agents until the user approves the removal.

## Key

A standard [age](https://age-encryption.org/v1) X25519 identity file (`AGE-SECRET-KEY-1…`), default `~/.hippo/key.txt`. Never stored in the vault or the bucket.

## Backup (remote)

```
hippo/v1/<vault_id>/vault.json                                       plaintext, write-once
hippo/v1/<vault_id>/devices/<device_id>/<seq:08>-<sha16>.bundle.age  immutable
```

- `vault.json`: `{"format":"hippo-v1","vault_id","recipient":"age1…","created_at"}`.
- Bundle object = `age_encrypt(recipient, git_bundle)`; `sha16` = first 16 hex chars of SHA-256 of the **encrypted** bytes.
- Per device, `seq` starts at 1 and is contiguous. Bundle 1 is a full bundle of `main`; bundle *n* contains `<tip of n-1>..main`. Each device chain is self-contained.
- Objects are never overwritten. A device writes only under its own prefix.
- A bundle is *verified* once it has been re-downloaded, hash-checked, decrypted and `git bundle verify`'d.

## Manual restore (no hippo)

```sh
hf buckets cp -r hf://buckets/<user>/hippo/hippo/v1/<vault_id>/devices/<device_id> ./b   # or any S3 client on s3.hf.co
git init vault && cd vault
for f in ../b/*.bundle.age; do                     # sorted by seq
  age -d -i ~/.hippo/key.txt "$f" > /tmp/x.bundle
  git fetch /tmp/x.bundle main:refs/restore/main
done
git checkout -B main refs/restore/main
```

## Devices and agents

- **Device** = one vault replica: one folder with its own git working tree and random `device_id` (in `.hippo/vault.toml`). It is the only writer of `devices/<device_id>/`. A restored copy is a new device, even on the same machine.
- **Agent** (Claude Code, Cursor, ChatGPT desktop…) = a client of a vault. All agents on a machine share that machine's device. Provenance is recorded in commits as a git trailer, `Agent: <mcp client name>`, and in the local log `.hippo/log.jsonl` (`via`).

## Multi-device

Devices fetch each other's chains into `refs/hippo/peers/<device>/main` and `git merge` them. Conflicts stay in the files (diff3 markers); before each merge, both sides are pinned at `refs/hippo/premerge/<ts>/{ours,theirs}` and the diff is written to `.hippo/merges/`.
