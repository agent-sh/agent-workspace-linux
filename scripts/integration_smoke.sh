#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${BIN:-$ROOT_DIR/target/debug/agent-workspace-linux}"

need() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "missing required command: $1" >&2
    exit 1
  fi
}

need jq
need python3
need xmessage

cargo build --manifest-path "$ROOT_DIR/Cargo.toml" >/dev/null

if command -v node >/dev/null 2>&1; then
  echo "== mcp permissions smoke =="
  AGENT_WORKSPACE_BIN="$BIN" node "$ROOT_DIR/scripts/mcp_permissions_smoke.js"
  echo "== non-headless mcp viewer smoke =="
  AGENT_WORKSPACE_BIN="$BIN" node "$ROOT_DIR/scripts/mcp_non_headless_viewer_smoke.js"
  echo "== no-host-display mcp viewer smoke =="
  AGENT_WORKSPACE_BIN="$BIN" node "$ROOT_DIR/scripts/mcp_no_host_display_viewer_smoke.js"
  echo "== clean mcp permissions smoke =="
  AGENT_WORKSPACE_BIN="$BIN" node "$ROOT_DIR/scripts/mcp_clean_permissions_smoke.js"
  echo "== terminal TUI mcp smoke =="
  AGENT_WORKSPACE_BIN="$BIN" "$ROOT_DIR/scripts/terminal_lifecycle_smoke.sh"
else
  echo "== mcp permissions smoke skipped: node not found =="
fi

BROWSER_BIN="${BROWSER_BIN:-}"
if [[ -z "$BROWSER_BIN" ]]; then
  BROWSER_BIN="$(command -v google-chrome || command -v google-chrome-stable || command -v chromium || command -v chromium-browser || true)"
fi
GUI_EDITOR_BIN="${GUI_EDITOR_BIN:-}"
if [[ -z "$GUI_EDITOR_BIN" ]]; then
  GUI_EDITOR_BIN="$(command -v gnome-text-editor || command -v gedit || command -v xed || true)"
fi

SMOKE_DIR="$(mktemp -d)"
CONFIG_DIR="$SMOKE_DIR/config"
RUNTIME_DIR="$SMOKE_DIR/runtime"
mkdir -p "$CONFIG_DIR" "$RUNTIME_DIR"

WORKSPACE_IDS=()
STALE_CLEANUP_IDS=()

run_awl() {
  XDG_CONFIG_HOME="$CONFIG_DIR" XDG_RUNTIME_DIR="$RUNTIME_DIR" "$BIN" "$@"
}

cleanup() {
  exit_code=$?
  for workspace_id in "${WORKSPACE_IDS[@]:-}"; do
    run_awl workspace stop --id "$workspace_id" >/dev/null 2>&1 || true
  done
  for workspace_id in "${STALE_CLEANUP_IDS[@]:-}"; do
    run_awl workspace cleanup --id "$workspace_id" >/dev/null 2>&1 || true
  done
  if [[ "$exit_code" -eq 0 ]]; then
    rm -rf "$SMOKE_DIR"
  else
    echo "integration smoke failed; preserved temp dir: $SMOKE_DIR" >&2
  fi
}
trap cleanup EXIT

assert_json() {
  local filter="$1"
  local file="$2"
  shift 2
  jq -e "$@" "$filter" "$file" >/dev/null
}

expect_awl_failure() {
  local output_file="$1"
  shift
  if run_awl "$@" >"$output_file" 2>&1; then
    echo "expected command to fail: $*" >&2
    cat "$output_file" >&2
    return 1
  fi
}

pid_alive() {
  local pid="$1"
  [[ "$pid" =~ ^[0-9]+$ ]] && [[ "$pid" -gt 0 ]] && kill -0 "$pid" 2>/dev/null
}

pgid_alive() {
  local pgid="$1"
  [[ "$pgid" =~ ^[0-9]+$ ]] && [[ "$pgid" -gt 0 ]] || return 1
  ps -eo pgid= | tr -d ' ' | grep -qx "$pgid"
}

wait_pid_gone() {
  local label="$1"
  local pid="$2"
  [[ -n "$pid" && "$pid" != "null" ]] || return 0
  for _ in {1..40}; do
    if ! pid_alive "$pid"; then
      return 0
    fi
    sleep 0.1
  done
  echo "$label pid $pid is still running" >&2
  return 1
}

wait_pgid_gone() {
  local label="$1"
  local pgid="$2"
  [[ -n "$pgid" && "$pgid" != "null" ]] || return 0
  for _ in {1..40}; do
    if ! pgid_alive "$pgid"; then
      return 0
    fi
    sleep 0.1
  done
  echo "$label process group $pgid is still running" >&2
  return 1
}

echo "== cli permission ceiling smoke =="
CLI_PERMISSIONS="$SMOKE_DIR/cli-permissions.json"
CLI_OPEN_PROFILE="$SMOKE_DIR/cli-open-profile.json"
CLI_LOCKED_PROFILE="$SMOKE_DIR/cli-locked-profile.json"
run_awl permissions template closed --app sh > "$CLI_PERMISSIONS"
run_awl permissions validate --json "$CLI_PERMISSIONS" > "$SMOKE_DIR/cli-permissions-validate.json"
assert_json '.configured == true and .restricted == true and .ceiling.network.mode == "disabled" and .ceiling.apps.allow[0] == "sh"' "$SMOKE_DIR/cli-permissions-validate.json"
cat > "$CLI_OPEN_PROFILE" <<'JSON'
{
  "id": "cli-too-open",
  "network": { "mode": "inherit_host" },
  "mounts": [],
  "setup_commands": [],
  "startup_apps": []
}
JSON
cat > "$CLI_LOCKED_PROFILE" <<'JSON'
{
  "id": "cli-locked",
  "network": { "mode": "disabled" },
  "mounts": [],
  "setup_commands": [],
  "startup_apps": [
    { "command": ["sh", "-lc", "true"] }
  ]
}
JSON
expect_awl_failure "$SMOKE_DIR/cli-open-profile.err" --permissions "$CLI_PERMISSIONS" profile validate --json "$CLI_OPEN_PROFILE"
grep -q "exceeds MCP permission ceiling" "$SMOKE_DIR/cli-open-profile.err"
run_awl --permissions "$CLI_PERMISSIONS" profile put --json "$CLI_LOCKED_PROFILE" --dry-run > "$SMOKE_DIR/cli-locked-profile-put.json"
assert_json '.ok == true and .dry_run == true and .would_create == true' "$SMOKE_DIR/cli-locked-profile-put.json"

