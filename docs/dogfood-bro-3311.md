# BRO-3311 harness dogfood — 2026-10-25

Local SSD exercise on macOS, using the built `cuenca` binary from this branch,
Paseo 0.12.0 and T3 Code's live app-owned launch API. Both harnesses ran Codex.
No app code was patched. The authority was a fresh local process bound to
127.0.0.1 with `--unsafe-bind`, a test tailscale identity, and `--volume` on the
SSD. This proves the host-agnostic path; it does not attest production networking,
Finder/SMB, other vendors' actual launches, or the NUC deploy.

## Worktree creation and cost

The location `demo` was a plain directory with a README and bounded fixture
AGENTS.md (2 files, 266 bytes). Its principal had only the demo location grant.
Config and state were outside the working trees; tokens were not printed.

```text
cuenca --json fork demo --name harness-paseo --checkout /Volumes/Broomva/scratch/bro-3311/dogfood/paseo
{"commit":"bcf11f7418bb429d420948cab6cc488f946aec59","epoch":1,"files":2,"fork":"harness-paseo","head":"a6ae16ac25983c83ea50b645893bcf019289edd5","location":"demo","root":"/Volumes/Broomva/scratch/bro-3311/dogfood/paseo",...}
elapsed=0.220s
cuenca --json fork demo --name harness-t3 --checkout /Volumes/Broomva/scratch/bro-3311/dogfood/t3
{"commit":"bcf11f7418bb429d420948cab6cc488f946aec59","epoch":1,"files":2,"fork":"harness-t3","head":"a6ae16ac25983c83ea50b645893bcf019289edd5","location":"demo","root":"/Volumes/Broomva/scratch/bro-3311/dogfood/t3",...}
elapsed=0.224s
```

Each JSON also reported its own private `git_dir` under the fixture's
`state/worktrees/repo-*`. A conventional `git worktree add --detach` of the same
fixture took 0.076s (after an initial Git commit). Dependencies: none; reinstall
cost: 0s. This is a small-fixture comparison, not a claim of production speedup.
Cuenca creation includes HTTP fork creation, materialisation and object hashing.

## T3 launch

Registered the materialised directory with `t3_project_create`, then called:

```json
{
  "title": "BRO-3311 T3 worktree launch receipt",
  "projectId": "mcp:b2f444fc-4275-4348-84d9-68fd4b096d4b",
  "workspaceStrategy": {
    "type": "existing_worktree",
    "worktreePath": "/Volumes/Broomva/scratch/bro-3311/dogfood/t3"
  },
  "message": "Run pwd and git status --short; write t3-receipt.txt and print it."
}
```

The returned thread `mcp:a6c184ca-5d06-419d-94d7-20dc65d70c62` completed with
`branch: null`, the requested bound `worktreePath`, and this actual harness output:

```text
$ pwd
/Volumes/Broomva/scratch/bro-3311/dogfood/t3
$ git status --short

$ cat t3-receipt.txt
t3 launched in Cuenca fork
```

Thus the supported `existing_worktree` strategy accepts the detached fork worktree;
no branch override or patch was needed. The project scripts API exposes
`runOnWorktreeCreate`; no setup script or dependency installation was needed here.

## Paseo launch

```sh
paseo run --cwd /Volumes/Broomva/scratch/bro-3311/dogfood/paseo \
  --provider codex --title 'BRO-3311 Cuenca launch receipt' --wait-timeout 60s --json \
  'Run pwd and git status --short; write paseo-receipt.txt and print it.'
```

Paseo created workspace `wks_d1b127cdd4ae3192` and agent
`3a336d5c-4d84-40aa-86c5-09083c3d63cd` in the requested cwd. Its default permissions
asked for the bounded receipt command; that specific request was allowed through
`paseo permit allow`. Actual `paseo logs` output and the on-disk file agreed:

```text
/Volumes/Broomva/scratch/bro-3311/dogfood/paseo
paseo launched in Cuenca fork
```

Supported extension points inspected before launch: the installed CLI's `run --cwd`
and `--new-workspace worktree` help; the local Paseo source's
`packages/server/src/utils/worktree.ts` reads `paseo.json`'s `worktree.setup` and
`worktree.teardown`, and the CLI has `workspace/setup.ts`. The external cwd handoff
was sufficient. We did not ask Paseo to create a second worktree or run setup hooks.

## Publication and local volume visibility

After both harnesses stopped writing, ran `cuenca --json snapshot --dir ROOT`:

```text
T3:
{"changes":[{"change":"added","kind":"file","path":"t3-receipt.txt"}],"flush":{"error":null,"offline":false,"pending":0,"published":[{"fenced":false,"fork":"harness-t3","id":"dc136bd174356116c51de2c2c88d9be514650214d163af8dea0050dc24a67883","target":"c5f0a8225cbdb7ffae71e32d316bc9b9b267ce7f"}]}}
volume/forks/harness-t3/t3-receipt.txt: t3 launched in Cuenca fork
visible in 1.082s from snapshot start

Paseo:
{"changes":[{"change":"added","kind":"file","path":"paseo-receipt.txt"}],"flush":{"error":null,"offline":false,"pending":0,"published":[{"fenced":false,"fork":"harness-paseo","id":"bc3f3e8d07a4cef3b7ba53266ecaddfbf3bdc7245ff4098e1d165fb2d96886f1","target":"47a88acb649ab5ebef0fe50bf39fb80a68c2ace5"}]}}
volume/forks/harness-paseo/paseo-receipt.txt: paseo launched in Cuenca fork
visible in 1.225s from snapshot start
```

The read loop checked exact bytes on disk with a 15s deadline. `.git` did not appear
in either published change list. This is the actual materialiser path stock Samba
would serve; Finder transport was not exercised.

## Explicit remaining contract

The historical BRO-3311 release-verb acceptance is tracked by
[BRO-3486](https://linear.app/broomva/issue/BRO-3486): a fenced agent release after
acknowledged final snapshot needs a core/server API, which does not exist in merged
main. This PR supplies worktree materialisation, harness handoffs and publication;
it makes no ownership-release claim. P20 remains the coordinator's responsibility.
