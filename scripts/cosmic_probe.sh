#!/usr/bin/env bash
# Native COSMIC qualification with no host display, input or render devices.
set -euo pipefail
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${COSMIC_PROBE_BIN:-$ROOT_DIR/target/debug/agent-workspace-linux}"
IMAGE="${COSMIC_PROBE_IMAGE:-agent-workspace-linux-cosmic-probe:ubuntu-26.04}"
ARTIFACT_DIR="${COSMIC_PROBE_ARTIFACT_DIR:-$HOME/.cache/agent-workspace-cosmic-probe}"
BIN="$(realpath "$BIN")"
if [[ ! -x "$BIN" ]]; then echo "Build agent-workspace-linux first" >&2; exit 1; fi
mkdir -p "$ARTIFACT_DIR/tmp" "$ARTIFACT_DIR/scratch"
chmod 0777 "$ARTIFACT_DIR" "$ARTIFACT_DIR/tmp" "$ARTIFACT_DIR/scratch"
ARTIFACT_DIR="$(realpath "$ARTIFACT_DIR")"
docker build --file "$ROOT_DIR/scripts/cosmic_probe.Dockerfile" --tag "$IMAGE" "$ROOT_DIR/scripts"
docker image inspect "$IMAGE" --format '{{.Id}}' > "$ARTIFACT_DIR/image-id.txt"
# Bubblewrap needs user namespaces; no container capabilities or host devices.
timeout --signal=TERM --kill-after=5s 600s docker run --rm \
  --name "agent-workspace-cosmic-probe-$$" --network none --ipc private \
  --env LP_NUM_THREADS=2 --read-only --cap-drop ALL --security-opt no-new-privileges \
  --security-opt seccomp=unconfined --security-opt apparmor=unconfined \
  --security-opt systempaths=unconfined \
  --user 65534:65534 --pids-limit 256 --stop-timeout 5 \
  --mount "type=bind,src=$ARTIFACT_DIR/tmp,dst=/tmp" \
  --mount "type=bind,src=$ARTIFACT_DIR,dst=/artifacts" \
  --mount "type=bind,src=$BIN,dst=/opt/agent-workspace-linux,readonly" \
  "$IMAGE" 2>&1 | tee "$ARTIFACT_DIR/console.log"
