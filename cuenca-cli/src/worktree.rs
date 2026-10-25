//! A harness-compatible git worktree of an authority fork. Git is a local adapter:
//! its synthetic commit has no upstream history or push remote. Cuenca owns publication.

use std::collections::HashSet;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context as _, ensure};
use cuenca_core::ForkName;
use cuenca_fs::{BlobSource, Fetched};
use cuenca_tree::GitOid;
use cuenca_tree::git::{Blobs, REDIRECTING_ENV, Repo};
use serde_json::json;

use crate::local::{Local, State};
use crate::{Client, Report};

struct Remote<'a> {
    client: &'a Client,
    fork: &'a ForkName,
}

impl BlobSource for Remote<'_> {
    fn fetch(&self, oid: GitOid) -> std::io::Result<Option<Fetched>> {
        self.client
            .fork_blob(self.fork, oid)
            .map(|b| b.map(Fetched::Bytes))
            .map_err(std::io::Error::other)
    }
}

// Never execute user hooks, filters, templates or credential helpers during creation.
// Every invocation uses only the private repository, even inside another git checkout.
fn git(dir: &Path, args: &[&std::ffi::OsStr], input: &[u8]) -> anyhow::Result<String> {
    let mut cmd = Command::new("git");
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            cmd.env_remove(key);
        }
    }
    for key in REDIRECTING_ENV {
        cmd.env_remove(key);
    }
    let mut child = cmd
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "user.name=Cuenca",
            "-c",
            "user.email=cuenca@localhost",
            "-c",
            "commit.gpgsign=false",
        ])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_NO_LAZY_FETCH", "1")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting git for worktree")?;
    // Reap even if git closes stdin early.
    let write = child.stdin.take().context("git stdin")?.write_all(input);
    let out = child.wait_with_output()?;
    ensure!(
        out.status.success(),
        "git worktree: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    write?;
    Ok(String::from_utf8(out.stdout)?.trim().to_string())
}

/// Materialise a fork as a detached git worktree, with exact Cuenca bytes and index.
/// The destination must be missing or empty, its parent must exist, and it must be
/// exclusively owned by this caller until return (as with `cuenca checkout`).
/// Failures preserve destination files; no force removal or reset is performed.
pub fn checkout(client: &Client, fork: &str, dest: &Path) -> anyhow::Result<Report> {
    let name = ForkName::new(fork)?;
    let root = Local::prepare(dest)?;
    ensure!(
        !root.exists() || (root.is_dir() && std::fs::read_dir(&root)?.next().is_none()),
        "worktree destination must be missing or empty: {}",
        root.display()
    );
    let f = client.fork(&name)?;
    let (head, tree) = client.fork_tree(&name)?;
    let parent = root
        .parent()
        .context("worktree destination has no parent")?;
    // Materialise on the destination filesystem, so each move below is a rename.
    let stage = tempfile::Builder::new()
        .prefix(".cuenca-stage-")
        .tempdir_in(parent)?;
    let files = stage.path().join("files");
    cuenca_fs::checkout(
        &tree,
        &files,
        &Remote {
            client,
            fork: &name,
        },
    )?;
    let stores = crate::config::state_home()?.join("worktrees");
    std::fs::create_dir_all(&stores)?;
    let backing = tempfile::Builder::new()
        .prefix("repo-")
        .tempdir_in(stores)?;
    let repo = backing.path();
    git(
        repo,
        &[
            "init".as_ref(),
            "--bare".as_ref(),
            "--template=".as_ref(),
            "--object-format=sha1".as_ref(),
        ],
        b"",
    )?;
    // Feed one verified blob at a time, rather than retaining a whole fork in RAM.
    let mut seen = HashSet::new();
    for (_, entry) in tree.entries() {
        if !seen.insert(entry.oid()) {
            continue;
        }
        let bytes = client
            .fork_blob(&name, entry.oid())?
            .context("fork blob missing")?;
        let oid = git(
            repo,
            &[
                "hash-object".as_ref(),
                "-w".as_ref(),
                "--stdin".as_ref(),
                "--no-filters".as_ref(),
            ],
            &bytes,
        )?;
        ensure!(oid == entry.oid().to_hex(), "fork blob hash mismatch");
    }
    let oid = Repo::open(repo).write_tree(&tree, &Blobs::new())?;
    ensure!(oid == head, "fork head/tree mismatch");
    let commit = git(
        repo,
        &[
            "commit-tree".as_ref(),
            oid.to_hex().as_ref(),
            "-m".as_ref(),
            "Cuenca fork checkout".as_ref(),
        ],
        b"",
    )?;
    git(
        repo,
        &[
            "worktree".as_ref(),
            "add".as_ref(),
            "--detach".as_ref(),
            "--no-checkout".as_ref(),
            root.as_os_str(),
            commit.as_ref(),
        ],
        b"",
    )?;
    // Once .git points here, retain the backing store even if a later step fails.
    let repo = backing.keep();
    git(&root, &["read-tree".as_ref(), commit.as_ref()], b"")?;
    for entry in std::fs::read_dir(&files)? {
        let entry = entry?;
        std::fs::rename(entry.path(), root.join(entry.file_name()))?;
    }
    let _local = Local::create(State {
        format: 1,
        root: root.clone(),
        root_id: crate::local::identity(&root)?,
        url: client.url().to_string(),
        fork: name.clone(),
        location: f.location.clone(),
        epoch: f.epoch(),
        published: tree.clone(),
    })?;
    Ok(Report {
        json: json!({ "fork": name, "location": f.location, "epoch": f.epoch(), "head": head,
            "root": root, "git_dir": repo, "commit": commit, "files": tree.len() }),
        text: format!("worktree of {name} (head {head}) into {}\n", root.display()),
        exit: 0,
    })
}
