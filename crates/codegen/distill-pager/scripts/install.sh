#!/usr/bin/env bash
# Modified for Distill by Samuel Fajreldines, 2026. Source-only installation.
set -euo pipefail
script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/../../../.." && pwd)"
cd "$repo_root"
cargo build --release -p distill-pager-bin --bin distill
install_dir="${DISTILL_BIN_DIR:-$HOME/.local/bin}"
mkdir -p "$install_dir"
install -m 755 "${CARGO_TARGET_DIR:-target}/release/distill" "$install_dir/distill"
printf 'Distill installed at %s/distill\nAdd this directory to PATH if needed.\n' "$install_dir"
