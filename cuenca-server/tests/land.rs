//! Landing (BRO-3255 acceptance): a fork's draft becomes one branch and one pull request
//! on the location's remote, whose tree is the draft's; a kill at each step is finished
//! on restart with no duplicate; a location without a remote waits for the owner.
//!
//! The remote is a local bare repository standing in for GitHub (`url.insteadOf`), and
//! `gh` a script that keeps pull requests as files ([`Upstream`]).

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use cuenca_core::{
    ChangesetId, Event, ForkName, Holder, LandingStep, LocationName, NodeId, PrincipalName, Store,
    Writer,
};
use cuenca_server::admin::{AdminRequest, GitSource};
use cuenca_server::land::Lander;
use cuenca_server::{Authority, PrincipalKind};
use serde_json::{Value, json};

const NA: &str = "nLandA11CNTRL";

fn f1() -> ForkName {
    ForkName::new("f1").unwrap()
}

fn l1() -> LocationName {
    LocationName::new("l1").unwrap()
}

/// Wait until the landing of `fork` reaches a step `done` accepts; that step.
fn wait_for(store: &Store, fork: &ForkName, done: impl Fn(&LandingStep) -> bool) -> LandingStep {
    let start = Instant::now();
    loop {
        if let Some(l) = store.fork(fork).unwrap().unwrap().landing
            && done(&l.step)
        {
            return l.step;
        }
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "landing of {fork} is at {:?}",
            store.fork(fork).unwrap().unwrap().landing
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn in_review(step: &LandingStep) -> bool {
    matches!(step, LandingStep::InReview { .. })
}

/// An authority with location l1 (from `up`'s work repository, landing to `remote`) and
/// agent a1 holding fork f1 with one draft, verified: the token and the draft's id.
fn requested(s: &Server, up: &Upstream, remote: Option<&str>) -> (String, ChangesetId) {
    let v = s.admin(AdminRequest::LocationAdd {
        name: l1(),
        remote: remote.map(str::to_string),
        git: Some(GitSource {
            repo: up.work.clone(),
            rev: "HEAD".into(),
        }),
        dir: None,
    });
    assert_eq!(v["ok"], true, "{v}");
    let a1 = s.mint("a1", PrincipalKind::Agent, NA, grant(&["l1"], &[]));
    s.whois.node(NA);
    let r = s.post(&a1, "/v1/forks", json!({ "fork": "f1", "location": "l1" }));
    assert_eq!(r.status(), 200, "{}", r.text().unwrap());
    let head = s.fork_head(&a1, "f1");
    let cs = s.write(
        &a1,
        &head,
        &[("README", b"landed\n"), ("new/file.txt", b"new\n")],
    );
    let r: Value = s.publish(&a1, "f1", 1, &cs).json().unwrap();
    let id = ChangesetId::from_hex(r["id"].as_str().unwrap()).unwrap();
    verify_and_ask(s, &a1, "f1", id);
    (a1, id)
}

fn verify_and_ask(s: &Server, token: &str, fork: &str, id: ChangesetId) {
    verify_and_ask_at(s, token, fork, 1, id)
}

fn verify_and_ask_at(s: &Server, token: &str, fork: &str, epoch: u64, id: ChangesetId) {
    let state: Value = s.get(token, &format!("/v1/forks/{fork}")).json().unwrap();
    let location = LocationName::new(state["location"].as_str().unwrap()).unwrap();
    let main = s
        .authority
        .store()
        .location(&location)
        .unwrap()
        .unwrap()
        .main;
    let r = s.post(
        token,
        &format!("/v1/forks/{fork}/verdicts"),
        json!({ "epoch": epoch, "changeset": id.to_hex(), "base": main.to_hex(), "checks": [] }),
    );
    assert_eq!(r.status(), 200, "{}", r.text().unwrap());
    let r = s.post(
        token,
        &format!("/v1/forks/{fork}/landing"),
        json!({ "epoch": epoch, "changeset": id.to_hex() }),
    );
    assert_eq!(r.status(), 200, "{}", r.text().unwrap());
}

/// The landing records in the log, by kind.
fn steps(store: &Store) -> [usize; 5] {
    let mut n = [0; 5];
    for r in store.records(0, usize::MAX).unwrap() {
        match r.event {
            Event::ExportIntent { .. } => n[0] += 1,
            Event::Exported { .. } => n[1] += 1,
            Event::ReviewIntent { .. } => n[2] += 1,
            Event::InReview { .. } => n[3] += 1,
            Event::LandingFailed { .. } => n[4] += 1,
            _ => {}
        }
    }
    n
}

/// One branch `cp/<id>` with one new commit onto main, whose tree is the draft's, and
/// one pull request from it.
fn assert_landed_once(up: &Upstream, store: &Store, id: ChangesetId) {
    let branch = format!("cp/{id}");
    let target = store.changeset(id).unwrap().unwrap().target;
    assert_eq!(
        up.git(&["rev-parse", &format!("refs/heads/{branch}^{{tree}}")]),
        target.to_hex(),
        "the PR's tree is the draft's"
    );
    assert_eq!(
        up.git(&["rev-list", "--count", &format!("main..{branch}")]),
        "1",
        "one commit"
    );
    assert_eq!(
        up.git(&["rev-parse", &format!("{branch}^")]),
        up.git(&["rev-parse", "main"]),
        "onto main"
    );
    assert_eq!(
        up.creates(),
        [format!("{branch} main cuenca-test/land")],
        "one pull request"
    );
    assert_eq!(steps(store), [1, 1, 1, 1, 0], "each step recorded once");
    store.replay_check().unwrap();
}

#[test]
fn a_draft_lands_as_one_pull_request_whose_tree_is_the_drafts() {
    let s = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let up = Upstream::new(tmp.path(), &[("README", "one\n"), ("src/lib.rs", "//!\n")]);
    let (a1, id) = requested(&s, &up, Some(&up.url));
    let lander = Lander::spawn(
        s.authority.clone(),
        up.land_config(tmp.path().join("repos")),
    )
    .unwrap();
    let step = wait_for(s.authority.store(), &f1(), in_review);
    let LandingStep::InReview { pr, url, export } = step else {
        unreachable!()
    };
    assert_eq!(
        (pr, url.as_str(), export.base_ref.as_str()),
        (1, "https://github.com/cuenca-test/land/pull/1", "main")
    );
    assert_landed_once(&up, s.authority.store(), id);
    let body = up.body(&export.branch);
    assert!(body.contains(&id.to_hex()), "{body}");
    assert!(body.contains("declares no checks"), "{body}");
    // The commit names the changeset, and nothing of the request: who asked is in
    // the PR body.
    let message = up.git(&["log", "-1", "--format=%an%n%B", &export.branch]);
    assert!(message.starts_with("Cuenca\n"), "{message}");
    assert!(body.contains("`a1@"), "{body}");
    assert!(
        message.contains(&format!("Cuenca-Changeset: {id}")),
        "{message}"
    );

    // The agent reads where it is; asking again changes nothing.
    let v: Value = s.get(&a1, "/v1/forks/f1/landing").json().unwrap();
    assert_eq!(v["landing"]["step"]["step"], "in_review", "{v}");
    assert_eq!(v["landing"]["step"]["pr"], 1);
    assert_eq!(v["remote"], true);
    assert_eq!(v["error"], Value::Null);
    let len = s.authority.store().len().unwrap();
    let r = s.post(
        &a1,
        "/v1/forks/f1/landing",
        json!({ "epoch": 1, "changeset": id.to_hex() }),
    );
    assert_eq!(r.status(), 200);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(s.authority.store().len().unwrap(), len);
    assert_landed_once(&up, s.authority.store(), id);
    // The owner cannot move main around GitHub.
    let v = s.admin(AdminRequest::Land {
        fork: f1(),
        changeset: None,
    });
    assert_eq!(v["ok"], false, "{v}");
    assert!(v["error"].as_str().unwrap().contains("git remote"), "{v}");
    drop(lander);
}

#[test]
fn a_remote_without_the_bases_tree_fails_the_landing_and_opens_nothing() {
    let s = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let up = Upstream::new(tmp.path(), &[("README", "one\n")]);
    // The remote's main is replaced by an unrelated history, which has no commit
    // whose tree is the location's main.
    let (_a1, _id) = requested(&s, &up, Some(&up.url));
    let other = tmp.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    git(&other, &["init", "-q", "-b", "main"]);
    std::fs::write(other.join("x"), "x").unwrap();
    git(&other, &["add", "-A"]);
    git(&other, &["commit", "-q", "-m", "unrelated"]);
    git(
        &other,
        &["push", "-q", "--force", up.remote.to_str().unwrap(), "main"],
    );
    let lander = Lander::spawn(
        s.authority.clone(),
        up.land_config(tmp.path().join("repos")),
    )
    .unwrap();
    let step = wait_for(s.authority.store(), &f1(), |s| {
        matches!(s, LandingStep::Failed { .. })
    });
    let LandingStep::Failed { reason } = step else {
        unreachable!()
    };
    assert!(reason.contains("not on the remote"), "{reason}");
    assert!(up.creates().is_empty());
    assert_eq!(steps(s.authority.store()), [0, 0, 0, 0, 1]);
    assert!(
        up.git(&["for-each-ref", "refs/heads/cp/"]).is_empty(),
        "no branch"
    );
    drop(lander);
}

#[test]
fn a_branch_that_is_someone_elses_fails_the_landing() {
    let s = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let up = Upstream::new(tmp.path(), &[("README", "one\n")]);
    let (_a1, id) = requested(&s, &up, Some(&up.url));
    // Someone pushed cp/<id> first: a commit onto main (so its parent's tree is
    // main's), but not the draft's tree.
    std::fs::write(up.work.join("README"), "someone else\n").unwrap();
    git(&up.work, &["commit", "-q", "-am", "theirs"]);
    git(
        &up.work,
        &[
            "push",
            "-q",
            up.remote.to_str().unwrap(),
            &format!("HEAD:refs/heads/cp/{id}"),
        ],
    );
    let lander = Lander::spawn(
        s.authority.clone(),
        up.land_config(tmp.path().join("repos")),
    )
    .unwrap();
    let step = wait_for(s.authority.store(), &f1(), |s| {
        matches!(s, LandingStep::Failed { .. })
    });
    let LandingStep::Failed { reason } = step else {
        unreachable!()
    };
    assert!(reason.contains("someone else's branch"), "{reason}");
    assert!(up.creates().is_empty());
    // Judged before any export intent: nothing was pushed, nothing intended.
    assert_eq!(steps(s.authority.store()), [0, 0, 0, 0, 1]);
    drop(lander);
}

/// Spec row (2 · Landing): an agent's `land` on a location without a git remote waits
/// and main does not move, 10 of 10; after the owner's `cuenca admin land` it moves, and
/// the previous tree stays in the log.
#[test]
fn without_a_remote_the_request_waits_for_the_owner_10_of_10() {
    let s = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let up = Upstream::new(tmp.path(), &[("README", "0\n")]);
    let (a1, first) = requested(&s, &up, None);
    let lander = Lander::spawn(
        s.authority.clone(),
        up.land_config(tmp.path().join("repos")),
    )
    .unwrap();
    let store = s.authority.store();
    let mut mains = vec![store.location(&l1()).unwrap().unwrap().main];
    let mut waited = 0;
    for n in 0..10 {
        let (fork, id) = if n == 0 {
            ("f1".to_string(), first)
        } else {
            let fork = format!("f{}", n + 1);
            let r = s.post(&a1, "/v1/forks", json!({ "fork": fork, "location": "l1" }));
            assert_eq!(r.status(), 200, "{}", r.text().unwrap());
            let head = s.fork_head(&a1, &fork);
            let cs = s.write(&a1, &head, &[("README", format!("{n}\n").as_bytes())]);
            let r: Value = s.publish(&a1, &fork, 1, &cs).json().unwrap();
            let id = ChangesetId::from_hex(r["id"].as_str().unwrap()).unwrap();
            verify_and_ask(&s, &a1, &fork, id);
            (fork, id)
        };
        let name = ForkName::new(&fork).unwrap();
        // The lander has had rounds to act on it, and has not.
        s.authority.wake_lander();
        std::thread::sleep(Duration::from_millis(300));
        let v: Value = s
            .get(&a1, &format!("/v1/forks/{fork}/landing"))
            .json()
            .unwrap();
        assert_eq!(v["landing"]["step"]["step"], "requested", "{v}");
        assert_eq!(v["remote"], false);
        assert_eq!(
            store.location(&l1()).unwrap().unwrap().main,
            *mains.last().unwrap()
        );
        waited += 1;
        // A changeset the owner did not read is refused.
        let v = s.admin(AdminRequest::Land {
            fork: name.clone(),
            changeset: Some(ChangesetId::of(&cuenca_tree::Changeset::between(
                &cuenca_tree::Tree::new(),
                &cuenca_tree::Tree::new(),
            ))),
        });
        assert_eq!(v["ok"], false, "{v}");
        let v = s.admin(AdminRequest::Land {
            fork: name.clone(),
            changeset: Some(id),
        });
        assert_eq!(v["ok"], true, "{v}");
        let main = store.location(&l1()).unwrap().unwrap().main;
        assert_eq!(main, store.changeset(id).unwrap().unwrap().target);
        assert_eq!(
            store.fork(&name).unwrap().unwrap().landing.unwrap().step,
            LandingStep::Landed {
                offset: v["offset"].as_u64().unwrap()
            }
        );
        mains.push(main);
    }
    assert_eq!(waited, 10);
    // Every earlier main is still in the store and named by the log.
    let records = store.records(0, usize::MAX).unwrap();
    let moves = records
        .iter()
        .filter(|r| matches!(r.event, Event::MainAdvanced { .. }))
        .count();
    assert_eq!(moves, 10);
    for main in &mains {
        assert_eq!(store.tree(*main).unwrap().git_tree_id(), *main);
    }
    assert_eq!(
        steps(store),
        [0; 5],
        "no export on a location without a remote"
    );
    assert!(up.creates().is_empty());
    store.replay_check().unwrap();
    drop(lander);
}

const NB: &str = "nLandB22CNTRL";

/// A changeset exported once is the same export for every later landing of it: after a
/// takeover drops the first request, and from another fork with the same edit. Each
/// reaches review on the one branch and pull request; none fails.
#[test]
fn a_changeset_landed_again_reuses_its_export_and_pull_request() {
    let s = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let up = Upstream::new(tmp.path(), &[("README", "one\n")]);
    let (a1, id) = requested(&s, &up, Some(&up.url));
    let _lander = Lander::spawn(
        s.authority.clone(),
        up.land_config(tmp.path().join("repos")),
    )
    .unwrap();
    wait_for(s.authority.store(), &f1(), in_review);
    // The owner moves f1 to a2, who asks to land the same head.
    let a2 = s.mint("a2", PrincipalKind::Agent, NB, grant(&["l1"], &[]));
    let v = s.admin(AdminRequest::TakeOver {
        fork: f1(),
        expected_epoch: 1,
        principal: PrincipalName::new("a2").unwrap(),
        node: NodeId::new(NB).unwrap(),
    });
    assert_eq!(v["ok"], true, "{v}");
    s.whois.node(NB);
    // The authority's copy of the export is lost too: the export it builds again
    // must be the one on the remote.
    drop(_lander);
    std::fs::remove_dir_all(tmp.path().join("repos")).unwrap();
    let _lander = Lander::spawn(
        s.authority.clone(),
        up.land_config(tmp.path().join("repos")),
    )
    .unwrap();
    verify_and_ask_at(&s, &a2, "f1", 2, id);
    let step = wait_for(s.authority.store(), &f1(), |st| {
        in_review(st) || matches!(st, LandingStep::Failed { .. })
    });
    assert!(
        matches!(step, LandingStep::InReview { pr: 1, .. }),
        "{step:?}"
    );
    // Another fork with the same edit: the same changeset, the same export.
    s.whois.node(NA);
    let r = s.post(&a1, "/v1/forks", json!({ "fork": "f2", "location": "l1" }));
    assert_eq!(r.status(), 200, "{}", r.text().unwrap());
    let head = s.fork_head(&a1, "f2");
    let cs = s.write(
        &a1,
        &head,
        &[("README", b"landed\n"), ("new/file.txt", b"new\n")],
    );
    let r: Value = s.publish(&a1, "f2", 1, &cs).json().unwrap();
    assert_eq!(r["id"], id.to_hex(), "the same edit is the same changeset");
    verify_and_ask(&s, &a1, "f2", id);
    let f2 = ForkName::new("f2").unwrap();
    let step = wait_for(s.authority.store(), &f2, |st| {
        in_review(st) || matches!(st, LandingStep::Failed { .. })
    });
    assert!(
        matches!(step, LandingStep::InReview { pr: 1, .. }),
        "{step:?}"
    );
    assert_eq!(up.creates().len(), 1, "one pull request");
    assert_eq!(steps(s.authority.store())[4], 0, "nothing failed");
    s.authority.store().replay_check().unwrap();
}

/// A git or gh call that hangs is killed after the timeout and retried, with the
/// error shown to the agent, and the landing goes on once the call answers.
#[test]
fn a_hung_call_is_killed_and_retried() {
    let s = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let up = Upstream::new(tmp.path(), &[("README", "one\n")]);
    let (a1, _id) = requested(&s, &up, Some(&up.url));
    let hang = tmp.path().join("gh-hangs");
    let flag = tmp.path().join("hang");
    std::fs::write(&flag, "").unwrap();
    std::fs::write(
        &hang,
        format!(
            // Hangs while the flag is there, 30 s at most, so a failing test still ends.
            "#!/bin/sh\ni=0\nwhile [ -f '{}' ] && [ $i -lt 300 ]; do sleep 0.1; i=$((i+1)); done\nexec '{}' \"$@\"\n",
            flag.display(),
            up.gh.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&hang, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let mut config = up.land_config(tmp.path().join("repos"));
    config.gh = hang;
    // Long enough for the local git calls on a loaded machine; the hang is gh's.
    config.timeout = Duration::from_secs(5);
    let _lander = Lander::spawn(s.authority.clone(), config).unwrap();
    let start = Instant::now();
    let error = loop {
        let v: Value = s.get(&a1, "/v1/forks/f1/landing").json().unwrap();
        if let Some(e) = v["error"].as_str() {
            break e.to_string();
        }
        assert!(start.elapsed() < Duration::from_secs(60), "{v}");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(
        error.starts_with("gh pr list did not end within 5 s"),
        "{error}"
    );
    assert_eq!(
        s.authority
            .store()
            .fork(&f1())
            .unwrap()
            .unwrap()
            .landing
            .unwrap()
            .step
            .name(),
        "reviewing"
    );
    std::fs::remove_file(&flag).unwrap();
    wait_for(s.authority.store(), &f1(), in_review);
    let v: Value = s.get(&a1, "/v1/forks/f1/landing").json().unwrap();
    assert_eq!(v["error"], Value::Null, "{v}");
    assert_eq!(up.creates().len(), 1);
}

/// A fork with several drafts lands its head onto main: the export carries every
/// draft's files, not only the last one's.
#[test]
fn a_fork_with_several_drafts_lands_its_head() {
    let s = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let up = Upstream::new(tmp.path(), &[("README", "one\n")]);
    let (a1, first) = requested(&s, &up, Some(&up.url));
    let _ = first;
    // Two more drafts on top of the first, then verify and ask for the head.
    let mut id = first;
    for n in 0..2 {
        let head = s.fork_head(&a1, "f1");
        let cs = s.write(
            &a1,
            &head,
            &[(&format!("more/{n}.txt"), format!("{n}\n").as_bytes())],
        );
        let r: Value = s.publish(&a1, "f1", 1, &cs).json().unwrap();
        id = ChangesetId::from_hex(r["id"].as_str().unwrap()).unwrap();
    }
    verify_and_ask(&s, &a1, "f1", id);
    let _lander = Lander::spawn(
        s.authority.clone(),
        up.land_config(tmp.path().join("repos")),
    )
    .unwrap();
    let step = wait_for(s.authority.store(), &f1(), |st| {
        in_review(st) || matches!(st, LandingStep::Failed { .. })
    });
    assert!(in_review(&step), "{step:?}");
    let branch = format!("cp/{id}");
    let head = s.authority.store().fork(&f1()).unwrap().unwrap().head;
    assert_eq!(
        up.git(&["rev-parse", &format!("{branch}^{{tree}}")]),
        head.to_hex()
    );
    assert_eq!(
        up.git(&["rev-parse", &format!("{branch}^")]),
        up.git(&["rev-parse", "main"])
    );
    // The first draft's file is there, from the authority's blobs.
    assert_eq!(up.git(&["show", &format!("{branch}:new/file.txt")]), "new");
    assert_eq!(up.git(&["show", &format!("{branch}:more/1.txt")]), "1");
}

/// A branch with the draft's tree onto a parent that is not main is not an export of
/// this landing either.
#[test]
fn a_branch_with_the_drafts_tree_onto_another_parent_fails_the_landing() {
    let s = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let up = Upstream::new(tmp.path(), &[("README", "one\n")]);
    let (_a1, id) = requested(&s, &up, Some(&up.url));
    // main → "other" (adds z) → the draft's tree exactly (z gone, the draft's files).
    std::fs::write(up.work.join("z"), "z").unwrap();
    git(&up.work, &["add", "-A"]);
    git(&up.work, &["commit", "-q", "-m", "other"]);
    std::fs::remove_file(up.work.join("z")).unwrap();
    std::fs::write(up.work.join("README"), "landed\n").unwrap();
    std::fs::create_dir_all(up.work.join("new")).unwrap();
    std::fs::write(up.work.join("new/file.txt"), "new\n").unwrap();
    git(&up.work, &["add", "-A"]);
    git(&up.work, &["commit", "-q", "-m", "the draft's tree"]);
    let target = s.authority.store().changeset(id).unwrap().unwrap().target;
    assert_eq!(
        git(&up.work, &["rev-parse", "HEAD^{tree}"]),
        target.to_hex()
    );
    git(
        &up.work,
        &[
            "push",
            "-q",
            up.remote.to_str().unwrap(),
            &format!("HEAD:refs/heads/cp/{id}"),
        ],
    );
    let _lander = Lander::spawn(
        s.authority.clone(),
        up.land_config(tmp.path().join("repos")),
    )
    .unwrap();
    let step = wait_for(s.authority.store(), &f1(), |st| {
        matches!(st, LandingStep::Failed { .. })
    });
    let LandingStep::Failed { reason } = step else {
        unreachable!()
    };
    assert!(reason.contains("not an export"), "{reason}");
    assert!(up.creates().is_empty());
}

/// An export already on the remote with the right trees but other metadata (another
/// author, date or message: an earlier authority's, or a person's) is adopted, not
/// fought: it becomes this landing's export, and its pull request is opened from it.
#[test]
fn an_export_with_the_right_trees_and_other_metadata_is_adopted() {
    let s = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let up = Upstream::new(tmp.path(), &[("README", "one\n")]);
    let (_a1, id) = requested(&s, &up, Some(&up.url));
    std::fs::write(up.work.join("README"), "landed\n").unwrap();
    std::fs::create_dir_all(up.work.join("new")).unwrap();
    std::fs::write(up.work.join("new/file.txt"), "new\n").unwrap();
    git(&up.work, &["add", "-A"]);
    git(&up.work, &["commit", "-q", "-m", "by hand"]);
    let theirs = git(&up.work, &["rev-parse", "HEAD"]);
    git(
        &up.work,
        &[
            "push",
            "-q",
            up.remote.to_str().unwrap(),
            &format!("HEAD:refs/heads/cp/{id}"),
        ],
    );
    let _lander = Lander::spawn(
        s.authority.clone(),
        up.land_config(tmp.path().join("repos")),
    )
    .unwrap();
    let step = wait_for(s.authority.store(), &f1(), |st| {
        in_review(st) || matches!(st, LandingStep::Failed { .. })
    });
    let LandingStep::InReview { export, pr: 1, .. } = step else {
        panic!("{step:?}")
    };
    assert_eq!(export.commit.to_hex(), theirs, "adopted, not rebuilt");
    assert_eq!(
        up.git(&["rev-parse", &format!("cp/{id}")]),
        theirs,
        "not moved"
    );
    assert_eq!(up.creates().len(), 1);
}

/// Nor is a merge commit with the draft's tree whose first parent is main.
#[test]
fn a_merge_commit_with_the_drafts_tree_is_not_an_export() {
    let s = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let up = Upstream::new(tmp.path(), &[("README", "one\n")]);
    let (_a1, id) = requested(&s, &up, Some(&up.url));
    // A side branch from main, merged back with the draft's tree as the result.
    git(&up.work, &["checkout", "-q", "-b", "side"]);
    std::fs::write(up.work.join("z"), "z").unwrap();
    git(&up.work, &["add", "-A"]);
    git(&up.work, &["commit", "-q", "-m", "side"]);
    git(&up.work, &["checkout", "-q", "main"]);
    git(&up.work, &["merge", "-q", "--no-ff", "--no-commit", "side"]);
    std::fs::remove_file(up.work.join("z")).unwrap();
    std::fs::write(up.work.join("README"), "landed\n").unwrap();
    std::fs::create_dir_all(up.work.join("new")).unwrap();
    std::fs::write(up.work.join("new/file.txt"), "new\n").unwrap();
    git(&up.work, &["add", "-A"]);
    git(&up.work, &["commit", "-q", "-m", "merge"]);
    let target = s.authority.store().changeset(id).unwrap().unwrap().target;
    assert_eq!(
        git(&up.work, &["rev-parse", "HEAD^{tree}"]),
        target.to_hex()
    );
    assert_eq!(
        git(&up.work, &["rev-parse", "HEAD^1^{tree}"]),
        s.authority
            .store()
            .location(&l1())
            .unwrap()
            .unwrap()
            .main
            .to_hex()
    );
    git(
        &up.work,
        &[
            "push",
            "-q",
            up.remote.to_str().unwrap(),
            &format!("HEAD:refs/heads/cp/{id}"),
        ],
    );
    let _lander = Lander::spawn(
        s.authority.clone(),
        up.land_config(tmp.path().join("repos")),
    )
    .unwrap();
    let step = wait_for(s.authority.store(), &f1(), |st| {
        matches!(st, LandingStep::Failed { .. })
    });
    let LandingStep::Failed { reason } = step else {
        unreachable!()
    };
    assert!(reason.contains("not an export"), "{reason}");
    assert!(up.creates().is_empty());
}

/// A pull request from the export's branch at another commit is not this landing's.
#[test]
fn a_pull_request_from_the_branch_at_another_commit_fails_the_landing() {
    let s = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let up = Upstream::new(tmp.path(), &[("README", "one\n")]);
    let (_a1, id) = requested(&s, &up, Some(&up.url));
    let key = format!("cp_{id}");
    std::fs::create_dir_all(tmp.path().join("prs")).unwrap();
    std::fs::write(tmp.path().join("prs").join(&key), "7\n").unwrap();
    std::fs::write(
        tmp.path().join("prs").join(format!("{key}.oid")),
        "1111111111111111111111111111111111111111\n",
    )
    .unwrap();
    let _lander = Lander::spawn(
        s.authority.clone(),
        up.land_config(tmp.path().join("repos")),
    )
    .unwrap();
    let step = wait_for(s.authority.store(), &f1(), |st| {
        matches!(st, LandingStep::Failed { .. })
    });
    let LandingStep::Failed { reason } = step else {
        unreachable!()
    };
    assert!(reason.contains("pull request 7"), "{reason}");
    assert!(up.creates().is_empty());
}

/// The landing's failure reason, once it is recorded.
fn failed(s: &Server) -> String {
    let step = wait_for(s.authority.store(), &f1(), |st| {
        matches!(st, LandingStep::Failed { .. })
    });
    let LandingStep::Failed { reason } = step else {
        unreachable!()
    };
    reason
}

/// A push GitHub refuses by rule (push protection, a file over its limit, a token
/// without the `workflow` scope) is refused again on every retry: the landing fails
/// once, with GitHub's words, and opens nothing.
#[test]
fn a_push_github_refuses_by_rule_fails_the_landing() {
    let s = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let up = Upstream::new(tmp.path(), &[("README", "one\n")]);
    let (_a1, _id) = requested(&s, &up, Some(&up.url));
    let hook = up.remote.join("hooks").join("pre-receive");
    std::fs::write(
        &hook,
        "#!/bin/sh\necho 'error: GH013: Repository rule violations found for refs/heads/cp.' >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let _lander = Lander::spawn(
        s.authority.clone(),
        up.land_config(tmp.path().join("repos")),
    )
    .unwrap();
    let reason = failed(&s);
    assert!(reason.contains("GH013"), "{reason}");
    assert!(up.creates().is_empty());
    assert!(up.git(&["for-each-ref", "refs/heads/cp/"]).is_empty());
}

/// A remote whose history fails fsck (here a commit with a malformed date) is refused
/// on every fetch: the landing fails rather than retrying forever.
#[test]
fn a_remote_history_that_fails_fsck_fails_the_landing() {
    let s = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let up = Upstream::new(tmp.path(), &[("README", "one\n")]);
    let (_a1, _id) = requested(&s, &up, Some(&up.url));
    let head = up.git(&["rev-parse", "main"]);
    let tree = up.git(&["rev-parse", "main^{tree}"]);
    let raw = format!(
        "tree {tree}\nparent {head}\nauthor A <a@example.invalid> not-a-date +0000\ncommitter A <a@example.invalid> 0 +0000\n\nbad date\n"
    );
    let mut child = Command::new("git")
        .args([
            "hash-object",
            "-t",
            "commit",
            "--literally",
            "-w",
            "--stdin",
        ])
        .current_dir(&up.remote)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(&mut child.stdin.take().unwrap(), raw.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let bad = String::from_utf8(out.stdout).unwrap();
    up.git(&["update-ref", "refs/heads/main", bad.trim()]);
    let _lander = Lander::spawn(
        s.authority.clone(),
        up.land_config(tmp.path().join("repos")),
    )
    .unwrap();
    let reason = failed(&s);
    assert!(reason.contains("fsck error"), "{reason}");
    assert!(up.creates().is_empty());
}

/// No `gh` where the configuration names it: the host can gain one, so the landing
/// is retried, with the error naming the call, rather than failed.
#[test]
fn a_missing_gh_is_retried() {
    let s = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let up = Upstream::new(tmp.path(), &[("README", "one\n")]);
    let (a1, _id) = requested(&s, &up, Some(&up.url));
    let mut config = up.land_config(tmp.path().join("repos"));
    config.gh = tmp.path().join("no-gh-here");
    let _lander = Lander::spawn(s.authority.clone(), config).unwrap();
    let start = Instant::now();
    let error = loop {
        let v: Value = s.get(&a1, "/v1/forks/f1/landing").json().unwrap();
        if let Some(e) = v["error"].as_str() {
            break e.to_string();
        }
        assert!(start.elapsed() < Duration::from_secs(60), "{v}");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(error.starts_with("running gh pr list"), "{error}");
    let step = s
        .authority
        .store()
        .fork(&f1())
        .unwrap()
        .unwrap()
        .landing
        .unwrap()
        .step;
    assert!(!matches!(step, LandingStep::Failed { .. }), "{step:?}");
}

/// `gh pr list --head cp/<id>` also lists pull requests from other people's forks
/// with a branch of that name; one of those, at another commit, is not this landing's.
#[test]
fn a_pull_request_from_another_repository_is_not_the_landings() {
    let s = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let up = Upstream::new(tmp.path(), &[("README", "one\n")]);
    let (_a1, id) = requested(&s, &up, Some(&up.url));
    std::fs::create_dir_all(tmp.path().join("prs")).unwrap();
    std::fs::write(
        tmp.path().join("prs").join(format!("cp_{id}.cross")),
        format!(
            r#"{{"number":99,"url":"https://github.com/someone/land/pull/99","headRefOid":"1111111111111111111111111111111111111111","headRefName":"cp/{id}","isCrossRepository":true}}"#
        ),
    )
    .unwrap();
    let _lander = Lander::spawn(
        s.authority.clone(),
        up.land_config(tmp.path().join("repos")),
    )
    .unwrap();
    let step = wait_for(s.authority.store(), &f1(), |st| {
        in_review(st) || matches!(st, LandingStep::Failed { .. })
    });
    let LandingStep::InReview { pr, .. } = step else {
        panic!("{step:?}")
    };
    assert_ne!(pr, 99);
    assert_eq!(up.creates().len(), 1);
}

/// A takeover drops the old holder's landing request; the error its retries hit is
/// no longer shown on the fork.
#[test]
fn a_takeover_clears_the_dropped_landings_retry_error() {
    let s = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let up = Upstream::new(tmp.path(), &[("README", "one\n")]);
    let (_a1, _id) = requested(&s, &up, Some(&up.url));
    let a2 = s.mint("a2", PrincipalKind::Agent, NB, grant(&["l1"], &[]));
    s.whois.node(NB);
    // The remote is unreachable: every round retries, with the error shown.
    let mut config = up.land_config(tmp.path().join("repos"));
    config.git_config = vec![format!(
        "url.file://{}.insteadOf={}",
        tmp.path().join("gone.git").display(),
        up.url
    )];
    let lander = Lander::spawn(s.authority.clone(), config.clone()).unwrap();
    let error = |s: &Server| -> Value {
        let v: Value = s.get(&a2, "/v1/forks/f1/landing").json().unwrap();
        v["error"].clone()
    };
    let start = Instant::now();
    while error(&s).is_null() {
        assert!(start.elapsed() < Duration::from_secs(60), "no retry error");
        std::thread::sleep(Duration::from_millis(50));
    }
    // The takeover lands between rounds, not under a step that would see it go.
    drop(lander);
    let v = s.admin(AdminRequest::TakeOver {
        fork: f1(),
        expected_epoch: 1,
        principal: PrincipalName::new("a2").unwrap(),
        node: NodeId::new(NB).unwrap(),
    });
    assert_eq!(v["ok"], true, "{v}");
    assert!(!error(&s).is_null(), "the error outlives the lander");
    let _lander = Lander::spawn(s.authority.clone(), config).unwrap();
    let start = Instant::now();
    while !error(&s).is_null() {
        assert!(start.elapsed() < Duration::from_secs(10), "{}", error(&s));
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A location whose remote is not on GitHub could be landed by no one: it is refused
/// when it is added.
#[test]
fn a_location_with_a_remote_off_github_is_refused() {
    let s = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let up = Upstream::new(tmp.path(), &[("README", "one\n")]);
    for remote in ["https://gitlab.com/broomva/cuenca", "/srv/git/x.git"] {
        let v = s.admin(AdminRequest::LocationAdd {
            name: l1(),
            remote: Some(remote.into()),
            git: Some(GitSource {
                repo: up.work.clone(),
                rev: "HEAD".into(),
            }),
            dir: None,
        });
        assert_eq!(v["ok"], false, "{v}");
        assert!(v.to_string().contains("not a GitHub repository"), "{v}");
    }
    assert!(s.authority.store().location(&l1()).unwrap().is_none());
}

// ---- kill the authority at each landing step -----------------------------------------

const DIR_ENV: &str = "CUENCA_LAND_CRASH_DIR";
const FAULT_ENV: &str = "CUENCA_LAND_CRASH_AT";

struct Nobody;

impl cuenca_server::Whois for Nobody {
    fn whois(&self, _ip: std::net::IpAddr) -> cuenca_server::WhoisFuture<'_> {
        Box::pin(async { Ok(None) })
    }
}

/// The child: a lander over the state the parent names, that aborts the process at the
/// step the parent names. Not a test on its own.
#[test]
#[ignore = "run by a_kill_at_each_landing_step_is_finished_on_restart as a child process"]
fn child_lander() {
    let (Ok(dir), Ok(at)) = (std::env::var(DIR_ENV), std::env::var(FAULT_ENV)) else {
        return;
    };
    let dir = PathBuf::from(dir);
    let up = Upstream::open(&dir);
    let authority = Arc::new(Authority::open(&dir.join("state"), Arc::new(Nobody)).unwrap());
    let mut config = up.land_config(dir.join("repos"));
    config.fault = Some(Arc::new(move |step: &str| {
        if step == at {
            std::process::abort();
        }
    }));
    let _lander = Lander::spawn(authority, config).unwrap();
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}

/// The state a child lands: location l1 on the upstream, fork f1 with a verified draft
/// whose landing is requested, written straight to the store.
fn prepare(dir: &Path, up: &Upstream) -> ChangesetId {
    let authority = Authority::open(&dir.join("state"), Arc::new(Nobody)).unwrap();
    let store = authority.store();
    let repo = cuenca_tree::git::Repo::open(&up.work);
    let main = repo.read_tree("HEAD").unwrap();
    for (oid, content) in repo.read_tree_blobs(&main).unwrap() {
        store
            .ingest_blob(oid, content.len() as u64, content.as_slice())
            .unwrap();
    }
    store.add_location(&l1(), Some(&up.url), &main).unwrap();
    let holder = Holder::new(PrincipalName::new("a1").unwrap(), NodeId::new(NA).unwrap());
    let writer = Writer::new(holder.clone(), 1);
    store.create_fork(&f1(), &l1(), &holder).unwrap();
    let content = b"landed after a kill\n";
    let oid = cuenca_tree::GitOid::for_blob(content);
    store
        .ingest_blob(oid, content.len() as u64, &content[..])
        .unwrap();
    let cs = cuenca_tree::Changeset::new(
        &main,
        vec![cuenca_tree::Op::put(
            cuenca_tree::RepoPath::new("README").unwrap(),
            cuenca_tree::Entry::File(oid),
        )],
    )
    .unwrap();
    let id = store.publish_draft(&f1(), &writer, &cs).unwrap().id;
    store
        .record_verify(&f1(), &writer, id, main.git_tree_id(), Vec::new())
        .unwrap();
    store.record_landing_intent(&f1(), &writer, id).unwrap();
    id
}

/// Spec row (3 · kill the authority at each landing transition): killed after the
/// export intent, after the push, after the review intent and after the pull request
/// opened (each before its confirmation is recorded), the authority finishes the
/// landing on restart, with one commit, one branch and one pull request.
#[test]
fn a_kill_at_each_landing_step_is_finished_on_restart() {
    for at in ["export_intent", "pushed", "review_intent", "pr_opened"] {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let up = Upstream::new(dir, &[("README", "one\n")]);
        let id = prepare(dir, &up);
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "child_lander", "--nocapture"])
            .env(DIR_ENV, dir)
            .env(FAULT_ENV, at)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let start = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if start.elapsed() > Duration::from_secs(60) {
                child.kill().unwrap();
                panic!("{at}: the child never reached its fault");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(!status.success(), "{at}: the child aborted");
        // What the killed authority left: the step's own record, not its confirmation.
        {
            let store = Store::open(dir.join("state/store")).unwrap();
            let left = store.fork(&f1()).unwrap().unwrap().landing.unwrap().step;
            let expected = match at {
                "export_intent" | "pushed" => "exporting",
                "review_intent" | "pr_opened" => "reviewing",
                _ => unreachable!(),
            };
            assert_eq!(left.name(), expected, "{at}");
            let pushed = !up.git(&["for-each-ref", "refs/heads/cp/"]).is_empty();
            assert_eq!(
                pushed,
                at != "export_intent",
                "{at}: pushed before the kill"
            );
            assert_eq!(up.creates().len(), usize::from(at == "pr_opened"), "{at}");
        }
        // Killed before the push, the commit may never reach the disk: drop the
        // authority's repository, so the restart builds the export again and must get
        // the commit its intent names.
        if at == "export_intent" {
            std::fs::remove_dir_all(dir.join("repos")).unwrap();
        }
        // Restart: the lander picks up at the step the log has.
        let authority = Arc::new(Authority::open(&dir.join("state"), Arc::new(Nobody)).unwrap());
        let lander = Lander::spawn(authority.clone(), up.land_config(dir.join("repos"))).unwrap();
        wait_for(authority.store(), &f1(), in_review);
        drop(lander);
        assert_landed_once(&up, authority.store(), id);
        eprintln!("killed at {at}: finished on restart, one commit, one branch, one PR");
    }
}

/// Two landings of one changeset, one killed mid-export: fork f1's lander dies after
/// its export intent, before the push; then fork e2 (the same edit, and first in the
/// lander's order) is asked to land it, and its export is pushed first. f1's own export
/// must be that same commit, so both reach review on the one pull request.
#[test]
fn a_landing_killed_mid_export_finishes_on_the_branch_another_landing_pushed() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let up = Upstream::new(dir, &[("README", "one\n")]);
    let id = prepare(dir, &up);
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "child_lander", "--nocapture"])
        .env(DIR_ENV, dir)
        .env(FAULT_ENV, "export_intent")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let start = Instant::now();
    while child.try_wait().unwrap().is_none() {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "never reached its fault"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let authority = Arc::new(Authority::open(&dir.join("state"), Arc::new(Nobody)).unwrap());
    let store = authority.store();
    assert_eq!(
        store
            .fork(&f1())
            .unwrap()
            .unwrap()
            .landing
            .unwrap()
            .step
            .name(),
        "exporting"
    );
    // e2: a fork of the same main with the same draft, verified and asked to land.
    let e2 = ForkName::new("e2").unwrap();
    let holder = Holder::new(PrincipalName::new("a2").unwrap(), NodeId::new(NA).unwrap());
    let writer = Writer::new(holder.clone(), 1);
    store.create_fork(&e2, &l1(), &holder).unwrap();
    let cs = store.changeset(id).unwrap().unwrap();
    assert_eq!(store.publish_draft(&e2, &writer, &cs).unwrap().id, id);
    let main = store.location(&l1()).unwrap().unwrap().main;
    store
        .record_verify(&e2, &writer, id, main, Vec::new())
        .unwrap();
    store.record_landing_intent(&e2, &writer, id).unwrap();
    let lander = Lander::spawn(authority.clone(), up.land_config(dir.join("repos"))).unwrap();
    for fork in [&e2, &f1()] {
        let step = wait_for(store, fork, |st| {
            in_review(st) || matches!(st, LandingStep::Failed { .. })
        });
        assert!(
            matches!(step, LandingStep::InReview { pr: 1, .. }),
            "{fork}: {step:?}"
        );
    }
    drop(lander);
    assert_eq!(up.creates().len(), 1);
    assert_eq!(
        up.git(&["rev-list", "--count", &format!("main..cp/{id}")]),
        "1"
    );
}

impl Upstream {
    /// The upstream a parent made in `dir` ([`Upstream::new`]), for its child.
    fn open(dir: &Path) -> Self {
        let remote = dir.join("remote.git");
        Self {
            dir: dir.to_path_buf(),
            url: UPSTREAM_URL.to_string(),
            work: dir.join("work"),
            gh: dir.join("gh"),
            git_config: vec![format!(
                "url.file://{}.insteadOf={UPSTREAM_URL}",
                remote.display()
            )],
            remote,
        }
    }
}

/// Push a commit adding `path` with `content` to the upstream's `main`, as a merge on
/// GitHub would: the new commit, and its tree.
fn push_to_main(up: &Upstream, path: &str, content: &str) -> (String, String) {
    let p = up.work.join(path);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, content).unwrap();
    common::git(&up.work, &["add", "-A"]);
    common::git(&up.work, &["commit", "-q", "-m", &format!("add {path}")]);
    common::git(
        &up.work,
        &["push", "-q", up.remote.to_str().unwrap(), "main"],
    );
    (
        up.git(&["rev-parse", "main"]).trim().to_string(),
        up.git(&["rev-parse", "main^{tree}"]).trim().to_string(),
    )
}

/// Append `n` commits to the work repository's `main`, each changing `counter.txt`, in
/// one `git fast-import` (1,100 `git commit` calls take minutes).
fn fast_import_commits(work: &Path, n: usize) {
    let mut stream = String::new();
    for i in 0..n {
        let data = format!("{i}\n");
        stream.push_str(&format!(
            "commit refs/heads/main\ncommitter T <t@example.com> {} +0000\ndata 2\nc\n",
            1_700_000_000 + i
        ));
        if i == 0 {
            stream.push_str("from refs/heads/main^0\n");
        }
        stream.push_str(&format!(
            "M 100644 inline counter.txt\ndata {}\n{data}\n",
            data.len()
        ));
    }
    let mut child = Command::new("git")
        .args(["fast-import", "--quiet"])
        .current_dir(work)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write as _;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stream.as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success());
    common::git(work, &["reset", "-q", "--hard", "main"]);
}

/// Wait until location l1's main is `tree`.
fn wait_for_main(store: &Store, tree: &str) {
    let start = Instant::now();
    while store.location(&l1()).unwrap().unwrap().main.to_hex() != tree {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "main of l1 is still {}",
            store.location(&l1()).unwrap().unwrap().main
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn follows(store: &Store) -> usize {
    store
        .records(0, usize::MAX)
        .unwrap()
        .into_iter()
        .filter(|r| matches!(r.event, Event::MainFollowed { .. }))
        .count()
}

/// BRO-3517: what merges on the remote becomes the location's main, so the next fork
/// starts from it; forks that exist keep their base, and a remote that has not moved
/// records nothing.
#[test]
fn main_follows_the_remote_and_the_next_fork_starts_from_it() {
    let s = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let up = Upstream::new(tmp.path(), &[("README", "one\n")]);
    let v = s.admin(AdminRequest::LocationAdd {
        name: l1(),
        remote: Some(up.url.clone()),
        git: Some(GitSource {
            repo: up.work.clone(),
            rev: "HEAD".into(),
        }),
        dir: None,
    });
    assert_eq!(v["ok"], true, "{v}");
    let a1 = s.mint("a1", PrincipalKind::Agent, NA, grant(&["l1"], &[]));
    s.whois.node(NA);
    let r = s.post(&a1, "/v1/forks", json!({ "fork": "f1", "location": "l1" }));
    assert_eq!(r.status(), 200, "{}", r.text().unwrap());
    let store = s.authority.store();
    let registered = store.location(&l1()).unwrap().unwrap().main;

    let mut config = up.land_config(tmp.path().join("repos"));
    config.follow = Duration::ZERO;
    let lander = Lander::spawn(s.authority.clone(), config).unwrap();
    // The remote has not moved: following it records nothing, round after round.
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(follows(store), 0);

    let (commit, tree) = push_to_main(&up, "merged/elsewhere.txt", "merged\n");
    wait_for_main(store, &tree);
    let loc = store.location(&l1()).unwrap().unwrap();
    assert_eq!(loc.followed.unwrap().to_hex(), commit);
    assert_eq!(follows(store), 1);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(follows(store), 1, "an unmoved remote is not followed again");

    // The fork that existed keeps its base; the next fork starts from the new main and
    // holds what merged.
    let f1 = store.fork(&f1()).unwrap().unwrap();
    assert_eq!(
        (f1.base, store.root_main(&f1.name).unwrap()),
        (registered, registered)
    );
    let r = s.post(&a1, "/v1/forks", json!({ "fork": "f2", "location": "l1" }));
    assert_eq!(r.status(), 200, "{}", r.text().unwrap());
    let f2 = store.fork(&ForkName::new("f2").unwrap()).unwrap().unwrap();
    assert_eq!(f2.base.to_hex(), tree);
    assert_eq!(store.root_main(&f2.name).unwrap().to_hex(), tree);
    let head = s.fork_head(&a1, "f2");
    assert!(
        head.get("merged/elsewhere.txt").is_some(),
        "the new fork holds the merged file"
    );
    drop(lander);
}

/// BRO-3517: a fork made before main followed the remote lands onto the main its line
/// started from, so its pull request changes only what the fork changed and never
/// reverts what merged since. Committed onto the new main instead, the fork's tree
/// (which lacks the merged file) would delete it.
#[test]
fn a_fork_older_than_main_lands_onto_its_root_and_reverts_nothing() {
    let s = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let up = Upstream::new(tmp.path(), &[("README", "one\n"), ("src/lib.rs", "//!\n")]);
    let v = s.admin(AdminRequest::LocationAdd {
        name: l1(),
        remote: Some(up.url.clone()),
        git: Some(GitSource {
            repo: up.work.clone(),
            rev: "HEAD".into(),
        }),
        dir: None,
    });
    assert_eq!(v["ok"], true, "{v}");
    let root_commit = up.git(&["rev-parse", "main"]).trim().to_string();
    let a1 = s.mint("a1", PrincipalKind::Agent, NA, grant(&["l1"], &[]));
    s.whois.node(NA);
    let r = s.post(&a1, "/v1/forks", json!({ "fork": "f1", "location": "l1" }));
    assert_eq!(r.status(), 200, "{}", r.text().unwrap());
    let head = s.fork_head(&a1, "f1");
    let cs = s.write(&a1, &head, &[("README", b"changed by f1\n")]);
    let r: Value = s.publish(&a1, "f1", 1, &cs).json().unwrap();
    let id = ChangesetId::from_hex(r["id"].as_str().unwrap()).unwrap();

    // Someone else's change merges on the remote, and main follows it.
    let (_, tree) = push_to_main(&up, "merged/elsewhere.txt", "merged\n");
    let mut config = up.land_config(tmp.path().join("repos"));
    config.follow = Duration::ZERO;
    let lander = Lander::spawn(s.authority.clone(), config).unwrap();
    wait_for_main(s.authority.store(), &tree);

    // The fork verifies against main as it is now, and asks to land.
    verify_and_ask(&s, &a1, "f1", id);
    let step = wait_for(s.authority.store(), &f1(), in_review);
    let LandingStep::InReview { export, .. } = step else {
        unreachable!()
    };
    assert_eq!(
        up.git(&["rev-parse", &format!("{}^", export.branch)])
            .trim(),
        root_commit,
        "the export's parent is the commit the fork's line started from"
    );
    let changed = up.git(&[
        "diff",
        "--name-only",
        &format!("{}^", export.branch),
        &export.branch,
    ]);
    assert_eq!(
        changed.trim(),
        "README",
        "the pull request changes only README"
    );
    // GitHub's merge brings in the rest: the merged file survives.
    let merged = up.git(&["merge-tree", "--write-tree", "main", &export.branch]);
    let merged_tree = merged.lines().next().unwrap();
    let files = up.git(&["ls-tree", "-r", "--name-only", merged_tree]);
    assert!(
        files.lines().any(|f| f == "merged/elsewhere.txt"),
        "{files}"
    );
    assert!(files.lines().any(|f| f == "README"), "{files}");
    drop(lander);
}

/// BRO-3517: the root of a line is derived from the forks the log keeps, so a fork of a
/// fork made before main followed the remote lands onto the main its line started from,
/// even when that main is more than 1000 first-parent commits behind the remote's tip.
#[test]
fn a_fork_of_an_old_fork_lands_onto_the_line_root_past_a_long_history() {
    let s = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let up = Upstream::new(tmp.path(), &[("README", "one\n")]);
    let v = s.admin(AdminRequest::LocationAdd {
        name: l1(),
        remote: Some(up.url.clone()),
        git: Some(GitSource {
            repo: up.work.clone(),
            rev: "HEAD".into(),
        }),
        dir: None,
    });
    assert_eq!(v["ok"], true, "{v}");
    let root_commit = up.git(&["rev-parse", "main"]).trim().to_string();
    let a1 = s.mint("a1", PrincipalKind::Agent, NA, grant(&["l1"], &[]));
    s.whois.node(NA);
    let r = s.post(&a1, "/v1/forks", json!({ "fork": "f1", "location": "l1" }));
    assert_eq!(r.status(), 200, "{}", r.text().unwrap());
    let head = s.fork_head(&a1, "f1");
    let cs = s.write(&a1, &head, &[("one.txt", b"from f1\n")]);
    assert_eq!(s.publish(&a1, "f1", 1, &cs).status(), 200);
    // g is a fork of f1: its base is f1's head, which the remote has never seen.
    let r = s.post(&a1, "/v1/forks", json!({ "fork": "g", "from": "f1" }));
    assert_eq!(r.status(), 200, "{}", r.text().unwrap());
    let head = s.fork_head(&a1, "g");
    let cs = s.write(&a1, &head, &[("two.txt", b"from g\n")]);
    let r: Value = s.publish(&a1, "g", 1, &cs).json().unwrap();
    let id = ChangesetId::from_hex(r["id"].as_str().unwrap()).unwrap();

    // 1,100 commits merge on the remote after the line began, each with its own tree,
    // so the line's root is past the old 1000-commit search window.
    fast_import_commits(&up.work, 1100);
    let (_, tree) = push_to_main(&up, "merged/elsewhere.txt", "merged\n");
    let mut config = up.land_config(tmp.path().join("repos"));
    config.follow = Duration::ZERO;
    let lander = Lander::spawn(s.authority.clone(), config).unwrap();
    wait_for_main(s.authority.store(), &tree);

    verify_and_ask(&s, &a1, "g", id);
    let g = ForkName::new("g").unwrap();
    let step = wait_for(s.authority.store(), &g, in_review);
    let LandingStep::InReview { export, .. } = step else {
        unreachable!()
    };
    assert_eq!(
        up.git(&["rev-parse", &format!("{}^", export.branch)])
            .trim(),
        root_commit,
        "the export's parent is the commit the line started from, 1,101 back"
    );
    let depth = up.git(&["rev-list", "--count", &format!("{root_commit}..main")]);
    assert_eq!(depth.trim(), "1101");
    let changed = up.git(&[
        "diff",
        "--name-only",
        &format!("{}^", export.branch),
        &export.branch,
    ]);
    assert_eq!(
        changed.lines().collect::<Vec<_>>(),
        ["one.txt", "two.txt"],
        "the pull request holds the line's changes and nothing else"
    );
    // The projections still match a replay of the log.
    s.authority.store().replay_check().unwrap();
    drop(lander);
}

/// BRO-3517: following runs on its own thread, so a remote whose fetch hangs never holds
/// up a landing asked for while the follow is stuck.
#[test]
fn a_stalled_follow_never_delays_a_new_landing() {
    let s = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let up = Upstream::new(tmp.path(), &[("README", "one\n")]);
    // git, except that fetching to follow main hangs for 12 s.
    let wrapper = tmp.path().join("git-stalls-following");
    std::fs::write(
        &wrapper,
        "#!/bin/sh\ncase \"$*\" in *refs/cuenca/followed*) sleep 12 ;; esac\nexec git \"$@\"\n",
    )
    .unwrap();
    std::fs::set_permissions(
        &wrapper,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    let mut config = up.land_config(tmp.path().join("repos"));
    config.git = wrapper;
    config.follow = Duration::ZERO;
    // The location exists before the lander starts, so the follower is stuck in its
    // fetch by the time the landing is asked for.
    let v = s.admin(AdminRequest::LocationAdd {
        name: l1(),
        remote: Some(up.url.clone()),
        git: Some(GitSource {
            repo: up.work.clone(),
            rev: "HEAD".into(),
        }),
        dir: None,
    });
    assert_eq!(v["ok"], true, "{v}");
    let lander = Lander::spawn(s.authority.clone(), config).unwrap();
    std::thread::sleep(Duration::from_secs(1));
    let a1 = s.mint("a1", PrincipalKind::Agent, NA, grant(&["l1"], &[]));
    s.whois.node(NA);
    let r = s.post(&a1, "/v1/forks", json!({ "fork": "f1", "location": "l1" }));
    assert_eq!(r.status(), 200, "{}", r.text().unwrap());
    let head = s.fork_head(&a1, "f1");
    let cs = s.write(&a1, &head, &[("README", b"landed\n")]);
    let r: Value = s.publish(&a1, "f1", 1, &cs).json().unwrap();
    let id = ChangesetId::from_hex(r["id"].as_str().unwrap()).unwrap();
    let asked = Instant::now();
    verify_and_ask(&s, &a1, "f1", id);
    wait_for(s.authority.store(), &f1(), in_review);
    assert!(
        asked.elapsed() < Duration::from_secs(8),
        "the landing waited {:?} on a stalled follow",
        asked.elapsed()
    );
    drop(lander);
}
