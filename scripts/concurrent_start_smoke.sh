#!/usr/bin/env bash
# Concurrent X11 workspace starts must all succeed on distinct displays.
set -euo pipefail
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${AGENT_WORKSPACE_BIN:-$ROOT_DIR/target/debug/agent-workspace-linux}"
COUNT="${AWL_CONCURRENT_STARTS:-6}"
WORK="$(mktemp -d "${XDG_CACHE_HOME:-$HOME/.cache}/awl-concurrent.XXXXXXXX")"
chmod 700 "$WORK"
export XDG_RUNTIME_DIR="$WORK/runtime"
mkdir -m 700 "$XDG_RUNTIME_DIR"
cleanup() {
  for i in $(seq 1 "$COUNT"); do "$BIN" workspace stop --id "concurrent-$i" >/dev/null 2>&1 || true; done
}
trap cleanup EXIT
for i in $(seq 1 "$COUNT"); do
  "$BIN" workspace start --id "concurrent-$i" --ack-hidden-workspace >"$WORK/$i.json" 2>"$WORK/$i.err" &
done
wait || true
failed=0
for i in $(seq 1 "$COUNT"); do
  if ! jq -e '.ok == true' "$WORK/$i.json" >/dev/null 2>&1; then
    failed=$((failed + 1))
    echo "start $i failed: $(head -c 300 "$WORK/$i.err")" >&2
  fi
done
duplicates="$(jq -r '.status.display // empty' "$WORK"/*.json | sort | uniq -d)"
[[ -z "$duplicates" ]] || { echo "duplicate displays: $duplicates" >&2; failed=$((failed + 1)); }
[[ "$failed" -eq 0 ]] || { echo "concurrent start smoke failed; evidence $WORK" >&2; exit 1; }
echo "concurrent start smoke passed: $COUNT workspaces on distinct displays"
