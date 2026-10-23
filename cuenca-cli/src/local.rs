//! What the CLI keeps about a checkout, outside it: `<state home>/checkouts/<id>/`, where
//! `<id>` is derived from the checkout's canonical path.
//!
//! - `state.json`: the checkout's root, its fork and epoch, and the tree the authority
//!   last acknowledged for it (`published`);
//! - `outbox/<seq>.json`: snapshots not yet acknowledged, in order, each a changeset and
//!   the tree it ends at. Each one's base is the previous one's tree, the first's is
//!   `published`; the last one's tree is what the next snapshot is taken against;
//! - `objects/`: a copy of every blob a queued snapshot adds, laid out as the authority's
//!   blob store, so a file edited after its snapshot cannot change what is published;
//! - `cache.json`: the cuenca-fs stat cache;
//! - `lock`: held while a verb reads or writes any of the above.
//!
//! Each file is replaced atomically. A snapshot is queued by one rename (its outbox
//! entry, written after its objects), and acknowledged by rewriting `state.json` and then
//! removing the entry, so a crash between the two leaves an entry whose tree is already
//! `published`, which the next flush drops.

use std::fs::File;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use cuenca_core::{ForkName, LocationName};
use cuenca_fs::{Content, NewBlob, StatCache};
use cuenca_tree::{BlobHasher, Changeset, GitOid, Tree};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::client::{Client, ClientError};

const FORMAT: u32 = 1;
/// How many blobs are uploaded at once.
const UPLOADERS: usize = 8;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    pub format: u32,
    /// The checkout's canonical path.
    pub root: PathBuf,
    /// The root directory's identity when it was checked out ([`identity`]): a
    /// directory removed and made again at the same path is not this checkout.
    pub root_id: Identity,
    /// The authority this checkout was made from.
    pub url: String,
    /// The fork its drafts go to: the one checked out, or the sibling the authority put
    /// a stale-epoch draft in.
    pub fork: ForkName,
    pub location: LocationName,
    /// The epoch drafts are written under.
    pub epoch: u64,
    /// The tree the authority last acknowledged for this checkout.
    pub published: Tree,
}

/// A queued snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub changeset: Changeset,
    pub tree: Tree,
    /// The blobs it adds, each with its size, copied into `objects/`.
    pub blobs: Vec<(GitOid, u64)>,
}

/// An open checkout's state, locked for as long as this lives.
pub struct Local {
    dir: PathBuf,
    pub state: State,
    _lock: File,
}

/// The state directory of the checkout rooted at `root` (canonical).
fn dir_for(root: &Path) -> anyhow::Result<PathBuf> {
    let digest = Sha256::digest(root.as_os_str().as_encoded_bytes());
    Ok(crate::config::state_home()?
        .join("checkouts")
        .join(hex::encode(&digest[..16])))
}

/// Which directory a path names: its device, inode and birth time. The birth time is
/// what tells a directory made again at the same path apart on Linux, where ext4 hands
/// a freed inode number straight back out (APFS does not reuse them). On a filesystem
/// that records no birth time, the inode alone has to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub dev: u64,
    pub ino: u64,
    /// Nanoseconds since the Unix epoch, when the filesystem records it.
    pub born_ns: Option<u128>,
}

pub fn identity(path: &Path) -> std::io::Result<Identity> {
    use std::os::unix::fs::MetadataExt as _;
    let m = std::fs::symlink_metadata(path)?;
    let born_ns = m
        .created()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos());
    Ok(Identity {
        dev: m.dev(),
        ino: m.ino(),
        born_ns,
    })
}

/// The canonical form of `path`, which may not exist yet (its parent must).
fn canonical(path: &Path) -> anyhow::Result<PathBuf> {
    if let Ok(p) = std::fs::canonicalize(path) {
        return Ok(p);
    }
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        bail!("{} has no parent directory", path.display());
    };
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    Ok(std::fs::canonicalize(parent)
        .with_context(|| format!("resolving {}", parent.display()))?
        .join(name))
}

