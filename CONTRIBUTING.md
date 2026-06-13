# Contributing

Thanks for taking a look. This project is pre-1.0, so small focused changes with
strong evidence are much easier to review than broad rewrites.

## Prerequisites

You need:

- **Rust toolchain** — install via [rustup](https://rustup.rs/)
- **System dependencies** (Debian/Ubuntu):

```bash
sudo apt install xvfb openbox xdotool xauth x11-utils imagemagick xclip \
    bubblewrap pkg-config libxkbcommon-x11-dev
```

## Build

```bash
cargo build --locked
cargo run -- doctor   # quick self-check
```

## Quality Gates

Run all four gates before pushing:

```bash
cargo fmt --check
cargo clippy --locked -- -D warnings
cargo test --locked
git diff --check
```

All four gates must pass. PRs that fail any gate will not be merged.

## Integration Smoke Test

For changes that touch runtime behaviour — MCP tool handling, permissions,
workspace control, viewer lifecycle, browser integration, or release gates — also
run the integration smoke:

```bash
scripts/integration_smoke.sh
```

When a real GPUI viewer is already open and you do not want the smoke to open a
second monitor:

```bash
AGENT_WORKSPACE_NO_NEW_VIEWER=1 scripts/integration_smoke.sh
```

## Project Layout

```
agent-workspace-linux/
├── src/                # Single Rust crate
│   ├── main.rs         # Binary entry point
│   ├── server.rs       # MCP server core
│   ├── workspace.rs    # Workspace lifecycle
│   ├── permissions.rs  # Permission ceiling enforcement
│   ├── viewer.rs       # GPUI live viewer
│   ├── browser.rs      # Workspace browser integration
│   ├── agent.rs        # Agent context helpers
│   ├── control.rs      # Live control state (active/read_only/paused)
│   ├── guardrails.rs   # Action guardrails
│   ├── profile.rs      # Profile management
│   ├── policy.rs       # Policy enforcement
│   └── approval.rs     # Human approval gate
├── scripts/            # Shell and Python smoke/QA scripts
├── skills/             # MCP skill definitions
└── docs/               # Additional documentation
```

## Pull Requests

- Keep PRs small and focused on a single concern.
- Include evidence that the change works: test output, smoke run excerpt, or
  `doctor` output as appropriate.
- Describe what changed and why; reviewers should not have to guess intent.
- All four gates plus `scripts/integration_smoke.sh` must pass for any change
  touching runtime behaviour.

## Release Model

Releases are tagged manually. A human final-diff review of all changes precedes
any version tag. `.github/workflows/release.yml` is the authoritative release
path: on version tags it builds and uploads GitHub Release binaries plus
matching `.sha256` sidecars, then publishes the npm wrapper from the same
workflow. There is no separate npm release workflow or automated release cut.
The npm wrapper's postinstall script treats the sidecar as part of the release
contract and fails installation if it is missing or does not match the binary.

## Public Hygiene

- Do not commit `target/`, `.codex/`, copied browser profiles, local MCP config,
  real account data, or generated release evidence.
- Keep browser/account dogfood reports metadata-only. Raw logged-in page text,
  links, headings, and account details do not belong in tracked files.
- Keep default MCP usage clean: no `--permissions` means no MCP-level ceiling;
  locked permission mode is opt-in through an explicit permissions file.
- Prefer repo-owned MCP/CLI evidence over host browser bridges or external
  automation when validating this runtime.
