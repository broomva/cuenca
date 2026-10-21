//! The gate engine (spec section 4), log-only: every gate runs on a landing request,
//! its result is journaled with the request, and nothing is refused because of it.
//! Gates become blocking in phase 3 (BRO-3307).
//!
//! Gates are implementations of three traits, after Mononoke's hook design (its code is
//! GPL-2.0, so none of it is used): [`FileGate`] sees one path a changeset touches,
//! [`ChangesetGate`] the whole changeset, and [`RefGate`] the move of the location's
//! main the landing would make. [`Engine::compile`] builds them from the policy file
//! of the **accepted base**, the main the change lands onto, never from the candidate's
//! own tree: a change that edits the policy is judged by the old one. The evaluation
//! records the SHA-256 of that policy file and of the base's check manifest
//! ([`CHECKS_PATH`]) next to the results.
//!
//! The change judged is the fork's head changeset, which is exactly what landing moves
//! main by: main moves only by a changeset that starts from main
//! ([`crate::Store::advance_main`]). A head on top of earlier drafts does not start from
//! main, fails `fast_forward`, and cannot land as it is; earlier drafts are judged when
//! they are squashed into a head that can.
//!
//! An evaluation is a pure function of its [`Input`]: the store
//! ([`crate::Store::evaluate_landing`]) assembles the input from one consistent
//! snapshot, so a dry run and a landing request against the same log state return the
//! same results.
//!
//! What an evaluation costs is bounded by what the caller already sent: one read of
//! each distinct blob the changeset puts (whatever number of paths name it), at most
//! [`policy::MAX_POLICY_PATTERNS`] patterns over each new blob and each path, the
//! declarations ([`MAX_DECLARATIONS`]), and one scan of the log for main's history.
//! Rate, concurrency and deadlines are the authority's availability layer, which
//! this node does not defend (BRO-3312), as `cuenca-server`'s crate docs state.
//!
//! Findings never carry content: a secret finding names the path and the pattern, not
//! the bytes it matched, because the evaluation is readable by everyone who reads the
//! fork's history.

mod builtin;
mod policy;

use std::collections::{BTreeMap, BTreeSet};

use cuenca_tree::{Changeset, ContentHash, Entry, GitOid, Op, RepoPath, Tree};
use serde::{Deserialize, Serialize};

pub use policy::{CHECKS_PATH, MAX_POLICY_BYTES, POLICY_PATH, Policy};

/// A landing request may declare at most this many paths, deletes and reverts
/// together; each declaration is at most [`cuenca_tree::MAX_PATH_BYTES`] long. They
/// are journaled with the request, so they are bounded like everything else the log
/// keeps.
pub const MAX_DECLARATIONS: usize = 4096;

/// At most this many findings are kept per gate; the rest are counted in
/// [`GateResult::more`], so one changeset cannot grow the log without bound.
pub const MAX_FINDINGS_PER_GATE: usize = 50;

/// Who asked for the landing, as the authority established it. Gates may treat them
/// differently: a protected path needs a human.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorKind {
    Agent,
    Human,
    /// A service principal (Maestro). No key yields one yet: it arrives with the shared
    /// identity crate (BRO-3317).
    Service,
}

/// Paths the author declares it deletes, or puts back to content main held before, on
/// purpose (the Deletion Gate's `Deletes:` and `Reverts:` lines). An entry ending in
/// `/` covers every path under it; any other entry covers exactly that path.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Declarations {
    #[serde(default)]
    pub deletes: Vec<String>,
    #[serde(default)]
    pub reverts: Vec<String>,
}

impl Declarations {
    /// Refuse declarations the log should not keep: too many, or one too long.
    pub fn check(&self) -> std::result::Result<(), String> {
        let count = self.deletes.len() + self.reverts.len();
        if count > MAX_DECLARATIONS {
            return Err(format!(
                "{count} declarations, over the limit of {MAX_DECLARATIONS}"
            ));
        }
        match self
            .deletes
            .iter()
            .chain(&self.reverts)
            .find(|d| d.len() > cuenca_tree::MAX_PATH_BYTES)
        {
            Some(d) => Err(format!(
                "a declaration of {} bytes, over the {}-byte path limit",
                d.len(),
                cuenca_tree::MAX_PATH_BYTES
            )),
            None => Ok(()),
        }
    }
}

