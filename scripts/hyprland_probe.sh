#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${HYPRLAND_PROBE_BIN:-$ROOT_DIR/target/debug/agent-workspace-linux}"
IMAGE="${HYPRLAND_PROBE_IMAGE:-agent-workspace-linux-hyprland-probe:ubuntu-26.04}"
ARTIFACT_DIR="${HYPRLAND_PROBE_ARTIFACT_DIR:-$(mktemp -d "${XDG_CACHE_HOME:-$HOME/.cache}/hyprland-probe.XXXXXXXX")}"

need() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "missing required command: $1" >&2
    exit 1
  fi
}

need docker
need mktemp
need tee

if [[ ! -x "$BIN" ]]; then
  echo "Build agent-workspace-linux first, or set HYPRLAND_PROBE_BIN to the binary path" >&2
  exit 1
fi
BIN="$(realpath "$BIN")"
RENDER_NODE="${HYPRLAND_PROBE_RENDER_NODE:-}"
if [[ -z "$RENDER_NODE" ]]; then
  for candidate in /dev/dri/renderD*; do
    if [[ -r "$candidate" && -w "$candidate" ]]; then RENDER_NODE="$candidate"; break; fi
  done
fi
if [[ -z "$RENDER_NODE" ]]; then echo "Hyprland native capture needs an accessible DRM render node" >&2; exit 1; fi
mkdir -p "$ARTIFACT_DIR"
chmod 0777 "$ARTIFACT_DIR"
ARTIFACT_DIR="$(cd "$ARTIFACT_DIR" && pwd)"

echo "== build disposable Hyprland probe image =="
docker build \
  --file "$ROOT_DIR/scripts/hyprland_probe.Dockerfile" \
  --tag "$IMAGE" \
  "$ROOT_DIR/scripts"

{
  echo "image=$IMAGE"
  docker version --format 'client={{.Client.Version}} server={{.Server.Version}}'
  docker image inspect "$IMAGE" --format 'image_id={{.Id}}'
} > "$ARTIFACT_DIR/environment.txt"

echo "== test native Hyprland at 100% and 150% through workspace IPC =="
set +e
docker run \
  --rm \
  --name "agent-workspace-hyprland-probe-$$" \
  --network none \
  --ipc private \
  --env LP_NUM_THREADS=2 \
  --read-only \
  --cap-drop ALL \
  --security-opt no-new-privileges \
  --security-opt seccomp=unconfined \
  --security-opt apparmor=unconfined \
  --security-opt systempaths=unconfined \
  --user 65534:65534 \
  --group-add "$(stat -c %g "$RENDER_NODE")" \
  --device "$RENDER_NODE" \
  --pids-limit 256 \
  --stop-timeout 5 \
  --tmpfs /tmp:rw,nosuid,nodev,size=1g \
  --mount "type=bind,src=$ARTIFACT_DIR,dst=/artifacts" \
  --mount "type=bind,src=$BIN,dst=/opt/agent-workspace-linux,readonly" \
  "$IMAGE" "$@" 2>&1 | tee "$ARTIFACT_DIR/console.log"
probe_status=${PIPESTATUS[0]}
set -e

echo "Hyprland probe artifacts: $ARTIFACT_DIR"
exit "$probe_status"
