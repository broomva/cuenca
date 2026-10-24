//! The volume: every location's main and every fork's head, materialised from the log
//! as plain directories for Finder to read through stock Samba (spec section 2a).
//!
//! ```text
//! <volume>/main/<location>/   the location's main
//! <volume>/forks/<fork>/      the fork's head: its latest draft, or its base before one
//! <volume>/.cuenca/           the materialiser's own state, 0700: staging and records
//! ```
//!
//! [`Volume::converge`] makes the volume match the log: for each location and fork whose
//! tree on disk is not the one the log names, it checks the tree out into
//! `.cuenca/staging/`, sets the modes, flushes it to disk and swaps it into place with
//! one atomic exchange (`RENAME_EXCHANGE` on Linux, `RENAME_SWAP` on macOS), or one
//! rename that refuses to replace anything for a tree shown for the first time. So a
//! reader of the volume sees a tree the log has held, whole, or the next one, whole;
//! never a tree half written. Only after the swap does it record which tree is shown, in
//! `.cuenca/state/`, so a materialiser killed at any point redoes at most the tree it was
//! on: on restart it clears the staging directory and converges again.
//!
//! [`Materialiser`] runs it on a thread of the authority: once at start, then whenever
//! the log grows, checked every [`VolumeConfig::interval`] (1 s by default; the spec's
//! row is a draft visible within 15 s).
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
//! Linux: ext4, btrfs, xfs, tmpfs; `renamex_np(RENAME_SWAP)` on APFS). Elsewhere every
//! update of a tree already shown fails, loudly, and is retried.
//!
//! # Not here yet
//!
//! Owner forks, `forks/owner-<name>/`, written in place from Finder and snapshotted by
//! the authority, are the second half of BRO-3256. Until then every fork is shown
//! read-only from the log, whatever its name.
//!
//! # Trust
//!
//! Blob content comes from the store's object directory and is hashed against its id
//! before it is written ([`cuenca_fs::ObjectDir`]), and trees come from the store, which
//! checks them on every read. The volume's own records are trusted only to skip work: a
//! record that names the wrong tree leaves that tree shown until the log moves again, so
//! nothing but the materialiser may write `.cuenca/`. One materialiser per volume is
//! enforced by a lock on `.cuenca/lock`.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use cuenca_core::{ForkName, LocationName, Store};
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
/// How long a tree that failed to materialise waits before it is tried again, unless the
/// log moves that path to another tree first ([`VolumeConfig::retry_after`]).
pub const RETRY_AFTER: Duration = Duration::from_secs(30);

/// One thing the volume shows.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
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
    /// Trees that could not be materialised this time, with why. What was shown before
    /// stays shown.
    pub failed: Vec<(Shown, String)>,
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
    /// fork's head. One failure (a missing blob, a full disk) leaves that path showing
    /// its previous tree and does not stop the others.
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
            if self.shown.get(&what) == Some(&tree) && self.root.join(what.rel()).is_dir() {
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

    /// Materialise `tree` and swap it into `what`'s place.
    fn show(&mut self, store: &Store, what: &Shown, tree: GitOid) -> Result<(), ShowError> {
        let staged = self.staging.join(self.next_staging.to_string());
        self.next_staging += 1;
        let result = self.stage_and_swap(store, what, tree, &staged);
        // What is left at the staging path is the tree that was shown before (after a
        // swap) or a tree never shown (after a failure): either way, nobody's.
        if let Err(e) = remove_any(&staged)
            && result.is_ok()
        {
            tracing::warn!(path = %staged.display(), error = %e, "could not remove a replaced tree");
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
        set_modes(staged)?;
        flush(&self.staging)?;
        let dest = self.root.join(what.rel());
        swap_into_place(staged, &dest)?;
        File::open(dest.parent().expect("main/ or forks/"))?.sync_all()?;
        write_record(&self.state.join(what.rel()), tree)?;
        self.shown.insert(what.clone(), tree);
        Ok(())
    }
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

/// Folders [`DIR_MODE`], files [`FILE_MODE`] or [`EXEC_MODE`]; symlinks as they are.
/// Only ever run on a staged tree, which no one else can reach.
fn set_modes(dir: &Path) -> io::Result<()> {
    fs::set_permissions(dir, fs::Permissions::from_mode(DIR_MODE))?;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let path = entry.path();
        if kind.is_dir() {
            set_modes(&path)?;
        } else if kind.is_file() {
            let exec = entry.metadata()?.permissions().mode() & 0o100 != 0;
            let mode = if exec { EXEC_MODE } else { FILE_MODE };
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
}

impl VolumeConfig {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            interval: Duration::from_secs(1),
            retry_after: RETRY_AFTER,
        }
    }
}

/// The materialiser's thread. Dropping it stops the thread.
pub struct Materialiser {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Materialiser {
    /// Open the volume and start converging it: now, then each time the log grows.
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
                    while !stop.load(Ordering::SeqCst) {
                        let len = authority.store.len().ok();
                        if len.is_none() || len != seen || volume.retry_due() {
                            match volume.converge(&authority.store) {
                                Ok(pass) => {
                                    report(&pass);
                                    seen = len;
                                }
                                Err(e) => tracing::warn!(error = %e, "materialiser pass failed"),
                            }
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
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