/// What a landing request brings besides the changeset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LandingRequest {
    pub author: AuthorKind,
    #[serde(default)]
    pub declarations: Declarations,
}

impl LandingRequest {
    pub fn new(author: AuthorKind) -> Self {
        Self {
            author,
            declarations: Declarations::default(),
        }
    }
}

/// Which trait a gate implements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateKind {
    /// The base's own files: a policy or check manifest that cannot be read or
    /// compiled.
    Policy,
    File,
    Changeset,
    Ref,
}

/// One reason a gate failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// The path it is about, when it is about one.
    pub path: Option<String>,
    pub reason: String,
}

/// One gate's result. It passed when it has no findings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateResult {
    pub gate: String,
    pub kind: GateKind,
    pub findings: Vec<Finding>,
    /// Findings beyond [`MAX_FINDINGS_PER_GATE`], counted and not kept.
    #[serde(default)]
    pub more: u64,
}

impl GateResult {
    pub fn passed(&self) -> bool {
        self.findings.is_empty()
    }
}

/// A file the evaluation read from the base: its path, and the SHA-256 of its bytes
/// (`None` when the base has no regular file there).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaseFile {
    pub path: String,
    pub sha256: Option<ContentHash>,
}

/// How the gates' results are used. Only `LogOnly` exists before phase 3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateMode {
    LogOnly,
}

/// The gates' verdict on one landing request, journaled with it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evaluation {
    pub mode: GateMode,
    /// The main tree the change was judged against: the accepted base.
    pub base: GitOid,
    /// The log's length when the base was read; the evaluation reflects every record
    /// before it.
    pub log_len: u64,
    /// The clock the time-dependent gates read, in milliseconds since the Unix epoch.
    pub at_ms: u64,
    pub author: AuthorKind,
    pub declarations: Declarations,
    /// The base's policy file; the gates were compiled from it.
    pub policy: BaseFile,
    /// The base's check manifest, which verify (BRO-3254) reads.
    pub checks: BaseFile,
    pub results: Vec<GateResult>,
    /// Every gate passed. Log-only: a `false` here refuses nothing.
    pub passed: bool,
}

impl Evaluation {
    /// The same verdict, regardless of when it was computed: what a dry run and a
    /// landing request against the same log state must agree on.
    pub fn verdict(&self) -> (GitOid, &BaseFile, &BaseFile, &[GateResult], bool) {
        (
            self.base,
            &self.policy,
            &self.checks,
            &self.results,
            self.passed,
        )
    }
}

/// Main's history as the gates read it.
#[derive(Debug, Clone, Default)]
pub struct History {
    /// Every `(path, blob)` main has held at a path since the location was added.
    pub held: BTreeSet<(RepoPath, GitOid)>,
    /// Each move of main, oldest first: when it happened and the paths it touched.
    pub moves: Vec<(u64, Vec<RepoPath>)>,
}

/// Everything an evaluation reads. Assembled by the store from one snapshot.
pub struct Input<'a> {
    /// The base main's tree.
    pub base: &'a Tree,
    pub base_oid: GitOid,
    pub log_len: u64,
    pub at_ms: u64,
    /// The base's check manifest bytes: `Ok(None)` when absent, `Err` when something
    /// is there that is not a readable regular file (a failing `checks` result).
    pub checks: std::result::Result<Option<&'a [u8]>, String>,
    pub changeset: &'a Changeset,
    /// The tree the changeset starts from (its own base, which may not be main).
    pub changeset_base: &'a Tree,
    /// Reads a blob a put names. Called once per put, so a large changeset is never
    /// held in memory whole.
    pub read: &'a dyn Fn(GitOid) -> std::result::Result<Vec<u8>, String>,
    pub history: &'a History,
    pub request: &'a LandingRequest,
}

