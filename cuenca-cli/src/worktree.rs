//! A harness-compatible git worktree of an authority fork. Git is a local adapter:
//! its synthetic commit has no upstream history or push remote. Cuenca owns publication.

use std::collections::HashSet;
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use rustix::fs::{AtFlags, CWD, FileType, OFlags, openat, statat};
use rustix::io::Errno;
use std::process::{Command, Stdio};

use anyhow::{Context as _, ensure};
use cuenca_core::ForkName;
use cuenca_fs::{BlobSource, Fetched};
use cuenca_tree::GitOid;
use cuenca_tree::git::{Blobs, Repo};
use serde_json::json;

use crate::local::{Local, State};
use crate::{Client, Report};

struct Remote<'a> {
    client: &'a Client,
    fork: &'a ForkName,
    objects: PathBuf,
    fetching: Mutex<()>,
}

impl BlobSource for Remote<'_> {
    fn fetch(&self, oid: GitOid) -> std::io::Result<Option<Fetched>> {
        // Serialize cache fills: duplicate files may be materialised by different
        // workers, but each distinct blob crosses HTTP only once.
        let _fetching = self
            .fetching
            .lock()
            .map_err(|_| std::io::Error::other("blob cache lock"))?;
        let source = cuenca_fs::ObjectDir::new(&self.objects);
        if let Some(found) = source.fetch(oid)? {
            return Ok(Some(found));
        }
        let Some(bytes) = self
            .client
            .fork_blob(self.fork, oid)
            .map_err(std::io::Error::other)?
        else {
            return Ok(None);
        };
        let hex = oid.to_hex();
        let dir = self.objects.join(&hex[..2]);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join(&hex[2..]), bytes)?;
        // cuenca-fs hashes the cached file before accepting it into the checkout.
        source.fetch(oid)
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

/// Validate a host bootstrap destination before any remote fork creation.
/// Walk the original components (including those before `..`) with no-follow
/// descriptor operations. `components()` removes repeated/trailing slashes and `.`.
/// The returned physical path is used unchanged for registration and materialisation.
/// The caller exclusively owns the destination and its ancestors until return:
/// Git itself uses pathname-based APIs, so concurrent ancestor renames are unsupported.
pub fn prepare(dest: &Path) -> anyhow::Result<PathBuf> {
    ensure!(
        !dest.as_os_str().is_empty(),
        "worktree destination is empty"
    );
    let input = if dest.is_absolute() {
        dest.to_path_buf()
    } else {
        std::env::current_dir()?.join(dest)
    };
    let components: Vec<_> = input.components().collect();
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    let mut at = openat(CWD, "/", flags, rustix::fs::Mode::empty())?;
    let mut root = PathBuf::from("/");
    for (i, component) in components.iter().enumerate() {
        let name = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(name) => *name,
            Component::ParentDir => std::ffi::OsStr::new(".."),
            Component::Prefix(_) => anyhow::bail!("unsupported worktree path"),
        };
        match statat(&at, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) => {
                ensure!(
                    FileType::from_raw_mode(st.st_mode) != FileType::Symlink,
                    "worktree destination has a symlink component: {}",
                    dest.display()
                );
                ensure!(
                    FileType::from_raw_mode(st.st_mode) == FileType::Directory,
                    "worktree destination component is not a directory: {}",
                    dest.display()
                );
                at = openat(&at, name, flags, rustix::fs::Mode::empty()).with_context(|| {
                    format!(
                        "opening worktree component without following symlinks: {}",
                        dest.display()
                    )
                })?;
            }
            Err(Errno::NOENT) if i + 1 == components.len() => {}
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("checking worktree component: {}", dest.display()));
            }
        }
        if *component == Component::ParentDir {
            root.pop();
        } else {
            root.push(name);
        }
    }
    ensure!(
        root.parent().is_some(),
        "worktree destination must have a parent"
    );
    if root.exists() {
        ensure!(
            std::fs::read_dir(&root)?.next().is_none(),
            "worktree destination must be missing or empty: {}",
            root.display()
        );
        // Git removes an empty destination after a failed add. Fail before invoking
        // it for a read-only directory, even when the process could override mode bits.
        use std::os::unix::fs::PermissionsExt as _;
        ensure!(
            std::fs::metadata(&root)?.permissions().mode() & 0o222 != 0,
            "worktree destination is not writable: {}",
            root.display()
        );
        let _probe =
            tempfile::tempfile_in(&root).context("worktree destination is not writable")?;
    }
    Local::prepare_root(root)
}

/// Materialise a fork as a detached git worktree, with exact Cuenca bytes and index.
/// Destination and ancestors must be exclusively owned until return; see [`prepare`].
/// Failures preserve destination files; no force removal or reset is performed.
pub fn checkout(client: &Client, fork: &str, dest: &Path) -> anyhow::Result<Report> {
    let name = ForkName::new(fork)?;
    let root = prepare(dest)?;
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
    let objects = stage.path().join("objects");
    cuenca_fs::checkout(
        &tree,
        &files,
        &Remote {
            client,
            fork: &name,
            objects: objects.clone(),
            fetching: Mutex::new(()),
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
    // Reuse the object files verified by the materialiser; do not fetch them again.
    let mut seen = HashSet::new();
    for (_, entry) in tree.entries() {
        if !seen.insert(entry.oid()) {
            continue;
        }
        let hex = entry.oid().to_hex();
        let object = objects.join(&hex[..2]).join(&hex[2..]);
        let oid = git(
            repo,
            &[
                "hash-object".as_ref(),
                "-w".as_ref(),
                "--no-filters".as_ref(),
                object.as_os_str(),
            ],
            b"",
        )?;
        ensure!(oid == entry.oid().to_hex(), "fork blob hash mismatch");
    }
    let oid = Repo::open(repo)
        .with_isolated_config()
        .write_tree(&tree, &Blobs::new())?;
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
    git(&root, &["update-index".as_ref(), "--refresh".as_ref()], b"")?;
    let local = Local::create(State {
        format: 1,
        root: root.clone(),
        root_id: crate::local::identity(&root)?,
        url: client.url().to_string(),
        fork: name.clone(),
        location: f.location.clone(),
        epoch: f.epoch(),
        published: tree.clone(),
    })?;
    let mut cache = local.load_cache()?;
    let seen = crate::verbs::look(&root, &tree, &mut cache)?;
    local.save_cache(&cache)?;
    let ignored_note = crate::verbs::ignored_note(&seen.ignored);
    Ok(Report {
        json: json!({ "fork": name, "location": f.location, "epoch": f.epoch(), "head": head,
            "root": root, "git_dir": repo, "commit": commit, "files": tree.len(),
            "clean": seen.changes.is_empty(), "ignored": seen.ignored }),
        text: format!(
            "worktree of {name} (head {head}) into {}\n{ignored_note}",
            root.display()
        ),
        exit: 0,
    })
}
