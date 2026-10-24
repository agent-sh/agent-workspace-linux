#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMAGE="${KWIN_PROBE_IMAGE:-agent-workspace-linux-kwin-probe:ubuntu-24.04}"
ARTIFACT_DIR="${KWIN_PROBE_ARTIFACT_DIR:-$(mktemp -d "${TMPDIR:-/tmp}/kwin-probe.XXXXXXXX")}"

need() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "missing required command: $1" >&2
    exit 1
  fi
}

need docker
need mktemp
need tee

mkdir -p "$ARTIFACT_DIR"
chmod 0777 "$ARTIFACT_DIR"
ARTIFACT_DIR="$(cd "$ARTIFACT_DIR" && pwd)"

echo "== build disposable KWin probe image =="
docker build \
  --file "$ROOT_DIR/scripts/kwin_probe.Dockerfile" \
  --tag "$IMAGE" \
  "$ROOT_DIR/scripts"

{
  echo "image=$IMAGE"
  docker version --format 'client={{.Client.Version}} server={{.Server.Version}}'
  docker image inspect "$IMAGE" --format 'image_id={{.Id}}'
} > "$ARTIFACT_DIR/environment.txt"

echo "== probe virtual QPainter and nested private Xvfb backends =="
set +e
docker run \
  --rm \
  --name "agent-workspace-kwin-probe-$$" \
  --network none \
  --ipc private \
  --read-only \
  --cap-drop ALL \
  --security-opt no-new-privileges \
  --user 65534:65534 \
  --pids-limit 256 \
  --stop-timeout 5 \
  --tmpfs /tmp:rw,nosuid,nodev,size=512m \
  --mount "type=bind,src=$ARTIFACT_DIR,dst=/artifacts" \
  "$IMAGE" "$@" 2>&1 | tee "$ARTIFACT_DIR/console.log"
probe_status=${PIPESTATUS[0]}
set -e

echo "KWin probe artifacts: $ARTIFACT_DIR"
exit "$probe_status"
