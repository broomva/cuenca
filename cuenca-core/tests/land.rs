//! Landing's recorded state machine (spec section 6): each step follows only its
//! predecessor, repeats only itself, and a remote decides which path a request takes.

mod common;

use common::*;
use cuenca_core::{ChangesetId, CoreError, Export, ForkName, LandingStep, PrincipalName, Store};
use cuenca_tree::GitOid;

const REMOTE: &str = "https://github.com/broomva/cuenca-land-test";

/// A location `l` (with `remote`) and fork `f` whose head draft `id` is verified.
fn verified(remote: Option<&str>) -> (tempfile::TempDir, Store, ChangesetId) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let main = tree(&store, &[("a", b"1")]);
    store.add_location(&loc("l"), remote, &main).unwrap();
    store.create_fork(&fork("f"), &loc("l"), &agent()).unwrap();
    let cs = write(&store, &main, &[("a", b"2")]);
    let id = store.publish_draft(&fork("f"), &w1(), &cs).unwrap().id;
    verify(&store, &fork("f"), &w1(), id);
    (dir, store, id)
}

fn export(id: ChangesetId) -> Export {
    Export::new(
        id,
        GitOid::for_blob(b"commit"),
        GitOid::for_blob(b"parent"),
        "main",
    )
}

fn step(store: &Store, f: &ForkName) -> LandingStep {
    store.fork(f).unwrap().unwrap().landing.unwrap().step
}

fn f() -> ForkName {
    fork("f")
}

#[test]
fn a_remote_landing_moves_one_step_at_a_time_and_each_retry_appends_nothing() {
    let (_d, store, id) = verified(Some(REMOTE));
    let e = export(id);
    // Nothing before the request.
    assert!(matches!(
        store.record_export_intent(&f(), id, &e),
        Err(CoreError::NoLanding { .. })
    ));
    store.record_landing_intent(&f(), &w1(), id).unwrap();
    assert_eq!(step(&store, &f()), LandingStep::Requested);
    // Out of order: each step needs the one before it.
    for out_of_order in [
        store.record_exported(&f(), id),
        store.record_review_intent(&f(), id),
        store.record_in_review(&f(), id, 7, "https://github.com/x/y/pull/7"),
    ] {
        assert!(
            matches!(out_of_order, Err(CoreError::LandingStep { .. })),
            "{out_of_order:?}"
        );
    }
    type Step<'a> = Box<dyn Fn() -> cuenca_core::Result<u64> + 'a>;
    let url = "https://github.com/broomva/cuenca-land-test/pull/7";
    let steps: Vec<(Step, &str)> = vec![
        (
            Box::new(|| store.record_export_intent(&f(), id, &e)),
            "exporting",
        ),
        (Box::new(|| store.record_exported(&f(), id)), "exported"),
        (
            Box::new(|| store.record_review_intent(&f(), id)),
            "reviewing",
        ),
        (
            Box::new(|| store.record_in_review(&f(), id, 7, url)),
            "in_review",
        ),
    ];
    for (record, name) in &steps {
        let at = record().unwrap();
        assert_eq!(at, store.len().unwrap() - 1, "{name} appended");
        assert_eq!(step(&store, &f()).name(), *name);
        // A retry of the step just taken answers its offset and appends nothing.
        assert_eq!(record().unwrap(), at, "{name} retried");
        assert_eq!(at, store.len().unwrap() - 1);
    }
    match step(&store, &f()) {
        LandingStep::InReview { export, pr, url: u } => {
            assert_eq!((export, pr, u.as_str()), (e.clone(), 7, url));
        }
        other => panic!("{other:?}"),
    }
    // An earlier step is not a retry once the landing has moved past it.
    assert!(matches!(
        store.record_exported(&f(), id),
        Err(CoreError::LandingStep { .. })
    ));
    assert!(matches!(
        store.record_in_review(&f(), id, 8, url),
        Err(CoreError::LandingStep { .. })
    ));
    // An open PR is not in flight any more: it cannot fail.
    assert!(matches!(
        store.record_landing_failed(&f(), id, "late"),
        Err(CoreError::LandingStep { .. })
    ));
    // The owner never moves main on a location with a remote.
    assert!(matches!(
        store.advance_main(&f(), id),
        Err(CoreError::HasRemote(_))
    ));
    // Asking again for the same changeset is a retry of the request.
    let request = store.fork(&f()).unwrap().unwrap().landing.unwrap().offset;
    assert_eq!(
        store.record_landing_intent(&f(), &w1(), id).unwrap(),
        request
    );
    store.replay_check().unwrap();
}

