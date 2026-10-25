//! Owner forks in the volume (BRO-3256 parts 2 and 3, spec section 2a): shown once,
//! writable, never replaced; what Finder writes there is snapshotted into the fork's
//! drafts by the authority, under the `.forkignore` the fork holds; and forking one
//! snapshots it first.

mod common;

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::time::{Duration, Instant};

use common::*;
use cuenca_core::{CoreError, ForkName, Holder, LocationName, Store};
use cuenca_server::PrincipalKind::{Agent, OwnerDevice};
use cuenca_server::admin::AdminRequest;
use cuenca_server::materialise::{Materialiser, Shown, Volume, VolumeConfig};
use cuenca_tree::{Entry, GitOid, Mode, Op, RepoPath, Tree};

fn loc(s: &str) -> LocationName {
    LocationName::new(s).unwrap()
}

fn fork(s: &str) -> ForkName {
    ForkName::new(s).unwrap()
}

fn path(s: &str) -> RepoPath {
    RepoPath::new(s).unwrap()
}

fn ingest(store: &Store, content: &[u8]) -> GitOid {
    let oid = GitOid::for_blob(content);
    store
        .ingest_blob(oid, content.len() as u64, content)
        .unwrap();
    oid
}

/// A store with location `site` (main: `README`, an executable `run` and a symlink
/// `link`) and the owner fork `owner-mac` of it.
fn setup(dir: &Path) -> Store {
    let store = Store::open(dir.join("store")).unwrap();
    let main = Tree::from_entries([
        (path("README"), Entry::File(ingest(&store, b"main\n"))),
        (
            path("run"),
            Entry::Executable(ingest(&store, b"#!/bin/sh\n")),
        ),
        (path("link"), Entry::Symlink(ingest(&store, b"README"))),
    ])
    .unwrap();
    store.add_location(&loc("site"), None, &main).unwrap();
    store
        .create_fork(&fork("owner-mac"), &loc("site"), &Holder::volume())
        .unwrap();
    store
}

fn mode(p: &Path) -> u32 {
    std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o7777
}

fn owner() -> Shown {
    Shown::Fork(fork("owner-mac"))
}

/// The ops of the owner fork's latest draft, as `(path, Some(mode))` for a put and
/// `(path, None)` for a delete.
fn last_ops(store: &Store) -> Vec<(String, Option<Mode>)> {
    let f = store.fork(&fork("owner-mac")).unwrap().unwrap();
    let (id, _) = f.head_changeset.expect("a draft");
    store
        .changeset(id)
        .unwrap()
        .unwrap()
        .ops
        .iter()
        .map(|op| match op {
            Op::Put { path, entry } => (path.as_str().to_string(), Some(entry.mode())),
            Op::Delete { path } => (path.as_str().to_string(), None),
            _ => unreachable!(),
        })
        .collect()
}

#[test]
fn an_owner_fork_is_shown_once_writable_and_never_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let store = setup(dir.path());
    let root = dir.path().join("volume");
    let mut volume = Volume::open(&root, store.object_dir()).unwrap();
    let pass = volume.converge(&store).unwrap();
    assert_eq!(pass.built.len(), 2, "{pass:?}");
    let shown = root.join("forks/owner-mac");
    assert_eq!(mode(&shown), 0o2770);
    assert_eq!(mode(&shown.join("README")), 0o660);
    assert_eq!(mode(&shown.join("run")), 0o770);
    assert_eq!(
        std::fs::read_link(shown.join("link")).unwrap(),
        Path::new("README")
    );
    // main/ and forks/ themselves stay read-only to the group.
    assert_eq!(mode(&root.join("forks")), 0o2750);
    assert_eq!(mode(&root.join("main/site")), 0o2750);
    let inode = std::fs::metadata(&shown).unwrap().ino();

    // Finder writes; the snapshot publishes it; the volume does not replace the folder.
    std::fs::write(shown.join("notes.md"), b"from finder\n").unwrap();
    let snapped = volume.snapshot_owner_forks(&store).unwrap();
    assert_eq!(snapped.published.len(), 1, "{snapped:?}");
    assert!(snapped.failed.is_empty(), "{snapped:?}");
    assert_eq!(
        last_ops(&store),
        vec![("notes.md".into(), Some(Mode::File))]
    );
    let pass = volume.converge(&store).unwrap();
    assert!(pass.built.is_empty() && pass.failed.is_empty(), "{pass:?}");
    assert_eq!(pass.unchanged, 2);
    assert_eq!(std::fs::metadata(&shown).unwrap().ino(), inode, "replaced");

    // An edit the snapshot has not taken yet survives every converge, the log moving
    // included.
    std::fs::write(shown.join("draft.md"), b"unsaved\n").unwrap();
    for _ in 0..3 {
        let pass = volume.converge(&store).unwrap();
        assert!(pass.built.is_empty() && pass.failed.is_empty(), "{pass:?}");
    }
    assert_eq!(std::fs::read(shown.join("draft.md")).unwrap(), b"unsaved\n");
    assert_eq!(std::fs::metadata(&shown).unwrap().ino(), inode);

    // Nothing changed since: nothing published.
    volume.snapshot_owner_forks(&store).unwrap();
    let snapped = volume.snapshot_owner_forks(&store).unwrap();
    assert!(snapped.published.is_empty(), "{snapped:?}");
    assert_eq!(snapped.clean, 1);
}

