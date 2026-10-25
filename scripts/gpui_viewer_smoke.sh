#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${BIN:-$ROOT_DIR/target/debug/agent-workspace-linux}"
VIEWER_SMOKE_SUMMARY_PATH="${VIEWER_SMOKE_SUMMARY_PATH:-}"

need() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "missing required command: $1" >&2
    exit 1
  fi
}

need jq
need python3
need xclock
need xdotool
need xwininfo
need xprop
need xwd
need convert
need identify

cargo build --manifest-path "$ROOT_DIR/Cargo.toml" >/dev/null

SMOKE_DIR="$(mktemp -d)"
VIEWER_CONFIG_HOME="$SMOKE_DIR/config"
VIEWER_RUNTIME_DIR="$SMOKE_DIR/runtime"
WORKSPACE_ID="gpui-viewer-smoke-$$"
COMPANION_WORKSPACE_ID="${WORKSPACE_ID}-stopped"
VIEWER_PID=""
mkdir -m 700 -p "$VIEWER_RUNTIME_DIR"
export XDG_RUNTIME_DIR="$VIEWER_RUNTIME_DIR"

cleanup() {
  exit_code=$?
  if [[ -n "$VIEWER_PID" ]]; then
    kill "$VIEWER_PID" >/dev/null 2>&1 || true
    wait "$VIEWER_PID" >/dev/null 2>&1 || true
  fi
  "$BIN" workspace stop --id "$WORKSPACE_ID" --timeout-ms 15000 >/dev/null 2>&1 || true
  "$BIN" workspace stop --id "$COMPANION_WORKSPACE_ID" --timeout-ms 15000 >/dev/null 2>&1 || true
  "$BIN" workspace cleanup --id "$WORKSPACE_ID" >/dev/null 2>&1 || true
  "$BIN" workspace cleanup --id "$COMPANION_WORKSPACE_ID" >/dev/null 2>&1 || true
  if [[ "$exit_code" -eq 0 ]]; then
    rm -rf "$SMOKE_DIR"
  else
    echo "GPUI viewer smoke failed; preserved temp dir: $SMOKE_DIR" >&2
  fi
}
trap cleanup EXIT

echo "== start hidden workspace =="
"$BIN" workspace start \
  --id "$WORKSPACE_ID" \
  --ack-hidden-workspace \
  --purpose "GPUI viewer smoke" \
  --width 800 \
  --height 500 \
  > "$SMOKE_DIR/start.json"
jq -e '.ok == true and .status.ready == true' "$SMOKE_DIR/start.json" >/dev/null
WORKSPACE_RUNTIME_DIR="$(jq -r '.status.runtime_dir' "$SMOKE_DIR/start.json")"

echo "== prepare stopped companion workspace =="
"$BIN" workspace start \
  --id "$COMPANION_WORKSPACE_ID" \
  --ack-hidden-workspace \
  --purpose "GPUI viewer workspace switch smoke" \
  --width 640 \
  --height 420 \
  > "$SMOKE_DIR/companion-start.json"
jq -e '.ok == true and .status.ready == true' "$SMOKE_DIR/companion-start.json" >/dev/null
"$BIN" workspace stop \
  --id "$COMPANION_WORKSPACE_ID" \
  --timeout-ms 15000 \
  > "$SMOKE_DIR/companion-stop.json"
jq -e '.ok == true' "$SMOKE_DIR/companion-stop.json" >/dev/null

echo "== launch xclock =="
"$BIN" workspace launch \
  --id "$WORKSPACE_ID" \
  --name xclock \
  --wait-window \
  --window-timeout-ms 10000 \
  -- xclock \
  > "$SMOKE_DIR/launch.json"
jq -e '.ok == true and (.windows | length) >= 1 and .apps[0].name == "xclock"' "$SMOKE_DIR/launch.json" >/dev/null

echo "== prove window screenshot path =="
"$BIN" workspace screenshot-window \
  --id "$WORKSPACE_ID" \
  --app xclock \
  --output "$SMOKE_DIR/xclock-window.png" \
  --timeout-ms 10000 \
  > "$SMOKE_DIR/window-shot.json"
