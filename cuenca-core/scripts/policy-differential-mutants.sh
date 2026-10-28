#!/usr/bin/env bash
# Mutation proof for the policy differential test (owner ask A15, BRO-3313): each mutant
# removes one refusal the engine makes because PyYAML (p9) reads a policy differently,
# and scripts/policy-differential.py must report a disagreement for every one.
# Run from the crate directory: scripts/policy-differential-mutants.sh [--p9 P9_PY]
# The source is restored after each mutant, also on interrupt.
set -uo pipefail
cd "$(dirname "$0")/.."
. ../scripts/mutation-guard.sh
backup=$guard_backup
lock=$(git rev-parse --show-toplevel)/Cargo.lock
# Checked byte for byte: a truncated copy (full scratch disk) would be copied back over the real lock.
cp "$lock" "$backup/Cargo.lock" && cmp -s "$lock" "$backup/Cargo.lock" || { echo "cannot back up $lock"; exit 6; }
# A failed copy back keeps the run dir, and .backup/path or .backup/lock-failed makes the
# next run refuse to start. .backup/path goes only after the lock is back too.
restore() {
  if [ -n "${current:-}" ] && ! cp "$backup/file" "$current"; then
    echo "restore failed: $current is still mutated; its original is $backup/file" >&2; trap - EXIT; exit 4
  fi
  if ! cp "$backup/Cargo.lock" "$lock"; then
    : >"$backup/lock-failed"
    echo "restore failed: $lock; its original is $backup/Cargo.lock" >&2; trap - EXIT; exit 4
  fi
  current=; rm -f "$backup/path"
}
trap 'guard_kill; restore; guard_cleanup' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

target=${CARGO_TARGET_DIR:-$(cargo metadata --format-version 1 --no-deps | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')}
engine=$target/debug/examples/policy_projection

differential() {
  bounded cargo build -q -p cuenca-core --example policy_projection --locked >"$backup/build" 2>&1 || {
    local s=$?
    [ "$s" -ge 128 ] && return "$s"
    return 3
  }
  bounded python3 scripts/policy-differential.py --engine "$engine" --cases 5000 --show 1 "$@" >"$backup/log" 2>&1
}

# A mutant only counts as killed against a run that agrees unmutated.
if ! differential "$@"; then
  echo "baseline disagrees: fix it before reading any mutant verdict"; cat "$backup/build" "$backup/log"; exit 2
fi
echo "baseline  $(tail -1 "$backup/log")"

survived=0
args=("$@") # mutate's own arguments shadow these, so the --p9 passthrough reads a copy
mutate() { # [file] old, new (default file: src/gates/policy.rs)
  local file=src/gates/policy.rs old=$1 new=$2
  if [ $# -eq 3 ]; then file=$1; old=$2; new=$3; fi
  cp "$file" "$backup/file" && echo "$PWD/$file" >"$backup/path" || { echo "cannot back up $file"; exit 6; }
  current=$file
  if ! python3 - "$file" "$old" "$new" <<'PY'
import sys, pathlib
p = pathlib.Path(sys.argv[1]); s = p.read_text()
if s.count(sys.argv[2]) != 1: sys.exit("anchor not unique: " + sys.argv[2])
p.write_text(s.replace(sys.argv[2], sys.argv[3]))
PY
  then echo "ANCHOR MISSING  $old"; survived=1; restore; return; fi
  differential ${args[@]+"${args[@]}"}; local status=$?
  if [ "$status" -ge 128 ]; then
    echo "ABORTED   $old (status $status)"; exit 3
  elif [ "$status" -eq 1 ]; then
    echo "killed    $old -> $new ($(grep -m1 -o 'DISAGREE.*' "$backup/log" | cut -c1-100))"
  elif [ "$status" -eq 3 ]; then
    echo "NO BUILD  $old (a mutant that does not build proves nothing)"; survived=1
  elif [ "$status" -eq 0 ]; then
    echo "SURVIVED  $old -> $new"; survived=1
  else
    echo "ERROR     $old"; cat "$backup/log"; survived=1
  fi
  restore
}

mutate "                '\\u{85}' | '\\u{2028}' | '\\u{2029}' | '\\u{fffe}' | '\\u{ffff}'" "                '\\u{fffe}' | '\\u{ffff}'"
mutate "                '\\u{85}' | '\\u{2028}' | '\\u{2029}' | '\\u{fffe}' | '\\u{ffff}'" "                '\\u{85}' | '\\u{2028}' | '\\u{2029}'"
mutate "|| (c.is_control() && !matches!(c, '\\n' | '\\r'))" "|| (c.is_control() && !matches!(c, '\\t' | '\\n' | '\\r'))"
mutate "|| (c.is_control() && !matches!(c, '\\n' | '\\r'))" ""
mutate "        let text = text.strip_prefix('\\u{feff}').unwrap_or(text);" ''
# The alias refusal is a resource bound (an alias copies its node at every use), not a
# reading p9 differs on: both expand aliases alike. mutation-check.sh covers it.
mutate '                Event::Scalar(_, _, _, Some(_))
                | Event::SequenceStart(_, Some(_))' '                Event::SequenceStart(_, Some(_))'
# The refusals of whole YAML features (r10, A, B and C).
mutate '            TokenType::VersionDirective(..) | TokenType::TagDirective(..) => {' '            TokenType::VersionDirective(..) | TokenType::TagDirective(..) if false => {'
mutate '            TokenType::Anchor(_) => return refuse("a YAML anchor", line),' ''
mutate '            TokenType::Value if flow > 0 && line != last_line => {' '            TokenType::Value if false => {'
mutate 'if flow > 0 && !flow_plain_ok(&value) =>' 'if false =>'
mutate '        .is_some_and(|c| c.is_ascii_alphanumeric() || "_./+".contains(c))' '        .is_some_and(|c| c.is_ascii_alphanumeric() || "_./+-|>:".contains(c))'
mutate '        && chars.all(|c| c.is_ascii_alphanumeric() || "_./*@+- ".contains(c))' '        && chars.all(|c| c.is_ascii_alphanumeric() || "_./*@+- ?#:".contains(c))'
mutate '                if key
                    && (matches!(' '                if false
                    && (matches!('
mutate 'if !matches!(Yaml::from_str(v), Yaml::String(_))' 'if false'
mutate src/gates/builtin.rs 'let mut k = 1;' 'let mut k = 0;'
mutate src/gates/builtin.rs 'String::from("(?s)^")' 'String::from("(?is)^")'
mutate src/gates/policy.rs 'let lower = path.to_lowercase();' 'let lower = path;'
mutate 'path.filter(|p| !p.trim().is_empty())' 'path.filter(|p| !p.is_empty())'
mutate src/gates/builtin.rs 'pattern.to_lowercase()' 'pattern'
mutate '                if !plain_scalar_agrees(&value) =>' '                if false =>'
mutate '    if same.is_match(v) {' '    if true {'
mutate '                && v.as_bool().is_none()' '                && false'
mutate '            && !["squash", "merge", "rebase"].contains(&method.as_str())' '            && false'
mutate '        if gated && default_action == "auto" {' '        if false {'
mutate '                if gated && action == "auto" {' '                if false {'
mutate '            && !(lo..=hi).contains(&n)' '            && false'
mutate '        if strata != ["A", "B", "C"] {' '        if false {'
mutate ')? && !checks.iter().any(|c| c == "stability-check")' ')? && false'
mutate '.is_some_and(|n| n.iter().any(String::is_empty))' '.is_some_and(|_| false)'
mutate '                if !RULE_ACTIONS.contains(&action) {' '                if false {'

exit $survived