/// A materialiser killed between showing an owner fork and recording it finds the
/// folder there with no record: it adopts it, edits and all, and never rebuilds it.
#[test]
fn an_owner_fork_without_a_record_is_adopted_not_rebuilt() {
    let dir = tempfile::tempdir().unwrap();
    let store = setup(dir.path());
    let root = dir.path().join("volume");
    let mut volume = Volume::open(&root, store.object_dir()).unwrap();
    volume.converge(&store).unwrap();
    let shown = root.join("forks/owner-mac");
    std::fs::write(shown.join("edit.md"), b"kept\n").unwrap();
    drop(volume);
    std::fs::remove_file(root.join(".cuenca/state/forks/owner-mac")).unwrap();

    let mut volume = Volume::open(&root, store.object_dir()).unwrap();
    assert_eq!(volume.shown(&owner()), None);
    let pass = volume.converge(&store).unwrap();
    assert!(pass.built.is_empty() && pass.failed.is_empty(), "{pass:?}");
    assert!(volume.shown(&owner()).is_some(), "adopted");
    assert_eq!(std::fs::read(shown.join("edit.md")).unwrap(), b"kept\n");
    assert!(root.join(".cuenca/state/forks/owner-mac").exists());
}

/// Spec row 2 · D: an owner's edit through Finder is a draft in the log within 60 s
/// (here, with the authority's defaults: a pass every second, snapshots every 10 s).
#[test]
fn an_owner_edit_is_a_draft_within_60_s() {
    let s = Server::start();
    s.add_location("l1");
    // The owner act, on the admin socket; and an owner device to read the log with.
    let v = s.admin(AdminRequest::OwnerFork {
        location: loc("l1"),
        fork: fork("owner-mac"),
    });
    assert_eq!(v["ok"], true, "{v}");
    let mac = s.mint("mac", OwnerDevice, "nMacCNTRL", grant(&[], &[]));
    s.whois.node("nMacCNTRL");
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("volume");
    let materialiser =
        Materialiser::spawn(s.authority.clone(), VolumeConfig::new(root.clone())).unwrap();
    let shown = root.join("forks/owner-mac");
    let start = Instant::now();
    while !shown.is_dir() {
        assert!(start.elapsed() < Duration::from_secs(15), "not shown");
        std::thread::sleep(Duration::from_millis(50));
    }
    std::fs::write(shown.join("edit.md"), b"from finder\n").unwrap();
    let edited = Instant::now();
    loop {
        let tree = s.fork_head(&mac, "owner-mac");
        if tree.get("edit.md") == Some(&Entry::File(GitOid::for_blob(b"from finder\n"))) {
            break;
        }
        assert!(
            edited.elapsed() < Duration::from_secs(60),
            "no draft in 60 s"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    eprintln!(
        "owner edit drafted {:?} after it was written",
        edited.elapsed()
    );
    drop(materialiser);
    s.stop();
}

/// Spec row 2 · D: Finder's and iOS's metadata, in the fork and in a folder copied in,
/// never reaches a changeset.
#[test]
fn finder_metadata_never_reaches_a_changeset() {
    let dir = tempfile::tempdir().unwrap();
    let store = setup(dir.path());
    let root = dir.path().join("volume");
    let mut volume = Volume::open(&root, store.object_dir()).unwrap();
    volume.converge(&store).unwrap();
    let shown = root.join("forks/owner-mac");
    for (p, c) in [
        (".DS_Store", &b"\0\0\0\x01Bud1"[..]),
        ("._README", b"\0\x05\x16\x07"),
        ("copied/.DS_Store", b"\0\0\0\x01Bud1"),
        ("copied/._a.txt", b"\0\x05\x16\x07"),
        ("copied/a.txt", b"a\n"),
        ("copied/.fseventsd/x", b"x"),
        (".Spotlight-V100/store", b"s"),
        (".Trashes/501/old", b"t"),
    ] {
        let p = shown.join(p);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, c).unwrap();
    }
    let snapped = volume.snapshot_owner_forks(&store).unwrap();
    assert_eq!(snapped.published.len(), 1, "{snapped:?}");
    assert_eq!(
        last_ops(&store),
        vec![("copied/a.txt".into(), Some(Mode::File))]
    );
}

/// Spec row 2 · D: Finder overwriting a tracked symlink (it writes an XSym regular file)
/// or an executable (Samba's forced modes drop the bit): the next snapshot records the
/// change of kind or mode, 10 of 10, and changing them back is recorded too.
#[test]
fn a_symlink_or_executable_overwritten_from_finder_is_recorded_10_of_10() {
    let dir = tempfile::tempdir().unwrap();
    let store = setup(dir.path());
    let root = dir.path().join("volume");
    let mut volume = Volume::open(&root, store.object_dir()).unwrap();
    volume.converge(&store).unwrap();
    let shown = root.join("forks/owner-mac");
    for round in 0..10 {
        let xsym = format!("XSym\n0006\n{round:032}\nREADME\n");
        std::fs::remove_file(shown.join("link")).unwrap();
        std::fs::write(shown.join("link"), xsym).unwrap();
        std::fs::set_permissions(shown.join("run"), std::fs::Permissions::from_mode(0o660))
            .unwrap();
        let snapped = volume.snapshot_owner_forks(&store).unwrap();
        assert_eq!(snapped.published.len(), 1, "round {round}: {snapped:?}");
        assert_eq!(
            last_ops(&store),
            vec![
                ("link".into(), Some(Mode::File)),
                ("run".into(), Some(Mode::File)),
            ],
            "round {round}"
        );
        // And back: a symlink and an executable again.
        std::fs::remove_file(shown.join("link")).unwrap();
        std::os::unix::fs::symlink("README", shown.join("link")).unwrap();
        std::fs::set_permissions(shown.join("run"), std::fs::Permissions::from_mode(0o770))
            .unwrap();
        let snapped = volume.snapshot_owner_forks(&store).unwrap();
        assert_eq!(snapped.published.len(), 1, "round {round}: {snapped:?}");
        assert_eq!(
            last_ops(&store),
            vec![
                ("link".into(), Some(Mode::Symlink)),
                ("run".into(), Some(Mode::Executable)),
            ],
            "round {round}"
        );
    }
}

/// A file the authority cannot read fails the fork's snapshot loudly, naming it, and
/// publishes nothing, not even the fork's other edits; once readable, all of it goes.
/// (Unreadable to a non-root runner, as CI's are.)
#[test]
fn an_unreadable_file_fails_the_snapshot_loudly() {
    let dir = tempfile::tempdir().unwrap();
    let store = setup(dir.path());
    let root = dir.path().join("volume");
    let mut volume = Volume::open(&root, store.object_dir()).unwrap();
    volume.converge(&store).unwrap();
    let shown = root.join("forks/owner-mac");
    let len = store.len().unwrap();
    std::fs::write(shown.join("ok.md"), b"ok\n").unwrap();
    std::fs::write(shown.join("secret.md"), b"locked\n").unwrap();
    std::fs::set_permissions(
        shown.join("secret.md"),
        std::fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    let snapped = volume.snapshot_owner_forks(&store).unwrap();
    assert!(snapped.published.is_empty(), "{snapped:?}");
    assert_eq!(snapped.failed.len(), 1, "{snapped:?}");
    assert!(snapped.failed[0].1.contains("secret.md"), "{snapped:?}");
    assert_eq!(store.len().unwrap(), len, "nothing appended");

    std::fs::set_permissions(
        shown.join("secret.md"),
        std::fs::Permissions::from_mode(0o660),
    )
    .unwrap();
    let snapped = volume.snapshot_owner_forks(&store).unwrap();
    assert_eq!(snapped.published.len(), 1, "{snapped:?}");
    assert_eq!(
        last_ops(&store),
        vec![
            ("ok.md".into(), Some(Mode::File)),
            ("secret.md".into(), Some(Mode::File)),
        ]
    );
}

/// Only the volume writes an owner fork: an agent cannot create one, and the volume's
/// snapshots are drafts written by the volume under epoch 1.
#[test]
fn owner_fork_drafts_are_the_volumes() {
    let dir = tempfile::tempdir().unwrap();
    let store = setup(dir.path());
    assert!(matches!(
        store.create_fork(
            &fork("owner-ipad"),
            &loc("site"),
            &Holder::new(
                cuenca_core::PrincipalName::new("agent").unwrap(),
                cuenca_core::NodeId::new("nAgentCNTRL").unwrap(),
            ),
        ),
        Err(CoreError::OwnerFork(_))
    ));
    let root = dir.path().join("volume");
    let mut volume = Volume::open(&root, store.object_dir()).unwrap();
    volume.converge(&store).unwrap();
    std::fs::write(root.join("forks/owner-mac/x"), b"x").unwrap();
    volume.snapshot_owner_forks(&store).unwrap();
    let history = store.fork_history(&fork("owner-mac")).unwrap();
    let last = history.last().unwrap();
    match &last.event {
        cuenca_core::Event::DraftPublished { writer, .. } => {
            assert_eq!((writer.holder.clone(), writer.epoch), (Holder::volume(), 1));
        }
        e => panic!("{e:?}"),
    }
}

/// The admin verb makes owner forks only, and the volume's principal can be neither
/// minted nor revoked; an owner fork is not taken over.
#[test]
fn the_admin_socket_makes_owner_forks_and_keeps_the_volume_keyless() {
    let s = Server::start();
    s.add_location("l1");
    let volume = || cuenca_core::PrincipalName::new("volume").unwrap();
    let refused = [
        AdminRequest::OwnerFork {
            location: loc("l1"),
            fork: fork("mac"),
        },
        AdminRequest::KeyMint {
            principal: volume(),
            kind: Agent,
            node: cuenca_core::NodeId::new("nVolCNTRL").unwrap(),
            grant: grant(&["l1"], &[]),
        },
        AdminRequest::KeyRevoke {
            principal: volume(),
        },
    ];
    for req in refused {
        let v = s.admin(req);
        assert_eq!(v["ok"], false, "{v}");
    }
    let v = s.admin(AdminRequest::OwnerFork {
        location: loc("l1"),
        fork: fork("owner-mac"),
    });
    assert_eq!(v["ok"], true, "{v}");
    let v = s.admin(AdminRequest::TakeOver {
        fork: fork("owner-mac"),
        expected_epoch: 1,
        principal: cuenca_core::PrincipalName::new("owner").unwrap(),
        node: cuenca_core::NodeId::new("nMacCNTRL").unwrap(),
    });
    assert_eq!(v["ok"], false, "{v}");
    s.stop();
}

/// Over HTTP, no key creates an owner fork: an agent's request is refused as
/// forbidden, and nothing is appended.
#[test]
fn no_key_creates_an_owner_fork_over_http() {
    let s = Server::start();
    s.add_location("l1");
    let a1 = s.mint("a1", Agent, "nA1CNTRL", grant(&["l1"], &[]));
    s.whois.node("nA1CNTRL");
    let r = s.post(
        &a1,
        "/v1/forks",
        serde_json::json!({ "fork": "owner-a1", "location": "l1" }),
    );
    assert_eq!(r.status(), 403);
    let body: serde_json::Value = r.json().unwrap();
    assert_eq!(body["error"], "owner_fork", "{body}");
    let r = s.post(
        &a1,
        "/v1/forks",
        serde_json::json!({ "fork": "a1-work", "location": "l1" }),
    );
    assert_eq!(r.status(), 200);
    s.stop();
}

/// An authority without a volume refuses to fork an owner fork, since a fork of one
/// starts from its snapshot and only the volume takes one; nothing is appended.
#[test]
fn an_owner_fork_is_not_forked_without_a_volume() {
    let s = Server::start();
    s.add_location("l1");
    let v = s.admin(AdminRequest::OwnerFork {
        location: loc("l1"),
        fork: fork("owner-mac"),
    });
    assert_eq!(v["ok"], true, "{v}");
    let mac = s.mint("mac", OwnerDevice, "nMacCNTRL", grant(&[], &[]));
    s.whois.node("nMacCNTRL");
    let len = s.authority.store().len().unwrap();
    let r = fork_from(&s, &mac, "mac-1", "owner-mac");
    assert_eq!(r.status(), 409);
    let body: serde_json::Value = r.json().unwrap();
    assert_eq!(body["error"], "snapshot_failed", "{body}");
    assert!(
        body["message"].as_str().unwrap().contains("no volume"),
        "{body}"
    );
    assert_eq!(s.authority.store().len().unwrap(), len, "nothing appended");
    s.stop();
}

/// An authority with location `l1`, the owner fork `owner-mac` shown in a volume by a
/// materialiser whose own snapshots run once at start and then not for an hour (so
/// only a fork's snapshot-first takes the edits), and an owner device `mac`.
fn forking_setup() -> (
    Server,
    String,
    std::path::PathBuf,
    tempfile::TempDir,
    Materialiser,
) {
    let s = Server::start();
    s.add_location("l1");
    let v = s.admin(AdminRequest::OwnerFork {
        location: loc("l1"),
        fork: fork("owner-mac"),
    });
    assert_eq!(v["ok"], true, "{v}");
    let mac = s.mint("mac", OwnerDevice, "nMacCNTRL", grant(&[], &[]));
    s.whois.node("nMacCNTRL");
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("volume");
    let mut config = VolumeConfig::new(root.clone());
    config.snapshot_interval = Duration::from_secs(3600);
    let materialiser = Materialiser::spawn(s.authority.clone(), config).unwrap();
    let shown = root.join("forks/owner-mac");
    let start = Instant::now();
    while !shown.is_dir() {
        assert!(start.elapsed() < Duration::from_secs(15), "not shown");
        std::thread::sleep(Duration::from_millis(50));
    }
    // Past the materialiser's first pass, whose snapshot is its last for an hour.
    std::thread::sleep(Duration::from_millis(1500));
    (s, mac, shown, dir, materialiser)
}

fn fork_from(s: &Server, token: &str, name: &str, from: &str) -> reqwest::blocking::Response {
    s.post(
        token,
        "/v1/forks",
        serde_json::json!({ "fork": name, "from": from }),
    )
}

/// Spec row 2 · D: fork an owner fork that has edits not yet snapshotted; the new fork
/// contains them, 10 of 10. The fork starts from the snapshot the authority takes
/// first, in the owner fork's location, held by the device that forked it, and later
/// writes stay in the owner fork.
#[test]
fn forking_an_owner_fork_takes_its_unsnapshotted_edits_10_of_10() {
    let (s, mac, shown, _dir, materialiser) = forking_setup();
    let mut held = 0;
    for i in 0..10 {
        let name = format!("edit-{i}.md");
        let content = format!("unsnapshotted {i}\n");
        std::fs::write(shown.join(&name), &content).unwrap();
        let before = s.fork_head(&mac, "owner-mac");
        assert!(before.get(&name).is_none(), "already snapshotted");
        let r = fork_from(&s, &mac, &format!("mac-{i}"), "owner-mac");
        assert_eq!(r.status(), 200, "{}", r.text().unwrap());
        let body: serde_json::Value = r.json().unwrap();
        assert_eq!(body["fork"]["location"], "l1", "{body}");
        assert_eq!(body["fork"]["from"], "owner-mac", "{body}");
        let tree = s.fork_head(&mac, &format!("mac-{i}"));
        // Every edit so far, this one included.
        let all = (0..=i).all(|j| {
            tree.get(&format!("edit-{j}.md"))
                == Some(&Entry::File(GitOid::for_blob(
                    format!("unsnapshotted {j}\n").as_bytes(),
                )))
        });
        if all {
            held += 1;
        }
        // The owner fork's head is the snapshot the fork started from.
        let f = s
            .authority
            .store()
            .fork(&fork(&format!("mac-{i}")))
            .unwrap()
            .unwrap();
        let owner = s
            .authority
            .store()
            .fork(&fork("owner-mac"))
            .unwrap()
            .unwrap();
        assert_eq!((f.base, f.head), (owner.head, owner.head));
        assert_eq!(f.holder().unwrap().principal.as_str(), "mac");
    }
    assert_eq!(held, 10, "forks holding every unsnapshotted edit");
    // A write after the last fork stays in the owner fork, not in the forks.
    std::fs::write(shown.join("later.md"), b"later\n").unwrap();
    let r = fork_from(&s, &mac, "mac-last", "owner-mac");
    assert_eq!(r.status(), 200);
    assert!(s.fork_head(&mac, "mac-9").get("later.md").is_none());
    assert!(s.fork_head(&mac, "owner-mac").get("later.md").is_some());
    drop(materialiser);
    s.stop();
}

/// Spec row 2 · D: fork an owner fork into a checkout while Finder on another device
/// keeps writing it. Each fork holds every write that finished before it was asked
/// for, whole; every write is in the owner fork at the end; 0 lost, 10 of 10.
#[test]
fn forking_an_owner_fork_while_it_is_written_loses_nothing_10_of_10() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let (s, mac, shown, _dir, materialiser) = forking_setup();
    let done = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let (done, stop, shown) = (done.clone(), stop.clone(), shown.clone());
        std::thread::spawn(move || {
            let mut n = 0;
            while !stop.load(Ordering::SeqCst) {
                // In two parts, as a client writes a file larger than one request, so a
                // snapshot can meet it half written.
                use std::io::Write as _;
                let mut f = std::fs::File::create(shown.join(format!("w{n:04}.md"))).unwrap();
                let whole = format!("write {n}\n");
                let (head, tail) = whole.as_bytes().split_at(3);
                f.write_all(head).unwrap();
                f.sync_all().unwrap();
                std::thread::sleep(Duration::from_millis(2));
                f.write_all(tail).unwrap();
                drop(f);
                n += 1;
                done.store(n, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(3));
            }
            n
        })
    };
    let content = |n: usize| Entry::File(GitOid::for_blob(format!("write {n}\n").as_bytes()));
    let mut held = 0;
    for i in 0..10 {
        std::thread::sleep(Duration::from_millis(100));
        let finished = done.load(Ordering::SeqCst);
        let r = fork_from(&s, &mac, &format!("mac-{i}"), "owner-mac");
        assert_eq!(r.status(), 200, "{}", r.text().unwrap());
        let tree = s.fork_head(&mac, &format!("mac-{i}"));
        let all = (0..finished).all(|n| tree.get(&format!("w{n:04}.md")) == Some(&content(n)));
        if all {
            held += 1;
        }
    }
    stop.store(true, Ordering::SeqCst);
    let written = writer.join().unwrap();
    assert_eq!(held, 10, "forks holding every finished write, whole");
    // A write still in progress at a snapshot may be taken as far as it had gone (the
    // stated limit, BRO-3450); its finished version is the owner fork's, checked below.
    // Every write reaches the owner fork: one more fork snapshots it first.
    assert_eq!(fork_from(&s, &mac, "mac-end", "owner-mac").status(), 200);
    let owner = s.fork_head(&mac, "owner-mac");
    let lost: Vec<usize> = (0..written)
        .filter(|&n| owner.get(&format!("w{n:04}.md")) != Some(&content(n)))
        .collect();
    assert!(
        lost.is_empty(),
        "lost {} of {written}: {lost:?}",
        lost.len()
    );
    eprintln!("{written} writes while forking 10 times; 0 lost");
    drop(materialiser);
    s.stop();
}

