//! The authority's shared state, who a request comes from, and what a caller may read.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use cuenca_core::{CoreError, Fork, ForkName, Holder, Location, PrincipalName, Store};
use cuenca_tree::{Changeset, GitOid, Op};

use crate::identity::Whois;
use crate::keys::{KeyRecord, KeyRing, PrincipalKind};
use crate::materialise::Volume;

/// Trees whose blob sets are kept in memory for read checks. Trees never change, so
/// an entry is never stale; the cache is only cleared to bound its size.
const BLOB_SET_CACHE: usize = 64;

/// The authority: the store, the keys and the node resolver.
pub struct Authority {
    pub(crate) store: Store,
    pub(crate) keys: KeyRing,
    pub(crate) whois: Arc<dyn Whois>,
    /// Where blob uploads are staged before they are verified.
    pub(crate) staging: PathBuf,
    blob_sets: Mutex<HashMap<GitOid, Arc<HashSet<GitOid>>>>,
    /// Blobs each principal has sent the bytes of (see [`Authority::check_puts`]).
    possessed: Mutex<HashMap<PrincipalName, HashSet<GitOid>>>,
    /// Principals revoked by this process, refused before any record of it is
    /// written ([`Authority::ban`]).
    banned: Mutex<HashSet<PrincipalName>>,
    /// Taken for reading by every agent write from its last check to its commit
    /// ([`Authority::guarded`]), and for writing by [`Authority::ban`].
    revocation: std::sync::RwLock<()>,
    /// Bumped to wake the lander ([`Authority::wake_lander`]).
    lander_wake: (Mutex<u64>, std::sync::Condvar),
    /// The last error each landing hit, while it is retried.
    pub(crate) landing_errors: crate::land::LandingErrors,
    /// The volume, once a materialiser keeps it: shared so that forking an owner fork
    /// snapshots it first, with no materialiser pass in between.
    volume: OnceLock<Arc<Mutex<Volume>>>,
}

/// How long forking an owner fork keeps trying to snapshot it before refusing: a file
/// that changes between the scan and its upload fails that attempt, so a fork taken
/// while a client keeps writing may need several.
const SNAPSHOT_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

/// The pause between two attempts, with the volume unlocked.
const SNAPSHOT_PAUSE: std::time::Duration = std::time::Duration::from_millis(100);

/// Why forking a fork was refused.
#[derive(Debug, thiserror::Error)]
pub enum BranchError {
    #[error(transparent)]
    Core(#[from] CoreError),
    /// The owner fork could not be snapshotted, so it was not forked: a fork of it
    /// would lack what the owner wrote since its last snapshot.
    #[error("owner fork {fork} could not be snapshotted first, so it was not forked: {error}")]
    Snapshot { fork: String, error: String },
}

/// An authenticated request's principal, on the node Tailscale named.
#[derive(Debug, Clone)]
pub struct Caller {
    pub key: KeyRecord,
    pub holder: Holder,
}

/// Why a request was not authenticated. Each is answered 401 without saying which;
/// the reason goes to the authority's log.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("no bearer token")]
    NoToken,
    #[error("unknown or wrong key")]
    BadKey,
    #[error("principal {0} is revoked")]
    Revoked(String),
    #[error("tailscale does not name source {0}")]
    UnnamedSource(IpAddr),
    #[error("whois for {0} failed: {1}")]
    WhoisFailed(IpAddr, String),
    #[error("key of {principal} is bound to node {bound}, request came from {actual}")]
    WrongNode {
        principal: String,
        bound: String,
        actual: String,
    },
    #[error("node {0} serves a subnet route")]
    ServesRoutes(String),
    #[error("{0}")]
    Internal(String),
}

