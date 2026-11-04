//! Process-group termination (ADR 0002, BRO-3599): a git wrapper whose grandchild holds
//! its pipes ends within the deadline, and the grandchild is gone, whether the wrapper
//! hangs or exits normally.
//!
//! The leak check (no process, thread or fd left after 50 stalls) is in
//! `bounded_leaks.rs`, alone in its binary, so other tests' threads do not count.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use cuenca_tree::bounded::{Limits, RunError, run};

/// A shell script that starts a long-lived grandchild holding stdout and stderr, writes
/// its pid to `pidfile`, then runs `then`.
fn wrapper(pidfile: &Path, then: &str) -> Command {
    let mut cmd = Command::new("sh");
    cmd.args([
        "-c",
        &format!("sleep 60 & echo $! > '{}'; {then}", pidfile.display()),
    ]);
    cmd
}

/// Whether `pid` is a live process (a zombie is not).
fn alive(pid: &str) -> bool {
    let out = Command::new("ps")
        .args(["-o", "stat=", "-p", pid])
        .output()
        .unwrap();
    let stat = String::from_utf8_lossy(&out.stdout);
    let stat = stat.trim();
    !stat.is_empty() && !stat.starts_with('Z')
}

/// Wait up to 5 s for `pid` to be gone (reaping by init after a reparent is not
/// instant); whether it is.
fn gone(pid: &str) -> bool {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if !alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn a_hung_wrapper_and_its_grandchild_are_killed_at_the_deadline() {
    let tmp = tempfile::tempdir().unwrap();
    let pidfile = tmp.path().join("pid");
    let timeout = Duration::from_secs(1);
    let start = Instant::now();
    let e = run(
        wrapper(&pidfile, "sleep 30"),
        b"",
        "git fetch",
        Limits::within(timeout, 1 << 20),
    )
    .unwrap_err();
    let took = start.elapsed();
    assert!(matches!(e, RunError::TimedOut { .. }), "{e}");
    assert!(took < timeout + Duration::from_secs(2), "took {took:?}");
    let pid = std::fs::read_to_string(&pidfile).unwrap();
    assert!(
        gone(pid.trim()),
        "the grandchild {} outlived the call",
        pid.trim()
    );
}

/// The direct child exits while its grandchild still holds the pipes: they are read
/// until the deadline, then the group is killed and the call ends with the direct
/// child's status and output (ADR 0002).
#[test]
fn a_normal_exit_with_an_inherited_pipe_ends_at_the_deadline_and_kills_the_holder() {
    let tmp = tempfile::tempdir().unwrap();
    let pidfile = tmp.path().join("pid");
    let timeout = Duration::from_secs(2);
    let start = Instant::now();
    let out = run(
        wrapper(&pidfile, "echo answered"),
        b"",
        "git ls-remote",
        Limits::within(timeout, 1 << 20),
    )
    .unwrap();
    let took = start.elapsed();
    assert!(took >= timeout - Duration::from_millis(100), "{took:?}");
    assert!(took < timeout + Duration::from_secs(2), "{took:?}");
    assert!(out.status.success());
    assert_eq!(out.stdout, b"answered\n");
    let pid = std::fs::read_to_string(&pidfile).unwrap();
    assert!(
        gone(pid.trim()),
        "the grandchild {} outlived the call",
        pid.trim()
    );
}

/// A descendant left behind without the pipes does not hold the call open, and is
/// killed with the group when the call ends.
#[test]
fn a_descendant_without_the_pipes_is_killed_when_the_call_ends() {
    let tmp = tempfile::tempdir().unwrap();
    let pidfile = tmp.path().join("pid");
    let mut cmd = Command::new("sh");
    cmd.args([
        "-c",
        &format!(
            "sleep 60 </dev/null >/dev/null 2>&1 & echo $! > '{}'; echo done",
            pidfile.display()
        ),
    ]);
    let start = Instant::now();
    let out = run(
        cmd,
        b"",
        "git gc",
        Limits::within(Duration::from_secs(20), 4096),
    )
    .unwrap();
    assert!(start.elapsed() < Duration::from_secs(10));
    assert_eq!(out.stdout, b"done\n");
    let pid = std::fs::read_to_string(&pidfile).unwrap();
    assert!(
        gone(pid.trim()),
        "the descendant {} outlived the call",
        pid.trim()
    );
}

/// What a descendant prints after the direct child exits, before the deadline, is
/// kept, and the call ends when the pipes close, not at the deadline.
#[test]
fn a_descendants_later_output_is_kept() {
    let mut cmd = Command::new("sh");
    cmd.args(["-c", "(sleep 0.2; printf completed-output) & exit 0"]);
    let start = Instant::now();
    let out = run(
        cmd,
        b"",
        "git fetch",
        Limits::within(Duration::from_secs(20), 4096),
    )
    .unwrap();
    assert!(out.status.success());
    assert_eq!(out.stdout, b"completed-output");
    assert!(start.elapsed() < Duration::from_secs(10));
}

#[test]
fn a_wrapper_printing_past_its_cap_is_killed_with_its_grandchild() {
    let tmp = tempfile::tempdir().unwrap();
    let pidfile = tmp.path().join("pid");
    let e = run(
        wrapper(&pidfile, "yes"),
        b"",
        "git log",
        Limits::within(Duration::from_secs(30), 1 << 16),
    )
    .unwrap_err();
    assert!(matches!(e, RunError::Capped { cap: 65536, .. }), "{e}");
    let pid = std::fs::read_to_string(&pidfile).unwrap();
    assert!(
        gone(pid.trim()),
        "the grandchild {} outlived the call",
        pid.trim()
    );
}

#[test]
fn a_child_not_reading_its_stdin_ends_at_the_deadline() {
    // stdin is never read and the input is far over a pipe's buffer: the write blocks
    // until the deadline, and the call still ends on time.
    let mut cmd = Command::new("sh");
    cmd.args(["-c", "sleep 30"]);
    let timeout = Duration::from_secs(1);
    let start = Instant::now();
    let e = run(
        cmd,
        &vec![0u8; 8 << 20],
        "git cat-file",
        Limits::within(timeout, 1 << 20),
    )
    .unwrap_err();
    assert!(matches!(e, RunError::TimedOut { .. }), "{e}");
    assert!(start.elapsed() < timeout + Duration::from_secs(2));
}
