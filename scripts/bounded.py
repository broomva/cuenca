#!/usr/bin/env python3
"""Run a command with a wall-clock limit and a disk budget on its scratch directory.

    bounded.py --dir DIR --budget-mb N --timeout SECONDS -- COMMAND...

COMMAND runs in its own session with TMPDIR=DIR, so every tempfile it and its children
make lands in DIR. Once a second the bytes allocated under DIR are summed; when they pass
the budget, or the time limit passes, the whole process group is killed with SIGKILL.
A fast writer can pass the budget by up to one poll's worth of writes before it dies.
Exit status: the command's own, 128 + signal if it died of one, 124 on timeout, 125 on
an exceeded budget. The reason goes to stderr.

The command is outside our process group, so a Ctrl-C or a harness's kill would not reach
it: SIGINT, SIGTERM and SIGHUP to this process kill the command's group first, then exit
128 + signal. --pgid-file records the group while it runs, so a caller that outlives a
SIGKILL of this process can still kill it.

Why: a mutant that turns a bounded write loop into an unbounded one filled the Mac's
disk twice on 2026-10-24 (BRO-3455). A guard has to stop it, not the disk.
"""

import argparse
import os
import signal
import subprocess
import sys
import time

TIMEOUT, BUDGET = 124, 125


def allocated(root):
    """Bytes allocated under root (st_blocks, so sparse files count what they use)."""
    total = 0
    for dirpath, dirnames, filenames in os.walk(root, onerror=lambda _: None):
        for name in dirnames + filenames:
            try:
                total += os.lstat(os.path.join(dirpath, name)).st_blocks * 512
            except OSError:
                pass  # removed while we walked
    return total


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--dir", required=True)
    ap.add_argument("--budget-mb", type=int, required=True)
    ap.add_argument("--timeout", type=int, required=True)
    ap.add_argument("--poll", type=float, default=1.0)
    ap.add_argument("--pgid-file")
    ap.add_argument("command", nargs=argparse.REMAINDER)
    a = ap.parse_args()
    command = a.command[1:] if a.command[:1] == ["--"] else a.command
    if not command:
        ap.error("no command")
    os.makedirs(a.dir, exist_ok=True)
    running = []

    def stop(signum, _frame):
        for child in running:
            kill_group(child)
        sys.exit(128 + signum)

    for signum in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(signum, stop)
    child = subprocess.Popen(command, env={**os.environ, "TMPDIR": a.dir}, start_new_session=True)
    running.append(child)
    if a.pgid_file:
        with open(a.pgid_file, "w") as f:
            f.write(f"{child.pid}\n")
    try:
        return watch(a, child, command)
    finally:
        if a.pgid_file:
            os.unlink(a.pgid_file)


def kill_group(child):
    try:
        os.killpg(child.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    child.wait()


def watch(a, child, command):
    budget = a.budget_mb * 1024 * 1024
    start = time.monotonic()
    reason = None
    while reason is None:
        try:
            status = child.wait(timeout=a.poll)
            kill_group(child)  # whatever the command left running in its group
            return status if status >= 0 else 128 - status
        except subprocess.TimeoutExpired:
            pass
        if time.monotonic() - start > a.timeout:
            reason = (TIMEOUT, f"timed out after {a.timeout} s")
        else:
            used = allocated(a.dir)
            if used > budget:
                reason = (BUDGET, f"wrote {used // (1024 * 1024)} MiB under {a.dir}, budget {a.budget_mb} MiB")
    kill_group(child)
    print(f"bounded: killed {command[0]}: {reason[1]}", file=sys.stderr)
    return reason[0]


if __name__ == "__main__":
    sys.exit(main())
