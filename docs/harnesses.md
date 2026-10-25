# Agent harnesses in Cuenca forks

Cuenca owns the fork and its publication. A harness works in a local directory;
Maestro decides where and which harness runs (BRO-3315). Agents use the CLI or
stdio MCP, while Finder shows the authority volume over stock Samba. Each agent
gets its own fork and directory. See [ADR 0001](adr/0001-harness-worktrees.md).

## Obtain a fork

Install `cuenca` as in the [README](../README.md). Have the owner register a location
and mint a scoped agent key, then log in without putting the key in shell history:

```sh
cuenca login --url http://AUTHORITY_TAILNET_IP:7419 --token-stdin < /path/to/private-key-file
mkdir -p "$HOME/cuenca-forks"
cuenca --json fork my-location --name task-123 --checkout "$HOME/cuenca-forks/task-123"
```

Or materialise an existing fork, including a fork of another fork:

```sh
cuenca fork --from owner-work --name task-124
cuenca --json worktree task-124 "$HOME/cuenca-forks/task-124"
```

`worktree FORK DIR` and `fork --checkout DIR` produce real detached Git worktrees.
Their synthetic commit/index matches the full Cuenca fork, including content
`.gitignore` excludes. The destination must be missing or empty, with an existing
parent; symlink paths, existing Cuenca checkouts and nested Cuenca checkouts are
refused. A failed `fork --checkout` keeps the new fork: retry with `worktree`.

The JSON result gives `root`, `fork`, `head` (Cuenca tree), `commit` (synthetic Git
commit), and `git_dir` (private backing store). It has no Git remote or upstream
history. Set `CUENCA_STATE_HOME` before creation if its backing store must live on
an SSD. Keep that store while the worktree exists. For a harness needing only a
plain directory, `cuenca checkout FORK DIR` remains available.

## Start a harness

These are cwd handoffs, with no app patches and no new worktree request inside the
harness. Run each command on the machine holding the checkout, with `cuenca` on
PATH and access to the same Cuenca config/state. Replace the example task prompt.
Check the installed harness's `--help` when its version differs.

Claude Code:

```sh
cd "$HOME/cuenca-forks/task-123"
claude 'Implement the task. Use cuenca snapshot to publish; leave landing to Cuenca.'
# Optional stdio tools (register on this host):
claude mcp add cuenca -- cuenca mcp
```

Codex:

```sh
codex -C "$HOME/cuenca-forks/task-123" 'Implement the task in this fork.'
# Bounded noninteractive example:
codex exec -C "$HOME/cuenca-forks/task-123" --sandbox workspace-write 'Implement the task.'
```

Grok:

```sh
cd "$HOME/cuenca-forks/task-123"
grok -p 'Implement the task in this fork.'
```

Gemini through Antigravity (`agy` on hosts using that CLI):

```sh
cd "$HOME/cuenca-forks/task-123"
agy -p 'Implement the task in this fork.'
```

For the standalone Gemini CLI, use the same cwd and `gemini -p 'Implement the task.'`.
These commands use the installed provider configuration; Cuenca selects no model.

T3 Code: register/open the materialised directory as a project, then bind the
session with the supported launch API. Substitute the returned project id and the
absolute `root` from `cuenca --json worktree`. No shell `cd` can change T3's binding.

```json
{
  "title": "Cuenca task-123",
  "projectId": "REGISTERED_FORK_PROJECT_ID",
  "workspaceStrategy": {
    "type": "existing_worktree",
    "worktreePath": "/absolute/path/to/cuenca-forks/task-123"
  },
  "message": "Implement the task in this Cuenca fork; snapshot through cuenca."
}
```

Call `t3_thread_launch` with that object. The worktree is detached, so omit `branch`.
Use the project's scripts (`runOnWorktreeCreate`) for supported setup behavior;
Cuenca does not install packages or replace app setup. Do not ask T3 to make a
second worktree. A session launch test is recorded separately from unit tests.

Paseo (supported CLI cwd handoff, with its existing daemon/provider configuration):

```sh
paseo run --cwd "$HOME/cuenca-forks/task-123" --provider codex \
  --title 'Cuenca task-123' 'Implement the task; publish with cuenca snapshot.'
```

Use `--cwd` with the existing checkout. Paseo's `--new-workspace worktree` creates
another checkout and is unnecessary here. Its workspace setup command is app-owned
(`paseo workspace setup` on versions exposing it); inspect local help before use.

## Publish and finish

```sh
cuenca status --dir "$HOME/cuenca-forks/task-123"
cuenca snapshot --dir "$HOME/cuenca-forks/task-123"
cuenca verify --dir "$HOME/cuenca-forks/task-123"
cuenca land --dir "$HOME/cuenca-forks/task-123"
```

Snapshots scan working files (Git commits/staging are optional), exclude `.git`,
and use the normal durable outbox offline. Verify reads checks from the location's
base; land requests the authority's PR flow. Local Git hooks are not a replacement
for authority checks. Never work through the SMB mount as an agent.

Stop the harness before final snapshot. There is currently no agent `release`
verb/API; snapshot does **not** release ownership. Keep the checkout/backing store
until queued snapshots are acknowledged. Do not remove them while offline work is
queued. Follow the authority's existing owner takeover procedure when custody must
move. Removing a Git worktree does not release a Cuenca fork.

First checkout cost is proportional to all fork files and Git object ingestion;
package installation is separate. See the BRO-3311 dogfood receipt for measured
local timings, actual harness output, and launch/deployment limits.