jq -e '.ok == true and .screenshot.width > 0 and .screenshot.height > 0 and .screenshot.bytes > 0' "$SMOKE_DIR/window-shot.json" >/dev/null
test -s "$SMOKE_DIR/xclock-window.png"

echo "== prove app log path =="
"$BIN" workspace logs \
  --id "$WORKSPACE_ID" \
  --stream stdout \
  xclock \
  > "$SMOKE_DIR/app-log.json"
jq -e '.ok == true and .app_log.stream == "stdout" and (.app_log.path | endswith(".stdout.log"))' "$SMOKE_DIR/app-log.json" >/dev/null

echo "== prove event artifact path =="
"$BIN" workspace artifacts \
  --id "$WORKSPACE_ID" \
  --existing \
  > "$SMOKE_DIR/artifacts.json"
jq -e '.ok == true and any(.files[]; .kind == "event_log" and .exists == true)' "$SMOKE_DIR/artifacts.json" >/dev/null

echo "== prove observe screenshot reuse =="
"$BIN" workspace observe \
  --id "$WORKSPACE_ID" \
  --screenshot \
  --all-windows \
  --events-tail 2 \
  > "$SMOKE_DIR/observe-shot-1.json"
jq -e '.ok == true and (.screenshot.path | endswith("/observe-frame.png")) and .screenshot.source == "workspace_observe" and .screenshot.target == "root"' "$SMOKE_DIR/observe-shot-1.json" >/dev/null
test -s "$WORKSPACE_RUNTIME_DIR/observe-frame.png"
"$BIN" workspace observe \
  --id "$WORKSPACE_ID" \
  --screenshot \
  --all-windows \
  --events-tail 2 \
  > "$SMOKE_DIR/observe-shot-2.json"
jq -e '.ok == true and (.screenshot.path | endswith("/observe-frame.png")) and .screenshot.source == "workspace_observe" and .screenshot.target == "root"' "$SMOKE_DIR/observe-shot-2.json" >/dev/null
OBSERVE_ROOT_SCREENSHOTS="$(find "$WORKSPACE_RUNTIME_DIR" -maxdepth 1 -type f -name 'screenshot-*.png' | wc -l)"
if [[ "$OBSERVE_ROOT_SCREENSHOTS" -ne 0 ]]; then
  echo "observe should reuse observe-frame.png instead of accumulating root screenshots" >&2
  find "$WORKSPACE_RUNTIME_DIR" -maxdepth 1 -type f -name 'screenshot-*.png' -print >&2
  exit 1
fi

echo "== open GPUI viewer through X11/Xwayland =="
mkdir -p "$VIEWER_CONFIG_HOME/agent-workspace-linux"
mkdir -m 700 -p "$VIEWER_RUNTIME_DIR"
cat > "$VIEWER_CONFIG_HOME/agent-workspace-linux/viewer.json" <<'JSON'
{
  "width": 380.0,
  "height": 340.0,
  "screen_stream": true,
  "footer_mode": "task",
  "x": 64.0,
  "y": 72.0
}
JSON
jq -e '.footer_mode == "task"' "$VIEWER_CONFIG_HOME/agent-workspace-linux/viewer.json" >/dev/null
XDG_CONFIG_HOME="$VIEWER_CONFIG_HOME" XDG_RUNTIME_DIR="$VIEWER_RUNTIME_DIR" AGENT_WORKSPACE_VIEWER_BACKEND=x11 "$BIN" viewer --id "$WORKSPACE_ID" > "$SMOKE_DIR/viewer.out.log" 2> "$SMOKE_DIR/viewer.err.log" &
VIEWER_PID=$!
sleep 2
if ! kill -0 "$VIEWER_PID" >/dev/null 2>&1; then
  echo "viewer exited early" >&2
  cat "$SMOKE_DIR/viewer.out.log" >&2 || true
  cat "$SMOKE_DIR/viewer.err.log" >&2 || true
  exit 1
fi

