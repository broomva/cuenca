# Cuenca

Cuenca is an agent-native change plane. Agents fork any storage location, work in a local checkout, publish drafts, verify against checks their base declares, and land through a pull request. They do all of this through a `cuenca` CLI and the same verbs as an MCP server. One authority on the tailnet keeps the log, the forks and the blobs. It serves the volume to the owner's Finder and Files app over stock Samba.

The design is the spec, rev 2.2: [`docs/specs/2026-10-19-agent-native-change-plane.html`](https://github.com/broomva/workspace/blob/main/docs/specs/2026-10-19-agent-native-change-plane.html) in broomva/workspace. The build plan, one graph node per crate or feature, is [`docs/plans/2026-10-21-cuenca-build-plan.html`](https://github.com/broomva/workspace/blob/main/docs/plans/2026-10-21-cuenca-build-plan.html).

| Crate | What it is |
|---|---|
| `cuenca-tree` | Git-isomorphic trees and changesets, and a bridge that reads and writes them through git with every object checked |
| `cuenca-core` | The authority's durable log, locations, forks with fencing epochs, landing's recorded steps, and the blob store |
| `cuenca-fs` | Snapshot any directory into a tree under its `.forkignore`, and check a tree out with copy-on-write clones |
| `cuenca-server` | The authority: the agent API on the tailnet, per-principal keys, node identity, the local admin socket, and the lander that opens pull requests |
| `cuenca-cli` | The `cuenca` binary: agent verbs (`forks`, `fork`, `checkout`, `status`, `snapshot`, `diff`, `log`, `verify`, `land`) with `--json`, a per-checkout outbox, watch mode, `cuenca mcp` (the same verbs as MCP tools), `cuenca admin` owner verbs, and `cuenca serve` |

## Agents over MCP

`cuenca mcp` serves the agent verbs as MCP tools on standard input and output: one tool per verb, each with JSON input and output schemas. It acts as the principal whose key `cuenca login` stored (or `CUENCA_URL` and `CUENCA_TOKEN`). No owner verb is a tool, so an agent cannot mint a key, register a location or take a fork over through it. Paths given to tools are absolute.

```bash
cuenca login --url http://<authority>:7419 --token-stdin < key   # once per machine
claude mcp add cuenca -- cuenca mcp                              # Claude Code
codex mcp add cuenca -- cuenca mcp                               # Codex
```

For Codex the same entry can go in `~/.codex/config.toml`:

```toml
[mcp_servers.cuenca]
command = "cuenca"
args = ["mcp"]
```

## Checks and `cuenca verify`

A location declares its checks in `.cuenca/checks.toml` on its main. Each check is a name and an argv, run from the checkout's root, with an optional time limit (the default is 1800 s):

```toml
[[check]]
name = "test"
argv = ["cargo", "test", "--locked"]
timeout_secs = 900
```

`cuenca verify` runs this list on the fork's head, in a temporary checkout. The list always comes from the location's main, never from the fork: a fork that edits its own `.cuenca/checks.toml` is still judged by main's. The authority records the result beside the head changeset, with the hash of the manifest it was judged by, and refuses any record whose checks are not exactly main's list. If main has no manifest, verify passes with zero checks.

`cuenca verify` exits 0 if every check passed and the record was taken, and 1 if a check failed. It exits 3 if the authority did not take the record: the fork moved during the run, it was taken over, the key was revoked, or the authority could not be reached. The check results are still printed. In `--json` and MCP output they come with `recorded: false`, `offset: null` and a `record_error`. Re-run verify on the new head to record it.

Verify is advisory for now. The results are the caller's own, and a check reads the fork's files, so a fork that rewrites a script a check calls can make that check pass. Landing needs a verify of the head against main, whatever its result; nothing else waits on it. GitHub's required checks are the gate at landing.

## Landing: `cuenca land`

`cuenca land` asks the authority to land the fork's head draft. The head must have a verify recorded against the location's main as it is now; otherwise land is refused with `not_verified`. Asking again for the same draft is a retry and records nothing.

**A location with a git remote** (a GitHub repository: `https://github.com/o/r`, `git@github.com:o/r.git` or `ssh://git@github.com/o/r`) lands through a pull request:

1. The authority keeps a bare repository per location in `<state>/git/`. It fetches the remote's default branch and finds the newest first-parent commit (of the last 1000) whose tree is the main the verify was judged against.
2. It writes the fork's head tree and commits it onto that commit. The commit depends on the changeset and its parent only: its author and committer are `Cuenca`, and its date is when the changeset was first recorded. So every landing of one changeset onto one main builds the same commit, whichever fork or holder asks, and rebuilding it after a crash gives the same id. The pull request's body names who asked.
3. It pushes the commit to `cp/<changeset id>`, but only if that branch does not exist yet. A branch already at the commit counts as pushed. A changeset landed before is reused (its commit is the same one, built again): after a takeover dropped the first request, or from another fork with the same edit, the branch already holds one commit with the fork's head tree onto a parent whose tree is the main the verify judged, and that export and its pull request become this landing's. A branch holding anything else fails the landing.
4. It opens a pull request with `gh`, using the authority host's token (`GH_TOKEN` from `/etc/cuenca/gh.env`, [deploy/README.md](deploy/README.md) step 5), or finds the one already open from that branch.

Each step is written to the log as an intent before the call to GitHub and confirmed after it. An authority killed between the two finishes the step on restart, with no duplicate commit, branch or pull request. The authority is not a ruleset bypass actor, so GitHub's required checks gate the merge. A step that fails for a reason that may pass (the network, GitHub, a call that ran past 10 minutes and was killed) is retried every 30 s, and the error is shown at `GET /v1/forks/{fork}/landing`. A rejection that will never pass, such as push protection or a file over GitHub's size limit, is retried too; it shows there until the owner acts. A step that fails for good (the remote has no commit with the base's tree, the branch belongs to someone else, the holder was revoked) is recorded as `failed`; landing it again needs a new draft.

`cuenca land` waits up to `--wait` seconds (60 by default; the MCP tool always waits 60) and reports the step it reached: `requested`, `exporting`, `exported`, `reviewing`, `in_review` (with `pr` and `url`) or `failed` (with `reason`, and exit 1). `cuenca status` shows the same step later.

**A location without a git remote** never lands on an agent's request alone. The request waits (`waiting_for: "owner"`) until the owner reads its diff and verify, and lands it on the authority's host:

```bash
cuenca admin --socket <admin.sock> land <fork> --changeset <id>   # main moves; the earlier main stays in the log
```

Taking a fork over drops its holder's landing request, including one in flight. A branch or pull request already made for it stays on GitHub.

The branch is pushed to the location's own repository, so workflows that run on `push` or `pull_request` there run on the agent's tree before anyone reviews it, with whatever secrets those workflows get. Agents that push branches today have the same reach. Keep secrets out of workflows that run on unreviewed branches.

Run the authority with `--gh <path>` if `gh` is not on its `PATH`. The authority runs git with no system or global config. It also sets git's credential helper for `https://github.com` to `gh auth git-credential`.

## The volume: `cuenca serve --volume <dir>`

With `--volume` (or `CUENCA_VOLUME`), the authority keeps every location's main at `<dir>/main/<location>/` and every fork's head at `<dir>/forks/<fork>/` as plain folders, for stock Samba to serve to Finder. It checks the log for growth every second, so a published draft shows within a few seconds. Each tree is checked out into `<dir>/.cuenca/staging/`, flushed to disk, and swapped into place in one atomic exchange, so a reader sees a whole tree the log has held, never a half-written one. Before a swap the path's record is forgotten, and it is written again after, so an authority that fails or is killed while materialising rebuilds that path on its next pass or restart. Folders are `2750` and files `0640` (`0750` for executables): Samba's account reads them through the authority's group and cannot change them. Only one authority may materialise a volume at a time.

**Owner forks** are the folders Finder writes. The owner makes one per device on the authority's host:

```bash
cuenca admin owner-fork <location> owner-mac
```

It is held by the volume itself, never by a key, and it never moves: it cannot be taken over, and the volume holds nothing but `owner-*` forks. The authority shows it once at `<dir>/forks/owner-mac/`, writable through the group: folders `2770`, files `0660`, executables `0770`. After that, the folder is never replaced or swapped, so an edit is never moved out from under Finder. Every 10 s the authority snapshots each owner fork under the `.forkignore` the fork holds (the location's, until someone edits it in the fork), or the defaults when there is none. So `.DS_Store`, `._*` and the other defaults are left out, and publishes the changes as a draft written by the volume. An edit is in the log well inside the spec's 60 s.

The snapshot records Finder's own conversions as changes:
- a symlink saved as an XSym file becomes a change of kind;
- a lost executable bit becomes a change of mode.

`cuenca diff` shows both. A file the authority cannot read, one that keeps changing while it is read, or a rule added to the fork's `.forkignore` that names a path the fork tracks, fails that fork's snapshot loudly, with the path in the log output. Nothing of the fork is published until a later pass succeeds. An owner fork is refused at creation if the location's main tracks a path its own rules name, such as a committed `.DS_Store` or `.env`, since such a fork could never be snapshotted; remove those paths from main first. To work on an owner fork elsewhere, fork it: `cuenca fork --from owner-mac` (MCP: `fork` with `from`). The authority snapshots the owner fork first, so the new fork holds every edit that reached the volume, and what Finder writes later stays in the owner fork. A file still being written then is taken as far as it had gone (the owner fork's next snapshot has it whole), and a file that changes between the scan and its upload makes the authority snapshot again; if it cannot within 5 s, the fork is refused with `409 snapshot_failed`, naming the file.

More crates land as the plan's nodes do. Install, operate and recover sections arrive with the close-out node (BRO-3260).

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

These crates moved here from `broomva/life` (`crates/cuenca/`) on 2026-10-21 (BRO-3333). `git log --follow` keeps each file's history.

MIT licensed.
