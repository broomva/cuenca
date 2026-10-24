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
//! An owner fork (`forks/owner-<x>/`, held by the volume: [`cuenca_core::is_owner_fork`])
//! is the other way round: the directory is what the owner's devices write through
//! Samba, and the log follows it (spec section 2a). The materialiser shows it once, from
//! the log, writable (folders `2770`, files `0660`, executables `0770`), by the same
//! staged, exclusive rename, and never exchanges it afterwards, so nothing written from
//! Finder is ever moved out. [`Volume::snapshot_owner_forks`] then scans each one with
//! `cuenca-fs` under the location's `.forkignore` (so `.DS_Store`, `._*` and the other
//! defaults never reach a changeset), and publishes what changed as a draft of the fork,
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
//! Forking an owner fork into a checkout (snapshot first, spec section 2a) is not here
//! yet (BRO-3256 part 3).
//!
//! # Trust
//!
//! Blob content comes from the store's object directory and is hashed against its id
//! before it is written ([`cuenca_fs::ObjectDir`]), and trees come from the store, which
//! checks them on every read. Nothing in the volume is trusted beyond that: the
//! read-only trees are the materialiser's alone (their modes keep Samba's account out),
//! owner forks are read as untrusted input (the scan never follows a link out of the
//! fork, and the store hashes every blob it takes), and the volume's own records are
//! trusted only to skip work: a record that names the wrong tree leaves that tree shown
//! until the log moves again, so nothing but the materialiser may write `.cuenca/`. One
//! materialiser per volume is enforced by a lock on `.cuenca/lock`.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use cuenca_core::{ChangesetId, ForkName, Holder, LocationName, Store, Writer, is_owner_fork};
use cuenca_fs::ObjectDir;
use cuenca_tree::GitOid;
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

    /// Whether it is an owner fork, which is shown once and then written from Finder.
    fn is_owner(&self) -> bool {
        matches!(self, Shown::Fork(f) if is_owner_fork(f))
    }

    fn modes(&self) -> Modes {
        if self.is_owner() {
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
            want.push((Shown::Main(location.name), location.main));
        }
        for fork in store.forks()? {
            want.push((Shown::Fork(fork.name), fork.head));
        }
        let mut pass = Pass::default();
        for (what, tree) in want {
            if what.is_owner() && self.owner_shown(&what, tree)? {
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
            match self.show(store, &what, tree) {
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

    /// Materialise `tree` and swap it into `what`'s place.
    fn show(&mut self, store: &Store, what: &Shown, tree: GitOid) -> Result<(), ShowError> {
        let staged = self.staging.join(self.next_staging.to_string());
        self.next_staging += 1;
        let result = self.stage_and_swap(store, what, tree, &staged);
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
        staged: &Path,
    ) -> Result<(), ShowError> {
        let full = store.tree(tree)?;
        cuenca_fs::checkout(&full, staged, &self.objects)?;
        set_modes(staged, what.modes())?;
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
        if what.is_owner() {
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
            if !is_owner_fork(&fork.name) {
                continue;
            }
            let what = Shown::Fork(fork.name.clone());
            let dir = self.root.join(what.rel());
            if !self.shown.contains_key(&what)
                || !fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir())
            {
                continue;
            }
            let cache = self.caches.entry(fork.name.clone()).or_default();
            match snapshot_one(store, &fork, &dir, cache) {
                Ok((published, skipped)) => {
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
                Err(e) => snapped.failed.push((fork.name.clone(), e)),
            }
        }
        Ok(snapped)
    }
}

/// The draft one snapshot published, if any, and what it left out.
type SnapshotOne = (Option<(ChangesetId, u64)>, Vec<cuenca_fs::Skipped>);

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
    if snap.is_clean() {
        return Ok((None, snap.skipped));
    }
    let mut batch = store.blob_batch();
    for blob in &snap.new_blobs {
        let added = match &blob.content {
            cuenca_fs::Content::File(path) => {
                let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
                batch.add(blob.oid, blob.size, file)
            }
            cuenca_fs::Content::Inline(bytes) => batch.add(blob.oid, blob.size, bytes.as_slice()),
        };
        // A file that changed since the scan hashes to another id: refused, and the
        // next pass reads it again.
        added.map_err(|e| e.to_string())?;
    }
    batch.commit().map_err(|e| e.to_string())?;
    let writer = Writer::new(Holder::volume(), fork.epoch());
    let published = store
        .publish_draft(&fork.name, &writer, &snap.changeset)
        .map_err(|e| e.to_string())?;
    Ok((Some((published.id, published.offset)), snap.skipped))
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
