//! Real authority + CLI + Git: a harness sees a true worktree, Cuenca sees its fork.
mod common;
use common::*;
use serde_json::json;
use std::os::unix::fs::PermissionsExt as _;
use std::process::Command;

#[test]
fn worktree_fork_edits_snapshot_and_isolation() {
    let a = Authority::start();
    let site = site(&a);
    // Git ignore and attributes must not remove or rewrite Cuenca content.
    std::fs::write(site.path().join(".gitignore"), "secret.txt\n").unwrap();
    std::fs::write(site.path().join("secret.txt"), "retained\n").unwrap();
    std::fs::write(site.path().join(".gitattributes"), "*.txt text eol=crlf\n").unwrap();
    std::os::unix::fs::symlink("docs/index.md", site.path().join("link")).unwrap();
    // Register the richer location independently of site().
    a.admin(&["location", "add", "rich", "--dir", s(site.path())]);
    let agent = Agent::login(&a, &a.mint("builder", &["--location", "rich"]));
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("work");
    let out = agent.ok(&["fork", "rich", "--name", "harness", "--checkout", s(&root)]);
    assert!(root.join(".git").is_file());
    assert_eq!(
        git(&root, &["rev-parse", "--show-toplevel"]),
        s(&root.canonicalize().unwrap())
    );
    assert!(git(&root, &["worktree", "list", "--porcelain"]).contains(s(&root)));
    assert_eq!(git(&root, &["rev-parse", "HEAD^{tree}"]), out["head"]);
    assert!(git(&root, &["status", "--porcelain"]).is_empty());
    assert!(git(&root, &["remote"]).is_empty());
    assert_eq!(
        std::fs::read(root.join("secret.txt")).unwrap(),
        b"retained\n"
    );
    assert_eq!(
        std::fs::read_link(root.join("link")).unwrap(),
        std::path::Path::new("docs/index.md")
    );
    assert_ne!(
        std::fs::metadata(root.join("run.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0
    );
    let v = agent.ok(&["status", "--dir", s(&root)]);
    assert_eq!(v["changes"], json!([]), "{v}");
    // A harness uses cwd directly; its edit publishes as a Cuenca draft, including
    // content gitignore names, and never touches the source location or another fork.
    let harness = Command::new("/bin/sh")
        .current_dir(&root)
        .args([
            "-c",
            "printf 'harness edit\n' > secret.txt; printf 'new\n' > new.txt",
        ])
        .output()
        .unwrap();
    assert!(harness.status.success());
    agent.ok(&["snapshot", "--dir", s(&root)]);
    assert_ne!(agent.head("harness"), out["head"]);
    let copy = temp.path().join("copy");
    agent.ok(&["checkout", "harness", s(&copy)]);
    assert_eq!(
        std::fs::read(copy.join("secret.txt")).unwrap(),
        b"harness edit\n"
    );
    assert_eq!(std::fs::read(copy.join("new.txt")).unwrap(), b"new\n");
    assert!(!copy.join(".git").exists());
    assert_eq!(
        std::fs::read(site.path().join("secret.txt")).unwrap(),
        b"retained\n"
    );
    // Reusing a live checkout is refused, even if empty after a harness removes files.
    let (code, _) = agent.run(&["worktree", "harness", s(&root)]);
    assert_ne!(code, 0);
    let second = temp.path().join("second");
    let out2 = agent.ok(&["worktree", "harness", s(&second)]);
    assert_ne!(out["git_dir"], out2["git_dir"]);
}

#[test]
fn worktree_refuses_nonempty_symlink_and_nested_destinations() {
    let a = Authority::start();
    let _site = site(&a);
    let agent = Agent::login(&a, &a.mint("builder", &["--location", "site"]));
    agent.ok(&["fork", "site", "--name", "harness"]);
    let temp = tempfile::tempdir().unwrap();
    let occupied = temp.path().join("occupied");
    std::fs::create_dir(&occupied).unwrap();
    std::fs::write(occupied.join("keep"), "untouched").unwrap();
    let (code, v) = agent.run(&["worktree", "missing", s(&occupied)]);
    assert_ne!(code, 0, "{v}");
    assert!(
        v["error"].as_str().unwrap().contains("missing or empty"),
        "{v}"
    );
    assert_eq!(
        std::fs::read_to_string(occupied.join("keep")).unwrap(),
        "untouched"
    );
    assert!(!occupied.join(".git").exists());
    let link = temp.path().join("link");
    std::os::unix::fs::symlink(&occupied, &link).unwrap();
    assert_ne!(agent.run(&["worktree", "harness", s(&link)]).0, 0);
    let empty = temp.path().join("empty");
    std::fs::create_dir(&empty).unwrap();
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&empty, &link).unwrap();
    let (code, v) = agent.run(&["worktree", "harness", s(&link)]);
    assert_ne!(code, 0, "{v}");
    assert!(std::fs::read_dir(&empty).unwrap().next().is_none());
    let root = temp.path().join("root");
    agent.ok(&["checkout", "harness", s(&root)]);
    assert_ne!(
        agent
            .run(&["worktree", "harness", s(&root.join("nested"))])
            .0,
        0
    );
    assert!(!root.join("nested").exists());
}

#[test]
fn worktree_ignores_redirecting_git_environment_and_hooks() {
    let a = Authority::start();
    let _site = site(&a);
    let agent = Agent::login(&a, &a.mint("builder", &["--location", "site"]));
    agent.ok(&["fork", "site", "--name", "harness"]);
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("root");
    let hooks = temp.path().join("hooks");
    std::fs::create_dir(&hooks).unwrap();
    let marker = temp.path().join("hook-ran");
    let hook = hooks.join("post-checkout");
    std::fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = agent
        .cmd(&["--json", "worktree", "harness", s(&root)])
        .env("GIT_DIR", temp.path().join("wrong"))
        .env("GIT_INDEX_FILE", temp.path().join("index"))
        .env("GIT_TEMPLATE_DIR", &hooks)
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "core.hooksPath")
        .env("GIT_CONFIG_VALUE_0", &hooks)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(!marker.exists());
    assert!(!temp.path().join("index").exists());
    assert!(!temp.path().join("wrong").exists());
}
