//! The volume: every location's main and every fork's head, materialised from the log
//! as plain directories for Finder to read through stock Samba (spec section 2a).
//!
//! ```text
//! <volume>/main/<location>/   the location's main
//! <volume>/forks/<fork>/      the fork's head: its latest draft, or its base before one
//! <volume>/forks/owner-<x>/   an owner fork: written in place from Finder, snapshotted
//! <volume>/.cuenca/           the materialiser's own state, 0700: staging and records
//! ```
//!
//! [`Volume::converge`] makes the volume match the log: for each location and fork whose
//! tree on disk is not the one the log names, it checks the tree out into
//! `.cuenca/staging/`, sets the modes, flushes it to disk and swaps it into place with
//! one atomic exchange (`RENAME_EXCHANGE` on Linux, `RENAME_SWAP` on macOS), or one
//! rename that refuses to replace anything for a tree shown for the first time. So a
//! reader of the volume sees a tree the log has held, whole, or the next one, whole;
//! never a tree half written. Before the swap it forgets the path's record in
//! `.cuenca/state/`, and only after the swap does it record the new tree, so a
//! materialiser that fails after the swap, or is killed between the swap and the record,
//! rebuilds that path on its next pass (or on restart, after clearing the staging
//! directory) instead of trusting a record for it.
//!
//! [`Materialiser`] runs it on a thread of the authority: once at start, then whenever
//! the log grows, checked every [`VolumeConfig::interval`] (1 s by default; the spec's
//! row is a draft visible within 15 s).
//!
//! # Owner forks
//!
//! An owner fork (`forks/owner-<x>/`, held by the volume: [`Holder::is_volume`]; the
//! materialiser goes by the holder the log names, not by the name)
//! is the other way round: the directory is what the owner's devices write through
//! Samba, and the log follows it (spec section 2a). The materialiser shows it once, from
//! the log, writable (folders `2770`, files `0660`, executables `0770`), by the same
//! staged, exclusive rename, and never exchanges it afterwards, so nothing written from
//! Finder is ever moved out. [`Volume::snapshot_owner_forks`] then scans each one with
//! `cuenca-fs` under the `.forkignore` the fork holds, or the defaults (so `.DS_Store`,
//! `._*` and the other defaults never reach a changeset unless the fork's own rules let
//! them), and publishes what changed as a draft of the fork,
//! written by the volume; the [`Materialiser`] does it every
//! [`VolumeConfig::snapshot_interval`] (10 s by default; the spec's row is 60 s). A file
//! the authority cannot read, or one that keeps changing, fails that fork's snapshot
//! loudly, naming the path, and nothing of it is published until a later pass succeeds.
//!
//! # Modes
//!
//! What is shown is read-only to everyone but the authority's own user: folders `2750`,
//! files `0640` (`0750` for an executable), so Samba's account, a member of the
//! authority's group, can read them and not change them, and no other account can read
//! them (spec section 2a). `main/` and `forks/` are `2750` too. Trees are checked out
//! under `.cuenca/`, which is `0700` and so not setgid, and a rename keeps a file's group:
//! on Linux a shown tree carries the authority's primary group, not the volume's. So the
//! authority must run with Samba's group as its primary group (the deploy, BRO-3257, runs
//! it as `cuenca:cuenca`).
//!
//! # Filesystem
//!
//! The volume needs a filesystem with atomic exchange (`renameat2(RENAME_EXCHANGE)` on
//! Linux: ext4, btrfs, xfs, tmpfs; `renamex_np(RENAME_SWAP)` on APFS) and an exclusive
//! rename (`RENAME_NOREPLACE` / `RENAME_EXCL`). Without exchange, every update of a tree
//! already shown fails, loudly, and is retried; without the exclusive rename, a first
//! show fails too.
//!
//! The volume's directory belongs to the materialiser: whatever is under `main/` and
//! `forks/` at a path the log names is replaced by the log's tree.
//!
//! "Whole" is per lookup: a reader that opens `forks/<fork>/` after a swap sees the new
//! tree, one that opened it before keeps the old one, which is then removed beneath it.
//!
//! Forking an owner fork (spec section 2a) snapshots it first, through
//! [`Volume::snapshot_owner_fork`] with the volume locked from the snapshot to the fork
//! ([`Authority::branch_fork`]): the new fork holds every write the snapshot read, and
//! what Finder writes later stays in the owner fork.
//!
//! A snapshot reads each file as it is at that moment. A file a client is still
//! writing is taken as far as it has been written (as `git add` would take it): the
//! draft, and a fork started from it, hold that partial file, and the owner fork's next
//! snapshot holds the finished one, so nothing is lost. A snapshot that should wait for
//! writes to settle is BRO-3450.
//!
//! # Trust
//!
//! Blob content comes from the store's object directory and is hashed against its id
//! before it is written ([`cuenca_fs::ObjectDir`]), and trees come from the store, which
//! checks them on every read. Nothing in the volume is trusted beyond that: the
//! read-only trees are the materialiser's alone (their modes keep Samba's account out),
//! owner forks are read as untrusted input (the scan never follows a link out of the
//! fork; a file's upload reopens it by path, never through a final link or blocking on
//! a FIFO, and the store hashes every blob it takes against the id the scan found, so
//! anything swapped in meanwhile is refused), and the volume's own records are
//! trusted only to skip work: a record that names the wrong tree leaves that tree shown
//! until the log moves again, so nothing but the materialiser may write `.cuenca/`. One
//! materialiser per volume is enforced by a lock on `.cuenca/lock`.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cuenca_core::{ChangesetId, Fork, ForkName, Holder, LocationName, Store, Writer};
use cuenca_fs::ObjectDir;
use cuenca_tree::{Changeset, Entry, GitOid, Op};
use rustix::fs::{CWD, FlockOperation, RenameFlags};