echo "== doctor =="
run_awl doctor > "$SMOKE_DIR/doctor.json"
assert_json '.ready_for_x11_workspace == true' "$SMOKE_DIR/doctor.json"

echo "== profile import/export =="
IMPORT_PROFILE="$SMOKE_DIR/import-profile.json"
jq -n '{
  id: "import-smoke",
  cwd: "/tmp",
  env: [{name:"ONE", value:"1"}],
  startup_apps: [{name:"noop", command:["/bin/true"], cwd:"/tmp"}]
}' > "$IMPORT_PROFILE"
run_awl profile validate --json "$IMPORT_PROFILE" > "$SMOKE_DIR/import-validate.json"
assert_json '.ok == true and .profile.id == "import-smoke" and .check.requires_hidden_workspace_ack == true' "$SMOKE_DIR/import-validate.json"
INVALID_PROFILE="$SMOKE_DIR/invalid-profile.json"
jq -n '{id:"invalid-profile", cwd:"relative"}' > "$INVALID_PROFILE"
if run_awl profile validate --json "$INVALID_PROFILE" > "$SMOKE_DIR/invalid-validate.json" 2> "$SMOKE_DIR/invalid-validate.err"; then
  echo "invalid profile validate unexpectedly succeeded" >&2
  exit 1
fi
grep -q "must be absolute" "$SMOKE_DIR/invalid-validate.err"
run_awl profile import --json "$IMPORT_PROFILE" --dry-run > "$SMOKE_DIR/import-dry.json"
assert_json '.dry_run == true and .would_create == true and .saved == false' "$SMOKE_DIR/import-dry.json"
run_awl profile import --json "$IMPORT_PROFILE" > "$SMOKE_DIR/import.json"
assert_json '.created == true and .saved == true' "$SMOKE_DIR/import.json"
if run_awl profile import --json "$IMPORT_PROFILE" > "$SMOKE_DIR/import-duplicate.json" 2> "$SMOKE_DIR/import-duplicate.err"; then
  echo "duplicate profile import unexpectedly succeeded" >&2
  exit 1
fi
grep -q "already exists" "$SMOKE_DIR/import-duplicate.err"
mkdir -p "$SMOKE_DIR/export-dir"
run_awl profile export import-smoke --output "$SMOKE_DIR/export-dir" > "$SMOKE_DIR/export.json"
assert_json '.wrote == true and .output_path == $path' "$SMOKE_DIR/export.json" --arg path "$SMOKE_DIR/export-dir/import-smoke.json"
test -f "$SMOKE_DIR/export-dir/import-smoke.json"
run_awl profile delete --dry-run import-smoke > "$SMOKE_DIR/profile-delete-dry.json"
assert_json '.dry_run == true and .would_delete == true and .deleted == false and .profile.id == "import-smoke"' "$SMOKE_DIR/profile-delete-dry.json"
run_awl profile delete import-smoke > "$SMOKE_DIR/profile-delete.json"
assert_json '.dry_run == false and .would_delete == true and .deleted == true and .profile.id == "import-smoke"' "$SMOKE_DIR/profile-delete.json"
run_awl profile list > "$SMOKE_DIR/profile-list-after-delete.json"
assert_json 'all(.profiles[]; .id != "import-smoke")' "$SMOKE_DIR/profile-list-after-delete.json"
CHROME_TEMPLATE_ARGS=(restricted-chrome --id restricted-chrome-smoke)
EXPECTED_BROWSER_BIN="${BROWSER_BIN:-google-chrome}"
if [[ -n "$BROWSER_BIN" ]]; then
  CHROME_TEMPLATE_ARGS+=(--browser-path "$BROWSER_BIN")
fi
run_awl profile template "${CHROME_TEMPLATE_ARGS[@]}" > "$SMOKE_DIR/restricted-chrome-template.json"
assert_json '.id == "restricted-chrome-smoke" and .network.mode == "disabled" and .require_enforced_policy == true and (.description | contains("--no-sandbox")) and .startup_apps[0].command[0] == $browser and (.startup_apps[0].command | index("--no-sandbox"))' "$SMOKE_DIR/restricted-chrome-template.json" --arg browser "$EXPECTED_BROWSER_BIN"
run_awl profile validate --json "$SMOKE_DIR/restricted-chrome-template.json" > "$SMOKE_DIR/restricted-chrome-template-validate.json"
assert_json '.ok == true and .profile.id == "restricted-chrome-smoke" and .check.applied_policy.enforcement.network.enforced == true' "$SMOKE_DIR/restricted-chrome-template-validate.json"
BROWSER_SESSION_DATA="$SMOKE_DIR/browser-session-data"
mkdir -p "$BROWSER_SESSION_DATA"
BROWSER_SESSION_ARGS=(browser-session --id browser-session-smoke --user-data-dir "$BROWSER_SESSION_DATA")
if [[ -n "$BROWSER_BIN" ]]; then
  BROWSER_SESSION_ARGS+=(--browser-path "$BROWSER_BIN")