VIEWER_WINDOW="$(xdotool search --pid "$VIEWER_PID" 2>/dev/null | tail -n 1 || true)"
if [[ -z "$VIEWER_WINDOW" ]]; then
  VIEWER_WINDOW="$(xdotool search --class agent-workspace-linux-viewer 2>/dev/null | tail -n 1 || true)"
fi
if [[ -z "$VIEWER_WINDOW" ]]; then
  echo "viewer X11 window not found" >&2
  xwininfo -root -tree >&2 || true
  exit 1
fi

xprop -id "$VIEWER_WINDOW" WM_CLASS > "$SMOKE_DIR/viewer-wm-class.txt"
xprop -id "$VIEWER_WINDOW" _NET_WM_STATE > "$SMOKE_DIR/viewer-wm-state.txt"
xprop -id "$VIEWER_WINDOW" _NET_WM_WINDOW_TYPE > "$SMOKE_DIR/viewer-window-type.txt"
xprop -id "$VIEWER_WINDOW" WM_CLASS | grep -q "agent-workspace-linux-viewer"
grep -q "_NET_WM_STATE_SKIP_TASKBAR" "$SMOKE_DIR/viewer-wm-state.txt"
grep -q "_NET_WM_STATE_SKIP_PAGER" "$SMOKE_DIR/viewer-wm-state.txt"
if grep -Eq "_NET_WM_STATE_(ABOVE|STICKY)" "$SMOKE_DIR/viewer-wm-state.txt"; then
  echo "viewer default should not request always-on-top X11 state" >&2
  exit 1
fi
grep -Eq "_NET_WM_WINDOW_TYPE_(NOTIFICATION|UTILITY)" "$SMOKE_DIR/viewer-window-type.txt"

echo "== prove duplicate viewer launch reuses existing instance =="
XDG_CONFIG_HOME="$VIEWER_CONFIG_HOME" XDG_RUNTIME_DIR="$VIEWER_RUNTIME_DIR" AGENT_WORKSPACE_VIEWER_BACKEND=x11 "$BIN" viewer --id "$WORKSPACE_ID" > "$SMOKE_DIR/duplicate-viewer.out.log" 2> "$SMOKE_DIR/duplicate-viewer.err.log"
grep -q "already running" "$SMOKE_DIR/duplicate-viewer.err.log"
VIEWER_WINDOW_COUNT="$(xdotool search --pid "$VIEWER_PID" 2>/dev/null | wc -l)"
if [[ "$VIEWER_WINDOW_COUNT" -ne 1 ]]; then
  echo "duplicate viewer launch should leave exactly one window for viewer pid $VIEWER_PID; found $VIEWER_WINDOW_COUNT" >&2
  xdotool search --pid "$VIEWER_PID" >&2 || true
  exit 1
fi

xwininfo -id "$VIEWER_WINDOW" > "$SMOKE_DIR/viewer-window.txt"
VIEWER_X="$(awk '/Absolute upper-left X:/ { print $4 }' "$SMOKE_DIR/viewer-window.txt")"
VIEWER_Y="$(awk '/Absolute upper-left Y:/ { print $4 }' "$SMOKE_DIR/viewer-window.txt")"
xwd -silent -id "$VIEWER_WINDOW" | convert xwd:- "$SMOKE_DIR/viewer.png"
identify -format '%w %h %[fx:mean]\n' "$SMOKE_DIR/viewer.png" > "$SMOKE_DIR/viewer-image.txt"
test -s "$WORKSPACE_RUNTIME_DIR/viewer-frame.png"
"$BIN" workspace events \
  --id "$WORKSPACE_ID" \
  --tail 20 \
  > "$SMOKE_DIR/viewer-events.json"
jq -e 'any(.events[]; .kind == "screenshot" and .detail.source == "viewer_stream" and .detail.target == "root" and (.detail.path | endswith("/viewer-frame.png")))' "$SMOKE_DIR/viewer-events.json" >/dev/null
VIEWER_ROOT_SCREENSHOTS="$(find "$WORKSPACE_RUNTIME_DIR" -maxdepth 1 -type f -name 'screenshot-*.png' | wc -l)"
if [[ "$VIEWER_ROOT_SCREENSHOTS" -ne 0 ]]; then
  echo "viewer should reuse viewer-frame.png instead of accumulating root screenshots" >&2
  find "$WORKSPACE_RUNTIME_DIR" -maxdepth 1 -type f -name 'screenshot-*.png' -print >&2
  exit 1
