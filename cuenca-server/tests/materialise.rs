//! The volume kept in step with the log (BRO-3256): every location's main and every
//! fork's head shown read-only, swapped in whole, converging after a kill.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::*;
use cuenca_core::{ForkName, Holder, LocationName, NodeId, PrincipalName, Store, Writer};
use cuenca_server::PrincipalKind::Agent;
use cuenca_server::materialise::{Materialiser, Shown, Volume, VolumeConfig};
use cuenca_tree::{Changeset, Entry, GitOid, Op, RepoPath, Tree};
use serde_json::json;

fn loc(s: &str) -> LocationName {
    LocationName::new(s).unwrap()
}

fn fork(s: &str) -> ForkName {
    ForkName::new(s).unwrap()
}

fn path(s: &str) -> RepoPath {
    RepoPath::new(s).unwrap()
}

fn writer() -> Writer {
    Writer::new(
        Holder::new(
            PrincipalName::new("agent").unwrap(),
            NodeId::new("nAgentCNTRL").unwrap(),
        ),
        1,
    )
}

fn ingest(store: &Store, content: &[u8]) -> GitOid {
    let oid = GitOid::for_blob(content);
    store
        .ingest_blob(oid, content.len() as u64, content)
        .unwrap();
    oid
}

/// One file to put: its path, how to make its entry from its blob, its content.
type Put<'a> = (&'a str, fn(GitOid) -> Entry, &'a [u8]);

/// The changeset putting each [`Put`] over `base`.
fn change(store: &Store, base: &Tree, ops: &[Put]) -> Changeset {
    let ops = ops
        .iter()
        .map(|(p, make, c)| Op::put(path(p), make(ingest(store, c))))
        .collect();
    Changeset::new(base, ops).unwrap()
}

/// A store with location `site` (main: `README`) and fork `f1` of it.
fn setup(dir: &Path) -> Store {
    let store = Store::open(dir.join("store")).unwrap();
    let main =
        Tree::from_entries([(path("README"), Entry::File(ingest(&store, b"main\n")))]).unwrap();
    store.add_location(&loc("site"), None, &main).unwrap();
    store
        .create_fork(&fork("f1"), &loc("site"), &writer().holder)
        .unwrap();
    store
}

fn mode(p: &Path) -> u32 {
    std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o7777
}

/// The tree a shown directory holds, read back as a snapshot would.
fn read_back(dir: &Path) -> GitOid {
    let mut cache = cuenca_fs::StatCache::new();
    cuenca_fs::snapshot(dir, &Tree::new(), &mut cache)
        .unwrap()
        .tree
        .git_tree_id()
}

fn staging_is_empty(volume: &Path) -> bool {
    std::fs::read_dir(volume.join(".cuenca/staging"))
        .unwrap()
        .next()
        .is_none()
}

