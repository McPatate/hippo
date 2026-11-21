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

### 1. Survey first, ask nothing yet

Read everything before the first question: pending drafts and existing memories are judged together.

1. `recall` `memory-policy` (with `review: true`) if it exists: how this user wants their memory
   organized. It wins over the defaults below.
2. `pending_changes`: what agents changed that the user hasn't decided yet (drafts, removals,
   renames). A draft is a candidate, not a memory to keep as it is.
3. `list_memories`, then `recall` (with `review: true`, so these reads don't count as uses) what
   you need to compare.
4. Classify every pending change and every memory worth touching:
   - **lasting and well placed** (a rule, a preference, a fact that will still hold in a month)
     → approve as is
   - **work state** (handoff notes, status, todos, run results, goal shifts; dated names are a tell)
     → fold: pull what will still matter (decisions, constraints, facts about the setup) into the
     persistent memory where it belongs, existing or new, and drop the note. Nothing lasting → drop.
     Work still in flight → its latest state goes into the `handoff` for that topic, and the notes
     are dropped.
   - **stale handoffs** (marked `stale` in the index) → retire, folding lasting decisions into
     persistent memories
   - **duplicates and overlaps**, drafts included → merge into one memory
   - **memories mixing topics** → split
   - **names or descriptions that contradict the content, or are cryptic** → rename / rewrite
   - **scope**: a memory belongs to the project it was learned in. Making one global needs a reason
     (it is about who the user is or how they work everywhere, or it was useful in several
     projects). Ask "make it global?", not "scope it to its project?". Rules learned in a work repo
     are often ambiguous: ask about each one, never as a batch.
   - personal details agents don't need (emails, phone numbers) → drop them
5. Show the plan in a few lines before asking anything: counts per kind, then one line per item
   (e.g. "fold 3 safetensors 0.9 handoffs → safetensors-release-process", "stale:
   aqwenator-goal-shift-2026-11-19", "approve as is: no-worktrees-work-like-me"). Biggest wins first.

### 2. Decide with the user

1. Go through the plan with `AskUserQuestion`: one question per item (a fold or merge of several
   notes is one item), at most 4 per call, no `multiSelect` (previews only show on single-select).
   Each question names the change and why. The recommended option comes first with
   "(Recommended)", then whichever of `Fold into <memory>`, `Merge`, `Approve as is`, `Retire`,
   `Reject` and `Decide later` apply, each saying what it does (e.g. "deletes atomic-prs; its
   content is in pr-style" / "keeps atomic-prs as it is"). Every option that changes a memory has a
   `preview` with the resulting memory (or the content being deleted). The user can always type
   their own answer: follow it.
2. Call `propose_changes` once with every write the user agreed to: folds, merges, splits, renames,
   rescopes and retirements of approved memories. Group operations that belong together in one
   change (a merge is a `write` of the result plus a `remove` with `into` for each absorbed
   approved memory). Use `rename` and `scope` instead of rewriting a memory just to move it.
3. Then call `decide_changes` once with every pending change the user approved or rejected, folded
   and merged drafts included (rejected: their content now lives in the result).
4. hippo shows a dialog for each call. Writes go first so nothing is lost if the user declines one;
   if they decline, nothing in that call was applied: ask what to change and try again.

### 3. Remember how they want it

When the user steers in a way that will matter next time (a rule, a preference, "not now"), update
`memory-policy` (type `feedback`, visible everywhere) with one line about it, through
`propose_changes` so it is approved in the same dialog.

Keep your own messages short: the questions and the dialog carry the review.