fi

python3 - "$SMOKE_DIR/viewer-image.txt" "$SMOKE_DIR/viewer-window.txt" <<'PY'
import pathlib
import re
import sys

target_width = 380
target_height = 340
width, height, mean = pathlib.Path(sys.argv[1]).read_text().strip().split()
width = int(width)
height = int(height)
mean = float(mean)
window_info = pathlib.Path(sys.argv[2]).read_text()
x_match = re.search(r"Absolute upper-left X:\s+(-?\d+)", window_info)
y_match = re.search(r"Absolute upper-left Y:\s+(-?\d+)", window_info)
if not x_match or not y_match:
    raise SystemExit("viewer xwininfo output did not include absolute position")
x = int(x_match.group(1))
y = int(y_match.group(1))
scale_x = width / target_width
scale_y = height / target_height
if abs(scale_x - scale_y) > 0.15 or not (0.75 <= scale_x <= 3.25):
    raise SystemExit(f"viewer size has unexpected scale: {width}x{height}")
scale = (scale_x + scale_y) / 2
logical_width = width / scale
logical_height = height / scale
logical_x = x / scale
logical_y = y / scale
if not (370 <= logical_width <= 410 and 320 <= logical_height <= 370):
    raise SystemExit(
        f"viewer did not honor seeded compact size: {width}x{height} "
        f"(logical {logical_width:.1f}x{logical_height:.1f}, scale {scale:.2f})"
    )
if not (44 <= logical_x <= 84 and 52 <= logical_y <= 92):
    raise SystemExit(
        f"viewer did not honor seeded position: {x},{y} "
        f"(logical {logical_x:.1f},{logical_y:.1f}, scale {scale:.2f})"
    )
if mean <= 0.01:
    raise SystemExit(f"viewer image appears blank: mean={mean}")
PY

kill "$VIEWER_PID" >/dev/null 2>&1 || true
wait "$VIEWER_PID" >/dev/null 2>&1 || true
VIEWER_PID=""

echo "== open opt-in topmost GPUI viewer through X11/Xwayland =="
XDG_CONFIG_HOME="$VIEWER_CONFIG_HOME" XDG_RUNTIME_DIR="$VIEWER_RUNTIME_DIR" AGENT_WORKSPACE_VIEWER_BACKEND=x11 "$BIN" viewer --id "$WORKSPACE_ID" --always-on-top > "$SMOKE_DIR/topmost-viewer.out.log" 2> "$SMOKE_DIR/topmost-viewer.err.log" &
VIEWER_PID=$!
sleep 2
if ! kill -0 "$VIEWER_PID" >/dev/null 2>&1; then
  echo "topmost viewer exited early" >&2
  cat "$SMOKE_DIR/topmost-viewer.out.log" >&2 || true
  cat "$SMOKE_DIR/topmost-viewer.err.log" >&2 || true
  exit 1
fi

TOPMOST_WINDOW="$(xdotool search --pid "$VIEWER_PID" 2>/dev/null | tail -n 1 || true)"
if [[ -z "$TOPMOST_WINDOW" ]]; then
  TOPMOST_WINDOW="$(xdotool search --class agent-workspace-linux-viewer 2>/dev/null | tail -n 1 || true)"
fi
if [[ -z "$TOPMOST_WINDOW" ]]; then
  echo "topmost viewer X11 window not found" >&2
  xwininfo -root -tree >&2 || true
  exit 1
fi

