# @agent-sh/agent-workspace-linux

npm distribution wrapper for the [`agent-workspace-linux`](https://github.com/agent-sh/agent-workspace-linux) MCP server — isolated Linux desktop workspaces for AI agents.

## Installation

```sh
npm install -g @agent-sh/agent-workspace-linux
```

The installer automatically downloads the prebuilt binary for your architecture
from the matching [GitHub Release](https://github.com/agent-sh/agent-workspace-linux/releases),
then verifies the required `<asset>.sha256` sidecar before installing it.

**Linux only.** `x64` (x86_64) and `arm64` (aarch64) are supported.

> **Note — `--ignore-scripts`:** package managers that skip lifecycle scripts (e.g. `pnpm` with `ignore-scripts=true`, some CI setups) will not download the binary automatically. Run the postinstall manually to recover:
> ```sh
> node $(npm root -g)/@agent-sh/agent-workspace-linux/scripts/postinstall.js
> ```

Release downloads accept absolute or relative HTTPS redirect locations, with
a maximum of five redirect hops. Malformed URLs and non-HTTPS redirect targets
fail the download; redirects do not bypass the required checksum verification.

Each binary or checksum download has a 60-second inactivity timeout while waiting
for response headers or body progress. The timer resets whenever bytes arrive, so
a slow transfer that is still making progress can continue. A separate 30-minute
hard ceiling only bounds pathological transfers. Timeout failures close the active
output and remove its partial file before a retry can begin.

Checksum sidecars must contain a valid 64-hex-digit SHA-256 digest and name the
expected release asset. Space and tab separators are accepted. A bare digest or
an entry naming a different asset is rejected.

The postinstall script applies executable permissions to the verified staging
file before replacing an installed binary. If permission preparation fails, it
reports failure and leaves the previously installed binary untouched.

Each install uses its own uniquely created staging directory beside the binary.
Concurrent installs do not remove or overwrite one another's downloads. Normal
completion and errors clean up the attempt's own staging directory; forced
termination can leave an abandoned `.staging-*` directory. A later install does
not delete it automatically, because another installer may still be using it.

## Usage

Once installed, the server is on your PATH (the command stays unscoped):

```sh
agent-workspace-linux
```

It is an [MCP](https://modelcontextprotocol.io/) server that speaks JSON-RPC over stdio. For Codex for Linux, prefer the dedicated **Agent Workspaces** feature page so command paths, permission rules, and reconnect/restart control stay out of the generic MCP settings page. If an older Codex install still shows this backend in generic MCP/configuration pages, remove the stale `agent-workspace-linux` MCP tables before reconnecting through the feature page. For other MCP clients, wire it into the client config, e.g. for Claude Code:

```json
{
  "mcpServers": {
    "agent-workspace-linux": {
      "command": "agent-workspace-linux",
      "args": []
    }
  }
}
```

The command-line wrapper preserves the native program's exit code and terminating
signal. In particular, a child terminated by SIGINT causes the wrapper to
terminate by SIGINT too, rather than report exit code zero.

## Source and full documentation

All source code, tool documentation, and issue tracking are at:
**<https://github.com/agent-sh/agent-workspace-linux>**

## License

MIT
