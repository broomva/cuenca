//! Owner forks in the volume (BRO-3256 part 2, spec section 2a): shown once, writable,
//! never replaced; what Finder writes there is snapshotted into the fork's drafts by the
//! authority, under the location's `.forkignore`.

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

/// Over HTTP, no key creates an owner fork: an agent's or an owner device's request is
/// refused as forbidden, and nothing is appended.
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