/// An owner fork that cannot be snapshotted is not forked: the fork would lack what the
/// owner wrote. The request is refused (409 `snapshot_failed`, naming the file) and
/// nothing is appended; once the file is readable, the fork is made.
#[test]
fn an_owner_fork_that_cannot_be_snapshotted_is_not_forked() {
    if rustix::process::geteuid().is_root() {
        return; // root reads a mode 000 file
    }
    let (s, mac, shown, _dir, materialiser) = forking_setup();
    std::fs::write(shown.join("secret.md"), b"locked\n").unwrap();
    std::fs::set_permissions(
        shown.join("secret.md"),
        std::fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    let len = s.authority.store().len().unwrap();
    let r = fork_from(&s, &mac, "mac-1", "owner-mac");
    assert_eq!(r.status(), 409);
    let body: serde_json::Value = r.json().unwrap();
    assert_eq!(body["error"], "snapshot_failed", "{body}");
    assert!(
        body["message"].as_str().unwrap().contains("secret.md"),
        "{body}"
    );
    // The fork's own path, not where the volume is on the authority's host.
    let message = body["message"].as_str().unwrap();
    assert!(!message.contains("/forks/owner-mac/"), "{message}");
    assert_eq!(s.authority.store().len().unwrap(), len, "nothing appended");
    std::fs::set_permissions(
        shown.join("secret.md"),
        std::fs::Permissions::from_mode(0o660),
    )
    .unwrap();
    let r = fork_from(&s, &mac, "mac-1", "owner-mac");
    assert_eq!(r.status(), 200);
    assert!(s.fork_head(&mac, "mac-1").get("secret.md").is_some());
    drop(materialiser);
    s.stop();
}

/// Forking a fork reads it: an agent forks only a fork it reads (an owner fork it is
/// not granted is not found), and a request names exactly one of `location` and
/// `from`.
#[test]
fn forking_a_fork_needs_reading_it() {
    let s = Server::start();
    s.add_location("l1");
    let v = s.admin(AdminRequest::OwnerFork {
        location: loc("l1"),
        fork: fork("owner-mac"),
    });
    assert_eq!(v["ok"], true, "{v}");
    let a1 = s.mint("a1", Agent, "nA1CNTRL", grant(&["l1"], &[]));
    s.whois.node("nA1CNTRL");
    let len = s.authority.store().len().unwrap();
    assert_eq!(fork_from(&s, &a1, "a1-copy", "owner-mac").status(), 404);
    assert_eq!(fork_from(&s, &a1, "a1-copy", "nope").status(), 404);
    let both = s.post(
        &a1,
        "/v1/forks",
        serde_json::json!({ "fork": "a1-copy", "from": "owner-mac", "location": "l1" }),
    );
    assert_eq!(both.status(), 400);
    let neither = s.post(&a1, "/v1/forks", serde_json::json!({ "fork": "a1-copy" }));
    assert_eq!(neither.status(), 400);
    assert_eq!(s.authority.store().len().unwrap(), len);
    // A fork granted without its location is not forked: its location is not read.
    let a3 = s.mint("a3", Agent, "nA3CNTRL", grant(&[], &["owner-mac"]));
    s.whois.node("nA3CNTRL");
    assert_eq!(fork_from(&s, &a3, "a3-copy", "owner-mac").status(), 404);
    // An agent granted the owner fork reads it, so its request reaches the snapshot
    // (which needs a volume, and there is none here).
    let a2 = s.mint("a2", Agent, "nA2CNTRL", grant(&["l1"], &["owner-mac"]));
    s.whois.node("nA2CNTRL");
    let r = fork_from(&s, &a2, "a2-copy", "owner-mac");
    assert_eq!(r.status(), 409);
    let body: serde_json::Value = r.json().unwrap();
    assert_eq!(body["error"], "snapshot_failed", "{body}");
    s.whois.node("nA1CNTRL");
    // Its own fork it reads, so it forks it.
    let r = s.post(
        &a1,
        "/v1/forks",
        serde_json::json!({ "fork": "a1-work", "location": "l1" }),
    );
    assert_eq!(r.status(), 200);
    let r = fork_from(&s, &a1, "a1-copy", "a1-work");
    assert_eq!(r.status(), 200, "{}", r.text().unwrap());
    s.stop();
}

/// A rule the owner adds to the fork's own `.forkignore` in Finder that names a tracked
/// path fails the snapshot loudly, naming the file, in the log and in a status file in
/// the volume, and publishes nothing (leaving the path out would delete it); once the
/// rule is gone, the edits are published and the status file is removed.
#[test]
fn a_fork_rule_naming_a_tracked_path_fails_the_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let store = setup(dir.path());
    let root = dir.path().join("volume");
    let mut volume = Volume::open(&root, store.object_dir()).unwrap();
    volume.converge(&store).unwrap();
    let shown = root.join("forks/owner-mac");
    let len = store.len().unwrap();
    std::fs::write(shown.join(".forkignore"), b"README\n").unwrap();
    std::fs::write(shown.join("notes.md"), b"notes\n").unwrap();
    let snapped = volume.snapshot_owner_forks(&store).unwrap();
    assert!(snapped.published.is_empty(), "{snapped:?}");
    assert_eq!(snapped.failed.len(), 1, "{snapped:?}");
    assert!(snapped.failed[0].1.contains("README"), "{snapped:?}");
    assert_eq!(store.len().unwrap(), len, "nothing appended");
    // The owner sees why in Finder: a read-only status file naming the path.
    let status = root.join("status/owner-mac.txt");
    let text = std::fs::read_to_string(&status).unwrap();
    assert!(
        text.contains("README") && text.contains("owner-mac"),
        "{text}"
    );
    assert_eq!(mode(&status) & 0o7777, 0o640);
    assert_eq!(mode(&root.join("status")) & 0o7777, 0o2750);
    // A pass failing the same way leaves it as it was.
    let before = std::fs::metadata(&status).unwrap().modified().unwrap();
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(volume.snapshot_owner_forks(&store).unwrap().failed.len(), 1);
    assert_eq!(
        std::fs::metadata(&status).unwrap().modified().unwrap(),
        before
    );

    std::fs::remove_file(shown.join(".forkignore")).unwrap();
    let snapped = volume.snapshot_owner_forks(&store).unwrap();
    assert_eq!(snapped.published.len(), 1, "{snapped:?}");
    assert!(
        !status.exists(),
        "a snapshot that succeeds removes the status file"
    );
    assert_eq!(
        last_ops(&store),
        vec![("notes.md".into(), Some(Mode::File))]
    );
}