use crate::Authority;

/// Folders the volume shows: the owner reads and writes, the group reads, setgid so new
/// entries keep the group.
const DIR_MODE: u32 = 0o2750;
/// Files the volume shows.
const FILE_MODE: u32 = 0o640;
/// Executables the volume shows: the owner's execute bit is what a snapshot reads.
const EXEC_MODE: u32 = 0o750;
/// An owner fork's folders, files and executables: Samba's account writes them through
/// the group (spec section 2a).
const OWNER_DIR_MODE: u32 = 0o2770;
const OWNER_FILE_MODE: u32 = 0o660;
const OWNER_EXEC_MODE: u32 = 0o770;
/// How often owner forks are snapshotted ([`VolumeConfig::snapshot_interval`]).
pub const SNAPSHOT_INTERVAL: Duration = Duration::from_secs(10);
/// How long a tree that failed to materialise waits before it is tried again, unless the
/// log moves that path to another tree first ([`VolumeConfig::retry_after`]).
pub const RETRY_AFTER: Duration = Duration::from_secs(30);

/// One thing the volume shows.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Shown {
    /// `main/<location>/`.
    Main(LocationName),
    /// `forks/<fork>/`.
    Fork(ForkName),
}

impl Shown {
    /// Its path under the volume, also its record's path under `.cuenca/state/`.
    fn rel(&self) -> PathBuf {
        match self {
            Shown::Main(l) => Path::new("main").join(l.as_str()),
            Shown::Fork(f) => Path::new("forks").join(f.as_str()),
        }
    }
}

impl Modes {
    /// An owner fork's modes, or a read-only tree's.
    fn of(owner: bool) -> Self {
        if owner {
            Modes {
                dir: OWNER_DIR_MODE,
                file: OWNER_FILE_MODE,
                exec: OWNER_EXEC_MODE,
            }
        } else {
            Modes {
                dir: DIR_MODE,
                file: FILE_MODE,
                exec: EXEC_MODE,
            }
        }
    }
}

/// Whether `fork` is an owner fork, by the holder the log names: shown once, then
/// written from Finder and snapshotted.
fn is_owner(fork: &Fork) -> bool {
    fork.holder().is_some_and(Holder::is_volume)
}

#[derive(Debug, Clone, Copy)]
struct Modes {
    dir: u32,
    file: u32,
    exec: u32,
}

impl std::fmt::Display for Shown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.rel().display())
    }
}

/// What one [`Volume::converge`] did.
#[derive(Debug, Default)]
pub struct Pass {
    /// Trees swapped into place, with the tree each now shows.
    pub built: Vec<(Shown, GitOid)>,
    /// Trees already in place.
    pub unchanged: usize,
    /// Trees that could not be materialised this time, with why. The path may show
    /// either its previous tree or the new one, and is rebuilt on a later pass.
    pub failed: Vec<(Shown, String)>,
}

