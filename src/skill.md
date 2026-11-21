---
name: hippo
description: The user's hippo memory. `/hippo review` goes through pending memory changes and tidies the memory with the user (merge, split, rename, rescope, retire), deciding everything in Claude Code. Use when the user asks to review, tidy, clean up or reorganize their memory.
argument-hint: review
---

# 🦛 hippo

`$ARGUMENTS` is what the user asked for. Today that is `review` (or nothing, which means `review`).

## `review`: the smart review

The user decides every change; you prepare them. hippo applies nothing until the user says yes in a
dialog hippo shows itself (you never see or answer it).

### 1. Pending changes first

Call `pending_changes`. These are changes agents made earlier that the user hasn't decided yet
(drafts, removals, renames). If there are any, go through them with the user **before** proposing
anything new.

A draft is a candidate, not a memory to keep as it is: judge it with the same eye as step 2.

- **Lasting and well placed** (a rule, a preference, a fact that will still hold in a month):
  recommend `Approve`.
- **Work state** (handoffs, status, todos, run results, goal shifts; dated names are a tell):
  recommend folding it. Pull out what will still matter (decisions, constraints, facts about the
  setup) into the persistent memory where it belongs, existing or new, and reject the note. If
  nothing lasts, recommend `Reject`.
- **Overlapping** another draft or an existing memory: recommend merging them.

Several notes on the same topic make one question ("fold the 3 safetensors 0.9 handoffs into
safetensors-release-process").

- One `AskUserQuestion` question per change or group (up to 4 per call). Put hippo's lines for the
  change in the question. Put the content in the option `preview`: the memory, the diff, the
  content of a memory being deleted, or for a fold the resulting memory.
- Options: the recommended one first with "(Recommended)", then whichever of `Fold into <memory>`,
  `Approve as is`, `Reject` and `Decide later` apply. Say what each does in its description, e.g.
  "deletes atomic-prs; its content is in pr-style" / "keeps atomic-prs as it is".
- Then call `propose_changes` once with the folds and merges (the writes into persistent
  memories), then `decide_changes` once with every approval and rejection, rejected folded notes
  included. hippo shows a dialog for each. Folds go first so nothing is lost if the user declines
  one; if they do, ask what they want to change.

### 2. Tidy the memory

1. `recall` `memory-policy` (with `review: true`) if it exists: how this user wants their memory organized. It wins over
   the defaults below.
2. `list_memories`, then `recall` (with `review: true`, so these reads don't count as uses) only what
   you need to compare.
3. Look for:
   - **duplicates and overlaps** → merge into one memory
   - **memories mixing topics** → split
   - **names or descriptions that contradict the content, or are cryptic** → rename / rewrite
   - **scope**: a memory belongs to the project it was learned in. Making one global needs a reason
     (it is about who the user is or how they work everywhere, or it was useful in several
     projects). Ask "make it global?", not "scope it to its project?". Rules learned in a work repo
     are often ambiguous: ask about each one, never as a batch.
   - **stale state** (handoffs, PR or release status that is over) → retire, keeping any lasting
     decision in another memory
   - personal details agents don't need (emails, phone numbers) → drop them
4. Steer with the user: one `AskUserQuestion` question per change, at most 4 per call, no
   `multiSelect` (previews only show on single-select). Each question names the change and why;
   the recommended option comes first with "(Recommended)"; every option that changes a memory has
   a `preview` with the resulting memory (or the content being deleted). The user can always type
   their own answer: follow it.
5. Call `propose_changes` once with every change the user agreed to. Group operations that belong
   together in one change (a merge is a `write` of the result plus a `remove` with `into` for each
   absorbed memory). Use `rename` and `scope` instead of rewriting a memory just to move it.
6. hippo shows the user one dialog listing every change. If they decline, nothing was applied: ask
   what to change and propose again.

### 3. Remember how they want it

When the user steers in a way that will matter next time (a rule, a preference, "not now"), update
`memory-policy` (type `feedback`, visible everywhere) with one line about it, through
`propose_changes` so it is approved in the same dialog.

Keep your own messages short: the questions and the dialog carry the review.
