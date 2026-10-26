# Cuenca 2026 dogfood receipts — Mac part (BRO-3259)

Updated: 2026-10-25T12:47Z, from `date -u`; local trial observed 2026-10-25T07:31Z.
**Preparation only; node incomplete.**
Delivery PR: https://github.com/broomva/cuenca/pull/31.
R35, R41 and R43 remain pending. The required finish-line checker intentionally
fails only on the pending R35/R41/R43 result statuses. The real merge is recorded
below. Nothing in this document substitutes a local
projection, advisory verification, or open PR for live acceptance.

## Contract and dependency trace

Contract: workspace `docs/specs/2026-10-25-cuenca-finish-line.md`, section dogfood;
accepted `docs/adrs/2026-10-25-cuenca-deployment-evidence.md`; graph node dogfood.
The node depends on mcp, land and deploy. Production Finder/Files, the full production
load run and LAN SMB endpoint **wait on BRO-3461 (NUC host readiness) and BRO-3257
(live deployment)**. The builder does not merge PRs; the coordinator merged #30.

Upstream implementation: `cuenca-cli/src/{mcp,verbs,config}.rs`,
`cuenca-server/src/{authority,land,materialise}.rs`,
`cuenca-cli/tests/common/mod.rs`, `.github/workflows/ci.yml`.
Source SHA: `d2f3b92ae3c5af53ece908aa7cf1c668f5b0cda3`. Downstream: BRO-3259 review,
workspace finish-line checker and eventual evidence index. This PR changes only
documents and a reproducible receipt driver; no runtime or deployment configuration.
The owner/coordinator supplies review; #30 is already merged. BRO-3461 supplies
the live host and BRO-3257 supplies live deployment.
No new App grant is requested: this trial uses the existing shared gh landing path.

Snapshot: isolated `.worktrees/bro-3259`, branch `feature/bro-3259-dogfood`,
initially clean and 0 ahead/behind fetched origin/main. Other open PRs were #3 and
#29; neither was modified. BRO-3259 was Backlog through the Broomva CLI connection.
Bridge stamp and bookkeeping status mtimes were 2026-10-25 02:17:54 and 02:18:19
America/Bogota, respectively; no branch session was in the conversation index.
No Codex bridge hook capture is assumed; session-end capture remains unverified.

## Dogfood plan and environment

- Entry surface: real `cuenca mcp` stdio against real `cuenca serve` on the SSD.
- Driver: [run-local.py](dogfood-2026/run-local.py), Python stdio JSON-RPC client,
  direct Cuenca CLI for load and repeat land/status, and real GitHub via the lander.
- Evidence: raw MCP [first fresh PR transcript](dogfood-2026/mcp-first-pr.jsonl),
  [final transcript](dogfood-2026/mcp.jsonl), [commands](dogfood-2026/commands.jsonl),
  journal logs, metrics and hashes below. Key-mint output is redacted.
- Smoke: initialize, tools/list, scoped forks.
- End-to-end: fork, checkout, edit, status, diff, snapshot, log, verify, land;
  GitHub export/PR, explicit no-remote owner wait, concurrent drafts and restart.
- Receipt anchor: this document and the BRO-3259 receipts PR.

Host: macOS 26.5.2, arm64, external SSD scratch. Debug binary
built from the source SHA above with Rust 1.99.0. Authority and advisory runner use
the same macOS user. Bind is explicitly unsafe loopback, not a production tailnet
binding. The Tailscale identity executable is the **test suite's local fixture**
(`dogfoodlocalnode`); this proves neither real tailnet authentication nor OS custody.
No Samba service is installed or modified. Only public repository contents and a
scratch location created for this trial are registered. No private location is read.

[Envelope](dogfood-2026/envelope.json) retains authority argv, state path and
SHA-256 of the binary, public identity fixture and base check manifest. Local tokens
and authority keys stay outside Git; [privacy check](dogfood-2026/privacy-check.json)
compares receipt bytes against generated token values without printing them.

## Reproduction

Checkpoint commits and pushes preceded build/test/load steps. Scratch and TMPDIR
were under `/Volumes/Broomva/scratch/bro-3259`; no builds used `/tmp`.
For a fresh trial (the scratch directory must not already exist):

```sh
export CARGO_TARGET_DIR=/Volumes/Broomva/scratch/bro-3259/target
export TMPDIR=/Volumes/Broomva/scratch/bro-3259/tmp
cargo build -p cuenca-cli --locked
python3 docs/dogfood-2026/run-local.py \
  --scratch /Volumes/Broomva/scratch/bro-3259/new-trial \
  --output /Volumes/Broomva/scratch/bro-3259/new-receipts \
  --binary "$CARGO_TARGET_DIR/debug/cuenca" --github --agents 30 --seconds 120
```

`--github` creates/adopts a real PR on broomva/cuenca; it never merges.
Without it the public git seed also uses owner-wait landing. The final completed
trial used `trial-5`; earlier fixture/setup attempts are described below. A repeat
of the same primary candidate adopted the existing export/PR, not another PR.
The first creation latency is reported separately from later adoption latency.

## Disposable authority exports and cleanup

