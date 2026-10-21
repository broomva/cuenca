//! The built-in gates, configured by a compiled [`Policy`].
//!
//! | Gate | Kind | Fails when |
//! |---|---|---|
//! | `secret_path` | file | a put's path matches a secret-file pattern |
//! | `secret_content` | file | a put's new content matches a secret pattern |
//! | `file_size` | file | a put's new content is over the size limit |
//! | `protected_path` | file | an author other than a human touches (puts or deletes) a protected path |
//! | `governance_rate` | changeset | a change to governance paths would exceed the policy's rate |
//! | `deletion` | changeset | a delete is not declared |
//! | `revert` | changeset | a put restores content main held at that path before, undeclared |
//! | `fast_forward` | ref | the changeset does not start from main as it is now |
//! | `landing_window` | ref | the policy has landing windows and the request falls outside them |

use regex::Regex;
use regex::bytes::Regex as BytesRegex;

use super::policy::{Policy, Window, compile_bytes};
use super::{
    AuthorKind, ChangesetGate, Context, FileChange, FileGate, Finding, RefGate, RefMove, covered,
};
use cuenca_tree::{Changeset, Op};

/// Secret patterns every evaluation runs, whatever the policy says. The names go into
/// findings; the matched bytes never do.
pub(super) const SECRET_PATTERNS: &[(&str, &str)] = &[
    (
        "private key",
        r"-----BEGIN (?:RSA |EC |DSA |OPENSSH |PGP |ENCRYPTED )?PRIVATE KEY(?: BLOCK)?-----",
    ),
    ("AWS access key id", r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b"),
    ("GitHub token", r"\bgh[pousr]_[A-Za-z0-9]{36,}"),
    (
        "GitHub fine-grained token",
        r"\bgithub_pat_[A-Za-z0-9_]{60,}",
    ),
    ("Slack token", r"\bxox[abprs]-[A-Za-z0-9-]{10,}"),
    ("Anthropic API key", r"\bsk-ant-[A-Za-z0-9_-]{20,}"),
    (
        "OpenAI API key",
        r"\bsk-(?:proj-|svcacct-)?[A-Za-z0-9_-]{32,}",
    ),
    ("Stripe live key", r"\b[rs]k_live_[A-Za-z0-9]{20,}"),
    ("Google API key", r"\bAIza[0-9A-Za-z_-]{35}"),
    ("Cuenca key", r"\bcuenca_[0-9a-f]{16,}_[0-9a-f]{32,}"),
];

type Gates = (
    Vec<Box<dyn FileGate>>,
    Vec<Box<dyn ChangesetGate>>,
    Vec<Box<dyn RefGate>>,
);

pub(super) fn gates(policy: &Policy) -> Gates {
    let mut content: Vec<(String, BytesRegex)> = SECRET_PATTERNS
        .iter()
        .map(|(name, re)| {
            (
                name.to_string(),
                compile_bytes(re).expect("built-in pattern"),
            )
        })
        .collect();
    content.extend(policy.secret_patterns.iter().cloned());
    let file: Vec<Box<dyn FileGate>> = vec![
        Box::new(SecretPath(policy.secret_paths.clone())),
        Box::new(SecretContent(content)),
        Box::new(FileSize(policy.max_file_bytes)),
        Box::new(ProtectedPath(
            policy.protected.iter().map(|p| p.to_lowercase()).collect(),
        )),
    ];
    let mut changeset: Vec<Box<dyn ChangesetGate>> = Vec::new();
    if let Some(max) = policy.governance_max {
        changeset.push(Box::new(GovernanceRate {
            paths: policy
                .governance_paths
                .iter()
                .map(|g| (g.clone(), glob(g)))
                .collect(),
            max,
            window_ms: policy.governance_window_ms,
        }));
    }
    if policy.declare_deletions {
        changeset.push(Box::new(Deletion));
    }
    if policy.declare_reverts {
        changeset.push(Box::new(Revert));
    }
    let mut reference: Vec<Box<dyn RefGate>> = vec![Box::new(FastForward)];
    if !policy.landing_windows.is_empty() {
        reference.push(Box::new(LandingWindow(policy.landing_windows.clone())));
    }
    (file, changeset, reference)
}

/// Whether a put changes content (not only the mode).
fn new_content<'a>(file: &'a FileChange<'_>) -> Option<&'a [u8]> {
    let (entry, content) = file.after?;
    match file.before {
        Some(before) if before.oid() == entry.oid() => None,
        _ => Some(content),
    }
}

struct SecretPath(Vec<Regex>);

impl FileGate for SecretPath {
    fn name(&self) -> &'static str {
        "secret_path"
    }
    fn check(&self, _: &Context<'_>, file: &FileChange<'_>) -> Vec<String> {
        if file.after.is_none() {
            return vec![];
        }
        self.0
            .iter()
            .filter(|re| re.is_match(file.path.as_str()))
            .map(|re| format!("the path matches the secret-file pattern {:?}", re.as_str()))
            .collect()
    }
}

