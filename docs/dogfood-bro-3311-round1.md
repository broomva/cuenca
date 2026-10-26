# BRO-3311 round 1 response receipt

Receipt times below came from `date -u +%Y-%m-%dT%H:%MZ` on the live host.
Round 1 reviewed `8964ea8`: Grok A 6/10 REVISE, Gemini B 5/10 REVISE.
This receipt records builder fixes and observations; it is not a new P20 pass.

## Reproduction before fixing

Committed and test-locked `cuenca-cli/tests/worktree_review.rs` before product
changes (`cc1d6cd`). At `2026-10-25T07:00Z`, all six tests failed against the
reviewed implementation. Actual failure excerpts:

```text
symlink destinations accepted: [.../link/, .../link/., .../link//,
  .../link/new, .../link/child/new, .../link/../unused, relative link/.]
Git removed the caller's empty directory
local invalid checkout succeeded after creating invalid-1
partial clone, which this bridge does not write into
assertion failed: git(... diff-files --name-only).is_empty()
duplicate HTTP blob fetch: {.../4163036e...: 2, .../8a36ffb5...: 3}
test result: FAILED. 0 passed; 6 failed
```

The fixture helper was subsequently corrected to force-add its explicitly declared
files, so the host's global Git ignore cannot hide `.DS_Store` from the report
comparison. The locked test file was not changed. CI now uses a physical TMPDIR
for the strict symlink contract, including macOS's `/var` alias.

## New local SSD fork and harness command

At `2026-10-25T07:05Z`, a new local authority, scoped test principal and plain
2-file/266-byte location were created under
`/Volumes/Broomva/scratch/bro-3311/dogfood-r1`. A loopback authority used
`--unsafe-bind` and the existing deterministic test Tailscale identity, with its
volume on the same SSD. Credentials stayed outside the working tree and logs.

```text
cuenca --json fork demo --name harness-r1 --checkout /Volumes/Broomva/scratch/bro-3311/dogfood-r1/work
{"clean":true,"commit":"6c95e03e14964b57ae502c5430c47888ca2a0e85","epoch":1,"files":2,"fork":"harness-r1","git_dir":"/Volumes/Broomva/scratch/bro-3311/dogfood-r1/state/worktrees/repo-WKS3Ns","head":"a6ae16ac25983c83ea50b645893bcf019289edd5","ignored":[],"location":"demo","root":"/Volumes/Broomva/scratch/bro-3311/dogfood-r1/work"}
elapsed=0.258s
```

Immediately after creation, `git diff-files --name-only` was empty, and the
checkout's external `cache.json` existed. For the same fixture, conventional
`git worktree add --detach` took 0.086s; no dependencies, reinstall cost 0s.
This is a bounded cost comparison, not a large-tree speed claim.

The current capacity file and active lanes were checked before requesting a
bounded **T3-owned Grok 4.7 harness test**, client id
`bro3311-r1-dogfood-20261025`. Returned task id:

```text
node:delegated-task:command%3Amcp%3Acf33bf88-16b6-4a2d-9de0-a4ff7d126677%3Adelegate-task%3Abro3311-r1-dogfood-20261025
```

It ran these receipt commands with an explicit working-directory change, exit 0:

```text
$ cd /Volumes/Broomva/scratch/bro-3311/dogfood-r1/work
$ date -u +%Y-%m-%dT%H:%MZ
2026-10-25T07:08Z
$ pwd
/Volumes/Broomva/scratch/bro-3311/dogfood-r1/work
$ git diff-files --name-only

$ printf 'round 1 harness in Cuenca fork\n' > r1-harness-receipt.txt
$ cat r1-harness-receipt.txt
round 1 harness in Cuenca fork
```

The child's first invocation used its default shell cwd; it removed the receipt
it had created there and repeated the command with explicit `cd`. Only the
corrected run above is counted. Its T3 app thread binding stayed unchanged:
this proves a real harness command in the fork, not a new T3 workspace launch.
No Paseo session was spawned. The original pre-review T3/Paseo launch evidence
remains in [the original receipt](dogfood-bro-3311.md).

## Real `link/` refusal and no orphan fork

At `2026-10-25T07:09Z`, created `link` pointing to a new empty `refused-target`:

```text
cuenca --json worktree harness-r1 /Volumes/Broomva/scratch/bro-3311/dogfood-r1/link/
{"error":"worktree destination has a symlink component: /Volumes/Broomva/scratch/bro-3311/dogfood-r1/link/"}
exit=1

cuenca --json fork demo --name must-not-exist --checkout /Volumes/Broomva/scratch/bro-3311/dogfood-r1/link/
{"error":"worktree destination has a symlink component: /Volumes/Broomva/scratch/bro-3311/dogfood-r1/link/"}
exit=1
```

An exact directory listing showed the target remained empty (no `.git`), and
`cuenca --json forks` contained only `harness-r1`, with no `must-not-exist` fork.
Regression tests additionally cover every reported suffix, relative paths,
symlinked ancestors and a symlink preceding `..`.

## Publish the actual harness file

```text
cuenca --json snapshot --dir /Volumes/Broomva/scratch/bro-3311/dogfood-r1/work
{"changes":[{"change":"added","kind":"file","path":"r1-harness-receipt.txt"}],"flush":{"error":null,"offline":false,"pending":0,"published":[{"fenced":false,"fork":"harness-r1","id":"b485deb767792a0295b3d3550b044443ca1c77e64c12bde013f7e4990d3528e9","target":"cd7ec02bf69c3abe462321960dc312312af4b8bb"}]}}
exit=0
volume/forks/harness-r1/r1-harness-receipt.txt: round 1 harness in Cuenca fork
visible in 1.293s from snapshot start
```

