# ADR 0002: A remote-backed location's main follows its remote

Date: 2026-11-04, revision 4. Status: proposed (BRO-3601, part 2 of BRO-3517). Design
only. Owner decisions D1 and D2 are accepted (2026-11-04); no code lands until this ADR
passes review.

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

**Owner decision D1, accepted 2026-11-04:** the bound covers the git and network work
only, and local store work has no bound, as is true for every authority path today.
Bounding redb and disk I/O would need a separate writer process with a watchdog, a
different and larger change.

## Requirements

1. **Bounded git work.** As above: one deadline per follow operation, and no git
   child outlives it. Process-group termination makes that true, below.
2. **Bounded resources.** A stalled or killed call leaks no processes (except
   `setsid` escapes, below), threads or file descriptors, however many times it
   recurs, and the output it captures is bounded by a per-call cap.
3. **No lock removal.** A ref lock is never removed automatically. A lock fails the
   follow safely, and the location shows it.
4. **Allowed blobs follow, and done work is kept.** A blob up to the per-blob cap
   (256 MiB) follows when its batch fits in the deadline. Committed batches are never
   read again. A follow that does not fit fails visibly; it does not loop silently.
5. **Gate history.** Every main move (`MainAdvanced` and `MainFollowed`) is part of
   main's history (`gates::History`) for the revert and governance gates, with all
   the paths and blobs it changed.
6. **Gate freshness.** A landing intent is never recorded against a main that moved
   (by a landing or a follow) between gate evaluation and the append.

## Decision

- **Process-group termination, with bounded output.** Every git or gh child is spawned
  in its own process group (`CommandExt::process_group(0)`). At its deadline the
  lander kills the group (`killpg`, SIGKILL) and reaps the direct child. Every holder
  of the pipes is in the group, so the reader threads then see EOF and are joined.
  The same applies when the direct child exits normally while a descendant still holds
  a pipe: the group is killed after the deadline, and readers are joined. Captured
  output is bounded too:
  - each call declares a stdout cap: about 4 KiB for `rev-parse`, 1 MiB for
    `ls-remote`, 128 MiB for the parent search log (1,000,001 lines at about 82 bytes
    is about 78 MiB), 256 MiB for `ls-tree`, and 64 MiB for
    `cat-file --batch-check`;
  - stderr keeps only its last 64 KiB;
  - a reader that passes its cap kills the group, and the call fails naming the cap;
  - **the `cat-file --batch` protocol is bounded field by field.** Each header line is
    read with a limit of 256 bytes; git's `<oid> <type> <size>\n` is under 100. An
    unterminated or longer header kills the group and fails the call. Each body is read
    to exactly its declared size, which must be at most the per-blob cap and, for a
    blob, equal to the size `--batch-check` reported. For a tree object, it must equal
    the length of the canonical body cuenca computed. Raw batch stdout therefore
    never exceeds the sum of the declared sizes plus one header per object.

  **The caps are hard limits, and they are independent of the deadline.** A valid
  tree too large for the `ls-tree` or `--batch-check` cap does not follow, however
  long the timeout. The follow fails naming the cap, the bytes reached, and the
  `LandConfig` field that sets it. The supported recovery is to raise that field
  within the host's memory, the same trade as registration's existing 1 GiB tree
  limit. A repository that outgrows any setting the host can hold is out of scope for
  following, as it already is for `location add`.

  This covers the landing path, which was BRO-3599 and becomes a prerequisite of this
  build. It also covers the `cuenca_tree::git::Repo` runner and batch reader (stdin
  writer, stdout and stderr readers), which take a deadline and caps. The batch reader
  streams each blob into its batch buffer, which is bounded by the batch size below.
  A descendant that calls `setsid` leaves the group and is not killed. git's own
  helpers do not do that. An ssh ControlMaster can, and that case is documented as
  unsupported for remotes.
- **Ref locks are never removed automatically.** No lander can prove that the process
  which took a `refs/cuenca/*.lock` is dead: a killed call may never have touched the
  ref, and another git process may hold the lock. So a fetch that finds a lock, or
  fails on one, fails that location's follow with an error naming the lock. The error
  is shown in the location's status and logged, and following retries on later rounds.
  The documented recovery, after checking that no git process works in
  `<repos>/<location>.git`, is to remove the lock file. #46's automatic removal and
  revision 2's killed-writer rule are both dropped.
