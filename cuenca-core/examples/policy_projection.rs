//! What the gate engine reads from each policy file named on the command line, one JSON
//! line per file, for the differential test against p9 (`scripts/policy-differential.py`).
//!
//! A line is `{"file", "ok": false, "error"}` when the policy does not compile, else
//! `{"file", "ok": true, "protected", "governance", "secret_paths"}`: the
//! `require_human` paths, the governance tier (`null` or its paths, which of
//! [`SAMPLE_PATHS`] they match, and the window in seconds) and the secret-path patterns
//! the policy adds to the built-in one.

use cuenca_core::gates::Policy;
use serde_json::json;

const SAMPLE_PATHS: &[&str] = &[
    "CLAUDE.md",
    "claude.md",
    "x/CLAUDE.md",
    "AGENTS.md",
    "METALAYER.md",
    ".control/policy.yaml",
    ".control/rcs-parameters.toml",
    "docs/charter.md",
    "docs/ADR-001.md",
    "docs/a.md",
    "docs/ab.md",
    "0",
    "-",
    "a",
    "b",
    "z",
    "[",
    "]",
];

fn main() {
    let builtin = Policy::default().secret_paths.len();
    for file in std::env::args().skip(1) {
        let bytes = std::fs::read(&file).unwrap_or_else(|e| panic!("{file}: {e}"));
        let line = match Policy::parse(&bytes) {
            Err(error) => json!({ "file": file, "ok": false, "error": error }),
            Ok(p) => json!({
                "file": file,
                "ok": true,
                "protected": p.protected,
                "governance": p.governance.map(|g| json!({
                    "paths": g.paths,
                    "matches": SAMPLE_PATHS
                        .iter()
                        .copied()
                        .filter(|s| g.matches(s))
                        .collect::<Vec<_>>(),
                    "window_seconds": g.window_ms / 1000,
                })),
                "secret_paths": p.secret_paths[builtin..]
                    .iter()
                    .map(|r| r.as_str())
                    .collect::<Vec<_>>(),
            }),
        };
        println!("{line}");
    }
}
