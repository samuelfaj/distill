#!/bin/sh
# Install or upgrade Distill from its public GitHub releases.
set -eu
repo=samuelfaj/distill
install_dir=${DISTILL_INSTALL_DIR:-"$HOME/.local/share/distill"}
version=${DISTILL_VERSION:-latest}
case "$(uname -s)" in
  Darwin) os=macos ;;
  Linux) os=linux ;;
  MINGW*|MSYS*|CYGWIN*)
    # Git Bash reports a Windows kernel, not Linux. Hand off to install.ps1 so the Windows layout and .exe naming stay in one place.
    exec powershell.exe -NoProfile -ExecutionPolicy Bypass -Command "irm https://raw.githubusercontent.com/$repo/main/install.ps1 | iex"
    ;;
  *) echo 'This installer supports macOS, Linux, and Git Bash on Windows.' >&2; exit 1 ;;
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
bin_dir="$install_dir/bin"
case ":$PATH:" in
  *":$bin_dir:"*) on_path=1 ;;
  *) on_path=0 ;;
esac
if [ "$on_path" = 0 ] && [ -z "${DISTILL_NO_MODIFY_PATH:-}" ]; then
  case "${SHELL:-}" in
    */zsh) rc="${ZDOTDIR:-$HOME}/.zshrc"; line="export PATH=\"$bin_dir:\$PATH\"" ;;
    */bash) if [ "$os" = macos ]; then rc="$HOME/.bash_profile"; else rc="$HOME/.bashrc"; fi; line="export PATH=\"$bin_dir:\$PATH\"" ;;
    */fish) rc="${XDG_CONFIG_HOME:-$HOME/.config}/fish/config.fish"; line="fish_add_path \"$bin_dir\"" ;;
    *) rc="$HOME/.profile"; line="export PATH=\"$bin_dir:\$PATH\"" ;;
  esac
  if [ -f "$rc" ] && grep -qF "$line" "$rc"; then
    printf 'PATH entry already in %s.\n' "$rc"
  elif mkdir -p "$(dirname "$rc")" && printf '\n# Added by the Distill installer\n%s\n' "$line" >> "$rc"; then
    printf 'Added %s to PATH in %s. Open a new terminal to use distill.\n' "$bin_dir" "$rc"
  else
    on_path=0; echo "Could not update $rc." >&2
  fi
  [ -f "$rc" ] && grep -qF "$line" "$rc" && on_path=1
fi
if [ "$on_path" = 0 ]; then
  printf 'Add this directory to your shell PATH:\n  export PATH="%s:$PATH"\n' "$bin_dir"
fi
printf 'For future upgrades, run: distill update\n'
