//! Verify records (spec rev 2.2 sections 4 and 6): the checks run are the base's, read
//! by the store from the location's main, never the candidate's; only the fork's holder
//! records one, on the fork's head, against the main of the moment.

mod common;

use common::*;
use cuenca_core::checks::{Check, CheckRun, CheckStatus, MANIFEST_PATH, Manifest};
use cuenca_core::{ChangesetId, CoreError, Event, Store, Writer};
use cuenca_tree::{Entry, GitOid, Tree};

const BASE: &[u8] = b"[[check]]\nname = \"gate\"\nargv = [\"sh\", \"-c\", \"exit 3\"]\n";

fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    (dir, store)
}

/// Location `l` whose main holds `files`, and fork `f` held by [`agent`] with one draft
/// that writes `edit`. Returns the draft's id.
fn forked(
    files: &[(&str, &[u8])],
    edit: &[(&str, &[u8])],
) -> (tempfile::TempDir, Store, Tree, ChangesetId) {
    let (d, store) = store();
    let main = tree(&store, files);
    store.add_location(&loc("l"), None, &main).unwrap();
    store.create_fork(&fork("f"), &loc("l"), &agent()).unwrap();
    let cs = write(&store, &main, edit);
    let id = store.publish_draft(&fork("f"), &w1(), &cs).unwrap().id;
    (d, store, main, id)
}

fn run(check: &Check, status: CheckStatus, exit_code: Option<i32>) -> CheckRun {
    CheckRun {
        check: check.clone(),
        status,
        exit_code,
        duration_ms: 1,
    }
}

/// The base's checks, each reported as `status`.
fn ran(manifest: &[u8], status: CheckStatus, exit_code: Option<i32>) -> Vec<CheckRun> {
    Manifest::parse(manifest)
        .unwrap()
        .checks
        .iter()
        .map(|c| run(c, status, exit_code))
        .collect()
}

#[test]
fn a_verify_is_judged_by_the_bases_manifest_and_records_its_hash() {
    let weakened: &[u8] = b"[[check]]\nname = \"gate\"\nargv = [\"true\"]\n";
    let (_d, store, main, id) = forked(
        &[("a", b"1"), (MANIFEST_PATH, BASE)],
        &[(MANIFEST_PATH, weakened)],
    );
    let base = main.git_tree_id();
    let before = store.len().unwrap();

    // The candidate's own list is not the base's: refused, nothing recorded.
    let theirs = ran(weakened, CheckStatus::Passed, Some(0));
    let e = store
        .record_verify(&fork("f"), &w1(), id, base, theirs)
        .unwrap_err();
    assert!(matches!(e, CoreError::ManifestMismatch(_)), "{e}");
    assert_eq!(store.len().unwrap(), before);

    // The base's list, as it ran: recorded, with the base manifest's id and SHA-256.
    let ours = ran(BASE, CheckStatus::Failed, Some(3));
    let (offset, manifest) = store
        .record_verify(&fork("f"), &w1(), id, base, ours.clone())
        .unwrap();
    let manifest = manifest.unwrap();
    assert_eq!(manifest.oid, GitOid::for_blob(BASE));
    assert_eq!(manifest.sha256, sha256(BASE));
    let record = store.records(offset, 1).unwrap().remove(0);
    let head = store.fork(&fork("f")).unwrap().unwrap().head;
    assert_eq!(
        record.event,
        Event::VerifyRecorded {
            fork: fork("f"),
            writer: w1(),
            changeset: id,
            candidate: head,
            base,
            manifest: Some(manifest),
            checks: ours.clone(),
        }
    );
    assert!(store.fork_history(&fork("f")).unwrap().contains(&record));

    // A second run is a second record; the log replays.
    let (again, _) = store
        .record_verify(&fork("f"), &w1(), id, base, ours)
        .unwrap();
    assert_eq!(again, offset + 1);
    store.replay_check().unwrap();
}

