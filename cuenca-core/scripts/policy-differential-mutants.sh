#!/usr/bin/env bash
# Mutation proof for the policy differential test (owner ask A15, BRO-3313): each mutant
# removes one refusal the engine makes because PyYAML (p9) reads a policy differently,
# and scripts/policy-differential.py must report a disagreement for every one.
# Run from the crate directory: scripts/policy-differential-mutants.sh [--p9 P9_PY]
# The source is restored after each mutant, also on interrupt.
set -uo pipefail
cd "$(dirname "$0")/.."
backup=$(mktemp -d)
target=${CARGO_TARGET_DIR:-$(cargo metadata --format-version 1 --no-deps | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')}
engine=$target/debug/examples/policy_projection
restore() { [ -n "${current:-}" ] && cp "$backup/file" "$current"; current=; }
trap 'restore; rm -rf "$backup"' EXIT
trap 'exit 130' INT TERM

differential() {
  cargo build -q -p cuenca-core --example policy_projection --locked >"$backup/build" 2>&1 || return 3
  timeout 600 python3 scripts/policy-differential.py --engine "$engine" --cases 5000 --show 1 "$@" >"$backup/log" 2>&1
}

# A mutant only counts as killed against a run that agrees unmutated.
if ! differential "$@"; then
  echo "baseline disagrees: fix it before reading any mutant verdict"; cat "$backup/build" "$backup/log"; exit 2
fi
echo "baseline  $(tail -1 "$backup/log")"

survived=0
mutate() { # old, new: in src/gates/policy.rs
  local file=src/gates/policy.rs old=$1 new=$2; shift 2
  current=$file; cp "$file" "$backup/file"
  if ! python3 - "$file" "$old" "$new" <<'PY'
import sys, pathlib
p = pathlib.Path(sys.argv[1]); s = p.read_text()
if s.count(sys.argv[2]) != 1: sys.exit("anchor not unique: " + sys.argv[2])
p.write_text(s.replace(sys.argv[2], sys.argv[3]))
PY
  then echo "ANCHOR MISSING  $old"; survived=1; restore; return; fi
  differential "$@"
  case $? in
    1) echo "killed    $old ($(grep -m1 -o 'DISAGREE.*' "$backup/log" | cut -c1-100))" ;;
    3) echo "NO BUILD  $old (a mutant that does not build proves nothing)"; survived=1 ;;
    0) echo "SURVIVED  $old"; survived=1 ;;
    *) echo "ERROR     $old"; cat "$backup/log"; survived=1 ;;
  esac
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
mutate '            Ok((Event::Scalar(value, TScalarStyle::Plain, _, None), mark)) if value == "<<" => {' '            Ok((Event::Scalar(value, TScalarStyle::Plain, _, None), mark)) if false && value == "<<" => {'
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
