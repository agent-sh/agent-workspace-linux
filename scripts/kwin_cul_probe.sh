#!/usr/bin/env bash
set -euo pipefail
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${KWIN_PROBE_BIN:-$ROOT_DIR/target/debug/agent-workspace-linux}"
CUL_BIN="${COMPUTER_USE_LINUX_BIN:-$ROOT_DIR/../computer-use-linux/target/debug/computer-use-linux}"
ARTIFACT_DIR="${KWIN_PROBE_ARTIFACT_DIR:-$(mktemp -d "${XDG_CACHE_HOME:-$HOME/.cache}/kwin-cul-probe.XXXXXXXX")}"
RENDER_NODE="${KWIN_PROBE_RENDER_NODE:-}"
if [[ -z "$RENDER_NODE" ]]; then
  for candidate in /dev/dri/renderD*; do
    if [[ -r "$candidate" && -w "$candidate" ]]; then RENDER_NODE="$candidate"; break; fi
  done
fi
if [[ -z "$RENDER_NODE" ]]; then echo 'A DRM render node is required' >&2; exit 1; fi
if [[ ! -x "$BIN" || ! -x "$CUL_BIN" ]]; then
  echo 'Build both binaries and set KWIN_PROBE_BIN and COMPUTER_USE_LINUX_BIN' >&2
  exit 1
fi
BIN="$(realpath "$BIN")"
CUL_BIN="$(realpath "$CUL_BIN")"
mkdir -p "$ARTIFACT_DIR"
chmod 0777 "$ARTIFACT_DIR"
ARTIFACT_DIR="$(realpath "$ARTIFACT_DIR")"
docker build -t agent-workspace-linux-kwin-probe:ubuntu-26.04 -f "$ROOT_DIR/scripts/kwin_probe.Dockerfile" "$ROOT_DIR/scripts"
docker build -t agent-workspace-linux-kwin-cul:ubuntu-26.04 -f "$ROOT_DIR/scripts/kwin_cul_probe.Dockerfile" "$ROOT_DIR/scripts"
sha256sum "$BIN" "$CUL_BIN" > "$ARTIFACT_DIR/binaries.sha256"
set +e
docker run --rm --name "agent-workspace-kwin-cul-$$" \
  --network none --ipc private --read-only --cap-drop ALL \
  --security-opt no-new-privileges --user 65534:65534 \
  --group-add "$(stat -c %g "$RENDER_NODE")" --device "$RENDER_NODE" \
  --env LP_NUM_THREADS=2 --pids-limit 384 \
  --tmpfs /tmp:rw,nosuid,nodev,size=1g \
  --mount "type=bind,src=$BIN,dst=/opt/agent-workspace-linux,readonly" \
  --mount "type=bind,src=$CUL_BIN,dst=/opt/computer-use-linux,readonly" \
  --mount "type=bind,src=$ROOT_DIR/scripts,dst=/opt/kwin-probe,readonly" \
  --mount "type=bind,src=$ARTIFACT_DIR,dst=/artifacts" \
  --entrypoint python3 agent-workspace-linux-kwin-cul:ubuntu-26.04 \
  /opt/kwin-probe/kwin_cul_smoke.py 2>&1 | tee "$ARTIFACT_DIR/console.log"
status=${PIPESTATUS[0]}
set -e
echo "KWin/CUL acceptance artifacts: $ARTIFACT_DIR"
exit "$status"