#[test]
fn the_reported_list_must_be_the_bases_exactly() {
    let two: &[u8] = b"[[check]]\nname = \"a\"\nargv = [\"true\"]\n[[check]]\nname = \"b\"\nargv = [\"x\", \"y\"]\ntimeout_secs = 9\n";
    let (_d, store, main, id) = forked(&[(MANIFEST_PATH, two)], &[("n", b"n")]);
    let base = main.git_tree_id();
    let good = ran(two, CheckStatus::Passed, Some(0));
    let mut variants: Vec<Vec<CheckRun>> = vec![
        vec![],
        good[..1].to_vec(),
        vec![good[1].clone(), good[0].clone()],
        [good.clone(), good[..1].to_vec()].concat(),
    ];
    let mut v = good.clone();
    v[1].check.argv = vec!["x".into()];
    variants.push(v);
    let mut v = good.clone();
    v[1].check.timeout_secs = 10;
    variants.push(v);
    let mut v = good.clone();
    v[0].check.name = "c".into();
    variants.push(v);
    for (i, checks) in variants.into_iter().enumerate() {
        let e = store
            .record_verify(&fork("f"), &w1(), id, base, checks)
            .unwrap_err();
        assert!(
            matches!(e, CoreError::ManifestMismatch(_)),
            "variant {i}: {e}"
        );
    }
    store
        .record_verify(&fork("f"), &w1(), id, base, good)
        .unwrap();
}

#[test]
fn a_base_with_no_manifest_has_no_checks() {
    let (_d, store, main, id) = forked(&[("a", b"1")], &[(MANIFEST_PATH, BASE)]);
    let base = main.git_tree_id();
    let e = store
        .record_verify(
            &fork("f"),
            &w1(),
            id,
            base,
            ran(BASE, CheckStatus::Failed, Some(3)),
        )
        .unwrap_err();
    assert!(matches!(e, CoreError::ManifestMismatch(_)), "{e}");
    let (_, manifest) = store
        .record_verify(&fork("f"), &w1(), id, base, vec![])
        .unwrap();
    assert_eq!(manifest, None);
}

#[test]
fn a_base_manifest_that_does_not_parse_or_is_no_file_is_refused() {
    for (what, main) in [
        ("garbage", vec![(MANIFEST_PATH, &b"[[check]\n"[..])]),
        (
            "unknown key",
            vec![(
                MANIFEST_PATH,
                &b"[[check]]\nname = \"t\"\nargv = [\"true\"]\nshell = true\n"[..],
            )],
        ),
    ] {
        let (_d, store, main, id) = forked(&main, &[("n", b"n")]);
        let e = store
            .record_verify(&fork("f"), &w1(), id, main.git_tree_id(), vec![])
            .unwrap_err();
        assert!(matches!(e, CoreError::BadManifest(_)), "{what}: {e}");
    }

    let (_d, store) = store();
    // The link's target, read as a manifest, would parse (a comment): it is refused
    // for being a link, not for its content.
    let target = ingest(&store, b"#elsewhere.toml");
    let main = Tree::from_entries([
        (path("elsewhere.toml"), Entry::File(ingest(&store, BASE))),
        (path(MANIFEST_PATH), Entry::Symlink(target)),
    ])
    .unwrap();
    store.add_location(&loc("l"), None, &main).unwrap();
    store.create_fork(&fork("f"), &loc("l"), &agent()).unwrap();
    let id = store
        .publish_draft(&fork("f"), &w1(), &write(&store, &main, &[("n", b"n")]))
        .unwrap()
        .id;
    let e = store
        .record_verify(&fork("f"), &w1(), id, main.git_tree_id(), vec![])
        .unwrap_err();
    assert!(matches!(e, CoreError::BadManifest(_)), "symlink: {e}");
}