xprop -id "$TOPMOST_WINDOW" WM_CLASS > "$SMOKE_DIR/topmost-viewer-wm-class.txt"
xprop -id "$TOPMOST_WINDOW" _NET_WM_STATE > "$SMOKE_DIR/topmost-viewer-wm-state.txt"
xprop -id "$TOPMOST_WINDOW" _NET_WM_WINDOW_TYPE > "$SMOKE_DIR/topmost-viewer-window-type.txt"
grep -q "agent-workspace-linux-viewer" "$SMOKE_DIR/topmost-viewer-wm-class.txt"
grep -q "_NET_WM_STATE_SKIP_TASKBAR" "$SMOKE_DIR/topmost-viewer-wm-state.txt"
grep -q "_NET_WM_STATE_SKIP_PAGER" "$SMOKE_DIR/topmost-viewer-wm-state.txt"
grep -q "_NET_WM_STATE_ABOVE" "$SMOKE_DIR/topmost-viewer-wm-state.txt"
grep -q "_NET_WM_STATE_STICKY" "$SMOKE_DIR/topmost-viewer-wm-state.txt"
grep -q "_NET_WM_WINDOW_TYPE_NOTIFICATION" "$SMOKE_DIR/topmost-viewer-window-type.txt"
grep -q "_NET_WM_WINDOW_TYPE_UTILITY" "$SMOKE_DIR/topmost-viewer-window-type.txt"
xwd -silent -id "$TOPMOST_WINDOW" | convert xwd:- "$SMOKE_DIR/topmost-viewer.png"
identify -format '%w %h %[fx:mean]\n' "$SMOKE_DIR/topmost-viewer.png" > "$SMOKE_DIR/topmost-viewer-image.txt"

python3 - "$SMOKE_DIR/topmost-viewer-image.txt" <<'PY'
import pathlib
import sys

target_width = 380
target_height = 340
width, height, mean = pathlib.Path(sys.argv[1]).read_text().strip().split()
width = int(width)
height = int(height)
mean = float(mean)
scale_x = width / target_width
scale_y = height / target_height
if abs(scale_x - scale_y) > 0.15 or not (0.75 <= scale_x <= 3.25):
    raise SystemExit(f"topmost viewer size has unexpected scale: {width}x{height}")
scale = (scale_x + scale_y) / 2
logical_width = width / scale
logical_height = height / scale
if not (370 <= logical_width <= 410 and 320 <= logical_height <= 370):
    raise SystemExit(
        f"topmost viewer did not honor seeded compact size: {width}x{height} "
        f"(logical {logical_width:.1f}x{logical_height:.1f}, scale {scale:.2f})"
    )
if mean <= 0.01:
    raise SystemExit(f"topmost viewer image appears blank: mean={mean}")
PY

kill "$VIEWER_PID" >/dev/null 2>&1 || true
wait "$VIEWER_PID" >/dev/null 2>&1 || true
VIEWER_PID=""

echo "== prove MCP-bound viewer exits when workspace runtime is removed =="
XDG_CONFIG_HOME="$VIEWER_CONFIG_HOME" XDG_RUNTIME_DIR="$VIEWER_RUNTIME_DIR" AGENT_WORKSPACE_VIEWER_BACKEND=x11 "$BIN" viewer --id "$WORKSPACE_ID" --exit-when-workspace-gone > "$SMOKE_DIR/bound-viewer.out.log" 2> "$SMOKE_DIR/bound-viewer.err.log" &
VIEWER_PID=$!
sleep 2
if ! kill -0 "$VIEWER_PID" >/dev/null 2>&1; then
  echo "target-bound viewer exited before workspace removal" >&2
  cat "$SMOKE_DIR/bound-viewer.out.log" >&2 || true
  cat "$SMOKE_DIR/bound-viewer.err.log" >&2 || true
  exit 1
fi
"$BIN" workspace stop --id "$WORKSPACE_ID" --timeout-ms 15000 > "$SMOKE_DIR/bound-workspace-stop.json"
jq -e '.ok == true' "$SMOKE_DIR/bound-workspace-stop.json" >/dev/null
"$BIN" workspace cleanup --id "$WORKSPACE_ID" > "$SMOKE_DIR/bound-workspace-cleanup.json"
jq -e '.dry_run == false and (.removed | length) == 1' "$SMOKE_DIR/bound-workspace-cleanup.json" >/dev/null
for _ in $(seq 1 50); do
  if ! kill -0 "$VIEWER_PID" >/dev/null 2>&1; then
    wait "$VIEWER_PID" >/dev/null 2>&1 || true
    VIEWER_PID=""
    break
  fi
  sleep 0.2