The read loop checked exact bytes under a 15s deadline. This exercises the local
authority materialiser, not Finder/SMB networking or NUC deployment.

## Round 1 responses

- **Major 1:** descriptor-relative `statat(SYMLINK_NOFOLLOW)`/`openat(O_NOFOLLOW)`
  walks all original input components before any canonicalization. The resulting
  normalized physical path is passed to `Local::prepare_root` and used unchanged.
  Destination and ancestors must remain caller-exclusive during the Git operation.
- **Major 2(a):** cache each distinct HTTP blob on disk during materialisation;
  duplicate paths and Git ingestion reuse it. A real HTTP relay verifies one
  download per distinct blob, instead of the reproduced counts of 2 and 3.
- **Major 2(b):** run the same `look` scan as plain checkout and save `cache.json`.
  The scanner's normal racy-mtime rule remains; files too new to cache are rehashed.
- **Major 2(c):** report `clean` and `ignored`, including the normal text notice;
  compare against plain checkout for tracked default-ignored content.
- **Major 2(d):** run `git update-index --refresh` after moving files; assert
  `git diff-files` is empty before any `git status` can hide stale stat entries.
- **Minor 1:** prevalidate `fork --checkout` before authority fork creation;
  tests and dogfood show invalid destinations create no authority fork.
- **Minor 2:** precheck empty-directory writability (mode and actual write probe)
  before Git can remove it on a failed add; test preserves a 0555 directory.
- **Minor 3:** docs explicitly identify both commands as CLI-only host bootstrap
  operations, absent from MCP tools/options.
- **Minor 4:** the private Git tree bridge opts into isolated config, including
  its partial-clone probe; a poisoned global config is now harmless. Other bridge
  callers retain their existing configuration behavior.
- **Minor 5:** choose explicit retention and manual reclaim, not silent deletion
  of optional local-only commits. ADR 0001 records why; `docs/harnesses.md` gives
  exact-store inspection, prune/dry-run, and interactive removal commands, with
  live worktree and queued-snapshot restrictions.


## Acceptance after fixes

The full CI workflow command set ran from `2026-10-25T07:04Z` through
`2026-10-25T07:08Z`, with all exit codes 0. Targets were on the SSD under
`/Volumes/Broomva/scratch/bro-3311`; temporary paths used the short physical SSD
path `/Volumes/Broomva/scratch/b3311-tmp`. Actual command/exit receipts:

```text
COMMAND cargo +1.99.0 fmt --all -- --check
EXIT 0
COMMAND cargo +1.99.0 clippy --workspace --all-targets --locked -- -D warnings
EXIT 0
COMMAND cargo +1.99.0 test --workspace --locked
EXIT 0
COMMAND cargo +1.93.0 check --workspace --locked --target-dir /Volumes/Broomva/scratch/bro-3311/target-msrv
EXIT 0
COMMAND cargo audit
EXIT 0
COMMAND bash -n deploy/install.sh deploy/check.sh deploy/test-container.sh deploy/test-systemd.sh deploy/wait-tailnet deploy/tailnet-addr.sh
EXIT 0
COMMAND bash deploy/test-container.sh
EXIT 0
COMMAND bash -e -c test\ -s\ docs/harnesses.md\ \&\&\ cargo\ test\ -p\ cuenca-cli\ worktree
EXIT 0
COMMAND bstack test-lock verify --base origin/main --worktree
EXIT 0
```

Workspace tests were all green. The exact node done command ran the original
3 worktree integration tests plus the 6 locked round 1 regressions: 9 passed,
0 failed. The Ubuntu container printed `test-container: all passed`. Test-lock
verification reported one clean active lock, with no locked-file changes.

## Mutation proof

At `2026-10-25T07:09Z`, checkpointed and pushed before running:

```sh
CARGO_TARGET_DIR=/Volumes/Broomva/scratch/bro-3311/target \
CUENCA_MUT_TMPDIR=/Volumes/Broomva/scratch/b3311-mut \
bash cuenca-cli/scripts/mutation-check.sh --worktree
```

Completed by `2026-10-25T07:11Z`, exit 0. All 13 current mutants built and
failed tests against a green full-CLI baseline. Actual output:

```text
baseline  green
killed    src/worktree.rs: let root = prepare(dest)?;
killed    src/worktree.rs: std::fs::read_dir(&root)?.next().is_none()
killed    src/worktree.rs: Local::prepare_root(root)
killed    src/worktree.rs: if key.to_string_lossy().starts_with("GIT_") {
killed    src/worktree.rs:             "--no-checkout".as_ref(),
killed    src/worktree.rs:     git(&root, &["read-tree".as_ref(), commit.as_ref()], b"")?;
killed    src/worktree.rs: if let Some(found) = source.fetch(oid)? {
killed    src/worktree.rs: let object = objects.join(&hex[..2]).join(&hex[2..]);
killed    src/worktree.rs:         .with_isolated_config()
killed    src/worktree.rs:     git(&root, &["update-index".as_ref(), "--refresh".as_ref()], b"")?;
killed    src/worktree.rs:     local.save_cache(&cache)?;
killed    src/worktree.rs: "clean": seen.changes.is_empty(), "ignored": seen.ignored
killed    src/main.rs:                 cuenca_cli::worktree::prepare(dir)?;
```

The sweep restores source and Cargo.lock; both were checked against Git after
completion. The component-walk bypass mutant is caught by the same locked test
that reproduced `link/`, `link/.`, `link//`, symlinked ancestors and relative forms.
This builder did not merge. Coordinator P20 on the new head remains required.
