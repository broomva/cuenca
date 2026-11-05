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
        format!(
            "#!/bin/sh\n[ -n \"$CUENCA_PROBE\" ] && exit 0\ncase \" $* \" in {pattern}) {action} ;; esac\nexec git \"$@\"\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    // A test thread that forked while the script was open for writing holds it until
    // its child execs, and exec then fails with ETXTBSY. Once one exec succeeds no
    // writer is left, so every later run of the script is safe.
    let start = std::time::Instant::now();
    loop {
        match std::process::Command::new(&path)
            .env("CUENCA_PROBE", "1")
            .status()
        {
            Err(e) if e.raw_os_error() == Some(26) && start.elapsed().as_secs() < 10 => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => panic!("cannot run the wrapper: {e}"),
            Ok(_) => break,
        }
    }
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

/// Every blob body a tree read streams is held to the per-blob cap, like
/// `read_blobs`, before any is read; a blob the caller knows is not read at all.
#[test]
fn a_tree_read_holds_each_blob_to_the_per_blob_cap() {
    let mut w = Work::new();
    w.write("big.txt", Kind::File, b"ten bytes!");
    let head = w.snapshot();
    let repo = Repo::open(w.path()).with_memory_limits(4, 1 << 20);
    for e in [
        repo.read_tree(&head.to_hex()).unwrap_err(),
        repo.read_tree_known(&head.to_hex(), |_| false).unwrap_err(),
    ] {
        assert!(
            matches!(
                e,
                GitError::TooLarge {
                    size: 10,
                    limit: 4,
                    ..
                }
            ),
            "{e}"
        );
    }
    repo.read_tree_known(&head.to_hex(), |_| true).unwrap();
}

/// Output past the last object, even bytes the reader already buffered with it, fails
/// the batch.
#[test]
fn bytes_after_the_last_batch_object_are_refused() {
    let (w, _) = work();
    let tmp = tempfile::tempdir().unwrap();
    let junk = wrapper(
        tmp.path(),
        "*' --batch '*",
        "git \"$@\"; s=$?; printf JUNK; exit $s",
    );
    let e = Repo::open(w.path())
        .with_git(&junk)
        .with_deadline(Instant::now() + Duration::from_secs(30))
        .read_blobs(blob_ids(&w))
        .unwrap_err();
    assert!(matches!(e, GitError::Parse(_)), "{e}");
}

/// A tree body cut short by a git that then fails reports git's failure, not a
/// substituted object.
#[test]
fn a_truncated_tree_body_reports_gits_failure() {
    let mut w = Work::new();
    w.write("a.txt", Kind::File, b"a\n");
    let head = w.snapshot();
    let tmp = tempfile::tempdir().unwrap();
    let cut = wrapper(
        tmp.path(),
        "*' --batch '*",
        "git \"$@\" | head -c 60; echo boom >&2; exit 1",
    );
    let e = Repo::open(w.path())
        .with_git(&cut)
        .with_deadline(Instant::now() + Duration::from_secs(30))
        .read_tree_known(&head.to_hex(), |_| true)
        .unwrap_err();
    match &e {
        GitError::Failed { stderr, .. } => assert!(stderr.contains("boom"), "{e}"),
        e => panic!("{e}"),
    }
}

/// The same, with the junk in the same write as the last object, so the reader has it
/// buffered when the last object ends.
#[test]
fn bytes_buffered_with_the_last_batch_object_are_refused() {
    let (w, _) = work();
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("out");
    let junk = wrapper(
        tmp.path(),
        "*' --batch '*",
        &format!(
            "git \"$@\" > '{o}'; s=$?; printf JUNK >> '{o}'; cat '{o}'; exit $s",
            o = out.display()
        ),
    );
    let e = Repo::open(w.path())
        .with_git(&junk)
        .with_deadline(Instant::now() + Duration::from_secs(30))
        .read_blobs(blob_ids(&w))
        .unwrap_err();
    assert!(matches!(e, GitError::Parse(_)), "{e}");
}

/// `write_tree` reads a ruled blob the repository already has and the caller did not
/// send; that read is held to the per-blob cap too.
#[test]
fn a_write_holds_an_unsent_ruled_blob_to_the_per_blob_cap() {
    let mut w = Work::new();
    w.write(
        ".gitattributes",
        Kind::File,
        b"*.txt text eol=lf\n# fifty bytes or so\n",
    );
    w.write("a.txt", Kind::File, b"a\n");
    w.snapshot();
    let tree = w.model_tree();
    let e = Repo::open(w.path())
        .with_memory_limits(10, 1 << 20)
        .write_tree(&tree, &Default::default())
        .unwrap_err();
    assert!(matches!(e, GitError::TooLarge { limit: 10, .. }), "{e}");
}

/// A batch reader whose git prints far more than its objects declare is stopped by
/// the batch's stdout cap as the output streams, naming the cap.
#[test]
fn streamed_output_past_the_batch_cap_fails_as_a_cap() {
    let (w, _) = work();
    let tmp = tempfile::tempdir().unwrap();
    let flood = wrapper(
        tmp.path(),
        "*' --batch '*",
        "head -c 10000 /dev/zero; exec sleep 30",
    );
    let a = GitOid::for_blob(b"a\n");
    let start = Instant::now();
    let e = Repo::open(w.path())
        .with_git(&flood)
        .with_deadline(start + Duration::from_secs(30))
        .read_blobs([a])
        .unwrap_err();
    assert!(matches!(e, GitError::Capped { field: "batch", .. }), "{e}");
    assert!(start.elapsed() < Duration::from_secs(10));
}

/// Once the stop flag is set, the bridge starts no further git call: a read fails with
/// `Stopped` at once instead of running a git that would hang.
#[test]
fn a_set_stop_flag_starts_no_git_call() {
    let (w, head) = work();
    let tmp = tempfile::tempdir().unwrap();
    let hang = wrapper(tmp.path(), "*", "exec sleep 30");
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let start = Instant::now();
    let e = Repo::open(w.path())
        .with_git(&hang)
        .with_deadline(start + Duration::from_secs(10))
        .with_stop(stop)
        .read_tree(&head.to_hex())
        .unwrap_err();
    assert!(matches!(e, GitError::Stopped), "{e}");
    assert!(start.elapsed() < Duration::from_secs(5));
}