fn read_state(dir: &Path) -> anyhow::Result<State> {
    let path = dir.join("state.json");
    let state: State = serde_json::from_slice(&std::fs::read(&path)?)
        .with_context(|| format!("{} is damaged", path.display()))?;
    if state.format != FORMAT {
        bail!(
            "{} has format {}, not {FORMAT}",
            path.display(),
            state.format
        );
    }
    Ok(state)
}

fn queued_in(dir: &Path) -> usize {
    std::fs::read_dir(dir.join("outbox")).map_or(0, |d| d.count())
}

fn lock(dir: &Path) -> anyhow::Result<File> {
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("lock"))?;
    file.lock()
        .with_context(|| format!("locking {}", dir.display()))?;
    Ok(file)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path)
        .with_context(|| format!("writing {}", path.display()))?;
    File::open(dir)?.sync_all()?;
    Ok(())
}

impl Local {
    /// Check that a checkout may be made at `dest` before anything is written there, and
    /// return its canonical path. Refused: `dest` inside another checkout (whose
    /// snapshots would take it in), and `dest` registered as a live checkout. A
    /// registration whose directory was removed or replaced is dropped if nothing is
    /// queued in it, and refused otherwise, naming where its queued snapshots are.
    pub fn prepare(dest: &Path) -> anyhow::Result<PathBuf> {
        let root = canonical(dest)?;
        for outer in root.ancestors().skip(1) {
            let dir = dir_for(outer)?;
            if let Ok(state) = read_state(&dir)
                && identity(outer).ok() == Some(state.root_id)
            {
                bail!(
                    "{} is inside {}, a checkout of fork {}; check out somewhere else",
                    root.display(),
                    outer.display(),
                    state.fork
                );
            }
        }
        let dir = dir_for(&root)?;
        if dir.join("state.json").exists() {
            let _held = lock(&dir)?;
            let state = read_state(&dir)?;
            if identity(&root).ok() == Some(state.root_id) {
                bail!(
                    "{} is already a checkout of fork {}",
                    root.display(),
                    state.fork
                );
            }
            let queued = queued_in(&dir);
            if queued > 0 {
                bail!(
                    "{} was a checkout of fork {} with {queued} snapshot(s) still queued; \
                     they are in {}. Publish or remove them before checking out here",
                    root.display(),
                    state.fork,
                    dir.display()
                );
            }
            std::fs::remove_dir_all(&dir)
                .with_context(|| format!("dropping the stale registration {}", dir.display()))?;
        }
        Ok(root)
    }

    /// Register a new checkout at `root`, which must already hold `state.published`.
    pub fn create(state: State) -> anyhow::Result<Self> {
        let dir = dir_for(&state.root)?;
        if dir.join("state.json").exists() {
            bail!(
                "{} is already registered as a checkout; its state is {}",
                state.root.display(),
                dir.display()
            );
        }
        for d in ["outbox", "objects"] {
            std::fs::create_dir_all(dir.join(d))
                .with_context(|| format!("creating {}", dir.display()))?;
        }
        std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
        let lock = lock(&dir)?;
        let local = Self {
            dir,
            state,
            _lock: lock,
        };
        local.save_state()?;
        Ok(local)
    }

    /// The checkout holding `path` (it or its nearest registered ancestor).
    pub fn find(path: &Path) -> anyhow::Result<Self> {
        let path =
            std::fs::canonicalize(path).with_context(|| format!("resolving {}", path.display()))?;
        for root in path.ancestors() {
            let dir = dir_for(root)?;
            if !dir.join("state.json").exists() {
                continue;
            }
            let lock = lock(&dir)?;
            let state = read_state(&dir)?;
            if state.root != root {
                bail!("{} does not belong to {}", dir.display(), root.display());
            }
            if identity(root).ok() != Some(state.root_id) {
                bail!(
                    "{} is not the directory checked out there as fork {} (it was removed or \
                     replaced); that checkout's state, with {} snapshot(s) queued, is in {}",
                    root.display(),
                    state.fork,
                    queued_in(&dir),
                    dir.display()
                );
            }
            return Ok(Self {
                dir,
                state,
                _lock: lock,
            });
        }
        bail!(
            "{} is not inside a cuenca checkout (make one with `cuenca checkout <fork> <dir>`)",
            path.display()
        )
    }

