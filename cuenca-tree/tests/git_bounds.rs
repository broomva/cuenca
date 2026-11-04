//! The git bridge within a deadline and output caps (ADR 0002): one deadline spans every
//! call of an operation, the `cat-file --batch` protocol is bounded field by field, and
//! a listing past its cap fails as a cap, not a timeout, until the cap is raised.
//!
//! A git wrapper stands in for a misbehaving git: it matches one kind of call and runs
//! the real git for every other.

mod common;

use common::{Kind, Work};
use cuenca_tree::GitOid;
use cuenca_tree::git::{GitError, Repo, RepoCaps};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// A `git` wrapper in `dir` that runs `action` for a call whose arguments include
/// `pattern` (a shell `case` pattern over ` $* `), and the real git otherwise.
fn wrapper(dir: &Path, pattern: &str, action: &str) -> PathBuf {
    let path = dir.join("git-wrapper");
    std::fs::write(
        &path,
        format!("#!/bin/sh\ncase \" $* \" in {pattern}) {action} ;; esac\nexec git \"$@\"\n"),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    path
}

/// A repository with a few files, committed.
fn work() -> (Work, GitOid) {
    let mut w = Work::new();
    w.write("a.txt", Kind::File, b"a\n");
    w.write("dir/b.txt", Kind::File, b"b\n");
    w.write(".gitattributes", Kind::File, b"*.txt text\n");
    let head = w.snapshot();
    (w, head)
}

fn blob_ids(w: &Work) -> Vec<GitOid> {
    w.model_tree().entries().map(|(_, e)| e.oid()).collect()
}

/// Each call sleeps 1.5 s, under a 2 s deadline for the whole read: per-call timeouts
/// would let the read run for 1.5 s times its number of calls; one deadline ends it at
/// 2 s.
#[test]
fn one_deadline_spans_every_call_of_a_read() {
    let (w, head) = work();
    let tmp = tempfile::tempdir().unwrap();
    let slow = wrapper(tmp.path(), "*", "sleep 1.5");
    let deadline = Duration::from_secs(2);
    let start = Instant::now();
    let e = Repo::open(w.path())
        .with_git(&slow)
        .with_deadline(start + deadline)
        .read_tree(&head.to_hex())
        .unwrap_err();
    let took = start.elapsed();
    assert!(matches!(e, GitError::TimedOut { .. }), "{e}");
    assert!(took < deadline + Duration::from_secs(1), "took {took:?}");
    // Without the wrapper the same read fits easily.
    Repo::open(w.path())
        .with_deadline(Instant::now() + Duration::from_secs(60))
        .read_tree(&head.to_hex())
        .unwrap();
}

/// A `cat-file --batch` that reads none of its stdin and prints nothing: the request
/// (far over a pipe's buffer) cannot be written, and the call still ends at the
/// deadline.
#[test]
fn a_batch_reader_with_blocked_stdin_ends_at_the_deadline() {
    let mut w = Work::new();
    for i in 0..3000 {
        w.write(&format!("f{i}"), Kind::File, format!("{i}\n").as_bytes());
    }
    w.snapshot();
    let ids = blob_ids(&w);
    let tmp = tempfile::tempdir().unwrap();
    let stuck = wrapper(tmp.path(), "*' --batch '*", "exec sleep 30");
    let deadline = Duration::from_secs(2);
    let start = Instant::now();
    let e = Repo::open(w.path())
        .with_git(&stuck)
        .with_deadline(start + deadline)
        .read_blobs(ids)
        .unwrap_err();
    assert!(matches!(e, GitError::TimedOut { .. }), "{e}");
    assert!(start.elapsed() < deadline + Duration::from_secs(1));
}

/// A header longer than the limit is refused as soon as the limit is passed, not at
/// the deadline.
#[test]
fn an_overlong_batch_header_is_refused() {
    let (w, _) = work();
    let tmp = tempfile::tempdir().unwrap();
    let long = wrapper(
        tmp.path(),
        "*' --batch '*",
        "head -c 300 /dev/zero | tr '\\0' a; exec sleep 30",
    );
    let start = Instant::now();
    let e = Repo::open(w.path())
        .with_git(&long)
        .with_deadline(start + Duration::from_secs(30))
        .read_blobs(blob_ids(&w))
        .unwrap_err();
    assert!(matches!(e, GitError::BadHeader), "{e}");
    assert!(start.elapsed() < Duration::from_secs(10));
}

/// A header the output ends in the middle of is refused.
#[test]
fn an_unterminated_batch_header_is_refused() {
    let (w, _) = work();
    let tmp = tempfile::tempdir().unwrap();
    let cut = wrapper(tmp.path(), "*' --batch '*", "printf abc; exit 0");
    let e = Repo::open(w.path())
        .with_git(&cut)
        .with_deadline(Instant::now() + Duration::from_secs(30))
        .read_blobs(blob_ids(&w))
        .unwrap_err();
    assert!(matches!(e, GitError::BadHeader), "{e}");
}

/// A body declared at another size than `--batch-check` reported is refused before it
/// is read.
#[test]
fn a_batch_size_other_than_batch_checks_is_refused() {
    let (w, _) = work();
    let tmp = tempfile::tempdir().unwrap();
    let lying = wrapper(
        tmp.path(),
        "*' --batch '*",
        "read oid; printf '%s blob 999\\n' \"$oid\"; exec sleep 30",
    );
    let start = Instant::now();
    let e = Repo::open(w.path())
        .with_git(&lying)
        .with_deadline(start + Duration::from_secs(30))
        .read_blobs(blob_ids(&w))
        .unwrap_err();
    assert!(
        matches!(e, GitError::SizeMismatch { declared: 999, .. }),
        "{e}"
    );
    assert!(start.elapsed() < Duration::from_secs(10));
}

/// A tree listing past the `ls-tree` cap fails as a cap, naming the field, however
/// long the deadline; once the cap is raised the same tree reads.
#[test]
fn a_listing_past_its_cap_fails_as_a_cap_until_raised() {
    let (w, head) = work();
    let tight = RepoCaps {
        ls_tree: 64,
        ..RepoCaps::default()
    };
    let repo = Repo::open(w.path()).with_deadline(Instant::now() + Duration::from_secs(600));
    let e = repo
        .clone()
        .with_caps(tight)
        .read_tree(&head.to_hex())
        .unwrap_err();
    match &e {
        GitError::Capped {
            field: "ls_tree",
            cap: 64,
            reached,
            ..
        } => assert!(*reached > 64),
        e => panic!("{e}"),
    }
    assert!(e.to_string().contains("(ls_tree)"), "{e}");
    repo.with_caps(RepoCaps::default())
        .read_tree(&head.to_hex())
        .unwrap();
}

/// `--batch-check` past its cap fails naming its field.
#[test]
fn a_batch_check_past_its_cap_fails_as_a_cap() {
    let (w, _) = work();
    let e = Repo::open(w.path())
        .with_caps(RepoCaps {
            batch_check: 50,
            ..RepoCaps::default()
        })
        .blob_sizes(&blob_ids(&w))
        .unwrap_err();
    assert!(
        matches!(
            e,
            GitError::Capped {
                field: "batch_check",
                ..
            }
        ),
        "{e}"
    );
}

/// `--batch-check` gives each blob's size; a missing object is an error.
#[test]
fn blob_sizes_are_batch_checks() {
    let (w, _) = work();
    let repo = Repo::open(w.path());
    let a = GitOid::for_blob(b"a\n");
    assert_eq!(repo.blob_sizes(&[a]).unwrap()[&a], 2);
    let absent = GitOid::for_blob(b"not in the repository");
    assert!(matches!(
        repo.blob_sizes(&[absent]),
        Err(GitError::Missing(o)) if o == absent
    ));
}

/// `read_tree_known` reads no blob the caller says it knows, so a tree whose plain
/// blob is missing from the repository still reads; a blob a content rule applies to
/// is read and checked whatever the caller says.
#[test]
fn read_tree_known_skips_only_unruled_blobs() {
    let w = Work::new();
    let missing = GitOid::for_blob(b"never written");
    let attrs = GitOid::for_blob(b"also never written\n");
    let mktree = |entries: &str| {
        let out = std::process::Command::new("git")
            .args(["-C", &w.path().display().to_string(), "mktree", "--missing"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut c| {
                use std::io::Write;
                c.stdin.take().unwrap().write_all(entries.as_bytes())?;
                c.wait_with_output()
            })
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    };
    let plain = mktree(&format!("100644 blob {missing}\tplain\n"));
    let repo = Repo::open(w.path());
    assert!(matches!(
        repo.read_tree(&plain),
        Err(GitError::Missing(o)) if o == missing
    ));
    let tree = repo.read_tree_known(&plain, |_| true).unwrap();
    assert_eq!(tree.get("plain").unwrap().oid(), missing);

    let ruled = mktree(&format!("100644 blob {attrs}\t.gitattributes\n"));
    assert!(matches!(
        repo.read_tree_known(&ruled, |_| true),
        Err(GitError::Missing(o)) if o == attrs
    ));
}