/// A file still being written when the fork is asked for is taken as far as it has
/// been written (the stated limit; BRO-3450 makes snapshots wait for writes to
/// settle), and nothing is lost: the owner fork's next snapshot, and the next fork,
/// hold it whole.
#[test]
fn a_file_being_written_while_forked_is_whole_in_the_next_snapshot() {
    use std::io::Write as _;
    let (s, mac, shown, _dir, materialiser) = forking_setup();
    let whole = vec![b'x'; 8192];
    let mut f = std::fs::File::create(shown.join("big.bin")).unwrap();
    f.write_all(&whole[..4096]).unwrap();
    f.sync_all().unwrap();
    let r = fork_from(&s, &mac, "mac-1", "owner-mac");
    assert_eq!(r.status(), 200, "{}", r.text().unwrap());
    f.write_all(&whole[4096..]).unwrap();
    drop(f);
    let half = Entry::File(GitOid::for_blob(&[b'x'; 4096]));
    assert_eq!(s.fork_head(&mac, "mac-1").get("big.bin"), Some(&half));
    let big = Entry::File(GitOid::for_blob(&[b'x'; 8192]));
    assert_eq!(fork_from(&s, &mac, "mac-2", "owner-mac").status(), 200);
    assert_eq!(s.fork_head(&mac, "mac-2").get("big.bin"), Some(&big));
    assert_eq!(s.fork_head(&mac, "owner-mac").get("big.bin"), Some(&big));
    drop(materialiser);
    s.stop();
}

