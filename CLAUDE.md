# broomva/cuenca

Cuenca, the agent-native change plane. A Rust workspace: every `cuenca-*` directory is a member crate.

- **Source of truth.** The spec and the build plan live in broomva/workspace: `docs/specs/2026-10-19-agent-native-change-plane.html` and `docs/plans/2026-10-21-cuenca-build-plan.html`, plus its graph `docs/plans/2026-10-21-cuenca.graph.yaml`.
  - One Linear ticket per graph node, in the Broomva workspace.
  - When a ticket and the spec disagree, the spec wins and the ticket is fixed.
- **Driver.** The persist loop's brief is at `~/.config/broomva/fleet/briefs/cuenca/BRIEF.md`.
- **Gates.** `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, `cargo test --workspace --locked`. Each crate also has `scripts/mutation-check.sh`.
  - Every node passes P20 at the level its graph node names before merging.
- **Toolchain.** Pinned in `rust-toolchain.toml` (1.99.0); MSRV is 1.93. A toolchain bump is a reviewed edit.
- **Agent surface.** The CLI and MCP, never the SMB mount. Owner verbs live only under `cuenca admin`, on the authority's local socket.