/// What one [`Volume::snapshot_owner_forks`] did.
#[derive(Debug, Default)]
pub struct Snapped {
    /// Owner forks whose edits were published, with the draft and its record's offset.
    pub published: Vec<(ForkName, ChangesetId, u64)>,
    /// Owner forks whose directory holds exactly their head.
    pub clean: usize,
    /// Owner forks whose snapshot failed (an unreadable file, a file that kept changing,
    /// a tracked path the rules now name), with why, naming the path. Nothing of them
    /// was published; the next pass tries again.
    pub failed: Vec<(ForkName, String)>,
    /// Entries left out that are not content (sockets, FIFOs, devices) or whose name
    /// git reserves: `(fork, path, why)`.
    pub skipped: Vec<(ForkName, String, &'static str)>,
}

/// The volume's directory and what it shows.
pub struct Volume {
    root: PathBuf,
    objects: ObjectDir,
    staging: PathBuf,
    state: PathBuf,
    /// What each path shows, as recorded in `.cuenca/state/`.
    shown: HashMap<Shown, GitOid>,
    /// Trees that failed, and when; tried again after `retry_after`.
    failed: HashMap<Shown, (GitOid, Instant)>,
    retry_after: Duration,
    /// Each owner fork's stat cache, so a snapshot reads only files that changed.
    caches: HashMap<ForkName, cuenca_fs::StatCache>,
    /// The lock that keeps this the volume's only materialiser, held while it lives.
    _lock: File,
    next_staging: u64,
}

impl Volume {
    /// Open the volume at `root` (created if missing) for a store whose blobs are in
    /// `objects` ([`Store::object_dir`]). Fails if another materialiser has it open.
    /// Leftovers of a materialiser that was killed are removed: what it was staging was
    /// never shown.
    pub fn open(root: &Path, objects: &Path) -> io::Result<Self> {
        fs::create_dir_all(root)?;
        fs::set_permissions(root, fs::Permissions::from_mode(DIR_MODE))?;
        let private = root.join(".cuenca");
        fs::create_dir_all(&private)?;
        fs::set_permissions(&private, fs::Permissions::from_mode(0o700))?;
        let lock = File::create(private.join("lock"))?;
        rustix::fs::flock(&lock, FlockOperation::NonBlockingLockExclusive).map_err(|e| {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "{}: another materialiser has this volume open ({e})",
                    root.display()
                ),
            )
        })?;
        let staging = private.join("staging");
        if staging.exists() {
            fs::remove_dir_all(&staging)?;
        }
        fs::create_dir_all(&staging)?;
        let state = private.join("state");
        for dir in ["main", "forks"] {
            fs::create_dir_all(state.join(dir))?;
            let shown = root.join(dir);
            fs::create_dir_all(&shown)?;
            fs::set_permissions(&shown, fs::Permissions::from_mode(DIR_MODE))?;
        }
        let shown = read_records(&state)?;
        Ok(Self {
            root: root.to_path_buf(),
            objects: ObjectDir::new(objects),
            staging,
            state,
            shown,
            failed: HashMap::new(),
            retry_after: RETRY_AFTER,
            caches: HashMap::new(),
            _lock: lock,
            next_staging: 0,
        })
    }

    /// The volume's directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The tree `what` shows now, as the volume records it.
    pub fn shown(&self, what: &Shown) -> Option<GitOid> {
        self.shown.get(what).copied()
    }

    /// How long a tree that failed waits before it is tried again ([`RETRY_AFTER`] by
    /// default).
    pub fn set_retry_after(&mut self, wait: Duration) {
        self.retry_after = wait;
    }

    /// Whether a tree that failed is due to be tried again.
    pub fn retry_due(&self) -> bool {
        self.failed
            .values()
            .any(|(_, at)| at.elapsed() >= self.retry_after)
    }

    /// Make the volume show what the log holds now: each location's main and each
    /// fork's head, and each owner fork once. One failure (a missing blob, a full disk)
    /// does not stop the others; the path may show either its previous tree or the new
    /// one, and is rebuilt on a later pass.
    pub fn converge(&mut self, store: &Store) -> cuenca_core::Result<Pass> {
        let mut want = Vec::new();
        for location in store.locations()? {
            want.push((Shown::Main(location.name), location.main, false));
        }
        for fork in store.forks()? {
            let owner = is_owner(&fork);
            want.push((Shown::Fork(fork.name), fork.head, owner));
        }
        let mut pass = Pass::default();
        for (what, tree, owner) in want {
            if owner && self.owner_shown(&what, tree)? {
                pass.unchanged += 1;
                continue;
            }
            if self.shown.get(&what) == Some(&tree)
                && fs::symlink_metadata(self.root.join(what.rel())).is_ok_and(|m| m.is_dir())
            {
                self.failed.remove(&what);
                pass.unchanged += 1;
                continue;
            }
            if let Some((failed, at)) = self.failed.get(&what)
                && *failed == tree
                && at.elapsed() < self.retry_after
            {
                continue;
            }
            match self.show(store, &what, tree, owner) {
                Ok(()) => {
                    self.failed.remove(&what);
                    pass.built.push((what, tree));
                }
                Err(e) => {
                    self.failed.insert(what.clone(), (tree, Instant::now()));
                    pass.failed.push((what, e.to_string()));
                }
            }
        }
        Ok(pass)
    }

    /// Whether the owner fork `what` is shown already, so it is left alone: its
    /// directory is what Finder writes, never replaced. A directory there without a
    /// record (the materialiser was killed between its rename and its record) is the
    /// tree it showed, written on since: adopted, never rebuilt.
    fn owner_shown(&mut self, what: &Shown, tree: GitOid) -> io::Result<bool> {
        let dir = fs::symlink_metadata(self.root.join(what.rel())).is_ok_and(|m| m.is_dir());
        if !dir {
            return Ok(false);
        }
        if !self.shown.contains_key(what) {
            write_record(&self.state.join(what.rel()), tree)?;
            self.shown.insert(what.clone(), tree);
        }
        self.failed.remove(what);
        Ok(true)
    }

    /// Materialise `tree` and swap it into `what`'s place (put it there, for an owner
    /// fork, which is never exchanged).
    fn show(
        &mut self,
        store: &Store,
        what: &Shown,
        tree: GitOid,
        owner: bool,
    ) -> Result<(), ShowError> {
        let staged = self.staging.join(self.next_staging.to_string());
        self.next_staging += 1;
        let result = self.stage_and_swap(store, what, tree, owner, &staged);
        // What is left at the staging path is the tree that was shown before (after a
        // swap) or a tree never shown (after a failure): either way, nobody's.
        if let Err(e) = remove_any(&staged) {
            tracing::warn!(path = %staged.display(), error = %e, "could not remove a staged tree");
        }
        result
    }

    fn stage_and_swap(
        &mut self,
        store: &Store,
        what: &Shown,
        tree: GitOid,
        owner: bool,
        staged: &Path,
    ) -> Result<(), ShowError> {
        let full = store.tree(tree)?;
        cuenca_fs::checkout(&full, staged, &self.objects)?;
        set_modes(staged, Modes::of(owner))?;
        flush(&self.staging)?;
        let dest = self.root.join(what.rel());
        let record = self.state.join(what.rel());
        // Forget what the path shows before changing it: if anything after the swap
        // fails, or the process dies between the swap and the record, the path is
        // rebuilt on the next pass, never believed to show a tree it may not. (Only a
        // kill between the swap and a forget placed after it would tell the two orders
        // apart; no test kills at that point.)
        self.shown.remove(what);
        forget_record(&record)?;
        if owner {
            // Shown once, never exchanged: an owner fork already there is Finder's.
            rename_new(staged, &dest)?;
        } else {
            swap_into_place(staged, &dest)?;
        }
        File::open(dest.parent().expect("main/ or forks/"))?.sync_all()?;
        write_record(&record, tree)?;
        self.shown.insert(what.clone(), tree);
        Ok(())
    }
}

