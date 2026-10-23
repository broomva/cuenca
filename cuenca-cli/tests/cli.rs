//! The `cuenca` binary end to end: a real `cuenca serve` (bound to 127.0.0.1 with
//! `--unsafe-bind`, its `tailscale` a script naming every source as one node), owner acts
//! through `cuenca admin`, and every agent verb through the binary, each client with its
//! own config and state directory.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

const NODE: &str = "nMac11CNTRL";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_cuenca")
}

/// A `cuenca serve` on a port of its own, restartable on the same state and port.
struct Authority {
    dir: tempfile::TempDir,
    port: u16,
    child: Option<Child>,
}

impl Authority {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let script = dir.path().join("tailscale");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\ncase \"$1\" in\n  whois) echo '{{\"Node\":{{\"StableID\":\"{NODE}\"}}}}' ;;\n  status) echo '{{\"Self\":{{\"ID\":\"{NODE}\"}}}}' ;;\n  *) exit 1 ;;\nesac\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut a = Self {
            dir,
            port,
            child: None,
        };
        a.up();
        a
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn socket(&self) -> PathBuf {
        self.dir.path().join("admin.sock")
    }

    fn up(&mut self) {
        let child = Command::new(bin())
            .args(["serve", "--unsafe-bind", "--bind"])
            .arg(format!("127.0.0.1:{}", self.port))
            .arg("--state")
            .arg(self.dir.path().join("state"))
            .arg("--admin-socket")
            .arg(self.socket())
            .arg("--tailscale")
            .arg(self.dir.path().join("tailscale"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        self.child = Some(child);
        let deadline = Instant::now() + Duration::from_secs(60);
        while std::net::TcpStream::connect(("127.0.0.1", self.port)).is_err()
            || !self.socket().exists()
        {
            assert!(Instant::now() < deadline, "cuenca serve did not come up");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Kill the authority without warning.
    fn down(&mut self) {
        if let Some(mut c) = self.child.take() {
            c.kill().unwrap();
            c.wait().unwrap();
        }
    }

    fn admin(&self, args: &[&str]) -> Output {
        let out = Command::new(bin())
            .arg("admin")
            .arg("--socket")
            .arg(self.socket())
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "admin {args:?}: {out:?}");
        out
    }

    /// Mint an agent key and return its token.
    fn mint(&self, principal: &str, extra: &[&str]) -> String {
        let mut args = vec!["key", "mint", principal, "--kind", "agent", "--node", NODE];
        args.extend_from_slice(extra);
        let out = self.admin(&args);
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }
}

impl Drop for Authority {
    fn drop(&mut self) {
        self.down();
    }
}

/// One principal's `cuenca`, with its own HOME, config and state.
struct Agent {
    home: tempfile::TempDir,
}

impl Agent {
    fn login(a: &Authority, token: &str) -> Self {
        let me = Self {
            home: tempfile::tempdir().unwrap(),
        };
        let mut child = me
            .cmd(&["login", "--url", &a.url(), "--token-stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write as _;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(token.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "login: {out:?}");
        let mode = std::fs::metadata(me.home.path().join("config.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        me
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(bin());
        c.args(args)
            .env("HOME", self.home.path())
            .env("CUENCA_CONFIG", self.home.path().join("config.json"))
            .env("CUENCA_STATE_HOME", self.home.path().join("state"))
            .env_remove("CUENCA_URL")
            .env_remove("CUENCA_TOKEN");
        c
    }

    /// Run a verb with `--json`: its exit status and its JSON.
    fn run(&self, args: &[&str]) -> (i32, Value) {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        let out = self.cmd(&all).output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        let json = serde_json::from_str(text.trim())
            .unwrap_or_else(|e| panic!("{args:?} printed {text:?}: {e}; stderr {:?}", out.stderr));
        (out.status.code().unwrap(), json)
    }

    fn ok(&self, args: &[&str]) -> Value {
        let (code, v) = self.run(args);
        assert_eq!(code, 0, "{args:?}: {v}");
        v
    }

    /// The fork's head as the authority has it.
    fn head(&self, fork: &str) -> String {
        let v = self.ok(&["forks"]);
        v["forks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["name"] == fork)
            .unwrap_or_else(|| panic!("{fork} not in {v}"))["head"]
            .as_str()
            .unwrap()
            .to_string()
    }
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

/// A plain directory (not a git repository) registered as location `site`.
fn site(a: &Authority) -> tempfile::TempDir {
    let site = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(site.path().join("docs")).unwrap();
    std::fs::write(site.path().join("docs/index.md"), "# site\n").unwrap();
    std::fs::write(site.path().join("run.sh"), "#!/bin/sh\necho hi\n").unwrap();
    std::fs::set_permissions(
        site.path().join("run.sh"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    std::fs::write(site.path().join(".DS_Store"), "finder").unwrap();
    assert!(!site.path().join(".git").exists());
    a.admin(&["location", "add", "site", "--dir", s(site.path())]);
    site
}

#[test]
fn every_agent_verb_works_on_a_plain_directory() {
    let a = Authority::start();
    let _site = site(&a);
    let a1 = Agent::login(&a, &a.mint("a1", &["--location", "site"]));

    let v = a1.ok(&["forks"]);
    assert_eq!(v["locations"][0]["name"], "site", "{v}");
    let v = a1.ok(&["fork", "site", "--name", "f1"]);
    assert_eq!(v["fork"]["name"], "f1");
    let generated = a1.ok(&["fork", "site"]);
    assert!(
        generated["fork"]["name"]
            .as_str()
            .unwrap()
            .starts_with("site-a1-"),
        "{generated}"
    );

    let work = tempfile::tempdir().unwrap();
    let co = work.path().join("f1");
    let v = a1.ok(&["checkout", "f1", s(&co)]);
    assert_eq!(v["files"], 2, "{v}");
    assert_eq!(v["clean"], true);
    assert_eq!(
        std::fs::read_to_string(co.join("docs/index.md")).unwrap(),
        "# site\n"
    );
    assert!(!co.join(".DS_Store").exists());
    let mode = std::fs::metadata(co.join("run.sh"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o111, 0o111);

    let dir = ["--dir", s(&co)];
    let v = a1.ok(&[&["status"][..], &dir].concat());
    assert_eq!(v["changes"].as_array().unwrap().len(), 0, "{v}");
    assert_eq!(v["authority"]["epoch"], 1);

    std::fs::write(co.join("docs/index.md"), "# site, edited\n").unwrap();
    std::fs::write(co.join("new.txt"), "new\n").unwrap();
    std::fs::remove_file(co.join("run.sh")).unwrap();
    std::fs::create_dir(co.join("node_modules")).unwrap();
    std::fs::write(co.join("node_modules/dep.js"), "x").unwrap();
    let v = a1.ok(&[&["status"][..], &dir].concat());
    let changes: Vec<(String, String)> = v["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["path"].as_str().unwrap().to_string(),
                c["change"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        changes,
        [
            ("docs/index.md".into(), "modified".into()),
            ("new.txt".into(), "added".into()),
            ("run.sh".into(), "deleted".into()),
        ]
    );
    let v = a1.ok(&[&["diff"][..], &dir].concat());
    assert_eq!(v["changes"].as_array().unwrap().len(), 3, "{v}");

    let v = a1.ok(&[&["snapshot"][..], &dir].concat());
    assert_eq!(v["flush"]["published"].as_array().unwrap().len(), 1, "{v}");
    assert_eq!(v["flush"]["pending"], 0);
    let target = v["flush"]["published"][0]["target"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(a1.head("f1"), target);
    // Nothing changed since: a second snapshot publishes nothing.
    let v = a1.ok(&[&["snapshot"][..], &dir].concat());
    assert_eq!(v["changes"].as_array().unwrap().len(), 0, "{v}");
    assert_eq!(v["flush"]["published"].as_array().unwrap().len(), 0);

    // Run from inside the checkout, with no --dir.
    let inside = a1
        .cmd(&["--json", "log"])
        .current_dir(co.join("docs"))
        .output()
        .unwrap();
    assert!(inside.status.success(), "{inside:?}");
    let v: Value = serde_json::from_slice(&inside.stdout).unwrap();
    let kinds: Vec<_> = v["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["event"]["event"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(kinds, ["fork_created", "draft_published"], "{v}");

    let v = a1.ok(&["diff", "location:site", "f1"]);
    assert_eq!(v["changes"].as_array().unwrap().len(), 3, "{v}");

    let (code, v) = a1.run(&[&["verify"][..], &dir].concat());
    assert_eq!(code, 2, "{v}");
    assert_eq!(v["status"], "unavailable");

    let v = a1.ok(&[&["land"][..], &dir].concat());
    assert_eq!(v["status"], "requested", "{v}");

    // The human output works too.
    let out = a1.cmd(&["status", "--dir", s(&co)]).output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("checkout of fork f1"));
}

/// Two clients on one machine: an agent that may read another's fork, but does not hold
/// it, is refused every write, and the holder's fork is unchanged.
#[test]
fn a_write_from_a_non_holder_is_refused() {
    let a = Authority::start();
    let _site = site(&a);
    let a1 = Agent::login(&a, &a.mint("a1", &["--location", "site"]));
    let a2 = Agent::login(&a, &a.mint("a2", &["--location", "site", "--fork", "f1"]));
    a1.ok(&["fork", "site", "--name", "f1"]);
    let head = a1.head("f1");

    let work = tempfile::tempdir().unwrap();
    let co = work.path().join("f1-by-a2");
    a2.ok(&["checkout", "f1", s(&co)]);
    for i in 0..50 {
        std::fs::write(co.join("intruder.txt"), format!("{i}\n")).unwrap();
        let (code, v) = a2.run(&["snapshot", "--dir", s(&co)]);
        assert_eq!(code, 1, "attempt {i}: {v}");
        let error = v["flush"]["error"].as_str().unwrap();
        assert!(error.contains("not_holder"), "attempt {i}: {error}");
    }
    assert_eq!(a1.head("f1"), head);
    // Nothing was lost on a2's side: the edit is still in its checkout and its outbox.
    let v = a2.ok(&["status", "--dir", s(&co)]);
    assert_eq!(v["queued"], 50, "{v}");
    assert_eq!(
        std::fs::read_to_string(co.join("intruder.txt")).unwrap(),
        "49\n"
    );
}

/// The owner takes the fork over while its holder keeps working: the old holder's next
/// snapshots are kept in a sibling fork, which its checkout follows, and nothing is lost.
#[test]
fn after_a_takeover_the_old_holders_snapshots_go_to_a_sibling() {
    let a = Authority::start();
    let _site = site(&a);
    let a1 = Agent::login(&a, &a.mint("a1", &["--location", "site"]));
    let _a2 = a.mint("a2", &["--location", "site"]);
    a1.ok(&["fork", "site", "--name", "f1"]);
    let work = tempfile::tempdir().unwrap();
    let co = work.path().join("f1");
    a1.ok(&["checkout", "f1", s(&co)]);
    a.admin(&[
        "take-over",
        "f1",
        "--expected-epoch",
        "1",
        "--principal",
        "a2",
        "--node",
        NODE,
    ]);

    std::fs::write(co.join("late.txt"), "late\n").unwrap();
    let v = a1.ok(&["snapshot", "--dir", s(&co)]);
    let sent = &v["flush"]["published"][0];
    assert_eq!(sent["fenced"], true, "{v}");
    let sibling = sent["fork"].as_str().unwrap().to_string();
    assert_ne!(sibling, "f1");
    let st = a1.ok(&["status", "--dir", s(&co)]);
    assert_eq!(st["fork"], sibling.as_str(), "{st}");

    std::fs::write(co.join("later.txt"), "later\n").unwrap();
    let v = a1.ok(&["snapshot", "--dir", s(&co)]);
    assert_eq!(v["flush"]["published"][0]["fork"], sibling.as_str(), "{v}");
    let again = work.path().join("sibling");
    a1.ok(&["checkout", &sibling, s(&again)]);
    for f in ["late.txt", "later.txt", "docs/index.md"] {
        assert_eq!(
            std::fs::read(again.join(f)).unwrap(),
            std::fs::read(co.join(f)).unwrap(),
            "{f}"
        );
    }
}

/// The authority is unreachable while the agent keeps working: every snapshot queues,
/// and all of them publish, in order, once it is back. No file is lost.
#[test]
fn snapshots_taken_offline_publish_on_reconnect() {
    let mut a = Authority::start();
    let _site = site(&a);
    let a1 = Agent::login(&a, &a.mint("a1", &["--location", "site"]));
    a1.ok(&["fork", "site", "--name", "f1"]);
    let work = tempfile::tempdir().unwrap();
    let co = work.path().join("f1");
    a1.ok(&["checkout", "f1", s(&co)]);

    a.down();
    for i in 0..5 {
        std::fs::write(co.join(format!("offline-{i}.txt")), format!("{i}\n")).unwrap();
        let (code, v) = a1.run(&["snapshot", "--dir", s(&co)]);
        assert_eq!(code, 0, "{v}");
        assert_eq!(v["flush"]["offline"], true, "{v}");
        assert_eq!(v["flush"]["pending"], i + 1);
    }
    // Edits after a snapshot do not change what that snapshot publishes.
    std::fs::write(co.join("offline-0.txt"), "rewritten\n").unwrap();
    let v = a1.ok(&["status", "--dir", s(&co)]);
    assert_eq!(v["queued"], 5);
    assert_eq!(v["authority"]["offline"], true, "{v}");

    a.up();
    let v = a1.ok(&["snapshot", "--dir", s(&co)]);
    assert_eq!(v["flush"]["published"].as_array().unwrap().len(), 6, "{v}");
    assert_eq!(v["flush"]["pending"], 0);
    let log = a1.ok(&["log", "f1"]);
    assert_eq!(log["records"].as_array().unwrap().len(), 7, "{log}");

    let again = work.path().join("f1-again");
    a1.ok(&["checkout", "f1", s(&again)]);
    for i in 0..5 {
        let name = format!("offline-{i}.txt");
        assert_eq!(
            std::fs::read(again.join(&name)).unwrap(),
            std::fs::read(co.join(&name)).unwrap(),
            "{name}"
        );
    }
}

/// In watch mode a file written in the checkout reaches the authority's fork within 15 s.
#[test]
fn watch_mode_publishes_a_new_file_within_15_s() {
    let a = Authority::start();
    let _site = site(&a);
    let a1 = Agent::login(&a, &a.mint("a1", &["--location", "site"]));
    a1.ok(&["fork", "site", "--name", "f1"]);
    let work = tempfile::tempdir().unwrap();
    let co = work.path().join("f1");
    a1.ok(&["checkout", "f1", s(&co)]);
    let head = a1.head("f1");

    let mut watcher = a1
        .cmd(&["--json", "snapshot", "--watch", "--dir", s(&co)])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let written = Instant::now();
    std::fs::write(co.join("watched.txt"), "seen\n").unwrap();
    let mut moved = None;
    while written.elapsed() < Duration::from_secs(15) {
        if a1.head("f1") != head {
            moved = Some(written.elapsed());
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    watcher.kill().unwrap();
    watcher.wait().unwrap();
    let moved = moved.expect("the fork did not move within 15 s");
    eprintln!("watch: published {moved:?} after the write");
    let v = a1.ok(&["diff", "location:site", "f1"]);
    assert_eq!(v["changes"][0]["path"], "watched.txt", "{v}");
}

/// No agent verb exists for an owner act: minting a key, registering a location or
/// taking a fork over is only `cuenca admin`, which needs the authority's local socket.
#[test]
fn owner_acts_have_no_agent_verb() {
    let a = Authority::start();
    let a1 = Agent::login(&a, &a.mint("a1", &[]));
    for verb in [
        &["key", "mint", "x"][..],
        &["mint", "x"],
        &["location", "add", "x"],
        &["take-over", "f1"],
        &["revoke", "a1"],
        &["bypass"],
    ] {
        let out = a1.cmd(verb).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{verb:?}: {out:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("unrecognized subcommand"),
            "{verb:?}: {out:?}"
        );
    }
    // `cuenca admin` without the socket reaches nothing.
    let out = a1
        .cmd(&[
            "admin",
            "--socket",
            "/nonexistent/admin.sock",
            "key",
            "list",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
}

/// Spec row R07: a 1,000-file snapshot is published within 5 s. Not met yet: 11 s on a
/// Mac, because the authority makes a full sync per blob; batched ingest is BRO-3416.
#[test]
#[ignore = "R07 needs batched blob ingest (BRO-3416): 11 s today"]
fn a_thousand_file_snapshot_publishes_within_5_s() {
    let a = Authority::start();
    let _site = site(&a);
    let a1 = Agent::login(&a, &a.mint("a1", &["--location", "site"]));
    a1.ok(&["fork", "site", "--name", "f1"]);
    let work = tempfile::tempdir().unwrap();
    let co = work.path().join("f1");
    a1.ok(&["checkout", "f1", s(&co)]);
    for d in 0..10 {
        std::fs::create_dir(co.join(format!("d{d}"))).unwrap();
        for f in 0..100 {
            std::fs::write(co.join(format!("d{d}/f{f}.txt")), format!("file {d}/{f}\n")).unwrap();
        }
    }
    let started = Instant::now();
    let v = a1.ok(&["snapshot", "--dir", s(&co)]);
    let took = started.elapsed();
    assert_eq!(v["changes"].as_array().unwrap().len(), 1000);
    assert_eq!(v["flush"]["published"].as_array().unwrap().len(), 1, "{v}");
    eprintln!("R07: 1,000-file snapshot published in {took:?}");
    assert!(took < Duration::from_secs(5), "took {took:?}");
}

/// `dir`'s files, recursively, copied into `to`.
fn copy_dir(dir: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(dir).unwrap() {
        let e = e.unwrap();
        let dest = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &dest);
        } else {
            std::fs::copy(e.path(), dest).unwrap();
        }
    }
}

impl Agent {
    /// The state directory of this agent's only checkout.
    fn checkout_state(&self) -> PathBuf {
        let all: Vec<_> = std::fs::read_dir(self.home.path().join("state/checkouts"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(all.len(), 1, "{all:?}");
        all[0].clone()
    }

    fn records(&self, fork: &str) -> Vec<String> {
        self.ok(&["log", fork])["records"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["event"]["event"].as_str().unwrap().to_string())
            .collect()
    }
}

/// A path whose checkout was removed is not that checkout any more: checking another
/// fork out there replaces an empty registration, refuses one with queued snapshots
/// before writing a file, and no snapshot ever reaches the old fork from it.
#[test]
fn a_reused_checkout_path_never_publishes_to_the_old_fork() {
    let mut a = Authority::start();
    let _site = site(&a);
    let a1 = Agent::login(&a, &a.mint("a1", &["--location", "site"]));
    a1.ok(&["fork", "site", "--name", "f1"]);
    a1.ok(&["fork", "site", "--name", "f2"]);
    let work = tempfile::tempdir().unwrap();
    let co = work.path().join("w");
    a1.ok(&["checkout", "f1", s(&co)]);
    std::fs::write(co.join("f1-only.txt"), "mine\n").unwrap();
    a1.ok(&["snapshot", "--dir", s(&co)]);
    let f1 = a1.head("f1");

    // Removed and made again by hand: not the checkout.
    std::fs::remove_dir_all(&co).unwrap();
    std::fs::create_dir(&co).unwrap();
    std::fs::write(co.join("other.txt"), "x").unwrap();
    let (code, v) = a1.run(&["snapshot", "--dir", s(&co)]);
    assert_eq!(code, 1, "{v}");
    assert!(v["error"].as_str().unwrap().contains("removed or"), "{v}");
    std::fs::remove_dir_all(&co).unwrap();

    // Nothing queued: a new checkout replaces the stale registration.
    let v = a1.ok(&["checkout", "f2", s(&co)]);
    assert_eq!(v["fork"], "f2");
    std::fs::write(co.join("f2-only.txt"), "f2\n").unwrap();
    let v = a1.ok(&["snapshot", "--dir", s(&co)]);
    assert_eq!(v["flush"]["published"][0]["fork"], "f2", "{v}");
    assert_eq!(a1.head("f1"), f1);

    // Something queued: refused before a file is written, and the queue is kept.
    a.down();
    std::fs::write(co.join("queued.txt"), "q\n").unwrap();
    a1.ok(&["snapshot", "--dir", s(&co)]);
    a.up();
    std::fs::remove_dir_all(&co).unwrap();
    let (code, v) = a1.run(&["checkout", "f1", s(&co)]);
    assert_eq!(code, 1, "{v}");
    assert!(v["error"].as_str().unwrap().contains("still queued"), "{v}");
    assert!(!co.exists(), "nothing was written");
    assert_eq!(a1.head("f1"), f1);
}

/// A checkout inside another would be taken into the outer fork's snapshots: refused.
#[test]
fn a_checkout_inside_a_checkout_is_refused() {
    let a = Authority::start();
    let _site = site(&a);
    let a1 = Agent::login(&a, &a.mint("a1", &["--location", "site"]));
    a1.ok(&["fork", "site", "--name", "f1"]);
    a1.ok(&["fork", "site", "--name", "f2"]);
    let work = tempfile::tempdir().unwrap();
    let outer = work.path().join("f1");
    a1.ok(&["checkout", "f1", s(&outer)]);
    let inner = outer.join("docs/f2");
    let (code, v) = a1.run(&["checkout", "f2", s(&inner)]);
    assert_eq!(code, 1, "{v}");
    assert!(v["error"].as_str().unwrap().contains("is inside"), "{v}");
    assert!(!inner.exists());
}

/// The three windows a crash can open around acknowledging a draft, each simulated by
/// putting back the checkout's state as it was: the next snapshot publishes nothing
/// twice and loses nothing.
#[test]
fn a_crash_around_acknowledgement_loses_and_duplicates_nothing() {
    let mut a = Authority::start();
    let _site = site(&a);
    let a1 = Agent::login(&a, &a.mint("a1", &["--location", "site"]));
    let _a2 = a.mint("a2", &["--location", "site"]);
    a1.ok(&["fork", "site", "--name", "f1"]);
    let work = tempfile::tempdir().unwrap();
    let co = work.path().join("f1");
    a1.ok(&["checkout", "f1", s(&co)]);
    let state = a1.checkout_state();
    let saved = work.path().join("saved");
    let queue = |name: &str, a: &mut Authority| {
        std::fs::write(co.join(name), format!("{name}\n")).unwrap();
        a.down();
        a1.ok(&["snapshot", "--dir", s(&co)]);
        a.up();
    };
    let put_back = |what: &Path, to: &Path| {
        std::fs::remove_dir_all(to).unwrap();
        copy_dir(what, to);
    };

    // 1. The authority took the draft; the client never recorded it.
    queue("one.txt", &mut a);
    copy_dir(&state, &saved.join("1"));
    a1.ok(&["snapshot", "--dir", s(&co)]);
    put_back(&saved.join("1"), &state);
    let v = a1.ok(&["snapshot", "--dir", s(&co)]);
    assert_eq!(v["flush"]["pending"], 0, "{v}");
    assert_eq!(a1.records("f1"), ["fork_created", "draft_published"]);

    // 2. The client recorded it, but its outbox entry was left behind.
    queue("two.txt", &mut a);
    copy_dir(&state.join("outbox"), &saved.join("2"));
    a1.ok(&["snapshot", "--dir", s(&co)]);
    copy_dir(&saved.join("2"), &state.join("outbox"));
    let v = a1.ok(&["snapshot", "--dir", s(&co)]);
    assert_eq!(v["flush"]["published"].as_array().unwrap().len(), 0, "{v}");
    assert_eq!(a1.ok(&["status", "--dir", s(&co)])["queued"], 0);
    assert_eq!(a1.records("f1").len(), 3);

    // 3. The authority took it, the client never recorded it, and the fork was taken
    // over before the retry: the retry is recognised as already in, not fenced.
    queue("three.txt", &mut a);
    copy_dir(&state, &saved.join("3"));
    a1.ok(&["snapshot", "--dir", s(&co)]);
    put_back(&saved.join("3"), &state);
    a.admin(&[
        "take-over",
        "f1",
        "--expected-epoch",
        "1",
        "--principal",
        "a2",
        "--node",
        NODE,
    ]);
    let v = a1.ok(&["snapshot", "--dir", s(&co)]);
    assert_eq!(v["flush"]["pending"], 0, "{v}");
    assert_eq!(v["flush"]["published"][0]["fenced"], false, "{v}");
    let st = a1.ok(&["status", "--dir", s(&co)]);
    assert_eq!(st["queued"], 0);
    assert_eq!(st["fork"], "f1");
    let all = a1.ok(&["forks"]);
    assert!(
        all["forks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|f| f["parent"].is_null()),
        "no sibling: {all}"
    );
}

/// A file that never holds still fails every scan; watch mode says a snapshot is
/// overdue, as a failure, rather than staying quiet.
#[test]
fn watch_mode_reports_a_snapshot_it_cannot_take() {
    let a = Authority::start();
    let _site = site(&a);
    let a1 = Agent::login(&a, &a.mint("a1", &["--location", "site"]));
    a1.ok(&["fork", "site", "--name", "f1"]);
    let work = tempfile::tempdir().unwrap();
    let co = work.path().join("f1");
    a1.ok(&["checkout", "f1", s(&co)]);

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = {
        let stop = stop.clone();
        let log = co.join("dev.log");
        std::thread::spawn(move || {
            use std::io::Write as _;
            let mut f = std::fs::File::create(log).unwrap();
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                f.write_all(b"a log line\n").unwrap();
                std::thread::sleep(Duration::from_millis(1));
            }
        })
    };
    let out = a1
        .cmd(&[
            "--json",
            "snapshot",
            "--watch",
            "--max-wait",
            "2",
            "--dir",
            s(&co),
        ])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(6));
    let mut out = out;
    out.kill().unwrap();
    let output = out.wait_with_output().unwrap();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    writer.join().unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.lines().any(|l| l.contains("\"overdue\":true")),
        "{text}"
    );
}

/// A git location that commits paths the default `.forkignore` names: checkout, status
/// and diff succeed and name them; a snapshot refuses to delete them from the fork until
/// told `--drop-ignored`, and its message names that flag.
#[test]
fn a_git_location_with_ignored_paths_is_reported_not_broken() {
    let a = Authority::start();
    let repo = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
    };
    git(&["init", "-q"]);
    std::fs::create_dir_all(repo.path().join("node_modules")).unwrap();
    std::fs::write(repo.path().join("node_modules/dep.js"), "x\n").unwrap();
    std::fs::write(repo.path().join(".DS_Store"), "finder").unwrap();
    std::fs::write(repo.path().join("main.js"), "main\n").unwrap();
    // Forced: a global gitignore may name these too.
    git(&["add", "-f", "-A", "."]);
    git(&["add", "-f", ".DS_Store", "node_modules/dep.js"]);
    git(&["commit", "-qm", "one"]);
    a.admin(&["location", "add", "app", "--git", s(repo.path())]);
    let a1 = Agent::login(&a, &a.mint("a1", &["--location", "app"]));
    a1.ok(&["fork", "app", "--name", "f1"]);
    let work = tempfile::tempdir().unwrap();
    let co = work.path().join("f1");

    let v = a1.ok(&["checkout", "f1", s(&co)]);
    assert_eq!(v["clean"], true, "{v}");
    assert_eq!(v["ignored"].as_array().unwrap().len(), 2, "{v}");
    let v = a1.ok(&["status", "--dir", s(&co)]);
    assert_eq!(v["changes"].as_array().unwrap().len(), 0, "{v}");
    assert_eq!(v["ignored"].as_array().unwrap().len(), 2, "{v}");
    // Watch agrees the checkout is clean: no snapshot attempt, no overdue failure.
    let watcher = a1
        .cmd(&[
            "--json",
            "snapshot",
            "--watch",
            "--max-wait",
            "1",
            "--dir",
            s(&co),
        ])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(4));
    let mut watcher = watcher;
    watcher.kill().unwrap();
    let out = watcher.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.trim().is_empty(),
        "a clean checkout's watch printed: {text}"
    );
    std::fs::write(co.join("main.js"), "edited\n").unwrap();
    let v = a1.ok(&["diff", "--dir", s(&co)]);
    assert_eq!(v["changes"].as_array().unwrap().len(), 1, "{v}");
    assert_eq!(v["changes"][0]["path"], "main.js");

    let (code, v) = a1.run(&["snapshot", "--dir", s(&co)]);
    assert_eq!(code, 1, "{v}");
    assert!(
        v["error"].as_str().unwrap().contains("--drop-ignored"),
        "{v}"
    );
    let v = a1.ok(&["snapshot", "--drop-ignored", "--dir", s(&co)]);
    assert_eq!(v["flush"]["published"].as_array().unwrap().len(), 1, "{v}");
    let v = a1.ok(&["diff", "location:app", "f1"]);
    let mut paths: Vec<_> = v["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            format!(
                "{} {}",
                c["change"].as_str().unwrap(),
                c["path"].as_str().unwrap()
            )
        })
        .collect();
    paths.sort();
    assert_eq!(
        paths,
        [
            "deleted .DS_Store",
            "deleted node_modules/dep.js",
            "modified main.js"
        ]
    );
    assert!(co.join("node_modules/dep.js").exists(), "left on disk");
}
