//! Landing (spec section 6): the authority turns a landing request on a location with
//! a git remote into a branch and a GitHub pull request.
//!
//! One [`Lander`] thread per authority drives every open request, one at a time, through
//! the steps the log records ([`cuenca_core::LandingStep`]):
//!
//! 1. **requested → exporting.** Fetch the remote's default branch into the authority's
//!    own bare repository for the location (`<state>/git/<location>.git`), find the
//!    newest first-parent commit whose tree is the main the request's verify judged
//!    ([`cuenca_core::Landing::main`]), write the fork's head tree with
//!    [`cuenca_tree::git::Repo::write_tree`] and commit it onto that parent. A
//!    `cp/<id>` already on the remote that is an export of this changeset onto that
//!    main is taken as this landing's instead. The commit depends on the changeset and its parent alone
//!    (author and committer `Cuenca`, dated when the changeset was first recorded), so
//!    every landing of one changeset builds the same commit and a rebuild after a crash
//!    gives the same id. Record the export intent, naming the commit.
//! 2. **exporting → exported.** Push the commit to `cp/<changeset id>` with an
//!    expected-old value of "absent", so a push cannot move a branch that exists; a
//!    branch already at the commit is a push that happened. Record the export.
//! 3. **exported → reviewing → in review.** Record the review intent, then find the
//!    pull request from that branch (`gh pr list --state all`) or open one
//!    (`gh pr create`), and record it.
//!
//! A restart picks up at the step the log has, so a kill between a call and its record
//! repeats the call, and each call is idempotent against the remote: 0 duplicate
//! commits, branches or pull requests. GitHub's required checks gate the merge; the
//! authority is not a bypass actor.
//!
//! Errors that may pass (the network, GitHub, a full disk) are retried on the next
//! round and shown at `GET /v1/forks/{fork}/landing`; errors that will not (no commit of
//! the remote has the base's tree, the branch is someone else's, the holder was revoked,
//! GitHub refused the push by rule ([`refused_for_good`]), the remote's history fails
//! fsck) are recorded as `LandingFailed`. A missing `git` or `gh` binary is retried:
//! the host can gain it. Once the cause is fixed, landing again takes a new draft
//! with a changed tree: changeset ids are content-addressed, so the same edit from
//! that fork is the failed changeset again. A failing fsck has no owner-side fix
//! yet (BRO-3484).
//!
//! Identity: `gh` with the token already on the authority host (spec section 0, "the
//! shared gh login at first"; the GitHub App is BRO-3305), as git's credential helper
//! for `https://github.com` remotes too. Only GitHub remotes land through a PR.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use cuenca_core::checks::CheckStatus;
use cuenca_core::{
    ChangesetId, CoreError, Event, Export, Fork, ForkName, Landing, LandingStep, Location, Store,
    export_branch,
};
use cuenca_tree::GitOid;
use cuenca_tree::git::{Blobs, REDIRECTING_ENV, Repo};

use crate::authority::Authority;

/// How far back the remote's first-parent history is searched for the base's tree.
const PARENT_SEARCH: usize = 1000;

/// A crash test's hook, called with the name of each point a kill is tested at.
pub type Fault = Arc<dyn Fn(&str) + Send + Sync>;

/// Where the lander works and what it runs.
#[derive(Clone)]
pub struct LandConfig {
    /// The authority's bare repositories, one per location.
    pub repos: PathBuf,
    /// The `gh` binary.
    pub gh: PathBuf,
    /// The `git` binary.
    pub git: PathBuf,
    /// Extra `git -c` settings, before the lander's own (tests point a GitHub URL at
    /// a local repository with `url.<path>.insteadOf`).
    pub git_config: Vec<String>,
    /// How long the lander sleeps when nothing wakes it.
    pub poll: Duration,
    /// The longest one git or gh call may take; past it the call is killed and the
    /// step retried on the next round.
    pub timeout: Duration,
    /// Called at each point a crash test kills the authority: `export_intent`,
    /// `pushed`, `review_intent`, `pr_opened`.
    pub fault: Option<Fault>,
}

