# hippo — exploration

> Your AI memory as plain files you own, encrypted locally and backed up to a Hugging Face bucket, so every agent can use it and none can keep it.

Goal of this doc: pick the building blocks for a usable demo tonight. Hackathon scope, but nothing that corners us later.

> **Update (same night):** S3 was dropped in favor of the native bucket API via `hf-hub` 1.0 (reuses the `hf auth login` token, no S3 keys), plus "Sign in with HF" OAuth (PKCE + loopback). Keys live at `~/.hippo/key.txt`, outside the vault. See `todo.md` for current state.

## 1. Prior art: what to steal, what to avoid

| Project | What it is | Take | Leave |
|---|---|---|---|
| **Basic Memory** | Markdown files + SQLite index + MCP server (Python, AGPL) | Closest thing to hippo. Validates "Markdown is canonical, SQLite is a derived index, MCP is the adapter". | No private, owned backup story (they sell their own cloud sync). AGPL, Python. |
| **Claude Code memory** | One fact per `.md` file with YAML frontmatter, plus a `MEMORY.md` index | A ready-made, agent-friendly convention. Importing it is trivial. | Tied to one agent, stored locally only. |
| **mem0 / Letta / OpenMemory** | Memory *servers*, vector-first | Nothing for v1. | Data lives in a DB or service, not in files you own. |
| **restic / rustic** | Encrypted, deduplicated, snapshotting backup | Proves the "immutable objects + snapshots" model. `rustic_core` is a solid fallback engine. | No merge story across devices, opaque repo format, and more machinery than we need. |
| **git-remote-gcrypt** | Encrypted git remote | Proves "git + encryption + dumb storage" works. | Rewrites the whole pack on push, so it's fragile and slow. |

## 2. Key finding: the backup format is **encrypted incremental git bundles**

The vault is already a git repo. `git bundle` is a standard, documented file format that holds a range of commits. So:

```
backup  = git bundle create <last-verified>..main  →  age encrypt  →  PUT (create-only)
restore = list → GET → age decrypt → git bundle verify → git fetch   (in order)
```

Validated in a throwaway prototype (`scratchpad/bundle-test`):
- A full bundle (270 B), then an incremental one (352 B), restored into an empty repo ✅
- Divergent edits on two devices merge cleanly through the bundles ✅
- Conflicting edits stop the merge, and both versions are kept with conflict markers. Nothing is silently dropped ✅

Why this beats a custom manifest/patch format:
- **No format to invent.** git already provides the hashes, parents, verification (`git bundle verify`), history and three-way merge.
- **Restore without hippo.** A user can recover with stock tools alone: `aws s3 cp` (or `hf buckets cp`), then `age -d`, then `git fetch`. That's the strongest portability guarantee we can offer.
- **Everything is encrypted.** File names, commit messages and contents are all opaque to the bucket. The bucket only sees how many objects there are, their sizes and their timestamps.

### Your Q2: MVCC across devices

The git commit graph *is* the MVCC structure. Bucket layout:

```
<bucket>/v1/devices/<device-id>/000001.bundle.age     # immutable, create-only (If-None-Match: *)
<bucket>/v1/devices/<device-id>/000002.bundle.age
<bucket>/v1/devices/<device-id>/full-000040.bundle.age  # periodic compaction point
<bucket>/v1/vault.json                                   # format version, created_at (write-once)
```

- Each device appends only under its own prefix, so writers never contend and we don't need compare-and-swap.
- The bucket has **zero mutable objects**. A device's "head" is its highest-numbered bundle that verifies. HF honors `If-None-Match` on PutObject, so a retried or duplicate upload can never overwrite an existing bundle.
- Pulling from another device means fetching its new bundles and running `git merge`. A conflict stays in the files as markers (and shows in the TUI) until someone resolves it.
- Compaction means writing a `full-N` bundle. Restore starts from the newest full bundle. In v1 nothing is ever deleted.
- CRDTs (automerge) would be overkill, and their binary documents work against "readable Markdown is canonical".

## 3. HF Storage Buckets: what the docs say