fi
run_awl profile template "${BROWSER_SESSION_ARGS[@]}" > "$SMOKE_DIR/browser-session-template.json"
assert_json '.id == "browser-session-smoke" and .network.mode == "inherit_host" and .require_enforced_policy == true and (.description | contains("explicit user approval")) and .mounts[0].workspace_path == "/workspace/browser-user-data" and .mounts[0].mode == "read_write" and .startup_apps[0].command[0] == $browser and (.startup_apps[0].command | index("--no-sandbox")) and (.startup_apps[0].command | index("--user-data-dir=/workspace/browser-user-data"))' "$SMOKE_DIR/browser-session-template.json" --arg browser "$EXPECTED_BROWSER_BIN"
run_awl profile validate --json "$SMOKE_DIR/browser-session-template.json" > "$SMOKE_DIR/browser-session-template-validate.json"
assert_json '.ok == true and .profile.id == "browser-session-smoke" and .check.applied_policy.enforcement.mounts.enforced == true and .check.applied_policy.enforcement.network.state == "not_requested"' "$SMOKE_DIR/browser-session-template-validate.json"
if [[ -n "$BROWSER_BIN" ]]; then
  run_awl profile import --json "$SMOKE_DIR/browser-session-template.json" > "$SMOKE_DIR/browser-session-import.json"
  assert_json '.created == true and .saved == true' "$SMOKE_DIR/browser-session-import.json"
  BROWSER_SESSION_ID="browser-session-open-smoke-$$"
  WORKSPACE_IDS+=("$BROWSER_SESSION_ID")
  run_awl workspace open-profile --ack-hidden-workspace --profile browser-session-smoke --id "$BROWSER_SESSION_ID" --purpose "Browser session smoke" --startup-wait-window --startup-screenshot-window --startup-window-timeout-ms 20000 > "$SMOKE_DIR/browser-session-open.json"
  assert_json '.ready == true and .startup_launched == true and .startup.launched[0].ok == true and (.startup.launched[0].screenshot.bytes > 0) and .startup.launched[0].apps[0].profile_id == "browser-session-smoke"' "$SMOKE_DIR/browser-session-open.json"
  run_awl workspace observe --id "$BROWSER_SESSION_ID" --screenshot --events --events-tail 20 > "$SMOKE_DIR/browser-session-observe.json"
  assert_json '(.screenshot.bytes > 0) and (.active_window.title | length > 0) and (.apps[] | select(.name == "browser-session-no-sandbox" and .running == true and .profile_id == "browser-session-smoke")) and (.events | length > 0)' "$SMOKE_DIR/browser-session-observe.json"
  run_awl workspace run --id "$BROWSER_SESSION_ID" --name browser-session-mount-probe --timeout-ms 8000 --tail-bytes 4000 -- bash -lc 'printf browser-session-write-ok > /workspace/browser-user-data/session-write.txt' > "$SMOKE_DIR/browser-session-mount-probe.json"
  assert_json '.succeeded == true and .launch.apps[0].mount_isolation == "bubblewrap_mount_namespace"' "$SMOKE_DIR/browser-session-mount-probe.json"
  grep -q '^browser-session-write-ok$' "$BROWSER_SESSION_DATA/session-write.txt"
  run_awl workspace stop --id "$BROWSER_SESSION_ID" > "$SMOKE_DIR/browser-session-stop.json"
  assert_json '.ok == true and (.apps[] | select(.name == "browser-session-no-sandbox" and .running == false))' "$SMOKE_DIR/browser-session-stop.json"
  run_awl profile delete browser-session-smoke > "$SMOKE_DIR/browser-session-delete.json"
  assert_json '.deleted == true and .profile.id == "browser-session-smoke"' "$SMOKE_DIR/browser-session-delete.json"
else
  echo "== browser-session startup smoke skipped: Chrome/Chromium not found =="
fi

echo "== open-profile dry-run =="
OPEN_PROFILE="$SMOKE_DIR/open-profile.json"
jq -n '{
  id: "open-preview",
  width: 800,
  height: 600,
  cwd: "/tmp",
  setup_commands: [{name:"noop-setup", command:["/bin/true"]}],
  startup_apps: [{name:"noop-startup", command:["/bin/true"]}]
}' > "$OPEN_PROFILE"
run_awl profile import --json "$OPEN_PROFILE" > /dev/null
OPEN_ID="open-preview-smoke-$$"
run_awl workspace open-profile --dry-run --profile open-preview --id "$OPEN_ID" --setup --startup-wait-window > "$SMOKE_DIR/open-noack.json"
assert_json '.would_open == false and .approval.missing_acknowledgements[0].id == "hidden_workspace"' "$SMOKE_DIR/open-noack.json"
run_awl workspace open-profile --dry-run --ack-hidden-workspace --profile open-preview --id "$OPEN_ID" --setup --startup-wait-window > "$SMOKE_DIR/open-ack.json"
assert_json '.would_open == true and .setup.command_count == 1 and .startup.app_count == 1 and .approval.approved == true' "$SMOKE_DIR/open-ack.json"
test ! -d "$RUNTIME_DIR/agent-workspace-linux/$OPEN_ID"

echo "== open-profile setup and startup =="
OPEN_REAL_PROFILE="$SMOKE_DIR/open-real-profile.json"
jq -n '{
  id: "open-real",
  width: 900,
  height: 650,
  cwd: "/tmp",
  setup_commands: [
    {
      name: "setup-marker",
      command: ["bash", "-lc", "echo setup-log-smoke; printf setup-ok > \"$AGENT_WORKSPACE_RUNTIME_DIR/setup-marker.txt\""]
    }
  ],
  startup_apps: [
    {
      name: "open-profile-message",
      command: ["xmessage", "-buttons", "OK:0", "-default", "OK", "Open profile smoke"]
    }
  ]
}' > "$OPEN_REAL_PROFILE"
run_awl profile import --json "$OPEN_REAL_PROFILE" > /dev/null
OPEN_REAL_ID="open-real-smoke-$$"
WORKSPACE_IDS+=("$OPEN_REAL_ID")
run_awl workspace open-profile --ack-hidden-workspace --profile open-real --id "$OPEN_REAL_ID" --purpose "Open profile smoke" --setup --setup-timeout-ms 10000 --setup-kill-on-timeout --startup-wait-window --startup-screenshot-window --startup-window-timeout-ms 10000 > "$SMOKE_DIR/open-real.json"
assert_json '.ready == true and .setup_succeeded == true and .startup_launched == true and .setup.succeeded == true and .startup.launched[0].ok == true and (.startup.launched[0].screenshot.bytes > 0)' "$SMOKE_DIR/open-real.json"
test "$(cat "$RUNTIME_DIR/agent-workspace-linux/$OPEN_REAL_ID/setup-marker.txt")" = "setup-ok"
OPEN_REAL_SETUP_APP_ID="$(jq -r '.setup.launched[0].apps[0].id' "$SMOKE_DIR/open-real.json")"
OPEN_REAL_APP_ID="$(jq -r '.startup.launched[0].apps[0].id' "$SMOKE_DIR/open-real.json")"
OPEN_REAL_WINDOW_ID="$(jq -r '.startup.launched[0].windows[0].id' "$SMOKE_DIR/open-real.json")"
run_awl workspace apps --id "$OPEN_REAL_ID" --app "$OPEN_REAL_APP_ID" > "$SMOKE_DIR/open-real-apps.json"
assert_json '.apps[0].name == "open-profile-message" and .apps[0].running == true and .apps[0].profile_id == "open-real"' "$SMOKE_DIR/open-real-apps.json"
run_awl workspace key-window --id "$OPEN_REAL_ID" "$OPEN_REAL_WINDOW_ID" Return > "$SMOKE_DIR/open-real-key.json"
run_awl workspace wait-app --id "$OPEN_REAL_ID" --timeout-ms 5000 "$OPEN_REAL_APP_ID" > "$SMOKE_DIR/open-real-wait.json"
assert_json '.ok == true and .apps[0].running == false and .apps[0].exit_code == 0' "$SMOKE_DIR/open-real-wait.json"
run_awl workspace stop --id "$OPEN_REAL_ID" > "$SMOKE_DIR/open-real-stop.json"
assert_json '.ok == true and .status.ready == false' "$SMOKE_DIR/open-real-stop.json"
run_awl workspace logs --id "$OPEN_REAL_ID" --stream stdout --tail-bytes 2000 "$OPEN_REAL_SETUP_APP_ID" > "$SMOKE_DIR/open-real-setup-log.json"
assert_json '.ok == true and .message == "workspace app log read from saved manifest" and (.app_log.content | contains("setup-log-smoke"))' "$SMOKE_DIR/open-real-setup-log.json"