#[test]
fn a_draft_is_shown_whole_and_read_only() {
    let dir = tempfile::tempdir().unwrap();
    let store = setup(dir.path());
    let root = dir.path().join("volume");
    let mut volume = Volume::open(&root, store.object_dir()).unwrap();

    let pass = volume.converge(&store).unwrap();
    assert_eq!(pass.built.len(), 2, "{pass:?}");
    assert!(pass.failed.is_empty(), "{pass:?}");
    assert_eq!(
        std::fs::read(root.join("main/site/README")).unwrap(),
        b"main\n"
    );
    assert_eq!(
        std::fs::read(root.join("forks/f1/README")).unwrap(),
        b"main\n"
    );

    let base = store.fork_head(&fork("f1")).unwrap();
    let cs = change(
        &store,
        &base,
        &[
            ("src/lib.rs", Entry::File, b"fn main() {}\n"),
            ("bin/run", Entry::Executable, b"#!/bin/sh\n"),
            ("link", Entry::Symlink, b"src/lib.rs"),
        ],
    );
    store.publish_draft(&fork("f1"), &writer(), &cs).unwrap();
    let pass = volume.converge(&store).unwrap();
    assert_eq!(
        pass.built,
        vec![(Shown::Fork(fork("f1")), cs.target)],
        "{pass:?}"
    );
    assert_eq!(pass.unchanged, 1);

    let shown = root.join("forks/f1");
    assert_eq!(read_back(&shown), cs.target);
    assert_eq!(
        volume.shown(&Shown::Fork(fork("f1"))),
        Some(cs.target),
        "recorded"
    );
    // Main did not move.
    assert!(!root.join("main/site/src").exists());
    // Read-only to all but the owner; the group reads; no one else.
    for d in [
        &root,
        &root.join("main"),
        &root.join("forks"),
        &shown,
        &shown.join("src"),
    ] {
        assert_eq!(mode(d), 0o2750, "{}", d.display());
    }
    assert_eq!(mode(&shown.join("README")), 0o640);
    assert_eq!(mode(&shown.join("src/lib.rs")), 0o640);
    assert_eq!(mode(&shown.join("bin/run")), 0o750);
    assert_eq!(
        std::fs::read_link(shown.join("link")).unwrap(),
        PathBuf::from("src/lib.rs")
    );
    assert_eq!(mode(&root.join(".cuenca")), 0o700);
    // The replaced tree is gone.
    assert!(staging_is_empty(&root));

    // A draft that deletes a path: it is gone from the volume, and the rest is the draft.
    let head = store.fork_head(&fork("f1")).unwrap();
    let cs2 = Changeset::new(&head, vec![Op::delete(path("src/lib.rs"))]).unwrap();
    store.publish_draft(&fork("f1"), &writer(), &cs2).unwrap();
    volume.converge(&store).unwrap();
    assert!(
        !shown.join("src/lib.rs").exists(),
        "a deleted file is still shown"
    );
    assert_eq!(read_back(&shown), cs2.target);

    // Nothing changed: nothing built.
    let pass = volume.converge(&store).unwrap();
    assert!(pass.built.is_empty(), "{pass:?}");
    assert_eq!(pass.unchanged, 2);
}

#[test]
fn a_reopened_volume_rebuilds_nothing_it_shows() {
    let dir = tempfile::tempdir().unwrap();
    let store = setup(dir.path());
    let root = dir.path().join("volume");
    Volume::open(&root, store.object_dir())
        .unwrap()
        .converge(&store)
        .unwrap();
    let mut volume = Volume::open(&root, store.object_dir()).unwrap();
    let pass = volume.converge(&store).unwrap();
    assert!(pass.built.is_empty(), "{pass:?}");
    assert_eq!(pass.unchanged, 2);

    // A shown tree replaced by a symlink behind the volume's back is shown again, and
    // what the link pointed at is left alone.
    let outside = dir.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("keep"), b"keep\n").unwrap();
    std::fs::remove_dir_all(root.join("forks/f1")).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("forks/f1")).unwrap();
    let pass = volume.converge(&store).unwrap();
    assert_eq!(pass.built.len(), 1, "{pass:?}");
    assert!(
        std::fs::symlink_metadata(root.join("forks/f1"))
            .unwrap()
            .is_dir()
    );
    assert_eq!(std::fs::read(outside.join("keep")).unwrap(), b"keep\n");

    // A shown tree removed behind the volume's back is shown again.
    std::fs::remove_dir_all(root.join("forks/f1")).unwrap();
    let pass = volume.converge(&store).unwrap();
    assert_eq!(pass.built.len(), 1, "{pass:?}");
    assert_eq!(
        std::fs::read(root.join("forks/f1/README")).unwrap(),
        b"main\n"
    );
}

#[test]
fn one_materialiser_per_volume() {
    let dir = tempfile::tempdir().unwrap();
    let store = setup(dir.path());
    let root = dir.path().join("volume");
    let _first = Volume::open(&root, store.object_dir()).unwrap();
    let err = Volume::open(&root, store.object_dir())
        .err()
        .expect("second open refused");
    assert!(err.to_string().contains("another materialiser"), "{err}");
}