impl LandConfig {
    pub fn new(repos: PathBuf, gh: PathBuf) -> Self {
        Self {
            repos,
            gh,
            git: PathBuf::from("git"),
            git_config: Vec::new(),
            poll: Duration::from_secs(30),
            timeout: Duration::from_secs(600),
            fault: None,
        }
    }
}

/// A running lander; dropping it stops the thread after its current step.
pub struct Lander {
    stop: Arc<AtomicBool>,
    authority: Arc<Authority>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Lander {
    /// Start the lander thread over `authority`.
    pub fn spawn(authority: Arc<Authority>, config: LandConfig) -> std::io::Result<Self> {
        std::fs::create_dir_all(&config.repos)?;
        let stop = Arc::new(AtomicBool::new(false));
        let worker = Worker {
            a: authority.clone(),
            config,
        };
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("cuenca-lander".into())
                .spawn(move || worker.run(&stop))?
        };
        Ok(Self {
            stop,
            authority,
            thread: Some(thread),
        })
    }
}

impl Drop for Lander {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.authority.wake_lander();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Why a step did not happen.
#[derive(Debug)]
enum LandError {
    /// May pass: retry on the next round.
    Retry(String),
    /// Will not pass: record the landing as failed.
    Fail(String),
    /// The request moved on under the lander (a new request, a takeover): drop it.
    Gone,
}

impl From<CoreError> for LandError {
    fn from(e: CoreError) -> Self {
        match e {
            CoreError::Revoked(p) => {
                Self::Fail(format!("{p}, who asked for this landing, is revoked"))
            }
            CoreError::NoLanding { .. }
            | CoreError::LandingStep { .. }
            | CoreError::UnknownFork(_) => Self::Gone,
            e => Self::Retry(e.to_string()),
        }
    }
}

type Step<T = ()> = Result<T, LandError>;

struct Worker {
    a: Arc<Authority>,
    config: LandConfig,
}

impl Worker {
    fn run(&self, stop: &AtomicBool) {
        let mut seen = self.a.lander_generation();
        while !stop.load(Ordering::SeqCst) {
            self.round(stop);
            seen = self.a.wait_lander(seen, self.config.poll);
        }
    }

    /// Drive every open request on a location with a remote as far as it goes.
    fn round(&self, stop: &AtomicBool) {
        let forks = match self.a.store.forks() {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(error = %e, "lander: cannot list forks");
                return;
            }
        };
        for fork in forks {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            let Some(landing) = fork.landing.clone() else {
                // A takeover dropped the request: its last error is not the fork's now.
                self.a.set_landing_error(&fork.name, None);
                continue;
            };
            if !in_flight(&landing.step) {
                self.a.set_landing_error(&fork.name, None);
                continue;
            }
            match self.drive(&fork.name, landing.changeset) {
                Ok(()) | Err(LandError::Gone) => self.a.set_landing_error(&fork.name, None),
                Err(LandError::Retry(e)) => {
                    tracing::warn!(fork = %fork.name, error = %e, "landing will be retried");
                    self.a.set_landing_error(&fork.name, Some(e));
                }
                Err(LandError::Fail(reason)) => {
                    tracing::warn!(fork = %fork.name, %reason, "landing failed");
                    match self
                        .a
                        .store
                        .record_landing_failed(&fork.name, landing.changeset, &reason)
                    {
                        Ok(_) => self.a.set_landing_error(&fork.name, None),
                        Err(e) => self.a.set_landing_error(&fork.name, Some(e.to_string())),
                    }
                }
            }
        }
    }