impl Authority {
    /// Open the authority's state in `dir`: the store in `dir/store`, the keys file
    /// `dir/keys.json`, staging in `dir/staging`.
    pub fn open(dir: &Path, whois: Arc<dyn Whois>) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let store = Store::open(dir.join("store")).map_err(std::io::Error::other)?;
        let keys = KeyRing::open(dir.join("keys.json"))?;
        // The volume's principal has no key (it holds owner forks); one on file was
        // written by hand or by an older build, and is refused rather than honoured.
        if keys
            .list()
            .iter()
            .any(|k| k.principal.as_str() == cuenca_core::VOLUME_PRINCIPAL)
        {
            return Err(std::io::Error::other(format!(
                "{}: holds a key for principal {:?}, which is the volume's and has no key; \
                 remove it",
                dir.join("keys.json").display(),
                cuenca_core::VOLUME_PRINCIPAL
            )));
        }
        // Uploads a crash interrupted are never resumed.
        let staging = dir.join("staging");
        if staging.exists() {
            std::fs::remove_dir_all(&staging)?;
        }
        std::fs::create_dir_all(&staging)?;
        Ok(Self {
            store,
            keys,
            whois,
            staging,
            blob_sets: Mutex::new(HashMap::new()),
            possessed: Mutex::new(HashMap::new()),
            banned: Mutex::new(HashSet::new()),
            revocation: std::sync::RwLock::new(()),
            lander_wake: (Mutex::new(0), std::sync::Condvar::new()),
            landing_errors: Default::default(),
            volume: OnceLock::new(),
        })
    }

    /// Wake the lander: a landing was asked for.
    pub fn wake_lander(&self) {
        let (count, cv) = &self.lander_wake;
        *count.lock().expect("lander lock") += 1;
        cv.notify_all();
    }

    pub(crate) fn lander_generation(&self) -> u64 {
        *self.lander_wake.0.lock().expect("lander lock")
    }

    /// Wait until woken after `seen`, or `timeout`; the generation now.
    pub(crate) fn wait_lander(&self, seen: u64, timeout: std::time::Duration) -> u64 {
        let (count, cv) = &self.lander_wake;
        let guard = count.lock().expect("lander lock");
        let (guard, _) = cv
            .wait_timeout_while(guard, timeout, |n| *n == seen)
            .expect("lander lock");
        *guard
    }

    pub(crate) fn set_landing_error(&self, fork: &cuenca_core::ForkName, error: Option<String>) {
        self.landing_errors.set(fork, error);
    }

    /// Share the volume a materialiser keeps ([`crate::materialise::Materialiser`]).
    /// One per authority: a second is refused.
    pub(crate) fn set_volume(&self, volume: Arc<Mutex<Volume>>) -> std::io::Result<()> {
        self.volume
            .set(volume)
            .map_err(|_| std::io::Error::other("the authority already has a volume"))
    }

    /// The shared volume, locked: a pass that panicked leaves it as consistent as a
    /// failed one (each pass starts from what is on disk), so a poisoned lock is taken.
    pub(crate) fn lock_volume(volume: &Mutex<Volume>) -> std::sync::MutexGuard<'_, Volume> {
        volume.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Fork the fork `from` into `fork`, held by `holder` (the caller), at `from`'s head
    /// ([`Store::branch_fork`]), as a write by `holder`'s principal
    /// ([`Authority::guarded`]). A fork the rules will refuse anyway (its name, its
    /// holder, no such `from`) is refused before anything else is done.
    ///
    /// An owner fork the volume shows is snapshotted first (spec section 2a: "the
    /// authority snapshots it, and the new fork starts from that snapshot"), with the
    /// volume locked from the snapshot to the fork, so its head cannot move in between.
    /// A file a client is still writing is taken as far as it has been written; its
    /// finished version is the owner fork's next snapshot (see [`crate::materialise`]).
    /// The new fork holds every write the snapshot read, and anything written later
    /// stays in `from`. A snapshot that fails (a file the authority cannot read, one
    /// that changed between the scan and its upload) is taken again, with the volume
    /// unlocked between attempts, for up to `SNAPSHOT_BUDGET`, then refused, and no
    /// fork is made.
    ///
    /// The volume is locked outside the revocation lock, never inside it, so a slow
    /// snapshot never holds up a revocation.
    pub fn branch_fork(
        &self,
        fork: &ForkName,
        from: &ForkName,
        holder: &Holder,
    ) -> Result<u64, BranchError> {
        let principal = &holder.principal;
        self.guarded(principal, || self.store.check_branch(fork, from, holder))?;
        let source = self
            .store
            .fork(from)?
            .ok_or_else(|| CoreError::UnknownFork(from.to_string()))?;
        let Some(volume) = self
            .volume
            .get()
            .filter(|_| source.holder().is_some_and(Holder::is_volume))
        else {
            return Ok(self.guarded(principal, || self.store.branch_fork(fork, from, holder))?);
        };
        let started = std::time::Instant::now();
        let mut attempt = 0;
        loop {
            attempt += 1;
            // Locked from the snapshot to the fork; unlocked between attempts.
            let mut locked = Self::lock_volume(volume);
            // Read under the lock: only the volume moves an owner fork's head.
            let source = self
                .store
                .fork(from)?
                .ok_or_else(|| CoreError::UnknownFork(from.to_string()))?;
            let error = match locked.snapshot_owner_fork(&self.store, &source) {
                None | Some(Ok(_)) => {
                    return Ok(
                        self.guarded(principal, || self.store.branch_fork(fork, from, holder))?
                    );
                }
                Some(Err(error)) => error,
            };
            if started.elapsed() + SNAPSHOT_PAUSE >= SNAPSHOT_BUDGET {
                tracing::error!(fork = %from, error, attempt, "owner fork snapshot failed; not forked");
                return Err(BranchError::Snapshot {
                    fork: from.to_string(),
                    error: without_host_paths(&error, locked.root(), from),
                });
            }
            drop(locked);
            tracing::warn!(fork = %from, error, attempt, "owner fork snapshot failed; trying again");
            std::thread::sleep(SNAPSHOT_PAUSE);
        }
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn keys(&self) -> &KeyRing {
        &self.keys
    }

    /// Where uploads are staged before they are verified.
    pub fn staging_dir(&self) -> &Path {
        &self.staging
    }

    /// Who `token`, arriving from `source`, is: the key must verify, its principal must
    /// not be revoked in the log, Tailscale must name the source as the key's node, and
    /// that node must serve no subnet route.
    pub async fn authenticate(
        &self,
        token: Option<&str>,
        source: SocketAddr,
    ) -> Result<Caller, AuthError> {
        let token = token.ok_or(AuthError::NoToken)?;
        let key = self.keys.verify(token).ok_or(AuthError::BadKey)?;
        if self.is_banned(&key.principal) {
            return Err(AuthError::Revoked(key.principal.to_string()));
        }
        if self
            .store
            .revoked(&key.principal)
            .map_err(|e| AuthError::Internal(e.to_string()))?
            .is_some()
        {
            return Err(AuthError::Revoked(key.principal.to_string()));
        }
        let ip = canonical(source.ip());
        let peer = self
            .whois
            .whois(ip)
            .await
            .map_err(|e| AuthError::WhoisFailed(ip, e))?
            .ok_or(AuthError::UnnamedSource(ip))?;
        if peer.node != key.node {
            return Err(AuthError::WrongNode {
                principal: key.principal.to_string(),
                bound: key.node.to_string(),
                actual: peer.node.to_string(),
            });
        }
        if peer.serves_routes {
            return Err(AuthError::ServesRoutes(peer.node.to_string()));
        }
        let holder = Holder::new(key.principal.clone(), key.node.clone());
        Ok(Caller { key, holder })
    }

    /// The blob ids `tree` names, from the cache or the store.
    pub(crate) fn blob_set(&self, tree: GitOid) -> cuenca_core::Result<Arc<HashSet<GitOid>>> {
        if let Some(set) = self.blob_sets.lock().expect("cache lock").get(&tree) {
            return Ok(set.clone());
        }
        let set: Arc<HashSet<GitOid>> = Arc::new(
            self.store
                .tree(tree)?
                .entries()
                .map(|(_, e)| e.oid())
                .collect(),
        );
        let mut cache = self.blob_sets.lock().expect("cache lock");
        if cache.len() >= BLOB_SET_CACHE {
            cache.clear();
        }
        cache.insert(tree, set.clone());
        Ok(set)
    }
}

