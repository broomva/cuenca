# Sourced by every crate's scripts/mutation-check.sh, before anything makes a tempdir.
# Gives the run its own scratch directory, exported as TMPDIR, and runs each cargo test
# under scripts/bounded.py, so a mutant that loops or writes without end is killed by a
# timeout or a disk budget and not by a full disk (BRO-3455).
#
#   CUENCA_MUT_TMPDIR      parent of the run dirs (default /tmp/cuenca-mut). Keep it
#                          short: tests bind Unix sockets at $TMPDIR/.tmpXXXXXX/admin.sock,
#                          and macOS caps a socket path at 104 bytes, so a run dir longer
#                          than 64 bytes is refused. Point it at a roomy disk if /tmp is not.
#   CUENCA_MUT_BUDGET_MB   bytes one running cargo test may have under the run dir before
#                          it is killed (default 4096; leaving more on exit only warns)
#   CUENCA_MUT_TIMEOUT     seconds one cargo test may run, build included (default 1800)
#
# A run dir is named run-<pid> and holds .backup. When the next run starts, one whose
# process is gone (SIGKILLed, so its EXIT trap never ran) is removed, unless it may have
# left a mutant in the source (.backup/path), failed to copy Cargo.lock back
# (.backup/lock-failed) or left a test group still running (.backup/pgid): then the guard
# names each and refuses to start, deleting nothing.
#
# One sweep at a time per checkout: every sweep mutates the same source and Cargo.lock,
# whichever crate it is for and whatever CUENCA_MUT_TMPDIR it uses. The lock is a flock
# on fd 9, held by this shell's open file and so released by the kernel when the sweep
# exits, however it dies (BRO-3467 #7).
#
# Exit codes of a mutation-check.sh (BRO-3467 #8): 0 every mutant killed, 1 one survived
# or proved nothing, 2 the baseline is red, 3 a mutant run was killed by a signal, 4 the
# source or Cargo.lock could not be restored, 5 this guard refused to start, 6 a backup
# failed, 130 / 143 interrupted by SIGINT / SIGTERM.
guard_repo=$(git rev-parse --show-toplevel)
guard_lock=$(git rev-parse --absolute-git-dir)/cuenca-mutation.lock
exec 9>>"$guard_lock"
if ! python3 -c 'import fcntl; fcntl.flock(9, fcntl.LOCK_EX | fcntl.LOCK_NB)' 2>/dev/null; then
  # The file names the holder's shell pid and, once known, its pgid file. bounded.py
  # inherits fd 9, so a SIGKILLed sweep's test runner keeps the lock until its tests
  # exit; that is meant (they still build in the checkout), but then the pid is dead.
  read -r guard_holder guard_holder_pgid <"$guard_lock" 2>/dev/null
  if [ -n "$guard_holder" ] && ps -p "$guard_holder" >/dev/null 2>&1; then
    echo "mutation-guard: another mutation sweep runs in $guard_repo (pid $guard_holder); wait for it to finish" >&2
  else
    echo "mutation-guard: the sweep that locked $guard_repo (pid ${guard_holder:-unknown}) is gone, but an orphaned test runner still holds $guard_lock" >&2
    if [ -n "$guard_holder_pgid" ] && [ -s "$guard_holder_pgid" ]; then
      echo "  its tests run as process group $(cat "$guard_holder_pgid"): kill -KILL -- -$(cat "$guard_holder_pgid")" >&2
    else
      echo "  find it with: lsof $guard_lock" >&2
    fi
  fi
  exit 5
