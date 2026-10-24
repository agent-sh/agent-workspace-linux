#!/usr/bin/env bash
set -euo pipefail

ARTIFACT_DIR="${KWIN_PROBE_ARTIFACT_DIR:-/artifacts}"
QDBUS="$(command -v qdbus || true)"
if [[ -z "$QDBUS" && -x /usr/lib/qt5/bin/qdbus ]]; then
  QDBUS=/usr/lib/qt5/bin/qdbus
fi

if [[ "${1:-}" == "--client-env" || "${0##*/}" == "kwin-client-env" ]]; then
  : "${KWIN_PROBE_CLIENT_ENV:?missing client environment output path}"
  {
    printf 'DISPLAY=%q\n' "${DISPLAY:-}"
    printf 'XAUTHORITY=%q\n' "${XAUTHORITY:-}"
    printf 'WAYLAND_DISPLAY=%q\n' "${WAYLAND_DISPLAY:-}"
  } > "$KWIN_PROBE_CLIENT_ENV"
  exit 0
fi

need() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "missing required command: $1" >&2
    exit 1
  fi
}

for command_name in dbus-daemon kwin_wayland python3 spectacle timeout wayland-info weston-terminal wtype Xvfb xclock xdotool xwininfo; do
  need "$command_name"
done
[[ -n "$QDBUS" && -x "$QDBUS" ]] || { echo "qdbus not found" >&2; exit 1; }

mkdir -p "$ARTIFACT_DIR"
{
  kwin_wayland --version
  printf 'uid=%s gid=%s\n' "$(id -u)" "$(id -g)"
  printf 'initial_DISPLAY=%s\n' "${DISPLAY:-<unset>}"
  printf 'initial_WAYLAND_DISPLAY=%s\n' "${WAYLAND_DISPLAY:-<unset>}"
  printf 'initial_XDG_RUNTIME_DIR=%s\n' "${XDG_RUNTIME_DIR:-<unset>}"
  printf 'initial_DBUS_SESSION_BUS_ADDRESS=%s\n' "${DBUS_SESSION_BUS_ADDRESS:-<unset>}"
  test ! -e /dev/uinput && echo 'host_uinput_absent=true'
  test ! -S /run/ydotoold/socket && echo 'host_ydotoold_socket_absent=true'
  test ! -e /tmp/.X11-unix && echo 'host_x11_socket_directory_absent=true'
} > "$ARTIFACT_DIR/isolation.txt"
kwin_wayland --help > "$ARTIFACT_DIR/kwin-help.txt" 2>&1
dpkg-query -W -f='${binary:Package}\t${Version}\n' \
  dbus dbus-x11 kwin-wayland kwin-common libqt5core5t64 libqt5gui5t64 libwayland-client0 \
  qdbus-qt5 kde-spectacle wayland-utils weston wtype x11-apps x11-utils xdotool xvfb xwayland \
  > "$ARTIFACT_DIR/package-versions.txt"
cat /etc/os-release > "$ARTIFACT_DIR/os-release.txt"

record_result() {
  local mode="$1"
  local status="$2"
  python3 - "$ARTIFACT_DIR/results.jsonl" "$mode" "$status" <<'PY'
import json
import pathlib
import sys

path, mode, status = sys.argv[1:]
with pathlib.Path(path).open("a", encoding="utf-8") as stream:
    stream.write(json.dumps({"mode": mode, "status": status}, sort_keys=True) + "\n")
PY
}

