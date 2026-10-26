# Security Policy

`agent-workspace-linux` runs Linux desktops in the background for agents. It is
not a sandbox: workspace apps run as the user, with the user's files, network,
keyring and SSH agent. Security reports should focus on actions reaching the
user's visible desktop, wrong read-only/write/destructive marks, app allowlist
bypasses, live-control bypasses, credential exposure in logs or events, or
real-world action approval failures.

## Supported Versions

| Version | Supported |
|---------|-----------|
| latest  | Yes       |

This project is pre-1.0. Security fixes target `main` and ship in the next
release.

## Reporting

Please report vulnerabilities **privately** using
[GitHub private security advisories](https://github.com/agent-sh/agent-workspace-linux/security/advisories/new)
for this repository. Do not open a public issue for security reports.

Do not include live credentials, copied browser profiles, private logs, or raw
account page contents in any report.

Helpful reports include:

- the commit or release you tested
- Linux distribution and display session type (X11 or Wayland)
- the exact MCP or CLI command used
- whether the MCP was started with `--permissions`
- the output of `agent-workspace-linux doctor`, with local paths redacted if needed
- the smallest reproduction that shows the failure

## Trust Model

- **Control socket**: the workspace control socket is a same-uid Unix socket
  with mode 0600. It provides no cross-user protection by design — any process
  running as the same UID can connect.
- **Not a sandbox**: an app launched in a workspace can do anything you can.
  Agents act for you; the controls below decide what they may do.
- **Action marks**: every MCP tool is marked read-only, write, destructive or
  open-world, so the agent host can ask before risky actions.
- **App allowlist**: with a `--permissions PATH` file (or the
  `AGENT_WORKSPACE_PERMISSIONS` environment variable), `apps.allow` limits which
  programs agents may launch for the life of that MCP process, enforced at both
  the MCP front end and the workspace daemon's IPC socket. Without it, the MCP
  adds no ceiling of its own; the host/client harness owns approval.
- **Live viewer control**: the GPUI monitor allows a human operator to pause or
  stop agent actions. This is best-effort — it does not provide a hard
  cryptographic guarantee against a racing agent action before the control
  signal is received.
- **Workspace vs. desktop**: workspace input, screenshots, windows, clipboard,
  and browser control target the workspace, not the user's visible desktop or
  host Chrome. Leakage onto the desktop is a reportable bug.
- **Browser profiles**: browser-session profiles are for explicitly
  user-approved browser data only. Use copied/disposable profiles for
  real-account dogfood whenever possible.
