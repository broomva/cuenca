//! Running a git (or gh) child within a deadline and bounded output (ADR 0002, BRO-3599).
//!
//! Every child is spawned in its own process group (`process_group(0)`), so the whole
//! group, not only the direct child, is killed (`killpg`, SIGKILL) when the call ends:
//! at its deadline, when its stdout passes its cap, or once the direct child has exited
//! and the pipes have closed. A helper, transport or hook wrapper that the direct child
//! left behind holding a pipe is read from until the pipes close or the deadline
//! passes, whichever is first; then the group is killed and the call ends with the
//! direct child's status and what was printed (ADR 0002). No such process outlives the
//! call.
//!
//! The pipes are driven by the calling thread alone with `poll(2)` on non-blocking file
//! descriptors: stdin is written, stdout is gathered up to its cap, and stderr keeps only
//! its last [`STDERR_TAIL`] bytes. No thread is spawned, so a stall leaves no thread
//! behind, and the descriptors are closed when the call returns, however it ends.
//!
//! The direct child is checked for exit without being reaped (`waitid` with `WNOWAIT`),
//! and the group is killed before the child is reaped, so the group id cannot have been
//! reused by an unrelated process when the kill is sent.
//!
//! Limit: a descendant that calls `setsid` (an ssh `ControlMaster`, a daemon) leaves the
//! group and is not killed. git's own helpers do not. If such a process holds a pipe,
//! the call still ends at its deadline, as a timeout.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags};
use rustix::process::{Pid, Signal, WaitId, WaitIdOptions};

/// How much of a child's stderr is kept: its last 64 KiB.
pub const STDERR_TAIL: usize = 64 * 1024;
/// How much of the start of a child's stderr is kept too, for a caller that classifies
/// a failure by a message git prints before more output: its first 16 KiB.
pub const STDERR_HEAD: usize = 16 * 1024;

/// The longest one `poll` waits before the direct child's exit and the deadline are
/// checked again.
const SLICE: Duration = Duration::from_millis(20);

/// How long the pipes are still read after the group is killed at the deadline, for
/// what its members printed before they died.
const GRACE: Duration = Duration::from_millis(200);

/// What one call may take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// When the call is killed if it has not ended; `None` waits as long as it runs.
    pub deadline: Option<Instant>,
    /// The most stdout bytes the call may print; past it the call is killed.
    pub stdout_cap: u64,
}

impl Limits {
    /// A call that must end within `timeout` from now, printing at most `stdout_cap`.
    pub fn within(timeout: Duration, stdout_cap: u64) -> Self {
        Self {
            deadline: Some(Instant::now() + timeout),
            stdout_cap,
        }
    }
}

/// Why a call has no output.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    /// The child could not be started (no such binary, say).
    #[error("running {what}: {source}")]
    Spawn { what: String, source: io::Error },
    /// The deadline passed first; the group was killed.
    #[error("{what} did not end within its deadline and was killed, after {elapsed:?}")]
    TimedOut { what: String, elapsed: Duration },
    /// stdout passed its cap; the group was killed.
    #[error("{what} printed {reached} bytes, over its {cap}-byte output cap, and was killed")]
    Capped {
        what: String,
        cap: u64,
        reached: u64,
    },
    /// The pipes or the wait failed.
    #[error("waiting for {what}: {source}")]
    Io { what: String, source: io::Error },
}

/// What a call that ended printed.
#[derive(Debug)]
pub struct Output {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    /// The last [`STDERR_TAIL`] bytes of stderr.
    pub stderr: Vec<u8>,
    /// The first [`STDERR_HEAD`] bytes of stderr.
    pub stderr_head: Vec<u8>,
    /// Why not all of the input was written, if it was not (the child closed its stdin,
    /// say). The child's own failure usually explains it, so it is reported only when
    /// the child succeeded.
    pub stdin_error: Option<io::Error>,
}

/// Run `cmd` with `input` on its stdin, within `limits`. `what` names the call in errors.
pub fn run(cmd: Command, input: &[u8], what: &str, limits: Limits) -> Result<Output, RunError> {
    let mut child = Running::spawn(cmd, what, limits)?;
    child.input = input.to_vec();
    child.finish()
}

/// A child in its own process group, with its pipes driven by `poll`.
pub struct Running {
    what: String,
    started: Instant,
    limits: Limits,
    child: Child,
    pid: Pid,
    stdin: Option<ChildStdin>,
    input: Vec<u8>,
    written: usize,
    stdin_error: Option<io::Error>,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
    out: Vec<u8>,
    err: VecDeque<u8>,
    err_head: Vec<u8>,
    /// Set once the group has been killed and the child reaped.
    status: Option<ExitStatus>,
}