    /// Move the landing of `id` from `fork` step by step until it is in review, or a
    /// step does not happen.
    fn drive(&self, name: &ForkName, id: ChangesetId) -> Step {
        loop {
            let fork = self.a.store.fork(name)?.ok_or(LandError::Gone)?;
            let landing = match &fork.landing {
                Some(l) if l.changeset == id => l.clone(),
                _ => return Err(LandError::Gone),
            };
            let location = self
                .a
                .store
                .location(&fork.location)?
                .ok_or(LandError::Gone)?;
            // A location without a remote waits for the owner.
            let Some(remote) = location.remote.clone() else {
                return Ok(());
            };
            let slug = github_slug(&remote).ok_or_else(|| {
                LandError::Fail(format!(
                    "remote {remote:?} is not a GitHub repository; only GitHub remotes land through a pull request"
                ))
            })?;
            let git = Git::new(&self.config, &location)?;
            match &landing.step {
                LandingStep::Requested => {
                    // An earlier landing of the same changeset (dropped by a takeover,
                    // or from another fork with the same edit) may have exported it
                    // already: that export is this one's. Building it again gives the
                    // same commit when the parent is the same, so this matters when it
                    // is not: the remote's main has a newer commit with the same tree,
                    // or the old parent fell out of the search.
                    let export = match git.remote_branch(&remote, &export_branch(id))? {
                        Some(at) => git.adopt(&remote, &self.a.store, id, landing.main, at)?,
                        None => self.build(&git, &remote, &landing, None)?,
                    };
                    self.a.store.record_export_intent(name, id, &export)?;
                    self.fault("export_intent");
                }
                LandingStep::Exporting { export } => {
                    let pushed = git.remote_branch(&remote, &export.branch)? == Some(export.commit);
                    if !pushed && !git.has_commit(export.commit)? {
                        // Lost with the crash, or never fsynced: build it again, onto
                        // the parent the intent names; it must come out the same.
                        let again = self.build(&git, &remote, &landing, Some(export))?;
                        if again.commit != export.commit {
                            return Err(LandError::Fail(format!(
                                "the export rebuilt as {}, not {}, the commit its intent names",
                                again.commit, export.commit
                            )));
                        }
                    }
                    if !pushed {
                        git.push(&remote, export)?;
                    }
                    self.fault("pushed");
                    self.a.store.record_exported(name, id)?;
                }
                LandingStep::Exported { .. } => {
                    self.a.store.record_review_intent(name, id)?;
                    self.fault("review_intent");
                }
                LandingStep::Reviewing { export } => {
                    let (pr, url) = self.review(&slug, &fork, &landing, export)?;
                    self.fault("pr_opened");
                    self.a.store.record_in_review(name, id, pr, &url)?;
                }
                _ => return Ok(()),
            }
        }
    }

    fn fault(&self, at: &str) {
        if let Some(f) = &self.config.fault {
            f(at);
        }
    }

    /// The export of the landing: the target tree written into the location's bare
    /// repository and committed onto `parent`, or onto the newest first-parent commit
    /// of the remote's default branch whose tree is the main the request's verify judged.
    fn build(
        &self,
        git: &Git,
        remote: &str,
        landing: &Landing,
        intent: Option<&Export>,
    ) -> Step<Export> {
        let store = &self.a.store;
        let changeset = store
            .changeset(landing.changeset)?
            .ok_or_else(|| LandError::Fail(format!("no changeset {}", landing.changeset)))?;
        // The main the verify judged the head against: the export is the head's tree onto
        // the remote's commit of that main. (Drafts chain, so the head changeset's own
        // base is the previous draft's tree, which the remote has never seen.)
        let main = landing.main;
        let (parent, base_ref) = match intent {
            Some(e) => {
                git.fetch(remote, &e.parent.to_hex())?;
                (e.parent, e.base_ref.clone())
            }
            None => {
                let base_ref = git.default_branch(remote)?;
                git.fetch(remote, &format!("refs/heads/{base_ref}"))?;
                let parent = git.commit_with_tree(main)?.ok_or_else(|| {
                    LandError::Fail(format!(
                        "no commit in the last {PARENT_SEARCH} of {remote}'s {base_ref} has tree {main}: \
                         the location's main is not on the remote"
                    ))
                })?;
                (parent, base_ref)
            }
        };
        let tree = store.tree(changeset.target)?;
        // Every blob of the head that main does not have: the parent's commit brings
        // main's, and the fork's drafts may have added any of the rest.
        let on_main: std::collections::HashSet<GitOid> =
            store.tree(main)?.entries().map(|(_, e)| e.oid()).collect();
        let mut blobs = Blobs::new();
        for (_, entry) in tree.entries() {
            let oid = entry.oid();
            if !on_main.contains(&oid)
                && let std::collections::hash_map::Entry::Vacant(slot) = blobs.entry(oid)
            {
                slot.insert(store.read_blob(oid)?);
            }
        }
        let written = git
            .repo()
            .write_tree(&tree, &blobs)
            .map_err(|e| LandError::Retry(format!("writing the tree: {e}")))?;
        if written != changeset.target {
            return Err(LandError::Fail(format!(
                "git wrote tree {written}, not {}",
                changeset.target
            )));
        }
        // The commit is a function of the changeset and its parent alone, never of the
        // request (who asked, from which fork, when): every landing of one changeset
        // onto one main builds the same commit, so the one branch `cp/<id>` always
        // holds the commit any of them expects. Who asked goes in the PR body.
        let at_secs = store
            .changeset_first_at(landing.changeset)?
            .ok_or_else(|| LandError::Fail(format!("no changeset {}", landing.changeset)))?
            / 1000;
        let message = format!(
            "cuenca: land {}\n\nCuenca-Changeset: {}\n",
            &landing.changeset.to_hex()[..12],
            landing.changeset,
        );
        let commit = git.commit(written, parent, at_secs, &message)?;
        Ok(Export::new(landing.changeset, commit, parent, &base_ref))
    }

