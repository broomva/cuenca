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

exit $survived
