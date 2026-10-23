#!/usr/bin/env bash
# Mutation check for cuenca-cli: each mutant breaks one guarantee, and the tests must
# fail for every one. Run from the crate directory: scripts/mutation-check.sh
# Prints one line per mutant and exits non-zero if any mutant survives. The source is
# restored after each mutant, also on interrupt.
set -uo pipefail
cd "$(dirname "$0")/.."
backup=$(mktemp -d)
lock=$(git rev-parse --show-toplevel)/Cargo.lock
cp "$lock" "$backup/Cargo.lock"
restore() { [ -n "${current:-}" ] && cp "$backup/file" "$current"; current=; cp "$backup/Cargo.lock" "$lock"; }
trap 'restore; rm -rf "$backup"' EXIT
trap 'exit 130' INT TERM

# A mutant only counts as killed against a suite that passes unmutated.
if ! cargo test -q -p cuenca-cli >"$backup/log" 2>&1; then
  echo "baseline is red: fix the suite before reading any mutant verdict"; cat "$backup/log"; exit 2
fi
echo "baseline  green"

survived=0
mutate() { # file, old, new, then the cargo test arguments that should catch it
  local file=$1 old=$2 new=$3; shift 3
  current=$file; cp "$file" "$backup/file"
  if ! python3 - "$file" "$old" "$new" <<'PY'
import sys, pathlib
p = pathlib.Path(sys.argv[1]); s = p.read_text()
if s.count(sys.argv[2]) != 1: sys.exit("anchor not unique: " + sys.argv[2])
p.write_text(s.replace(sys.argv[2], sys.argv[3]))
PY
  then echo "ANCHOR MISSING  $file: $old"; survived=1; restore; return; fi
  if cargo test -q -p cuenca-cli "$@" >"$backup/log" 2>&1; then
    echo "SURVIVED  $file: $old"; survived=1
  elif grep -qE '^error\[E[0-9]+\]|could not compile|failed to select a version' "$backup/log"; then
    echo "NO BUILD  $file: $old (a mutant that does not build proves nothing)"; survived=1
  else
    echo "killed    $file: $old"
  fi
  restore
}

# The outbox: a snapshot is queued before it is sent, so an unreachable authority
# loses nothing; a flush stops at the first draft that cannot go out.
mutate src/verbs.rs 'local.queue(snap.changeset, snap.tree, &snap.new_blobs)?;' 'let _ = (snap.changeset, snap.tree);' --test cli
mutate src/local.rs '                    flush.error = Some(format!("{e:#}"));
                    break;' '                    flush.error = Some(format!("{e:#}"));' --test cli
mutate src/local.rs '        flush.pending = left;' '        flush.pending = 0;' --test cli
# What is queued is what was scanned: an object that no longer hashes to its id is refused.
mutate src/local.rs 'if hashed.oid() != blob.oid {' 'if false {' --lib
# Acknowledgement moves the checkout's base, and follows a fenced draft to its sibling.
mutate src/local.rs '        self.state.published = entry.tree.clone();' '' --test cli
mutate src/local.rs '                self.state.fork = p.fork;' '' --test cli
# Watch mode snapshots once changes settle (or keep going too long).
mutate src/verbs.rs 'if settled || overdue {' 'if false {' --test cli watch
# The key's file is private.
mutate src/config.rs '.set_permissions(std::fs::Permissions::from_mode(0o600))?;' '.set_permissions(std::fs::Permissions::from_mode(0o644))?;' --test cli every_agent_verb

# P20 r1 (B, C): a crash around acknowledgement neither blocks the outbox nor fences a
# retry the fork already took.
mutate src/local.rs 'if entry.changeset.target == published && entry.changeset.base != published {' 'if false {' --test cli crash
mutate src/local.rs '}) if code == "not_at_base" && head == entry.changeset.target.to_hex() => Sent {' '}) if false => Sent {' --test cli crash
# A path is its checkout only while it is the same directory; no checkout inside another;
# a registration with queued snapshots is never dropped.
mutate src/local.rs 'if identity(root).ok() != Some(state.root_id) {' 'if false {' --test cli reused
mutate src/local.rs '&& identity(outer).ok() == Some(state.root_id)' '&& false' --test cli inside
mutate src/local.rs 'if queued > 0 {' 'if false {' --test cli reused
# Watch mode reports a snapshot it cannot take.
mutate src/verbs.rs 'if now - since >= max_wait && now - reported >= max_wait {' 'if false {' --test cli watch_mode_reports

# P20 r1 (A): a scan that publishes nothing reports ignored base paths instead of failing.
mutate src/verbs.rs '    let options = SnapshotOptions {
        drop_ignored: true,' '    let options = SnapshotOptions {
        drop_ignored: false,' --test cli a_git_location

# P20 r2 (B, C): base paths .forkignore names are no change to the watch, and a
# directory made again at a checkout's path is told apart by its birth time.
mutate src/verbs.rs 'seen.changes.is_empty() && (seen.ignored.is_empty() || !flags.drop_ignored),' 'seen.changes.is_empty() && seen.ignored.is_empty(),' --test cli a_git_location

# P20 r3 (A, B, C): a former holder sends again the base blobs its draft puts; an
# upload cut off by a refusal is a refusal, not offline; and on macOS the root's birth
# time, which setting an old mtime moves, is no part of a checkout's identity (that
# mutant is only caught on macOS).
mutate src/local.rs 'if code == "missing_blob")' 'if false)' --test cli after_a_takeover_a_rename

# P20 r4 (B, C): the queue keeps the bytes of the base blobs a draft puts, so a former
# holder can send them after the checkout has edited or deleted those files.
mutate src/local.rs 'for blob in new.iter().chain(&base_blobs) {' 'for blob in new.iter() {' --test cli queued_renames_edited
mutate src/client.rs '        match self.whoami() {' '        match Err::<Whoami, _>(ClientError::Offline { url: url.clone(), reason: String::new() }) {' --test cli a_refused_large_upload
mutate src/local.rs 'fn born_ns(_: &std::fs::Metadata) -> Option<u128> {
    None
}' 'fn born_ns(m: &std::fs::Metadata) -> Option<u128> {
    m.created().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_nanos())
}' --test cli an_old_mtime

exit $survived
