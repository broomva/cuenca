//! Main follows the remote (ADR 0002, BRO-3601): what merges on a location's remote
//! default branch becomes its main within a few lander rounds, with the git work of
//! each follow under one deadline, ref locks left in place, missing blobs read in
//! batches that are kept, and a follow that cannot fit failing visibly with main left
//! where it was.
//!
//! The remote is a local bare repository standing in for GitHub (`url.insteadOf`), as in
//! `land.rs`, and a git wrapper stands in for a slow or hung remote.

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::*;
use cuenca_core::{ChangesetId, Event, LocationName, Store};
use cuenca_server::PrincipalKind;
use cuenca_server::admin::{AdminRequest, GitSource};
use cuenca_server::land::{LandConfig, Lander};
use cuenca_tree::GitOid;
use serde_json::{Value, json};

const NA: &str = "nFollowA1CNTR";

fn loc(name: &str) -> LocationName {
    LocationName::new(name).unwrap()
}

/// Register `name` from `up`'s work repository, landing to `up`'s remote.
fn register(s: &Server, up: &Upstream, name: &str) {
    let v = s.admin(AdminRequest::LocationAdd {
        name: loc(name),
        remote: Some(up.url.clone()),
        git: Some(GitSource {
            repo: up.work.clone(),
            rev: "HEAD".into(),
        }),
        dir: None,
    });
    assert_eq!(v["ok"], true, "{v}");
}

/// Commit `files` in `up`'s work repository and push it to the remote's `main`, as a
/// pull request merging on GitHub would: the new tree.
fn merge_on_remote(up: &Upstream, files: &[(&str, &[u8])]) -> GitOid {
    for (path, content) in files {
        let p = up.work.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }
    git(&up.work, &["add", "-A"]);
    git(&up.work, &["commit", "-q", "-m", "merged on the remote"]);
    git(
        &up.work,
        &["push", "-q", up.remote.to_str().unwrap(), "HEAD:main"],
    );
    remote_tree(up)
}

fn remote_tree(up: &Upstream) -> GitOid {
    GitOid::from_hex(&up.git(&["rev-parse", "main^{tree}"])).unwrap()
}

fn main_of(store: &Store, name: &str) -> GitOid {
    store.location(&loc(name)).unwrap().unwrap().main
}