echo "== local-only workspace =="
LOCAL_PROFILE="$SMOKE_DIR/local-profile.json"
jq -n '{
  id: "local-only",
  network: {mode:"local_only", allow_hosts:["localhost:3000", "127.0.0.1:5173"]}
}' > "$LOCAL_PROFILE"
run_awl profile import --json "$LOCAL_PROFILE" > /dev/null
LOCAL_ID="local-only-smoke-$$"
WORKSPACE_IDS+=("$LOCAL_ID")
run_awl workspace start --ack-hidden-workspace --profile local-only --id "$LOCAL_ID" --purpose "Integration smoke" > "$SMOKE_DIR/local-start.json"
run_awl workspace status --id "$LOCAL_ID" > "$SMOKE_DIR/local-status.json"
assert_json '.applied_policy.enforcement.network.enforced == true and .applied_policy.enforcement.network.backend == "bubblewrap_loopback_only" and .user_acknowledged_unenforced_policy == false' "$SMOKE_DIR/local-status.json"
SESSION_ID="$(jq -r '.session_id' "$SMOKE_DIR/local-status.json")"

NETWORK_PROBE="$SMOKE_DIR/network_probe.py"
cat > "$NETWORK_PROBE" <<'PY'
import socket
import threading

s = socket.socket()
s.bind(("127.0.0.1", 0))
s.listen(1)
port = s.getsockname()[1]

def serve():
    conn, _ = s.accept()
    conn.recv(16)
    conn.sendall(b"loopback-ok")
    conn.close()

t = threading.Thread(target=serve)
t.start()
c = socket.create_connection(("127.0.0.1", port), timeout=2)
c.sendall(b"ping")
print(c.recv(64).decode())
c.close()
t.join(2)

try:
    socket.create_connection(("1.1.1.1", 80), timeout=2)
except OSError:
    print("external-blocked")
else:
    raise SystemExit("external network unexpectedly reachable")
PY
run_awl workspace run --id "$LOCAL_ID" --timeout-ms 8000 --tail-bytes 4000 -- python3 "$NETWORK_PROBE" > "$SMOKE_DIR/local-run.json"
assert_json '.succeeded == true and (.stdout.content | contains("loopback-ok")) and (.stdout.content | contains("external-blocked")) and .launch.apps[0].network_isolation == "bubblewrap_loopback_only"' "$SMOKE_DIR/local-run.json"
assert_json '(.launch.status.apps | length) == 0 and (.wait.status.apps | length) == 0 and (.launch.apps | length) == 1 and (.wait.apps | length) == 1' "$SMOKE_DIR/local-run.json"
run_awl workspace env --id "$LOCAL_ID" > "$SMOKE_DIR/local-env.json"
assert_json '.environment.session_id == $sid and (.environment.variables[] | select(.name == "AGENT_WORKSPACE_SESSION_ID" and .value == $sid))' "$SMOKE_DIR/local-env.json" --arg sid "$SESSION_ID"
run_awl workspace events --id "$LOCAL_ID" --tail 20 > "$SMOKE_DIR/local-events.json"
assert_json '.events[] | select(.kind == "workspace_start" and .detail.session_id == $sid)' "$SMOKE_DIR/local-events.json" --arg sid "$SESSION_ID"
run_awl workspace stop --id "$LOCAL_ID" > "$SMOKE_DIR/local-stop.json"
assert_json '.ok == true and .status.ready == false' "$SMOKE_DIR/local-stop.json"
WORKSPACE_IDS=()
run_awl workspace manifest --id "$LOCAL_ID" > "$SMOKE_DIR/local-stopped-manifest.json"
assert_json '.manifest.session_id == $sid and .manifest.ready == false and .manifest.stopped_at_unix != null' "$SMOKE_DIR/local-stopped-manifest.json" --arg sid "$SESSION_ID"

echo "== disabled-network workspace =="
DISABLED_PROFILE="$SMOKE_DIR/disabled-profile.json"
jq -n '{
  id: "disabled-network",
  network: {mode:"disabled"},
  require_enforced_policy: true
}' > "$DISABLED_PROFILE"
run_awl profile import --json "$DISABLED_PROFILE" > /dev/null
DISABLED_ID="disabled-network-smoke-$$"
WORKSPACE_IDS+=("$DISABLED_ID")
run_awl workspace start --ack-hidden-workspace --profile disabled-network --id "$DISABLED_ID" --purpose "Disabled network smoke" > "$SMOKE_DIR/disabled-start.json"
run_awl workspace status --id "$DISABLED_ID" > "$SMOKE_DIR/disabled-status.json"
assert_json '.applied_policy.enforcement.network.enforced == true and .applied_policy.enforcement.network.backend == "bubblewrap_unshare_net" and .user_acknowledged_unenforced_policy == false' "$SMOKE_DIR/disabled-status.json"
DISABLED_PROBE="$SMOKE_DIR/disabled_network_probe.py"
cat > "$DISABLED_PROBE" <<'PY'
import socket

try:
    socket.create_connection(("1.1.1.1", 80), timeout=2)
except OSError:
    print("direct-blocked")
