#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
cd "$repo_dir"

cargo build --locked -p distill-pager-bin --bin distill

binary="${CARGO_TARGET_DIR:-target}/debug/distill"
if [[ ! -x "$binary" ]]; then
  echo "Distill binary not found at $repo_dir/$binary" >&2
  exit 1
fi

exec "$binary" "$@"
