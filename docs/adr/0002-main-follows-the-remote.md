# ADR 0002: A remote-backed location's main follows its remote

Date: 2026-11-04, revision 2. Status: proposed (BRO-3601, part 2 of BRO-3517). Design
only; no code lands until this ADR passes review and its owner decisions are taken.

## Context

A location with a GitHub remote lands through pull requests, but its `main` never
moves. `Event::MainAdvanced` refuses with `HasRemote`, and no verb refreshes it. So
every fork starts from the tree the location was registered with, and agents never see
each other's merged work. That blocks the shared-workspace use (BRO-3529).

Part 1 (BRO-3600, cuenca#47) parents each landing on `Store::root_main`, the main the
fork's line started from. A fork made before main moves therefore lands with only its
own changes. What remains is moving main itself.

The combined attempt (cuenca#40 → #46) ran five P20 rounds, scoring 4, 5, 6, 5, 6, and
stopped on two refuted continuation predictions. Revision 1 of this ADR then failed
design review at 4. Its BLOCKER was that it promised an end-to-end deadline that the
store's synchronous disk and database work cannot honour. This revision promises less,
states what it does not bound, and names the decisions only the owner can take.

## What is bounded and what is not

Following has two kinds of work.

- **Remote-facing and git work:** `ls-remote`, `fetch`, `rev-parse`, `ls-tree`,
  `cat-file`. A slow network or a hung transport or credential helper stalls here, and
  every incident in the earlier rounds was here. This work is **bounded**. Each
  follow operation has one deadline, `follow_timeout`, measured from its start. Every
  git call gets the time remaining, and the deadline and the stop flag are checked
  before each step.
- **Local store work:** blob staging, `ingest_blob`, `check_entries`, tree
  serialization, and the `follow_main` write transaction. It runs synchronously
  against the authority's own disk and redb, exactly as every landing, publish and
  admin act does today, and nothing in the authority can interrupt it. This work is
  **not bounded** by the deadline. A stalled disk stalls following the same way it
  stalls landing.

**Owner decision D1:** accept that the bound covers the git and network work only, and
that local store work has no bound, as is true for every authority path today.
Recommended: accept. Bounding redb and disk I/O would need a separate writer process
with a watchdog, and that is a different and larger change.

## Requirements

1. **Bounded git work.** As above: one deadline per follow operation, and no git
   child outlives it. Process-group termination makes that true, below.
2. **Bounded resources.** A stalled or killed call leaks no processes, threads, file
   descriptors or buffers, however many times it recurs.
3. **No unsafe lock removal.** A ref lock is removed only when the lander can prove
   that the process that took it is dead. Otherwise following fails safely and shows
   the lock.
4. **Every allowed blob follows, and progress survives retries.** A blob up to the
   per-blob cap (256 MiB) follows. An update of any total size eventually follows.
   Work already done is never repeated, so a run of timeouts cannot starve progress.
5. **Gate history.** `MainFollowed` is part of main's history (`gates::History`) for
   the revert and governance gates.
6. **Gate freshness.** A landing intent is never recorded against a main that moved
   (by a landing or a follow) between gate evaluation and the append.

## Decision

- **Process-group termination; no detached readers.** Every git or gh child is spawned
  in its own process group (`CommandExt::process_group(0)`). At its deadline the
  lander kills the group (`killpg`, SIGKILL) and reaps the direct child. Every holder
  of the pipes is in the group, so the reader threads then see EOF and are joined.
  The same applies when the direct child exits normally while a descendant still holds
  a pipe: the group is killed after the deadline, and readers are joined.
  - **Covers:** the landing path, which was BRO-3599 and becomes a prerequisite of this
    build. It also covers the `cuenca_tree::git::Repo` runner and batch reader
    (stdin writer, stdout and stderr readers), which take a deadline.
  - **Limit:** a descendant that calls `setsid` leaves the group and is not killed.
    git's own helpers do not do that. An ssh ControlMaster can, and that case is
    documented as unsupported for remotes.
- **Locks are removed only after the lander killed their writer.** The lander records,
  in memory, every ref it was updating when it killed a call's group and reaped it. At
  the start of the next fetch of that ref, a lock present there is removed. That is
  proven safe: the group is dead, and `Lander::spawn` holds an exclusive `flock` on
  `<repos>/.lander.lock` for its lifetime, so no other lander writes there. Any other
  lock is left alone. Following then fails for that location with an error naming the
  lock, shown in the location status, until an operator removes it. Lock age is not
  used.
- **Ingest by size, resumable.**
  1. Read the tree structure with `ls-tree` (tree objects are still verified byte by
     byte, as in `read_tree`). The blobs the store lacks are listed, and their sizes
     come from `cat-file --batch-check`.
  2. Missing blobs are grouped greedily into batches of at most 64 MiB. A blob bigger
     than 64 MiB, up to the 256 MiB cap, forms a batch of its own.
  3. Each batch is read with `read_blobs`, checked against its ids, ingested and
     committed before the next.
  4. Every committed batch is durable, so a later follow skips it. Under the deadline
     an update of any size therefore completes over as many rounds as it needs.
  5. `MainFollowed` is recorded only when every blob is present.
  6. Content-rule checks (symlinks, `.gitattributes`) run on the ruled blobs before
     they are ingested. Rule checking stays in one place: `read_tree_known`'s rule
     logic moves to a function used here.
- **History.** `main_history` replays the log in order, tracking each location's
  current main tree:
  - `LocationAdded` sets it.
  - `MainAdvanced` applies its changeset. This replaces today's deferred collection,
    so log order is preserved.
  - `MainFollowed` diffs the current tree against the followed tree, then sets it.

  `held` gains every `(path, blob)` of the followed tree. `moves` gains
  `(at, paths)`, where `paths` covers additions, deletions, blob changes and mode
  changes. `history_tags` selects `main_followed`.
- **Fence on every move, by log offset.** Gate evaluation records the location's
  *move key*: the offset of its last `MainAdvanced` (`main_moved.offset`, which exists
  today) together with the offset of its last `MainFollowed`, stored as
  `Location::followed = Some((commit, offset))`. Today the landing-intent append
  refuses with `MainMoved` when `main_moved.offset >= seen`, where `seen` is the log
  length the gate evaluation read, and its loop then re-evaluates (`store.rs`,
  `record_landing_intent`). The same comparison is added for the follow offset, inside
  the same write transaction. Every move writes a new record at a new offset, so an away-and-back
  sequence still changes the key. Both parts are log offsets, set only by the events
  themselves, so a replay derives exactly the stored values. Old rows have no
  `followed` and need none. A stored counter is rejected for this reason: existing
  rows would read 0 while a replay counts their past `MainAdvanced` moves, and
  `replay_check` would disagree. That is the failure class of round 1's stored root.
- **Where it runs.** Following runs on the lander thread, after the round's landings,
  with at most one follow operation per round: the location that is due and was least
  recently followed. No second thread is used.
- **Kept from #46 (each passed review):**
  - `MainFollowed { location, main, commit }` and `Store::follow_main`.
  - `Location::followed`, now `(commit, offset)` for the fence above.
  - The two-phase parent search with lookahead and distinct `Absent` and
    `BeyondWindow` results.
  - Followed roots in backups.
  - The CLI log line.

## Bounds, stated with their assumptions

Let `T` be `follow_timeout`, `L` the time of one round's landings, and `S` the
unbounded local store time of one follow. Assume git children exit within `T` after
their group is killed; this is the `setsid` limit above.

- **Extra delay to a landing request** from following: at most `T + S` per round. A
  request that arrives after a round's landing snapshot also waits for the rest of that
  round (`L` plus at most `T + S`), as it does today without following.
- **Shutdown:** at most `T + S` from a follow in progress, on top of what an in-flight
  landing step already takes today. Landing calls keep `LandConfig::timeout`, and their
  group kill comes from the same change.
- **Freshness:** with `N` remote-backed locations and every follow succeeding, a
  location is followed at least once every `N` rounds, and a round lasts at most
  `poll + L + T + S`. Under persistent failures of other locations, following still
  rotates least-recently-followed first, so one failing location delays the others by
  one round per cycle, not indefinitely. A location that itself keeps failing does not
  move, and shows its error.

**Owner decision D2:** accept these bounds with `T = 60 s` and one follow per round.
For about 10 locations and an idle 30 s poll, that is about 5 minutes of lag at worst
on a healthy fleet. Recommended: accept. A per-location interval or concurrent
following can come later, if lag matters.

## Tests the build must carry

- **Bounds:**
  - each git step sleeping just under the remaining time ends within `T`;
  - a git wrapper whose grandchild holds stdout ends within `T`, with no process,
    thread or fd left;
  - the same, repeated 50 times, with no growth;
  - a normal exit with an inherited pipe;
  - blocked stdin on the batch reader.
- **Locks:**
  - a lock the lander created by a killed fetch is removed;
  - an old lock it did not create is left, and the location shows the error;
  - a second `Lander::spawn` on the same repos fails;
  - a lock left by a crash and restart is not removed automatically.
- **Ingest:**
  - a 100 MiB blob, between the batch size and the per-blob cap, follows;
  - an update over the total read cap follows across rounds;
  - repeated timeouts before the first batch still make progress once a round is
    long enough;
  - ruled blobs are still checked.
- **History:**
  - all three event kinds, interleaved across two locations, give `held` and `moves`
    in log order;
  - the revert gate catches a blob that was followed away and then restored;
  - `governance_rate` counts a followed governance change at its time, at the window's
    edges;
  - the `history_tags` test selects `main_followed`.
- **Fence:** a deterministic follow between gate evaluation and the intent append
  makes the append fail and the re-evaluation see the followed main, including
  away-and-back.
- **Carried over from #46:** follow, next fork, no revert, a fork-of-fork root 1,101
  commits back, the backup round-trip, and the hung-remote fairness test.
- **Mutations:** one for each mechanism above, not one for each requirement.

## Consequences and limits

- D1: local store work is not interruptible, the same as everywhere in the authority.
- `setsid` descendants escape the group kill; that is documented and unsupported.
- One follow per round bounds the delay and costs freshness (D2).
- A location registered from a tree its remote never had still cannot land. Its first
  follow moves main to the remote's tree, and only forks made after that can land.
- Process-group kill (BRO-3599) lands first, as part of this arc, because both the
  landing and following bounds depend on it.