/// A fork the rules refuse (its name, or an owner fork's name for a device) is refused
/// before the owner fork is snapshotted: the owner fork gets no draft, the log does not
/// grow.
#[test]
fn a_refused_fork_of_an_owner_fork_snapshots_nothing() {
    let (s, mac, shown, _dir, materialiser) = forking_setup();
    let taken = s.post(
        &mac,
        "/v1/forks",
        serde_json::json!({ "fork": "taken", "location": "l1" }),
    );
    assert_eq!(taken.status(), 200);
    std::fs::write(shown.join("edit.md"), b"unsnapshotted\n").unwrap();
    let len = s.authority.store().len().unwrap();
    for (name, status) in [("owner-evil", 403), ("taken", 409), ("x-from-y", 400)] {
        let r = fork_from(&s, &mac, name, "owner-mac");
        assert_eq!(r.status(), status, "{name}: {}", r.text().unwrap());
    }
    assert_eq!(s.authority.store().len().unwrap(), len, "appended");
    drop(materialiser);
    s.stop();
}

/// Forking an owner fork holds no lock a revocation waits for while it snapshots:
/// revoking someone else meanwhile is immediate.
#[test]
fn a_revocation_does_not_wait_for_a_fork() {
    let (s, mac, shown, _dir, materialiser) = forking_setup();
    s.mint("other", Agent, "nOtherCNTRL", grant(&["l1"], &[]));
    // A file the authority cannot read keeps the fork in its snapshot attempts (100 ms
    // apart, for 5 s) for a while, then refuses it.
    std::fs::write(shown.join("secret.md"), b"locked\n").unwrap();
    std::fs::set_permissions(
        shown.join("secret.md"),
        std::fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    let url = s.url.clone();
    let forking = std::thread::spawn(move || {
        reqwest::blocking::Client::new()
            .post(format!("{url}/v1/forks"))
            .bearer_auth(&mac)
            .json(&serde_json::json!({ "fork": "mac-1", "from": "owner-mac" }))
            .send()
            .unwrap()
            .status()
    });
    std::thread::sleep(Duration::from_millis(100));
    // On a thread with a deadline: a revocation stuck behind the fork fails the test
    // rather than hanging it.
    let (tx, rx) = std::sync::mpsc::channel();
    let admin = s.admin.clone();
    std::thread::spawn(move || {
        let start = Instant::now();
        let v = cuenca_server::admin::call(
            &admin,
            &AdminRequest::KeyRevoke {
                principal: cuenca_core::PrincipalName::new("other").unwrap(),
            },
        )
        .unwrap();
        let _ = tx.send((v, start.elapsed()));
    });
    // Well under the fork's 5 s of attempts, and well over a loaded host's fsync.
    let Ok((v, took)) = rx.recv_timeout(Duration::from_secs(2)) else {
        // Stuck behind the fork. Exit at once: a server whose locks are stuck cannot
        // shut down, so failing the test the usual way would hang it instead.
        eprintln!("a_revocation_does_not_wait_for_a_fork: the revocation waited for the fork");
        std::process::exit(101);
    };
    assert_eq!(v["ok"], true, "{v}");
    eprintln!("revoked in {took:?} while a fork was snapshotting");
    assert_eq!(forking.join().unwrap(), 409);
    drop(materialiser);
    s.stop();
}

/// The volume is unlocked between a fork's snapshot attempts: while one fork keeps
/// failing its snapshot, a fork of another owner fork, which needs the volume too, is
/// made at once rather than after the first fork's 5 s.
#[test]
fn a_fork_retrying_its_snapshot_leaves_the_volume_to_others() {
    let (s, mac, shown, _dir, materialiser) = forking_setup();
    let v = s.admin(AdminRequest::OwnerFork {
        location: loc("l1"),
        fork: fork("owner-ipad"),
    });
    assert_eq!(v["ok"], true, "{v}");
    let ipad = shown.parent().unwrap().join("owner-ipad");
    let start = Instant::now();
    while !ipad.is_dir() {
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "owner-ipad not shown"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    std::fs::write(ipad.join("note.md"), b"from the ipad\n").unwrap();
    std::fs::write(shown.join("secret.md"), b"locked\n").unwrap();
    std::fs::set_permissions(
        shown.join("secret.md"),
        std::fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    let (url, token) = (s.url.clone(), mac.clone());
    let started = Instant::now();
    let stuck = std::thread::spawn(move || {
        let status = reqwest::blocking::Client::new()
            .post(format!("{url}/v1/forks"))
            .bearer_auth(&token)
            .json(&serde_json::json!({ "fork": "mac-1", "from": "owner-mac" }))
            .send()
            .unwrap()
            .status();
        (status, started.elapsed())
    });
    std::thread::sleep(Duration::from_millis(300));
    // Three forks one after another, so a lock held across the pauses would have to lose
    // the race to each of them for this to pass.
    let other = Instant::now();
    for name in ["mac-2", "mac-3", "mac-4"] {
        let r = fork_from(&s, &mac, name, "owner-ipad");
        assert_eq!(r.status(), 200, "{}", r.text().unwrap_or_default());
    }
    let took = other.elapsed();
    assert!(
        took < Duration::from_secs(3),
        "three forks waited {took:?} for the first fork's attempts"
    );
    let (status, first) = stuck.join().unwrap();
    assert_eq!(status, 409);
    // The second fork was made while the first was still trying.
    assert!(first > Duration::from_millis(300) + took, "{first:?}");
    drop(materialiser);
    s.stop();
}

/// A snapshot that fails is tried again before the fork is refused: a file unreadable
/// for a moment does not refuse the fork.
#[test]
fn a_snapshot_failing_once_is_tried_again() {
    let (s, mac, shown, _dir, materialiser) = forking_setup();
    let secret = shown.join("secret.md");
    std::fs::write(&secret, b"locked\n").unwrap();
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o000)).unwrap();
    // Readable again after the first attempt, before the last (100 ms apart).
    let unlock = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(250));
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o660)).unwrap();
    });
    let r = fork_from(&s, &mac, "mac-1", "owner-mac");
    unlock.join().unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().unwrap());
    assert!(s.fork_head(&mac, "mac-1").get("secret.md").is_some());
    drop(materialiser);
    s.stop();
}