    pub fn root(&self) -> &Path {
        &self.state.root
    }

    pub fn save_state(&self) -> anyhow::Result<()> {
        write_atomic(
            &self.dir.join("state.json"),
            &serde_json::to_vec(&self.state)?,
        )
    }

    pub fn load_cache(&self) -> anyhow::Result<StatCache> {
        Ok(StatCache::load(&self.dir.join("cache.json"))?)
    }

    pub fn save_cache(&self, cache: &StatCache) -> anyhow::Result<()> {
        Ok(cache.save(&self.dir.join("cache.json"))?)
    }

    fn objects(&self) -> PathBuf {
        self.dir.join("objects")
    }

    fn object_path(&self, oid: GitOid) -> PathBuf {
        let hex = oid.to_hex();
        self.objects().join(&hex[..2]).join(&hex[2..])
    }

    /// The queued snapshots, oldest first, with their sequence numbers.
    pub fn outbox(&self) -> anyhow::Result<Vec<(u64, Entry)>> {
        let mut out = Vec::new();
        for item in std::fs::read_dir(self.dir.join("outbox"))? {
            let item = item?;
            let name = item.file_name();
            let Some(seq) = name
                .to_str()
                .and_then(|n| n.strip_suffix(".json"))
                .and_then(|n| n.parse::<u64>().ok())
            else {
                continue;
            };
            let entry: Entry = serde_json::from_slice(&std::fs::read(item.path())?)
                .with_context(|| format!("{} is damaged", item.path().display()))?;
            out.push((seq, entry));
        }
        out.sort_by_key(|(seq, _)| *seq);
        Ok(out)
    }

    /// The tree the next snapshot is taken against: the last queued one's, or the
    /// acknowledged one.
    pub fn local_tree(&self) -> anyhow::Result<Tree> {
        Ok(match self.outbox()?.pop() {
            Some((_, e)) => e.tree,
            None => self.state.published.clone(),
        })
    }

    /// Queue a snapshot from `base` (the current [`Local::local_tree`]) to `tree`: copy
    /// each new blob into `objects/`, checking it still hashes to the id the scan gave it,
    /// then write the entry.
    ///
    /// The objects and their directories are handed to the disk with a plain `fsync`; the
    /// entry is then written with a full sync (`F_FULLFSYNC` on macOS, which flushes the
    /// drive's cache, the objects' writes included), so the entry is never durable before
    /// the objects it names. A full sync per object would cost ~5 ms each on a Mac.
    pub fn queue(&self, changeset: Changeset, tree: Tree, new: &[NewBlob]) -> anyhow::Result<u64> {
        let mut dirs = std::collections::BTreeSet::new();
        for blob in new {
            if self.store_object(blob)? {
                dirs.insert(self.object_path(blob.oid).with_file_name(""));
            }
        }
        if !dirs.is_empty() {
            dirs.insert(self.objects());
        }
        for dir in dirs {
            rustix::fs::fsync(File::open(&dir)?)
                .with_context(|| format!("syncing {}", dir.display()))?;
        }
        let seq = self.outbox()?.last().map_or(1, |(s, _)| s + 1);
        let entry = Entry {
            changeset,
            tree,
            blobs: new.iter().map(|b| (b.oid, b.size)).collect(),
        };
        write_atomic(
            &self.dir.join("outbox").join(format!("{seq:020}.json")),
            &serde_json::to_vec(&entry)?,
        )?;
        Ok(seq)
    }