/// Most blobs remembered per principal; past it the record starts over, and the client
/// sends the bytes again when a draft is refused for a missing blob.
const MAX_POSSESSED: usize = 1 << 20;

impl Authority {
    /// Refuse `principal` from now on, in this process: the part of revocation that
    /// needs no write, so it holds even when the disk is full. The admin socket's
    /// `key_revoke` does this first, then records the revocation in the log.
    pub fn ban(&self, principal: &PrincipalName) {
        // Refused at once, reads included; then wait for writes already past their
        // check, so once this returns no write by `principal` is still to commit.
        self.banned
            .lock()
            .expect("ban lock")
            .insert(principal.clone());
        drop(self.revocation.write().expect("revocation lock"));
    }

    /// Run `write` for `principal` only if it is not revoked, checked again here, at
    /// the end of a request that was authenticated at its start: a ban (or a recorded
    /// revocation) either lands before this check and refuses the write, or waits for
    /// the write to commit.
    pub(crate) fn guarded<T, E: From<CoreError>>(
        &self,
        principal: &PrincipalName,
        write: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, E> {
        let _writes = self.revocation.read().expect("revocation lock");
        if self.is_banned(principal) || self.store.revoked(principal)?.is_some() {
            return Err(CoreError::Revoked(principal.to_string()).into());
        }
        write()
    }

    /// Whether this process has refused `principal` ([`Authority::ban`]).
    pub fn is_banned(&self, principal: &PrincipalName) -> bool {
        self.banned.lock().expect("ban lock").contains(principal)
    }

    /// Remember that `principal` sent the bytes of `oid`, verified.
    pub(crate) fn possesses(&self, principal: &PrincipalName, oid: GitOid) {
        let mut all = self.possessed.lock().expect("possession lock");
        let set = all.entry(principal.clone()).or_default();
        if set.len() >= MAX_POSSESSED {
            set.clear();
        }
        set.insert(oid);
    }

    /// Whether `caller` may name each blob `changeset` puts, in a draft of `fork`.
    ///
    /// The store accepts any blob it holds, whoever sent it, and a blob in a fork the
    /// caller holds can be read back. So a put must name a blob the caller can already
    /// read, or has shown it has:
    ///
    /// - a caller who reads the fork may name a blob in the fork's head, the tree the
    ///   API serves it;
    /// - any caller may name a blob whose bytes its principal uploaded to this
    ///   authority since it started.
    ///
    /// A former holder (whose draft the rules fence into its sibling) reads the fork no
    /// more, so it gets the second rule only. Anything else is refused as
    /// `MissingBlob`, the same answer an absent blob gets, so the refusal does not say
    /// whether the store holds it. This runs before the rules, so after a restart even
    /// a retried draft is refused this way until the client sends the bytes again.
    pub(crate) fn check_puts(
        &self,
        caller: &Caller,
        fork: &Fork,
        changeset: &Changeset,
    ) -> cuenca_core::Result<()> {
        let head = if caller.sees_fork(fork) {
            Some(self.blob_set(fork.head)?)
        } else {
            None
        };
        let possessed = self.possessed.lock().expect("possession lock");
        let own = possessed.get(&caller.key.principal);
        for op in &changeset.ops {
            if let Op::Put { entry, .. } = op {
                let oid = entry.oid();
                let known = head.as_ref().is_some_and(|set| set.contains(&oid))
                    || own.is_some_and(|set| set.contains(&oid));
                if !known {
                    return Err(CoreError::MissingBlob(oid));
                }
            }
        }
        Ok(())
    }
}

/// A failed owner-fork snapshot as the caller is told it: paths are the fork's own
/// (the fork's folder itself is named by the fork), and where the volume is on the
/// authority's host stays in its log.
fn without_host_paths(error: &str, root: &Path, fork: &ForkName) -> String {
    let dir = root.join("forks").join(fork.as_str());
    let dir = dir.display().to_string();
    let root = root.display().to_string();
    error
        .replace(&format!("{dir}/"), "")
        .replace(&dir, fork.as_str())
        .replace(&format!("{root}/"), "<volume>/")
}

/// An IPv4 address that arrived as IPv4-mapped IPv6 is the IPv4 address.
fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    }
}

