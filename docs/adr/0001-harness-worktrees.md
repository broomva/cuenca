# ADR 0001: A Git worktree is a local adapter for a Cuenca fork

Date: 2026-10-25. Status: accepted for BRO-3311 implementation; P20 pending.

## Context

The workspace spec (2026-10-19-agent-native-change-plane, rev 2.2, sections 8–9,
phase 2 and Open questions) makes Cuenca forks the unit of isolation. Harnesses
already accept local working directories, and some require Git worktree metadata.
Cuenca locations also include plain directories and unpublished/untracked files,
so an upstream Git commit cannot be the source of a fork checkout. Maestro owns
session placement (BRO-3315); this change supplies the directory it can place in.

The current node request explicitly requires a fork materialised as a Git worktree.
BRO-3311 additionally describes `fork --checkout`, a release verb, and launch tests.

## Options

1. Plain `cuenca checkout`: sufficient for cwd-based harnesses, but no Git metadata.
2. Attach to the user's existing repository: requires matching history, risks
   shared configuration/refs, and excludes locations without Git.
3. A private bare repository and a detached linked worktree of the fork tree.

## Decision

Choose option 3 for `cuenca worktree FORK DIR`, and use it for
`cuenca fork LOCATION --checkout DIR` (also `fork --from FORK`). Keep plain
`cuenca checkout` unchanged. Each checkout gets a private bare store under
`CUENCA_STATE_HOME/worktrees/repo-*` (or the usual Cuenca state home), no remote,
and one synthetic root commit whose tree is exactly the authority fork head.
The commit has no upstream ancestry. Git index and files initially match that tree.

Validate the original path component by component with `statat` using
`SYMLINK_NOFOLLOW` and `openat` using `O_NOFOLLOW`; normalize separator/dot spellings
without canonicalizing through links. Pass that same physical root to local
registration. The destination and its ancestors are exclusively owned by the caller
for the operation; concurrent ancestor renames are unsupported because Git itself
uses pathname-based APIs. Preflight empty-directory writability and validate the
fork shortcut's destination before creating the authority fork.

Materialise through cuenca-fs in a sibling staging directory, caching each distinct
HTTP blob once on disk, then reuse those verified staged bytes in the Git store, and use cuenca-tree's write/read-back checks. Create the linked
worktree without checkout and move the staged files into it. This avoids Git
attributes, filters, hooks, ignore rules and line-ending conversions changing the
fork bytes. The adapter’s Git command helper discards inherited `GIT_*` overrides and uses
no global/system config, user templates, hooks or signing. The existing tree
bridge is invoked with its opt-in isolated configuration, so its partial-clone probe
also ignores global/system config and inherited Git overrides. Other bridge callers
retain their existing configuration contract. No authority key is written
into the worktree or Git config. Refresh the Git index after moving the files,
then run and persist the same `look`/stat-cache scan used by plain checkout,
including `clean`/`ignored` reporting and the normal conservative racy-mtime rule. Refuse occupied, symlink, live and nested checkout paths.

Snapshot/verify/land remain Cuenca verbs. Git commits are optional local history;
only files on disk are snapshotted. The existing outbox/fencing behavior applies.
No automatic Git push, merge, dependency install, harness patch or placement is added.

## Consequences and limits

- First materialisation copies the full fork, then hashes its blobs into a private
  object store; it is not an instant upstream worktree or dependency cache. Measure
  this separately from installation cost. Each distinct blob is fetched once and reused for duplicate paths and Git ingestion.
- Preserve the backing directory while the linked worktree exists. After linking,
  a failure retains it and destination files for inspection; creation does not
  force-remove user files. Failed one-command checkout leaves the created fork
  available for retry. State cleanup and disk retention are explicit operator work. Reusing a deleted
  checkout path intentionally does not auto-delete the previous backing store:
  optional local-only commits may still need recovery. The exact-store inspection,
  prune and interactive reclaim commands are in `docs/harnesses.md`; never glob-delete
  stores or remove a registration with queued snapshots.
- `worktree` and `fork --checkout` are CLI-only host bootstrap verbs, absent from
  MCP tools and schemas. Start MCP after obtaining the directory through the CLI.
- T3 must bind the actual checkout through its supported existing-worktree strategy;
  Paseo accepts its cwd. Neither app is patched; setup scripts remain app-owned.
- The merged authority exposes takeover but no agent release endpoint. This slice
  does not invent a client-only ownership release or pretend that snapshot releases
  a fork. A final snapshot is supported; lease release needs its own core/server
  contract, tracked in [BRO-3486](https://linear.app/broomva/issue/BRO-3486). P20 should assess this explicit BRO-3311 gap.
- Local authority volume evidence does not prove Finder's WAN/SMB timing or deployment
  to the NUC. Record launch/volume evidence and unavailable checks in the receipt.
