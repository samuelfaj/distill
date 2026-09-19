#!/bin/sh
set -eu

repo_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
install_dir="$HOME/.local/share/distill/bin"
cd "$repo_dir"
cargo build -p distill-pager-bin --bin distill
mkdir -p "$install_dir"
install -m 755 "${CARGO_TARGET_DIR:-target}/debug/distill" "$install_dir/distill.new"
mv -f "$install_dir/distill.new" "$install_dir/distill"
ln -sfn distill "$install_dir/grok"
printf 'Distill instalado em %s\n' "$install_dir/distill"
printf 'Use esse diretório no início do PATH. Abra uma nova sessão após atualizar.\n'