else:
    raise SystemExit("direct network unexpectedly reachable")

try:
    socket.getaddrinfo("example.com", 80)
except OSError:
    print("dns-blocked")
else:
    raise SystemExit("dns unexpectedly resolved")
PY
run_awl workspace run --id "$DISABLED_ID" --timeout-ms 8000 --tail-bytes 4000 -- python3 "$DISABLED_PROBE" > "$SMOKE_DIR/disabled-run.json"
assert_json '.succeeded == true and (.stdout.content | contains("direct-blocked")) and (.stdout.content | contains("dns-blocked")) and .launch.apps[0].network_isolation == "bubblewrap_unshare_net"' "$SMOKE_DIR/disabled-run.json"
assert_json '(.launch.status.apps | length) == 0 and (.wait.status.apps | length) == 0 and (.launch.apps | length) == 1 and (.wait.apps | length) == 1' "$SMOKE_DIR/disabled-run.json"
if [[ -n "$BROWSER_BIN" ]]; then
  run_awl workspace launch --id "$DISABLED_ID" --name disabled-network-browser --wait-window --screenshot-window --window-timeout-ms 20000 -- "$BROWSER_BIN" "--user-data-dir=$SMOKE_DIR/disabled-browser-profile" --no-sandbox --disable-dev-shm-usage --no-first-run --no-default-browser-check --new-window https://example.com > "$SMOKE_DIR/disabled-browser-launch.json"
  assert_json '.ok == true and (.screenshot.bytes > 0) and .apps[0].network_isolation == "bubblewrap_unshare_net" and .windows[0].wm_class == "Google-chrome"' "$SMOKE_DIR/disabled-browser-launch.json"
  DISABLED_BROWSER_APP_ID="$(jq -r '.apps[0].id' "$SMOKE_DIR/disabled-browser-launch.json")"
  run_awl workspace wait-window --id "$DISABLED_ID" --app "$DISABLED_BROWSER_APP_ID" --title example.com --timeout-ms 10000 > "$SMOKE_DIR/disabled-browser-wait.json"
  assert_json '.ok == true and (.windows[0].title | contains("example.com"))' "$SMOKE_DIR/disabled-browser-wait.json"
else
  echo "== disabled-network browser smoke skipped: Chrome/Chromium not found =="
fi
run_awl workspace stop --id "$DISABLED_ID" > /dev/null

echo "== mount enforcement workspace =="
MOUNT_RW_HOST="$SMOKE_DIR/mount-rw"
MOUNT_RO_HOST="$SMOKE_DIR/mount-ro"
mkdir -p "$MOUNT_RW_HOST" "$MOUNT_RO_HOST"
printf 'seed\n' > "$MOUNT_RO_HOST/seed.txt"
MOUNT_PROFILE="$SMOKE_DIR/mount-profile.json"
jq -n --arg rw "$MOUNT_RW_HOST" --arg ro "$MOUNT_RO_HOST" '{
  id: "mount-policy",
  cwd: "/workspace/rw",
  require_enforced_policy: true,
  mounts: [
    {host_path: $rw, workspace_path: "/workspace/rw", mode: "read_write"},
    {host_path: $ro, workspace_path: "/workspace/ro", mode: "read_only"}
  ]
}' > "$MOUNT_PROFILE"
run_awl profile import --json "$MOUNT_PROFILE" > /dev/null
MOUNT_ID="mount-policy-smoke-$$"
WORKSPACE_IDS+=("$MOUNT_ID")
run_awl workspace start --ack-hidden-workspace --profile mount-policy --id "$MOUNT_ID" --purpose "Mount policy smoke" > "$SMOKE_DIR/mount-start.json"
run_awl workspace status --id "$MOUNT_ID" > "$SMOKE_DIR/mount-status.json"
assert_json '.applied_policy.enforcement.mounts.enforced == true and .applied_policy.enforcement.mounts.backend == "bubblewrap_mount_namespace" and .user_acknowledged_unenforced_policy == false' "$SMOKE_DIR/mount-status.json"
run_awl workspace run --id "$MOUNT_ID" --timeout-ms 8000 --tail-bytes 4000 -- bash -lc 'set -eu; test "$(cat /workspace/ro/seed.txt)" = "seed"; echo rw-ok > /workspace/rw/out.txt; if sh -c "echo blocked > /workspace/ro/nope.txt" 2>/tmp/ro-write.err; then echo ro-write-unexpected; exit 7; else echo ro-blocked; fi' > "$SMOKE_DIR/mount-run.json"
assert_json '.succeeded == true and (.stdout.content | contains("ro-blocked")) and .launch.apps[0].mount_isolation == "bubblewrap_mount_namespace"' "$SMOKE_DIR/mount-run.json"
grep -q '^rw-ok$' "$MOUNT_RW_HOST/out.txt"
test ! -e "$MOUNT_RO_HOST/nope.txt"
if [[ -n "$GUI_EDITOR_BIN" ]]; then
  echo "== mounted GUI editor workspace =="
  printf 'seed editor\n' > "$MOUNT_RW_HOST/editor-note.txt"
  run_awl workspace launch --id "$MOUNT_ID" --name mounted-gui-editor --env XDG_CACHE_HOME=/tmp/agent-workspace-cache --wait-window --screenshot-window --window-timeout-ms 20000 -- "$GUI_EDITOR_BIN" /workspace/rw/editor-note.txt > "$SMOKE_DIR/mount-editor-launch.json"
  assert_json '.ok == true and (.screenshot.bytes > 0) and ((.windows | length) > 0) and .apps[0].mount_isolation == "bubblewrap_mount_namespace"' "$SMOKE_DIR/mount-editor-launch.json"
  MOUNT_EDITOR_APP_ID="$(jq -r '.apps[0].id' "$SMOKE_DIR/mount-editor-launch.json")"
  MOUNT_EDITOR_TEXT=$'edited-from-integration-smoke\nmounted-editor-save-ok\n'
  for _ in {1..30}; do
    run_awl workspace screenshot-window --id "$MOUNT_ID" --app "$MOUNT_EDITOR_APP_ID" --output "$SMOKE_DIR/mount-editor-ready.png" --timeout-ms 5000 > "$SMOKE_DIR/mount-editor-ready-screenshot.json"
    if jq -e '.screenshot.bytes > 1000' "$SMOKE_DIR/mount-editor-ready-screenshot.json" >/dev/null; then
      break
    fi
    sleep 0.2
  done
  assert_json '.screenshot.bytes > 1000 and .screenshot.path == $path' "$SMOKE_DIR/mount-editor-ready-screenshot.json" --arg path "$SMOKE_DIR/mount-editor-ready.png"
  run_awl workspace focus-window --id "$MOUNT_ID" --app "$MOUNT_EDITOR_APP_ID" --timeout-ms 5000 > "$SMOKE_DIR/mount-editor-focus.json"
  run_awl workspace click-window --id "$MOUNT_ID" --app "$MOUNT_EDITOR_APP_ID" --timeout-ms 5000 120 150 > "$SMOKE_DIR/mount-editor-click-body.json"
  run_awl workspace key-window --id "$MOUNT_ID" --app "$MOUNT_EDITOR_APP_ID" --timeout-ms 5000 ctrl+a > "$SMOKE_DIR/mount-editor-select.json"
  run_awl workspace paste-window --id "$MOUNT_ID" --app "$MOUNT_EDITOR_APP_ID" --timeout-ms 5000 "$MOUNT_EDITOR_TEXT" > "$SMOKE_DIR/mount-editor-paste.json"
  run_awl workspace key-window --id "$MOUNT_ID" --app "$MOUNT_EDITOR_APP_ID" --timeout-ms 5000 ctrl+s > "$SMOKE_DIR/mount-editor-save.json"
  for _ in {1..30}; do
    if grep -q '^mounted-editor-save-ok$' "$MOUNT_RW_HOST/editor-note.txt"; then
      break
    fi
    sleep 0.2
  done
  grep -q '^edited-from-integration-smoke$' "$MOUNT_RW_HOST/editor-note.txt"
  grep -q '^mounted-editor-save-ok$' "$MOUNT_RW_HOST/editor-note.txt"
  run_awl workspace screenshot-window --id "$MOUNT_ID" --app "$MOUNT_EDITOR_APP_ID" --output "$SMOKE_DIR/mount-editor.png" --timeout-ms 5000 > "$SMOKE_DIR/mount-editor-screenshot.json"
  assert_json '.screenshot.bytes > 0 and .screenshot.path == $path' "$SMOKE_DIR/mount-editor-screenshot.json" --arg path "$SMOKE_DIR/mount-editor.png"