struct SecretContent(Vec<(String, BytesRegex)>);

impl FileGate for SecretContent {
    fn name(&self) -> &'static str {
        "secret_content"
    }
    fn check(&self, _: &Context<'_>, file: &FileChange<'_>) -> Vec<String> {
        let Some(content) = new_content(file) else {
            return vec![];
        };
        self.0
            .iter()
            .filter(|(_, re)| re.is_match(content))
            .map(|(name, _)| format!("the content matches the {name} pattern"))
            .collect()
    }
}

struct FileSize(u64);

impl FileGate for FileSize {
    fn name(&self) -> &'static str {
        "file_size"
    }
    fn check(&self, _: &Context<'_>, file: &FileChange<'_>) -> Vec<String> {
        match new_content(file) {
            Some(c) if c.len() as u64 > self.0 => {
                vec![format!("{} bytes, over the {}-byte limit", c.len(), self.0)]
            }
            _ => vec![],
        }
    }
}

/// `path_touched` rules, matched as p9 matches its `require_human` rules: a
/// case-folded substring. The path is matched with a leading `/`, so a rule written
/// as `/CLAUDE.md` matches `CLAUDE.md` at the root (and a nested one: folding and
/// substrings only ever add matches).
struct ProtectedPath(Vec<String>);

impl FileGate for ProtectedPath {
    fn name(&self) -> &'static str {
        "protected_path"
    }
    fn check(&self, cx: &Context<'_>, file: &FileChange<'_>) -> Vec<String> {
        if cx.author == AuthorKind::Human {
            return vec![];
        }
        let path = format!("/{}", file.path.as_str().to_lowercase());
        self.0
            .iter()
            .filter(|rule| path.contains(rule.as_str()))
            .map(|rule| format!("protected by the rule {rule:?}: a human must land it"))
            .collect()
    }
}

struct GovernanceRate {
    paths: Vec<(String, Regex)>,
    max: u64,
    window_ms: u64,
}

impl GovernanceRate {
    fn touches<'p>(&self, mut paths: impl Iterator<Item = &'p str>) -> bool {
        paths.any(|p| self.paths.iter().any(|(_, re)| re.is_match(p)))
    }
}

impl ChangesetGate for GovernanceRate {
    fn name(&self) -> &'static str {
        "governance_rate"
    }
    fn check(&self, cx: &Context<'_>, changeset: &Changeset) -> Vec<Finding> {
        if !self.touches(changeset.ops.iter().map(|op| op.path().as_str())) {
            return vec![];
        }
        let since = cx.at_ms.saturating_sub(self.window_ms);
        let recent = cx
            .history
            .moves
            .iter()
            .filter(|(at, paths)| *at >= since && self.touches(paths.iter().map(|p| p.as_str())))
            .count() as u64;
        if recent < self.max {
            return vec![];
        }
        vec![Finding {
            path: None,
            reason: format!(
                "{recent} change(s) to governance paths landed in the last {} s; the policy allows {}",
                self.window_ms / 1000,
                self.max
            ),
        }]
    }
}

