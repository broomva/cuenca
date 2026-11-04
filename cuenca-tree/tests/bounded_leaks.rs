//! A stalled call leaks nothing, however many times it recurs (ADR 0002 requirement 2):
//! 50 wrappers whose grandchild holds stdout, each killed at its deadline, leave no
//! process, thread or file descriptor behind. Alone in its binary, so no other test's
//! threads or descriptors are counted.

use std::process::Command;
use std::time::{Duration, Instant};

use cuenca_tree::bounded::{Limits, RunError, run};

fn fds() -> usize {
    std::fs::read_dir("/dev/fd").unwrap().count()
}

#[cfg(target_os = "linux")]
fn threads() -> usize {
    std::fs::read_dir("/proc/self/task").unwrap().count()
}

/// macOS: `ps -M` prints a header and one line per thread of the process.
#[cfg(not(target_os = "linux"))]
fn threads() -> usize {
    let out = Command::new("ps")
        .args(["-M", "-p", &std::process::id().to_string()])
        .output()
        .unwrap();
    let lines = String::from_utf8_lossy(&out.stdout).lines().count();
    assert!(lines > 1, "ps -M listed no threads");
    lines - 1
}

/// Processes whose command line carries `marker`.
fn holders(marker: &str) -> usize {
    let out = Command::new("ps")
        .args(["-A", "-o", "stat=,args="])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.contains(marker) && !l.trim_start().starts_with('Z'))
        .count()
}

#[test]
fn fifty_stalls_leave_no_process_thread_or_fd() {
    // A sleep length no other process on the host uses, to count only these holders.
    let marker = format!("sleep 61{}", std::process::id() % 1000);
    let script = format!("{marker} & {marker}");
    let once = || {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", &script]);
        let start = Instant::now();
        let e = run(
            cmd,
            b"",
            "git fetch",
            Limits::within(Duration::from_millis(200), 1 << 20),
        )
        .unwrap_err();
        assert!(matches!(e, RunError::TimedOut { .. }), "{e}");
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "{:?}",
            start.elapsed()
        );
    };
    once();
    let (fds0, threads0) = (fds(), threads());
    for _ in 0..50 {
        once();
    }
    assert_eq!(fds(), fds0, "file descriptors grew");
    assert_eq!(threads(), threads0, "threads grew");
    let start = Instant::now();
    while holders(&marker) > 0 && start.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(holders(&marker), 0, "a grandchild outlived its call");
}