else
  echo "== mounted GUI editor workspace skipped: no supported editor found =="
fi
run_awl workspace stop --id "$MOUNT_ID" > /dev/null

echo "== window, screenshot, input, clipboard, and artifacts =="
GUI_ID="gui-smoke-$$"
WORKSPACE_IDS+=("$GUI_ID")
run_awl workspace start --ack-hidden-workspace --id "$GUI_ID" --purpose "GUI smoke" > "$SMOKE_DIR/gui-start.json"
run_awl workspace launch --id "$GUI_ID" --name message --wait-window --screenshot-window --window-timeout-ms 10000 -- xmessage -buttons OK:0 -default OK "Agent workspace smoke" > "$SMOKE_DIR/gui-launch.json"
assert_json '.ok == true and (.screenshot.bytes > 0) and ((.windows | length) > 0) and .apps[0].running == true' "$SMOKE_DIR/gui-launch.json"
GUI_APP_ID="$(jq -r '.apps[0].id' "$SMOKE_DIR/gui-launch.json")"
run_awl workspace windows --id "$GUI_ID" --class xmessage > "$SMOKE_DIR/gui-windows.json"
assert_json '(.windows | length) > 0 and .windows[0].title == "xmessage"' "$SMOKE_DIR/gui-windows.json"
run_awl workspace windows --id "$GUI_ID" --app "$GUI_APP_ID" > "$SMOKE_DIR/gui-windows-by-app.json"
assert_json '(.windows | length) > 0 and .windows[0].app_id == $app_id' "$SMOKE_DIR/gui-windows-by-app.json" --arg app_id "$GUI_APP_ID"
run_awl workspace screenshot --id "$GUI_ID" --output "$SMOKE_DIR/gui-root.png" > "$SMOKE_DIR/gui-screenshot.json"
assert_json '.screenshot.bytes > 0 and .screenshot.path == $path' "$SMOKE_DIR/gui-screenshot.json" --arg path "$SMOKE_DIR/gui-root.png"
test -s "$SMOKE_DIR/gui-root.png"
run_awl workspace clipboard-set --id "$GUI_ID" "clipboard-smoke" > "$SMOKE_DIR/gui-clipboard-set.json"
assert_json '.clipboard.bytes == 15 and .clipboard.content == null' "$SMOKE_DIR/gui-clipboard-set.json"
run_awl workspace clipboard-get --id "$GUI_ID" > "$SMOKE_DIR/gui-clipboard-get.json"
assert_json '.clipboard.content == "clipboard-smoke"' "$SMOKE_DIR/gui-clipboard-get.json"
run_awl workspace observe --id "$GUI_ID" --screenshot --events --events-tail 20 > "$SMOKE_DIR/gui-observe.json"
assert_json '.screenshot.bytes > 0 and (.events[] | select(.kind == "app_launch" and .detail.app_id == $app_id))' "$SMOKE_DIR/gui-observe.json" --arg app_id "$GUI_APP_ID"
run_awl workspace key-window --id "$GUI_ID" --app "$GUI_APP_ID" Return > "$SMOKE_DIR/gui-key.json"
assert_json '.ok == true and .windows[0].app_id == $app_id' "$SMOKE_DIR/gui-key.json" --arg app_id "$GUI_APP_ID"
run_awl workspace wait-app --id "$GUI_ID" --timeout-ms 5000 "$GUI_APP_ID" > "$SMOKE_DIR/gui-wait.json"
assert_json '.ok == true and .apps[0].running == false and .apps[0].exit_code == 0' "$SMOKE_DIR/gui-wait.json"
run_awl workspace stop --id "$GUI_ID" > /dev/null
run_awl workspace artifacts --id "$GUI_ID" --existing > "$SMOKE_DIR/gui-artifacts.json"
assert_json '(.files[] | select(.kind == "manifest" and .exists == true)) and (.files[] | select(.kind == "event_log" and .exists == true and .bytes > 0)) and (.files[] | select(.kind == "app_log" and .exists == true)) and (.files[] | select(.kind == "screenshot" and .exists == true and .bytes > 0))' "$SMOKE_DIR/gui-artifacts.json"

