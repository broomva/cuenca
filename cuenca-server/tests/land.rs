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
        json!({ "epoch": 1, "changeset": id.to_hex(), "base": main.to_hex(), "checks": [] }),
    );
    assert_eq!(r.status(), 200, "{}", r.text().unwrap());
    let r = s.post(
        token,
        &format!("/v1/forks/{fork}/landing"),
        json!({ "epoch": 1, "changeset": id.to_hex() }),
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
    // The commit names the changeset and who asked.
    let message = up.git(&["log", "-1", "--format=%an%n%B", &export.branch]);
    assert!(message.starts_with("a1\n"), "{message}");
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
    // Someone pushed cp/<id> first, at main.
    git(
        &up.work,
        &[
            "push",
            "-q",
            up.remote.to_str().unwrap(),
            &format!("main:refs/heads/cp/{id}"),
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
    assert_eq!(steps(s.authority.store()), [1, 0, 0, 0, 1]);
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