    /// Copy one blob into `objects/`; false if it was there already.
    fn store_object(&self, blob: &NewBlob) -> anyhow::Result<bool> {
        let dest = self.object_path(blob.oid);
        if dest.exists() {
            return Ok(false);
        }
        let dir = dest.parent().expect("an object has a directory");
        std::fs::create_dir_all(dir)?;
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        let mut hasher = BlobHasher::new(blob.size);
        match &blob.content {
            Content::Inline(bytes) => {
                hasher.update(bytes);
                tmp.write_all(bytes)?;
            }
            Content::File(path) => {
                let mut file =
                    File::open(path).with_context(|| format!("reading {}", path.display()))?;
                let mut buf = vec![0; 256 * 1024];
                loop {
                    let n = file.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    hasher.update(&buf[..n]);
                    tmp.write_all(&buf[..n])?;
                }
            }
        }
        let changed = || {
            let what = match &blob.content {
                Content::File(p) => p.display().to_string(),
                Content::Inline(_) => "a symlink".into(),
            };
            anyhow::anyhow!("{what} changed while it was snapshotted; snapshot again")
        };
        let hashed = hasher.finish().map_err(|_| changed())?;
        if hashed.oid() != blob.oid {
            return Err(changed());
        }
        rustix::fs::fsync(tmp.as_file())?;
        tmp.persist(&dest)?;
        Ok(true)
    }

    /// Publish the outbox in order. Stops at the first draft that cannot go out, and
    /// leaves it and everything after it queued.
    pub fn flush(&mut self, client: &Client) -> Flush {
        let mut flush = Flush::default();
        let entries = match self.outbox() {
            Ok(e) => e,
            Err(e) => {
                flush.error = Some(format!("{e:#}"));
                return flush;
            }
        };
        let mut left = entries.len();
        for (seq, entry) in entries {
            match self.publish_one(client, seq, &entry) {
                Ok(sent) => {
                    left -= 1;
                    flush.published.extend(sent);
                }
                Err(e) => {
                    flush.offline = e
                        .downcast_ref::<ClientError>()
                        .is_some_and(|c| c.is_offline());
                    flush.error = Some(format!("{e:#}"));
                    break;
                }
            }
        }
        flush.pending = left;
        if flush.error.is_none()
            && let Err(e) = self.clear_objects()
        {
            flush.error = Some(format!("{e:#}"));
        }
        flush
    }

    /// Publish one queued snapshot. `None`: it was already acknowledged (a crash came
    /// between acknowledging it and removing it).
    fn publish_one(
        &mut self,
        client: &Client,
        seq: u64,
        entry: &Entry,
    ) -> anyhow::Result<Option<Sent>> {
        let path = self.dir.join("outbox").join(format!("{seq:020}.json"));
        let published = self.state.published.git_tree_id();
        if entry.changeset.target == published && entry.changeset.base != published {
            std::fs::remove_file(&path)?;
            return Ok(None);
        }
        if entry.changeset.base != published {
            bail!(
                "outbox entry {seq} starts from {}, not the acknowledged tree {published}",
                entry.changeset.base
            );
        }
        self.upload(client, &entry.blobs)?;
        let sent = match client.publish(&self.state.fork, self.state.epoch, &entry.changeset) {
            Ok(p) => {
                let sent = Sent {
                    id: p.id,
                    fork: p.fork.clone(),
                    fenced: p.fenced,
                    target: entry.changeset.target,
                };
                self.state.fork = p.fork;
                self.state.epoch = p.epoch;
                sent
            }
            // Already in the fork: a retry of a draft the fork took before a takeover.
            Err(ClientError::Api {
                code,
                head: Some(head),
                ..
            }) if code == "not_at_base" && head == entry.changeset.target.to_hex() => Sent {
                id: changeset_id(&entry.changeset),
                fork: self.state.fork.clone(),
                fenced: false,
                target: entry.changeset.target,
            },
            Err(e) => return Err(e.into()),
        };
        self.state.published = entry.tree.clone();
        self.save_state()?;
        std::fs::remove_file(&path)?;
        Ok(Some(sent))
    }