if [[ -n "$BROWSER_BIN" ]]; then
  echo "== browser local-dev workspace =="
  BROWSER_ID="browser-local-dev-smoke-$$"
  BROWSER_PORT="$(python3 - <<'PY'
import socket

s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
)"
  BROWSER_README_URL="http://127.0.0.1:${BROWSER_PORT}/README.md"
  BROWSER_DOC_URL="http://127.0.0.1:${BROWSER_PORT}/docs/permission-model.md"
  WORKSPACE_IDS+=("$BROWSER_ID")
  run_awl workspace start --ack-hidden-workspace --id "$BROWSER_ID" --purpose "Browser local-dev smoke" > "$SMOKE_DIR/browser-start.json"
  run_awl workspace launch --id "$BROWSER_ID" --name dev-server --cwd "$ROOT_DIR" -- python3 -m http.server "$BROWSER_PORT" --bind 127.0.0.1 > "$SMOKE_DIR/browser-server.json"
  run_awl workspace run --id "$BROWSER_ID" --name dev-server-probe --timeout-ms 10000 --tail-bytes 4000 -- python3 -c "import time, urllib.request
url = '$BROWSER_README_URL'
last = None
for _ in range(30):
    try:
        print(urllib.request.urlopen(url, timeout=2).readline().decode().strip())
        break
    except Exception as exc:
        last = exc
        time.sleep(0.1)
else:
    raise SystemExit(last)" > "$SMOKE_DIR/browser-probe.json"
  assert_json '.succeeded == true and (.stdout.content | contains("# agent-workspace-linux"))' "$SMOKE_DIR/browser-probe.json"
  run_awl workspace launch --id "$BROWSER_ID" --name browser-local-dev --wait-window --screenshot-window --window-timeout-ms 15000 -- "$BROWSER_BIN" "--user-data-dir=$SMOKE_DIR/browser-profile" --no-first-run --no-default-browser-check --new-window "$BROWSER_README_URL" > "$SMOKE_DIR/browser-launch.json"
  assert_json '.ok == true and (.screenshot.bytes > 0) and ((.windows | length) > 0) and .apps[0].running == true' "$SMOKE_DIR/browser-launch.json"
  BROWSER_APP_ID="$(jq -r '.apps[0].id' "$SMOKE_DIR/browser-launch.json")"
  run_awl workspace key-window --id "$BROWSER_ID" --app "$BROWSER_APP_ID" --timeout-ms 5000 ctrl+l > "$SMOKE_DIR/browser-key-address.json"
  run_awl workspace paste-window --id "$BROWSER_ID" --app "$BROWSER_APP_ID" --timeout-ms 5000 "$BROWSER_DOC_URL" > "$SMOKE_DIR/browser-paste-address.json"
  run_awl workspace key-window --id "$BROWSER_ID" --app "$BROWSER_APP_ID" --timeout-ms 5000 Return > "$SMOKE_DIR/browser-key-return.json"
  run_awl workspace wait-window --id "$BROWSER_ID" --app "$BROWSER_APP_ID" --title permission-model.md --timeout-ms 10000 > "$SMOKE_DIR/browser-wait-doc-window.json"
  assert_json '.ok == true and (.windows[0].title | contains("permission-model.md"))' "$SMOKE_DIR/browser-wait-doc-window.json"
  run_awl workspace observe --id "$BROWSER_ID" --screenshot --events --events-tail 20 > "$SMOKE_DIR/browser-observe.json"
  assert_json '(.screenshot.bytes > 0) and (.active_window.title | contains("permission-model.md"))' "$SMOKE_DIR/browser-observe.json"
  run_awl workspace stop --id "$BROWSER_ID" > "$SMOKE_DIR/browser-stop.json"
  assert_json '.ok == true and (.apps[] | select(.name == "dev-server" and .running == false)) and (.apps[] | select(.name == "browser-local-dev" and .running == false))' "$SMOKE_DIR/browser-stop.json"

  echo "== browser native input workspace =="
  NATIVE_BROWSER_ID="browser-native-input-smoke-$$"
  NATIVE_BROWSER_URL="$(python3 - <<'PY'
from urllib.parse import quote

html = """<!doctype html>
<meta charset="utf-8">
<title>Agent Workspace Native Browser OK</title>
<input id="probe" autofocus>
<script>
const probe = document.getElementById("probe");
function updateTitle() {
  document.title = probe.value ? `typed:${probe.value}` : "Agent Workspace Native Browser OK";
}
probe.addEventListener("input", updateTitle);
setTimeout(() => probe.focus(), 100);
</script>
"""
print("data:text/html;charset=utf-8," + quote(html))
PY
)"
  WORKSPACE_IDS+=("$NATIVE_BROWSER_ID")
  run_awl workspace start --ack-hidden-workspace --id "$NATIVE_BROWSER_ID" --purpose "Browser native input smoke" > "$SMOKE_DIR/native-browser-start.json"
  run_awl workspace launch --id "$NATIVE_BROWSER_ID" --name browser-native-input --wait-window --screenshot-window --window-timeout-ms 15000 -- "$BROWSER_BIN" "--user-data-dir=$SMOKE_DIR/native-browser-profile" --no-sandbox --disable-dev-shm-usage --no-first-run --no-default-browser-check --ozone-platform=x11 --new-window about:blank > "$SMOKE_DIR/native-browser-launch.json"
  assert_json '.ok == true and (.screenshot.bytes > 0) and ((.windows | length) > 0) and .apps[0].running == true' "$SMOKE_DIR/native-browser-launch.json"
  NATIVE_BROWSER_APP_ID="$(jq -r '.apps[0].id' "$SMOKE_DIR/native-browser-launch.json")"
  run_awl workspace key-window --id "$NATIVE_BROWSER_ID" --app "$NATIVE_BROWSER_APP_ID" --timeout-ms 5000 ctrl+l > "$SMOKE_DIR/native-browser-key-address.json"
  run_awl workspace paste-window --id "$NATIVE_BROWSER_ID" --app "$NATIVE_BROWSER_APP_ID" --timeout-ms 5000 "$NATIVE_BROWSER_URL" > "$SMOKE_DIR/native-browser-paste-address.json"
  run_awl workspace key-window --id "$NATIVE_BROWSER_ID" --app "$NATIVE_BROWSER_APP_ID" --timeout-ms 5000 Return > "$SMOKE_DIR/native-browser-key-return.json"
  run_awl workspace wait-window --id "$NATIVE_BROWSER_ID" --app "$NATIVE_BROWSER_APP_ID" --title "Agent Workspace Native Browser OK" --timeout-ms 10000 > "$SMOKE_DIR/native-browser-wait-loaded.json"
  assert_json '.ok == true and (.windows[0].title | contains("Agent Workspace Native Browser OK"))' "$SMOKE_DIR/native-browser-wait-loaded.json"
  run_awl workspace type-window --id "$NATIVE_BROWSER_ID" --app "$NATIVE_BROWSER_APP_ID" --timeout-ms 5000 typed-ok > "$SMOKE_DIR/native-browser-type.json"
  run_awl workspace wait-window --id "$NATIVE_BROWSER_ID" --app "$NATIVE_BROWSER_APP_ID" --title "typed:typed-ok" --timeout-ms 10000 > "$SMOKE_DIR/native-browser-wait-typed.json"
  assert_json '.ok == true and (.windows[0].title | contains("typed:typed-ok"))' "$SMOKE_DIR/native-browser-wait-typed.json"
  run_awl workspace observe --id "$NATIVE_BROWSER_ID" --screenshot --events --events-tail 20 > "$SMOKE_DIR/native-browser-observe.json"
  assert_json '(.screenshot.bytes > 0) and (.active_window.title | contains("typed:typed-ok"))' "$SMOKE_DIR/native-browser-observe.json"
  run_awl workspace stop --id "$NATIVE_BROWSER_ID" > "$SMOKE_DIR/native-browser-stop.json"
  assert_json '.ok == true and (.apps[] | select(.name == "browser-native-input" and .running == false))' "$SMOKE_DIR/native-browser-stop.json"