#[test]
fn a_tree_that_fails_leaves_the_previous_one_shown() {
    let dir = tempfile::tempdir().unwrap();
    let store = setup(dir.path());
    let root = dir.path().join("volume");
    let mut volume = Volume::open(&root, store.object_dir()).unwrap();
    volume.converge(&store).unwrap();

    let base = store.fork_head(&fork("f1")).unwrap();
    let cs = change(&store, &base, &[("new", Entry::File, b"new content\n")]);
    store.publish_draft(&fork("f1"), &writer(), &cs).unwrap();
    // Damage the blob on disk: the checkout hashes it and refuses.
    let hex = GitOid::for_blob(b"new content\n").to_hex();
    let object = store.object_dir().join(&hex[..2]).join(&hex[2..]);
    let mut perms = std::fs::metadata(&object).unwrap().permissions();
    perms.set_mode(0o644);
    std::fs::set_permissions(&object, perms).unwrap();
    std::fs::write(&object, b"tampered\n").unwrap();

    let pass = volume.converge(&store).unwrap();
    assert!(pass.built.is_empty(), "{pass:?}");
    assert_eq!(pass.failed.len(), 1, "{pass:?}");
    assert_eq!(pass.failed[0].0, Shown::Fork(fork("f1")));
    assert!(!root.join("forks/f1/new").exists());
    assert_eq!(
        std::fs::read(root.join("forks/f1/README")).unwrap(),
        b"main\n"
    );
    assert!(staging_is_empty(&root));
    // Not retried at once.
    let pass = volume.converge(&store).unwrap();
    assert!(pass.failed.is_empty() && pass.built.is_empty(), "{pass:?}");
    assert!(!volume.retry_due());
}

/// A tree that failed is tried again once its wait is over, with no new log entry: the
/// materialiser's loop polls for it.
#[test]
fn a_failed_tree_is_tried_again_after_its_wait() {
    let s = Server::start();
    s.add_location("l1");
    let store = s.authority.store();
    store
        .create_fork(&fork("f1"), &loc("l1"), &writer().holder)
        .unwrap();
    let base = store.fork_head(&fork("f1")).unwrap();
    let cs = change(store, &base, &[("new", Entry::File, b"new content\n")]);
    store.publish_draft(&fork("f1"), &writer(), &cs).unwrap();
    let hex = GitOid::for_blob(b"new content\n").to_hex();
    let object = store.object_dir().join(&hex[..2]).join(&hex[2..]);
    let mut perms = std::fs::metadata(&object).unwrap().permissions();
    perms.set_mode(0o644);
    std::fs::set_permissions(&object, perms).unwrap();
    std::fs::write(&object, b"tampered\n").unwrap();

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("volume");
    let mut volume = Volume::open(&root, store.object_dir()).unwrap();
    volume.set_retry_after(Duration::from_millis(300));
    let pass = volume.converge(store).unwrap();
    assert_eq!(pass.failed.len(), 1, "{pass:?}");
    assert!(!volume.retry_due());
    std::thread::sleep(Duration::from_millis(350));
    assert!(volume.retry_due());
    drop(volume);

    // The materialiser's loop: the blob is repaired, the log does not move, and the
    // tree is shown once the wait is over.
    let len = store.len().unwrap();
    let mut config = VolumeConfig::new(root.clone());
    config.interval = Duration::from_millis(50);
    config.retry_after = Duration::from_millis(500);
    let materialiser = Materialiser::spawn(s.authority.clone(), config).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        !root.join("forks/f1/new").exists(),
        "failed on the first pass"
    );
    std::fs::write(&object, b"new content\n").unwrap();
    let start = Instant::now();
    while !root.join("forks/f1/new").exists() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "never tried again"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(store.len().unwrap(), len, "the log did not move");
    drop(materialiser);
    s.stop();
}