    /// The pull request from the export's branch: found, or opened.
    fn review(
        &self,
        slug: &str,
        fork: &Fork,
        landing: &Landing,
        export: &Export,
    ) -> Step<(u64, String)> {
        let gh = Gh {
            bin: &self.config.gh,
            slug,
            timeout: self.config.timeout,
        };
        if let Some(found) = gh.find(export)? {
            return Ok(found);
        }
        let title = format!(
            "cuenca: {} ({})",
            fork.name,
            &landing.changeset.to_hex()[..12]
        );
        gh.create(export, &title, &self.body(fork, landing, export)?)?;
        // The URL `gh pr create` prints is not trusted alone: find what it opened.
        gh.find(export)?.ok_or_else(|| {
            LandError::Retry(format!(
                "gh opened a pull request from {} but does not list it yet",
                export.branch
            ))
        })
    }

    fn body(&self, fork: &Fork, landing: &Landing, export: &Export) -> Step<String> {
        let mut verify = String::from("none recorded");
        if let Some(v) = &fork.verified
            && v.changeset == landing.changeset
            && let Some(record) = self.a.store.records(v.offset, 1)?.first()
            && let Event::VerifyRecorded { checks, base, .. } = &record.event
        {
            verify = if checks.is_empty() {
                format!("the base `{base}` declares no checks")
            } else {
                let each: Vec<String> = checks
                    .iter()
                    .map(|c| {
                        let status = match c.status {
                            CheckStatus::Passed => "passed",
                            CheckStatus::Failed => "failed",
                            CheckStatus::TimedOut => "timed out",
                            CheckStatus::NotRun => "not run",
                        };
                        format!("`{}` {status}", c.check.name)
                    })
                    .collect();
                format!("against base `{base}`: {}", each.join(", "))
            };
        }
        let holder = fork.holder().map(|h| h.to_string()).unwrap_or_default();
        Ok(format!(
            "Landed by Cuenca from fork `{}`, at the request of `{holder}`.\n\n\
             - Changeset: `{}`\n\
             - Tree: `{}`, the draft's\n\
             - Parent: `{}`\n\
             - Verify (advisory; the caller's own result): {verify}\n\n\
             GitHub's required checks gate the merge.\n",
            fork.name, landing.changeset, export.commit, export.parent,
        ))
    }
}

fn in_flight(step: &LandingStep) -> bool {
    matches!(
        step,
        LandingStep::Requested
            | LandingStep::Exporting { .. }
            | LandingStep::Exported { .. }
            | LandingStep::Reviewing { .. }
    )
}

/// `owner/repo` of a GitHub remote: `https://github.com/o/r(.git)`,
/// `ssh://git@github.com/o/r(.git)` or `git@github.com:o/r(.git)`.
pub fn github_slug(remote: &str) -> Option<String> {
    let rest = remote
        .strip_prefix("https://github.com/")
        .or_else(|| remote.strip_prefix("ssh://git@github.com/"))
        .or_else(|| remote.strip_prefix("git@github.com:"))?;
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let (owner, repo) = rest.split_once('/')?;
    let ok = |s: &str| {
        !s.is_empty()
            && !s.starts_with('.')
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    };
    (ok(owner) && ok(repo)).then(|| format!("{owner}/{repo}"))
}