/// One path a changeset touches. `after` is `None` for a delete.
pub struct FileChange<'a> {
    pub path: &'a RepoPath,
    pub before: Option<&'a Entry>,
    pub after: Option<(&'a Entry, &'a [u8])>,
}

/// What every gate sees besides its subject.
pub struct Context<'a> {
    pub author: AuthorKind,
    pub at_ms: u64,
    pub declarations: &'a Declarations,
    pub base: &'a Tree,
    pub history: &'a History,
}

/// The move of main a landing would make.
pub struct RefMove<'a> {
    /// Main now.
    pub main: GitOid,
    /// The changeset's own base: a fast-forward only when it equals `main`.
    pub from: GitOid,
    pub to: GitOid,
    pub changeset: &'a Changeset,
}

/// A gate on one path. Returns its findings' reasons (empty: passed).
pub trait FileGate: Send + Sync {
    fn name(&self) -> &'static str;
    fn check(&self, cx: &Context<'_>, file: &FileChange<'_>) -> Vec<String>;
    /// True when the result depends only on the content and on whether the put
    /// changes it, not on the path: the engine then checks each blob once, however
    /// many paths name it.
    fn by_content(&self) -> bool {
        false
    }
}

/// A gate on a whole changeset.
pub trait ChangesetGate: Send + Sync {
    fn name(&self) -> &'static str;
    fn check(&self, cx: &Context<'_>, changeset: &Changeset) -> Vec<Finding>;
}

/// A gate on the move of main.
pub trait RefGate: Send + Sync {
    fn name(&self) -> &'static str;
    fn check(&self, cx: &Context<'_>, change: &RefMove<'_>) -> Vec<Finding>;
}

/// The gates compiled from one base policy.
pub struct Engine {
    policy: BaseFile,
    /// Why the base policy could not be used, when it could not (the built-in gates
    /// run instead, and the evaluation fails on a `policy` result).
    policy_error: Option<String>,
    file: Vec<Box<dyn FileGate>>,
    changeset: Vec<Box<dyn ChangesetGate>>,
    reference: Vec<Box<dyn RefGate>>,
}

impl Engine {
    /// Compile the gates from the base's policy file: `Ok(None)` when the base has
    /// none (the built-in defaults apply), `Err` when it has one that cannot be read as
    /// a file (the defaults apply, and every evaluation fails on `policy`).
    pub fn compile(policy_bytes: std::result::Result<Option<&[u8]>, String>) -> Self {
        let (policy_file, parsed) = match policy_bytes {
            Ok(bytes) => (base_file(POLICY_PATH, bytes), bytes.map(Policy::parse)),
            Err(e) => (base_file(POLICY_PATH, None), Some(Err(e))),
        };
        let (policy, policy_error) = match parsed {
            None => (Policy::default(), None),
            Some(Ok(p)) => (p, None),
            Some(Err(e)) => (Policy::default(), Some(e)),
        };
        let (file, changeset, reference) = builtin::gates(&policy);
        Self {
            policy: policy_file,
            policy_error,
            file,
            changeset,
            reference,
        }
    }

