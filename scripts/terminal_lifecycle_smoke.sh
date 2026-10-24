#!/usr/bin/env bash
set -euo pipefail
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
node "$ROOT_DIR/scripts/mcp_terminal_tui_smoke.js"
compat_cache="${XDG_CACHE_HOME:-$HOME/.cache}"
mkdir -p "$compat_cache"
compat_dir="$(mktemp -d "$compat_cache/awl-terminal-compat.XXXXXXXX")"
trap 'rm -r -- "$compat_dir"' EXIT
cc -O2 -Wall -Wextra -Werror "$ROOT_DIR/scripts/fixtures/no_peerpidfd.c" -o "$compat_dir/no-peerpidfd"
echo "== terminal lifecycle without SO_PEERPIDFD =="
"$compat_dir/no-peerpidfd" node "$ROOT_DIR/scripts/mcp_terminal_tui_smoke.js"