else
  echo "== browser local-dev and native input workspaces skipped: Chrome/Chromium not found =="
fi

echo "== crashed-daemon stale cleanup =="
CRASH_ID="crash-cleanup-smoke-$$"
WORKSPACE_IDS+=("$CRASH_ID")
STALE_CLEANUP_IDS+=("$CRASH_ID")
run_awl workspace start --ack-hidden-workspace --id "$CRASH_ID" --purpose "Crash cleanup smoke" > "$SMOKE_DIR/crash-start.json"
run_awl workspace status --id "$CRASH_ID" > "$SMOKE_DIR/crash-status.json"
CRASH_DAEMON_PID="$(jq -r '.daemon_pid // empty' "$SMOKE_DIR/crash-status.json")"
CRASH_X_PID="$(jq -r '.x_server_pid // empty' "$SMOKE_DIR/crash-status.json")"
CRASH_WM_PID="$(jq -r '.window_manager_pid // empty' "$SMOKE_DIR/crash-status.json")"
test -n "$CRASH_DAEMON_PID"
test -n "$CRASH_X_PID"
run_awl workspace launch --id "$CRASH_ID" --name sleepy -- sleep 1000 > "$SMOKE_DIR/crash-launch.json"
assert_json '.ok == true and .apps[0].name == "sleepy" and .apps[0].running == true and .apps[0].process_group_id != null' "$SMOKE_DIR/crash-launch.json"
CRASH_APP_PID="$(jq -r '.apps[0].pid' "$SMOKE_DIR/crash-launch.json")"
CRASH_APP_PGID="$(jq -r '.apps[0].process_group_id' "$SMOKE_DIR/crash-launch.json")"
kill -9 "$CRASH_DAEMON_PID"
wait_pid_gone "workspace daemon" "$CRASH_DAEMON_PID"
# The X server and window manager die with their daemon; only apps remain.
wait_pid_gone "window manager" "$CRASH_WM_PID"
wait_pid_gone "X server" "$CRASH_X_PID"
for _ in {1..40}; do
  run_awl workspace list > "$SMOKE_DIR/crash-list.json"
  if jq -e --arg id "$CRASH_ID" '.workspaces[] | select(.id == $id and .running == false)' "$SMOKE_DIR/crash-list.json" >/dev/null; then
    break
  fi
  sleep 0.1
done
assert_json '.workspaces[] | select(.id == $id and .running == false)' "$SMOKE_DIR/crash-list.json" --arg id "$CRASH_ID"
run_awl workspace cleanup --dry-run --id "$CRASH_ID" > "$SMOKE_DIR/crash-cleanup-dry.json"
assert_json '.candidates[] | select(.id == $id and (.process_cleanup[] | contains("would terminate app sleepy process group")) and ([.process_cleanup[] | select(contains("X server"))] | length == 0))' "$SMOKE_DIR/crash-cleanup-dry.json" --arg id "$CRASH_ID"
run_awl workspace cleanup --id "$CRASH_ID" > "$SMOKE_DIR/crash-cleanup.json"
assert_json '.removed[] | select(.id == $id and (.process_cleanup[] | contains("terminated app sleepy process group")))' "$SMOKE_DIR/crash-cleanup.json" --arg id "$CRASH_ID"
wait_pgid_gone "sleepy app" "$CRASH_APP_PGID"
wait_pid_gone "sleepy app" "$CRASH_APP_PID"
test ! -d "$RUNTIME_DIR/agent-workspace-linux/$CRASH_ID"

echo "== self-stop from workspace app =="
SELF_STOP_ID="self-stop-smoke-$$"
WORKSPACE_IDS+=("$SELF_STOP_ID")
run_awl workspace start --ack-hidden-workspace --id "$SELF_STOP_ID" --purpose "Self-stop smoke" > "$SMOKE_DIR/self-stop-start.json"
run_awl workspace launch --id "$SELF_STOP_ID" --name self-stop-client -- bash -lc '"$1" workspace stop --id "$2" >/tmp/agent-workspace-self-stop-smoke.out 2>/tmp/agent-workspace-self-stop-smoke.err; sleep 10' _ "$BIN" "$SELF_STOP_ID" > "$SMOKE_DIR/self-stop-launch.json"
sleep 2
run_awl workspace list > "$SMOKE_DIR/self-stop-list.json"
assert_json '.workspaces[] | select(.id == $id and .running == false and .manifest.ready == false and .manifest.stopped_at_unix != null)' "$SMOKE_DIR/self-stop-list.json" --arg id "$SELF_STOP_ID"

echo "integration smoke passed"
