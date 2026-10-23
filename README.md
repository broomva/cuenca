# Cuenca

Cuenca is an agent-native change plane. Agents fork any storage location, work in a local checkout, publish drafts, verify against checks their base declares, and land through a pull request. They do all of this through a `cuenca` CLI and the same verbs as an MCP server. One authority on the tailnet keeps the log, the forks and the blobs. It serves the volume to the owner's Finder and Files app over stock Samba.

The design is the spec, rev 2.2: [`docs/specs/2026-10-19-agent-native-change-plane.html`](https://github.com/broomva/workspace/blob/main/docs/specs/2026-10-19-agent-native-change-plane.html) in broomva/workspace. The build plan, one graph node per crate or feature, is [`docs/plans/2026-10-21-cuenca-build-plan.html`](https://github.com/broomva/workspace/blob/main/docs/plans/2026-10-21-cuenca-build-plan.html).

| Crate | What it is |
|---|---|
| `cuenca-tree` | Git-isomorphic trees and changesets, and a bridge that reads and writes them through git with every object checked |
| `cuenca-core` | The authority's durable log, locations, forks with fencing epochs, and the blob store |
| `cuenca-fs` | Snapshot any directory into a tree under its `.forkignore`, and check a tree out with copy-on-write clones |
| `cuenca-server` | The authority: the agent API on the tailnet, per-principal keys, node identity, and the local admin socket |
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

Verify is advisory for now. The results are the caller's own, and a check reads the fork's files, so a fork that rewrites a script a check calls can make that check pass. The record blocks nothing. GitHub's required checks are the gate at landing.

More crates land as the plan's nodes do. Install, operate and recover sections arrive with the close-out node (BRO-3260).

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

These crates moved here from `broomva/life` (`crates/cuenca/`) on 2026-10-21 (BRO-3333). `git log --follow` keeps each file's history.

MIT licensed.
