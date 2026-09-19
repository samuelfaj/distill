#!/bin/sh
# Install or upgrade Distill from its public GitHub releases.
set -eu
repo=samuelfaj/distill
install_dir=${DISTILL_INSTALL_DIR:-"$HOME/.local/share/distill"}
version=${DISTILL_VERSION:-latest}
case "$(uname -s)" in
  Darwin) os=macos ;;
  Linux) os=linux ;;
  *) echo 'This installer supports macOS and Linux.' >&2; exit 1 ;;
esac
case "$(uname -m)" in
  arm64|aarch64) arch=aarch64 ;;
  x86_64|amd64) arch=x86_64 ;;
  *) echo 'Unsupported CPU architecture.' >&2; exit 1 ;;
esac
if [ "$os" = macos ] && [ "$(sysctl -in hw.optional.arm64 2>/dev/null || true)" = 1 ]; then
  arch=aarch64
fi
command -v curl >/dev/null || { echo 'Install curl first.' >&2; exit 1; }
if [ "$version" = latest ]; then
  version=$(curl -fsSL --retry 3 "https://api.github.com/repos/$repo/releases/latest" |
    sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)
fi
version=${version#v}
case "$version" in
  ''|*[!0-9A-Za-z.+-]*) echo 'Invalid release version.' >&2; exit 1 ;;
esac
asset="distill-$os-$arch"
base="https://github.com/$repo/releases/download/v$version"
tmp_dir=$(mktemp -d)
trap 'rm -rf "$tmp_dir"' EXIT HUP INT TERM
curl -fL --retry 3 "$base/$asset" -o "$tmp_dir/$asset"
curl -fsSL --retry 3 "$base/SHA256SUMS" -o "$tmp_dir/SHA256SUMS"
expected=$(awk -v asset="$asset" '$2 == asset { print $1 }' "$tmp_dir/SHA256SUMS")
if command -v sha256sum >/dev/null 2>&1; then
  actual=$(sha256sum "$tmp_dir/$asset" | awk '{print $1}')
else
  actual=$(shasum -a 256 "$tmp_dir/$asset" | awk '{print $1}')
fi
[ -n "$expected" ] && [ "$actual" = "$expected" ] || {
  echo 'Checksum verification failed. Your existing installation was not changed.' >&2; exit 1;
}
chmod 755 "$tmp_dir/$asset"
"$tmp_dir/$asset" --version
mkdir -p "$install_dir/bin" "$install_dir/downloads"
binary="$install_dir/downloads/distill-$version-$os-$arch"
if [ -e "$binary" ]; then
  cmp -s "$tmp_dir/$asset" "$binary" || {
    echo "Existing $binary differs from the verified release; installation stopped." >&2; exit 1;
  }
else
  install -m 755 "$tmp_dir/$asset" "$binary.tmp.$$"
  mv "$binary.tmp.$$" "$binary"
fi
ln -s "../downloads/distill-$version-$os-$arch" "$install_dir/bin/distill.tmp.$$"
mv -f "$install_dir/bin/distill.tmp.$$" "$install_dir/bin/distill"
printf '%s\n' "$repo" > "$install_dir/.distill-release"
printf '\nDistill %s installed. Restart any open Distill sessions.\n' "$version"
printf 'Add this directory to your shell PATH:\n  export PATH="%s/bin:$PATH"\n' "$install_dir"
printf 'For future upgrades, run: distill update\n'