/// The record is written only after the swap: a swap that fails (here, `forks/` not
/// writable) leaves the previous tree shown and no record believed, so a reopened volume
/// builds the new tree instead of believing it shown.
#[test]
fn a_failed_swap_records_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let store = setup(dir.path());
    let root = dir.path().join("volume");
    let mut volume = Volume::open(&root, store.object_dir()).unwrap();
    volume.converge(&store).unwrap();
    let before = volume.shown(&Shown::Fork(fork("f1"))).unwrap();

    let base = store.fork_head(&fork("f1")).unwrap();
    let cs = change(&store, &base, &[("new", Entry::File, b"new\n")]);
    store.publish_draft(&fork("f1"), &writer(), &cs).unwrap();
    let forks = root.join("forks");
    std::fs::set_permissions(&forks, std::fs::Permissions::from_mode(0o550)).unwrap();
    let pass = volume.converge(&store).unwrap();
    std::fs::set_permissions(&forks, std::fs::Permissions::from_mode(0o2750)).unwrap();
    assert_eq!(pass.failed.len(), 1, "{pass:?}");
    assert_eq!(volume.shown(&Shown::Fork(fork("f1"))), None);
    assert_eq!(
        read_back(&root.join("forks/f1")),
        before,
        "the previous tree is shown"
    );
    drop(volume);

    let mut volume = Volume::open(&root, store.object_dir()).unwrap();
    assert_eq!(volume.shown(&Shown::Fork(fork("f1"))), None, "recorded");
    let pass = volume.converge(&store).unwrap();
    assert_eq!(pass.built, vec![(Shown::Fork(fork("f1")), cs.target)]);
}

/// A record that cannot be written after its swap (here, a directory where its
/// temporary file goes) is not believed: when the fork goes back to the tree it showed
/// before, the volume shows that tree again, in the same process and after a restart.
#[test]
fn a_tree_whose_record_failed_is_not_believed() {
    let dir = tempfile::tempdir().unwrap();
    let store = setup(dir.path());
    let root = dir.path().join("volume");
    let mut volume = Volume::open(&root, store.object_dir()).unwrap();
    volume.converge(&store).unwrap();
    let before = volume.shown(&Shown::Fork(fork("f1"))).unwrap();
    let blocker = root.join(".cuenca/state/forks/.f1.tmp");

    // The same process: a draft adds `new`, its record fails, the next draft removes it.
    let add_then_revert = |volume: &mut Volume| {
        let base = store.fork_head(&fork("f1")).unwrap();
        let cs = change(&store, &base, &[("new", Entry::File, b"new\n")]);
        store.publish_draft(&fork("f1"), &writer(), &cs).unwrap();
        std::fs::create_dir(&blocker).unwrap();
        let pass = volume.converge(&store).unwrap();
        std::fs::remove_dir(&blocker).unwrap();
        assert_eq!(pass.failed.len(), 1, "{pass:?}");
        assert!(
            root.join("forks/f1/new").exists(),
            "swapped before the record failed"
        );
        let head = store.fork_head(&fork("f1")).unwrap();
        let revert = Changeset::new(&head, vec![Op::delete(path("new"))]).unwrap();
        assert_eq!(revert.target, before);
        store
            .publish_draft(&fork("f1"), &writer(), &revert)
            .unwrap();
    };
    add_then_revert(&mut volume);
    let pass = volume.converge(&store).unwrap();
    assert_eq!(
        pass.built,
        vec![(Shown::Fork(fork("f1")), before)],
        "{pass:?}"
    );
    assert_eq!(read_back(&root.join("forks/f1")), before);

    // After a restart: the same, with the volume reopened between the failure and the
    // revert's pass.
    add_then_revert(&mut volume);
    drop(volume);
    let mut volume = Volume::open(&root, store.object_dir()).unwrap();
    let pass = volume.converge(&store).unwrap();
    assert_eq!(
        pass.built,
        vec![(Shown::Fork(fork("f1")), before)],
        "{pass:?}"
    );
    assert_eq!(read_back(&root.join("forks/f1")), before);
}

