#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TEST_DIR="$(mktemp -d)"
trap 'rm -rf "$TEST_DIR"' EXIT

bash -n "$ROOT_DIR/scripts/kwin_probe.sh"
bash -n "$ROOT_DIR/scripts/kwin_probe_guest.sh"

mkdir -p "$TEST_DIR/bin" "$TEST_DIR/artifacts"
cat > "$TEST_DIR/bin/docker" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "$KWIN_PROBE_TEST_LOG"
case "${1:-}" in
  build)
    exit 0
    ;;
  version)
    echo 'client=test server=test'
    ;;
  image)
    echo 'image_id=test'
    ;;
  run)
    exit 0
    ;;
  *)
    echo "unexpected docker command: $*" >&2
    exit 1
    ;;
esac
SH
chmod +x "$TEST_DIR/bin/docker"

export KWIN_PROBE_TEST_LOG="$TEST_DIR/docker.log"
PATH="$TEST_DIR/bin:$PATH" \
KWIN_PROBE_ARTIFACT_DIR="$TEST_DIR/artifacts" \
  "$ROOT_DIR/scripts/kwin_probe.sh" >/dev/null

mapfile -t calls < "$KWIN_PROBE_TEST_LOG"
[[ "${#calls[@]}" -eq 4 ]]
[[ "${calls[0]}" == build* ]]
[[ "${calls[1]}" == version* ]]
[[ "${calls[2]}" == "image inspect"* ]]
run_call="${calls[3]}"
[[ "$run_call" == run* ]]
[[ "$run_call" == *"--rm"* ]]
[[ "$run_call" == *"--network none"* ]]
[[ "$run_call" == *"--ipc private"* ]]
[[ "$run_call" == *"--read-only"* ]]
[[ "$run_call" == *"--cap-drop ALL"* ]]
[[ "$run_call" == *"--security-opt no-new-privileges"* ]]
[[ "$run_call" == *"--user 65534:65534"* ]]
[[ "$run_call" == *"--tmpfs /tmp:rw,nosuid,nodev,size=512m"* ]]
[[ "$run_call" == *"type=bind,src=$TEST_DIR/artifacts,dst=/artifacts"* ]]

for forbidden in \
  --privileged \
  /dev/uinput \
  /tmp/.X11-unix \
  /run/user \
  ydotoold \
  DISPLAY= \
  WAYLAND_DISPLAY= \
  XDG_RUNTIME_DIR=; do
  if [[ "$run_call" == *"$forbidden"* ]]; then
    echo "unsafe host dependency in docker run: $forbidden" >&2
    exit 1
  fi
done

mount_count="$(printf '%s\n' "$run_call" | grep -o -- '--mount' | wc -l)"
[[ "$mount_count" -eq 1 ]]
test -s "$TEST_DIR/artifacts/environment.txt"
test -f "$TEST_DIR/artifacts/console.log"

echo "KWin probe isolation tests ok"
