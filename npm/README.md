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

## Source and full documentation

All source code, tool documentation, and issue tracking are at:
**<https://github.com/agent-sh/agent-workspace-linux>**

## License

MIT
