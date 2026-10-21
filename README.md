# Cuenca

Cuenca is an agent-native change plane. Agents fork any storage location, work in a local checkout, publish drafts, verify against checks their base declares, and land through a pull request. They do all of this through a `cuenca` CLI and the same verbs as an MCP server. One authority on the tailnet keeps the log, the forks and the blobs. It serves the volume to the owner's Finder and Files app over stock Samba.

The design is the spec, rev 2.2: [`docs/specs/2026-10-19-agent-native-change-plane.html`](https://github.com/broomva/workspace/blob/main/docs/specs/2026-10-19-agent-native-change-plane.html) in broomva/workspace. The build plan, one graph node per crate or feature, is [`docs/plans/2026-10-21-cuenca-build-plan.html`](https://github.com/broomva/workspace/blob/main/docs/plans/2026-10-21-cuenca-build-plan.html).

| Crate | What it is |
|---|---|
| `cuenca-tree` | Git-isomorphic trees and changesets, and a bridge that reads and writes them through git with every object checked |
| `cuenca-core` | The authority's durable log, locations, forks with fencing epochs, and the blob store |

More crates land as the plan's nodes do. Install, operate and recover sections arrive with the close-out node (BRO-3260).

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

These crates moved here from `broomva/life` (`crates/cuenca/`) on 2026-10-21 (BRO-3333). `git log --follow` keeps each file's history.

MIT licensed.