impl Volume {
    /// Snapshot every owner fork the volume shows and publish what changed in each as a
    /// draft written by the volume (spec section 2a: "snapshotted by the authority from
    /// its tree, within 60 seconds of a change, into a draft changeset"). One fork's
    /// failure does not stop the others.
    pub fn snapshot_owner_forks(&mut self, store: &Store) -> cuenca_core::Result<Snapped> {
        let mut snapped = Snapped::default();
        for fork in store.forks()? {
            if !is_owner(&fork) {
                continue;
            }
            match self.snapshot_owner_fork(store, &fork) {
                None => {}
                Some(Ok((published, skipped))) => {
                    match published {
                        Some((id, offset)) => {
                            snapped.published.push((fork.name.clone(), id, offset))
                        }
                        None => snapped.clean += 1,
                    }
                    snapped.skipped.extend(
                        skipped
                            .into_iter()
                            .map(|s| (fork.name.clone(), s.path, s.reason)),
                    );
                }
                Some(Err(e)) => snapped.failed.push((fork.name.clone(), e)),
            }
        }
        Ok(snapped)
    }

    /// Snapshot one owner fork, as [`Volume::snapshot_owner_forks`] does each: `None`
    /// when the volume does not show it (nothing was written to it here, so the log's
    /// head is all of it), else what was published and what was left out, or why the
    /// snapshot failed (nothing of it was published).
    pub fn snapshot_owner_fork(
        &mut self,
        store: &Store,
        fork: &cuenca_core::Fork,
    ) -> Option<Result<SnapshotOne, String>> {
        let what = Shown::Fork(fork.name.clone());
        let dir = self.root.join(what.rel());
        if !is_owner(fork)
            || !self.shown.contains_key(&what)
            || !fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir())
        {
            return None;
        }
        let cache = self.caches.entry(fork.name.clone()).or_default();
        Some(snapshot_one(store, fork, &dir, cache))
    }
}

/// The draft one snapshot published, if any, and what it left out.
pub type SnapshotOne = (Option<(ChangesetId, u64)>, Vec<cuenca_fs::Skipped>);

/// Snapshot one owner fork's directory against its head; publish the changes, if any,
/// as the volume under the fork's epoch.
fn snapshot_one(
    store: &Store,
    fork: &cuenca_core::Fork,
    dir: &Path,
    cache: &mut cuenca_fs::StatCache,
) -> Result<SnapshotOne, String> {
    let head = store.tree(fork.head).map_err(|e| e.to_string())?;
    let snap = cuenca_fs::snapshot(dir, &head, cache).map_err(|e| e.to_string())?;
    stage_and_publish(store, fork, &head, snap, dir)
}

/// Stage the blobs a snapshot of `dir` against `head` names and publish its changeset.
/// Each put whose blob is new and not yet staged is staged from its own path
/// ([`stage_put`]: a symlink from its text), so a blob several paths share is staged as
/// long as one of them still gives it. A file that changed since the scan, and whose
/// blob was not staged already, is taken as it is now (a write in progress, as far as
/// it has gone): its put names what it holds now, and is left out if that is what the
/// head holds; one whose blob another path staged keeps the scanned version, its change
/// a later write for the next snapshot. A `.forkignore` that changed fails the snapshot.
/// No other op changes, so a rename or a deletion is never split.
fn stage_and_publish(
    store: &Store,
    fork: &cuenca_core::Fork,
    head: &cuenca_tree::Tree,
    snap: cuenca_fs::Snapshot,
    dir: &Path,
) -> Result<SnapshotOne, String> {
    if snap.is_clean() {
        return Ok((None, snap.skipped));
    }
    // Each new blob, with where the scan listed it (one path per blob, whichever it met
    // first, or a symlink's text).
    let listed: HashMap<GitOid, (u64, &cuenca_fs::Content)> = snap
        .new_blobs
        .iter()
        .map(|b| (b.oid, (b.size, &b.content)))
        .collect();
    let mut batch = store.blob_batch();
    let mut staged: HashSet<GitOid> = HashSet::new();
    let mut ops = Vec::with_capacity(snap.changeset.ops.len());
    let mut rewritten = false;
    for op in &snap.changeset.ops {
        let Op::Put { path, entry } = op else {
            ops.push(op.clone());
            continue;
        };
        let oid = entry.oid();
        let Some(&(size, source)) = listed.get(&oid) else {
            // The head has it already.
            ops.push(op.clone());
            continue;
        };
        if staged.contains(&oid) {
            ops.push(op.clone());
            continue;
        }
        let now = stage_put(&mut batch, &dir.join(path.as_str()), entry, size, source)?;
        staged.insert(now);
        // The scan followed the rules it read; rules changed since would be published
        // beside paths they name. Snapshot again instead.
        if now != oid && path.as_str() == cuenca_fs::FORKIGNORE {
            return Err(format!(
                "{}: changed while the fork was snapshotted; snapshot again",
                cuenca_fs::FORKIGNORE
            ));
        }
        if now == oid {
            ops.push(op.clone());
            continue;
        }
        rewritten = true;
        let entry = match entry {
            Entry::Executable(_) => Entry::Executable(now),
            Entry::Symlink(_) => Entry::Symlink(now),
            _ => Entry::File(now),
        };
        if head.get(path.as_str()) != Some(&entry) {
            ops.push(Op::put(path.clone(), entry));
        }
    }
    let changeset = if rewritten {
        Changeset::new(head, ops).map_err(|e| e.to_string())?
    } else {
        snap.changeset
    };
    if changeset.ops.is_empty() {
        return Ok((None, snap.skipped));
    }
    batch.commit().map_err(|e| e.to_string())?;
    let writer = Writer::new(Holder::volume(), fork.epoch());
    let published = store
        .publish_draft(&fork.name, &writer, &changeset)
        .map_err(|e| e.to_string())?;
    Ok((Some((published.id, published.offset)), snap.skipped))
}