#[test]
fn an_export_names_the_changesets_own_branch() {
    let (_d, store, id) = verified(Some(REMOTE));
    store.record_landing_intent(&f(), &w1(), id).unwrap();
    let mut e = export(id);
    e = serde_json::from_value(serde_json::json!({
        "commit": e.commit.to_hex(),
        "parent": e.parent.to_hex(),
        "branch": "main",
        "base_ref": "main",
    }))
    .unwrap();
    assert!(matches!(
        store.record_export_intent(&f(), id, &e),
        Err(CoreError::Inconsistent(_))
    ));
    // A different export of the same request is not a retry.
    store.record_export_intent(&f(), id, &export(id)).unwrap();
    let other = Export::new(
        id,
        GitOid::for_blob(b"other"),
        GitOid::for_blob(b"parent"),
        "main",
    );
    assert!(matches!(
        store.record_export_intent(&f(), id, &other),
        Err(CoreError::LandingStep { .. })
    ));
}

#[test]
fn a_location_without_a_remote_waits_for_the_owner_and_keeps_the_old_main() {
    let (_d, store, id) = verified(None);
    let old = store.location(&loc("l")).unwrap().unwrap().main;
    store.record_landing_intent(&f(), &w1(), id).unwrap();
    // The authority's export steps do not apply.
    assert!(matches!(
        store.record_export_intent(&f(), id, &export(id)),
        Err(CoreError::NoRemote(_))
    ));
    assert_eq!(store.location(&loc("l")).unwrap().unwrap().main, old);
    let at = store.advance_main(&f(), id).unwrap();
    let new = store.location(&loc("l")).unwrap().unwrap().main;
    assert_ne!(new, old);
    assert_eq!(step(&store, &f()), LandingStep::Landed { offset: at });
    // The earlier main stays in the log and its tree in the store.
    assert_eq!(store.tree(old).unwrap().git_tree_id(), old);
    assert!(
        store
            .records(0, usize::MAX)
            .unwrap()
            .iter()
            .any(|r| matches!(
                &r.event,
                cuenca_core::Event::LocationAdded { main, .. } if *main == old
            ))
    );
    // A landed request is a retry, and cannot fail.
    assert_eq!(store.advance_main(&f(), id).unwrap(), at);
    assert!(matches!(
        store.record_landing_failed(&f(), id, "late"),
        Err(CoreError::LandingStep { .. })
    ));
    store.replay_check().unwrap();
}

#[test]
fn a_request_needs_a_verify_of_the_head_against_main_as_it_is_now() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let main = tree(&store, &[("a", b"1")]);
    store.add_location(&loc("l"), None, &main).unwrap();
    for name in ["f", "g", "h"] {
        store.create_fork(&fork(name), &loc("l"), &agent()).unwrap();
    }
    let f1 = write(&store, &main, &[("a", b"f")]);
    let pf = store.publish_draft(&f(), &w1(), &f1).unwrap();
    // A verify of an earlier head does not cover the next one.
    verify(&store, &f(), &w1(), pf.id);
    let f2 = write(&store, &store.fork_head(&f()).unwrap(), &[("a", b"f2")]);
    let pf2 = store.publish_draft(&f(), &w1(), &f2).unwrap();
    assert!(matches!(
        store.record_landing_intent(&f(), &w1(), pf2.id),
        Err(CoreError::NotVerified { .. })
    ));
    // g verifies, then main moves under it: its verify no longer covers it.
    let g1 = write(&store, &main, &[("g", b"g")]);
    let pg = store.publish_draft(&fork("g"), &w1(), &g1).unwrap();
    verify(&store, &fork("g"), &w1(), pg.id);
    let h1 = write(&store, &main, &[("h", b"h")]);
    let ph = store.publish_draft(&fork("h"), &w1(), &h1).unwrap();
    land(&store, &fork("h"), &w1(), ph.id);
    assert!(matches!(
        store.record_landing_intent(&fork("g"), &w1(), pg.id),
        Err(CoreError::NotVerified { .. })
    ));
}

