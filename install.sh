#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_NAME="agent-workspace-linux"

PREFIX="${PREFIX:-$HOME/.local}"
BINDIR="${BINDIR:-$PREFIX/bin}"
CODEX_HOME="${CODEX_HOME:-$HOME/.codex}"
CODEX_CONFIG="${CODEX_CONFIG:-$CODEX_HOME/config.toml}"
CODEX_MCP_SERVER_NAME="${CODEX_MCP_SERVER_NAME:-agent-workspace-linux}"
MCP_PERMISSIONS="${MCP_PERMISSIONS:-}"

DRY_RUN=0
SKIP_BUILD=0
CONFIGURE_CODEX=0
CLEAN_CODEX_CONFIG=0
CODEX_MCP_CONFIG_CHANGED=0
RUN_DOCTOR=1
INSTALL_SKILL=1
SKILL_NAME="agent-workspace-linux"
SKILLS_DIR_EXPLICIT=0
if [ "${SKILLS_DIR+x}" = x ] && [ -n "$SKILLS_DIR" ]; then
  SKILLS_DIR_EXPLICIT=1
fi
SKILLS_DIR="${SKILLS_DIR:-$CODEX_HOME/skills}"
SKILL_SRC="$ROOT_DIR/skills/$SKILL_NAME/SKILL.md"

usage() {
  cat <<USAGE
Usage: ./install.sh [options]

Build and install agent-workspace-linux plus its lightweight skill.
Codex MCP registration is opt-in so the dedicated Codex for Linux feature page
can own Agent Workspace configuration without polluting generic MCP settings.

Options:
  --dry-run              Show what would happen without writing files.
  --skip-build           Install an already-built target/release binary.
  --codex-configure      Also register the MCP server in the Codex MCP config.
  --no-codex-config      Do not edit the Codex MCP config (default).
  --clean-codex-config   Remove existing Agent Workspace MCP entries from Codex config.
  --no-doctor            Do not run agent-workspace-linux doctor after install.
  --no-skill             Do not install the agent-workspace-linux skill.
  --skills-dir PATH      Install the skill under PATH (default: CODEX_HOME/skills).
  --prefix PATH          Install under PATH (default: ~/.local).
  --bindir PATH          Install binary into PATH (default: PREFIX/bin).
  --codex-home PATH      Use this Codex home (default: ~/.codex).
  --codex-config PATH    Use this Codex config file.
  --permissions PATH     Register the MCP server with a spawn-time permission ceiling.
  -h, --help             Show this help.
USAGE
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --dry-run)
      DRY_RUN=1
      ;;
    --skip-build)
      SKIP_BUILD=1
      ;;
    --codex-configure)
      CONFIGURE_CODEX=1
      ;;
    --clean-codex-config)
      CLEAN_CODEX_CONFIG=1
      ;;
    --no-codex-config)
      CONFIGURE_CODEX=0
      ;;
    --no-doctor)
      RUN_DOCTOR=0
      ;;
    --no-skill)
      INSTALL_SKILL=0
      ;;
    --skills-dir)
      SKILLS_DIR="${2:?missing value for --skills-dir}"
      SKILLS_DIR_EXPLICIT=1
      shift
      ;;
    --prefix)
      PREFIX="${2:?missing value for --prefix}"
      BINDIR="$PREFIX/bin"
      shift
      ;;
    --bindir)
      BINDIR="${2:?missing value for --bindir}"
      shift
      ;;
    --codex-home)
      CODEX_HOME="${2:?missing value for --codex-home}"
      CODEX_CONFIG="$CODEX_HOME/config.toml"
      if [ "$SKILLS_DIR_EXPLICIT" -eq 0 ]; then
        SKILLS_DIR="$CODEX_HOME/skills"
      fi
      shift
      ;;
    --codex-config)
      CODEX_CONFIG="${2:?missing value for --codex-config}"
      shift
      ;;
    --permissions)
      MCP_PERMISSIONS="${2:?missing value for --permissions}"
      CONFIGURE_CODEX=1
      shift
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      echo "unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
  shift
done

SOURCE_BIN="$ROOT_DIR/target/release/$BIN_NAME"
DEST_BIN="$BINDIR/$BIN_NAME"

toml_escape() {
  printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'
}