/// Wait up to `limit` for `done`; whether it happened.
fn within(limit: Duration, done: impl Fn() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < limit {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    done()
}

/// The `follow_error` `GET /v1/locations` shows for `name`.
fn follow_error(s: &Server, token: &str, name: &str) -> Option<String> {
    let v: Value = s.get(token, "/v1/locations").json().unwrap();
    v.as_array()
        .unwrap()
        .iter()
        .find(|l| l["name"] == name)
        .and_then(|l| l["follow_error"].as_str().map(str::to_string))
}

/// An agent who sees `locations`.
fn agent(s: &Server, locations: &[&str]) -> String {
    let token = s.mint("a1", PrincipalKind::Agent, NA, grant(locations, &[]));
    s.whois.node(NA);
    token
}

/// A git wrapper at `dir/git-<name>` that runs `action` for a call whose arguments
/// match the shell pattern `pattern` while `dir/flag` exists, and the real git
/// otherwise.
fn wrapper(dir: &Path, name: &str, pattern: &str, action: &str) -> PathBuf {
    let path = dir.join(format!("git-{name}"));
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nif [ -f '{flag}' ]; then case \" $* \" in {pattern}) {action} ;; esac; fi\nexec git \"$@\"\n",
            flag = dir.join("flag").display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    std::fs::write(dir.join("flag"), "").unwrap();
    path
}

fn lander(s: &Server, config: LandConfig) -> Lander {
    Lander::spawn(s.authority.clone(), config).unwrap()
}

fn store(s: &Server) -> &Store {
    s.authority.store()
}

/// A commit merged on the remote becomes main; the next fork starts from it; a fork
/// made before it lands onto the main its line started from, so its pull request holds
/// only its own change and reverts nothing that merged since.
#[test]
fn a_merge_on_the_remote_becomes_main_and_reverts_nothing() {
    let s = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let up = Upstream::new(tmp.path(), &[("README", "one\n")]);
    register(&s, &up, "l1");
    let a1 = agent(&s, &["l1"]);
    let r = s.post(&a1, "/v1/forks", json!({ "fork": "old", "location": "l1" }));
    assert_eq!(r.status(), 200, "{}", r.text().unwrap());
    let root = main_of(store(&s), "l1");
    let first_commit = up.git(&["rev-parse", "main"]);

    let _lander = lander(&s, up.land_config(tmp.path().join("repos")));
    let merged = merge_on_remote(&up, &[("merged.txt", b"from someone else\n")]);
    assert!(
        within(Duration::from_secs(30), || main_of(store(&s), "l1")
            == merged),
        "main did not follow"
    );
    let followed = store(&s)
        .records(0, usize::MAX)
        .unwrap()
        .into_iter()
        .filter(|r| matches!(r.event, Event::MainFollowed { .. }))
        .count();
    assert_eq!(followed, 1);
    let loc1 = store(&s).location(&loc("l1")).unwrap().unwrap();
    assert_eq!(
        loc1.followed.map(|f| f.commit.to_hex()),
        Some(up.git(&["rev-parse", "main"]))
    );

    // The next fork starts from the followed main.
    let r = s.post(&a1, "/v1/forks", json!({ "fork": "new", "location": "l1" }));
    assert_eq!(r.status(), 200, "{}", r.text().unwrap());
    assert_eq!(
        store(&s)
            .fork(&cuenca_core::ForkName::new("new").unwrap())
            .unwrap()
            .unwrap()
            .base,
        merged
    );

    // The old fork lands onto its root: one commit onto the first commit, holding only
    // its own file.
    let head = s.fork_head(&a1, "old");
    assert_eq!(head.git_tree_id(), root);
    let cs = s.write(&a1, &head, &[("mine.txt", b"mine\n")]);
    let r: Value = s.publish(&a1, "old", 1, &cs).json().unwrap();
    let id = ChangesetId::from_hex(r["id"].as_str().unwrap()).unwrap();
    let r = s.post(
        &a1,
        "/v1/forks/old/verdicts",
        json!({ "epoch": 1, "changeset": id.to_hex(), "base": merged.to_hex(), "checks": [] }),
    );
    assert_eq!(r.status(), 200, "{}", r.text().unwrap());
    let r = s.post(
        &a1,
        "/v1/forks/old/landing",
        json!({ "epoch": 1, "changeset": id.to_hex() }),
    );
    assert_eq!(r.status(), 200, "{}", r.text().unwrap());
    let branch = format!("cp/{id}");
    assert!(
        within(Duration::from_secs(30), || !up.creates().is_empty()),
        "no pull request"
    );
    assert_eq!(up.git(&["rev-parse", &format!("{branch}^")]), first_commit);
    assert_eq!(
        up.git(&["diff", "--name-only", &format!("{branch}^"), &branch]),
        "mine.txt",
        "the pull request holds the fork's change and reverts nothing"
    );
    store(&s).replay_check().unwrap();
}

/// A fork of a fork whose line started 1,101 first-parent commits behind the remote's
/// tip, past the near window, still lands onto its root.
#[test]
fn a_fork_of_a_fork_rooted_1101_commits_back_lands_onto_its_root() {
    let s = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let up = Upstream::new(tmp.path(), &[("README", "one\n")]);
    register(&s, &up, "l1");
    let a1 = agent(&s, &["l1"]);
    let r = s.post(&a1, "/v1/forks", json!({ "fork": "f", "location": "l1" }));
    assert_eq!(r.status(), 200, "{}", r.text().unwrap());
    let root_commit = up.git(&["rev-parse", "main"]);

    // 1,101 commits, each changing the tree, through fast-import.
    let mut stream = String::new();
    for i in 0..1101 {
        let data = format!("{i}\n");
        stream.push_str(&format!(
            "commit refs/heads/main\ncommitter t <t@t> {} +0000\ndata 1\nx\n{}M 100644 inline counter\ndata {}\n{data}\n",
            1_700_000_000 + i,
            if i == 0 { format!("from {root_commit}\n") } else { String::new() },
            data.len()
        ));
    }
    let mut child = std::process::Command::new("git")
        .args([
            "--git-dir",
            up.remote.to_str().unwrap(),
            "fast-import",
            "--quiet",
        ])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(child.stdin.as_mut().unwrap(), stream.as_bytes()).unwrap();
    drop(child.stdin.take());
    assert!(child.wait().unwrap().success());

    let r = s.post(&a1, "/v1/forks", json!({ "fork": "g", "from": "f" }));
    assert_eq!(r.status(), 200, "{}", r.text().unwrap());
    let _lander = lander(&s, up.land_config(tmp.path().join("repos")));
    let tip = remote_tree(&up);
    assert!(within(Duration::from_secs(60), || main_of(store(&s), "l1") == tip));

    let head = s.fork_head(&a1, "g");
    let cs = s.write(&a1, &head, &[("deep.txt", b"deep\n")]);
    let r: Value = s.publish(&a1, "g", 1, &cs).json().unwrap();
    let id = ChangesetId::from_hex(r["id"].as_str().unwrap()).unwrap();
    let r = s.post(
        &a1,
        "/v1/forks/g/verdicts",
        json!({ "epoch": 1, "changeset": id.to_hex(), "base": tip.to_hex(), "checks": [] }),
    );
    assert_eq!(r.status(), 200, "{}", r.text().unwrap());
    let r = s.post(
        &a1,
        "/v1/forks/g/landing",
        json!({ "epoch": 1, "changeset": id.to_hex() }),
    );
    assert_eq!(r.status(), 200, "{}", r.text().unwrap());
    assert!(
        within(Duration::from_secs(60), || !up.creates().is_empty()),
        "{:?}",
        s.get(&a1, "/v1/forks/g/landing").text()
    );
    assert_eq!(
        up.git(&["rev-parse", &format!("cp/{id}^")]),
        root_commit,
        "onto the root, 1,101 commits back"
    );
}

/// One location whose remote hangs does not stop the others from following: following
/// rotates least recently followed first, and the hung one shows its error.
#[test]
fn a_hung_remote_delays_the_others_by_a_round_not_forever() {
    let s = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let up = Upstream::new(tmp.path(), &[("README", "one\n")]);
    register(&s, &up, "hung");
    register(&s, &up, "fine");
    let a1 = agent(&s, &["hung", "fine"]);
    let mut config = up.land_config(tmp.path().join("repos"));
    config.git = wrapper(tmp.path(), "hangs", "*hung.git*", "exec sleep 30");
    config.follow_timeout = Duration::from_secs(2);
    let _lander = lander(&s, config);
    let merged = merge_on_remote(&up, &[("merged.txt", b"m\n")]);
    assert!(
        within(Duration::from_secs(30), || main_of(store(&s), "fine")
            == merged),
        "the healthy location did not follow"
    );
    assert!(within(Duration::from_secs(30), || {
        follow_error(&s, &a1, "hung").is_some_and(|e| e.contains("follow deadline (2 s)"))
    }));
    assert_ne!(main_of(store(&s), "hung"), merged);
    assert_eq!(follow_error(&s, &a1, "fine"), None);
}

/// Each git step of a follow sleeping most of the deadline does not stretch the follow
/// past it: the second step is killed when the one deadline passes.
#[test]
fn slow_steps_end_within_the_one_follow_deadline() {
    let s = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let up = Upstream::new(tmp.path(), &[("README", "one\n")]);
    register(&s, &up, "l1");
    let a1 = agent(&s, &["l1"]);
    let mut config = up.land_config(tmp.path().join("repos"));
    // Each call in the location's repository sleeps 2.5 s of a 3 s deadline.
    config.git = wrapper(tmp.path(), "slow", "*l1.git*", "sleep 2.5");
    config.follow_timeout = Duration::from_secs(3);
    let _lander = lander(&s, config);
    let start = Instant::now();
    let error = loop {
        if let Some(e) = follow_error(&s, &a1, "l1") {
            break e;
        }
        assert!(start.elapsed() < Duration::from_secs(60), "no follow error");
        std::thread::sleep(Duration::from_millis(50));
    };
    // "following stopped while <step> after <secs> s ..."
    let secs: f64 = error
        .split(" after ")
        .nth(1)
        .and_then(|r| r.split(" s").next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("{error}"));
    assert!(secs < 3.0 + 0.75, "{error}");
    assert!(error.contains("follow deadline"), "{error}");
}

/// A ref lock in the way fails the follow naming it and is never removed; following
/// retries on later rounds and resumes once the lock is gone. Covers a lock left by a
/// killed fetch.
#[test]
fn a_ref_lock_is_left_in_place_and_following_resumes_once_it_is_removed() {
    let s = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let up = Upstream::new(tmp.path(), &[("README", "one\n")]);
    register(&s, &up, "l1");
    let a1 = agent(&s, &["l1"]);
    let repos = tmp.path().join("repos");
    let lock = repos.join("l1.git/refs/cuenca/followed.lock");
    let mut config = up.land_config(repos.clone());
    // A fetch into the followed ref that takes the lock and hangs, as a fetch killed
    // mid-update leaves it.
    config.git = wrapper(
        tmp.path(),
        "locks",
        "*refs/cuenca/followed*",
        &format!(
            "mkdir -p '{dir}'; : > '{lock}'; exec sleep 30",
            dir = lock.parent().unwrap().display(),
            lock = lock.display()
        ),
    );
    config.follow_timeout = Duration::from_secs(2);
    let _lander = lander(&s, config);
    let main = main_of(store(&s), "l1");
    let merged = merge_on_remote(&up, &[("merged.txt", b"m\n")]);
    assert!(within(Duration::from_secs(30), || {
        follow_error(&s, &a1, "l1").is_some_and(|e| e.contains("followed.lock"))
    }));
    // The wrapper no longer hangs; only the lock is in the way now.
    std::fs::remove_file(tmp.path().join("flag")).unwrap();
    std::thread::sleep(Duration::from_secs(1));
    assert!(lock.exists(), "the lock was removed");
    assert_eq!(main_of(store(&s), "l1"), main);
    let e = follow_error(&s, &a1, "l1").unwrap();
    assert!(e.contains("never removed automatically"), "{e}");
    // The status names the lock in the repository, not the host's paths.
    assert!(!e.contains(&repos.display().to_string()), "{e}");

    std::fs::remove_file(&lock).unwrap();
    assert!(within(Duration::from_secs(30), || main_of(store(&s), "l1")
        == merged));
    assert!(within(Duration::from_secs(5), || follow_error(
        &s, &a1, "l1"
    )
    .is_none()));
}

/// The commit and tree a follow records are one pair: the tree is resolved from the
/// commit the fetch gave, not from the ref again, which another git process may have
/// moved in between. Here the ref is moved back to the old commit right after the
/// commit is resolved, on every round.
#[test]
fn a_follow_records_the_tree_of_the_commit_it_resolved() {
    let s = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let up = Upstream::new(tmp.path(), &[("README", "one\n")]);
    register(&s, &up, "l1");
    let first = up.git(&["rev-parse", "main"]);
    let merged = merge_on_remote(&up, &[("merged.txt", b"m\n")]);
    let tip = up.git(&["rev-parse", "main"]);
    let repos = tmp.path().join("repos");
    let mut config = up.land_config(repos.clone());
    config.git = wrapper(
        tmp.path(),
        "moves",
        "*'refs/cuenca/followed^{commit}'*",
        &format!(
            "git \"$@\"; s=$?; git -C '{repo}' update-ref refs/cuenca/followed {first}; exit $s",
            repo = repos.join("l1.git").display()
        ),
    );
    let _lander = lander(&s, config);
    assert!(
        within(Duration::from_secs(30), || main_of(store(&s), "l1")
            == merged),
        "main did not follow"
    );
    let loc1 = store(&s).location(&loc("l1")).unwrap().unwrap();
    assert_eq!(loc1.followed.map(|f| f.commit.to_hex()), Some(tip));
}

/// A fetch that fails on some other lock is reported as git said it, not as the
/// followed ref's lock with its recovery advice.
#[test]
fn a_fetch_failing_on_another_lock_is_not_the_followed_refs_lock() {
    let s = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let up = Upstream::new(tmp.path(), &[("README", "one\n")]);
    register(&s, &up, "l1");
    let a1 = agent(&s, &["l1"]);
    let mut config = up.land_config(tmp.path().join("repos"));
    config.git = wrapper(
        tmp.path(),
        "otherlock",
        "*' fetch '*",
        "echo \"fatal: Unable to create '/elsewhere/packed-refs.lock': File exists.\" >&2; exit 128",
    );
    let _lander = lander(&s, config);
    merge_on_remote(&up, &[("merged.txt", b"m\n")]);
    assert!(within(Duration::from_secs(30), || {
        follow_error(&s, &a1, "l1").is_some_and(|e| e.contains("packed-refs.lock"))
    }));
    let e = follow_error(&s, &a1, "l1").unwrap();
    assert!(!e.contains("never removed automatically"), "{e}");
}

/// A fetch that fails for another reason on the followed ref's lock path (no
/// permission) is reported as git said it, not as a lock held by someone.
#[test]
fn a_lock_path_without_permission_is_not_a_held_lock() {
    let s = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let up = Upstream::new(tmp.path(), &[("README", "one\n")]);
    register(&s, &up, "l1");
    let a1 = agent(&s, &["l1"]);
    let mut config = up.land_config(tmp.path().join("repos"));
    config.git = wrapper(
        tmp.path(),
        "noperm",
        "*' fetch '*",
        "echo \"fatal: cannot lock ref 'refs/cuenca/followed': Unable to create '/r/refs/cuenca/followed.lock': Permission denied\" >&2; exit 128",
    );
    let _lander = lander(&s, config);
    merge_on_remote(&up, &[("merged.txt", b"m\n")]);
    assert!(within(Duration::from_secs(30), || {
        follow_error(&s, &a1, "l1").is_some_and(|e| e.contains("Permission denied"))
    }));
    let e = follow_error(&s, &a1, "l1").unwrap();
    assert!(!e.contains("never removed automatically"), "{e}");
}

/// A git wrapper at `dir/git-pause` that logs every call to `dir/calls`, and pauses a
/// call matching `pattern`: it creates `dir/reached`, then waits (30 s at most) for
/// `dir/go` before running the real git.
fn pausing(dir: &Path, pattern: &str) -> PathBuf {
    let script = dir.join("git-pause");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho \"$*\" >> '{d}/calls'\ncase \" $* \" in {pattern}) : > '{d}/reached'; i=0; while [ ! -f '{d}/go' ] && [ $i -lt 300 ]; do sleep 0.1; i=$((i+1)); done ;; esac\nexec git \"$@\"\n",
            d = dir.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    script
}

/// Once the lander is told to stop, a follow starts no further git call: stopping
/// while the commit is being resolved, the tree is never resolved. The wrapper holds
/// the commit's resolution until the stop has been requested.
#[test]
fn a_stopping_lander_starts_no_further_git_call() {
    let s = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let up = Upstream::new(tmp.path(), &[("README", "one\n")]);
    register(&s, &up, "l1");
    merge_on_remote(&up, &[("merged.txt", b"m\n")]);
    let mut config = up.land_config(tmp.path().join("repos"));
    config.git = pausing(tmp.path(), "*'refs/cuenca/followed^{commit}'*");
    let lander = lander(&s, config);
    assert!(within(Duration::from_secs(30), || tmp
        .path()
        .join("reached")
        .exists()));
    lander.stop();
    std::fs::write(tmp.path().join("go"), "").unwrap();
    drop(lander);
    let log = std::fs::read_to_string(tmp.path().join("calls")).unwrap();
    assert!(
        !log.lines().any(|l| l.contains("^{tree}")),
        "a git call started after the stop:\n{log}"
    );
    assert_ne!(main_of(store(&s), "l1"), remote_tree(&up));
}

/// Stopping while a batch of blobs is read: no blob of it is ingested and main does
/// not move, even though the read itself completes.
#[test]
fn a_stopping_lander_ingests_nothing_more_and_records_no_follow() {
    let s = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let up = Upstream::new(tmp.path(), &[("README", "one\n")]);
    register(&s, &up, "l1");
    let main = main_of(store(&s), "l1");
    merge_on_remote(
        &up,
        &[("a.txt", b"stopping a\n"), ("b.txt", b"stopping b\n")],
    );
    let mut config = up.land_config(tmp.path().join("repos"));
    // The tree objects' batch runs first; the blobs' batch (two blobs) is held.
    let seen = tmp.path().join("seen");
    let script = pausing(tmp.path(), "*' --batch '*");
    let body = std::fs::read_to_string(&script).unwrap().replace(
        "*' --batch '*) :",
        &format!(
            "*' --batch '*) [ ! -f '{s}' ] && {{ : > '{s}'; exec git \"$@\"; }}; :",
            s = seen.display()
        ),
    );
    std::fs::write(&script, body).unwrap();
    config.git = script;
    let lander = lander(&s, config);
    assert!(within(Duration::from_secs(30), || tmp
        .path()
        .join("reached")
        .exists()));
    lander.stop();
    std::fs::write(tmp.path().join("go"), "").unwrap();
    drop(lander);
    assert_eq!(main_of(store(&s), "l1"), main);
    for content in [&b"stopping a\n"[..], b"stopping b\n"] {
        assert!(
            store(&s)
                .blob_info(GitOid::for_blob(content))
                .unwrap()
                .is_none()
        );
    }
}

/// A blob bigger than a batch and under the per-blob cap follows, in a batch of its
/// own.
#[test]
fn a_100_mib_blob_follows() {
    let s = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let up = Upstream::new(tmp.path(), &[("README", "one\n")]);
    register(&s, &up, "l1");
    let _lander = lander(&s, up.land_config(tmp.path().join("repos")));
    let big: Vec<u8> = (0..100 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    let merged = merge_on_remote(&up, &[("big.bin", &big)]);
    assert!(within(Duration::from_secs(120), || main_of(
        store(&s),
        "l1"
    ) == merged));
    let oid = GitOid::for_blob(&big);
    assert_eq!(
        store(&s).blob_info(oid).unwrap().unwrap().size,
        big.len() as u64
    );
}

/// An update with more blob bytes than fit in one follow's deadline follows across
/// rounds: each batch is kept once ingested, and a later follow reads only the rest.
#[test]
fn an_update_too_big_for_one_follow_follows_across_rounds() {
    let s = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let up = Upstream::new(tmp.path(), &[("README", "one\n")]);
    register(&s, &up, "l1");
    let a1 = agent(&s, &["l1"]);
    let mut config = up.land_config(tmp.path().join("repos"));
    // Every third `cat-file --batch` hangs: each round reads the tree objects (one call)
    // and one 1 MiB batch, then hangs on the next until the deadline. So each follow
    // gets exactly one batch in, whatever the host's speed, and must keep it.
    let count = tmp.path().join("count");
    config.git = wrapper(
        tmp.path(),
        "onebatch",
        "*' --batch '*",
        &format!(
            "n=$(cat '{c}' 2>/dev/null || echo 0); n=$((n+1)); echo $n > '{c}'; \
             if [ $((n % 3)) -eq 0 ]; then exec sleep 30; fi",
            c = count.display()
        ),
    );
    config.follow_timeout = Duration::from_secs(5);
    config.follow_batch_bytes = 1024 * 1024;
    let files: Vec<(String, Vec<u8>)> = (0..6)
        .map(|i| (format!("part{i}.bin"), vec![b'a' + i as u8; 1024 * 1024]))
        .collect();
    let refs: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(p, c)| (p.as_str(), c.as_slice()))
        .collect();
    let merged = merge_on_remote(&up, &refs);
    let _lander = lander(&s, config);
    let saw_partial = std::cell::Cell::new(false);
    let followed = within(Duration::from_secs(120), || {
        if follow_error(&s, &a1, "l1").is_some_and(|e| e.contains("bytes of blobs left")) {
            saw_partial.set(true);
        }
        main_of(store(&s), "l1") == merged
    });
    assert!(followed, "{:?}", follow_error(&s, &a1, "l1"));
    assert!(
        saw_partial.get(),
        "the update fit in one follow: the test proves nothing"
    );
    for (_, content) in &files {
        assert!(
            store(&s)
                .blob_info(GitOid::for_blob(content))
                .unwrap()
                .is_some()
        );
    }
}

/// A follow whose single batch cannot fit in the deadline fails naming its step, the
/// time it took and the bytes left, and main stays.
#[test]
fn a_follow_that_cannot_fit_fails_visibly_and_main_stays() {
    let s = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let up = Upstream::new(tmp.path(), &[("README", "one\n")]);
    register(&s, &up, "l1");
    let a1 = agent(&s, &["l1"]);
    let main = main_of(store(&s), "l1");
    let mut config = up.land_config(tmp.path().join("repos"));
    // The first `cat-file --batch` of the follow, the tree objects', answers; the next,
    // the blobs', hangs.
    let seen = tmp.path().join("seen");
    config.git = wrapper(
        tmp.path(),
        "stuck",
        "*' --batch '*",
        &format!(
            "if [ -f '{seen}' ]; then exec sleep 30; fi; : > '{seen}'",
            seen = seen.display()
        ),
    );
    config.follow_timeout = Duration::from_secs(2);
    let _lander = lander(&s, config);
    merge_on_remote(&up, &[("new.txt", b"twelve bytes")]);
    assert!(
        within(Duration::from_secs(30), || {
            follow_error(&s, &a1, "l1").is_some_and(|e| {
                e.starts_with("following stopped while reading blobs after ")
                    && e.contains("12 bytes of blobs left")
                    && e.contains("did not end before the deadline")
            })
        }),
        "{:?}",
        follow_error(&s, &a1, "l1")
    );
    assert_eq!(main_of(store(&s), "l1"), main);
}

/// A followed tree's ruled blobs are checked before main moves: a `.gitattributes`
/// line over the limit fails the follow, and main stays.
#[test]
fn a_followed_tree_breaking_a_content_rule_does_not_follow() {
    let s = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let up = Upstream::new(tmp.path(), &[("README", "one\n")]);
    register(&s, &up, "l1");
    let a1 = agent(&s, &["l1"]);
    let main = main_of(store(&s), "l1");
    let _lander = lander(&s, up.land_config(tmp.path().join("repos")));
    let line = vec![b'a'; cuenca_tree::MAX_GITATTRIBUTES_LINE + 10];
    merge_on_remote(&up, &[(".gitattributes", &line)]);
    assert!(within(Duration::from_secs(30), || {
        follow_error(&s, &a1, "l1").is_some_and(|e| e.contains(".gitattributes"))
    }));
    assert_eq!(main_of(store(&s), "l1"), main);
}

/// A tree listing past the `ls-tree` cap fails as a cap, naming the `LandConfig`
/// field, not as a timeout; with the cap raised the same tree follows.
#[test]
fn a_listing_past_its_cap_fails_until_the_cap_is_raised() {
    let s = Server::start();
    let tmp = tempfile::tempdir().unwrap();
    let up = Upstream::new(tmp.path(), &[("README", "one\n")]);
    register(&s, &up, "l1");
    let a1 = agent(&s, &["l1"]);
    let mut config = up.land_config(tmp.path().join("repos"));
    config.caps.ls_tree = 64;
    let lander1 = lander(&s, config);
    let merged = merge_on_remote(&up, &[("a.txt", b"a\n"), ("dir/b.txt", b"b\n")]);
    assert!(
        within(Duration::from_secs(30), || {
            follow_error(&s, &a1, "l1").is_some_and(|e| {
                e.contains("past its 64-byte output cap") && e.contains("LandConfig::caps.ls_tree")
            })
        }),
        "{:?}",
        follow_error(&s, &a1, "l1")
    );
    assert_ne!(main_of(store(&s), "l1"), merged);
    drop(lander1);
    let _lander = lander(&s, up.land_config(tmp.path().join("repos")));
    assert!(within(Duration::from_secs(30), || main_of(store(&s), "l1")
        == merged));
}