#[test]
fn a_failed_landing_stops_and_asking_again_is_a_retry() {
    let (_d, store, id) = verified(Some(REMOTE));
    let request = store.record_landing_intent(&f(), &w1(), id).unwrap();
    store.record_export_intent(&f(), id, &export(id)).unwrap();
    let long = "€".repeat(cuenca_core::MAX_LANDING_TEXT);
    let at = store.record_landing_failed(&f(), id, &long).unwrap();
    match step(&store, &f()) {
        LandingStep::Failed { reason } => {
            assert!(reason.len() <= cuenca_core::MAX_LANDING_TEXT && reason.starts_with('€'));
            // The same failure again is a retry.
            assert_eq!(store.record_landing_failed(&f(), id, &reason).unwrap(), at);
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        store.record_exported(&f(), id),
        Err(CoreError::LandingStep { .. })
    ));
    assert_eq!(
        store.record_landing_intent(&f(), &w1(), id).unwrap(),
        request
    );
    store.replay_check().unwrap();
}

#[test]
fn a_revoked_holder_s_request_goes_no_further() {
    for at_step in 0..2 {
        let (_d, store, id) = verified(Some(REMOTE));
        store.record_landing_intent(&f(), &w1(), id).unwrap();
        if at_step == 1 {
            store.record_export_intent(&f(), id, &export(id)).unwrap();
            store.record_exported(&f(), id).unwrap();
        }
        store.revoke(&PrincipalName::new("agent").unwrap()).unwrap();
        let refused = if at_step == 0 {
            store.record_export_intent(&f(), id, &export(id))
        } else {
            store.record_review_intent(&f(), id)
        };
        assert!(matches!(refused, Err(CoreError::Revoked(_))), "{refused:?}");
        // It can still be marked failed.
        store.record_landing_failed(&f(), id, "revoked").unwrap();
    }
    let (_d, store, id) = verified(None);
    store.record_landing_intent(&f(), &w1(), id).unwrap();
    store.revoke(&PrincipalName::new("agent").unwrap()).unwrap();
    assert!(matches!(
        store.advance_main(&f(), id),
        Err(CoreError::Revoked(_))
    ));
}

#[test]
fn a_takeover_drops_a_landing_in_flight() {
    let (_d, store, id) = verified(Some(REMOTE));
    store.record_landing_intent(&f(), &w1(), id).unwrap();
    store.record_export_intent(&f(), id, &export(id)).unwrap();
    store
        .take_over(&f(), 1, &holder("owner", "nOwnerMacCNTRL"))
        .unwrap();
    assert_eq!(store.fork(&f()).unwrap().unwrap().landing, None);
    assert!(matches!(
        store.record_exported(&f(), id),
        Err(CoreError::NoLanding { .. })
    ));
    store.replay_check().unwrap();
}

#[test]
fn the_owner_lands_only_a_live_request_for_the_head() {
    // A draft after the request: the request is no longer the head's.
    let (_d, store, id) = verified(None);
    store.record_landing_intent(&f(), &w1(), id).unwrap();
    let next = write(&store, &store.fork_head(&f()).unwrap(), &[("a", b"3")]);
    store.publish_draft(&f(), &w1(), &next).unwrap();
    assert!(matches!(
        store.advance_main(&f(), id),
        Err(CoreError::NotHead { .. })
    ));
    // A failed request does not land.
    let (_d, store, id) = verified(None);
    store.record_landing_intent(&f(), &w1(), id).unwrap();
    store
        .record_landing_failed(&f(), id, "owner said no")
        .unwrap();
    assert!(matches!(
        store.advance_main(&f(), id),
        Err(CoreError::LandingStep { .. })
    ));
    store.replay_check().unwrap();
}

#[test]
fn a_pull_request_needs_a_number_and_a_url() {
    let (_d, store, id) = verified(Some(REMOTE));
    store.record_landing_intent(&f(), &w1(), id).unwrap();
    store.record_export_intent(&f(), id, &export(id)).unwrap();
    store.record_exported(&f(), id).unwrap();
    store.record_review_intent(&f(), id).unwrap();
    let long = format!(
        "https://github.com/{}",
        "x".repeat(cuenca_core::MAX_LANDING_TEXT)
    );
    for (pr, url) in [
        (0, "https://github.com/x/y/pull/0"),
        (1, ""),
        (1, long.as_str()),
    ] {
        assert!(matches!(
            store.record_in_review(&f(), id, pr, url),
            Err(CoreError::Inconsistent(_))
        ));
    }
    store
        .record_in_review(&f(), id, 1, "https://github.com/x/y/pull/1")
        .unwrap();
}