/// Stage the blob of a put at `path` that the scan read as `entry` (`size` bytes),
/// from the path itself: a symlink from its text, read again (never followed), a file
/// by [`upload_file`] (as it is now, if it changed). If the path no longer gives it (gone
/// or renamed since the scan, or no longer of its kind), the blob is staged under its
/// scanned id from where the scan listed it, `source`, which may be another path
/// holding the same bytes; if neither gives it, the path's own error is returned. The
/// blob id staged.
fn stage_put(
    batch: &mut cuenca_core::BlobBatch<'_>,
    path: &Path,
    entry: &Entry,
    size: u64,
    source: &cuenca_fs::Content,
) -> Result<GitOid, String> {
    use std::os::unix::ffi::OsStrExt;
    let oid = entry.oid();
    let own = match entry {
        Entry::Symlink(_) => fs::read_link(path)
            .map_err(|e| format!("{}: {e}", path.display()))
            .and_then(|target| {
                let text = target.as_os_str().as_bytes();
                let now = GitOid::for_blob(text);
                batch
                    .add(now, text.len() as u64, text)
                    .map(|_| now)
                    .map_err(|e| format!("{}: {e}", path.display()))
            }),
        _ => upload_file(batch, path, oid, size),
    };
    let Err(error) = own else {
        return own;
    };
    let listed = match source {
        cuenca_fs::Content::Inline(bytes) => batch.add(oid, size, bytes.as_slice()).is_ok(),
        cuenca_fs::Content::File(listed) => open_regular(listed)
            .ok()
            .is_some_and(|file| batch.add(oid, size, file).is_ok()),
    };
    if listed { Ok(oid) } else { Err(error) }
}

/// How many times a file that keeps changing between its hash and its upload is read
/// again before the snapshot fails.
const UPLOAD_ATTEMPTS: u32 = 5;

/// Stage the file at `path`, which the scan found holding blob `oid` of `size` bytes:
/// if it has changed since (the store hashes what it takes, and refuses a mismatch), hash
/// what it holds now and stage that instead. The blob id staged. A file that keeps
/// changing fails, naming it.
fn upload_file(
    batch: &mut cuenca_core::BlobBatch<'_>,
    path: &Path,
    mut oid: GitOid,
    mut size: u64,
) -> Result<GitOid, String> {
    use cuenca_core::CoreError;
    for _ in 0..UPLOAD_ATTEMPTS {
        match batch.add(oid, size, open_regular(path)?) {
            Ok(_) => return Ok(oid),
            Err(CoreError::OidMismatch { .. } | CoreError::Hash(_)) => {
                // Changed since; a file still changing while it is hashed is tried
                // again.
                if let Some(now) = hash_file(path)? {
                    (oid, size) = now;
                }
            }
            Err(e) => return Err(format!("{}: {e}", path.display())),
        }
    }
    Err(format!(
        "{}: kept changing while it was uploaded; snapshot again",
        path.display()
    ))
}

