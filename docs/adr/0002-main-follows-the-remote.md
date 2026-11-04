# ADR 0002: A remote-backed location's main follows its remote

Date: 2026-11-04. Status: proposed (BRO-3601, part 2 of BRO-3517). Design only; no code
lands until this ADR passes review.

## Context

A location with a GitHub remote lands through pull requests, but its `main` never
moves. `Event::MainAdvanced` refuses with `HasRemote`, and no verb refreshes it. So
every fork starts from the tree the location was registered with, and agents never see
each other's merged work. That blocks the shared-workspace use (BRO-3529).

Part 1 (BRO-3600, cuenca#47) already parents each landing on `Store::root_main`, the
main the fork's line started from. A fork made before main moves therefore lands with
only its own changes. What remains is moving main itself.

A combined attempt (cuenca#40 → #46) went through five P20 rounds: 4, 5, 6, 5, 6. It
stopped on two refuted continuation predictions. Its findings set the requirements
below. Each round fixed one call site and the next round found another, because
nothing bounded the *operation* as a whole, and the gate engine (#3) arrived meanwhile
with its own view of main's history.

## Requirements

1. **One operation, one deadline.** A follow of one location is one operation with one
   deadline, `follow_timeout`, measured from its start. Every step checks the deadline
   and the stop flag before it starts, and passes the time remaining to the call it
   makes. That covers default-branch discovery, fetch, `rev-parse`, the tree read, the
   blob reads and the ingest. No step may outlive the operation, including reads
   through `cuenca_tree::git::Repo`, whose runner has no timeout today.
2. **Bounded ingest.** Any update the per-blob cap allows must follow, however large
   in total. Blobs are read and ingested in batches bounded by bytes (below the
   `Repo` memory limit). Each batch commits before the next, and the whole run stays
   under the operation's deadline. A follow that runs out of deadline leaves only
   ingested blobs behind; those are content-addressed and harmless. The next follow
   reads only what is still missing.
3. **Gate history.** `MainFollowed` is part of main's history for the gate engine
   (`gates::History`):
   - `held` gains every `(path, blob)` of the followed tree, as for `LocationAdded`.
   - `moves` gains `(at, paths)`, where `paths` are the paths that differ between the
     previous main's tree and the followed tree, a tree diff.

   Without this, the revert gate cannot see a blob that was followed away and later
   restored, and the governance gates miss followed changes. `history_tags` must
   select `main_followed`, and the tag test must cover it.
4. **Bounded effect on landings and shutdown.** Following never delays a landing
   request by more than one operation's deadline, and stopping the lander waits at
   most that long.

## Decision

- **Where it runs.** Following runs on the lander thread, after the round's landings.
  A second thread is not used: rounds 2–4 showed it needs cancellation of every call,
  descendant processes and threads, which the landing path does not have either
  (BRO-3599). Each round runs **at most one** follow operation, on the location that
  is due and was least recently followed. Requirement 4 then holds by construction:
  one round adds at most one deadline.
- **The deadline.** An `OpDeadline { until: Instant }` is threaded through the follow.
  `Git::run_in` gains a timeout argument, set to `deadline.remaining()`. The calls on
  the landing path keep `LandConfig::timeout`. `Repo` gains the same bound: its runner
  and batch reader take a deadline and kill their child when it passes. That fixes
  both the follow and any later caller that wants a bound.
- **Bounded joins.** After a kill, a call must not wait for its pipe-reader threads,
  since a descendant (an ssh or credential helper) can hold the pipe open past the
  kill (round 4). `output_within`, and the `Repo` runner, join readers only until the
  deadline, then detach them and return a timeout error. A detached reader ends when
  the descendant does. That bounds the caller without process-group kill. The
  descendant itself is BRO-3599's to reap.
- **Ingest.** `missing` blobs are grouped into batches of at most 64 MiB. Each batch is
  read with `read_blobs` and ingested. The deadline is checked between batches. The
  `MainFollowed` record is written only after every blob of the tree is present
  (`check_entries` already enforces this).
- **History.** `main_history` tracks each location's current main while it reads the
  log:
  - `LocationAdded` sets it.
  - `MainAdvanced` applies the changeset.
  - `MainFollowed` diffs it against the followed tree, then sets it.

  This adds one tree load per `MainFollowed`. The diff uses the existing `Tree` entry
  maps.
- **Kept from #46 (each passed review):**
  - `MainFollowed { location, main, commit }` and `Store::follow_main`, recorded with
    its tree in one transaction, refusing a location without a remote or a same-tree
    follow.
  - `Location::followed`.
  - `Repo::read_tree_known`: blobs the store holds are not re-hashed, while ruled blobs
    and tree objects still are.
  - The two-phase parent search: 1,000 first-parent commits, then 1,000,000, with one
    commit of lookahead, and distinct `Absent` and `BeyondWindow` results.
  - Followed roots in backups (`prefix_roots`).
  - The CLI log line.
- **Stale ref locks.** Before a fetch, a `refs/cuenca/<ref>.lock` older than
  `follow_timeout` is removed. The age check replaces #46's "any lock is stale"
  assumption, which round 5 showed is too strong: `Lander::spawn` is public, and a
  timed-out process's descendant can outlive it. A second lander on one repository is
  documented as unsupported, and `Lander::spawn` takes an advisory lock file in
  `repos/` so a second spawn fails loudly.

## Tests the build must carry

- **Deadline:** a follow whose steps each take just under the deadline ends within
  one deadline, and neither a landing asked for meanwhile nor stopping waits longer.
  The fixture is a git wrapper that sleeps per step, plus a `Repo` read that stalls.
- **Bounded joins:** a wrapper whose background grandchild holds stdout past the kill
  returns within the deadline.
- **Ingest:** an update of more than 1 GiB in total, with blobs below the per-blob
  cap, follows. Use a test-sized cap, so the fixture stays small.
- **History:**
  - a blob followed away and then restored by a draft is caught by the revert gate;
  - a followed change to a governance path appears in `moves` with its time;
  - the `history_tags` test selects `main_followed`.
- **Carried over from #46:** follow, next fork, no revert, a fork-of-fork root 1,101
  commits back, the backup round-trip, the hung-remote fairness test, and the stale
  lock test.
- **Mutations:** one per requirement (deadline not threaded, unbatched ingest,
  `MainFollowed` missing from history, the lock age check removed).

## Consequences and limits

- Main can trail the remote by up to `locations × (round interval)` when many
  locations are due, because each round follows one location. That is acceptable for
  a workspace of tens of locations. A per-location interval can come later.
- Process-group kill (BRO-3599) remains a separate change. With bounded joins, the
  bounds above hold without it. What it adds is reaping the descendants, so repeated
  stalls do not accumulate processes.
- A location registered from a tree its remote never had still cannot land, as before.
  Following does not change that: its first follow moves main to the remote's tree,
  and only forks made after that can land.