A disposable authority leaves `cp/<changeset-id>` branches and GitHub PRs behind.
A later authority can adopt the same remote export/PR after identity validation,
as the repeat trials did for #30. Removing local state does not reset that remote
namespace. After stopping the trial, archive receipts and the exact remote IDs,
check for live authority requests, close only a discarded trial PR if still open,
and remove only its retired branch with an expected-SHA lease. Preserve merged
PR history and verify the archive before removing that trial's scratch state.
The [workflow guide](dogfood-local-workflow.md#disposable-authority-cleanup) contains
the exact #30 inventory and conditional cleanup commands. The
[read-only inventory](dogfood-2026/cleanup-inventory.json) confirms the recorded
remote export branch still exists at its expected SHA. This update performs no
remote cleanup.

## R35 — real MCP workflow and useful GitHub change (pending)

The agent edited only `docs/dogfood-local-workflow.md` inside the **returned checkout**
`/Volumes/Broomva/scratch/bro-3259/trial-5/checkout`. The file explains the local evidence boundary, owner-wait
semantics and the required merged-tree check. This is a real documentation change.
The source tree imported 96 files. The raw transcript includes
all nine agent tools and no owner tools; every final request has a successful
matching response. Owner setup is performed separately over the local admin socket,
never represented as an agent/MCP privilege.

Readable sequence (full raw events are in the structured block and linked JSONL):

```text
initialize protocolVersion=2024-11-05 -> serverInfo.name=cuenca
tools/list -> forks fork checkout status snapshot diff log verify land
fork location=cuenca name=bro-3259-docs -> epoch=1
checkout fork=bro-3259-docs -> clean=true, files=96, returned root above
filesystem edit: docs/dogfood-local-workflow.md only, inside that root
status / diff -> exactly that added file
snapshot -> pending=0, published=1
log -> draft_published and verify_recorded
verify -> passed=true, checks=[], manifest=null (source main has no manifest)
land -> in_review, pr=30, URL below
log -> landing_intent, export_intent, exported, review_intent, in_review
```

- PR created by Cuenca: https://github.com/broomva/cuenca/pull/30 (**merged by the coordinator**; shared gh identity `broomva`, not an App).
- Changeset: `81a6cb1d11cb73d8a9301f90ba5c1ccd40e65b00987f654e28ef9407455fb38b`.
- Base tree: `c17ac6b0e26453f28936f18cd636f9e73282f198`.
- Candidate tree: `ccdf02aa132117edd64fd9346837efef120dc422`.
- Export commit: `f340cb61cf44db2442202a1a0254de9f03bcbc26`.
- GitHub export tree: `ccdf02aa132117edd64fd9346837efef120dc422` — **equal to candidate**.
- First fresh request → PR-open: 10.261 s, one sample,
  [original workflow](dogfood-2026/workflow-first-pr.json). This is not a p95.
- Merged SHA: `3063874e903d3ca7388b811a5846de0323a9b8b8`; merged tree
  `ccdf02aa132117edd64fd9346837efef120dc422` — **equal to candidate and PR head**.
- Main CI [run 40173296675](https://github.com/broomva/cuenca/actions/runs/40173296675)
  completed successfully at that merged SHA; all seven CI jobs passed.
  [Raw merge metadata](dogfood-2026/github-pr-30-merged.json),
  [raw main CI result](dogfood-2026/github-main-ci-40173296675.json), and
  [tree/containment receipt](dogfood-2026/tree-equality.json) bind the observations.

This whole-tree equality is scoped to **unmoved main**: the export and squash merge
both have parent `d2f3b92ae3c5af53ece908aa7cf1c668f5b0cda3`. The exact PR head tree
was checked against the candidate, and `git diff --exit-code <head> <merge>`
returned 0, establishing that the merge contains the complete head content.
GitHub records that exact head as the merged PR. Because #30 was squashed,
`git merge-base --is-ancestor <head> <merge>` returns 1; no ancestry is claimed.
If main moves, record the rebased candidate and merged result and verify the head
change is contained; never infer equality with the original candidate from PR
status alone. For an ordinary merge also verify head ancestry.

[GitHub checks at the exported head](dogfood-2026/github-pr-30.json) (preserved
pre-merge snapshot) report SUCCESS
for Format, Lint, Test (Linux), Test (macOS), MSRV Check, Security Audit,
Deploy (container), and CodeRabbit. This records observed checks, not a claim that
all merge reviews/rules have been satisfied. Local verify's empty manifest is
explicitly zero checks, not gated-merge evidence.

The no-remote scratch branch was also driven by MCP, with a nonempty base manifest:

```text
verify -> scratch-readme: passed, argv=[/bin/test, -s, README.md], exit_code=0
land -> status=requested, waiting_for=owner, pr=null
```

Manifest SHA-256: `3796ef17a51193b5c78b538aa1f10b48cc81a68b873eb1c8dc749003e1ace11c`;
[raw result](dogfood-2026/workflow.json). No owner admin land was called.
Finder/Files marker hash, timed screenshot, device comparison and ≤15 s production
visibility remain pending on BRO-3461 (host) and BRO-3257 (deploy). Local materializer
timing below is not that receipt.

## R41 — scaled concurrent trial (pending)

The final trial ran 30 independent agent principals/forks concurrently,
with 17 rounds of snapshots, base-manifest verification and landing
requests. Fifteen forks changed the same `conflict.txt` path to different contents;
fifteen used disjoint per-agent paths. They stayed distinct drafts. **Conflict
resolution/rebase/merged results were not exercised**: the scratch location has no
remote and every land request waits for the owner. No bulk GitHub PRs were opened.

Actual UTC interval: 2026-10-25T07:26:35Z to 2026-10-25T07:28:38Z; timed publish loop
121.261 s (requested 120 s). This is a deliberately scaled
local trial, not the specified one-hour production run. Setup and final audit are
outside the timed interval; the complete local trial stayed below 20 minutes.
Other local CI/benchmark activity shared this Mac during parts of the run.

- Acknowledged changesets: 510; lost: **0**; duplicates: **0**.
- Authority SIGKILL once at 2026-10-25T07:27:38Z, between acknowledged batches;
  restart readiness 0.110 s. No mid-request crash claim.
- p95 snapshot CLI wall time: 1.448 s.
- p95 snapshot invocation → local projected bytes visible: 8.616 s.
  This includes snapshot call time and direct filesystem polling; **not SMB/Finder**.
- p95 land invocation → owner-wait response: 0.094 s.
- p95 bulk request → GitHub PR-open: **unmeasured**, null in metrics.
- Logical returned journal JSON: 637,522 bytes;
  linear hourly projection 18,926,756 bytes/hour.
- `store/log.redb` allocated size: 1,589,248 → 2,641,920 bytes;
  linear allocated-growth projection 31,251,731 bytes/hour.
- Free disk: 116,200,325,120 → 116,044,050,432 bytes;
  journal-allocation-only extrapolation 154.7 days.

The extrapolation is **not observed hourly/daily acceptance** and excludes blobs,
client caches, logs, other workloads, and allocator nonlinearity. Logical JSON bytes
are serialized returned journal records, not exact on-disk redb accounting.

[Metrics](dogfood-2026/load.json), [acknowledgements](dogfood-2026/acknowledged.json),
[ID-by-ID audit](dogfood-2026/journal-audit.json), and `log-load-00.json` through
`log-load-29.json` retain the raw fork histories. A separate verification recomputed
the acknowledged-ID multiset against every fork's actual `draft_published` records;
all 510 matched exactly once after restart. It also checked all MCP pairs and the
GitHub export tree: [validation](dogfood-2026/validation.json).

## Mac↔VPS and R43 — local timing baseline (pending)

Exact attempted remote command:

```sh
ssh -o BatchMode=yes -o ConnectionAttempts=1 -o ConnectTimeout=5 \
  srv1692698 'cuenca --json forks'
```

Exit 255: `ssh: connect to host srv1692698 port 22: Operation timed out`.
[Receipt](dogfood-2026/vps.json). No remote fork listing or cross-host agreement is
claimed. The earlier 10-second connection attempt also timed out; a subsequent
client attempt hit its local deadline. No authority was deployed on the VPS.

Single-sample local SSD timings, [full commands/results](dogfood-2026/benchmarks.json):

- Workspace `git status --porcelain=v1 --untracked-files=normal`: 0.0826 s;
  output suppressed to keep unrelated/private path names out of receipts.
- `cargo build -p cuenca-tree --locked`: 4.3414 s; cached debug dependencies,
  following the MSRV check, not a clean release build.
- 1,000-file create: 0.0831 s; rewrite: 0.0791 s; subsequent file fsync: 0.0245 s.
- Cuenca snapshot of those 1,000 files: 0.7811 s, one published changeset, zero queued.

The bulk edits are confined to a separate Cuenca-returned scratch checkout.
A Cuenca checkout is a plain directory, so the requested Git workspace timing uses
the real workspace checkout, not a fabricated `.git` inside a fork.
There is **no SMB-side time or measured LAN crossover**. No disposable Cuenca SMB
endpoint was available and VPS SSH was unreachable; production waits on BRO-3461.
Use local checkouts for agent work, builds and bulk edits. The quantitative owner
SMB-versus-checkout recommendation remains pending the paired LAN measurements.

## Verification, failures and limitations

Local CI equivalent, exact commands and outputs are retained under
`dogfood-2026/ci-*.log` and [ci-summary.json](dogfood-2026/ci-summary.json):

```text
cargo fmt --all -- --check                                  exit 0
cargo clippy --workspace --all-targets --locked -- -D warnings exit 0
cargo test --workspace --locked                              exit 0 (370 passed, 0 failed)
cargo +1.93.0 check --workspace --locked                      exit 0
cargo audit                                                  exit 0
bash -n deploy/install.sh deploy/check.sh deploy/test-container.sh \
  deploy/test-systemd.sh deploy/wait-tailnet deploy/tailnet-addr.sh exit 0
bash deploy/test-container.sh                                exit 0
bash deploy/test-systemd.sh                                  exit 1 (missing required Linux binary argument)
```

The systemd script header requires a Linux Cuenca binary and privileged container.
The no-argument attempt failed before exercising systemd, so this is **unavailable**,
not a product/test pass. No live `deploy/check.sh`, production rollout or NUC reboot
was performed. This docs-only task did not change units or installation code.

Preparation failures are preserved honestly: an invalid fixture node name and a
Python argument collision stopped initial setup; a bad `/usr/bin/test` command on
macOS produced [recorded failed verify](dogfood-2026/fixture-failed-verify.json)
(`not_run`, missing executable), corrected to `/bin/test`; the first concurrent
cycle omitted verify and was [refused with HTTP 409 not_verified](dogfood-2026/fixture-unverified-land.json).
The fixture driver was corrected and the final fresh trial completed successfully.
These are receipt-driver errors/expected refusals, not unresolved Cuenca defects.
An earlier redb-path assumption stopped load before its first batch; corrected to
`authority/store/log.redb`. No earlier partial trial is folded into the 510 count.

**Anti-rationalization check: yes** — the actual CLI/MCP user surface was driven,
with real filesystem effects and a real Cuenca-created GitHub PR. Visual/Finder
interaction was not performed and remains an explicit acceptance gap.
No new ADR was needed; the accepted deployment-evidence ADR governs this split.

## Structured finish-line receipt (intentionally incomplete)

The PR/SHA fields now identify the real merged MCP workflow demonstration (#30).
For each pending row they bind the shared landed workflow baseline, not completion
of that row; the load/benchmark receipts still describe local preparation only.
Pending rows must not be changed to pass until the NUC/live-deployment receipts
exist (BRO-3461 host; BRO-3257 deploy). Receipt paths are relative to `docs/`.

```cuenca-dogfood
{
  "pr": "https://github.com/broomva/cuenca/pull/30",
  "merged_sha": "3063874e903d3ca7388b811a5846de0323a9b8b8",
  "receipt": "dogfood-2026/tree-equality.json",
  "mcp_transcript": [
    {
      "jsonrpc": "2.0",
      "id": 1,
      "method": "initialize",
      "params": {
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "clientInfo": {
          "name": "bro-3259-receipt",
          "version": "1"
        }
      }
    },
    {
      "jsonrpc": "2.0",
      "id": 1,
      "result": {
        "protocolVersion": "2024-11-05",
        "capabilities": {
          "tools": {}
        },
        "serverInfo": {
          "name": "cuenca",
          "version": "0.1.0"
        },
        "instructions": "Cuenca: fork any directory, work in a local checkout, publish snapshots as drafts and ask to land them. `fork` a location, `checkout` the fork into an empty directory, edit files there, `snapshot` to publish, `diff` and `log` to inspect, `land` to ask for it to land. Paths are absolute."
      }
    },
    {
      "jsonrpc": "2.0",
      "method": "notifications/initialized"
    },
    {
      "jsonrpc": "2.0",
      "id": 4,
      "method": "tools/list",
      "params": {}
    },
    {
      "jsonrpc": "2.0",
      "id": 4,
      "result": {
        "tools": [
          {
            "name": "forks",
            "description": "List the locations and forks this key may read.",
            "inputSchema": {
              "additionalProperties": false,
              "properties": {},
              "required": [],
              "type": "object"
            },
            "outputSchema": {
              "oneOf": [
                {
                  "properties": {
                    "forks": {
                      "items": {
                        "properties": {
                          "head": {
                            "type": "string"
                          },
                          "location": {
                            "type": "string"
                          },
                          "name": {
                            "type": "string"
                          }
                        },
                        "required": [
                          "name",
                          "location",
                          "head"
                        ],
                        "type": "object"
                      },
                      "type": "array"
                    },
                    "locations": {
                      "items": {
                        "type": "object"
                      },
                      "type": "array"
                    }
                  },
                  "required": [
                    "locations",
                    "forks"
                  ]
                },
                {
                  "properties": {
                    "error": {
                      "properties": {
                        "code": {
                          "description": "the authority's error code",
                          "type": "string"
                        },
                        "message": {
                          "type": "string"
                        },
                        "offline": {
                          "description": "the authority was unreachable",
                          "type": "boolean"
                        },
                        "status": {
                          "description": "the HTTP status",
                          "type": "integer"
                        }
                      },
                      "required": [
                        "message"
                      ],
                      "type": "object"
                    }
                  },
                  "required": [
                    "error"
                  ]
                }
              ],
              "type": "object"
            },
            "annotations": {
              "readOnlyHint": true
            }
          },
          {
            "name": "fork",
            "description": "Fork a location: a new fork of its main, held by you under epoch 1. Or fork a fork (`from` instead of `location`): a new fork of its head, in its location; an owner fork is snapshotted first, so the new fork holds every write that reached the volume. Name exactly one of `location` and `from`.",
            "inputSchema": {
              "additionalProperties": false,
              "properties": {
                "from": {
                  "description": "The fork to fork, at its head, instead of a location.",
                  "type": "string"
                },
                "location": {
                  "description": "The location to fork, at its main.",
                  "type": "string"
                },
                "name": {
                  "description": "The fork's name (lowercase ASCII, digits, `.`, `_`, `-`); generated if absent.",
                  "type": "string"
                }
              },
              "required": [],
              "type": "object"
            },
            "outputSchema": {
              "oneOf": [
                {
                  "properties": {
                    "fork": {
                      "properties": {
                        "head": {
                          "type": "string"
                        },
                        "location": {
                          "type": "string"
                        },
                        "name": {
                          "type": "string"
                        }
                      },
                      "required": [
                        "name",
                        "location",
                        "head"
                      ],
                      "type": "object"
                    }
                  },
                  "required": [
                    "fork"
                  ]
                },
                {
                  "properties": {
                    "error": {
                      "properties": {
                        "code": {
                          "description": "the authority's error code",
                          "type": "string"
                        },
                        "message": {
                          "type": "string"
                        },
                        "offline": {
                          "description": "the authority was unreachable",
                          "type": "boolean"
                        },
                        "status": {
                          "description": "the HTTP status",
                          "type": "integer"
                        }
                      },
                      "required": [
                        "message"
                      ],
                      "type": "object"
                    }
                  },
                  "required": [
                    "error"
                  ]
                }
              ],
              "type": "object"
            },
            "annotations": {
              "readOnlyHint": false,
              "destructiveHint": false
            }
          },
          {
            "name": "checkout",
            "description": "Materialise a fork's head in a directory (missing or empty) and make it a checkout.",
            "inputSchema": {
              "additionalProperties": false,
              "properties": {
                "dir": {
                  "description": "Absolute path of the directory to create or fill.",
                  "pattern": "^/",
                  "type": "string"
                },
                "fork": {
                  "description": "The fork to check out.",
                  "type": "string"
                }
              },
              "required": [
                "fork",
                "dir"
              ],
              "type": "object"
            },
            "outputSchema": {
              "oneOf": [
                {
                  "properties": {
                    "clean": {
                      "type": "boolean"
                    },
                    "cloned": {
                      "minimum": 0,
                      "type": "integer"
                    },
                    "copied": {
                      "minimum": 0,
                      "type": "integer"
                    },
                    "epoch": {
                      "minimum": 0,
                      "type": "integer"
                    },
                    "files": {
                      "minimum": 0,
                      "type": "integer"
                    },
                    "fork": {
                      "type": "string"
                    },
                    "head": {
                      "type": "string"
                    },
                    "ignored": {
                      "items": {
                        "type": "string"
                      },
                      "type": "array"
                    },
                    "location": {
                      "type": "string"
                    },
                    "root": {
                      "type": "string"
                    }
                  },
                  "required": [
                    "fork",
                    "location",
                    "epoch",
                    "head",
                    "root",
                    "files",
                    "clean",
                    "ignored"
                  ]
                },
                {
                  "properties": {
                    "error": {
                      "properties": {
                        "code": {
                          "description": "the authority's error code",
                          "type": "string"
                        },
                        "message": {
                          "type": "string"
                        },
                        "offline": {
                          "description": "the authority was unreachable",
                          "type": "boolean"
                        },
                        "status": {
                          "description": "the HTTP status",
                          "type": "integer"
                        }
                      },
                      "required": [
                        "message"
                      ],
                      "type": "object"
                    }
                  },
                  "required": [
                    "error"
                  ]
                }
              ],
              "type": "object"
            },
            "annotations": {
              "readOnlyHint": false,
              "destructiveHint": false
            }
          },
          {
            "name": "status",
            "description": "Show the checkout's fork, its changes since the last snapshot, what is queued, and the fork as the authority has it.",
            "inputSchema": {
              "additionalProperties": false,
              "properties": {
                "dir": {
                  "description": "Absolute path of a directory inside the checkout.",
                  "pattern": "^/",
                  "type": "string"
                }
              },
              "required": [
                "dir"
              ],
              "type": "object"
            },
            "outputSchema": {
              "oneOf": [
                {
                  "properties": {
                    "acknowledged": {
                      "type": "string"
                    },
                    "authority": {
                      "type": "object"
                    },
                    "changes": {
                      "items": {
                        "properties": {
                          "change": {
                            "enum": [
                              "added",
                              "modified",
                              "mode",
                              "deleted",
                              "other"
                            ]
                          },
                          "kind": {
                            "enum": [
                              "file",
                              "executable",
                              "symlink",
                              "other"
                            ]
                          },
                          "path": {
                            "type": "string"
                          }
                        },
                        "required": [
                          "path",
                          "change"
                        ],
                        "type": "object"
                      },
                      "type": "array"
                    },
                    "epoch": {
                      "minimum": 0,
                      "type": "integer"
                    },
                    "fork": {
                      "type": "string"
                    },
                    "ignored": {
                      "items": {
                        "type": "string"
                      },
                      "type": "array"
                    },
                    "location": {
                      "type": "string"
                    },
                    "queued": {
                      "minimum": 0,
                      "type": "integer"
                    },
                    "root": {
                      "type": "string"
                    },
                    "skipped": {
                      "type": "array"
                    }
                  },
                  "required": [
                    "root",
                    "fork",
                    "location",
                    "epoch",
                    "queued",
                    "changes",
                    "authority"
                  ]
                },
                {
                  "properties": {
                    "error": {
                      "properties": {
                        "code": {
                          "description": "the authority's error code",
                          "type": "string"
                        },
                        "message": {
                          "type": "string"
                        },
                        "offline": {
                          "description": "the authority was unreachable",
                          "type": "boolean"
                        },
                        "status": {
                          "description": "the HTTP status",
                          "type": "integer"
                        }
                      },
                      "required": [
                        "message"
                      ],
                      "type": "object"
                    }
                  },
                  "required": [
                    "error"
                  ]
                }
              ],
              "type": "object"
            },
            "annotations": {
              "readOnlyHint": true
            }
          },
          {
            "name": "snapshot",
            "description": "Snapshot the checkout and publish it as a draft. If the authority is unreachable it stays queued and goes out with the next snapshot.",
            "inputSchema": {
              "additionalProperties": false,
              "properties": {
                "dir": {
                  "description": "Absolute path of a directory inside the checkout.",
                  "pattern": "^/",
                  "type": "string"
                },
                "drop_ignored": {
                  "default": false,
                  "description": "Accept that paths `.forkignore` now names are deleted from the fork.",
                  "type": "boolean"
                }
              },
              "required": [
                "dir"
              ],
              "type": "object"
            },
            "outputSchema": {
              "oneOf": [
                {
                  "properties": {
                    "changes": {
                      "items": {
                        "properties": {
                          "change": {
                            "enum": [
                              "added",
                              "modified",
                              "mode",
                              "deleted",
                              "other"
                            ]
                          },
                          "kind": {
                            "enum": [
                              "file",
                              "executable",
                              "symlink",
                              "other"
                            ]
                          },
                          "path": {
                            "type": "string"
                          }
                        },
                        "required": [
                          "path",
                          "change"
                        ],
                        "type": "object"
                      },
                      "type": "array"
                    },
                    "flush": {
                      "properties": {
                        "error": {
                          "type": [
                            "string",
                            "null"
                          ]
                        },
                        "offline": {
                          "type": "boolean"
                        },
                        "pending": {
                          "minimum": 0,
                          "type": "integer"
                        },
                        "published": {
                          "type": "array"
                        }
                      },
                      "required": [
                        "published",
                        "pending",
                        "offline"
                      ],
                      "type": "object"
                    }
                  },
                  "required": [
                    "changes",
                    "flush"
                  ]
                },
                {
                  "properties": {
                    "error": {
                      "properties": {
                        "code": {
                          "description": "the authority's error code",
                          "type": "string"
                        },
                        "message": {
                          "type": "string"
                        },
                        "offline": {
                          "description": "the authority was unreachable",
                          "type": "boolean"
                        },
                        "status": {
                          "description": "the HTTP status",
                          "type": "integer"
                        }
                      },
                      "required": [
                        "message"
                      ],
                      "type": "object"
                    }
                  },
                  "required": [
                    "error"
                  ]
                }
              ],
              "type": "object"
            },
            "annotations": {
              "readOnlyHint": false,
              "destructiveHint": false
            }
          },
          {
            "name": "diff",
            "description": "Show what changed: in the checkout since its last snapshot (give dir only), or from tree a to tree b (give a and b only, each a fork name or `location:<name>`).",
            "inputSchema": {
              "additionalProperties": false,
              "properties": {
                "a": {
                  "description": "The tree to compare from.",
                  "type": "string"
                },
                "b": {
                  "description": "The tree to compare to.",
                  "type": "string"
                },
                "dir": {
                  "description": "Absolute path of a directory inside the checkout.",
                  "pattern": "^/",
                  "type": "string"
                }
              },
              "required": [],
              "type": "object"
            },
            "outputSchema": {
              "oneOf": [
                {
                  "properties": {
                    "changes": {
                      "items": {
                        "properties": {
                          "change": {
                            "enum": [
                              "added",
                              "modified",
                              "mode",
                              "deleted",
                              "other"
                            ]
                          },
                          "kind": {
                            "enum": [
                              "file",
                              "executable",
                              "symlink",
                              "other"
                            ]
                          },
                          "path": {
                            "type": "string"
                          }
                        },
                        "required": [
                          "path",
                          "change"
                        ],
                        "type": "object"
                      },
                      "type": "array"
                    },
                    "from": {
                      "type": "string"
                    },
                    "ignored": {
                      "items": {
                        "type": "string"
                      },
                      "type": "array"
                    },
                    "to": {
                      "type": "string"
                    }
                  },
                  "required": [
                    "from",
                    "to",
                    "changes",
                    "ignored"
                  ]
                },
                {
                  "properties": {
                    "error": {
                      "properties": {
                        "code": {
                          "description": "the authority's error code",
                          "type": "string"
                        },
                        "message": {
                          "type": "string"
                        },
                        "offline": {
                          "description": "the authority was unreachable",
                          "type": "boolean"
                        },
                        "status": {
                          "description": "the HTTP status",
                          "type": "integer"
                        }
                      },
                      "required": [
                        "message"
                      ],
                      "type": "object"
                    }
                  },
                  "required": [
                    "error"
                  ]
                }
              ],
              "type": "object"
            },
            "annotations": {
              "readOnlyHint": true
            }
          },
          {
            "name": "log",
            "description": "Show the log records naming a fork (give fork, or dir for the checkout's fork; at least one).",
            "inputSchema": {
              "additionalProperties": false,
              "properties": {
                "dir": {
                  "description": "Absolute path of a directory inside a checkout.",
                  "pattern": "^/",
                  "type": "string"
                },
                "fork": {
                  "description": "The fork.",
                  "type": "string"
                }
              },
              "required": [],
              "type": "object"
            },
            "outputSchema": {
              "oneOf": [
                {
                  "properties": {
                    "fork": {
                      "type": "string"
                    },
                    "records": {
                      "items": {
                        "type": "object"
                      },
                      "type": "array"
                    }
                  },
                  "required": [
                    "fork",
                    "records"
                  ]
                },
                {
                  "properties": {
                    "error": {
                      "properties": {
                        "code": {
                          "description": "the authority's error code",
                          "type": "string"
                        },
                        "message": {
                          "type": "string"
                        },
                        "offline": {
                          "description": "the authority was unreachable",
                          "type": "boolean"
                        },
                        "status": {
                          "description": "the HTTP status",
                          "type": "integer"
                        }
                      },
                      "required": [
                        "message"
                      ],
                      "type": "object"
                    }
                  },
                  "required": [
                    "error"
                  ]
                }
              ],
              "type": "object"
            },
            "annotations": {
              "readOnlyHint": true
            }
          },
          {
            "name": "verify",
            "description": "Run the checks the location's main declares in .cuenca/checks.toml (never the candidate's own) on the fork's head, in a temporary checkout, and record how they ended. Publishes anything queued first; the head must be the checkout's last snapshot. Advisory: the record blocks nothing. isError when a check failed, or when the authority did not take the record (recorded false, offset null, record_error says why; the check results are still reported).",
            "inputSchema": {
              "additionalProperties": false,
              "properties": {
                "dir": {
                  "description": "Absolute path of a directory inside the checkout.",
                  "pattern": "^/",
                  "type": "string"
                }
              },
              "required": [
                "dir"
              ],
              "type": "object"
            },
            "outputSchema": {
              "oneOf": [
                {
                  "properties": {
                    "base": {
                      "type": "string"
                    },
                    "candidate": {
                      "type": "string"
                    },
                    "changeset": {
                      "type": "string"
                    },
                    "checks": {
                      "items": {
                        "properties": {
                          "argv": {
                            "items": {
                              "type": "string"
                            },
                            "type": "array"
                          },
                          "duration_ms": {
                            "minimum": 0,
                            "type": "integer"
                          },
                          "exit_code": {
                            "type": [
                              "integer",
                              "null"
                            ]
                          },
                          "name": {
                            "type": "string"
                          },
                          "output": {
                            "type": "string"
                          },
                          "status": {
                            "enum": [
                              "passed",
                              "failed",
                              "timed_out",
                              "not_run"
                            ]
                          },
                          "timeout_secs": {
                            "minimum": 0,
                            "type": "integer"
                          }
                        },
                        "required": [
                          "name",
                          "argv",
                          "status",
                          "exit_code",
                          "duration_ms"
                        ],
                        "type": "object"
                      },
                      "type": "array"
                    },
                    "fork": {
                      "type": "string"
                    },
                    "manifest": {
                      "type": [
                        "object",
                        "null"
                      ]
                    },
                    "offset": {
                      "type": [
                        "integer",
                        "null"
                      ]
                    },
                    "passed": {
                      "type": "boolean"
                    },
                    "record_error": {
                      "type": [
                        "object",
                        "null"
                      ]
                    },
                    "recorded": {
                      "type": "boolean"
                    }
                  },
                  "required": [
                    "fork",
                    "changeset",
                    "candidate",
                    "base",
                    "manifest",
                    "checks",
                    "passed",
                    "recorded",
                    "offset",
                    "record_error"
                  ]
                },
                {
                  "properties": {
                    "error": {
                      "properties": {
                        "code": {
                          "description": "the authority's error code",
                          "type": "string"
                        },
                        "message": {
                          "type": "string"
                        },
                        "offline": {
                          "description": "the authority was unreachable",
                          "type": "boolean"
                        },
                        "status": {
                          "description": "the HTTP status",
                          "type": "integer"
                        }
                      },
                      "required": [
                        "message"
                      ],
                      "type": "object"
                    }
                  },
                  "required": [
                    "error"
                  ]
                }
              ],
              "type": "object"
            },
            "annotations": {
              "readOnlyHint": false,
              "destructiveHint": false
            }
          },
          {
            "name": "land",
            "description": "Ask to land the checkout's fork at its head, which `verify` must have checked against the location's main (publishes anything queued first). On a location with a git remote the authority opens a pull request, which GitHub's required checks gate; this waits up to a minute for it. On one without, the request waits for the owner. `status`: requested, exporting, exported, reviewing, in_review, landed, failed, or dropped (a takeover or a newer request replaced it).",
            "inputSchema": {
              "additionalProperties": false,
              "properties": {
                "dir": {
                  "description": "Absolute path of a directory inside the checkout.",
                  "pattern": "^/",
                  "type": "string"
                }
              },
              "required": [
                "dir"
              ],
              "type": "object"
            },
            "outputSchema": {
              "oneOf": [
                {
                  "properties": {
                    "changeset": {
                      "type": "string"
                    },
                    "fork": {
                      "type": "string"
                    },
                    "offset": {
                      "minimum": 0,
                      "type": "integer"
                    },
                    "pr": {
                      "type": [
                        "integer",
                        "null"
                      ]
                    },
                    "reason": {
                      "type": [
                        "string",
                        "null"
                      ]
                    },
                    "retrying_after": {
                      "type": [
                        "string",
                        "null"
                      ]
                    },
                    "status": {
                      "type": "string"
                    },
                    "url": {
                      "type": [
                        "string",
                        "null"
                      ]
                    },
                    "waiting_for": {
                      "enum": [
                        "owner",
                        "authority",
                        null
                      ],
                      "type": [
                        "string",
                        "null"
                      ]
                    }
                  },
                  "required": [
                    "fork",
                    "changeset",
                    "offset",
                    "status",
                    "waiting_for"
                  ]
                },
                {
                  "properties": {
                    "error": {
                      "properties": {
                        "code": {
                          "description": "the authority's error code",
                          "type": "string"
                        },
                        "message": {
                          "type": "string"
                        },
                        "offline": {
                          "description": "the authority was unreachable",
                          "type": "boolean"
                        },
                        "status": {
                          "description": "the HTTP status",
                          "type": "integer"
                        }
                      },
                      "required": [
                        "message"
                      ],
                      "type": "object"
                    }
                  },
                  "required": [
                    "error"
                  ]
                }
              ],
              "type": "object"
            },
            "annotations": {
              "readOnlyHint": false,
              "destructiveHint": false
            }
          }
        ]
      }
    },
    {
      "jsonrpc": "2.0",
      "id": 6,
      "method": "tools/call",
      "params": {
        "name": "forks",
        "arguments": {}
      }
    },
    {
      "jsonrpc": "2.0",
      "id": 6,
      "result": {
        "content": [
          {
            "type": "text",
            "text": "locations:\n  cuenca  main c17ac6b0e26453f28936f18cd636f9e73282f198  remote https://github.com/broomva/cuenca.git\n  scratch  main c3f0016f6aa4684cf5dea5a7de24cd39bc1301bc\nforks:\n"
          }
        ],
        "structuredContent": {
          "forks": [],
          "locations": [
            {
              "main": "c17ac6b0e26453f28936f18cd636f9e73282f198",
              "main_moved": null,
              "name": "cuenca",
              "remote": "https://github.com/broomva/cuenca.git"
            },
            {
              "main": "c3f0016f6aa4684cf5dea5a7de24cd39bc1301bc",
              "main_moved": null,
              "name": "scratch",
              "remote": null
            }
          ]
        },
        "isError": false
      }
    },
    {
      "jsonrpc": "2.0",
      "id": 8,
      "method": "tools/call",
      "params": {
        "name": "fork",
        "arguments": {
          "location": "cuenca",
          "name": "bro-3259-docs"
        }
      }
    },
    {
      "jsonrpc": "2.0",
      "id": 8,
      "result": {
        "content": [
          {
            "type": "text",
            "text": "forked cuenca as bro-3259-docs (epoch 1, head c17ac6b0e26453f28936f18cd636f9e73282f198)\n"
          }
        ],
        "structuredContent": {
          "fork": {
            "base": "c17ac6b0e26453f28936f18cd636f9e73282f198",
            "head": "c17ac6b0e26453f28936f18cd636f9e73282f198",
            "head_changeset": null,
            "holders": [
              {
                "node": "dogfoodlocalnode",
                "principal": "dogfood-mac"
              }
            ],
            "landing": null,
            "location": "cuenca",
            "name": "bro-3259-docs",
            "parent": null,
            "released": [],
            "verified": null
          }
        },
        "isError": false
      }
    },
    {
      "jsonrpc": "2.0",
      "id": 10,
      "method": "tools/call",
      "params": {
        "name": "checkout",
        "arguments": {
          "fork": "bro-3259-docs",
          "dir": "/Volumes/Broomva/scratch/bro-3259/trial-5/checkout"
        }
      }
    },
    {
      "jsonrpc": "2.0",
      "id": 10,
      "result": {
        "content": [
          {
            "type": "text",
            "text": "checked out bro-3259-docs (96 files, head c17ac6b0e26453f28936f18cd636f9e73282f198) into /Volumes/Broomva/scratch/bro-3259/trial-5/checkout\n"
          }
        ],
        "structuredContent": {
          "clean": true,
          "cloned": 0,
          "copied": 96,
          "epoch": 1,
          "files": 96,
          "fork": "bro-3259-docs",
          "head": "c17ac6b0e26453f28936f18cd636f9e73282f198",
          "ignored": [],
          "location": "cuenca",
          "root": "/Volumes/Broomva/scratch/bro-3259/trial-5/checkout"
        },
        "isError": false
      }
    },
    {
      "jsonrpc": "2.0",
      "id": 12,
      "method": "tools/call",
      "params": {
        "name": "status",
        "arguments": {
          "dir": "/Volumes/Broomva/scratch/bro-3259/trial-5/checkout"
        }
      }
    },
    {
      "jsonrpc": "2.0",
      "id": 12,
      "result": {
        "content": [
          {
            "type": "text",
            "text": "/Volumes/Broomva/scratch/bro-3259/trial-5/checkout is a checkout of fork bro-3259-docs (cuenca), epoch 1\nacknowledged c17ac6b0e26453f28936f18cd636f9e73282f198, 0 snapshot(s) queued\nauthority: head c17ac6b0e26453f28936f18cd636f9e73282f198, epoch 1, held by dogfood-mac@dogfoodlocalnode\nchanges since the last snapshot:\nA docs/dogfood-local-workflow.md\n"
          }
        ],
        "structuredContent": {
          "acknowledged": "c17ac6b0e26453f28936f18cd636f9e73282f198",
          "authority": {
            "epoch": 1,
            "head": "c17ac6b0e26453f28936f18cd636f9e73282f198",
            "holder": {
              "node": "dogfoodlocalnode",
              "principal": "dogfood-mac"
            },
            "landing": null
          },
          "changes": [
            {
              "change": "added",
              "kind": "file",
              "path": "docs/dogfood-local-workflow.md"
            }
          ],
          "epoch": 1,
          "fork": "bro-3259-docs",
          "ignored": [],
          "location": "cuenca",
          "queued": 0,
          "root": "/Volumes/Broomva/scratch/bro-3259/trial-5/checkout",
          "skipped": []
        },
        "isError": false
      }
    },
    {
      "jsonrpc": "2.0",
      "id": 14,
      "method": "tools/call",
      "params": {
        "name": "diff",
        "arguments": {
          "dir": "/Volumes/Broomva/scratch/bro-3259/trial-5/checkout"
        }
      }
    },
    {
      "jsonrpc": "2.0",
      "id": 14,
      "result": {
        "content": [
          {
            "type": "text",
            "text": "A docs/dogfood-local-workflow.md\n"
          }
        ],
        "structuredContent": {
          "changes": [
            {
              "change": "added",
              "kind": "file",
              "path": "docs/dogfood-local-workflow.md"
            }
          ],
          "from": "c17ac6b0e26453f28936f18cd636f9e73282f198",
          "ignored": [],
          "to": "working directory"
        },
        "isError": false
      }
    },
    {
      "jsonrpc": "2.0",
      "id": 16,
      "method": "tools/call",
      "params": {
        "name": "snapshot",
        "arguments": {
          "dir": "/Volumes/Broomva/scratch/bro-3259/trial-5/checkout"
        }
      }
    },
    {
      "jsonrpc": "2.0",
      "id": 16,
      "result": {
        "content": [
          {
            "type": "text",
            "text": "snapshot: 1 change(s)\nA docs/dogfood-local-workflow.md\npublished draft 81a6cb1d11cb73d8a9301f90ba5c1ccd40e65b00987f654e28ef9407455fb38b to bro-3259-docs\n"
          }
        ],
        "structuredContent": {
          "changes": [
            {
              "change": "added",
              "kind": "file",
              "path": "docs/dogfood-local-workflow.md"
            }
          ],
          "flush": {
            "error": null,
            "offline": false,
            "pending": 0,
            "published": [
              {
                "fenced": false,
                "fork": "bro-3259-docs",
                "id": "81a6cb1d11cb73d8a9301f90ba5c1ccd40e65b00987f654e28ef9407455fb38b",
                "target": "ccdf02aa132117edd64fd9346837efef120dc422"
              }
            ]
          }
        },
        "isError": false
      }
    },
    {
      "jsonrpc": "2.0",
      "id": 18,
      "method": "tools/call",
      "params": {
        "name": "diff",
        "arguments": {
          "a": "location:cuenca",
          "b": "bro-3259-docs"
        }
      }
    },
    {
      "jsonrpc": "2.0",
      "id": 18,
      "result": {
        "content": [
          {
            "type": "text",
            "text": "A docs/dogfood-local-workflow.md\n"
          }
        ],
        "structuredContent": {
          "changes": [
            {
              "change": "added",
              "kind": "file",
              "path": "docs/dogfood-local-workflow.md"
            }
          ],
          "from": "c17ac6b0e26453f28936f18cd636f9e73282f198",
          "ignored": [],
          "to": "ccdf02aa132117edd64fd9346837efef120dc422"
        },
        "isError": false
      }
    },
    {
      "jsonrpc": "2.0",
      "id": 20,
      "method": "tools/call",
      "params": {
        "name": "log",
        "arguments": {
          "dir": "/Volumes/Broomva/scratch/bro-3259/trial-5/checkout"
        }
      }
    },
    {
      "jsonrpc": "2.0",
      "id": 20,
      "result": {
        "content": [
          {
            "type": "text",
            "text": "     2  fork bro-3259-docs of cuenca created, held by dogfood-mac@dogfoodlocalnode\n     3  draft 81a6cb1d11cb73d8a9301f90ba5c1ccd40e65b00987f654e28ef9407455fb38b on bro-3259-docs by dogfood-mac@dogfoodlocalnode (epoch 1): 1 change(s)\n"
          }
        ],
        "structuredContent": {
          "fork": "bro-3259-docs",
          "records": [
            {
              "at_ms": 1792913190714,
              "event": {
                "base": "c17ac6b0e26453f28936f18cd636f9e73282f198",
                "event": "fork_created",
                "fork": "bro-3259-docs",
                "holder": {
                  "node": "dogfoodlocalnode",
                  "principal": "dogfood-mac"
                },
                "location": "cuenca"
              },
              "offset": 2
            },
            {
              "at_ms": 1792913191598,
              "event": {
                "changeset": {
                  "base": "c17ac6b0e26453f28936f18cd636f9e73282f198",
                  "ops": 1,
                  "target": "ccdf02aa132117edd64fd9346837efef120dc422"
                },
                "event": "draft_published",
                "fork": "bro-3259-docs",
                "id": "81a6cb1d11cb73d8a9301f90ba5c1ccd40e65b00987f654e28ef9407455fb38b",
                "writer": {
                  "epoch": 1,
                  "holder": {
                    "node": "dogfoodlocalnode",
                    "principal": "dogfood-mac"
                  }
                }
              },
              "offset": 3
            }
          ]
        },
        "isError": false
      }
    },
    {
      "jsonrpc": "2.0",
      "id": 22,
      "method": "tools/call",
      "params": {
        "name": "verify",
        "arguments": {
          "dir": "/Volumes/Broomva/scratch/bro-3259/trial-5/checkout"
        }
      }
    },
    {
      "jsonrpc": "2.0",
      "id": 22,
      "result": {
        "content": [
          {
            "type": "text",
            "text": "verify of 81a6cb1d11cb73d8a9301f90ba5c1ccd40e65b00987f654e28ef9407455fb38b on bro-3259-docs: 0 of 0 check(s) passed, judged by base c17ac6b0e26453f28936f18cd636f9e73282f198, which declares no checks\nrecorded at log offset 4; advisory, it blocks nothing\n"
          }
        ],
        "structuredContent": {
          "base": "c17ac6b0e26453f28936f18cd636f9e73282f198",
          "candidate": "ccdf02aa132117edd64fd9346837efef120dc422",
          "changeset": "81a6cb1d11cb73d8a9301f90ba5c1ccd40e65b00987f654e28ef9407455fb38b",
          "checks": [],
          "fork": "bro-3259-docs",
          "manifest": null,
          "offset": 4,
          "passed": true,
          "record_error": null,
          "recorded": true
        },
        "isError": false
      }
    },
    {
      "jsonrpc": "2.0",
      "id": 24,
      "method": "tools/call",
      "params": {
        "name": "land",
        "arguments": {
          "dir": "/Volumes/Broomva/scratch/bro-3259/trial-5/checkout"
        }
      }
    },
    {
      "jsonrpc": "2.0",
      "id": 24,
      "result": {
        "content": [
          {
            "type": "text",
            "text": "asked to land 81a6cb1d11cb73d8a9301f90ba5c1ccd40e65b00987f654e28ef9407455fb38b from bro-3259-docs: in review, pull request 30 (https://github.com/broomva/cuenca/pull/30)\n"
          }
        ],
        "structuredContent": {
          "changeset": "81a6cb1d11cb73d8a9301f90ba5c1ccd40e65b00987f654e28ef9407455fb38b",
          "fork": "bro-3259-docs",
          "offset": 5,
          "pr": 30,
          "reason": null,
          "retrying_after": null,
          "status": "in_review",
          "url": "https://github.com/broomva/cuenca/pull/30",
          "waiting_for": null
        },
        "isError": false
      }
    },
    {
      "jsonrpc": "2.0",
      "id": 26,
      "method": "tools/call",
      "params": {
        "name": "log",
        "arguments": {
          "dir": "/Volumes/Broomva/scratch/bro-3259/trial-5/checkout"
        }
      }
    },
    {
      "jsonrpc": "2.0",
      "id": 26,
      "result": {
        "content": [
          {
            "type": "text",
            "text": "     2  fork bro-3259-docs of cuenca created, held by dogfood-mac@dogfoodlocalnode\n     3  draft 81a6cb1d11cb73d8a9301f90ba5c1ccd40e65b00987f654e28ef9407455fb38b on bro-3259-docs by dogfood-mac@dogfoodlocalnode (epoch 1): 1 change(s)\n     4  verify of 81a6cb1d11cb73d8a9301f90ba5c1ccd40e65b00987f654e28ef9407455fb38b on bro-3259-docs: 0 of 0 check(s) passed (no manifest)\n     5  asked to land 81a6cb1d11cb73d8a9301f90ba5c1ccd40e65b00987f654e28ef9407455fb38b from bro-3259-docs\n     6  exporting 81a6cb1d11cb73d8a9301f90ba5c1ccd40e65b00987f654e28ef9407455fb38b from bro-3259-docs as commit f340cb61cf44db2442202a1a0254de9f03bcbc26 to cp/81a6cb1d11cb73d8a9301f90ba5c1ccd40e65b00987f654e28ef9407455fb38b\n     7  exported 81a6cb1d11cb73d8a9301f90ba5c1ccd40e65b00987f654e28ef9407455fb38b from bro-3259-docs\n     8  opening a pull request for 81a6cb1d11cb73d8a9301f90ba5c1ccd40e65b00987f654e28ef9407455fb38b from bro-3259-docs\n     9  81a6cb1d11cb73d8a9301f90ba5c1ccd40e65b00987f654e28ef9407455fb38b from bro-3259-docs is in review: https://github.com/broomva/cuenca/pull/30\n"
          }
        ],
        "structuredContent": {
          "fork": "bro-3259-docs",
          "records": [
            {
              "at_ms": 1792913190714,
              "event": {
                "base": "c17ac6b0e26453f28936f18cd636f9e73282f198",
                "event": "fork_created",
                "fork": "bro-3259-docs",
                "holder": {
                  "node": "dogfoodlocalnode",
                  "principal": "dogfood-mac"
                },
                "location": "cuenca"
              },
              "offset": 2
            },
            {
              "at_ms": 1792913191598,
              "event": {
                "changeset": {
                  "base": "c17ac6b0e26453f28936f18cd636f9e73282f198",
                  "ops": 1,
                  "target": "ccdf02aa132117edd64fd9346837efef120dc422"
                },
                "event": "draft_published",
                "fork": "bro-3259-docs",
                "id": "81a6cb1d11cb73d8a9301f90ba5c1ccd40e65b00987f654e28ef9407455fb38b",
                "writer": {
                  "epoch": 1,
                  "holder": {
                    "node": "dogfoodlocalnode",
                    "principal": "dogfood-mac"
                  }
                }
              },
              "offset": 3
            },
            {
              "at_ms": 1792913192201,
              "event": {
                "base": "c17ac6b0e26453f28936f18cd636f9e73282f198",
                "candidate": "ccdf02aa132117edd64fd9346837efef120dc422",
                "changeset": "81a6cb1d11cb73d8a9301f90ba5c1ccd40e65b00987f654e28ef9407455fb38b",
                "checks": [],
                "event": "verify_recorded",
                "fork": "bro-3259-docs",
                "manifest": null,
                "writer": {
                  "epoch": 1,
                  "holder": {
                    "node": "dogfoodlocalnode",
                    "principal": "dogfood-mac"
                  }
                }
              },
              "offset": 4
            },
            {
              "at_ms": 1792913192231,
              "event": {
                "changeset": "81a6cb1d11cb73d8a9301f90ba5c1ccd40e65b00987f654e28ef9407455fb38b",
                "event": "landing_intent",
                "fork": "bro-3259-docs",
                "writer": {
                  "epoch": 1,
                  "holder": {
                    "node": "dogfoodlocalnode",
                    "principal": "dogfood-mac"
                  }
                }
              },
              "offset": 5
            },
            {
              "at_ms": 1792913194322,
              "event": {
                "base_ref": "main",
                "branch": "cp/81a6cb1d11cb73d8a9301f90ba5c1ccd40e65b00987f654e28ef9407455fb38b",
                "changeset": "81a6cb1d11cb73d8a9301f90ba5c1ccd40e65b00987f654e28ef9407455fb38b",
                "commit": "f340cb61cf44db2442202a1a0254de9f03bcbc26",
                "event": "export_intent",
                "fork": "bro-3259-docs",
                "parent": "d2f3b92ae3c5af53ece908aa7cf1c668f5b0cda3"
              },
              "offset": 6
            },
            {
              "at_ms": 1792913194762,
              "event": {
                "changeset": "81a6cb1d11cb73d8a9301f90ba5c1ccd40e65b00987f654e28ef9407455fb38b",
                "event": "exported",
                "fork": "bro-3259-docs"
              },
              "offset": 7
            },
            {
              "at_ms": 1792913194768,
              "event": {
                "changeset": "81a6cb1d11cb73d8a9301f90ba5c1ccd40e65b00987f654e28ef9407455fb38b",
                "event": "review_intent",
                "fork": "bro-3259-docs"
              },
              "offset": 8
            },
            {
              "at_ms": 1792913195271,
              "event": {
                "changeset": "81a6cb1d11cb73d8a9301f90ba5c1ccd40e65b00987f654e28ef9407455fb38b",
                "event": "in_review",
                "fork": "bro-3259-docs",
                "pr": 30,
                "url": "https://github.com/broomva/cuenca/pull/30"
              },
              "offset": 9
            }
          ]
        },
        "isError": false
      }
    },
    {
      "jsonrpc": "2.0",
      "id": 28,
      "method": "tools/call",
      "params": {
        "name": "fork",
        "arguments": {
          "location": "scratch",
          "name": "bro-3259-owner-wait"
        }
      }
    },
    {
      "jsonrpc": "2.0",
      "id": 28,
      "result": {
        "content": [
          {
            "type": "text",
            "text": "forked scratch as bro-3259-owner-wait (epoch 1, head c3f0016f6aa4684cf5dea5a7de24cd39bc1301bc)\n"
          }
        ],
        "structuredContent": {
          "fork": {
            "base": "c3f0016f6aa4684cf5dea5a7de24cd39bc1301bc",
            "head": "c3f0016f6aa4684cf5dea5a7de24cd39bc1301bc",
            "head_changeset": null,
            "holders": [
              {
                "node": "dogfoodlocalnode",
                "principal": "dogfood-mac"
              }
            ],
            "landing": null,
            "location": "scratch",
            "name": "bro-3259-owner-wait",
            "parent": null,
            "released": [],
            "verified": null
          }
        },
        "isError": false
      }
    },
    {
      "jsonrpc": "2.0",
      "id": 30,
      "method": "tools/call",
      "params": {
        "name": "checkout",
        "arguments": {
          "fork": "bro-3259-owner-wait",
          "dir": "/Volumes/Broomva/scratch/bro-3259/trial-5/owner-wait-checkout"
        }
      }
    },
    {
      "jsonrpc": "2.0",
      "id": 30,
      "result": {
        "content": [
          {
            "type": "text",
            "text": "checked out bro-3259-owner-wait (2 files, head c3f0016f6aa4684cf5dea5a7de24cd39bc1301bc) into /Volumes/Broomva/scratch/bro-3259/trial-5/owner-wait-checkout\n"
          }
        ],
        "structuredContent": {
          "clean": true,
          "cloned": 0,
          "copied": 2,
          "epoch": 1,
          "files": 2,
          "fork": "bro-3259-owner-wait",
          "head": "c3f0016f6aa4684cf5dea5a7de24cd39bc1301bc",
          "ignored": [],
          "location": "scratch",
          "root": "/Volumes/Broomva/scratch/bro-3259/trial-5/owner-wait-checkout"
        },
        "isError": false
      }
    },
    {
      "jsonrpc": "2.0",
      "id": 32,
      "method": "tools/call",
      "params": {
        "name": "snapshot",
        "arguments": {
          "dir": "/Volumes/Broomva/scratch/bro-3259/trial-5/owner-wait-checkout"
        }
      }
    },
    {
      "jsonrpc": "2.0",
      "id": 32,
      "result": {
        "content": [
          {
            "type": "text",
            "text": "snapshot: 1 change(s)\nA owner-wait.txt\npublished draft a5e1d1d4822dae8e08048d1033178a5a1231bcd65893da15e8454bb23b6e0a86 to bro-3259-owner-wait\n"
          }
        ],
        "structuredContent": {
          "changes": [
            {
              "change": "added",
              "kind": "file",
              "path": "owner-wait.txt"
            }
          ],
          "flush": {
            "error": null,
            "offline": false,
            "pending": 0,
            "published": [
              {
                "fenced": false,
                "fork": "bro-3259-owner-wait",
                "id": "a5e1d1d4822dae8e08048d1033178a5a1231bcd65893da15e8454bb23b6e0a86",
                "target": "f705d3bdfc9ebbcdd627e22f8feb6170ff445b56"
              }
            ]
          }
        },
        "isError": false
      }
    },
    {
      "jsonrpc": "2.0",
      "id": 34,
      "method": "tools/call",
      "params": {
        "name": "verify",
        "arguments": {
          "dir": "/Volumes/Broomva/scratch/bro-3259/trial-5/owner-wait-checkout"
        }
      }
    },
    {
      "jsonrpc": "2.0",
      "id": 34,
      "result": {
        "content": [
          {
            "type": "text",
            "text": "verify of a5e1d1d4822dae8e08048d1033178a5a1231bcd65893da15e8454bb23b6e0a86 on bro-3259-owner-wait: 1 of 1 check(s) passed, judged by .cuenca/checks.toml ad7b94f448a7db68036893e5f2c9a1b68331c40e of base c3f0016f6aa4684cf5dea5a7de24cd39bc1301bc\n  pass  scratch-readme  7 ms\nrecorded at log offset 12; advisory, it blocks nothing\n"
          }
        ],
        "structuredContent": {
          "base": "c3f0016f6aa4684cf5dea5a7de24cd39bc1301bc",
          "candidate": "f705d3bdfc9ebbcdd627e22f8feb6170ff445b56",
          "changeset": "a5e1d1d4822dae8e08048d1033178a5a1231bcd65893da15e8454bb23b6e0a86",
          "checks": [
            {
              "argv": [
                "/bin/test",
                "-s",
                "README.md"
              ],
              "duration_ms": 7,
              "exit_code": 0,
              "name": "scratch-readme",
              "output": "",
              "status": "passed",
              "timeout_secs": 1800
            }
          ],
          "fork": "bro-3259-owner-wait",
          "manifest": {
            "oid": "ad7b94f448a7db68036893e5f2c9a1b68331c40e",
            "sha256": "3796ef17a51193b5c78b538aa1f10b48cc81a68b873eb1c8dc749003e1ace11c"
          },
          "offset": 12,
          "passed": true,
          "record_error": null,
          "recorded": true
        },
        "isError": false
      }
    },
    {
      "jsonrpc": "2.0",
      "id": 36,
      "method": "tools/call",
      "params": {
        "name": "land",
        "arguments": {
          "dir": "/Volumes/Broomva/scratch/bro-3259/trial-5/owner-wait-checkout"
        }
      }
    },
    {
      "jsonrpc": "2.0",
      "id": 36,
      "result": {
        "content": [
          {
            "type": "text",
            "text": "asked to land a5e1d1d4822dae8e08048d1033178a5a1231bcd65893da15e8454bb23b6e0a86 from bro-3259-owner-wait: requested\nthe location has no git remote: the owner lands it with `cuenca admin land`\n"
          }
        ],
        "structuredContent": {
          "changeset": "a5e1d1d4822dae8e08048d1033178a5a1231bcd65893da15e8454bb23b6e0a86",
          "fork": "bro-3259-owner-wait",
          "offset": 13,
          "pr": null,
          "reason": null,
          "retrying_after": null,
          "status": "requested",
          "url": null,
          "waiting_for": "owner"
        },
        "isError": false
      }
    }
  ],
  "results": {
    "R35": {
      "status": "pending",
      "pr": "https://github.com/broomva/cuenca/pull/30",
      "merged_sha": "3063874e903d3ca7388b811a5846de0323a9b8b8",
      "receipt": "dogfood-2026/tree-equality.json",
      "reason": "Real MCP-to-GitHub PR #30 merged; exact PR head, candidate and merged trees match on the unmoved base; main CI green. Production Finder/Files marker, screenshot, device agreement and timed visibility remain pending on BRO-3461 (host) and BRO-3257 (deploy)."
    },
    "R41": {
      "status": "pending",
      "pr": "https://github.com/broomva/cuenca/pull/30",
      "merged_sha": "3063874e903d3ca7388b811a5846de0323a9b8b8",
      "receipt": "dogfood-2026/load.json",
      "reason": "30 forks, 510 acknowledged drafts, one kill/restart, zero lost/duplicate IDs in a scaled 121.3-second local trial; one-hour production load and bulk GitHub request-to-PR p95 remain unmeasured. Production waits on BRO-3461 (host) and BRO-3257 (deploy)."
    },
    "R43": {
      "status": "pending",
      "pr": "https://github.com/broomva/cuenca/pull/30",
      "merged_sha": "3063874e903d3ca7388b811a5846de0323a9b8b8",
      "receipt": "dogfood-2026/benchmarks.json",
      "reason": "Local times measured. No disposable SMB endpoint reachable; actual Mac LAN SMB comparison waits on BRO-3461 (host) and BRO-3257 (deploy). VPS SSH timed out."
    }
  }
}
```

## Literal acceptance output

```text
test -s docs/dogfood-2026.md && BROOMVA_WORKSPACE=~/broomva python3 ~/broomva/scripts/cuenca_finish_line_check.py dogfood docs/dogfood-2026.md
dogfood: R35: missing passing result
exit 1
```

The graph-only nonempty-file check exits 0. The stronger required check exits 1,
as intended; the only remaining failures are the pending R35/R41/R43 statuses.
The checker is fail-fast and prints R35 first; the complete status audit below
also exercises all evidence fields and transcript checks without changing statuses.

[Complete checker audit](dogfood-2026/acceptance-remaining.json) lists exactly:

```text
R35: missing passing result
R41: missing passing result
R43: missing passing result
```

R35 still needs production Finder/Files/device/visibility receipts; R41 still needs
the full one-hour production/conflict and bulk PR-open latency trial; R43 still
needs paired Mac LAN SMB measurements. All wait on BRO-3461 (host) and BRO-3257
(deploy). No missing PR, SHA, receipt path or MCP structure error remains.

## Merged-receipt follow-up validation

Observed 2026-10-25T12:51Z (`date -u`). Re-ran format, Clippy, workspace tests
(370 passed, zero failed), MSRV 1.93 and security audit: all exit 0.
[Follow-up summary](dogfood-2026/revision-ci-summary.json) and
`revision-ci-{format,clippy,test,msrv,audit}.log` retain the actual output.
The test run began from pushed checkpoint `8eb997d`; subsequent integration of
already-published main and receipt edits changed documentation only, with Rust
sources, Cargo manifests/lockfile and CI configuration unchanged. No load or
production/SMB trial was repeated or promoted to pass.

The canonical acceptance checker still exits 1 at R35; the complete diagnostic
audit finds only the three pending statuses and no other failures. PR #31 remains
a receipt update, not node completion.
