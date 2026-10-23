//! The checks a location declares, and the record of a run of them (spec rev 2.2
//! sections 4 and 6: "a change never judges itself").
//!
//! A location's checks are listed in `.cuenca/checks.toml` ([`MANIFEST_PATH`]) of its
//! **base** tree, the main a change would land onto, never in the candidate's tree:
//!
//! ```toml
//! [[check]]
//! name = "test"
//! argv = ["cargo", "test", "--locked"]
//! timeout_secs = 900   # optional, default 1800
//! ```
//!
//! A base tree with no manifest has no checks. `cuenca verify` runs the base's list on
//! the caller's machine against the candidate and the authority records the result
//! ([`crate::Event::VerifyRecorded`]) beside the changeset. In phase 2 that record is
//! advisory: the caller reports its own results, and a check reads the candidate's
//! files, so a candidate that rewrites a script a check calls can make it pass. What
//! the authority does hold is that the list run is the base's: it reads the manifest
//! from the base tree itself, and refuses a record whose checks are not exactly that
//! list.

use cuenca_tree::{ContentHash, GitOid};
use serde::{Deserialize, Serialize};

/// Where the manifest sits in a tree.
pub const MANIFEST_PATH: &str = ".cuenca/checks.toml";
/// Largest manifest, in bytes.
pub const MAX_MANIFEST_BYTES: usize = 64 * 1024;
/// Most checks in one manifest.
pub const MAX_CHECKS: usize = 64;
/// Most arguments in one check's argv, and the longest argument, in bytes.
pub const MAX_ARGS: usize = 256;
pub const MAX_ARG_BYTES: usize = 4096;
/// Longest check name, in bytes.
pub const MAX_CHECK_NAME_BYTES: usize = 64;
/// A check's time limit when the manifest gives none, and the largest it may give.
pub const DEFAULT_TIMEOUT_SECS: u64 = 1800;
pub const MAX_TIMEOUT_SECS: u64 = 24 * 3600;

/// Why a manifest was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{MANIFEST_PATH}: {0}")]
pub struct ManifestError(pub String);

/// One declared check: a name and the argv to run, from the checkout's root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub name: String,
    pub argv: Vec<String>,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_SECS
}

/// A parsed manifest: its checks, in the order declared.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Manifest {
    pub checks: Vec<Check>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    check: Vec<Check>,
}

impl Manifest {
    /// Parse a manifest's bytes. Anything not exactly the documented form is refused:
    /// unknown keys, a name used twice, an empty argv, values over the limits.
    pub fn parse(bytes: &[u8]) -> Result<Self, ManifestError> {
        let bad = |m: String| Err(ManifestError(m));
        if bytes.len() > MAX_MANIFEST_BYTES {
            return bad(format!(
                "{} bytes is over the {MAX_MANIFEST_BYTES}-byte limit",
                bytes.len()
            ));
        }
        let text =
            std::str::from_utf8(bytes).map_err(|_| ManifestError("is not UTF-8".to_string()))?;
        let file: File = toml::from_str(text).map_err(|e| ManifestError(e.message().into()))?;
        if file.check.len() > MAX_CHECKS {
            return bad(format!("more than {MAX_CHECKS} checks"));
        }
        let mut names = std::collections::HashSet::new();
        for c in &file.check {
            let name_ok = !c.name.is_empty()
                && c.name.len() <= MAX_CHECK_NAME_BYTES
                && c.name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
            if !name_ok {
                return bad(format!(
                    "check name {:?} is not 1 to {MAX_CHECK_NAME_BYTES} of ASCII letters, digits, `.`, `_`, `-`",
                    c.name
                ));
            }
            if !names.insert(c.name.as_str()) {
                return bad(format!("check {:?} is declared twice", c.name));
            }
            if c.argv.is_empty() || c.argv[0].is_empty() {
                return bad(format!("check {:?} has no command", c.name));
            }
            if c.argv.len() > MAX_ARGS {
                return bad(format!("check {:?} has over {MAX_ARGS} arguments", c.name));
            }
            if c.argv
                .iter()
                .any(|a| a.len() > MAX_ARG_BYTES || a.contains('\0'))
            {
                return bad(format!(
                    "check {:?} has an argument over {MAX_ARG_BYTES} bytes or with a NUL",
                    c.name
                ));
            }
            if c.timeout_secs == 0 || c.timeout_secs > MAX_TIMEOUT_SECS {
                return bad(format!(
                    "check {:?} has timeout_secs {}, not 1 to {MAX_TIMEOUT_SECS}",
                    c.name, c.timeout_secs
                ));
            }
        }
        Ok(Self { checks: file.check })
    }
}

