# Cuenca dogfood 2026 — Mac preparation (BRO-3259)

Status: in progress; no live acceptance claimed. Production Finder/Files and full
load acceptance wait on BRO-3461. Merge evidence awaits the owner/coordinator;
this builder does not merge.

Receipt collection starts from Cuenca `d2f3b92ae3c5af53ece908aa7cf1c668f5b0cda3`.
Raw scratch inputs contain only a disposable location; no private locations are read.

## Dependency trace

Upstream: workspace finish-line spec, deployment-evidence ADR and finish-line
checker; `cuenca-cli/src/{mcp,verbs,config}.rs`, `cuenca-server/src/land.rs`,
`cuenca-cli/tests/common/mod.rs`, `.github/workflows/ci.yml`.
Downstream: BRO-3259 review and the workspace finish-line receipt index.
No runtime implementation or deployment configuration changes are planned.

## Dogfood plan

- Entry surface: `cuenca mcp` over stdio against an SSD-local `cuenca serve`.
- Driver: Python JSON-RPC client and direct Cuenca CLI; no mocked Cuenca server.
- Evidence: `docs/dogfood-2026/` raw transcript, metrics and checks.
- Smoke: initialize, list tools and list the scoped scratch location.
- End-to-end: fork, checkout, edit within returned checkout, status, snapshot,
  diff, log, verify and request landing; bounded concurrent draft/restart trial.
- Receipt anchor: this document and the ready BRO-3259 PR body.