impl Running {
    /// Start `cmd` in a new process group, with all three pipes non-blocking.
    pub fn spawn(mut cmd: Command, what: &str, limits: Limits) -> Result<Self, RunError> {
        let what = what.to_string();
        let child = cmd
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| RunError::Spawn {
                what: what.clone(),
                source,
            })?;
        let mut running = Self {
            what,
            started: Instant::now(),
            limits,
            pid: Pid::from_child(&child),
            child,
            stdin: None,
            input: Vec::new(),
            written: 0,
            stdin_error: None,
            stdout: None,
            stderr: None,
            out: Vec::new(),
            err: VecDeque::new(),
            err_head: Vec::new(),
            status: None,
        };
        let (stdin, stdout, stderr) = (
            running.child.stdin.take(),
            running.child.stdout.take(),
            running.child.stderr.take(),
        );
        let nonblocking = [
            stdin.as_ref().map(|p| p.as_fd()),
            stdout.as_ref().map(|p| p.as_fd()),
            stderr.as_ref().map(|p| p.as_fd()),
        ]
        .into_iter()
        .flatten()
        .try_for_each(|fd| rustix::io::ioctl_fionbio(fd, true));
        (running.stdin, running.stdout, running.stderr) = (stdin, stdout, stderr);
        if let Err(e) = nonblocking {
            return Err(running.fail_io(e.into()));
        }
        Ok(running)
    }

    /// Drive the pipes until stdout and stderr are closed, the input is written (or
    /// refused) and the direct child has exited; then kill what is left of its group.
    /// If the direct child has exited but a descendant still holds a pipe at the
    /// deadline, the group is killed then and the call ends with the direct child's
    /// status; if the direct child itself has not exited by then, the call times out.
    pub fn finish(mut self) -> Result<Output, RunError> {
        loop {
            let open = self.stdout.is_some() || self.stderr.is_some();
            let exited = self.exited()?;
            if !open && exited {
                break;
            }
            if self.past_deadline() {
                if !exited {
                    let _ = self.reap();
                    return Err(self.timed_out());
                }
                // The direct child is done and a descendant holds a pipe: the group goes
                // now, and what it printed before dying is still read.
                self.kill_group();
                let grace = Instant::now() + GRACE;
                while (self.stdout.is_some() || self.stderr.is_some()) && Instant::now() < grace {
                    if self.pump(true, grace.saturating_duration_since(Instant::now()))? {
                        self.read_stdout()?;
                    }
                }
                break;
            }
            let wait = self.remaining().map_or(SLICE, |left| left.min(SLICE));
            if open || self.stdin.is_some() {
                if self.pump(true, wait)? {
                    self.read_stdout()?;
                }
            } else {
                std::thread::sleep(wait);
            }
        }
        let status = self.reap()?;
        if self.stdin_error.is_none() && self.written < self.input.len() {
            self.stdin_error = Some(io::Error::new(
                io::ErrorKind::BrokenPipe,
                format!(
                    "{} ended having read {} of {} input bytes",
                    self.what,
                    self.written,
                    self.input.len()
                ),
            ));
        }
        Ok(Output {
            status,
            stdout: std::mem::take(&mut self.out),
            stderr: self.err.drain(..).collect(),
            stderr_head: std::mem::take(&mut self.err_head),
            stdin_error: self.stdin_error.take(),
        })
    }

    fn past_deadline(&self) -> bool {
        self.remaining().is_some_and(|left| left.is_zero())
    }

    fn timed_out(&self) -> RunError {
        RunError::TimedOut {
            what: self.what.clone(),
            elapsed: self.started.elapsed(),
        }
    }

    fn remaining(&self) -> Option<Duration> {
        self.limits
            .deadline
            .map(|d| d.saturating_duration_since(Instant::now()))
    }

    /// Whether the direct child has exited, without reaping it: its pid, and so its
    /// group id, stays reserved until [`Running::reap`].
    fn exited(&mut self) -> Result<bool, RunError> {
        if self.status.is_some() {
            return Ok(true);
        }
        loop {
            match rustix::process::waitid(
                WaitId::Pid(self.pid),
                WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
            ) {
                Ok(found) => return Ok(found.is_some()),
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => return Err(self.fail_io(e.into())),
            }
        }
    }

    /// SIGKILL to the whole group. The direct child is not yet reaped, so the group id is
    /// still this group's.
    fn kill_group(&mut self) {
        if self.status.is_none() {
            let _ = rustix::process::kill_process_group(self.pid, Signal::KILL);
        }
    }

    /// Kill the group, reap the direct child and close every pipe.
    fn reap(&mut self) -> Result<ExitStatus, RunError> {
        if let Some(status) = self.status {
            return Ok(status);
        }
        self.kill_group();
        self.stdin = None;
        self.stdout = None;
        self.stderr = None;
        let status = loop {
            match self.child.wait() {
                Ok(status) => break status,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(source) => {
                    return Err(RunError::Io {
                        what: self.what.clone(),
                        source,
                    });
                }
            }
        };
        self.status = Some(status);
        Ok(status)
    }

    fn fail_io(&mut self, source: io::Error) -> RunError {
        let _ = self.reap();
        RunError::Io {
            what: self.what.clone(),
            source,
        }
    }

    /// Wait up to `wait` (one slice at most) for any pipe to be ready, then write what
    /// stdin takes and read what stderr has: whether stdout (watched when
    /// `watch_stdout`) is ready to read.
    fn pump(&mut self, watch_stdout: bool, wait: Duration) -> Result<bool, RunError> {
        if self.stdin.is_some() && self.written == self.input.len() {
            // All written: closing stdin is the child's end of input.
            self.stdin = None;
        }
        let wait = wait.min(SLICE);
        let timeout = rustix::event::Timespec {
            tv_sec: 0,
            tv_nsec: wait.subsec_nanos().into(),
        };
        let (ready_in, ready_out, ready_err) = {
            let mut fds = Vec::with_capacity(3);
            let mut slots = [None; 3];
            if let Some(p) = &self.stdin {
                slots[0] = Some(fds.len());
                fds.push(PollFd::new(p, PollFlags::OUT));
            }
            if watch_stdout && let Some(p) = &self.stdout {
                slots[1] = Some(fds.len());
                fds.push(PollFd::new(p, PollFlags::IN));
            }
            if let Some(p) = &self.stderr {
                slots[2] = Some(fds.len());
                fds.push(PollFd::new(p, PollFlags::IN));
            }
            match rustix::event::poll(&mut fds, Some(&timeout)) {
                Ok(_) | Err(rustix::io::Errno::INTR) => {}
                Err(e) => {
                    drop(fds);
                    return Err(self.fail_io(e.into()));
                }
            }
            let ready = |slot: Option<usize>| slot.is_some_and(|i| !fds[i].revents().is_empty());
            (ready(slots[0]), ready(slots[1]), ready(slots[2]))
        };
        if ready_in {
            self.write_stdin();
        }
        if ready_err {
            self.read_stderr()?;
        }
        Ok(ready_out)
    }

    fn write_stdin(&mut self) {
        let Some(pipe) = &mut self.stdin else { return };
        loop {
            match pipe.write(&self.input[self.written..]) {
                Ok(0) => break,
                Ok(n) => {
                    self.written += n;
                    if self.written == self.input.len() {
                        self.stdin = None;
                        break;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    // The child closed its stdin (a broken pipe): no more is written.
                    self.stdin_error = Some(e);
                    self.stdin = None;
                    break;
                }
            }
        }
    }

    /// One read of stderr: a child that writes it without end still has the deadline
    /// checked between reads.
    fn read_stderr(&mut self) -> Result<(), RunError> {
        let Some(pipe) = &mut self.stderr else {
            return Ok(());
        };
        let mut buf = [0u8; 16 * 1024];
        loop {
            match pipe.read(&mut buf) {
                Ok(0) => {
                    self.stderr = None;
                    return Ok(());
                }
                Ok(n) => {
                    let room = STDERR_HEAD.saturating_sub(self.err_head.len()).min(n);
                    self.err_head.extend_from_slice(&buf[..room]);
                    self.err.extend(&buf[..n]);
                    let over = self.err.len().saturating_sub(STDERR_TAIL);
                    self.err.drain(..over);
                    return Ok(());
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(self.fail_io(e)),
            }
        }
    }

    /// One read of stdout, for the same reason as [`Running::read_stderr`].
    fn read_stdout(&mut self) -> Result<(), RunError> {
        let Some(pipe) = &mut self.stdout else {
            return Ok(());
        };
        let mut buf = [0u8; 64 * 1024];
        loop {
            match pipe.read(&mut buf) {
                Ok(0) => {
                    self.stdout = None;
                    return Ok(());
                }
                Ok(n) => {
                    let reached = (self.out.len() + n) as u64;
                    if reached > self.limits.stdout_cap {
                        let _ = self.reap();
                        return Err(RunError::Capped {
                            what: self.what.clone(),
                            cap: self.limits.stdout_cap,
                            reached,
                        });
                    }
                    self.out.extend_from_slice(&buf[..n]);
                    return Ok(());
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(self.fail_io(e)),
            }
        }
    }
}

impl Drop for Running {
    /// However the call ends, its group is killed and the child reaped: no git child
    /// outlives the [`Running`] that started it.
    fn drop(&mut self) {
        let _ = self.reap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", script]);
        cmd
    }

    #[test]
    fn output_and_input_pass_through() {
        let input = vec![b'x'; 1 << 20];
        let out = run(
            sh("cat; echo done >&2"),
            &input,
            "cat",
            Limits::within(Duration::from_secs(30), 2 << 20),
        )
        .unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout, input);
        assert_eq!(out.stderr, b"done\n");
        assert!(out.stdin_error.is_none());
    }

    #[test]
    fn a_child_that_ignores_its_input_still_ends() {
        let out = run(
            sh("exit 3"),
            &vec![0u8; 1 << 20],
            "exit",
            Limits::within(Duration::from_secs(30), 16),
        )
        .unwrap();
        assert_eq!(out.status.code(), Some(3));
        assert!(out.stdin_error.is_some());
    }

    #[test]
    fn stdout_past_its_cap_is_killed() {
        let start = Instant::now();
        let e = run(
            sh("yes"),
            b"",
            "yes",
            Limits::within(Duration::from_secs(30), 100_000),
        )
        .unwrap_err();
        match e {
            RunError::Capped { cap, reached, .. } => {
                assert_eq!(cap, 100_000);
                assert!(reached > cap, "{reached}");
            }
            e => panic!("{e}"),
        }
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn stdout_at_its_cap_is_kept() {
        let out = run(
            sh("head -c 4096 /dev/zero"),
            b"",
            "head",
            Limits::within(Duration::from_secs(30), 4096),
        )
        .unwrap();
        assert_eq!(out.stdout.len(), 4096);
    }

    #[test]
    fn stderr_keeps_only_its_tail() {
        let out = run(
            sh("head -c 300000 /dev/zero >&2; printf end >&2"),
            b"",
            "noisy",
            Limits::within(Duration::from_secs(30), 16),
        )
        .unwrap();
        assert_eq!(out.stderr.len(), STDERR_TAIL);
        assert!(out.stderr.ends_with(b"\0end"));
    }

    #[test]
    fn stderr_keeps_its_head_too() {
        let out = run(
            sh("printf 'error: GH013: refused\\n' >&2; head -c 300000 /dev/zero >&2"),
            b"",
            "push",
            Limits::within(Duration::from_secs(30), 16),
        )
        .unwrap();
        assert!(out.stderr_head.starts_with(b"error: GH013: refused\n"));
        assert_eq!(out.stderr_head.len(), STDERR_HEAD);
        assert!(!out.stderr.windows(6).any(|w| w == b"GH013:"));
    }

    #[test]
    fn endless_stderr_still_ends_at_the_deadline() {
        let start = Instant::now();
        let e = run(
            sh("yes >&2"),
            b"",
            "noisy",
            Limits::within(Duration::from_secs(1), 16),
        )
        .unwrap_err();
        assert!(matches!(e, RunError::TimedOut { .. }), "{e}");
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "{:?}",
            start.elapsed()
        );
    }

    #[test]
    fn many_writers_flooding_stderr_still_end_at_the_deadline() {
        let start = Instant::now();
        let e = run(
            sh("yes >&2 & yes >&2 & yes >&2 & yes >&2 & wait"),
            b"",
            "noisy",
            Limits::within(Duration::from_millis(300), 16),
        )
        .unwrap_err();
        assert!(matches!(e, RunError::TimedOut { .. }), "{e}");
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "{:?}",
            start.elapsed()
        );
    }

    #[test]
    fn input_is_written_after_the_outputs_close() {
        let input = vec![b'x'; 1 << 20];
        let out = run(
            sh("exec 1>&- 2>&-; sleep 0.1; cat >/dev/null"),
            &input,
            "cat",
            Limits::within(Duration::from_secs(30), 16),
        )
        .unwrap();
        assert!(out.status.success());
        assert!(out.stdin_error.is_none(), "{:?}", out.stdin_error);
    }

    #[test]
    fn a_missing_binary_is_a_spawn_error() {
        let e = run(
            Command::new("/nonexistent/git"),
            b"",
            "git",
            Limits::within(Duration::from_secs(1), 16),
        )
        .unwrap_err();
        assert!(matches!(e, RunError::Spawn { .. }), "{e}");
    }
}
