//! Running one declared check ([`cuenca_core::checks::Check`]) for `cuenca verify`, on
//! the caller's machine, in a checkout of the candidate.
//!
//! The check runs from the checkout's root with the caller's environment, standard
//! input closed, and standard output and error together in a temporary file (so a
//! chatty check cannot stall on a full pipe). It runs in its own process group: at its
//! time limit the whole group is killed, and so is anything it left running after it
//! exited. This is no sandbox (spec rev 2.2 section 6: phase 1 adds that); the check
//! runs as the caller.

use std::io::{Read as _, Seek as _, SeekFrom};
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use cuenca_core::checks::{Check, CheckRun, CheckStatus};

/// How much of a check's output a report keeps: the end of it.
pub const OUTPUT_TAIL_BYTES: u64 = 4096;

/// A check as it ran, and the end of what it printed.
#[derive(Debug, Clone)]
pub struct Ran {
    pub run: CheckRun,
    pub output: String,
}

/// Run `check` from `root`.
pub fn check(check: &Check, root: &Path) -> Ran {
    let start = Instant::now();
    let ended = |status, exit_code, output: String| Ran {
        run: CheckRun {
            check: check.clone(),
            status,
            exit_code,
            duration_ms: start.elapsed().as_millis() as u64,
        },
        output,
    };
    let mut log = match tempfile::tempfile() {
        Ok(f) => f,
        Err(e) => return ended(CheckStatus::NotRun, None, format!("output file: {e}")),
    };
    let (out, err) = match (log.try_clone(), log.try_clone()) {
        (Ok(o), Ok(e)) => (o, e),
        (Err(e), _) | (_, Err(e)) => {
            return ended(CheckStatus::NotRun, None, format!("output file: {e}"));
        }
    };
    // A relative program with a slash names a file in the checkout.
    let program = &check.argv[0];
    let program = if program.contains('/') && !program.starts_with('/') {
        root.join(program)
    } else {
        program.into()
    };
    let spawned = Command::new(program)
        .args(&check.argv[1..])
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        .process_group(0)
        .spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => return ended(CheckStatus::NotRun, None, format!("could not start: {e}")),
    };
    let group = rustix::process::Pid::from_child(&child);
    let deadline = start + Duration::from_secs(check.timeout_secs);
    let mut pause = Duration::from_millis(5);
    let exit = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() >= deadline => break None,
            Ok(None) => {
                std::thread::sleep(pause);
                pause = (pause * 2).min(Duration::from_millis(100));
            }
            Err(_) => break None,
        }
    };
    // Whatever the check left running goes with it.
    let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
    let exit = match exit {
        Some(s) => Some(s),
        None => {
            let _ = child.wait();
            None
        }
    };
    let output = tail(&mut log);
    match exit {
        None => ended(CheckStatus::TimedOut, None, output),
        Some(s) => match s.code() {
            Some(0) => ended(CheckStatus::Passed, Some(0), output),
            code => ended(CheckStatus::Failed, code, output),
        },
    }
}

fn tail(file: &mut std::fs::File) -> String {
    let len = file.seek(SeekFrom::End(0)).unwrap_or(0);
    let from = len.saturating_sub(OUTPUT_TAIL_BYTES);
    let mut bytes = Vec::new();
    if file.seek(SeekFrom::Start(from)).is_ok() {
        let _ = file.read_to_end(&mut bytes);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str, timeout_secs: u64) -> Check {
        Check {
            name: "t".into(),
            argv: vec!["sh".into(), "-c".into(), script.into()],
            timeout_secs,
        }
    }

    #[test]
    fn a_check_ends_as_its_exit_says() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("here"), "x").unwrap();
        let r = check(
            &sh("test -f here && echo out && echo err >&2", 5),
            dir.path(),
        );
        assert_eq!(
            (r.run.status, r.run.exit_code),
            (CheckStatus::Passed, Some(0))
        );
        assert_eq!(r.output, "out\nerr\n");
        let r = check(&sh("exit 3", 5), dir.path());
        assert_eq!(
            (r.run.status, r.run.exit_code),
            (CheckStatus::Failed, Some(3))
        );
        let r = check(&sh("kill -9 $$", 5), dir.path());
        assert_eq!((r.run.status, r.run.exit_code), (CheckStatus::Failed, None));
        let missing = Check {
            name: "t".into(),
            argv: vec!["./no-such-script".into()],
            timeout_secs: 5,
        };
        let r = check(&missing, dir.path());
        assert_eq!((r.run.status, r.run.exit_code), (CheckStatus::NotRun, None));
    }

    #[test]
    fn a_check_past_its_limit_is_killed_with_what_it_started() {
        let dir = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let r = check(&sh("sleep 30 & echo $! > pid; sleep 30", 1), dir.path());
        assert_eq!(
            (r.run.status, r.run.exit_code),
            (CheckStatus::TimedOut, None)
        );
        assert!(started.elapsed() < Duration::from_secs(10));
        let pid: i32 = std::fs::read_to_string(dir.path().join("pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // Killed, it is reaped by init soon after; until then `kill -0` still finds it.
        let gone = (0..100).any(|_| {
            let alive = Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success();
            std::thread::sleep(Duration::from_millis(20));
            !alive
        });
        assert!(gone, "the background sleep {pid} survived");
    }

    #[test]
    fn only_the_end_of_the_output_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let r = check(&sh("yes x | head -c 100000; echo END", 5), dir.path());
        assert_eq!(r.output.len() as u64, OUTPUT_TAIL_BYTES);
        assert!(r.output.ends_with("END\n"));
    }
}
