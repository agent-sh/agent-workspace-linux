# Permission Boundary

How the workspace permission boundary works today, and what is still planned.

A configured permission ceiling — set with `--permissions PATH` or the
`AGENT_WORKSPACE_PERMISSIONS` environment variable — is enforced, not aspirational:
the MCP front-end rejects any request that exceeds it, and the workspace daemon
re-enforces it on every IPC request, so workspace-launched apps and other
same-uid callers are held to the same ceiling. This is implemented and covered by
tests.

With no ceiling configured, the server imposes no boundary of its own and defers
to the host tool's approval flow (Claude Code, Codex, and similar). In that mode
the acknowledgement parameters and approval bundles are API shape and audit
metadata for the host UI; a richer human-approval experience in Codex for Linux
is the part still being built. Live viewer control (read_only/paused) is a
runtime convenience, not an authoritative boundary; mutating daemon IPC fails
closed if the shared control state cannot be read.

See [Status and remaining work](#status-and-remaining-work) for what is done and
what is still planned.

## Target Authority Model

There are two permission owners.

### UI-Owned Mode

When the MCP is spawned without permission fields, or a permission dimension is
empty, that dimension is open at the MCP layer. Codex for Linux owns the user
approval flow, profile editing UX, and per-session decisions.

This mode is intended for the Codex app integration and for developer dogfood.
The app can request workspace starts, mounts, network modes, setup commands, and
apps through its own UI approval flow.

The default MCP spawn path with no `--permissions` file stays in this mode: the
  server does not invent a second permission ceiling. It reports
  `configured=false` through `mcp_permissions`, points agents back to the
  host/client harness boundary, and uses `mcp_action_catalog` only as advisory
  classification for read-only, mutating, destructive, idempotent, and
  host-visible/open-world actions. Catalog `parameter_notes` are also advisory:
  they explain risk-changing arguments such as `dry_run`, `replace`,
  `output_path`, and `kill_on_timeout`, but they do not narrow default clean
  usage.

If the user grants the Codex session full access, UI-owned mode must respect
that choice. The user should approve the hidden workspace once, because it is a
separate agent-controlled environment that the user may not be looking at.
After that approval, workspace-local actions such as launching apps, sending
input, taking screenshots, and stopping the workspace should run without a
second generic permission prompt storm. The runtime still scopes those actions
to the hidden workspace display/IPC and still enforces any profile policy that
was selected for the workspace.

### MCP-Locked Mode

When the MCP is spawned with permission fields through
`agent-workspace-linux mcp --permissions PATH`, those fields form a ceiling that
is enforced for the lifetime of that MCP server process at two layers: the MCP
front-end rejects requests exceeding the ceiling, and the workspace daemon
re-enforces the ceiling on every IPC request, including requests from
workspace-launched apps and other same-uid callers. Codex for Linux may show
the policy, request narrower access, and operate inside it, but it cannot
broaden or rewrite it.

This mode supports non-Codex hosts such as Claude Code, auto-looping agents, and
headless workflows where the user preconfigures permissions in MCP config.

Example shape:

```json
{
  "network": {
    "mode": "local_only",
    "allow_hosts": ["localhost:3000"]
  },
  "mounts": [
    {
      "host_path": "/home/me/project",
      "workspace_path": "/workspace/project",
      "mode": "read_write"
    }
  ],
  "apps": {
    "allow": ["/usr/bin/firefox", "/usr/bin/xterm", "/usr/bin/npm"]
  }
}
```

Possible MCP config shape:

```json
{
  "mcpServers": {
    "agent-workspace-linux": {
      "command": "/home/me/.local/bin/agent-workspace-linux",
      "args": [
        "mcp",
        "--permissions",
        "/home/me/.config/agent-workspace-linux/permissions.json"
      ]
    }
  }
}
```

`./install.sh --permissions PATH` is an explicit opt-in path that writes this
locked MCP registration for generic Codex MCP-host workflows without
hand-editing `config.toml`. Running `./install.sh` without `--permissions` or
`--codex-configure` does not edit Codex MCP config; Codex for Linux should use
the dedicated Agent Workspaces feature page to own permission-file mutation,
restart/reconnect, and user-visible control instead of surfacing this backend
through the generic MCP settings page. `./install.sh --clean-codex-config`
removes stale `agent-workspace-linux` MCP server and nested tool tables from
older installs.

Rules:

- Missing or empty permission fields mean no MCP-level ceiling for that
  dimension.
- A full-access Codex session can use open dimensions without extra MCP-level
  prompts after hidden-workspace approval, but it cannot widen a populated
  spawn-time ceiling.
- Prefilled `network` is the maximum network access available to all workspace
  starts and launches.
- Prefilled `mounts` are the maximum file access. A UI may narrow mounts or
  downgrade read-write to read-only, but cannot add broader paths or upgrade
  access.
- Prefilled app allowlists limit launchable commands. The UI may show friendlier
  app pickers, but launches outside the ceiling are rejected. The allowlist
  matches the launched program, not its arguments; allowing shells, package
  managers, or browsers delegates follow-on behavior to that program inside the
  workspace policy.
- Spawn-time ceiling dimensions cannot be broadened without restarting the MCP
  server with new config.
- The active ceiling is visible through the read-only `mcp_permissions` tool.
- Agents and non-Codex hosts can call read-only `mcp_session_brief` for the
  active ceiling, live control mode, headless state, runtime readiness, known
  workspaces/profiles, compact live/stopped app activity, inferred task intent
  from profile/app activity, and suggested next MCP actions before attempting
  mutating tools. The brief now classifies each recommendation with action type,
  idempotency, and compact approval checkpoints, then derives read-only
  `mcp_task_plan` recommendations from saved profiles and runtime state,
  nudging agents toward app QA, browser/shopping/grocery, observe, or cleanup
  plans before direct mutation. Brief recommendations and the top-level brief
  now expose `approval_summary`, so hosts can show the first recommendation
  boundary before making a second planner call. They can also call
  `mcp_task_plan` directly with those intents to get a safe preview sequence,
  approval hints, and structured `approval_checkpoints` before executing real
  actions. These checkpoints let a
  host render required input, dry-run approval surfaces, profile writes, hidden
  workspace starts, live-control blockers, host-visible UI, permission blockers,
  destructive actions, and separate real-world approvals without scraping prose.
  Live-control checkpoints include the exact `confirmed_user_request=true`
  reactivation input for `mcp_control_update mode=active`.
  Plans also include `task_context` with normalized task kind, target workspace,
  provided inputs, missing inputs, safety boundaries, action boundaries, and
  approval kinds, so user-intent derivation is machine-readable for host UI and
  agent loops. `approval_summary` provides the host-renderable next boundary:
  blocking count, approval-required count, all approval kinds, and the first
  blocking or approval-required checkpoint so Codex Desktop and non-Codex hosts
  do not need to reimplement checkpoint ordering before showing a prompt.
  App-QA plans generated from natural testing phrases or a project path now
  carry through reviewed profile save, approved profile start, and read-only
  observation. Their action boundaries separately classify observation,
  hidden workspace start/attach, evidence collection, workspace-local input,
  and mounted project file writes; `project_file_write` stays an explicit
  approval class instead of being inferred from generic app interaction.
  Browser/shopping plans now carry through to the approved browser-profile run
  and read-only observation step, with explicit real-world approval text for
  checkout, purchases, or account changes; viewer steps are offered only when
  the plan has a concrete browser workspace run step and the MCP is not
  headless.
  Shopping/grocery intents also request task details (`target_url`,
  `shopping_list`, `fulfillment`, `substitution_policy`, and `budget`) as
  required input rather than permission blockers, so a clean/default MCP remains
  harness-owned unless `--permissions` is explicitly configured. Their action
  boundaries separate observe, navigation/search, item comparison, cart
  mutation, and checkout/account changes; cart mutation has its own approval
  class and final checkout/account changes stay real-world approvals. Grocery
  action boundaries now also report explicit approval state and missing
  approvals, so a host can record `cart_mutation_approved=true` separately from
  `real_world_action_approved=true`. Cleanup plans
  now pair dry-run preview with the destructive follow-up and verification
  step. Fresh-start and already-running app-QA/browser plans collect read-only
  evidence before input by reading recent workspace events and waiting for a
  stable app/window target before app logs or focused screenshots. If the target
  workspace is already running, the plan continues from that live workspace
  instead of starting another profile. Browser/shopping plans still expose the
  separate real-world approval boundary. Generated project-dev and
  browser-session profile steps plus saved-profile preview/run steps are
  preflighted against the active ceiling and expose permission blockers when
  the configured MCP cannot support that workflow. The integration
  smoke also runs a clean/default MCP JSON-RPC pass with no `--permissions`
  file and asserts that app-QA/browser plans stay free of permission blockers
  under the harness-owned boundary. The locked-permissions MCP smoke now also
  covers live `read_only` and `paused` control: dry-run previews and read-only
  profile returns remain available, real workspace starts are blocked, and
  host-output writes such as `profile_export.output_path` do not create files
  until control returns to `active`. Reactivating mutating agent actions through
  `mcp_control_update` requires `confirmed_user_request=true` when the current
  mode is `read_only` or `paused`, and session briefs carry the control update
  actor, timestamp, and reason for host UI and agent explanation.
- The permission ceiling (the authoritative boundary) is enforced at two layers:
  the MCP front-end (profile template/check/validate/put/import, workspace
  start/open-profile, direct launch/run, and profile setup/startup launches) and
  the workspace daemon IPC socket (every IPC request, including those from
  workspace-launched apps and other same-uid callers). Live control state
  (read_only/paused) is a separate runtime convenience layer: the daemon honors
  a runtime pause when it can read the shared control state and fails closed for
  mutating IPC if it cannot, while read-only inspection and the safety stop stay
  available. It is still not the authoritative security boundary. The standalone
  CLI can also generate and validate ceiling files for hosts that do not have the
  Codex for Linux UI.
- The CLI also accepts a leading `--permissions PATH` global option. When used,
  profile and workspace actions are checked against the same ceiling. This is
  intended for the Codex for Linux bridge when it discovers a locked MCP server
  config and needs to avoid bypassing that ceiling.

## Status and remaining work

The permission ceiling is enforced today at both the MCP front-end and the
workspace daemon. The items below track what is validated and the gaps that
remain (status as of 2026-06-02):

- A is validated for the current X11/bubblewrap runtime surface covered by the
  integration smoke. Real MCP dogfood and `scripts/integration_smoke.sh` have
  covered Chrome, native browser text input, local-dev browser QA, mounted GUI
  editor save-through, synthetic browser-session startup/observe/mounted-profile
  write-through, Codex desktop feature tests, disabled networking, local-only
  networking, read-only/read-write mounts, setup/startup commands, screenshots,
  window targeting, input,
  clipboard, app logs, events, manifests, stop, stale cleanup, daemon-crash
  recovery, self-stop from inside
  a workspace app, direct MCP stop/revoke cleanup, and consistent workspace
  discovery when a Codex/MCP launcher omits `XDG_RUNTIME_DIR`, and MCP daemon
  child cleanup so stopped workspaces do not leave zombies under a long-lived
  MCP process. A
  Codex-spawned MCP pass on this repository also revalidated project-dev
  mounts, Rust toolchain access, GUI input, events, Chrome, and current network
  enforcement. `cargo test` currently passes 149 tests.
- A still has known product gaps: host-localhost bridging for `local_only` and
  more varied real-project coverage. Broad network allowlists and egress proxy
  filtering are out of scope for this pass; the product network model is
  closed, local, or open.
- B has pivoted away from the earlier Codex conversation embed. The
  runtime-owned GPUI viewer is the intended serious UI surface, and the Codex
  Desktop feature should stay a thin launcher/settings bridge that does not
  revive the embedded screenshot panel. The viewer now provides a compact
  host-visible window with workspace observation, screen streaming, task/isolation
  footer context, app/event/log/artifact affordances, live read-only/paused/active
  control, Stop/Clean/Revoke paths, persisted size/position, default non-topmost
  behavior, and opt-in topmost behavior. `workspace_doctor` reports
  hidden-workspace readiness separately from host-visible viewer readiness so
  desktop sessions and headless hosts can be diagnosed without guessing. B still
  needs broader compositor validation and final UI approval review before it
  should become a hard trust boundary.
- C is partially covered. Desktop QA, local-dev browser QA, arbitrary startup
  app configuration, PID-less arbitrary app window targeting, and
  recovery/inspection flows work at the primitive level.
  MCP-locked permission ceilings and app allowlists have a first MCP-enforced
  slice, and `./install.sh --permissions PATH` now gives locked MCP hosts an
  explicit setup path without hand-editing Codex config. The default installer
  stays skill-first and leaves generic Codex MCP settings untouched. The CLI also has
  `permissions template open|closed|local` and
  `permissions validate --json PATH` so non-Codex hosts can generate and check a
  ceiling before spawning MCP. The Codex for Linux app picker now accepts both
  executable files and `.desktop` launchers, parsing launcher `Name`/`Exec`
  fields into startup app commands without a shell.
  Authenticated browser-profile sharing now has a `browser-session` starter
  template, a first Codex for Linux picker/copy/lock-warning flow for explicitly
  user-approved browser data directories, and an opt-in real-profile dogfood
  helper for Slack/GitHub. The installed MCP path has also proven that template
  end to end with a synthetic Chrome profile: approval preview, real startup,
  visible Chrome window, mounted browser-data read/write, screenshots, read-only
  observation, workspace-owned browser target discovery, page snapshot,
  navigation, browser action events, artifacts, stop, profile deletion, and stale
  cleanup. Real-account dogfood still requires a human-approved profile
  directory, but the remaining human step is now explicit and reproducible:
  `node scripts/mcp_real_profile_browser_session_dogfood.js --approved-user-data-dir ~/.config/google-chrome --site github`
  or `--site slack`.

### A. Runtime claims validated with real workloads

These runtime claims hold under real usage:

- Validated: Chrome/Chromium launches inside the agent workspace and is
  controllable through workspace-local window, keyboard, and paste operations
  without stealing the host desktop.
- Validated: Chrome/Chromium launches inside the agent workspace with an
  ephemeral `DevToolsActivePort` endpoint. `workspace_browser_targets`,
  `workspace_browser_snapshot`, `workspace_browser_search_results`, and
  `workspace_browser_navigate` expose target discovery, page readback,
  structured search/product card extraction, GPU VRAM filtering, and navigation
  through the repo-owned MCP by deriving the endpoint from the running workspace
  app and approved/copied browser profile path. Browser tool responses warn if
  activity events cannot be recorded in the workspace event log, so stale
  runtime/version skew is visible to the agent and user. This lets
  browser/shopping automation inspect and navigate the workspace browser
  through MCP tools instead of attaching to the user's host Chrome bridge or
  using external browser-control workarounds.
- Opt-in real-profile dogfood command: run
  `node scripts/mcp_real_profile_browser_session_dogfood.js --approved-user-data-dir PATH --site github|slack`
  after approving a stopped Chrome/Chromium user-data directory. The helper
  refuses obvious active-profile hazards (`Singleton*` or `DevToolsActivePort`),
  validates `Local State`, copies the approved directory to a disposable profile,
  generates the current `browser-session` template against that copy, opens it
  through `workspace_open_profile`, and proves logged-in page access with
  `workspace_browser_targets` plus `workspace_browser_navigate`/snapshot. The
  evidence reports a loopback workspace DevTools endpoint and does not call the
  host Chrome bridge.
- Validated: workspace QA has run against this repo, Codex for Linux, and
  `agent-chrome-bridge`, including local dev server/browser paths and
  project-mounted test commands.
- Validated: `network.mode=disabled` blocks external socket/DNS access from
  workspace-launched commands and browser windows when bubblewrap is available.
- Validated: `network.mode=local_only` allows sandbox loopback while blocking
  external network access. Host-localhost bridging remains a documented
  limitation.
- Validated: read-write mounts accept writes and read-only mounts reject writes
  through the bubblewrap mount namespace.
- Validated: screenshots, window listing, input, clipboard, app logs, events,
  artifacts, stop, stale cleanup, and stopped-manifest inspection work across
  successful and failed app launches.
- Validated: daemon-crash recovery removes manifest-recorded orphan app process
  groups and X11 runtime processes.
- Keep the user-facing network model to closed, local, or open. Do not treat
  broad host allowlists or egress proxy filtering as part of this gate.

### B. Provide Native Workspace Visibility

The user should be able to see and control what the agent is doing without
depending on a Codex-only conversation embed:

- Use the runtime-owned GPUI viewer as the canonical visible surface.
- Keep the viewer small, movable, resizable, readable, and not always-on-top by
  default; topmost behavior must stay an explicit opt-in.
- Show when a hidden workspace is active, which profile/policy is applied, which
  app/window is active, and what kind of task the agent appears to be doing.
- Provide obvious live controls for read-only, paused, active, Stop, Clean, and
  Revoke, while keeping safety stop available even when mutation is paused.
- Surface screenshots or live view updates from the workspace only when the user
  enables that stream, and reuse frame files so polling does not create an
  unbounded screenshot pile.
- Keep Codex Desktop as a thin settings/launcher integration that opens the
  native viewer and preserves configured MCP ceilings.

### C. Validate Capability Coverage

The primitive set should cover the optional tasks users may reasonably ask for:

- Desktop app QA with project mounts, setup commands, local dev servers, browser
  testing, screenshots, logs, and cleanup.
- Browser-centered tasks such as shopping or web workflows, including the
  `browser-session` starter for mounted browser data when the user explicitly
  grants that environment.
- Arbitrary apps chosen through file pickers or configured startup apps,
  including Linux `.desktop` launchers in the Codex for Linux app picker.
- Long-running auto-loop agents that need preconfigured network/file/app
  ceilings without Codex-specific UI.
- Recovery and inspection flows: list active/stopped workspaces, inspect
  artifacts, read logs/events, stop, cleanup, and delete saved environments.

The ceiling is enforced today; the remaining work above (broader viewer
compositor coverage, the host-side human-approval UX, and capability breadth) is
tracked so any gaps are known product limits rather than permission-system
surprises.