#[test]
fn only_the_holder_records_on_the_head_against_the_main_of_the_moment() {
    let (_d, store, main, id) = forked(&[(MANIFEST_PATH, BASE)], &[("n", b"n")]);
    let base = main.git_tree_id();
    let checks = || ran(BASE, CheckStatus::Failed, Some(3));
    let before = store.len().unwrap();
    let refused = |writer: &Writer, id: ChangesetId, base: GitOid, checks: Vec<CheckRun>| {
        store
            .record_verify(&fork("f"), writer, id, base, checks)
            .unwrap_err()
    };

    let other = Writer::new(holder("other", "nOtherCNTRL"), 1);
    assert!(matches!(
        refused(&other, id, base, checks()),
        CoreError::NotHolder { .. }
    ));
    let elsewhere = Writer::new(holder("agent", "nElsewhereCNTRL"), 1);
    assert!(matches!(
        refused(&elsewhere, id, base, checks()),
        CoreError::NotHolder { .. }
    ));

    // A changeset that is not the head.
    let first = id;
    let next = write(
        &store,
        &store.fork_head(&fork("f")).unwrap(),
        &[("m", b"m")],
    );
    let id = store.publish_draft(&fork("f"), &w1(), &next).unwrap().id;
    assert!(matches!(
        refused(&w1(), first, base, checks()),
        CoreError::NotHead { .. }
    ));
    let before = before + 1;

    // A base that is not the location's main.
    let e = refused(
        &w1(),
        id,
        store.fork(&fork("f")).unwrap().unwrap().head,
        checks(),
    );
    assert!(matches!(e, CoreError::BaseMoved { .. }), "{e}");

    // A run whose status and exit code disagree.
    for (status, code) in [
        (CheckStatus::Passed, Some(3)),
        (CheckStatus::Passed, None),
        (CheckStatus::Failed, Some(0)),
        (CheckStatus::TimedOut, Some(1)),
        (CheckStatus::NotRun, Some(0)),
    ] {
        let e = refused(&w1(), id, base, ran(BASE, status, code));
        assert!(
            matches!(e, CoreError::BadCheckRun { .. }),
            "{status:?} {code:?}: {e}"
        );
    }
    assert_eq!(store.len().unwrap(), before);
    store
        .record_verify(&fork("f"), &w1(), id, base, checks())
        .unwrap();

    // Taken over: the old holder's epoch is stale.
    let owner = holder("owner", "nOwnerCNTRL");
    store.take_over(&fork("f"), 1, &owner).unwrap();
    assert!(matches!(
        refused(&w1(), id, base, checks()),
        CoreError::StaleEpoch { .. }
    ));
    let w2 = Writer::new(owner.clone(), 2);
    store
        .record_verify(&fork("f"), &w2, id, base, checks())
        .unwrap();

    // Revoked: refused.
    store.revoke(&owner.principal).unwrap();
    assert!(matches!(
        refused(&w2, id, base, checks()),
        CoreError::Revoked(_)
    ));
    store.replay_check().unwrap();
}

#[test]
fn once_main_moves_a_verify_against_the_old_main_is_refused() {
    let (_d, store, main, id) = forked(&[(MANIFEST_PATH, BASE)], &[("n", b"n")]);
    let old = main.git_tree_id();
    store.create_fork(&fork("g"), &loc("l"), &agent()).unwrap();
    let g = store
        .publish_draft(&fork("g"), &w1(), &write(&store, &main, &[("g", b"g")]))
        .unwrap()
        .id;
    store
        .record_verify(
            &fork("g"),
            &w1(),
            g,
            old,
            ran(BASE, CheckStatus::Passed, Some(0)),
        )
        .unwrap();
    ask_landing(&store, &fork("g"), &w1(), g).unwrap();
    store.advance_main(&fork("g"), g).unwrap();
    let e = store
        .record_verify(
            &fork("f"),
            &w1(),
            id,
            old,
            ran(BASE, CheckStatus::Failed, Some(3)),
        )
        .unwrap_err();
    match e {
        CoreError::BaseMoved { main, base, .. } => {
            assert_eq!(base, old);
            assert_ne!(main, old);
        }
        e => panic!("{e}"),
    }
}
