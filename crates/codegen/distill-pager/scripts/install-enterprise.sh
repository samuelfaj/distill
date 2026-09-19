#!/usr/bin/env bash
# Modified for Distill by Samuel Fajreldines, 2026.
set -euo pipefail
exec bash "$(dirname -- "${BASH_SOURCE[0]}")/install.sh" "$@"
