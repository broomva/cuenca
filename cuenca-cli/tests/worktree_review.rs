//! Round 1 P20 reproductions: path spellings, bootstrap side effects and checkout cost.
mod common;
use common::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{BufRead as _, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

#[test]
fn worktree_refuses_every_symlink_component_spelling() {
    let a = Authority::start();
    let _site = site(&a);
    let agent = Agent::login(&a, &a.mint("builder", &["--location", "site"]));
    agent.ok(&["fork", "site", "--name", "harness"]);
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let mut accepted = Vec::new();
    for (i, suffix) in ["", "/", "/.", "//", "/new", "/child/new", "/../unused"]
        .iter()
        .enumerate()
    {
        let case = base.join(i.to_string());
        std::fs::create_dir(&case).unwrap();
        let target = case.join("target");
        std::fs::create_dir(&target).unwrap();
        if suffix.contains("child") {
            std::fs::create_dir(target.join("child")).unwrap();
        }
        std::os::unix::fs::symlink(&target, case.join("link")).unwrap();
        let dest = format!("{}/link{suffix}", case.display());
        let (code, value) = agent.run(&["worktree", "harness", &dest]);
        if code == 0 {
            accepted.push(dest.clone());
        } else {
            assert!(
                value["error"].as_str().unwrap().contains("symlink"),
                "{dest}: {value}"
            );
        }
        assert!(!target.join(".git").exists() || code == 0);
    }
    // Relative input must be checked before cwd-relative normalization as well.
    let case = base.join("relative");
    std::fs::create_dir(&case).unwrap();
    let target = case.join("target");
    std::fs::create_dir(&target).unwrap();
    std::os::unix::fs::symlink(&target, case.join("link")).unwrap();
    let out = agent
        .cmd(&["--json", "worktree", "harness", "link/."])
        .current_dir(&case)
        .output()
        .unwrap();
    if out.status.success() {
        accepted.push("relative link/.".into());
    }
    assert!(
        accepted.is_empty(),
        "symlink destinations accepted: {accepted:?}"
    );
    for entry in std::fs::read_dir(&base).unwrap() {
        let target = entry.unwrap().path().join("target");
        assert!(!target.join(".git").exists());
        assert!(!target.join("new").exists());
        assert!(!target.join("child/new").exists());
    }
}

#[test]
fn worktree_invalid_destination_creates_no_authority_fork() {
    let a = Authority::start();
    let _site = site(&a);
    let agent = Agent::login(&a, &a.mint("builder", &["--location", "site"]));
    let temp = tempfile::tempdir().unwrap();
    let occupied = temp.path().join("occupied");
    std::fs::create_dir(&occupied).unwrap();
    std::fs::write(occupied.join("keep"), "untouched").unwrap();
    let target = temp.path().join("target");
    std::fs::create_dir(&target).unwrap();
    let link = temp.path().join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let missing_parent = temp.path().join("absent/new");
    let readonly = temp.path().join("readonly");
    std::fs::create_dir(&readonly).unwrap();
    std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o555)).unwrap();
    let before = agent.ok(&["forks"])["forks"].clone();
    for (i, dest) in [
        s(&occupied),
        &format!("{}/", link.display()),
        s(&missing_parent),
        s(&readonly),
    ]
    .iter()
    .enumerate()
    {
        let (code, value) = agent.run(&[
            "fork",
            "site",
            "--name",
            &format!("invalid-{i}"),
            "--checkout",
            dest,
        ]);
        assert_ne!(code, 0, "{value}");
    }
    let after = agent.ok(&["forks"])["forks"].clone();
    assert_eq!(after, before, "local validation orphaned authority forks");
    assert_eq!(
        std::fs::read_to_string(occupied.join("keep")).unwrap(),
        "untouched"
    );
    assert!(!target.join(".git").exists());
    if readonly.exists() {
        std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

#[test]
fn worktree_readonly_empty_destination_survives_failure() {
    let a = Authority::start();
    let _site = site(&a);
    let agent = Agent::login(&a, &a.mint("builder", &["--location", "site"]));
    agent.ok(&["fork", "site", "--name", "harness"]);
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("readonly");
    std::fs::create_dir(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o555)).unwrap();
    let (code, value) = agent.run(&["worktree", "harness", s(&root)]);
    assert_ne!(code, 0, "{value}");
    let preserved = root.is_dir();
    if preserved {
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    assert!(preserved, "Git removed the caller's empty directory");
    assert!(std::fs::read_dir(&root).unwrap().next().is_none());
}

#[test]
fn worktree_initial_index_cache_and_ignored_report_match_plain_checkout() {
    let a = Authority::start();
    let up = Upstream::new(&[
        ("main.txt", "main\n"),
        (".DS_Store", "finder"),
        ("node_modules/dep.js", "x\n"),
    ]);
    a.admin(&["location", "add", "rich", "--git", s(&up.work)]);
    let agent = Agent::login(&a, &a.mint("builder", &["--location", "rich"]));
    agent.ok(&["fork", "rich", "--name", "harness"]);
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("work");
    let report = agent.ok(&["worktree", "harness", s(&root)]);
    // Do this before git status, which refreshes the index and masks the defect.
    assert!(git(&root, &["diff-files", "--name-only"]).is_empty());
    assert_eq!(report["clean"], true, "{report}");
    assert_eq!(
        report["ignored"],
        json!([".DS_Store", "node_modules/dep.js"]),
        "{report}"
    );
    let mut caches = 0;
    for entry in std::fs::read_dir(agent.home.path().join("state/checkouts")).unwrap() {
        let dir = entry.unwrap().path();
        let state: Value =
            serde_json::from_slice(&std::fs::read(dir.join("state.json")).unwrap()).unwrap();
        if state["root"] == json!(root.canonicalize().unwrap()) {
            let cache: Value = serde_json::from_slice(
                &std::fs::read(dir.join("cache.json")).expect("initial cache persisted"),
            )
            .unwrap();
            assert_eq!(cache["format"], 1);
            caches += 1;
        }
    }
    assert_eq!(caches, 1);
    let plain = agent.ok(&["checkout", "harness", s(&temp.path().join("plain"))]);
    assert_eq!(
        (report["clean"].clone(), report["ignored"].clone()),
        (plain["clean"].clone(), plain["ignored"].clone())
    );
}

#[test]
fn worktree_private_repo_ignores_global_partialclone() {
    let a = Authority::start();
    let _site = site(&a);
    let agent = Agent::login(&a, &a.mint("builder", &["--location", "site"]));
    agent.ok(&["fork", "site", "--name", "harness"]);
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("global.gitconfig");
    std::fs::write(&config, "[extensions]\npartialclone = poisoned\n").unwrap();
    let out = agent
        .cmd(&[
            "--json",
            "worktree",
            "harness",
            s(&temp.path().join("work")),
        ])
        .env("GIT_CONFIG_GLOBAL", &config)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
}

// A real HTTP relay counts blob reads, including duplicate files in different dirs.
struct CountingProxy {
    url: String,
    counts: Arc<Mutex<BTreeMap<String, usize>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl CountingProxy {
    fn new(authority: &Authority) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let counts = Arc::new(Mutex::new(BTreeMap::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (seen, halted, upstream) = (counts.clone(), stop.clone(), authority.url());
        let thread = std::thread::spawn(move || {
            let client = reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap();
            while !halted.load(Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(s) => s,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(e) => panic!("{e}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let mut input = std::io::BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                input.read_line(&mut first).unwrap();
                let path = first.split_whitespace().nth(1).unwrap().to_string();
                let mut auth = String::new();
                loop {
                    let mut line = String::new();
                    input.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if line.to_ascii_lowercase().starts_with("authorization:") {
                        auth = line.split_once(':').unwrap().1.trim().to_string();
                    }
                }
                *seen.lock().unwrap().entry(path.clone()).or_insert(0) += 1;
                let response = client
                    .get(format!("{upstream}{path}"))
                    .header("authorization", auth)
                    .send()
                    .unwrap();
                let status = response.status().as_u16();
                let bytes = response.bytes().unwrap();
                write!(
                    stream,
                    "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    bytes.len()
                )
                .unwrap();
                stream.write_all(&bytes).unwrap();
            }
        });
        Self {
            url,
            counts,
            stop,
            thread: Some(thread),
        }
    }
}
impl Drop for CountingProxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.take().unwrap().join().unwrap();
    }
}

#[test]
fn worktree_fetches_each_blob_once_and_reuses_verified_staging() {
    let a = Authority::start();
    let site = site(&a);
    std::fs::write(site.path().join("duplicate.md"), "# site\n").unwrap();
    a.admin(&["location", "add", "duplicates", "--dir", s(site.path())]);
    let agent = Agent::login(&a, &a.mint("builder", &["--location", "duplicates"]));
    agent.ok(&["fork", "duplicates", "--name", "harness"]);
    let proxy = CountingProxy::new(&a);
    let temp = tempfile::tempdir().unwrap();
    let out = agent
        .cmd(&[
            "--json",
            "worktree",
            "harness",
            s(&temp.path().join("work")),
        ])
        .env("CUENCA_URL", &proxy.url)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let counts = proxy.counts.lock().unwrap();
    let blobs: Vec<_> = counts
        .iter()
        .filter(|(path, _)| path.contains("/blobs/"))
        .collect();
    assert_eq!(blobs.len(), 2, "{counts:?}");
    assert!(
        blobs.iter().all(|(_, count)| **count == 1),
        "duplicate HTTP blob fetch: {counts:?}"
    );
}