/// Whether git's `stderr` from a failed call says the remote will refuse it again,
/// whatever the retry: GitHub's push rules (GH013: push protection, rulesets), its
/// file size limit (GH001, as an error: over 50 MB it is only a warning), a token not
/// allowed to touch workflow files (a PAT without the `workflow` scope, an App or
/// fine-grained token without the `workflows` permission), or a fetched history that
/// fails `transfer.fsckObjects` (a legacy `badDate`, say). The same commit pushed or
/// fetched again meets the same refusal.
fn refused_for_good(stderr: &str) -> bool {
    const MARKERS: [&str; 5] = [
        "error: GH001:",
        "error: GH013:",
        "push declined due to repository rule violations",
        "to create or update workflow",
        "fsck error",
    ];
    MARKERS.iter().any(|m| stderr.contains(m))
}

/// git, run on the location's bare repository with a clean environment.
struct Git<'c> {
    config: &'c LandConfig,
    dir: PathBuf,
}

impl<'c> Git<'c> {
    /// The bare repository of `location`, created on first use.
    fn new(config: &'c LandConfig, location: &Location) -> Step<Self> {
        let dir = config.repos.join(format!("{}.git", location.name));
        let git = Self { config, dir };
        if !git.dir.join("HEAD").exists() {
            let tmp = config.repos.join(format!(".{}.git.tmp", location.name));
            let _ = std::fs::remove_dir_all(&tmp);
            git.run_in(None, &["init", "--bare", "-q", &tmp.to_string_lossy()], b"")?;
            git.run_in(Some(&tmp), &["config", "transfer.fsckObjects", "true"], b"")?;
            std::fs::rename(&tmp, &git.dir).map_err(|e| LandError::Retry(e.to_string()))?;
        }
        Ok(git)
    }

    fn repo(&self) -> Repo {
        Repo::open(&self.dir)
    }

    fn command(&self, dir: Option<&Path>, args: &[&str]) -> Command {
        let mut cmd = Command::new(&self.config.git);
        for var in REDIRECTING_ENV {
            cmd.env_remove(var);
        }
        cmd.env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C");
        for setting in &self.config.git_config {
            cmd.args(["-c", setting]);
        }
        // `gh` answers for github.com, and nothing else does.
        let gh = self.config.gh.display().to_string().replace('\'', "'\\''");
        let helper = format!("!'{gh}' auth git-credential");
        cmd.args(["-c", "credential.helper="]).args([
            "-c",
            &format!("credential.https://github.com.helper={helper}"),
        ]);
        if let Some(dir) = dir {
            cmd.arg("-C").arg(dir);
        }
        cmd.args(args);
        cmd
    }

