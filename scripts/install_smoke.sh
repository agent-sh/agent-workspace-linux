#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN_NAME="agent-workspace-linux"
SOURCE_BIN="$ROOT_DIR/target/release/$BIN_NAME"

need() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "missing required command: $1" >&2
    exit 1
  fi
}

need cargo
need cmp

cargo build --manifest-path "$ROOT_DIR/Cargo.toml" --locked --release >/dev/null

SMOKE_DIR="$(mktemp -d)"
PREFIX="$SMOKE_DIR/prefix"
CODEX_HOME="$SMOKE_DIR/codex-home"
HOME_DIR="$SMOKE_DIR/home"
CONFIG="$CODEX_HOME/config.toml"

cleanup() {
  local exit_code=$?
  if [[ "$exit_code" -eq 0 ]]; then
    rm -rf "$SMOKE_DIR"
  else
    echo "installer smoke failed; preserved temp dir: $SMOKE_DIR" >&2
  fi
}
trap cleanup EXIT

mkdir -p "$CODEX_HOME" "$HOME_DIR"
cat >"$CONFIG" <<'TOML'
[profile]
name = "keep-me"

[mcp_servers.agent-workspace-linux]
command = "/old/agent-workspace-linux"
args = ["mcp"]

[mcp_servers.agent-workspace-linux.tools.workspace_start]
enabled = true

[mcp_servers.agent-workspace-linux.tools.workspace_stop]
enabled = false

[mcp_servers.agent-workspace-linux-extra]
command = "/keep/agent-workspace-linux-extra"
args = ["mcp"]

[mcp_servers.other]
command = "/keep/other"
args = ["mcp"]
TOML

HOME="$HOME_DIR" \
CODEX_HOME= \
SKILLS_DIR= \
PREFIX= \
BINDIR= \
"$ROOT_DIR/install.sh" \
  --skip-build \
  --no-doctor \
  --prefix "$PREFIX" \
  --codex-home "$CODEX_HOME" \
  --clean-codex-config >/dev/null

DEST_BIN="$PREFIX/bin/$BIN_NAME"
DEST_SKILL="$CODEX_HOME/skills/agent-workspace-linux/SKILL.md"

test -x "$DEST_BIN"
cmp -s "$SOURCE_BIN" "$DEST_BIN"
test -f "$DEST_SKILL"
cmp -s "$ROOT_DIR/skills/agent-workspace-linux/SKILL.md" "$DEST_SKILL"

if grep -Eq '^\[mcp_servers\.agent-workspace-linux(\]|\.)' "$CONFIG"; then
  echo "stale agent-workspace-linux MCP entries were not removed" >&2
  cat "$CONFIG" >&2
  exit 1
fi

grep -q '^\[profile\]' "$CONFIG"
grep -q '^\[mcp_servers\.agent-workspace-linux-extra\]' "$CONFIG"
grep -q '^\[mcp_servers\.other\]' "$CONFIG"

BACKUP_COUNT="$(find "$CODEX_HOME" -maxdepth 1 -name 'config.toml.bak-agent-workspace-clean-*' | wc -l)"
if [[ "$BACKUP_COUNT" -ne 1 ]]; then
  echo "expected one temp Codex config backup, found $BACKUP_COUNT" >&2
  find "$CODEX_HOME" -maxdepth 1 -type f -print >&2
  exit 1
fi

echo "installer smoke ok"