impl Caller {
    fn owner_device(&self) -> bool {
        self.key.kind == PrincipalKind::OwnerDevice
    }

    /// Main of a location: an owner device reads every one; an agent, those it is
    /// granted, which are also the ones it may fork.
    pub fn sees_location(&self, location: &Location) -> bool {
        self.owner_device() || self.key.grant.locations.contains(&location.name)
    }

    /// A fork: an owner device reads every one; an agent, the forks its grant names of
    /// the locations it is granted, and the fork it holds now (its principal on its
    /// key's node: a holder, as the core defines one), and no other. Fork names are
    /// first-come, so a granted name someone else creates first, from a location the
    /// agent is not granted, stays unread (BRO-3336). A former holder
    /// stops reading a fork when it is taken over; it keeps the sibling its stale
    /// drafts go to, which it holds.
    pub fn sees_fork(&self, fork: &Fork) -> bool {
        self.owner_device()
            || (self.key.grant.forks.contains(&fork.name)
                && self.key.grant.locations.contains(&fork.location))
            || fork.holder() == Some(&self.holder)
    }

    /// Whether a write to `fork` reaches the core's rules, which decide it: true for
    /// whoever reads the fork, and for a former holder, whose stale draft the rules
    /// keep in a sibling. Anyone else is answered as if the fork did not exist.
    pub fn reaches_fork(&self, fork: &Fork) -> bool {
        self.sees_fork(fork) || fork.holders.contains(&self.holder)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn a_snapshot_error_names_no_host_path() {
        let root = Path::new("/srv/cuenca/volume");
        let fork = ForkName::new("owner-mac").unwrap();
        let said = |e: &str| without_host_paths(e, root, &fork);
        assert_eq!(
            said("/srv/cuenca/volume/forks/owner-mac/a/b.md: changed while it was snapshotted"),
            "a/b.md: changed while it was snapshotted"
        );
        assert_eq!(
            said("/srv/cuenca/volume/forks/owner-mac: Permission denied (os error 13)"),
            "owner-mac: Permission denied (os error 13)"
        );
        assert_eq!(
            said("/srv/cuenca/volume/forks: gone"),
            "<volume>/forks: gone"
        );
        // A short or relative root replaces only whole path prefixes.
        let fork_path = |root: &str, e: &str| without_host_paths(e, Path::new(root), &fork);
        assert_eq!(
            fork_path(
                ".",
                "./forks/owner-mac/a.md: changed while it was snapshotted"
            ),
            "a.md: changed while it was snapshotted"
        );
        assert_eq!(
            fork_path("vol", "vol/forks/owner-mac/volume.md: Permission denied"),
            "volume.md: Permission denied"
        );
        assert_eq!(fork_path("vol", "vol/forks: gone"), "<volume>/forks: gone");
    }

    struct Nobody;

    impl crate::Whois for Nobody {
        fn whois(&self, _ip: IpAddr) -> crate::WhoisFuture<'_> {
            Box::pin(async { Ok(None) })
        }
    }