Source: [S3 gateway docs](https://huggingface.co/docs/hub/storage-buckets-s3), [buckets overview](https://huggingface.co/docs/hub/en/storage-buckets), [huggingface_hub guide](https://huggingface.co/docs/huggingface_hub/guides/buckets).

- Endpoint `https://s3.hf.co/<namespace>`, region `us-east-1`, path-style addressing, `HFAK…` keys generated from an HF token.
- ✅ PutObject, GetObject, ListObjectsV2, multipart upload, and `If-None-Match`/`If-Match` on PUT.
- ⚠️ GetObject returns a 302 redirect to the CDN **except for aws-cli, botocore and aws-sdk-rust**, which the gateway proxies. That pushes us to use `aws-sdk-s3`.
- ⚠️ Clients must set the checksum options to `when_required`, because the gateway doesn't parse trailing CRC32 checksums.
- ⚠️ No versioning and no object lock, and **deletes are immediate and permanent**. That makes our create-only naming the only protection against overwrites, so the code must never call delete.
- ❓ Not documented: ETags, HeadObject, and read-after-write or list-after-write consistency. So verification is a full read-back plus a sha256 comparison (cheap, since bundles are small).
- ❓ Default visibility may be public. Always create buckets with `private` set and check afterwards. The data is encrypted anyway, so this is defense in depth.
- Note: client-side encryption defeats Xet's chunk dedup. That's acceptable at memory-sized volumes.

## 4. Crate picks

| Need | Pick | Why / rejected |
|---|---|---|
| CLI | `clap` 4 | Standard choice. |
| TUI | `ratatui` 0.30 + crossterm | Standard choice. Goes in the second half of the night. |
| Git | **Shell out to the `git` CLI** | `git2` (libgit2) has no bundle support, and `gix` bundle support is incomplete as far as I know. git is on every developer machine; check for it at startup with a clear error. |
| Encryption | `age` 0.12 | X25519 identities, a stable format, interoperates with the `age` and `rage` CLIs. Marked "beta" on crates.io, but the file format is frozen. |
| S3 | `aws-sdk-s3` | It's the one Rust client the HF gateway explicitly proxies, and it supports `if_none_match`. It's heavy to compile; we accept that. Rejected `object_store` and `opendal` because their 302-redirect behavior is unverified. |
| Index / search | `rusqlite` (bundled) + FTS5 | A derived index that can be rebuilt from the files and is gitignored. No embeddings in v1. |
| MCP | `rmcp` 3.x, stdio | The official SDK. |
| Keychain / 1Password | `keyring` 4 and `op` CLI | Later. |
| Fallback backup engine | `rustic_core` | Only if bundles fall over. |

## 5. Vault layout

```
~/hippo/                    # default location, overridable
  memories/*.md             # one memory per file, YAML frontmatter (name, description, type, tags)
  media/                    # binaries, committed as-is (warn above ~20 MB)
  .git/                     # local history; commit signing disabled per repo
  .gitignore                # ignores .hippo/
  .hippo/                   # local-only, never committed or uploaded
    key.txt                 # age identity, chmod 600  ← YOUR KEY
    config.toml             # bucket, device id, endpoint
    state.json              # last uploaded / last verified bundle per device
    index.sqlite            # derived
```

Key policy for v1, as you requested: generate the key locally and write it into `.hippo/`. Right after `init`, show:

> ⚠️ Your vault key lives only on this machine. Nothing is cloud-backed yet. If you lose this key, you lose access to your backups — no one, including us, can recover them. Store a copy somewhere safe (password manager, printed paper). See `docs/keys.md`.

## 6. The three states (status line)

```
● local     12 changes since last save        (working tree vs HEAD, plus commits not yet bundled)
● uploaded  bundle 000041 · 2 min ago         (PUT returned 200)
● verified  bundle 000041 · 2 min ago ✓       (read back, sha256 matches, decrypts, git bundle verify passes)
```

A backup counts only once it reaches **verified**.

## 7. Risks and gotchas

1. **The user's global git config signs commits with gpg.** Found in the prototype. The vault repo must set `commit.gpgsign=false` and `tag.gpgsign=false` locally, and its own `user.name`/`user.email`.
2. **Large binaries bloat git.** v1 warns on them. v2 moves them to `blobs/<sha256>.age` outside git.
3. **List-after-write consistency is undocumented.** Restore checks that sequence numbers are contiguous and fails loudly on a gap instead of restoring partial data.
4. **The key sits inside the vault folder.** If the user syncs `~/hippo` with Dropbox or iCloud, the key travels with it. Call this out in the docs.
5. **Onboarding friction from S3 keys.** See the 🙋 question in section 3.

## 8. Tonight's MVP cut

1. `hippo init`: creates the vault, runs git init, generates the key, shows the disclaimer, and optionally asks for bucket and credentials.
2. `hippo add | ls | show | search | rm`, plus editing files directly. `hippo save` commits.
3. `hippo backup`: bundle, encrypt, create-only PUT, read back, verify, update state.
4. `hippo status`: shows the three states.
5. `hippo restore <dir>`: rebuilds the vault from the key and the bucket into an empty directory. Must work on a clean machine.
6. `hippo mcp`: stdio MCP with `list`, `read`, `write` and `search`. Each write commits locally and triggers an async backup.
7. Stretch: running `hippo` with no arguments opens a ratatui dashboard (status plus a memory browser), and an importer for Claude Code and ChatGPT memory exports.
