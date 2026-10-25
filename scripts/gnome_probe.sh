#!/usr/bin/env bash
set -euo pipefail
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${GNOME_PROBE_BIN:-$ROOT_DIR/target/debug/agent-workspace-linux}"
IMAGE="${GNOME_PROBE_IMAGE:-agent-workspace-linux-gnome-probe:ubuntu-26.04}"
ARTIFACT_DIR="${GNOME_PROBE_ARTIFACT_DIR:-$(mktemp -d "${XDG_CACHE_HOME:-$HOME/.cache}/gnome-probe.XXXXXXXX")}"
[[ -x "$BIN" ]] || { echo 'Build the workspace binary first' >&2; exit 1; }
BIN="$(realpath "$BIN")"
mkdir -p "$ARTIFACT_DIR"
chmod 0777 "$ARTIFACT_DIR"
ARTIFACT_DIR="$(realpath "$ARTIFACT_DIR")"
docker build --file "$ROOT_DIR/scripts/gnome_probe.Dockerfile" --tag "$IMAGE" "$ROOT_DIR/scripts"
# llvmpipe only: no host display, bus, input, GPU, or parent compositor.
docker run --rm --network none --ipc private --read-only --cap-drop ALL \
    --security-opt no-new-privileges --security-opt seccomp=unconfined \
    --security-opt apparmor=unconfined --user 65534:65534 --pids-limit 256 \
    --env LP_NUM_THREADS=2 --stop-timeout 5 --tmpfs /tmp:rw,nosuid,nodev,size=1g \
    --mount "type=bind,src=$ARTIFACT_DIR,dst=/artifacts" \
    --mount "type=bind,src=$BIN,dst=/opt/agent-workspace-linux,readonly" \
    "$IMAGE" "$@" 2>&1 | tee "$ARTIFACT_DIR/console.log"