/// The manifest a verify record was judged by: its blob in the base tree, by git id
/// and by the SHA-256 the authority derived from the verified bytes at ingest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestId {
    pub oid: GitOid,
    pub sha256: ContentHash,
}

/// How one check ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    /// Exited 0.
    Passed,
    /// Exited non-zero, or was ended by a signal.
    Failed,
    /// Ran past its `timeout_secs` and was killed.
    TimedOut,
    /// Could not be started (no such command, not executable).
    NotRun,
}

/// One check as it ran: the declared check, and how it ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckRun {
    #[serde(flatten)]
    pub check: Check,
    pub status: CheckStatus,
    /// The exit code, when it exited (not on a signal, a timeout, or `not_run`).
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<Manifest, ManifestError> {
        Manifest::parse(s.as_bytes())
    }

    #[test]
    fn parses_the_documented_form() {
        let m = parse(
            "[[check]]\nname = \"test\"\nargv = [\"cargo\", \"test\"]\n\n\
             [[check]]\nname = \"lint\"\nargv = [\"make\", \"lint\"]\ntimeout_secs = 60\n",
        )
        .unwrap();
        assert_eq!(m.checks.len(), 2);
        assert_eq!(m.checks[0].name, "test");
        assert_eq!(m.checks[0].timeout_secs, DEFAULT_TIMEOUT_SECS);
        assert_eq!(m.checks[1].argv, ["make", "lint"]);
        assert_eq!(m.checks[1].timeout_secs, 60);
        assert_eq!(parse("").unwrap(), Manifest::default());
    }

    #[test]
    fn refuses_anything_else() {
        for bad in [
            "[[check]]\nname = \"t\"\nargv = [\"true\"]\nnetwork = true\n",
            "[[checks]]\nname = \"t\"\nargv = [\"true\"]\n",
            "[[check]]\nname = \"t\"\nargv = [\"true\"]\n[[check]]\nname = \"t\"\nargv = [\"x\"]\n",
            "[[check]]\nname = \"t\"\nargv = []\n",
            "[[check]]\nname = \"t\"\nargv = [\"\"]\n",
            "[[check]]\nname = \"\"\nargv = [\"true\"]\n",
            "[[check]]\nname = \"a b\"\nargv = [\"true\"]\n",
            "[[check]]\nname = \"t\"\nargv = [\"true\"]\ntimeout_secs = 0\n",
            "[[check]]\nname = \"t\"\nargv = [\"true\"]\ntimeout_secs = 999999\n",
            "[[check]]\nname = \"t\"\nargv = [\"a\\u0000b\"]\n",
            "[[check]]\nname = \"t\"\n",
            "[[check]]\nargv = [\"true\"]\n",
            "[[check]\n",
            "check = 1\n",
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
        assert!(Manifest::parse(b"\xff").is_err());
        let big = format!("# {}\n", "x".repeat(MAX_MANIFEST_BYTES));
        assert!(parse(&big).is_err());
        let many: String = (0..=MAX_CHECKS)
            .map(|i| format!("[[check]]\nname = \"c{i}\"\nargv = [\"true\"]\n"))
            .collect();
        assert!(parse(&many).is_err());
    }

    #[test]
    fn a_run_serializes_flat() {
        let run = CheckRun {
            check: Check {
                name: "t".into(),
                argv: vec!["true".into()],
                timeout_secs: 5,
            },
            status: CheckStatus::TimedOut,
            exit_code: None,
            duration_ms: 7,
        };
        let v = serde_json::to_value(&run).unwrap();
        assert_eq!(v["name"], "t");
        assert_eq!(v["status"], "timed_out");
        assert_eq!(serde_json::from_value::<CheckRun>(v).unwrap(), run);
    }
}
