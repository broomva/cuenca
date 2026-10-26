# Local dogfood evidence boundary

A disposable SSD authority can exercise fork, checkout, status, snapshot,
diff, log, verify and land through `cuenca mcp`. The test suite's loopback
identity fixture is useful for this local workflow; it does not establish
real Tailscale authentication or the production permission boundary.

A location without a Git remote records a landing request and waits for
the owner (`cuenca land --wait 0` reports `waiting_for: owner`). A GitHub
location exports a branch and opens a PR through the existing lander.
Neither an open PR nor advisory verify is evidence of a gated merge.

Keep raw JSON-RPC receipts and the candidate tree id. Check that the exact
GitHub PR head's tree equals the Cuenca candidate, then verify the merge contains
that head's change and retain checks for the merged SHA. Whole-tree equality
with the original candidate holds only when main has not moved; record the export
and merge parents to establish that condition. If main advances, retain the rebased
candidate and merged result rather than assuming the original tree is still equal.
For a normal merge, check head ancestry. For a squash, check GitHub's merged PR
head/merge association and the resulting content; a squash does not preserve the
exported commit as a Git ancestor. The [Mac receipt](dogfood-2026/tree-equality.json)
records identical head/candidate/merged trees on an unmoved base for PR #30.

Production Finder/Files and load acceptance wait on BRO-3461 (NUC host readiness)
and BRO-3257 (live deployment); local preparation does not close those rows.

## Disposable authority cleanup

Stopping or deleting a disposable authority does not remove its remote
`cp/<changeset-id>` branches or GitHub PRs. A later authority can adopt the same
export and PR after checking their identity; a fresh state directory is not a
fresh GitHub namespace. Repeated trials in the Mac receipt adopted PR #30.

After the trial, stop its owned authority process and archive the raw receipts,
changeset id, exact export SHA, branch name and PR URL. Before removing scratch
state, check that no other authority still needs that landing request. Inspect the
recorded PR and remote branch explicitly; keep an active request, or close the
exact discarded trial PR before deleting its branch. A merged PR remains as history.

For this trial the inventory commands are:

```sh
branch=cp/81a6cb1d11cb73d8a9301f90ba5c1ccd40e65b00987f654e28ef9407455fb38b
expected=f340cb61cf44db2442202a1a0254de9f03bcbc26
gh pr view 30 -R broomva/cuenca --json state,headRefName,headRefOid,mergeCommit
git ls-remote --heads https://github.com/broomva/cuenca.git "refs/heads/$branch"
```

Once the trial is retired and the branch still equals the recorded export, remove
only that branch with an expected-SHA lease (from a checkout whose origin is
`broomva/cuenca`):

```sh
git push --force-with-lease="refs/heads/$branch:$expected" origin ":refs/heads/$branch"
```

If the branch is absent, no remote deletion is needed. If its SHA differs, retain
it and inspect the new owner/work. Remove only the recorded trial's local scratch
state after the receipt archive is verified. These are cleanup instructions;
the receipts update did not delete a remote branch or close another PR.
