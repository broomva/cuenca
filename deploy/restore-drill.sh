#!/usr/bin/env bash
# Real local SSD drill: generated TEST keys, subprocess SIGKILL at every boundary.
set -euo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
cd "$here"
scratch=${CUENCA_DRILL_SCRATCH:-/Volumes/Broomva/scratch/bro-3258}
mkdir -p "$scratch/tmp"
export TMPDIR="$scratch/tmp"
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$scratch/target}
printf 'drill_start_utc=%s host=%s os=%s arch=%s source_sha=%s\n' "$(date -u +%Y-%m-%dT%H:%MZ)" "$(hostname)" "$(uname -s)" "$(uname -m)" "$(git rev-parse HEAD)"
cargo build --locked -p cuenca-server --bin cuenca-backup --example restore-drill
run="$scratch/drill-$(date -u +%Y%m%dT%H%M%SZ)-$$"
"$CARGO_TARGET_DIR/debug/examples/restore-drill" "$CARGO_TARGET_DIR/debug/cuenca-backup" "$run"
printf 'drill_end_utc=%s fixture=%s\n' "$(date -u +%Y-%m-%dT%H:%MZ)" "$run"