desired_mcp_block() {
  local escaped_command escaped_permissions
  escaped_command="$(toml_escape "$DEST_BIN")"
  printf '[mcp_servers.%s]\n' "$CODEX_MCP_SERVER_NAME"
  printf 'command = "%s"\n' "$escaped_command"
  if [ -n "$MCP_PERMISSIONS" ]; then
    escaped_permissions="$(toml_escape "$MCP_PERMISSIONS")"
    printf 'args = ["mcp", "--permissions", "%s"]\n' "$escaped_permissions"
  else
    printf 'args = ["mcp"]\n'
  fi
}

codex_mcp_server_present() {
  local section_prefix
  section_prefix="[mcp_servers.$CODEX_MCP_SERVER_NAME"

  [ -f "$CODEX_CONFIG" ] || return 1

  awk -v prefix="$section_prefix" '
    index($0, prefix) == 1 {
      next_char = substr($0, length(prefix) + 1, 1)
      if (next_char == "]" || next_char == ".") {
        found = 1
      }
    }
    END { exit found ? 0 : 1 }
  ' "$CODEX_CONFIG"
}

strip_codex_mcp_server_config() {
  local input output section_prefix
  input="$1"
  output="$2"
  section_prefix="[mcp_servers.$CODEX_MCP_SERVER_NAME"

  awk -v prefix="$section_prefix" '
    index($0, prefix) == 1 {
      next_char = substr($0, length(prefix) + 1, 1)
      if (next_char == "]" || next_char == ".") {
        skip = 1
        next
      }
    }
    /^\[/ { skip = 0 }
    !skip { lines[++n] = $0 }
    END {
      while (n > 0 && lines[n] == "") {
        n--
      }
      for (i = 1; i <= n; i++) {
        print lines[i]
      }
    }
  ' "$input" >"$output"
}

clean_codex_config() {
  local config_dir tmp backup

  if [ "$DRY_RUN" -eq 1 ]; then
    if codex_mcp_server_present; then
      echo "Would remove Codex MCP server '$CODEX_MCP_SERVER_NAME' and nested tool entries from $CODEX_CONFIG"
    else
      echo "No Codex MCP server '$CODEX_MCP_SERVER_NAME' found in $CODEX_CONFIG"
    fi
    return
  fi

  if [ ! -f "$CODEX_CONFIG" ]; then
    echo "No Codex config found at $CODEX_CONFIG; nothing to clean."
    return
  fi

  config_dir="$(dirname "$CODEX_CONFIG")"
  tmp="$(mktemp "$config_dir/config.toml.tmp.XXXXXX")"
  strip_codex_mcp_server_config "$CODEX_CONFIG" "$tmp"

  if cmp -s "$tmp" "$CODEX_CONFIG"; then
    rm -f "$tmp"
    echo "No Codex MCP server '$CODEX_MCP_SERVER_NAME' found in $CODEX_CONFIG"
    return
  fi

  backup="$CODEX_CONFIG.bak-agent-workspace-clean-$(date +%Y%m%d%H%M%S)-$$"
  cp -p "$CODEX_CONFIG" "$backup"
  echo "Backed up Codex config to $backup"

  mv "$tmp" "$CODEX_CONFIG"
  chmod 600 "$CODEX_CONFIG" 2>/dev/null || true
  CODEX_MCP_CONFIG_CHANGED=1
  echo "Removed Codex MCP server '$CODEX_MCP_SERVER_NAME' from $CODEX_CONFIG"
}

write_codex_config() {
  local config_dir tmp desired backup
  config_dir="$(dirname "$CODEX_CONFIG")"
  desired="$(desired_mcp_block)"

  if [ "$DRY_RUN" -eq 1 ]; then
    echo "Would register Codex MCP server in $CODEX_CONFIG, replacing any existing '$CODEX_MCP_SERVER_NAME' MCP/tool entries:"
    printf '%s\n' "$desired"
    return
  fi

  mkdir -p "$config_dir"
  tmp="$(mktemp "$config_dir/config.toml.tmp.XXXXXX")"

  if [ -f "$CODEX_CONFIG" ]; then
    strip_codex_mcp_server_config "$CODEX_CONFIG" "$tmp"
  else
    : >"$tmp"
  fi

  if [ -s "$tmp" ]; then
    printf '\n' >>"$tmp"
  fi
  printf '%s\n' "$desired" >>"$tmp"

  if [ -f "$CODEX_CONFIG" ] && cmp -s "$tmp" "$CODEX_CONFIG"; then
    rm -f "$tmp"
    echo "Codex MCP config already up to date: $CODEX_CONFIG"
    return
  fi

  if [ -f "$CODEX_CONFIG" ]; then
    backup="$CODEX_CONFIG.bak-agent-workspace-$(date +%Y%m%d%H%M%S)-$$"
    cp -p "$CODEX_CONFIG" "$backup"
    echo "Backed up Codex config to $backup"
  fi

  mv "$tmp" "$CODEX_CONFIG"
  chmod 600 "$CODEX_CONFIG" 2>/dev/null || true
  CODEX_MCP_CONFIG_CHANGED=1
  echo "Registered Codex MCP server '$CODEX_MCP_SERVER_NAME' in $CODEX_CONFIG"
}

install_skill() {
  local dest_dir dest
  dest_dir="$SKILLS_DIR/$SKILL_NAME"
  dest="$dest_dir/SKILL.md"

  if [ ! -f "$SKILL_SRC" ]; then
    echo "missing skill source: $SKILL_SRC" >&2
    return
  fi

  if [ "$DRY_RUN" -eq 1 ]; then
    echo "Would install skill '$SKILL_NAME' to $dest"
    return
  fi

  mkdir -p "$dest_dir"
  install -Dm644 "$SKILL_SRC" "$dest"
  echo "Installed skill '$SKILL_NAME' to $dest"
  echo "The skill is the lightweight entry point; workspace MCP tools load on demand instead of all at once."
}

warn_running_mcp_processes() {
  local matches

  if [ "$DRY_RUN" -eq 1 ] || { [ "$CONFIGURE_CODEX" -ne 1 ] && [ "$CLEAN_CODEX_CONFIG" -ne 1 ]; }; then
    return
  fi

  if ! command -v ps >/dev/null 2>&1; then
    return
  fi

  matches="$(
    while read -r pid command_path rest; do
      if [ -z "${pid:-}" ] || [ "$pid" = "$$" ] || [ -z "${command_path:-}" ]; then
        continue
      fi
      command_name="${command_path##*/}"
      if [ "$command_path" != "$DEST_BIN" ] && [ "$command_name" != "$BIN_NAME" ]; then
        continue
      fi
      case " $command_path ${rest:-} " in
        *" mcp "*) printf '%s %s %s\n' "$pid" "$command_path" "${rest:-}" ;;
      esac
    done < <(ps -eo pid=,args=)
  )"

  if [ -n "$matches" ]; then
    echo
    echo "Detected running $BIN_NAME MCP process(es):"
    printf '%s\n' "$matches"
    echo "Restart Codex or reload MCP servers now; running MCP processes keep their old tool schema, templates, and behavior until restarted."
  fi
}