run_mode() (
  set -euo pipefail
  local mode="$1"
  local work="/tmp/kwin-probe-$mode"
  local runtime="$work/runtime"
  local home="$work/home"
  local mode_artifacts="$ARTIFACT_DIR/$mode"
  local socket="kwin-probe-$mode"
  local xvfb_display=":77"
  local kwin_pid=""
  local xvfb_pid=""
  local dbus_pid=""
  local app_pid=""

  mkdir -p "$runtime" "$home/.config" "$mode_artifacts" "$work/captures"
  chmod 0700 "$runtime"
  export HOME="$home"
  export XDG_RUNTIME_DIR="$runtime"
  export XDG_CONFIG_HOME="$home/.config"
  export XDG_CACHE_HOME="$home/.cache"
  export KWIN_COMPOSE=Q
  export LIBGL_ALWAYS_SOFTWARE=1
  export QT_QUICK_BACKEND=software
  unset DISPLAY WAYLAND_DISPLAY XAUTHORITY DBUS_SESSION_BUS_ADDRESS

  # shellcheck disable=SC2329
  cleanup() {
    local exit_code=$?
    trap - EXIT
    if [[ -n "$app_pid" ]]; then
      kill -- "-$app_pid" >/dev/null 2>&1 || true
      wait "$app_pid" >/dev/null 2>&1 || true
    fi
    if [[ -n "$kwin_pid" ]]; then
      kill -- "-$kwin_pid" >/dev/null 2>&1 || true
      wait "$kwin_pid" >/dev/null 2>&1 || true
    fi
    if [[ -n "$xvfb_pid" ]]; then
      kill -- "-$xvfb_pid" >/dev/null 2>&1 || true
      wait "$xvfb_pid" >/dev/null 2>&1 || true
    fi
    if [[ -n "$dbus_pid" ]]; then
      kill "$dbus_pid" >/dev/null 2>&1 || true
      wait "$dbus_pid" >/dev/null 2>&1 || true
    fi
    rm -rf "$work/captures"
    exit "$exit_code"
  }
  trap cleanup EXIT

  printf '[General]\n\n[Compositing]\nEnabled=true\n\n[Plugins]\n' > "$home/.config/kwinrc"
  dbus-daemon --session --fork --print-address=1 --print-pid=1 > "$work/dbus.txt"
  mapfile -t dbus_info < "$work/dbus.txt"
  export DBUS_SESSION_BUS_ADDRESS="${dbus_info[0]}"
  dbus_pid="${dbus_info[1]}"

  kwin_args=(
    --socket "$socket"
    --width 960
    --height 640
    --scale 1
    --output-count 1
    --no-lockscreen
    --no-global-shortcuts
    --no-kactivities
    --xwayland
  )
  if [[ "$mode" == "virtual" ]]; then
    kwin_args+=(--virtual)
  elif [[ "$mode" == "nested-xvfb" ]]; then
    setsid Xvfb "$xvfb_display" -screen 0 960x640x24 -nolisten tcp -noreset \
      > "$mode_artifacts/xvfb.log" 2>&1 &
    xvfb_pid=$!
    for _ in {1..100}; do
      [[ -S "/tmp/.X11-unix/X${xvfb_display#:}" ]] && break
      kill -0 "$xvfb_pid" >/dev/null 2>&1 || { cat "$mode_artifacts/xvfb.log" >&2; exit 1; }
      sleep 0.1
    done
    [[ -S "/tmp/.X11-unix/X${xvfb_display#:}" ]]
    kwin_args+=(--x11-display "$xvfb_display")
  else
    echo "unknown mode: $mode" >&2
    exit 2
  fi

  export KWIN_PROBE_CLIENT_ENV="$work/client-env.sh"
  mkdir -p /tmp/.X11-unix
  chmod 1777 /tmp/.X11-unix
  ln -s "$0" "$work/kwin-client-env"
  setsid kwin_wayland "${kwin_args[@]}" "$work/kwin-client-env" \
    > "$mode_artifacts/kwin.log" 2>&1 &
  kwin_pid=$!

  for _ in {1..150}; do
    if [[ -S "$runtime/$socket" && -s "$KWIN_PROBE_CLIENT_ENV" ]]; then
      if "$QDBUS" org.kde.KWin /org/kde/KWin org.kde.KWin.supportInformation \
          > "$mode_artifacts/support-information.txt" 2>&1; then
        break
      fi
    fi
    if ! kill -0 "$kwin_pid" >/dev/null 2>&1; then
      echo "KWin exited before readiness in $mode mode" >&2
      cat "$mode_artifacts/kwin.log" >&2
      exit 1
    fi
    sleep 0.1
  done
  [[ -S "$runtime/$socket" && -s "$KWIN_PROBE_CLIENT_ENV" ]]
  "$QDBUS" org.kde.KWin /org/kde/KWin org.kde.KWin.supportInformation >/dev/null

  # shellcheck disable=SC1090
  source "$KWIN_PROBE_CLIENT_ENV"
  [[ "$WAYLAND_DISPLAY" == "$socket" ]]
  [[ -n "$DISPLAY" ]]
  if [[ "$mode" == "nested-xvfb" ]]; then
    [[ "$DISPLAY" != "$xvfb_display" ]]
  fi
  {
    printf 'wayland_socket=%s\n' "$runtime/$socket"
    printf 'xwayland_display=%s\n' "$DISPLAY"
    printf 'xauthority=%s\n' "${XAUTHORITY:-<unset>}"
  } > "$mode_artifacts/readiness.txt"

  wayland-info > "$mode_artifacts/wayland-info.txt" 2>&1
  "$QDBUS" org.kde.KWin /org/kde/KWin > "$mode_artifacts/kwin-dbus.txt" 2>&1
  "$QDBUS" org.kde.KWin /Screenshot > "$mode_artifacts/screenshot-dbus.txt" 2>&1 || true

  xclock > "$mode_artifacts/xclock.log" 2>&1 &
  app_pid=$!
  for _ in {1..100}; do
    if xdotool search --class xclock > "$work/xclock-window.txt" 2>/dev/null; then
      break
    fi
    sleep 0.1
  done
  xdotool search --class xclock > "$work/xclock-window.txt"
  xwininfo -id "$(head -n 1 "$work/xclock-window.txt")" > "$mode_artifacts/xclock-window.txt"
  kill "$app_pid" >/dev/null 2>&1 || true
  wait "$app_pid" >/dev/null 2>&1 || true
  app_pid=""

  cat > "$work/native-reader.sh" <<SH
#!/bin/sh
IFS= read -r line
printf '%s' "\$line" > "$work/native-input.txt"
SH
  chmod +x "$work/native-reader.sh"
  setsid env -u DISPLAY WAYLAND_DISPLAY="$socket" SHELL="$work/native-reader.sh" weston-terminal \
    > "$mode_artifacts/weston-terminal.log" 2>&1 &
  app_pid=$!
  sleep 2
  env -u DISPLAY WAYLAND_DISPLAY="$socket" wtype -d 100 'kwin-native-wayland-input' \
    > "$mode_artifacts/wtype.log" 2>&1
  env -u DISPLAY WAYLAND_DISPLAY="$socket" wtype -k Return >> "$mode_artifacts/wtype.log" 2>&1
  for _ in {1..100}; do
    [[ -s "$work/native-input.txt" ]] && break
    sleep 0.1
  done
  [[ "$(cat "$work/native-input.txt")" == "kwin-native-wayland-input" ]]
  echo 'virtual_keyboard_input=passed' > "$mode_artifacts/native-input.txt"
  kill -- "-$app_pid" >/dev/null 2>&1 || true
  wait "$app_pid" >/dev/null 2>&1 || true
  app_pid=""

  capture="$work/captures/fullscreen.png"
  env -u DISPLAY WAYLAND_DISPLAY="$socket" timeout 30s spectacle -b -n -f -o "$capture" \
    > "$mode_artifacts/spectacle.log" 2>&1
  python3 - "$capture" "$mode_artifacts/capture.txt" <<'PY'
import pathlib
import struct
import sys

capture, output = sys.argv[1:]
data = pathlib.Path(capture).read_bytes()
if data[:8] != b"\x89PNG\r\n\x1a\n" or len(data) < 32:
    raise SystemExit("Spectacle did not produce a valid PNG")
width, height = struct.unpack(">II", data[16:24])
if width < 1 or height < 1:
    raise SystemExit("Spectacle produced an empty image")
pathlib.Path(output).write_text(
    f"capture=passed\npng_bytes={len(data)}\nwidth={width}\nheight={height}\n",
    encoding="utf-8",
)
PY
  rm -f "$capture"
  echo "$mode: readiness, XWayland, native input, and native capture passed"
)

modes=("$@")
if [[ "${#modes[@]}" -eq 0 ]]; then
  modes=(virtual nested-xvfb)
fi
status=0
for mode in "${modes[@]}"; do
  echo "== KWin $mode mode =="
  if run_mode "$mode"; then
    record_result "$mode" passed
  else
    mode_status=$?
    record_result "$mode" "failed:$mode_status"
    echo "$mode mode failed; see $ARTIFACT_DIR/$mode" >&2
    status=1
  fi
done
exit "$status"