/// A reader of the volume never finds a shown fork missing while its tree is replaced:
/// one thread looks, again and again, while 8 drafts of 1,500 files are swapped in.
#[test]
fn a_reader_never_sees_a_shown_tree_missing() {
    let dir = tempfile::tempdir().unwrap();
    let store = setup(dir.path());
    let root = dir.path().join("volume");
    let mut volume = Volume::open(&root, store.object_dir()).unwrap();
    let base = store.fork_head(&fork("f1")).unwrap();
    store
        .publish_draft(&fork("f1"), &writer(), &numbered(&store, &base, 0))
        .unwrap();
    volume.converge(&store).unwrap();

    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader = {
        let done = done.clone();
        let fork_dir = root.join("forks/f1");
        let shown = fork_dir.join("d0");
        std::thread::spawn(move || {
            let (mut looks, mut gaps, mut mixed) = (0u64, 0u64, 0u64);
            while !done.load(std::sync::atomic::Ordering::SeqCst) {
                looks += 1;
                if !shown.is_dir() {
                    gaps += 1;
                }
                // Two files at opposite ends of the tree, read through one handle on the
                // shown directory: they come from one draft, or the tree was mixed. (A
                // replaced tree is removed while it may still be open: a read that fails
                // proves nothing either way.)
                if let Ok(dir) = std::fs::File::open(&fork_dir) {
                    let read = |name: &str| -> Option<String> {
                        let fd = rustix::fs::openat(
                            &dir,
                            name,
                            rustix::fs::OFlags::RDONLY,
                            rustix::fs::Mode::empty(),
                        )
                        .ok()?;
                        std::io::read_to_string(std::fs::File::from(fd)).ok()
                    };
                    if let (Some(a), Some(b)) = (read("d0/f0"), read("d3/f1499")) {
                        let draft = |s: &str| s.split(' ').next().map(str::to_string);
                        if draft(&a) != draft(&b) {
                            mixed += 1;
                        }
                    }
                }
                // Not a busy spin: the suite's other tests run beside this one.
                std::thread::yield_now();
            }
            (looks, gaps, mixed)
        })
    };
    for n in 1..=8 {
        let base = store.fork_head(&fork("f1")).unwrap();
        let cs = numbered(&store, &base, n);
        store.publish_draft(&fork("f1"), &writer(), &cs).unwrap();
        let pass = volume.converge(&store).unwrap();
        assert_eq!(pass.built, vec![(Shown::Fork(fork("f1")), cs.target)]);
    }
    done.store(true, std::sync::atomic::Ordering::SeqCst);
    let (looks, gaps, mixed) = reader.join().unwrap();
    assert!(looks > 1000, "the reader looked only {looks} times");
    assert_eq!(gaps, 0, "{gaps} of {looks} looks found forks/f1 missing");
    assert_eq!(
        mixed, 0,
        "{mixed} of {looks} looks found files of two drafts in one tree"
    );
}

/// A record's temporary file is never another fork's record: forks `f1.tmp` and `f1.x`
/// (whose record, with a `.tmp` extension swapped in, would be written through
/// `f1.tmp`'s), reopened, rebuild nothing.
#[test]
fn records_of_dotted_names_do_not_collide() {
    let dir = tempfile::tempdir().unwrap();
    let store = setup(dir.path());
    for name in ["f1.tmp", "f1.x"] {
        store
            .create_fork(&fork(name), &loc("site"), &writer().holder)
            .unwrap();
    }
    let root = dir.path().join("volume");
    let pass = Volume::open(&root, store.object_dir())
        .unwrap()
        .converge(&store)
        .unwrap();
    assert_eq!(pass.built.len(), 4, "{pass:?}");
    let pass = Volume::open(&root, store.object_dir())
        .unwrap()
        .converge(&store)
        .unwrap();
    assert!(pass.built.is_empty(), "{pass:?}");
    assert_eq!(pass.unchanged, 4);
}