fi
echo $$ >"$guard_lock"
guard_root=${CUENCA_MUT_TMPDIR:-/tmp/cuenca-mut}
case $guard_root in
  /*) ;;
  *) echo "mutation-guard: CUENCA_MUT_TMPDIR must be absolute, not $guard_root" >&2; exit 5 ;;
esac
mkdir -p "$guard_root"
# Without ps and pgrep every old run would look dead.
command -v ps >/dev/null && command -v pgrep >/dev/null || { echo "mutation-guard: needs ps and pgrep" >&2; exit 5; }
for stale in "$guard_root"/run-*; do
  pid=${stale##*/run-}
  case $pid in '' | *[!0-9]*) continue ;; esac
  [ -d "$stale/.backup" ] || continue
  ps -p "$pid" >/dev/null 2>&1 && continue
  if [ -s "$stale/.backup/path" ] || [ -e "$stale/.backup/lock-failed" ] || { [ -s "$stale/.backup/pgid" ] && pgrep -g "$(cat "$stale/.backup/pgid")" >/dev/null; }; then
    echo "mutation-guard: run $pid died without cleaning up. Before running again:" >&2
    if [ -s "$stale/.backup/path" ]; then
      echo "  $(cat "$stale/.backup/path") may still be mutated: compare it with $stale/.backup/file and restore it (this overwrites edits made since)" >&2
      echo "  restore Cargo.lock from $stale/.backup/Cargo.lock or git" >&2
    elif [ -e "$stale/.backup/lock-failed" ]; then
      echo "  Cargo.lock was not restored: copy $stale/.backup/Cargo.lock back over it, or take it from git" >&2
    fi
    if [ -s "$stale/.backup/pgid" ] && pgrep -g "$(cat "$stale/.backup/pgid")" >/dev/null; then
      echo "  its tests still run as process group $(cat "$stale/.backup/pgid"): kill -KILL -- -$(cat "$stale/.backup/pgid")" >&2
    fi
    echo "  then remove $stale" >&2
    exit 5
  fi
  rm -rf "$stale"
done
guard_dir=$guard_root/run-$$
guard_len=$(printf %s "$guard_dir" | wc -c | tr -d ' ')
if [ "$guard_len" -gt 64 ]; then
  echo "mutation-guard: run dir $guard_dir is $guard_len bytes; the tests' sockets need it at most 64 (set CUENCA_MUT_TMPDIR shorter)" >&2
  exit 5
fi
# The script's own backups and logs sit in .backup, which bounded() keeps.
guard_backup=$guard_dir/.backup
mkdir -p "$guard_backup"
echo "$$ $guard_backup/pgid" >"$guard_lock"
export TMPDIR=$guard_dir
# A group left behind by a SIGKILLed bounded.py. Call before restoring the source, so an
# orphaned cargo cannot rewrite it afterwards.
# A bounded.py still running (the sweep was interrupted while it waited) is TERMed first:
# it kills its group itself, and reaping it here frees the sweep lock it inherited.
guard_bounded=
guard_kill() {
  if [ -n "$guard_bounded" ]; then
    kill -TERM "$guard_bounded" 2>/dev/null
    wait "$guard_bounded" 2>/dev/null
    guard_bounded=
  fi
  [ -s "$guard_backup/pgid" ] && kill -KILL -- "-$(cat "$guard_backup/pgid")" 2>/dev/null
  return 0
}
guard_cleanup() { guard_kill; rm -rf "$guard_dir"; }

# bounded COMMAND...: run COMMAND under the budget and the timeout. The run dir is
# emptied first, so the budget applies to this command alone. Status 124 is a timeout
# and 125 an exceeded budget; either one is reported on stderr. It runs in the background
# and is waited for: bash runs a trap only after a foreground command exits, but at once
# during a wait, so a SIGTERM to the sweep does not wait out the test (BRO-3467 #8).
bounded() {
  guard_kill
  find "$guard_dir" -mindepth 1 -maxdepth 1 ! -name .backup -exec rm -rf {} +
  python3 "$guard_repo/scripts/bounded.py" --dir "$guard_dir" --pgid-file "$guard_backup/pgid" \
    --budget-mb "${CUENCA_MUT_BUDGET_MB:-4096}" --timeout "${CUENCA_MUT_TIMEOUT:-1800}" -- "$@" &
  guard_bounded=$!
  wait "$guard_bounded"
  local status=$?
  guard_bounded=
  return "$status"
}