struct Deletion;

impl ChangesetGate for Deletion {
    fn name(&self) -> &'static str {
        "deletion"
    }
    fn check(&self, cx: &Context<'_>, changeset: &Changeset) -> Vec<Finding> {
        changeset
            .ops
            .iter()
            .filter_map(|op| match op {
                Op::Delete { path } if !covered(path.as_str(), &cx.declarations.deletes) => {
                    Some(Finding {
                        path: Some(path.to_string()),
                        reason: "deleted without a `deletes` declaration".into(),
                    })
                }
                _ => None,
            })
            .collect()
    }
}

struct Revert;

impl ChangesetGate for Revert {
    fn name(&self) -> &'static str {
        "revert"
    }
    fn check(&self, cx: &Context<'_>, changeset: &Changeset) -> Vec<Finding> {
        changeset
            .ops
            .iter()
            .filter_map(|op| {
                let Op::Put { path, entry } = op else {
                    return None;
                };
                let oid = entry.oid();
                let unchanged = cx.base.get(path.as_str()).is_some_and(|e| e.oid() == oid);
                let held = cx.history.held.contains(&(path.clone(), oid));
                (held && !unchanged && !covered(path.as_str(), &cx.declarations.reverts)).then(
                    || Finding {
                        path: Some(path.to_string()),
                        reason: "puts back content main held here before, without a `reverts` declaration".into(),
                    },
                )
            })
            .collect()
    }
}

struct FastForward;

impl RefGate for FastForward {
    fn name(&self) -> &'static str {
        "fast_forward"
    }
    fn check(&self, _: &Context<'_>, change: &RefMove<'_>) -> Vec<Finding> {
        if change.from == change.main {
            return vec![];
        }
        vec![Finding {
            path: None,
            reason: format!(
                "the changeset starts from tree {}, but main is at {}: not a fast-forward",
                change.from, change.main
            ),
        }]
    }
}

struct LandingWindow(Vec<Window>);

impl RefGate for LandingWindow {
    fn name(&self) -> &'static str {
        "landing_window"
    }
    fn check(&self, cx: &Context<'_>, _: &RefMove<'_>) -> Vec<Finding> {
        if self.0.iter().any(|w| inside(w, cx.at_ms)) {
            return vec![];
        }
        vec![Finding {
            path: None,
            reason: "outside every landing window of the policy".into(),
        }]
    }
}

/// Whether `at_ms` falls inside `w`, in `w`'s time zone.
pub(super) fn inside(w: &Window, at_ms: u64) -> bool {
    let local = (at_ms / 60_000) as i64 + i64::from(w.utc_offset);
    let day = local.div_euclid(1440);
    let minute = local.rem_euclid(1440) as u16;
    // 1970-01-01 was a Thursday: 3 with Monday as 0.
    let weekday = (day + 3).rem_euclid(7) as u8;
    w.days.contains(&weekday) && w.from <= minute && minute < w.to
}

