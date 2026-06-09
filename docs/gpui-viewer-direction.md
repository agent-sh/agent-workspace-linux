# GPUI Viewer Direction

Last updated: 2026-06-08

`agent-workspace-linux` owns the primary visible Agent Workspace monitor. Codex
Desktop and other MCP hosts should be thin launchers/settings surfaces, while
the runtime provides the reusable viewer, lifecycle registry, and workspace
control contract.

## Product Shape

- `agent-workspace-linux mcp` runs the stdio MCP server.
- `agent-workspace-linux viewer` opens the native GPUI monitor directly.
- `workspace_start` and `workspace_open_profile` open a target-bound viewer by
  default when the server is not headless and
  `workspace_doctor.ready_for_host_viewer=true`.
- `workspace_open_viewer` reopens or reuses a target-bound viewer explicitly.
- The viewer runs as a child/subcommand process, not inside the MCP stdio event
  loop.

The default viewer is a compact, square-ish, screen-first monitor for passive
work observation. It is draggable from the header area, resizable from the
bottom-right grip, and persists size, position, screen-stream preference, and
footer mode in the user's XDG config directory.

## Viewer Contract

- The default viewer opens unless the MCP is started with `--headless`, the host
  display is unavailable, or the tool call sets `open_viewer=false`.
- The default viewer does not request always-on-top state.
- `--always-on-top` and `workspace_open_viewer(always_on_top=true)` are opt-in
  overlay modes for hosts or users that explicitly ask for that behavior.
- `workspace_open_viewer` reuses a compatible existing registered viewer for the target
  workspace instead of opening duplicate GPUI windows. An
  `input_forwarding=true` request may open a separate input-capable viewer when
  only a read-only monitor is already running; later read-only opens can reuse
  that existing input-capable viewer without arming input.
- `--input-forwarding` and
  `workspace_open_viewer(input_forwarding=true)` opt a viewer process into
  manual read-write capability. Forwarding is still disabled when the window
  appears; the human must click the in-viewer `Input` control twice to arm it.
  All forwarded mouse, keyboard, scroll, and paste events go through
  workspace-owned IPC for the selected workspace, never the host desktop.
- MCP-opened viewers pass `--exit-when-workspace-gone`, so monitors opened for a
  task disappear when their selected workspace runtime is removed.
- Direct `agent-workspace-linux viewer` launches remain persistent, so they can
  act as a reusable local monitor.
- `viewer list` / `workspace_list_viewers` and `viewer close` /
  `workspace_close_viewer` are the repo-owned recovery surface for orphan or
  compositor-invisible viewer windows.

The viewer refreshes workspace status, app state, event state, and live-control
state without constantly capturing screen pixels. Screen streaming is off by
default; enabling `View` overwrites one reusable frame file instead of creating a
new timestamped screenshot per refresh. The footer favors user-useful context:
in-flight viewer actions, latest workspace events, browser reads/navigations,
active app/window, inferred task intent, and permission/isolation state.

Manual input forwarding is deliberately separate from MCP live control. The MCP
can be `active` while the viewer remains monitor-only, and an input-capable
viewer still starts with forwarding off. When enabled, the viewer maps panel
coordinates back through the rendered screenshot's `ObjectFit::Cover` crop/scale
using the source screenshot dimensions before dispatching `click`,
`move_pointer`, `drag`, `scroll`, `key`, `type_text`, or `paste_text` IPC
actions.

## Agent Boundary

Agents should treat the viewer as host-visible/open-world UI. It is available
when the user or host wants to watch, pause, resume, or inspect the workspace.
It should not be used as release evidence unless the evidence was collected
through the repo-owned runtime/MCP paths, not through Codex Desktop, Computer
Use, Playwright, or a host browser bridge.

Browser work should use Chrome/Chromium launched inside the workspace and the
workspace-owned MCP browser tools:

- `workspace_browser_targets`
- `workspace_browser_snapshot`
- `workspace_browser_search_results`
- `workspace_browser_navigate`

This keeps browser automation inside the workspace profile, permission, event
log, and audit boundary.

## Viewer Smoke

Exercise the GPUI viewer locally with:

```bash
scripts/gpui_viewer_smoke.sh
```

It starts a hidden workspace, opens the monitor window through X11/Xwayland, and
checks the default and opt-in always-on-top window states, compact sizing,
duplicate-launch reuse, and target-bound viewer teardown. It needs an X11 or
Xwayland display plus `xclock`, `xdotool`, `xwininfo`, `xprop`, `xwd`, and
ImageMagick (`convert`/`identify`).
