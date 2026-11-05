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
//! **Main follows the remote** (ADR 0002). After each round's landings, the lander
//! follows one location with a remote: the one least recently followed. It fetches the
//! remote's default branch into `refs/cuenca/followed`. When that tree is not the
//! location's main, it reads the tree's structure (ruled blobs and tree objects checked
//! as in [`Repo::read_tree`]), sizes the blobs the store lacks with `--batch-check`, and
//! reads them in batches of at most [`LandConfig::follow_batch_bytes`], each ingested
//! and committed before the next, so a later follow skips them. Once every blob is
//! present it records [`cuenca_core::Event::MainFollowed`]. All of a follow's git work
//! shares one deadline, [`LandConfig::follow_timeout`]; the local store work does not
//! (owner decision D1). A follow that does not fit fails with the step it reached, the
//! time it took and the bytes left, shown at `GET /v1/locations` and logged, and main
//! does not move. A ref lock is never removed: a follow that meets one fails naming it,
//! and is tried again on later rounds.
//!
//! Every git and gh call runs in its own process group, within [`LandConfig::timeout`]
//! and the stdout cap its kind of call has ([`OutputCaps`]); at either limit the whole
//! group is killed, helpers included, and the step is retried with an error naming the
//! limit ([`cuenca_tree::bounded`], ADR 0002, BRO-3599).
//!
//! Identity: `gh` with the token already on the authority host (spec section 0, "the
//! shared gh login at first"; the GitHub App is BRO-3305), as git's credential helper
//! for `https://github.com` remotes too. Only GitHub remotes land through a PR.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use cuenca_core::checks::CheckStatus;
use cuenca_core::{
    ChangesetId, CoreError, Event, Export, Fork, ForkName, Landing, LandingStep, Location,
    LocationName, Store, export_branch,
};
use cuenca_tree::GitOid;
use cuenca_tree::bounded::{self, Limits, RunError};
use cuenca_tree::git::{Blobs, DEFAULT_MAX_BLOB_BYTES, GitError, REDIRECTING_ENV, Repo, RepoCaps};

use crate::authority::Authority;

/// The first-parent commits searched first for the tree a landing is committed onto:
/// most lines start near the tip, so this is all most landings read.
const NEAR_SEARCH: usize = 1000;

/// How far back the search goes when the near window misses. The root of a fork's line
/// can sit far behind the tip once main follows the remote, so this bounds the work, not
/// the age of a fork. Its log is held to [`OutputCaps::parent_log`].
const PARENT_SEARCH: usize = 1_000_000;

/// What a search of the fetched first-parent history for a tree found.
enum Found {
    At(GitOid),
    /// The whole history was searched and no commit has the tree.
    Absent,
    /// [`PARENT_SEARCH`] commits were searched without reaching the end of history.
    BeyondWindow,
}

/// Where the remote's default branch is fetched to follow main, apart from the ref a
/// landing fetches into.
const FOLLOWED: &str = "refs/cuenca/followed";

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
    /// The longest one git or gh call may take; past it the call's whole process group
    /// is killed and the step retried on the next round.
    pub timeout: Duration,
    /// The most each kind of git or gh call may print on stdout.
    pub caps: OutputCaps,
    /// The one deadline of a follow: all of its git work (discovery, fetch, listing,
    /// sizing and blob reads) together. 60 s (owner decision D2).
    pub follow_timeout: Duration,
    /// The most blob bytes a follow reads into memory before ingesting them: missing
    /// blobs are read in batches of at most this much, and a larger blob, up to the
    /// per-blob cap, in a batch of its own. 64 MiB.
    pub follow_batch_bytes: u64,
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
            caps: OutputCaps::default(),
            follow_timeout: Duration::from_secs(60),
            follow_batch_bytes: 64 * 1024 * 1024,
            fault: None,
        }
    }
}

/// The most stdout one git or gh call may print, by kind of call (ADR 0002). Past its cap
/// a call's process group is killed and the step retried, with an error naming the cap
/// and the field that sets it. stderr keeps only its last
/// [`bounded::STDERR_TAIL`] bytes whatever the call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputCaps {
    /// A call that prints an id or nothing: `rev-parse`, `rev-list -n 1`, `cat-file -t`,
    /// `commit-tree`, `init`, `config`, `fetch --quiet`, `push --quiet`. 4 KiB.
    pub small: u64,
    /// `ls-remote`, `gh pr list` and `gh pr create`. 1 MiB.
    pub listing: u64,
    /// The parent search's first-parent `log`, of up to 1,000,001 commits at about 82
    /// bytes each. 128 MiB.
    pub parent_log: u64,
    /// A follow's `ls-tree -r -t` of the whole tree. 256 MiB.
    pub ls_tree: u64,
    /// A follow's `cat-file --batch-check` of the blobs the store lacks. 64 MiB.
    pub batch_check: u64,
}

