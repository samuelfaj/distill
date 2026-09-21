#!/usr/bin/env bash
set -euo pipefail

invoker_dir="$(pwd)"
repo_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
cd "$repo_dir"

# Same production build as .github/workflows/release.yml. GROK_VERSION is what
# marks the binary as a release; without it Distill treats the build as dev.
version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' crates/codegen/distill-version/Cargo.toml)"
if [[ -z "$version" ]]; then
  echo "Could not read version from crates/codegen/distill-version/Cargo.toml" >&2
  exit 1
fi

CARGO_PROFILE_RELEASE_DEBUG=0 GROK_VERSION="$version" \
  cargo build --locked --release -p distill-pager-bin --bin distill

binary="${CARGO_TARGET_DIR:-target}/release/distill"
case "$binary" in
  /*) ;;
  *) binary="$repo_dir/$binary" ;;
esac
if [[ ! -x "$binary" ]]; then
  echo "Distill binary not found at $binary" >&2
  exit 1
fi

cd "$invoker_dir"
exec "$binary" "$@"