/// Over HTTP, as an agent publishes: shown within the spec's 15 s.
#[test]
fn a_published_draft_is_in_the_volume_within_15_s() {
    let s = Server::start();
    s.add_location("l1");
    let a1 = s.mint("a1", Agent, "nA1CNTRL", grant(&["l1"], &[]));
    s.whois.node("nA1CNTRL");
    let r = s.post(&a1, "/v1/forks", json!({ "fork": "f1", "location": "l1" }));
    assert_eq!(r.status(), 200, "{}", r.text().unwrap());

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("volume");
    let mut config = VolumeConfig::new(root.clone());
    config.interval = Duration::from_millis(200);
    let materialiser = Materialiser::spawn(s.authority.clone(), config).unwrap();

    let cs = s.write(&a1, &Tree::new(), &[("notes/today.md", b"hello\n")]);
    let published = Instant::now();
    let r = s.publish(&a1, "f1", 1, &cs);
    assert_eq!(r.status(), 200, "{}", r.text().unwrap());
    let shown = root.join("forks/f1/notes/today.md");
    while !shown.exists() {
        assert!(
            published.elapsed() < Duration::from_secs(15),
            "not shown in 15 s"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let took = published.elapsed();
    assert_eq!(std::fs::read(&shown).unwrap(), b"hello\n");
    assert_eq!(read_back(&root.join("forks/f1")), cs.target);
    eprintln!("draft shown in the volume {took:?} after its publish was acknowledged");
    drop(materialiser);
    s.stop();
}

// ---- killed while materialising ----------------------------------------------------

const DIR_ENV: &str = "CUENCA_MATERIALISE_CRASH_DIR";
const KILLS: usize = 10;
/// Kills tried at most, to get [`KILLS`] inside a materialisation.
const ATTEMPTS: usize = 100;
/// Files each draft rewrites, so a checkout takes long enough to be killed inside.
const FILES: usize = 1500;

/// The draft numbered `n`: file `i` holds `n i`, ingested in one batch.
fn numbered(store: &Store, base: &Tree, n: u64) -> Changeset {
    let mut batch = store.blob_batch();
    let ops = (0..FILES)
        .map(|i| {
            let content = format!("{n} {i}\n");
            let oid = GitOid::for_blob(content.as_bytes());
            batch
                .add(oid, content.len() as u64, content.as_bytes())
                .unwrap();
            Op::put(path(&format!("d{}/f{i}", i % 8)), Entry::File(oid))
        })
        .collect();
    batch.commit().unwrap();
    Changeset::new(base, ops).unwrap()
}

/// The child: publish a draft, materialise, repeat, until killed. Not a test alone.
#[test]
#[ignore = "run by killed_while_materialising as a child process"]
fn child_materialiser() {
    let Ok(dir) = std::env::var(DIR_ENV) else {
        return;
    };
    let dir = PathBuf::from(dir);
    let store = Store::open(dir.join("store")).unwrap();
    let mut volume = Volume::open(&dir.join("volume"), store.object_dir()).unwrap();
    std::fs::write(dir.join("ready"), b"").unwrap();
    let mut n = store.len().unwrap();
    loop {
        let base = store.fork_head(&fork("f1")).unwrap();
        n += 1;
        store
            .publish_draft(&fork("f1"), &writer(), &numbered(&store, &base, n))
            .unwrap();
        // Mark the pass, and how long the last one took, so the parent can aim inside.
        std::fs::write(dir.join("in-pass"), b"").unwrap();
        let start = Instant::now();
        let pass = volume.converge(&store).unwrap();
        assert!(pass.failed.is_empty(), "{pass:?}");
        let tmp = dir.join("pass-us.tmp");
        std::fs::write(&tmp, start.elapsed().as_micros().to_string()).unwrap();
        std::fs::rename(tmp, dir.join("pass-us")).unwrap();
        std::fs::remove_file(dir.join("in-pass")).unwrap();
    }
}

#[allow(clippy::zombie_processes)] // the caller kills and waits for it
fn spawn_child(dir: &Path) -> Child {
    for f in ["ready", "in-pass"] {
        let _ = std::fs::remove_file(dir.join(f));
    }
    let child = Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "child_materialiser", "--nocapture"])
        .env(DIR_ENV, dir)
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(dir.join("child-stderr")).unwrap())
        .spawn()
        .unwrap();
    let start = Instant::now();
    while !dir.join("ready").exists() {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "child did not start"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    child
}