/// A shell glob as Python's `fnmatch` reads it (p9 matches governance paths with it),
/// case-folded: `*` is any run of characters, `/` included; `?` one character;
/// `[...]` a class, `[!...]` its complement; anything else literal.
pub(super) fn glob(pattern: &str) -> Regex {
    let mut re = String::from("(?is)^");
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' => re.push_str(".*"),
            '?' => re.push('.'),
            '[' => {
                // A class runs to the first `]` after its first character.
                let mut j = i + 1;
                if j < chars.len() && chars[j] == '!' {
                    j += 1;
                }
                if j < chars.len() && chars[j] == ']' {
                    j += 1;
                }
                while j < chars.len() && chars[j] != ']' {
                    j += 1;
                }
                if j >= chars.len() {
                    re.push_str("\\[");
                } else {
                    let body: String = chars[i + 1..j].iter().collect();
                    let (neg, body) = match body.strip_prefix('!') {
                        Some(rest) => (true, rest.to_string()),
                        None => (false, body),
                    };
                    re.push('[');
                    if neg {
                        re.push('^');
                    }
                    for c in body.chars() {
                        if c == '-' {
                            re.push('-');
                        } else {
                            re.push_str(&regex::escape(&c.to_string()));
                        }
                    }
                    re.push(']');
                    i = j;
                }
            }
            c => re.push_str(&regex::escape(&c.to_string())),
        }
        i += 1;
    }
    re.push('$');
    Regex::new(&re).unwrap_or_else(|_| Regex::new("$^").expect("never matches"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_read_as_fnmatch_does() {
        let cases: &[(&str, &str, bool)] = &[
            ("CLAUDE.md", "CLAUDE.md", true),
            ("CLAUDE.md", "claude.MD", true),
            ("CLAUDE.md", "x/CLAUDE.md", false),
            ("CLAUDE.md", "CLAUDExmd", false),
            ("*.md", "docs/a.md", true),
            ("docs/?.md", "docs/a.md", true),
            ("docs/?.md", "docs/ab.md", false),
            ("[ab].txt", "b.txt", true),
            ("[!ab].txt", "b.txt", false),
            ("[!ab].txt", "c.txt", true),
            ("[a-c].txt", "B.txt", true),
            ("[", "[", true),
            (".control/policy.yaml", ".control/policyxyaml", false),
        ];
        for (pattern, path, want) in cases {
            assert_eq!(glob(pattern).is_match(path), *want, "{pattern} vs {path}");
        }
    }

    #[test]
    fn windows_read_the_local_clock() {
        // 2026-10-21 (a Wednesday) 14:00 UTC = 09:00 in Bogota (-05:00).
        let at = 1_792_591_200_000;
        let w = |days: Vec<u8>, from, to, utc_offset| Window {
            days,
            from,
            to,
            utc_offset,
        };
        assert!(inside(&w(vec![2], 540, 600, -300), at));
        assert!(!inside(&w(vec![2], 541, 600, -300), at));
        assert!(!inside(&w(vec![2], 480, 540, -300), at));
        assert!(!inside(&w(vec![3], 540, 600, -300), at));
        assert!(inside(&w(vec![2], 840, 900, 0), at));
        // Across midnight UTC the local day is still Tuesday.
        let late = 1_792_548_000_000; // 2026-10-21 02:00 UTC = Tue 21:00 Bogota
        assert!(inside(&w(vec![1], 1260, 1320, -300), late));
        assert!(!inside(&w(vec![2], 1260, 1320, -300), late));
    }

    #[test]
    fn every_built_in_secret_pattern_matches_its_example_and_not_prose() {
        // Split with concat! so no scanner reads this file as holding secrets.
        let examples = [
            concat!("-----BEGIN OPENSSH ", "PRIVATE KEY-----"),
            concat!("AKIA", "IOSFODNN7EXAMPLE"),
            concat!("ghp_", "0123456789abcdefghijABCDEFGHIJ012345"),
            concat!(
                "github_pat_",
                "11ABCDEFG0123456789_abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOP"
            ),
            concat!("xoxb-", "1234567890-abcdefghij"),
            concat!("sk-ant-", "api03-abcdefghijklmnopqrstuv"),
            concat!("sk-proj-", "abcdefghijklmnopqrstuvwxyz0123456789"),
            concat!("sk_live_", "abcdefghijklmnopqrstuvwx"),
            concat!("AIza", "SyA-1234567890abcdefghijklmnopqrstu"),
            concat!(
                "cuenca_0123456789abcdef_",
                "0123456789abcdef0123456789abcdef"
            ),
        ];
        assert_eq!(examples.len(), SECRET_PATTERNS.len());
        for ((name, re), example) in SECRET_PATTERNS.iter().zip(examples) {
            let re = compile_bytes(re).unwrap();
            assert!(re.is_match(example.as_bytes()), "{name}: {example}");
            assert!(
                !re.is_match(b"The key was rotated; see the runbook for the ask-key procedure."),
                "{name} matches prose"
            );
        }
    }
}