done
if [[ -n "$VIEWER_PID" ]]; then
  echo "target-bound viewer should exit after workspace runtime is removed" >&2
  cat "$SMOKE_DIR/bound-viewer.out.log" >&2 || true
  cat "$SMOKE_DIR/bound-viewer.err.log" >&2 || true
  exit 1
fi

if [[ -n "$VIEWER_SMOKE_SUMMARY_PATH" ]]; then
  mkdir -p "$(dirname "$VIEWER_SMOKE_SUMMARY_PATH")"
  python3 - "$VIEWER_SMOKE_SUMMARY_PATH" "$VIEWER_WINDOW" "$TOPMOST_WINDOW" "$VIEWER_WINDOW_COUNT" "$SMOKE_DIR" <<'PY'
import json
import pathlib
import sys

summary_path = pathlib.Path(sys.argv[1])
viewer_window = sys.argv[2]
topmost_window = sys.argv[3]
viewer_window_count = int(sys.argv[4])
smoke_dir = pathlib.Path(sys.argv[5])


def read_text(name):
    path = smoke_dir / name
    return path.read_text(encoding="utf-8", errors="replace").strip()


summary = {
    "schema": "agent-workspace-linux.gpui_viewer_smoke_summary.v1",
    "viewer_backend_forced": "x11",
    "x11_xwayland_window_observed": True,
    "protocol_evidence": {
        "default_window_id": viewer_window,
        "topmost_window_id": topmost_window,
        "default_wm_class": read_text("viewer-wm-class.txt"),
        "default_wm_state": read_text("viewer-wm-state.txt"),
        "default_window_type": read_text("viewer-window-type.txt"),
        "topmost_wm_class": read_text("topmost-viewer-wm-class.txt"),
        "topmost_wm_state": read_text("topmost-viewer-wm-state.txt"),
        "topmost_window_type": read_text("topmost-viewer-window-type.txt"),
    },
    "default_viewer": {
        "skip_taskbar": "_NET_WM_STATE_SKIP_TASKBAR" in read_text("viewer-wm-state.txt"),
        "skip_pager": "_NET_WM_STATE_SKIP_PAGER" in read_text("viewer-wm-state.txt"),
        "above": "_NET_WM_STATE_ABOVE" in read_text("viewer-wm-state.txt"),
        "sticky": "_NET_WM_STATE_STICKY" in read_text("viewer-wm-state.txt"),
        "notification_or_utility": any(
            needle in read_text("viewer-window-type.txt")
            for needle in ["_NET_WM_WINDOW_TYPE_NOTIFICATION", "_NET_WM_WINDOW_TYPE_UTILITY"]
        ),
        "image_probe": read_text("viewer-image.txt"),
    },
    "duplicate_launch": {
        "reused_existing_instance": True,
        "window_count_for_original_pid": viewer_window_count,
    },
    "topmost_viewer": {
        "skip_taskbar": "_NET_WM_STATE_SKIP_TASKBAR" in read_text("topmost-viewer-wm-state.txt"),
        "skip_pager": "_NET_WM_STATE_SKIP_PAGER" in read_text("topmost-viewer-wm-state.txt"),
        "above": "_NET_WM_STATE_ABOVE" in read_text("topmost-viewer-wm-state.txt"),
        "sticky": "_NET_WM_STATE_STICKY" in read_text("topmost-viewer-wm-state.txt"),
        "notification": "_NET_WM_WINDOW_TYPE_NOTIFICATION" in read_text("topmost-viewer-window-type.txt"),
        "utility": "_NET_WM_WINDOW_TYPE_UTILITY" in read_text("topmost-viewer-window-type.txt"),
        "image_probe": read_text("topmost-viewer-image.txt"),
    },
    "target_bound_viewer_exited_after_workspace_cleanup": True,
}

summary_path.write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8")
PY
fi

echo "GPUI viewer smoke passed"
