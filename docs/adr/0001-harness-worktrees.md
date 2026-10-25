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

Materialise through cuenca-fs in a sibling staging directory, hash blobs into the
private store, and use cuenca-tree's write/read-back checks. Create the linked
worktree without checkout and move the staged files into it. This avoids Git
attributes, filters, hooks, ignore rules and line-ending conversions changing the
fork bytes. Git subprocesses discard inherited `GIT_*` overrides and use no global
or system config, user templates, hooks or signing. No authority key is written
into the worktree or Git config. Destination ownership is exclusive during creation,
as for cuenca-fs checkout. Refuse occupied, symlink, live and nested checkout paths.

Snapshot/verify/land remain Cuenca verbs. Git commits are optional local history;
only files on disk are snapshotted. The existing outbox/fencing behavior applies.
No automatic Git push, merge, dependency install, harness patch or placement is added.

## Consequences and limits

- First materialisation copies the full fork, then hashes its blobs into a private
  object store; it is not an instant upstream worktree or dependency cache. Measure
  this separately from installation cost. Blobs are fetched one at a time.
- Preserve the backing directory while the linked worktree exists. After linking,
  a failure retains it and destination files for inspection; creation does not
  force-remove user files. Failed one-command checkout leaves the created fork
  available for retry. State cleanup and disk retention are explicit operator work.
- T3 must bind the actual checkout through its supported existing-worktree strategy;
  Paseo accepts its cwd. Neither app is patched; setup scripts remain app-owned.
- The merged authority exposes takeover but no agent release endpoint. This slice
  does not invent a client-only ownership release or pretend that snapshot releases
  a fork. A final snapshot is supported; lease release needs its own core/server
  contract, tracked in [BRO-3486](https://linear.app/broomva/issue/BRO-3486). P20 should assess this explicit BRO-3311 gap.
- Local authority volume evidence does not prove Finder's WAN/SMB timing or deployment
  to the NUC. Record launch/volume evidence and unavailable checks in the receipt.