if [ "$SKIP_BUILD" -eq 0 ]; then
  if [ "$DRY_RUN" -eq 1 ]; then
    echo "Would build release binary with cargo."
  else
    cargo build --manifest-path "$ROOT_DIR/Cargo.toml" --release
  fi
fi

if [ "$DRY_RUN" -eq 1 ]; then
  echo "Would install $SOURCE_BIN to $DEST_BIN"
else
  if [ ! -x "$SOURCE_BIN" ]; then
    echo "missing release binary: $SOURCE_BIN" >&2
    echo "Run without --skip-build, or build it first." >&2
    exit 1
  fi
  install -Dm755 "$SOURCE_BIN" "$DEST_BIN"
  echo "Installed $DEST_BIN"
fi

if [ "$INSTALL_SKILL" -eq 1 ]; then
  install_skill
fi

if [ "$CONFIGURE_CODEX" -eq 1 ]; then
  write_codex_config
elif [ "$CLEAN_CODEX_CONFIG" -eq 1 ]; then
  clean_codex_config
else
  echo "Skipped Codex MCP config; use the Codex for Linux Agent Workspaces page for app-owned setup, or rerun with --codex-configure for a generic MCP host."
fi

if [ "$RUN_DOCTOR" -eq 1 ]; then
  if [ "$DRY_RUN" -eq 1 ]; then
    echo "Would run $DEST_BIN doctor"
  else
    "$DEST_BIN" doctor
  fi
fi

if [ "$DRY_RUN" -ne 1 ]; then
  if [ "$CONFIGURE_CODEX" -eq 1 ]; then
    warn_running_mcp_processes
    echo "Restart Codex or reload MCP servers so new workspace tools, parameters, templates, and behavior become available."
  elif [ "$CLEAN_CODEX_CONFIG" -eq 1 ] && [ "$CODEX_MCP_CONFIG_CHANGED" -eq 1 ]; then
    warn_running_mcp_processes
    echo "Restart Codex or reload MCP servers so Agent Workspace disappears from generic MCP/configuration pages."
  fi
fi