/// R40 with duplicated content, as when a file is copied in Finder and both copies keep
/// changing: two paths always holding the same bytes are rewritten every few
/// milliseconds while the owner fork is forked 10 times, and every fork is made, 10 of
/// 10 (before, the scan listed their shared blob under one path, and a change to that
/// path left the other's blob unstaged: refused as a missing blob).
#[test]
fn forking_while_two_copies_keep_changing_is_never_refused_10_of_10() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let (s, mac, shown, _dir, materialiser) = forking_setup();
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let (stop, shown) = (stop.clone(), shown.clone());
        std::thread::spawn(move || {
            let mut k = 0u64;
            while !stop.load(Ordering::SeqCst) {
                let content = format!("content {k}\n");
                std::fs::write(shown.join("a.md"), &content).unwrap();
                std::fs::write(shown.join("b.md"), &content).unwrap();
                k += 1;
                // A client's pace: a file rewritten with no pause at all is one that
                // "keeps changing", refused loudly, as before this change.
                std::thread::sleep(Duration::from_millis(5));
            }
        })
    };
    let mut made = 0;
    for i in 0..10 {
        std::thread::sleep(Duration::from_millis(50));
        let r = fork_from(&s, &mac, &format!("mac-{i}"), "owner-mac");
        if r.status() == 200 {
            made += 1;
        } else {
            eprintln!("fork {i}: {} {}", r.status(), r.text().unwrap());
        }
    }
    stop.store(true, Ordering::SeqCst);
    writer.join().unwrap();
    assert_eq!(made, 10, "forks made while two copies kept changing");
    drop(materialiser);
    s.stop();
}