impl Default for OutputCaps {
    fn default() -> Self {
        Self {
            small: 4 * 1024,
            listing: 1024 * 1024,
            parent_log: 128 * 1024 * 1024,
            ls_tree: 256 * 1024 * 1024,
            batch_check: 64 * 1024 * 1024,
        }
    }
}

/// Which of [`OutputCaps`] a call is held to.
#[derive(Clone, Copy, Debug)]
enum Cap {
    Small,
    Listing,
    ParentLog,
}

impl Cap {
    fn of(self, caps: &OutputCaps) -> (u64, &'static str) {
        match self {
            Self::Small => (caps.small, "LandConfig::caps.small"),
            Self::Listing => (caps.listing, "LandConfig::caps.listing"),
            Self::ParentLog => (caps.parent_log, "LandConfig::caps.parent_log"),
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
            followed_at: RefCell::new(HashMap::new()),
            stop: stop.clone(),
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
    /// When each location was last followed (successfully or not), for the order.
    followed_at: RefCell<HashMap<LocationName, Instant>>,
    /// The lander's stop flag, handed to the git bridge so a follow starts no git call
    /// after it is set.
    stop: Arc<AtomicBool>,
}

impl Worker {
    fn run(&self, stop: &AtomicBool) {
        let mut seen = self.a.lander_generation();
        while !stop.load(Ordering::SeqCst) {
            self.round(stop);
            seen = self.a.wait_lander(seen, self.config.poll);
        }
    }

    /// Drive every open request on a location with a remote as far as it goes, then
    /// follow one location's remote. Landings go first.
    fn round(&self, stop: &AtomicBool) {
        self.drive_all(stop);
        self.follow_next(stop);
    }

    fn drive_all(&self, stop: &AtomicBool) {
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

    /// Follow the location with a remote least recently followed (never followed first),
    /// one per round (owner decision D2): a location that keeps failing delays each of
    /// the others by one round per cycle, not indefinitely. The outcome is logged and
    /// shown at `GET /v1/locations`.
    fn follow_next(&self, stop: &AtomicBool) {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let locations = match self.a.store.locations() {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(error = %e, "lander: cannot list locations");
                return;
            }
        };
        let next = {
            let at = self.followed_at.borrow();
            locations
                .into_iter()
                .filter(|l| l.remote.is_some())
                .min_by_key(|l| (at.get(&l.name).copied(), l.name.clone()))
        };
        let Some(location) = next else { return };
        self.followed_at
            .borrow_mut()
            .insert(location.name.clone(), Instant::now());
        match self.follow(&location, stop) {
            Ok(Some(offset)) => {
                tracing::info!(location = %location.name, offset, "main followed the remote");
                self.a.set_follow_error(&location.name, None);
            }
            Ok(None) => self.a.set_follow_error(&location.name, None),
            Err(e) => {
                tracing::warn!(location = %location.name, error = %e, "main did not follow the remote; trying again on a later round");
                self.a.set_follow_error(&location.name, Some(e));
            }
        }
    }

    /// Follow `location`'s remote default branch (ADR 0002): the offset of the
    /// `MainFollowed` record, or `None` when main is that tree already. Every git call
    /// ends by one deadline, [`LandConfig::follow_timeout`] from now, checked with the
    /// stop flag before each step; blobs are read in batches, each ingested and
    /// committed before the next, and main moves only once every blob is present.
    fn follow(&self, location: &Location, stop: &AtomicBool) -> Result<Option<u64>, String> {
        let remote = location.remote.as_deref().unwrap_or_default();
        let mut f = Following {
            start: Instant::now(),
            deadline: Instant::now() + self.config.follow_timeout,
            timeout: self.config.follow_timeout,
            stop,
            step: "starting",
            left: None,
        };
        f.next("preparing the repository")?;
        let git = Git::new(&self.config, location, Some(f.deadline)).map_err(|e| f.failed(e))?;

        f.next("finding the default branch")?;
        let branch = git.default_branch(remote).map_err(|e| f.failed(e))?;
        f.next("fetching")?;
        git.fetch_to(remote, &format!("refs/heads/{branch}"), FOLLOWED)
            .map_err(|e| f.failed(e))?;
        f.next("resolving the fetched commit")?;
        let commit = git
            .rev_parse(&format!("{FOLLOWED}^{{commit}}"))
            .map_err(|e| f.failed(e))?;
        // The tree of that commit, not of the ref again: the ref may have moved since,
        // and the commit and tree recorded must be one pair.
        let tree_id = git
            .rev_parse(&format!("{commit}^{{tree}}"))
            .map_err(|e| f.failed(e))?;
        if tree_id == location.main {
            return Ok(None);
        }

        // The tree's structure: tree objects byte-checked and ruled blobs checked, as a
        // full read does; the other blobs are read below, only those the store lacks.
        f.next("listing the tree")?;
        let caps = RepoCaps {
            ls_tree: self.config.caps.ls_tree,
            batch_check: self.config.caps.batch_check,
            ..RepoCaps::default()
        };
        let repo = Repo::open(&git.dir)
            .with_isolated_config()
            .with_git(&self.config.git)
            .with_deadline(f.deadline)
            .with_stop(self.stop.clone())
            .with_caps(caps);
        let tree = repo
            .read_tree_known(&tree_id.to_hex(), |_| true)
            .map_err(|e| f.failed(git_error(e)))?;
        let store = &self.a.store;
        let mut missing = Vec::new();
        let mut seen = HashSet::new();
        for (_, entry) in tree.entries() {
            let oid = entry.oid();
            if seen.insert(oid)
                && store
                    .blob_info(oid)
                    .map_err(|e| f.failed(e.into()))?
                    .is_none()
            {
                missing.push(oid);
            }
        }

        f.next("sizing the missing blobs")?;
        let sizes = repo
            .blob_sizes(&missing)
            .map_err(|e| f.failed(git_error(e)))?;
        let mut batches: Vec<(Vec<GitOid>, u64)> = Vec::new();
        for oid in missing {
            let size = sizes[&oid];
            if size > DEFAULT_MAX_BLOB_BYTES {
                return Err(f.failed(LandError::Fail(format!(
                    "blob {oid} is {size} bytes, over the {DEFAULT_MAX_BLOB_BYTES}-byte per-blob cap"
                ))));
            }
            match batches.last_mut() {
                Some((oids, total))
                    if *total + size <= self.config.follow_batch_bytes && !oids.is_empty() =>
                {
                    oids.push(oid);
                    *total += size;
                }
                _ => batches.push((vec![oid], size)),
            }
        }
        let mut left: u64 = batches.iter().map(|(_, total)| total).sum();
        for (oids, total) in batches {
            f.left = Some(left);
            f.next("reading blobs")?;
            let blobs = repo
                .clone()
                .with_memory_limits(DEFAULT_MAX_BLOB_BYTES, total.max(1))
                .read_blobs(oids.iter().copied())
                .map_err(|e| f.failed(git_error(e)))?;
            // Local store work: not bounded by the deadline (owner decision D1).
            let mut batch = store.blob_batch();
            for oid in &oids {
                let content = &blobs[oid];
                batch
                    .add(*oid, content.len() as u64, content.as_slice())
                    .map_err(|e| f.failed(e.into()))?;
            }
            batch.commit().map_err(|e| f.failed(e.into()))?;
            left -= total;
        }
        f.left = None;
        f.step = "recording the follow";
        store
            .follow_main(&location.name, &tree, commit)
            .map_err(|e| f.failed(e.into()))
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
            // The main this fork's line started from; the landing is committed onto it.
            let root = self.a.store.root_main(&fork.name)?;
            let git = Git::new(&self.config, &location, None)?;
            match &landing.step {
                LandingStep::Requested => {
                    // An earlier landing of the same changeset (dropped by a takeover,
                    // or from another fork with the same edit) may have exported it
                    // already: that export is this one's. Building it again gives the
                    // same commit when the parent is the same, so this matters when it
                    // is not: the remote's main has a newer commit with the same tree,
                    // or the old parent fell out of the search.
                    let export = match git.remote_branch(&remote, &export_branch(id))? {
                        Some(at) => git.adopt(&remote, &self.a.store, id, root, at)?,
                        None => self.build(&git, &remote, &landing, root, None)?,
                    };
                    self.a.store.record_export_intent(name, id, &export)?;
                    self.fault("export_intent");
                }
                LandingStep::Exporting { export } => {
                    let pushed = git.remote_branch(&remote, &export.branch)? == Some(export.commit);
                    if !pushed && !git.has_commit(export.commit)? {
                        // Lost with the crash, or never fsynced: build it again, onto
                        // the parent the intent names; it must come out the same.
                        let again = self.build(&git, &remote, &landing, root, Some(export))?;
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
    /// of the remote's default branch whose tree is `root`, the main the fork's line
    /// started from ([`cuenca_core::Store::root_main`]).
    fn build(
        &self,
        git: &Git,
        remote: &str,
        landing: &Landing,
        root: GitOid,
        intent: Option<&Export>,
    ) -> Step<Export> {
        let store = &self.a.store;
        let changeset = store
            .changeset(landing.changeset)?
            .ok_or_else(|| LandError::Fail(format!("no changeset {}", landing.changeset)))?;
        // The main the fork's line started from: the export is the head's tree onto the
        // remote's commit of that main, so the pull request holds this line's changes
        // and nothing else, even once main has moved past it (BRO-3517). Today main moves
        // only by a landing on a location without a remote, so this is the main the
        // verify judged. (Drafts chain, so the head changeset's own base is the previous
        // draft's tree, which the remote has never seen.)
        let main = root;
        let (parent, base_ref) = match intent {
            Some(e) => {
                git.fetch(remote, &e.parent.to_hex())?;
                (e.parent, e.base_ref.clone())
            }
            None => {
                let base_ref = git.default_branch(remote)?;
                git.fetch(remote, &format!("refs/heads/{base_ref}"))?;
                let parent = match git.commit_with_tree(main)? {
                    Found::At(parent) => parent,
                    Found::Absent => {
                        return Err(LandError::Fail(format!(
                            "no first-parent commit of {remote}'s {base_ref} has tree {main}, the \
                             main this fork's line started from: that main is not on the remote \
                             (it was never pushed, or the history was rewritten past it); fork \
                             again from the location's main"
                        )));
                    }
                    Found::BeyondWindow => {
                        return Err(LandError::Fail(format!(
                            "tree {main}, the main this fork's line started from, is not in the \
                             last {PARENT_SEARCH} first-parent commits of {remote}'s {base_ref}: \
                             fork again from the location's main"
                        )));
                    }
                };
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
            config: &self.config,
            slug,
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

/// One follow operation's progress, for its checks and its error.
struct Following<'s> {
    start: Instant,
    deadline: Instant,
    timeout: Duration,
    stop: &'s AtomicBool,
    step: &'static str,
    /// The bytes of missing blobs not yet ingested, once they are known.
    left: Option<u64>,
}

impl Following<'_> {
    /// Go on to `step`, unless the authority is stopping or the deadline has passed.
    fn next(&mut self, step: &'static str) -> Result<(), String> {
        self.step = step;
        if self.stop.load(Ordering::SeqCst) {
            return Err(self.failed(LandError::Retry("the authority is stopping".into())));
        }
        if Instant::now() >= self.deadline {
            return Err(self.failed(LandError::Retry(format!(
                "the follow deadline ({} s) passed; raise LandConfig::follow_timeout for this \
                 host if this repeats",
                self.timeout.as_secs()
            ))));
        }
        Ok(())
    }

    /// The error of a follow that stopped at its step: the step, the time it took and
    /// the bytes left.
    fn failed(&self, e: LandError) -> String {
        let why = match e {
            LandError::Retry(e) | LandError::Fail(e) => e,
            LandError::Gone => "the location is gone".into(),
        };
        let left = self
            .left
            .map(|b| format!(", {b} bytes of blobs left"))
            .unwrap_or_default();
        format!(
            "following stopped while {} after {:.1} s{left}: {why}",
            self.step,
            self.start.elapsed().as_secs_f64()
        )
    }
}

/// A git bridge error as a step error; a cap names the `LandConfig` field that sets it.
fn git_error(e: GitError) -> LandError {
    LandError::Retry(match &e {
        GitError::Capped {
            field: field @ ("ls_tree" | "batch_check"),
            ..
        } => format!("{e}; the cap is set by LandConfig::caps.{field}"),
        _ => e.to_string(),
    })
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
    REFUSALS.iter().any(|m| stderr.contains(m))
}

/// The messages [`refused_for_good`] looks for, watched for in a git call's stderr as it
/// streams, so one is found however much output comes after it.
const REFUSALS: [&str; 5] = [
    "error: GH001:",
    "error: GH013:",
    "push declined due to repository rule violations",
    "to create or update workflow",
    "fsck error",
];

/// git, run on the location's bare repository with a clean environment.
struct Git<'c> {
    config: &'c LandConfig,
    dir: PathBuf,
    /// A follow's one deadline, over and above each call's timeout.
    deadline: Option<Instant>,
}

impl<'c> Git<'c> {
    /// The bare repository of `location`, created on first use; every call ends by
    /// `deadline` too, when there is one (a follow's).
    fn new(config: &'c LandConfig, location: &Location, deadline: Option<Instant>) -> Step<Self> {
        let dir = config.repos.join(format!("{}.git", location.name));
        let git = Self {
            config,
            dir,
            deadline,
        };
        if !git.dir.join("HEAD").exists() {
            let tmp = config.repos.join(format!(".{}.git.tmp", location.name));
            let _ = std::fs::remove_dir_all(&tmp);
            git.run_in(
                None,
                &["init", "--bare", "-q", &tmp.to_string_lossy()],
                Cap::Small,
            )?;
            git.run_in(
                Some(&tmp),
                &["config", "transfer.fsckObjects", "true"],
                Cap::Small,
            )?;
            std::fs::rename(&tmp, &git.dir).map_err(|e| LandError::Retry(e.to_string()))?;
        }
        Ok(git)
    }

    /// The bare repository through the git bridge, run as the configured git, every
    /// call of the operation it starts ending within the call timeout from now.
    fn repo(&self) -> Repo {
        Repo::open(&self.dir)
            .with_git(&self.config.git)
            .with_deadline(std::time::Instant::now() + self.config.timeout)
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

    fn run_in(&self, dir: Option<&Path>, args: &[&str], cap: Cap) -> Step<String> {
        let what = format!("git {}", args.first().copied().unwrap_or(""));
        let out = output_within(
            self.command(dir, args),
            b"",
            self.config,
            self.deadline,
            cap,
            &what,
            &REFUSALS,
        )?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let message = format!(
                "git {} failed: {}",
                args.first().copied().unwrap_or(""),
                stderr.trim()
            );
            // The tail is what is shown; a refusal is found wherever git printed it, as
            // stderr streamed ([`REFUSALS`]).
            let refused = out.stderr_seen.iter().any(|seen| *seen) || refused_for_good(&stderr);
            return Err(if refused {
                LandError::Fail(message)
            } else {
                LandError::Retry(message)
            });
        }
        String::from_utf8(out.stdout)
            .map_err(|_| LandError::Retry(format!("git {args:?} printed non-UTF-8")))
    }

    fn run(&self, args: &[&str], cap: Cap) -> Step<String> {
        self.run_in(Some(&self.dir), args, cap)
    }

    /// The remote's default branch, from its `HEAD`.
    fn default_branch(&self, remote: &str) -> Step<String> {
        let out = self.run(
            &["ls-remote", "--symref", "--", remote, "HEAD"],
            Cap::Listing,
        )?;
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
        self.fetch_to(remote, what, "refs/cuenca/fetched")
    }

    /// Fetch `what` from the remote into the local ref `to`.
    ///
    /// A ref lock is never removed here (ADR 0002): nothing can prove the process that
    /// took it is dead (a killed call may never have touched the ref, and another git
    /// process may hold it). A lock present before the fetch, or one the fetch fails on,
    /// fails the call with an error naming it.
    fn fetch_to(&self, remote: &str, what: &str, to: &str) -> Step {
        let lock = self.dir.join(format!("{to}.lock"));
        if lock.exists() {
            return Err(LandError::Retry(self.locked(to, &lock)));
        }
        let fetched = self.run(
            &[
                "fetch",
                "--quiet",
                "--no-tags",
                "--no-write-fetch-head",
                "--",
                remote,
                &format!("+{what}:{to}"),
            ],
            Cap::Small,
        );
        match fetched {
            // git names the lock it could not take; only this ref's own lock is this
            // error, and any other failure is reported as it is.
            Err(LandError::Retry(e)) if e.contains(&format!("{to}.lock")) => {
                tracing::warn!(error = %e, "git fetch failed on a ref lock");
                Err(LandError::Retry(self.locked(to, &lock)))
            }
            Err(e) => Err(e),
            Ok(_) => Ok(()),
        }
    }

    /// The error for the lock of ref `to` in the way. The status names the lock within
    /// the location's bare repository; the host path goes to the log only.
    fn locked(&self, to: &str, lock: &Path) -> String {
        tracing::warn!(
            lock = %lock.display(),
            repository = %self.dir.display(),
            "a ref lock is in the way of following; it is never removed automatically"
        );
        format!(
            "the ref lock {to}.lock in the location's bare repository is in the way: a git \
             process may hold it, or a killed one left it. It is never removed \
             automatically; once no git process works in the repository (its path is in \
             the authority's log), remove the file"
        )
    }

    /// The object `rev` names in the bare repository.
    fn rev_parse(&self, rev: &str) -> Step<GitOid> {
        let out = self.run(&["rev-parse", "--verify", "--quiet", rev], Cap::Small)?;
        GitOid::from_hex(out.trim()).map_err(|e| LandError::Retry(format!("{rev}: {e}")))
    }

    /// The newest first-parent commit of what was fetched whose tree is `tree`. The near
    /// window is read first, so a line rooted near the tip costs a few kilobytes; only a
    /// miss with the window full reads the deep one.
    fn commit_with_tree(&self, tree: GitOid) -> Step<Found> {
        for window in [NEAR_SEARCH, PARENT_SEARCH] {
            // One commit past the window, so a history exactly the window's length ends
            // the search as absent rather than beyond it.
            let out = self.run(
                &[
                    "log",
                    "--first-parent",
                    "--format=%H %T",
                    "-n",
                    &(window + 1).to_string(),
                    "refs/cuenca/fetched",
                ],
                Cap::ParentLog,
            )?;
            let mut seen = 0;
            for line in out.lines() {
                seen += 1;
                if seen > window {
                    break;
                }
                if let Some((commit, t)) = line.split_once(' ')
                    && t == tree.to_hex()
                {
                    return GitOid::from_hex(commit)
                        .map(Found::At)
                        .map_err(|e| LandError::Retry(e.to_string()));
                }
            }
            if seen <= window {
                return Ok(Found::Absent);
            }
        }
        Ok(Found::BeyondWindow)
    }

    /// Take the export already at `at` on the remote's branch for `id` as this
    /// landing's: one commit whose tree is the changeset's target, onto one parent whose
    /// tree is `main`, the main the fork's line started from ([`Store::root_main`]).
    /// Until main can move on a location with a remote, that is the main the verify
    /// judged, so an export pushed earlier is adopted too. Anything else at that branch
    /// is someone else's.
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
        let line = self.run(
            &["rev-list", "--parents", "-n", "1", &at.to_hex()],
            Cap::Small,
        )?;
        let ids: Vec<&str> = line.split_whitespace().collect();
        let tree_of = |rev: &str| {
            self.run(&["rev-parse", &format!("{rev}^{{tree}}")], Cap::Small)
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
        let cmd = self.command(Some(&self.dir), &["cat-file", "-t", &commit.to_hex()]);
        let out = output_within(
            cmd,
            b"",
            self.config,
            self.deadline,
            Cap::Small,
            "git cat-file",
            &[],
        )?;
        Ok(out.status.success() && out.stdout == b"commit\n")
    }

    /// Commit `tree` onto `parent`: the same inputs give the same id.
    fn commit(&self, tree: GitOid, parent: GitOid, at_secs: u64, message: &str) -> Step<GitOid> {
        let date = format!("{at_secs} +0000");
        let mut cmd = self.command(
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
        );
        cmd.env("GIT_AUTHOR_NAME", "Cuenca")
            .env("GIT_AUTHOR_EMAIL", "authority@cuenca.invalid")
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_NAME", "Cuenca")
            .env("GIT_COMMITTER_EMAIL", "authority@cuenca.invalid")
            .env("GIT_COMMITTER_DATE", &date);
        let out = output_within(
            cmd,
            message.as_bytes(),
            self.config,
            self.deadline,
            Cap::Small,
            "git commit-tree",
            &[],
        )?;
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
        let out = self.run(&["ls-remote", "--", remote, &refname], Cap::Listing)?;
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
        self.run(
            &[
                "push",
                "--quiet",
                "--no-verify",
                &format!("--force-with-lease={refname}:"),
                "--",
                remote,
                &format!("{}:{refname}", export.commit),
            ],
            Cap::Small,
        )?;
        Ok(())
    }
}

/// `gh`, for one repository.
struct Gh<'a> {
    config: &'a LandConfig,
    slug: &'a str,
}

impl Gh<'_> {
    fn run(&self, args: &[&str], input: &[u8]) -> Step<String> {
        let mut cmd = Command::new(&self.config.gh);
        cmd.args(args)
            .env("GH_PROMPT_DISABLED", "1")
            .env("NO_COLOR", "1");
        let what = format!(
            "gh {}",
            args.iter().take(2).copied().collect::<Vec<_>>().join(" ")
        );
        let out = output_within(cmd, input, self.config, None, Cap::Listing, &what, &[])?;
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

/// Run `cmd` with `input` on its stdin in its own process group, within the call
/// timeout and its stdout cap ([`bounded::run`]): a hung network call must not stall
/// every landing behind it, nor leave a helper running with its pipes.
fn output_within(
    cmd: Command,
    input: &[u8],
    config: &LandConfig,
    deadline: Option<Instant>,
    cap: Cap,
    what: &str,
    watch: &[&str],
) -> Step<bounded::Output> {
    let (stdout_cap, field) = cap.of(&config.caps);
    let mut limits = Limits::within(config.timeout, stdout_cap);
    // A follow's deadline binds when it comes first.
    let follow = deadline.filter(|d| limits.deadline.is_some_and(|own| *d < own));
    if let Some(d) = follow {
        limits.deadline = Some(d);
    }
    let out = bounded::run_watched(cmd, input, what, limits, watch).map_err(|e| {
        LandError::Retry(match e {
            RunError::TimedOut { .. } if follow.is_some() => format!(
                "{what} did not end by the follow deadline ({} s) and was killed",
                config.follow_timeout.as_secs()
            ),
            RunError::TimedOut { .. } => format!(
                "{what} did not end within {} s and was killed",
                config.timeout.as_secs()
            ),
            RunError::Capped { cap, reached, .. } => format!(
                "{what} printed at least {reached} bytes, past its {cap}-byte output cap, and was \
                 killed; the cap is set by {field}"
            ),
            e => e.to_string(),
        })
    })?;
    // A call that succeeded without taking all its input (a commit message, a pull
    // request body) acted on less than it was given; a failed call's own error says
    // more, so it is the one reported.
    if out.status.success()
        && let Some(e) = &out.stdin_error
    {
        return Err(LandError::Retry(format!(
            "{what} did not take all its input: {e}"
        )));
    }
    Ok(out)
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
    use super::{Cap, Git, LandConfig, LandError, github_slug, output_within, refused_for_good};

    /// A refusal git prints between more stderr than the kept tail and before more
    /// output still fails the landing for good, rather than retrying it forever.
    #[test]
    fn a_refusal_buried_in_stderr_fails_for_good() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = tmp.path().join("git");
        std::fs::write(
            &fake,
            "#!/bin/sh\nhead -c 40000 /dev/zero >&2\n\
             echo 'remote: error: GH013: Repository rule violations found' >&2\n\
             head -c 200000 /dev/zero >&2\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let mut config = LandConfig::new(tmp.path().join("repos"), "gh".into());
        config.git = fake;
        let git = Git {
            config: &config,
            dir: tmp.path().to_path_buf(),
            deadline: None,
        };
        // A script just written can be busy for a moment (ETXTBSY) when another test
        // thread forks while it is open; that is a retry, not this test's answer.
        let mut tries = 0;
        loop {
            match git.run(&["push"], Cap::Small) {
                Err(LandError::Fail(_)) => break,
                Err(LandError::Retry(e)) if e.contains("Text file busy") && tries < 20 => {
                    tries += 1;
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                other => panic!("{other:?}"),
            }
        }
    }

    /// A call that exits 0 without reading all its input is retried, not taken as done.
    #[test]
    fn a_call_that_drops_its_input_is_retried() {
        let config = LandConfig::new("/nonexistent".into(), "gh".into());
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "exec 0<&-; exit 0"]);
        match output_within(
            cmd,
            &vec![b'x'; 1 << 20],
            &config,
            None,
            Cap::Listing,
            "gh pr create",
            &[],
        ) {
            Err(LandError::Retry(e)) => {
                assert!(
                    e.starts_with("gh pr create did not take all its input"),
                    "{e}"
                )
            }
            other => panic!("{other:?}"),
        }
    }

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