    /// A ban waits for a write already past its check, so once `ban` returns no write
    /// by that principal is still to commit.
    #[test]
    fn a_ban_waits_for_a_write_past_its_check() {
        use std::sync::mpsc;
        use std::time::Duration;
        let dir = tempfile::tempdir().unwrap();
        let a = Arc::new(Authority::open(dir.path(), Arc::new(Nobody)).unwrap());
        let p = PrincipalName::new("a1").unwrap();
        let (inside, reached) = mpsc::channel();
        let (go, wait) = mpsc::channel::<()>();
        let writer = {
            let (a, p) = (a.clone(), p.clone());
            std::thread::spawn(move || {
                a.guarded(&p, || {
                    inside.send(()).unwrap();
                    wait.recv().unwrap();
                    Ok::<_, CoreError>(())
                })
            })
        };
        reached.recv().unwrap();
        let banner = {
            let (a, p) = (a.clone(), p.clone());
            std::thread::spawn(move || a.ban(&p))
        };
        std::thread::sleep(Duration::from_millis(200));
        assert!(a.is_banned(&p), "refused at once");
        assert!(
            !banner.is_finished(),
            "the ban must wait for the write to commit"
        );
        go.send(()).unwrap();
        writer.join().unwrap().unwrap();
        banner.join().unwrap();
        // And a write after the ban is refused.
        assert!(a.guarded(&p, || Ok::<_, CoreError>(())).is_err());
    }

    #[test]
    fn mapped_v4_is_v4() {
        let v4 = Ipv4Addr::new(100, 82, 195, 109);
        assert_eq!(canonical(IpAddr::V6(v4.to_ipv6_mapped())), IpAddr::V4(v4));
        let v6: Ipv6Addr = "fd7a:115c:a1e0::a937:c36e".parse().unwrap();
        assert_eq!(canonical(IpAddr::V6(v6)), IpAddr::V6(v6));
    }
}