    fn upload(&self, client: &Client, blobs: &[(GitOid, u64)]) -> anyhow::Result<()> {
        let next = std::sync::atomic::AtomicUsize::new(0);
        let first_error = std::sync::Mutex::new(None);
        std::thread::scope(|s| {
            for _ in 0..UPLOADERS.min(blobs.len()) {
                s.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(&(oid, size)) = blobs.get(i) else {
                            return;
                        };
                        if let Err(e) = client.put_blob(oid, &self.object_path(oid), size) {
                            first_error.lock().unwrap().get_or_insert(e);
                            next.store(blobs.len(), std::sync::atomic::Ordering::Relaxed);
                            return;
                        }
                    }
                });
            }
        });
        match first_error.into_inner().unwrap() {
            Some(e) => Err(e.into()),
            None => Ok(()),
        }
    }

    /// Remove every object once nothing is queued.
    fn clear_objects(&self) -> anyhow::Result<()> {
        if !self.outbox()?.is_empty() {
            return Ok(());
        }
        for sub in std::fs::read_dir(self.objects())? {
            let sub = sub?;
            if sub.file_type()?.is_dir() {
                std::fs::remove_dir_all(sub.path())?;
            }
        }
        Ok(())
    }
}

/// A changeset's id as the authority computes it: the SHA-256 of its canonical JSON.
pub fn changeset_id(changeset: &Changeset) -> String {
    cuenca_core::ChangesetId::of(changeset).to_hex()
}

/// One draft a flush published.
#[derive(Debug, Clone, Serialize)]
pub struct Sent {
    pub id: String,
    /// Where it went: the checkout's fork, or the sibling a stale epoch's draft is kept in.
    pub fork: ForkName,
    pub fenced: bool,
    pub target: GitOid,
}

/// What a flush did.
#[derive(Debug, Default, Serialize)]
pub struct Flush {
    pub published: Vec<Sent>,
    /// Snapshots still queued.
    pub pending: usize,
    /// Whether it stopped because the authority was unreachable.
    pub offline: bool,
    /// Why it stopped, if it did.
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(dir: &Path) -> Local {
        Local {
            dir: dir.to_path_buf(),
            state: State {
                format: FORMAT,
                root: dir.to_path_buf(),
                root_id: Identity {
                    dev: 0,
                    ino: 0,
                    born_ns: None,
                },
                url: "http://127.0.0.1:1".into(),
                fork: ForkName::new("f1").unwrap(),
                location: LocationName::new("l1").unwrap(),
                epoch: 1,
                published: Tree::new(),
            },
            _lock: File::create(dir.join("lock")).unwrap(),
        }
    }

    /// A blob is copied into the outbox only if its bytes are still the ones the scan
    /// hashed: a file edited between the scan and the copy fails the snapshot.
    #[test]
    fn an_object_is_kept_only_if_it_still_hashes_to_its_id() {
        let dir = tempfile::tempdir().unwrap();
        let l = local(dir.path());
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "edited").unwrap();
        let blob = |content: &[u8]| NewBlob {
            oid: GitOid::for_blob(content),
            size: content.len() as u64,
            content: Content::File(file.clone()),
        };
        let stale = blob(b"before");
        let err = l.store_object(&stale).unwrap_err();
        assert!(
            err.to_string().contains("changed while it was snapshotted"),
            "{err}"
        );
        assert!(!l.object_path(stale.oid).exists());

        let fresh = blob(b"edited");
        assert!(l.store_object(&fresh).unwrap());
        assert_eq!(std::fs::read(l.object_path(fresh.oid)).unwrap(), b"edited");
        assert!(!l.store_object(&fresh).unwrap(), "stored once");
    }
}