    fn run_in(&self, dir: Option<&Path>, args: &[&str], input: &[u8]) -> Step<String> {
        let what = format!("git {}", args.first().copied().unwrap_or(""));
        let out = output_within(self.command(dir, args), input, self.config.timeout, &what)?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let message = format!(
                "git {} failed: {}",
                args.first().copied().unwrap_or(""),
                stderr.trim()
            );
            return Err(if refused_for_good(&stderr) {
                LandError::Fail(message)
            } else {
                LandError::Retry(message)
            });
        }
        String::from_utf8(out.stdout)
            .map_err(|_| LandError::Retry(format!("git {args:?} printed non-UTF-8")))
    }

    fn run(&self, args: &[&str]) -> Step<String> {
        self.run_in(Some(&self.dir), args, b"")
    }

    /// The remote's default branch, from its `HEAD`.
    fn default_branch(&self, remote: &str) -> Step<String> {
        let out = self.run(&["ls-remote", "--symref", "--", remote, "HEAD"])?;
        out.lines()
            .find_map(|l| {
                l.strip_prefix("ref: refs/heads/")
                    .and_then(|r| r.strip_suffix("\tHEAD"))
            })
            .map(str::to_string)
            .ok_or_else(|| LandError::Retry(format!("{remote} names no default branch")))
    }

    /// Fetch `what` from the remote into `refs/cuenca/fetched`.
    fn fetch(&self, remote: &str, what: &str) -> Step {
        self.run(&[
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            "--",
            remote,
            &format!("+{what}:refs/cuenca/fetched"),
        ])?;
        Ok(())
    }

    /// The newest first-parent commit of what was fetched whose tree is `tree`.
    fn commit_with_tree(&self, tree: GitOid) -> Step<Option<GitOid>> {
        let out = self.run(&[
            "log",
            "--first-parent",
            "--format=%H %T",
            "-n",
            &PARENT_SEARCH.to_string(),
            "refs/cuenca/fetched",
        ])?;
        for line in out.lines() {
            if let Some((commit, t)) = line.split_once(' ')
                && t == tree.to_hex()
            {
                return GitOid::from_hex(commit)
                    .map(Some)
                    .map_err(|e| LandError::Retry(e.to_string()));
            }
        }
        Ok(None)
    }

    /// Take the export already at `at` on the remote's branch for `id` as this
    /// landing's: one commit whose tree is the changeset's target, onto one parent whose
    /// tree is `main`, the main its verify judged. Anything else at that branch is
    /// someone else's.
    fn adopt(
        &self,
        remote: &str,
        store: &Store,
        id: ChangesetId,
        main: GitOid,
        at: GitOid,
    ) -> Step<Export> {
        let changeset = store
            .changeset(id)?
            .ok_or_else(|| LandError::Fail(format!("no changeset {id}")))?;
        let branch = export_branch(id);
        self.fetch(remote, &format!("refs/heads/{branch}"))?;
        let line = self.run(&["rev-list", "--parents", "-n", "1", &at.to_hex()])?;
        let ids: Vec<&str> = line.split_whitespace().collect();
        let tree_of = |rev: &str| {
            self.run(&["rev-parse", &format!("{rev}^{{tree}}")])
                .map(|t| t.trim().to_string())
        };
        let ours = ids.len() == 2
            && ids[0] == at.to_hex()
            && tree_of(ids[0])? == changeset.target.to_hex()
            && tree_of(ids[1])? == main.to_hex();
        if !ours {
            return Err(LandError::Fail(format!(
                "{branch} on the remote is at {at}, which is not an export of {id}: someone else's branch"
            )));
        }
        let parent = GitOid::from_hex(ids[1]).map_err(|e| LandError::Retry(e.to_string()))?;
        let base_ref = self.default_branch(remote)?;
        Ok(Export::new(id, at, parent, &base_ref))
    }

    fn has_commit(&self, commit: GitOid) -> Step<bool> {
        let out = self
            .command(Some(&self.dir), &["cat-file", "-t", &commit.to_hex()])
            .stderr(Stdio::null())
            .output()
            .map_err(|e| LandError::Retry(format!("running git: {e}")))?;
        Ok(out.status.success() && out.stdout == b"commit\n")
    }

    /// Commit `tree` onto `parent`: the same inputs give the same id.
    fn commit(&self, tree: GitOid, parent: GitOid, at_secs: u64, message: &str) -> Step<GitOid> {
        let date = format!("{at_secs} +0000");
        let out = self
            .command(
                Some(&self.dir),
                &[
                    "commit-tree",
                    "--no-gpg-sign",
                    &tree.to_hex(),
                    "-p",
                    &parent.to_hex(),
                    "-F",
                    "-",
                ],
            )
            .env("GIT_AUTHOR_NAME", "Cuenca")
            .env("GIT_AUTHOR_EMAIL", "authority@cuenca.invalid")
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_NAME", "Cuenca")
            .env("GIT_COMMITTER_EMAIL", "authority@cuenca.invalid")
            .env("GIT_COMMITTER_DATE", &date)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                if let Some(mut stdin) = child.stdin.take() {
                    stdin.write_all(message.as_bytes())?;
                }
                child.wait_with_output()
            })
            .map_err(|e| LandError::Retry(format!("running git commit-tree: {e}")))?;
        if !out.status.success() {
            return Err(LandError::Retry(format!(
                "git commit-tree failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        GitOid::from_hex(String::from_utf8_lossy(&out.stdout).trim())
            .map_err(|e| LandError::Retry(e.to_string()))
    }

    /// Where the remote's `branch` is, if it exists.
    fn remote_branch(&self, remote: &str, branch: &str) -> Step<Option<GitOid>> {
        let refname = format!("refs/heads/{branch}");
        let out = self.run(&["ls-remote", "--", remote, &refname])?;
        for line in out.lines() {
            if let Some((sha, name)) = line.split_once('\t')
                && name == refname
            {
                return GitOid::from_hex(sha)
                    .map(Some)
                    .map_err(|e| LandError::Retry(e.to_string()));
            }
        }
        Ok(None)
    }

    /// Push the export's commit to its branch, which must not exist or already be at
    /// the commit.
    fn push(&self, remote: &str, export: &Export) -> Step {
        match self.remote_branch(remote, &export.branch)? {
            Some(at) if at == export.commit => return Ok(()),
            Some(at) => {
                return Err(LandError::Fail(format!(
                    "{} on the remote is at {at}, not {}: someone else's branch",
                    export.branch, export.commit
                )));
            }
            None => {}
        }
        let refname = format!("refs/heads/{}", export.branch);
        self.run(&[
            "push",
            "--quiet",
            "--no-verify",
            &format!("--force-with-lease={refname}:"),
            "--",
            remote,
            &format!("{}:{refname}", export.commit),
        ])?;
        Ok(())
    }
}

/// `gh`, for one repository.
struct Gh<'a> {
    bin: &'a Path,
    slug: &'a str,
    timeout: Duration,
}

impl Gh<'_> {
    fn run(&self, args: &[&str], input: &[u8]) -> Step<String> {
        let mut cmd = Command::new(self.bin);
        cmd.args(args)
            .env("GH_PROMPT_DISABLED", "1")
            .env("NO_COLOR", "1");
        let what = format!(
            "gh {}",
            args.iter().take(2).copied().collect::<Vec<_>>().join(" ")
        );
        let out = output_within(cmd, input, self.timeout, &what)?;
        if !out.status.success() {
            return Err(LandError::Retry(format!(
                "gh {} failed: {}",
                args.iter().take(2).copied().collect::<Vec<_>>().join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// The pull request whose head is the export's branch, in any state. One from the
    /// branch at another commit is not ours.
    fn find(&self, export: &Export) -> Step<Option<(u64, String)>> {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Pr {
            number: u64,
            url: String,
            head_ref_oid: String,
            head_ref_name: String,
            is_cross_repository: bool,
        }
        let out = self.run(
            &[
                "pr",
                "list",
                "--repo",
                self.slug,
                "--head",
                &export.branch,
                "--state",
                "all",
                "--json",
                "number,url,headRefOid,headRefName,isCrossRepository",
                "--limit",
                "20",
            ],
            b"",
        )?;
        let prs: Vec<Pr> = serde_json::from_str(&out)
            .map_err(|e| LandError::Retry(format!("gh pr list printed {out:?}: {e}")))?;
        // `--head` matches the branch name in any repository: a pull request from
        // someone's fork with a branch of that name is not ours.
        let mut ours = prs
            .into_iter()
            .filter(|p| p.head_ref_name == export.branch && !p.is_cross_repository);
        match ours.next() {
            None => Ok(None),
            Some(p) if p.head_ref_oid == export.commit.to_hex() => Ok(Some((p.number, p.url))),
            Some(p) => Err(LandError::Fail(format!(
                "pull request {} from {} is at {}, not {}",
                p.number, export.branch, p.head_ref_oid, export.commit
            ))),
        }
    }

    fn create(&self, export: &Export, title: &str, body: &str) -> Step {
        self.run(
            &[
                "pr",
                "create",
                "--repo",
                self.slug,
                "--head",
                &export.branch,
                "--base",
                &export.base_ref,
                "--title",
                title,
                "--body-file",
                "-",
            ],
            body.as_bytes(),
        )?;
        Ok(())
    }
}

/// Run `cmd` with `input` on its stdin, killing it if it has not ended within
/// `timeout` (a hung network call must not stall every landing behind it).
fn output_within(
    mut cmd: Command,
    input: &[u8],
    timeout: Duration,
    what: &str,
) -> Step<std::process::Output> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| LandError::Retry(format!("running {what}: {e}")))?;
    let mut stdin = child.stdin.take();
    let input = input.to_vec();
    let writer = std::thread::spawn(move || {
        if let Some(s) = stdin.as_mut() {
            let _ = s.write_all(&input);
        }
    });
    let read = |pipe: Option<Box<dyn std::io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf);
            }
            buf
        })
    };
    let stdout = read(child.stdout.take().map(|p| Box::new(p) as _));
    let stderr = read(child.stderr.take().map(|p| Box::new(p) as _));
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(LandError::Retry(format!(
                    "{what} did not end within {} s and was killed",
                    timeout.as_secs()
                )));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(LandError::Retry(format!("waiting for {what}: {e}"))),
        }
    };
    let _ = writer.join();
    Ok(std::process::Output {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

/// The last error each fork's landing hit, for `GET /v1/forks/{fork}/landing`.
#[derive(Default)]
pub(crate) struct LandingErrors(std::sync::Mutex<HashMap<ForkName, String>>);

impl LandingErrors {
    pub(crate) fn set(&self, fork: &ForkName, error: Option<String>) {
        let mut all = self.0.lock().expect("landing errors lock");
        match error {
            Some(e) => {
                all.insert(fork.clone(), e);
            }
            None => {
                all.remove(fork);
            }
        }
    }

    pub(crate) fn get(&self, fork: &ForkName) -> Option<String> {
        self.0
            .lock()
            .expect("landing errors lock")
            .get(fork)
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::{github_slug, refused_for_good};

    #[test]
    fn github_refusals_and_fsck_errors_are_for_good() {
        for stderr in [
            "remote: error: GH013: Repository rule violations found for refs/heads/cp/x.",
            " ! [remote rejected] cp/x -> cp/x (push declined due to repository rule violations)",
            "remote: error: GH001: Large files detected.",
            " ! [remote rejected] cp/x -> cp/x (refusing to allow a Personal Access Token to create or update workflow `.github/workflows/ci.yml` without `workflow` scope)",
            " ! [remote rejected] cp/x -> cp/x (refusing to allow a GitHub App to create or update workflow `.github/workflows/ci.yml` without `workflows` permission)",
            "error: object 1234: badDate: invalid author/committer line - bad date\nfatal: fsck error in packed object",
        ] {
            assert!(refused_for_good(stderr), "{stderr}");
        }
        for stderr in [
            "fatal: unable to access 'https://github.com/o/r/': Could not resolve host: github.com",
            "remote: Internal Server Error",
            " ! [rejected] cp/x -> cp/x (stale info)",
            "fatal: the remote end hung up unexpectedly",
            "remote: warning: GH001: Large files detected. You may want to try Git Large File Storage.\nerror: RPC failed; HTTP 502 curl 22 The requested URL returned error: 502",
        ] {
            assert!(!refused_for_good(stderr), "{stderr}");
        }
    }

    #[test]
    fn only_github_remotes_have_a_slug() {
        for (remote, slug) in [
            ("https://github.com/broomva/cuenca", "broomva/cuenca"),
            ("https://github.com/broomva/cuenca.git", "broomva/cuenca"),
            ("https://github.com/broomva/cuenca/", "broomva/cuenca"),
            ("git@github.com:broomva/cuenca.git", "broomva/cuenca"),
            (
                "ssh://git@github.com/broomva/my_repo.x",
                "broomva/my_repo.x",
            ),
        ] {
            assert_eq!(github_slug(remote).as_deref(), Some(slug), "{remote}");
        }
        for remote in [
            "https://gitlab.com/broomva/cuenca",
            "https://github.com/broomva",
            "https://github.com/broomva/cuenca/pulls",
            "https://github.com/../x",
            "https://github.com/a/b c",
            "file:///tmp/x.git",
            "/tmp/x.git",
        ] {
            assert_eq!(github_slug(remote), None, "{remote}");
        }
    }
}
