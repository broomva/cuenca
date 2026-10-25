# Local dogfood evidence boundary

A disposable SSD authority can exercise fork, checkout, status, snapshot,
diff, log, verify and land through `cuenca mcp`. The test suite's loopback
identity fixture is useful for this local workflow; it does not establish
real Tailscale authentication or the production permission boundary.

A location without a Git remote records a landing request and waits for
the owner (`cuenca land --wait 0` reports `waiting_for: owner`). A GitHub
location exports a branch and opens a PR through the existing lander.
Neither an open PR nor advisory verify is evidence of a gated merge.

Keep raw JSON-RPC receipts and the candidate tree id. After an authorized
merge, compare that id with the merged Git tree and retain required-check
results. Production Finder/Files and load acceptance wait on BRO-3461.