- **Ingest by size, keeping what is done.**
  1. Read the tree structure with `ls-tree` (tree objects are still verified byte by
     byte, as in `read_tree`). The blobs the store lacks are listed, and their sizes
     come from `cat-file --batch-check`.
  2. Missing blobs are grouped greedily into batches of at most 64 MiB. A blob bigger
     than 64 MiB, up to the 256 MiB cap, forms a batch of its own.
  3. Each batch is read, checked against its ids, ingested and committed before the
     next. A batch that completes is kept, and a later follow skips its blobs.
  4. `MainFollowed` is recorded only when every blob is present.
  5. Content-rule checks (symlinks, `.gitattributes`) run on the ruled blobs before
     they are ingested, in the one rule-checking function `read_tree_known` uses.

  **No eventual-progress guarantee.** A follow completes when its fetch, listing, size
  discovery and at least its next batch fit in `T` at the host's throughput. A
  single-blob batch must fit in `T` on its own. A follow that cannot fit fails with
  the step it reached, the time it took and the bytes left. That is shown in the
  location's status, and main does not move. For a time failure the remedy is a larger
  `follow_timeout` for the host, a configuration value, not a code change. A cap
  failure (above) has its own error and its own remedy; time does not cure it. Every fetch, listing and
  size discovery is repeated on each round, by design, because they are cheap
  relative to blob transfer for the repositories this workspace holds.
- **History, by tree diff for every move.** `main_history` replays the log in order,
  tracking each location's current main tree. Each kind of record moves it:
  - `LocationAdded` sets the tree, and `held` gains every `(path, blob)` of it, as it
    does today. A registered blob that the first follow replaces is therefore still in
    `held`, and restoring it is a revert.
  - `MainAdvanced` loads the actual new main, the landed fork's head tree, which is
    the head changeset's target and is stored. It does not apply the head changeset
    to the previous main, because a fork landed after several drafts has a head
    changeset whose base is an earlier draft, not main.
  - `MainFollowed` loads the followed tree.

  For each move, too, `held` gains every `(path, blob)` of the new tree. `moves` gains
  `(at, paths)`, where `paths` is the diff of the previous tree against the new one:
  additions, deletions, blob changes and mode changes. `history_tags` selects
  `main_followed`. This also fixes BRO-3603: today `main_history` records only the
  head changeset's ops for `MainAdvanced`, so the earlier drafts of a multi-draft
  landing are missing from `held` and `moves`.
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

**Owner decision D2, accepted 2026-11-04:** these bounds, with `T = 60 s` and one
follow per round. For about 10 locations and an idle 30 s poll, that is about 5 minutes
of lag at worst on a healthy fleet. A per-location interval or concurrent following can
come later, if lag matters.

## Tests the build must carry

- **Bounds:**
  - each git step sleeping just under the remaining time ends within `T`;
  - a git wrapper whose grandchild holds stdout ends within `T`, with no process,
    thread or fd left;
  - the same, repeated 50 times, with no growth;
  - a normal exit with an inherited pipe;
  - blocked stdin on the batch reader.
- **Locks:** a lock present at a fetch, or created by a killed fetch, is left in
  place. The location shows the error, retries continue, and following resumes after
  the lock is removed.
- **Output caps:**
  - a git wrapper printing past a call's stdout cap is killed and fails naming the
    cap and the `LandConfig` field;
  - stderr keeps only its tail;
  - a `cat-file --batch` wrapper emitting an overlong or unterminated header, or a
    body size different from `--batch-check`'s, is killed and fails;
  - a tree listing past the `ls-tree` cap fails as a cap failure, not a timeout, and
    follows once the cap is raised.
- **Ingest:**
  - a 100 MiB blob, between the batch size and the per-blob cap, follows;
  - an update over the total read cap follows across rounds when each batch fits in
    `T`;
  - a follow that cannot fit fails with its step, time and bytes left, and main
    stays;
  - ruled blobs are still checked.
- **History:**
  - all three event kinds, interleaved across two locations, give `held` and `moves`
    in log order;
  - a fork landed after three drafts records draft 1's paths and blobs (BRO-3603);
  - a registered blob that the first follow replaces, then restored by a draft, is
    caught by the revert gate;
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