/// The blob id and size of what the file at `path` holds now, or `None` if its size
/// changed while it was read.
fn hash_file(path: &Path) -> Result<Option<(GitOid, u64)>, String> {
    use std::io::Read as _;
    let fail = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());
    let file = open_regular(path)?;
    let size = file.metadata().map_err(|e| fail(&e))?.len();
    let mut hasher = cuenca_tree::BlobHasher::new(size);
    let mut buf = vec![0; 64 * 1024];
    let mut file = file.take(size + 1);
    loop {
        let n = file.read(&mut buf).map_err(|e| fail(&e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    match hasher.finish() {
        Ok(blob) => Ok(Some((blob.oid(), size))),
        Err(cuenca_tree::HashFailure::WrongLength { .. }) => Ok(None),
        Err(e) => Err(fail(&e)),
    }
}

/// Open a file the scan found, again by its path, for its upload: never through a
/// final symlink and never blocking on a FIFO, and only if it is a regular file. A
/// file swapped since the scan by anything else (a folder above it replaced by a link
/// included) is read only to be refused, since the store hashes what it takes against
/// the id the scan found.
fn open_regular(path: &Path) -> Result<File, String> {
    use std::os::unix::fs::OpenOptionsExt;
    let flags = rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK;
    let fail = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(flags.bits() as i32)
        .open(path)
        .map_err(|e| fail(&e))?;
    if !file.metadata().map_err(|e| fail(&e))?.is_file() {
        return Err(fail(&"no longer a regular file; snapshot again"));
    }
    Ok(file)
}

#[derive(Debug, thiserror::Error)]
enum ShowError {
    #[error(transparent)]
    Core(#[from] cuenca_core::CoreError),
    #[error(transparent)]
    Fs(#[from] cuenca_fs::FsError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Put `staged` at `dest`: exchanged with what is there, atomically, or renamed there if
/// nothing is, refusing to replace anything that appeared meanwhile. After an exchange,
/// what was at `dest` is at `staged`.
fn swap_into_place(staged: &Path, dest: &Path) -> io::Result<()> {
    match rustix::fs::renameat_with(CWD, staged, CWD, dest, RenameFlags::EXCHANGE) {
        Ok(()) => Ok(()),
        Err(rustix::io::Errno::NOENT) if !dest.exists() => {
            rustix::fs::renameat_with(CWD, staged, CWD, dest, RenameFlags::NOREPLACE)
                .map_err(io::Error::from)
        }
        Err(e) => Err(io::Error::from(e)),
    }
}

/// Put `staged` at `dest`, which must not exist: one rename that refuses to replace
/// anything.
fn rename_new(staged: &Path, dest: &Path) -> io::Result<()> {
    rustix::fs::renameat_with(CWD, staged, CWD, dest, RenameFlags::NOREPLACE)
        .map_err(io::Error::from)
}

/// Folders, files and executables to `modes`; symlinks as they are. Only ever run on a
/// staged tree, which no one else can reach.
fn set_modes(dir: &Path, modes: Modes) -> io::Result<()> {
    fs::set_permissions(dir, fs::Permissions::from_mode(modes.dir))?;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let path = entry.path();
        if kind.is_dir() {
            set_modes(&path, modes)?;
        } else if kind.is_file() {
            let exec = entry.metadata()?.permissions().mode() & 0o100 != 0;
            let mode = if exec { modes.exec } else { modes.file };
            fs::set_permissions(&path, fs::Permissions::from_mode(mode))?;
        }
    }
    Ok(())
}

/// Make what was staged durable before it is shown, so on Linux (`syncfs`) a power loss
/// after the swap cannot leave a shown tree with torn files. Elsewhere (`sync`, which
/// only schedules the writes) this is best effort; the deploy is Linux.
fn flush(staging: &Path) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        rustix::fs::syncfs(File::open(staging)?).map_err(io::Error::from)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = staging;
        rustix::fs::sync();
        Ok(())
    }
}

/// Remove a file or a directory tree, if there is one.
fn remove_any(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Remove the record at `path`, if there is one, durably.
fn forget_record(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => File::open(path.parent().expect("state/main or state/forks"))?.sync_all(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Record that `path`'s tree is `tree`: written beside it and renamed over it. The
/// temporary name starts with a dot, which no location or fork name can, so it is
/// never another record.
fn write_record(path: &Path, tree: GitOid) -> io::Result<()> {
    let name = path
        .file_name()
        .expect("a record has a name")
        .to_string_lossy();
    let tmp = path.with_file_name(format!(".{name}.tmp"));
    fs::write(&tmp, tree.to_hex())?;
    File::open(&tmp)?.sync_all()?;
    fs::rename(&tmp, path)?;
    File::open(path.parent().expect("state/main or state/forks"))?.sync_all()
}

/// The records under `state`. A record that does not parse is dropped: its tree is
/// materialised again.
fn read_records(state: &Path) -> io::Result<HashMap<Shown, GitOid>> {
    let mut shown = HashMap::new();
    for (dir, make) in [
        (
            "main",
            (|s: &str| LocationName::new(s).ok().map(Shown::Main)) as fn(&str) -> Option<Shown>,
        ),
        ("forks", |s: &str| ForkName::new(s).ok().map(Shown::Fork)),
    ] {
        for entry in fs::read_dir(state.join(dir))? {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let Some(what) = make(&name) else {
                continue;
            };
            let Ok(text) = fs::read_to_string(entry.path()) else {
                continue;
            };
            if let Ok(oid) = GitOid::from_hex(text.trim()) {
                shown.insert(what, oid);
            }
        }
    }
    Ok(shown)
}

/// Where the authority keeps its volume.
#[derive(Debug, Clone)]
pub struct VolumeConfig {
    pub root: PathBuf,
    /// How often the log is checked for growth.
    pub interval: Duration,
    /// How long a tree that failed waits before it is tried again.
    pub retry_after: Duration,
    /// How often owner forks are snapshotted.
    pub snapshot_interval: Duration,
}

impl VolumeConfig {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            interval: Duration::from_secs(1),
            retry_after: RETRY_AFTER,
            snapshot_interval: SNAPSHOT_INTERVAL,
        }
    }
}

/// The materialiser's thread. Dropping it stops the thread.
pub struct Materialiser {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Materialiser {
    /// Open the volume and start converging it: now, then each time the log grows; and
    /// snapshot its owner forks, now and every [`VolumeConfig::snapshot_interval`]. A
    /// pass that panics is reported and the loop goes on, so the volume does not stop
    /// following the log quietly.
    pub fn spawn(authority: Arc<Authority>, config: VolumeConfig) -> io::Result<Self> {
        let mut volume = Volume::open(&config.root, authority.store.object_dir())?;
        volume.set_retry_after(config.retry_after);
        let volume = Arc::new(Mutex::new(volume));
        // Forking an owner fork snapshots it under the same lock ([`Authority::branch_fork`]).
        authority.set_volume(volume.clone())?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("cuenca-materialiser".into())
                .spawn(move || {
                    let mut seen = None;
                    let mut snapped_at: Option<Instant> = None;
                    while !stop.load(Ordering::SeqCst) {
                        let ran = catch_unwind(AssertUnwindSafe(|| {
                            let mut volume = Authority::lock_volume(&volume);
                            if snapped_at.is_none_or(|t| t.elapsed() >= config.snapshot_interval) {
                                snapped_at = Some(Instant::now());
                                match volume.snapshot_owner_forks(&authority.store) {
                                    Ok(snapped) => report_snapped(&snapped),
                                    Err(e) => {
                                        tracing::error!(error = %e, "owner fork snapshots failed")
                                    }
                                }
                            }
                            let len = authority.store.len().ok();
                            if len.is_none() || len != seen || volume.retry_due() {
                                match volume.converge(&authority.store) {
                                    Ok(pass) => {
                                        report(&pass);
                                        seen = len;
                                    }
                                    Err(e) => {
                                        tracing::warn!(error = %e, "materialiser pass failed")
                                    }
                                }
                            }
                        }));
                        if ran.is_err() {
                            tracing::error!("a materialiser pass panicked; trying again");
                            seen = None;
                        }
                        std::thread::sleep(config.interval);
                    }
                })?
        };
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

fn report_snapped(snapped: &Snapped) {
    for (fork, id, offset) in &snapped.published {
        tracing::info!(%fork, changeset = %id, offset, "owner fork snapshotted");
    }
    for (fork, path, why) in &snapped.skipped {
        tracing::warn!(%fork, path, why, "left out of an owner fork's snapshot");
    }
    for (fork, error) in &snapped.failed {
        tracing::error!(%fork, error, "owner fork snapshot failed; nothing of it was published");
    }
}

fn report(pass: &Pass) {
    for (what, tree) in &pass.built {
        tracing::info!(path = %what, tree = %tree.to_hex(), "materialised");
    }
    for (what, error) in &pass.failed {
        tracing::warn!(path = %what, error, "could not materialise");
    }
}

impl Drop for Materialiser {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take()
            && t.join().is_err()
        {
            tracing::error!("the materialiser thread panicked");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cuenca_tree::{Entry, RepoPath, Tree};

    /// Below `converge`, which never shows an owner fork that is there already: if it
    /// ever did, the folder Finder writes is refused, never exchanged out of place.
    #[test]
    fn an_owner_fork_is_never_exchanged() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("store")).unwrap();
        let oid = GitOid::for_blob(b"main\n");
        store.ingest_blob(oid, 5, &b"main\n"[..]).unwrap();
        let main =
            Tree::from_entries([(RepoPath::new("README").unwrap(), Entry::File(oid))]).unwrap();
        let site = LocationName::new("site").unwrap();
        store.add_location(&site, None, &main).unwrap();
        let owner = ForkName::new("owner-mac").unwrap();
        store.create_fork(&owner, &site, &Holder::volume()).unwrap();
        let root = dir.path().join("volume");
        let mut volume = Volume::open(&root, store.object_dir()).unwrap();
        let shown = root.join("forks/owner-mac");
        fs::create_dir(&shown).unwrap();
        fs::write(shown.join("edit.md"), b"from finder\n").unwrap();

        let what = Shown::Fork(owner);
        assert!(
            volume
                .show(&store, &what, main.git_tree_id(), true)
                .is_err()
        );
        assert_eq!(fs::read(shown.join("edit.md")).unwrap(), b"from finder\n");
        assert!(!shown.join("README").exists());
        assert!(fs::read_dir(&volume.staging).unwrap().next().is_none());
    }

    /// A store with location `site` (main: `README`), its owner fork `owner-mac`, and a
    /// folder holding main, for the snapshot tests below.
    fn owner_dir() -> (tempfile::TempDir, Store, cuenca_core::Fork, Tree) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("store")).unwrap();
        let oid = GitOid::for_blob(b"main\n");
        store.ingest_blob(oid, 5, &b"main\n"[..]).unwrap();
        let main =
            Tree::from_entries([(RepoPath::new("README").unwrap(), Entry::File(oid))]).unwrap();
        let site = LocationName::new("site").unwrap();
        store.add_location(&site, None, &main).unwrap();
        let owner = ForkName::new("owner-mac").unwrap();
        store.create_fork(&owner, &site, &Holder::volume()).unwrap();
        fs::create_dir(dir.path().join("fork")).unwrap();
        fs::write(dir.path().join("fork/README"), b"main\n").unwrap();
        let fork = store.fork(&owner).unwrap().unwrap();
        (dir, store, fork, main)
    }

    fn scan(dir: &Path, head: &Tree) -> cuenca_fs::Snapshot {
        cuenca_fs::snapshot(dir, head, &mut cuenca_fs::StatCache::new()).unwrap()
    }

    /// Two new files with the same content, both changed before the upload (so the
    /// path the scan listed the blob under no longer holds it): each is taken as it is
    /// now, staged from its own path, and nothing is refused for the blob neither holds.
    #[test]
    fn a_blob_two_paths_share_is_staged_from_each_path() {
        let (d, store, fork, head) = owner_dir();
        let root = d.path().join("fork");
        fs::write(root.join("a.md"), b"same\n").unwrap();
        fs::write(root.join("b.md"), b"same\n").unwrap();
        let snap = scan(&root, &head);
        assert_eq!(snap.new_blobs.len(), 1, "one blob for two paths");
        fs::write(root.join("a.md"), b"A\n").unwrap();
        fs::write(root.join("b.md"), b"B\n").unwrap();
        let (published, _) = stage_and_publish(&store, &fork, &head, snap, &root).unwrap();
        assert!(published.is_some());
        let now = store.fork_head(&fork.name).unwrap();
        let blob = |c: &[u8]| Some(Entry::File(GitOid::for_blob(c)));
        assert_eq!(now.get("a.md").copied(), blob(b"A\n"));
        assert_eq!(now.get("b.md").copied(), blob(b"B\n"));
    }

    /// A symlink whose text is the content of new files the scan met first (so the blob
    /// is listed under a file): the link's put is staged from its text, never by opening
    /// the link as a file.
    #[test]
    fn a_symlink_sharing_a_files_bytes_is_staged_from_its_text() {
        let (d, store, fork, head) = owner_dir();
        let root = d.path().join("fork");
        for i in 0..70 {
            fs::write(root.join(format!("m{i:02}")), b"target").unwrap();
        }
        fs::create_dir(root.join("a")).unwrap();
        std::os::unix::fs::symlink("target", root.join("a/link")).unwrap();
        let snap = scan(&root, &head);
        // The files change before the upload, so none of them still holds the link's
        // bytes: only the link itself gives them.
        for i in 0..70 {
            fs::write(root.join(format!("m{i:02}")), b"edited").unwrap();
        }
        let (published, _) = stage_and_publish(&store, &fork, &head, snap, &root).unwrap();
        assert!(published.is_some());
        let now = store.fork_head(&fork.name).unwrap();
        let oid = GitOid::for_blob(b"target");
        assert_eq!(now.get("a/link").copied(), Some(Entry::Symlink(oid)));
        // The files share the blob the link staged, so each is as the scan read it (a
        // later write, for the next snapshot) or as it is now.
        let m00 = now.get("m00").copied();
        assert!(
            m00 == Some(Entry::File(oid)) || m00 == Some(Entry::File(GitOid::for_blob(b"edited"))),
            "{m00:?}"
        );
    }

    /// A path deleted between the scan and the upload whose blob another path still holds
    /// is staged from that path, under the id the scan read; nothing is refused.
    #[test]
    fn a_path_gone_since_the_scan_is_staged_from_another_holding_its_blob() {
        let (d, store, fork, head) = owner_dir();
        let root = d.path().join("fork");
        fs::write(root.join("a.md"), b"same\n").unwrap();
        fs::write(root.join("b.md"), b"same\n").unwrap();
        let snap = scan(&root, &head);
        fs::remove_file(root.join("a.md")).unwrap();
        let (published, _) = stage_and_publish(&store, &fork, &head, snap, &root).unwrap();
        assert!(published.is_some());
        let now = store.fork_head(&fork.name).unwrap();
        let same = Some(Entry::File(GitOid::for_blob(b"same\n")));
        assert_eq!(now.get("a.md").copied(), same);
        assert_eq!(now.get("b.md").copied(), same);
    }

    /// An executable changed after the scan keeps its mode in the put that names what it
    /// holds now.
    #[test]
    fn an_executable_changed_after_the_scan_stays_executable() {
        let (d, store, fork, head) = owner_dir();
        let root = d.path().join("fork");
        fs::write(root.join("run"), b"#!/bin/sh\n").unwrap();
        fs::set_permissions(root.join("run"), fs::Permissions::from_mode(0o755)).unwrap();
        let snap = scan(&root, &head);
        fs::write(root.join("run"), b"#!/bin/sh\nexit 0\n").unwrap();
        stage_and_publish(&store, &fork, &head, snap, &root).unwrap();
        let now = store.fork_head(&fork.name).unwrap();
        assert_eq!(
            now.get("run").copied(),
            Some(Entry::Executable(GitOid::for_blob(b"#!/bin/sh\nexit 0\n")))
        );
    }

    /// A `.forkignore` changed between the scan and the upload fails the snapshot: the
    /// scan followed the old rules, and the new ones could name paths it holds.
    #[test]
    fn rules_changed_after_the_scan_fail_the_snapshot() {
        let (d, store, fork, head) = owner_dir();
        let root = d.path().join("fork");
        fs::write(root.join(".forkignore"), b"*.tmp\n").unwrap();
        fs::write(root.join("secret.env"), b"S=1\n").unwrap();
        let snap = scan(&root, &head);
        fs::write(root.join(".forkignore"), b"*.tmp\nsecret.env\n").unwrap();
        let len = store.len().unwrap();
        let err = stage_and_publish(&store, &fork, &head, snap, &root).unwrap_err();
        assert!(err.contains(".forkignore"), "{err}");
        assert_eq!(store.len().unwrap(), len);
    }

    /// A file changed after the scan is published as it is now, not as the scan read it;
    /// one changed back to what the head holds is no change at all.
    #[test]
    fn a_file_changed_after_the_scan_is_taken_as_it_is_now() {
        let (d, store, fork, head) = owner_dir();
        let root = d.path().join("fork");
        fs::write(root.join("c.md"), b"one\n").unwrap();
        let snap = scan(&root, &head);
        fs::write(root.join("c.md"), b"two\n").unwrap();
        stage_and_publish(&store, &fork, &head, snap, &root).unwrap();
        let now = store.fork_head(&fork.name).unwrap();
        assert_eq!(
            now.get("c.md").copied(),
            Some(Entry::File(GitOid::for_blob(b"two\n")))
        );

        let fork = store.fork(&fork.name).unwrap().unwrap();
        fs::write(root.join("README"), b"edited\n").unwrap();
        let snap = scan(&root, &now);
        fs::write(root.join("README"), b"main\n").unwrap();
        let (published, _) = stage_and_publish(&store, &fork, &now, snap, &root).unwrap();
        assert_eq!(published, None, "a put of what the head holds");
    }

    /// A file that changed between the scan and its upload is staged as it is now,
    /// under its new id, never refused for the old one; a file that keeps changing (here,
    /// one that is gone) fails, naming it.
    #[test]
    fn an_upload_takes_what_the_file_holds_now() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("store")).unwrap();
        let file = dir.path().join("f");
        fs::write(&file, b"written later\n").unwrap();
        let scanned = GitOid::for_blob(b"scan\n");
        let mut batch = store.blob_batch();
        let got = upload_file(&mut batch, &file, scanned, 5).unwrap();
        assert_eq!(got, GitOid::for_blob(b"written later\n"));
        batch.commit().unwrap();
        assert_eq!(store.read_blob(got).unwrap(), b"written later\n");
        let mut batch = store.blob_batch();
        let err = upload_file(&mut batch, &dir.path().join("gone"), scanned, 5).unwrap_err();
        assert!(err.contains("gone"), "{err}");
    }

    /// An upload reopens only a regular file: a symlink or a FIFO swapped in since the
    /// scan is refused at once, never followed and never waited on.
    #[test]
    fn an_upload_opens_only_a_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        fs::write(&file, b"x").unwrap();
        assert!(open_regular(&file).is_ok());
        let link = dir.path().join("l");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(open_regular(&link).is_err(), "followed a symlink");
        let fifo = dir.path().join("p");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(made.success());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || tx.send(open_regular(&fifo).is_err()).unwrap());
        assert!(
            rx.recv_timeout(Duration::from_secs(5))
                .expect("blocked on a FIFO"),
            "opened a FIFO"
        );
    }
}
