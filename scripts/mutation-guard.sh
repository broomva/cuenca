# Sourced by every crate's scripts/mutation-check.sh, before anything makes a tempdir.
# Gives the run its own scratch directory, exported as TMPDIR, and runs each cargo test
# under scripts/bounded.py, so a mutant that loops or writes without end is killed by a
# timeout or a disk budget and not by a full disk (BRO-3455).
#
#   CUENCA_MUT_TMPDIR      parent of the run dirs (default /tmp/cuenca-mut). Keep it
#                          short: tests bind Unix sockets at $TMPDIR/.tmpXXXXXX/admin.sock,
#                          and macOS caps a socket path at 104 bytes, so a run dir longer
#                          than 64 bytes is refused. Point it at a roomy disk if /tmp is not.
#   CUENCA_MUT_BUDGET_MB   bytes one cargo test may leave under the run dir (default 4096)
#   CUENCA_MUT_TIMEOUT     seconds one cargo test may run, build included (default 1800)
#
# A run dir is named run-<pid>. One whose process is gone (killed with SIGKILL, so its
# EXIT trap never ran) is removed when the next run starts.
guard_repo=$(git rev-parse --show-toplevel)
guard_root=${CUENCA_MUT_TMPDIR:-/tmp/cuenca-mut}
case $guard_root in
  /*) ;;
  *) echo "mutation-guard: CUENCA_MUT_TMPDIR must be absolute, not $guard_root" >&2; exit 2 ;;
esac
mkdir -p "$guard_root"
for stale in "$guard_root"/run-*; do
  [ -d "$stale" ] || continue
  kill -0 "${stale##*/run-}" 2>/dev/null || rm -rf "$stale"
done
guard_dir=$guard_root/run-$$
if [ "${#guard_dir}" -gt 64 ]; then
  echo "mutation-guard: run dir $guard_dir is ${#guard_dir} bytes; the tests' sockets need it at most 64 (set CUENCA_MUT_TMPDIR shorter)" >&2
  exit 2
fi
# The script's own backups and logs sit in .backup, which bounded() keeps.
guard_backup=$guard_dir/.backup
mkdir -p "$guard_backup"
export TMPDIR=$guard_dir
# A group left behind by a SIGKILLed bounded.py is killed before its dir is removed.
guard_cleanup() {
  [ -s "$guard_backup/pgid" ] && kill -KILL -- "-$(cat "$guard_backup/pgid")" 2>/dev/null
  rm -rf "$guard_dir"
}

# bounded COMMAND...: run COMMAND under the budget and the timeout. The run dir is
# emptied first, so the budget applies to this command alone. Status 124 is a timeout
# and 125 an exceeded budget; either one is reported on stderr.
bounded() {
  find "$guard_dir" -mindepth 1 -maxdepth 1 ! -name .backup -exec rm -rf {} +
  python3 "$guard_repo/scripts/bounded.py" --dir "$guard_dir" --pgid-file "$guard_backup/pgid" \
    --budget-mb "${CUENCA_MUT_BUDGET_MB:-4096}" --timeout "${CUENCA_MUT_TIMEOUT:-1800}" -- "$@"
}
