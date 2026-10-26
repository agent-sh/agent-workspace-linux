# Permission model

A workspace is computer use that runs in the background. It is not a sandbox.
Apps launched in it run as you, with your files, network, keyring and SSH
agent; only their windows stay in the workspace. So the model decides what an
agent may do, not what it can reach.

Three controls do that:

1. **Action marks.** Every MCP tool is marked read-only, write, destructive or
   open-world, in its MCP annotations and in `mcp_action_catalog`. The agent
   host (Claude Code, Codex, ...) uses the marks to decide what to ask you
   about.
2. **The app allowlist.** An optional ceiling file limits which programs agents
   may launch.
3. **Live control.** The viewer, or `mcp_control_update`, puts the agent in
   `read_only` or `paused` at any time.

## Host-owned mode (the default)

With no `--permissions` file, the server sets no ceiling of its own. The
agent host owns approval, including full-access sessions. `mcp_permissions`
reports `configured=false` and points agents back to the host's boundary.

You approve the hidden workspace once (`--ack-hidden-workspace`,
`acknowledge_hidden_workspace=true`), because it is a desktop you may not be
looking at. After that, workspace-local actions such as launching apps,
sending input, taking screenshots and stopping the workspace run without a
second prompt per action.

## The app allowlist

Spawn the server with a ceiling file to limit launches:

```json
{
  "apps": {
    "allow": ["/usr/bin/firefox", "/usr/bin/xterm", "/usr/bin/npm"]
  }
}
```

```json
{
  "mcpServers": {
    "agent-workspace-linux": {
      "command": "/home/me/.local/bin/agent-workspace-linux",
      "args": ["mcp", "--permissions", "/home/me/.config/agent-workspace-linux/permissions.json"]
    }
  }
}
```

- The ceiling holds for the life of the MCP process. Clients may narrow it but
  not widen it; only a restart with a new file changes it.
- It is enforced twice: by the MCP front end (profile template, check,
  validate, put and import, workspace start and open-profile, launch and run,
  profile setup and startup apps) and by the workspace daemon on every IPC
  request, so apps inside the workspace and other same-user callers are held
  to it too.
- It matches the launched program, not its arguments. Allowing a shell,
  interpreter, package manager or browser allows what that program goes on to
  do. `mcp_permissions` lists such entries under `advisories`.
- An empty or missing `apps.allow` means no ceiling on launches.
- `network` and `mounts` in ceiling files written before 0.6.0 are kept in the
  reported ceiling so a client can show what was saved, and are not enforced.
  Each one adds an advisory that says so.

The CLI writes and checks ceiling files:

```bash
agent-workspace-linux permissions template open --app /usr/bin/xterm
agent-workspace-linux permissions validate --json permissions.json
```

A leading `--permissions PATH` on other CLI commands checks profile and
workspace actions against the same file. Codex for Linux uses this when it
finds a locked MCP registration.

`./install.sh --permissions PATH` writes such a registration for generic Codex
MCP hosts. Plain `./install.sh` does not edit Codex MCP config; Codex for
Linux owns the permission file from its Agent Workspaces page.
`./install.sh --clean-codex-config` removes stale registrations from older
installs.

## Live control

`read_only` and `paused` block mutating agent actions and keep inspection and
the safety stop available. The daemon honors them when it can read the shared
control state, and fails closed for mutating IPC when it cannot. Going back to
`active` through `mcp_control_update` needs `confirmed_user_request=true`.

Live control is a convenience for the human watching. The app allowlist is
the hard boundary.

Control-state writes go to an exclusively created staging file and are
published by atomic rename. Concurrent setters follow last-rename-wins.
Atomic publication does not promise durability across power loss.

## Planning before acting

`mcp_session_brief` returns the active ceiling, live control mode, runtime
readiness, known workspaces and profiles, recent app activity and suggested
next actions. `mcp_task_plan` turns an intent (app QA, browser or shopping,
observe, cleanup) into a preview-first sequence with `approval_checkpoints`
and an `approval_summary`, so a host can show the next approval without
parsing prose. Shopping plans keep cart changes and checkout or account
changes as separate approvals.

## Old profiles

Profiles written before 0.6.0 may carry `mounts`, `network` and
`require_enforced_policy`. They still load; `profile check` and `profile
validate` report one warning per field, and the next save drops them.
`--ack-unenforced-policy` and `acknowledge_unenforced_policy` are accepted
and ignored.
