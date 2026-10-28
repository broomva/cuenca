#!/usr/bin/env bash
# Stopped-authority local fixture entrypoint. No cloud resources or credentials.
set -euo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
cd "$here"
exec "${CUENCA_BACKUP_BIN:-${CARGO_TARGET_DIR:-$here/target}/debug/cuenca-backup}" backup "$@"