    /// Run every gate on the input.
    pub fn evaluate(&self, input: &Input<'_>) -> Evaluation {
        let cx = Context {
            author: input.request.author,
            at_ms: input.at_ms,
            declarations: &input.request.declarations,
            base: input.base,
            history: input.history,
        };
        let mut results = Vec::new();
        if let Some(error) = &self.policy_error {
            results.push(collect(
                "policy",
                GateKind::Policy,
                vec![Finding {
                    path: Some(POLICY_PATH.to_string()),
                    reason: error.clone(),
                }],
            ));
        }
        if let Err(error) = &input.checks {
            results.push(collect(
                "checks",
                GateKind::Policy,
                vec![Finding {
                    path: Some(CHECKS_PATH.to_string()),
                    reason: error.clone(),
                }],
            ));
        }
        // Puts are grouped by blob, so each blob is read once and the content gates
        // scan it at most twice (as new content, and as unchanged content), however
        // many paths name it. Deletes need no content.
        let mut file_findings: Vec<Vec<Finding>> = self.file.iter().map(|_| Vec::new()).collect();
        let mut unread = Vec::new();
        let mut by_blob: BTreeMap<GitOid, Vec<(&RepoPath, &Entry)>> = BTreeMap::new();
        for op in &input.changeset.ops {
            match op {
                Op::Put { path, entry } => {
                    by_blob.entry(entry.oid()).or_default().push((path, entry))
                }
                _ => {
                    let path = op.path();
                    let change = FileChange {
                        path,
                        before: input.changeset_base.get(path.as_str()),
                        after: None,
                    };
                    self.check_file(&cx, &change, &mut file_findings, &mut Default::default());
                }
            }
        }
        for (oid, puts) in by_blob {
            let content = match (input.read)(oid) {
                Ok(bytes) => bytes,
                Err(e) => {
                    unread.extend(puts.iter().map(|(path, _)| Finding {
                        path: Some(path.to_string()),
                        reason: format!("the content could not be read: {e}"),
                    }));
                    continue;
                }
            };
            // One cache per gate, per "the put changes the content" or not.
            let mut cache = Default::default();
            for (path, entry) in puts {
                let change = FileChange {
                    path,
                    before: input.changeset_base.get(path.as_str()),
                    after: Some((entry, content.as_slice())),
                };
                self.check_file(&cx, &change, &mut file_findings, &mut cache);
            }
        }
        // Back in path order, as the changeset lists them.
        for findings in &mut file_findings {
            findings.sort_by(|a, b| a.path.cmp(&b.path));
        }
        unread.sort_by(|a, b| a.path.cmp(&b.path));
        if !unread.is_empty() {
            results.push(collect("content", GateKind::File, unread));
        }
        for (gate, findings) in self.file.iter().zip(file_findings) {
            results.push(collect(gate.name(), GateKind::File, findings));
        }
        for gate in &self.changeset {
            let findings = gate.check(&cx, input.changeset);
            results.push(collect(gate.name(), GateKind::Changeset, findings));
        }
        let change = RefMove {
            main: input.base_oid,
            from: input.changeset.base,
            to: input.changeset.target,
            changeset: input.changeset,
        };
        for gate in &self.reference {
            let findings = gate.check(&cx, &change);
            results.push(collect(gate.name(), GateKind::Ref, findings));
        }
        let passed = results.iter().all(GateResult::passed);
        Evaluation {
            mode: GateMode::LogOnly,
            base: input.base_oid,
            log_len: input.log_len,
            at_ms: input.at_ms,
            author: input.request.author,
            declarations: input.request.declarations.clone(),
            policy: self.policy.clone(),
            checks: base_file(CHECKS_PATH, input.checks.clone().unwrap_or(None)),
            results,
            passed,
        }
    }
}

impl Engine {
    /// Run every file gate on one path. `cache[changed]` holds, per gate, the reasons
    /// a content-only gate gave for this blob already.
    fn check_file(
        &self,
        cx: &Context<'_>,
        change: &FileChange<'_>,
        findings: &mut [Vec<Finding>],
        cache: &mut [Vec<Option<Vec<String>>>; 2],
    ) {
        let changed = match (change.before, change.after) {
            (Some(before), Some((entry, _))) => usize::from(before.oid() != entry.oid()),
            _ => 1,
        };
        let memo = &mut cache[changed];
        memo.resize(self.file.len(), None);
        for (i, gate) in self.file.iter().enumerate() {
            let reasons = if gate.by_content() && change.after.is_some() {
                memo[i]
                    .get_or_insert_with(|| gate.check(cx, change))
                    .clone()
            } else {
                gate.check(cx, change)
            };
            findings[i].extend(reasons.into_iter().map(|reason| Finding {
                path: Some(change.path.to_string()),
                reason,
            }));
        }
    }
}

fn collect(gate: &str, kind: GateKind, mut findings: Vec<Finding>) -> GateResult {
    let more = findings.len().saturating_sub(MAX_FINDINGS_PER_GATE) as u64;
    findings.truncate(MAX_FINDINGS_PER_GATE);
    GateResult {
        gate: gate.to_string(),
        kind,
        findings,
        more,
    }
}

fn base_file(path: &str, bytes: Option<&[u8]>) -> BaseFile {
    BaseFile {
        path: path.to_string(),
        sha256: bytes.map(ContentHash::of),
    }
}

/// Whether `path` is covered by `declared` ([`Declarations`]): an entry ending in `/`
/// covers the paths under it, any other entry exactly itself. A leading `./` or `/` is
/// ignored. No entry covers the whole tree: `/`, `./` and the empty entry strip to
/// the empty string, which no path equals, and `.` is not a path.
pub fn covered(path: &str, declared: &[String]) -> bool {
    declared.iter().any(|d| {
        let d = d.strip_prefix("./").unwrap_or(d).trim_start_matches('/');
        match d.strip_suffix('/') {
            Some(_) => path.starts_with(d),
            None => path == d,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_declaration_covers_its_path_or_what_is_under_it_and_never_the_whole_tree() {
        let d = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(covered("a/b", &d(&["a/b"])));
        assert!(covered("a/b", &d(&["./a/b"])));
        assert!(covered("a/b", &d(&["/a/"])));
        assert!(!covered("a/bc", &d(&["a/b"])));
        assert!(!covered("ab/c", &d(&["a/"])));
        assert!(!covered("a/b", &d(&["a"])));
        for whole in ["/", "", ".", "./", "//"] {
            assert!(!covered("a", &d(&[whole])), "{whole:?}");
        }
    }

    /// Each blob is read once, and a content gate's finding still lands on every path
    /// that names it, in path order.
    #[test]
    fn a_blob_named_by_many_paths_is_read_once() {
        let secret = concat!("AKIA", "IOSFODNN7EXAMPLE").as_bytes().to_vec();
        let plain = b"plain".to_vec();
        let ops: Vec<Op> = ["z", "a", "m"]
            .iter()
            .map(|p| Op::put(RepoPath::new(*p).unwrap(), Entry::file(&secret)))
            .chain([Op::put(RepoPath::new("q").unwrap(), Entry::file(&plain))])
            .collect();
        let base = Tree::new();
        let changeset = Changeset::new(&base, ops).unwrap();
        let reads = std::cell::RefCell::new(Vec::new());
        let read = |oid: GitOid| {
            reads.borrow_mut().push(oid);
            Ok(if oid == GitOid::for_blob(&secret) {
                secret.clone()
            } else {
                plain.clone()
            })
        };
        let history = History::default();
        let request = LandingRequest::new(AuthorKind::Agent);
        let eval = Engine::compile(Ok(None)).evaluate(&Input {
            base: &base,
            base_oid: base.git_tree_id(),
            log_len: 0,
            at_ms: 0,
            checks: Ok(None),
            changeset: &changeset,
            changeset_base: &base,
            read: &read,
            history: &history,
            request: &request,
        });
        assert_eq!(reads.borrow().len(), 2, "one read per distinct blob");
        let content = eval
            .results
            .iter()
            .find(|r| r.gate == "secret_content")
            .unwrap();
        let paths: Vec<_> = content
            .findings
            .iter()
            .map(|f| f.path.clone().unwrap())
            .collect();
        assert_eq!(paths, ["a", "m", "z"]);
    }

    #[test]
    fn findings_beyond_the_cap_are_counted_not_kept() {
        let findings = (0..MAX_FINDINGS_PER_GATE + 7)
            .map(|i| Finding {
                path: Some(i.to_string()),
                reason: "x".into(),
            })
            .collect();
        let r = collect("deletion", GateKind::Changeset, findings);
        assert_eq!(r.findings.len(), MAX_FINDINGS_PER_GATE);
        assert_eq!(r.more, 7);
        assert!(!r.passed());
    }
}