/// Wait until the child has a tree staged (it is inside a materialisation), then a
/// random part of half the last pass's length, so the kill lands somewhere between the
/// checkout and the removal of the replaced tree. Panics, with the child's stderr, if the
/// child exits on its own.
fn aim(dir: &Path, child: &mut Child, seed: u64) {
    let staging = dir.join("volume/.cuenca/staging");
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            let err = std::fs::read_to_string(dir.join("child-stderr")).unwrap_or_default();
            panic!("the child exited on its own ({status}):\n{err}");
        }
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "no tree staged in 60 s"
        );
        let staged = std::fs::read_dir(&staging)
            .map(|mut d| d.next().is_some())
            .unwrap_or(false);
        if staged && dir.join("in-pass").exists() {
            break;
        }
        std::thread::sleep(Duration::from_micros(100));
    }
    let last: u64 = std::fs::read_to_string(dir.join("pass-us"))
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50_000);
    std::thread::sleep(Duration::from_micros(seed % (last / 2).max(1)));
}

/// Kill the materialiser at random moments inside a pass, until 10 kills have left a
/// tree staged (at most [`ATTEMPTS`]). After each kill, before anything
/// restarts: every shown directory holds, whole, a tree the log has held for it. Then a
/// reopened volume converges to the log and leaves no staging behind.
#[test]
fn killed_while_materialising() {
    let dir = tempfile::tempdir().unwrap();
    setup(dir.path());
    let mut seed = std::process::id() as u64;
    let mut killed_mid_pass = 0;
    let mut kill = 0;
    while killed_mid_pass < KILLS {
        assert!(
            kill < ATTEMPTS,
            "only {killed_mid_pass} of {kill} kills landed inside a materialisation"
        );
        kill += 1;
        let mut child = spawn_child(dir.path());
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        // Let a pass or two finish first, so the last pass's length is known.
        std::thread::sleep(Duration::from_millis(200));
        aim(dir.path(), &mut child, seed >> 33);
        child.kill().unwrap();
        child.wait().unwrap();

        let store = Store::open(dir.path().join("store")).unwrap();
        let root = dir.path().join("volume");
        if std::fs::read_dir(root.join(".cuenca/staging"))
            .unwrap()
            .next()
            .is_some()
        {
            killed_mid_pass += 1;
        }
        // Every tree the fork has held.
        let mut held = vec![store.fork(&fork("f1")).unwrap().unwrap().base];
        for record in store.fork_history(&fork("f1")).unwrap() {
            if let cuenca_core::Event::DraftPublished { changeset, .. } = record.event {
                held.push(changeset.target);
            }
        }
        let shown = root.join("forks/f1");
        // Once shown (by the first kill's convergence at the latest), never gone.
        assert!(
            kill == 1 || shown.exists(),
            "kill {kill}: the fork vanished"
        );
        if shown.exists() {
            let tree = read_back(&shown);
            assert!(
                held.contains(&tree),
                "kill {kill}: a tree the log never held is shown"
            );
        }
        let main = store.location(&loc("site")).unwrap().unwrap().main;
        if root.join("main/site").exists() {
            assert_eq!(read_back(&root.join("main/site")), main, "kill {kill}");
        }

        let mut volume = Volume::open(&root, store.object_dir()).unwrap();
        assert!(staging_is_empty(&root), "kill {kill}: staging left");
        let pass = volume.converge(&store).unwrap();
        assert!(pass.failed.is_empty(), "kill {kill}: {pass:?}");
        let head = store.fork(&fork("f1")).unwrap().unwrap().head;
        assert_eq!(read_back(&shown), head, "kill {kill}: did not converge");
        assert_eq!(read_back(&root.join("main/site")), main, "kill {kill}");
    }
    eprintln!("{kill} kills, {killed_mid_pass} left a tree staged mid-pass");
}
