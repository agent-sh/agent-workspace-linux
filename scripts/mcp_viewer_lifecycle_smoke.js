#!/usr/bin/env node
"use strict";

const childProcess = require("node:child_process");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

const repoRoot = path.resolve(__dirname, "..");
const bin =
  process.env.AGENT_WORKSPACE_BIN ||
  path.join(repoRoot, "target", "debug", "agent-workspace-linux");

if (!fs.existsSync(bin)) {
  throw new Error(`agent-workspace-linux binary not found at ${bin}; run cargo build first`);
}

const tempDir = fs.mkdtempSync(path.join(os.tmpdir(), "awl-vlc-"));
const configDir = path.join(tempDir, "c");
const runtimeDir = path.join(tempDir, "r");
fs.mkdirSync(configDir, { recursive: true });
fs.mkdirSync(runtimeDir, { recursive: true, mode: 0o700 });
fs.chmodSync(runtimeDir, 0o700);
fs.chmodSync(runtimeDir, 0o700);

const env = {
  ...process.env,
  XDG_CONFIG_HOME: configDir,
  XDG_RUNTIME_DIR: runtimeDir,
};

const child = childProcess.spawn(bin, ["mcp"], {
  cwd: repoRoot,
  env,
  stdio: ["pipe", "pipe", "pipe"],
});

let nextId = 1;
let stdoutBuffer = "";
let stderr = "";
const pending = new Map();

child.stderr.on("data", (chunk) => {
  stderr += String(chunk);
});

child.stdout.on("data", (chunk) => {
  stdoutBuffer += String(chunk);
  for (;;) {
    const newlineIndex = stdoutBuffer.indexOf("\n");
    if (newlineIndex === -1) return;
    const line = stdoutBuffer.slice(0, newlineIndex).trim();
    stdoutBuffer = stdoutBuffer.slice(newlineIndex + 1);
    if (!line) continue;
    let message;
    try {
      message = JSON.parse(line);
    } catch {
      abort(`invalid JSON-RPC line from MCP server: ${line}`);
    }
    const slot = pending.get(message.id);
    if (!slot) continue;
    pending.delete(message.id);
    clearTimeout(slot.timer);
    if (message.error) {
      slot.reject(new Error(JSON.stringify(message.error)));
    } else {
      slot.resolve(message.result);
    }
  }
});

child.on("exit", (code, signal) => {
  for (const slot of pending.values()) {
    clearTimeout(slot.timer);
    slot.reject(
      new Error(
        `MCP server exited before ${slot.method} response, code=${code}, signal=${signal}, stderr=${stderr}`,
      ),
    );
  }
  pending.clear();
});

function fail(message) {
  throw new Error(message);
}

function abort(message) {
  try {
    child.kill("SIGTERM");
  } catch {
    // ignore cleanup races
  }
  throw new Error(message);
}

function request(method, params, timeoutMs = 5000, label = method) {
  const id = nextId++;
  child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id, method, params })}\n`);
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      pending.delete(id);
      reject(new Error(`timed out waiting for ${label}`));
    }, timeoutMs);
    pending.set(id, { resolve, reject, timer, method: label });
  });
}

function notify(method, params) {
  child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", method, params })}\n`);
}

async function callTool(name, args, timeoutMs) {
  const result = await request(
    "tools/call",
    {
      name,
      arguments: args || {},
    },
    timeoutMs,
    `tools/call ${name}`,
  );
  if (result.isError) {
    throw new Error(`tool ${name} returned MCP error: ${JSON.stringify(result)}`);
  }
  if (result.structuredContent && typeof result.structuredContent === "object") {
    return result.structuredContent;
  }
  const text = result.content?.find((entry) => entry?.type === "text")?.text;
  return text ? JSON.parse(text) : null;
}

function assert(condition, message) {
  if (!condition) fail(message);
}

function pidExists(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

async function poll(label, fn, timeoutMs = 5000) {
  const deadline = Date.now() + timeoutMs;
  let lastValue;
  while (Date.now() < deadline) {
    lastValue = await fn();
    if (lastValue) return lastValue;
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  throw new Error(`timed out waiting for ${label}; last value: ${JSON.stringify(lastValue)}`);
}

async function closeAllViewers() {
  try {
    await callTool("workspace_close_viewer", { all: true }, 5000);
  } catch {
    childProcess.spawnSync(bin, ["viewer", "close", "--all"], {
      cwd: repoRoot,
      env,
      stdio: "ignore",
    });
  }
}

async function main() {
  const workspaceId = `vlc-${process.pid}`;
  const initializeResult = await request("initialize", {
    protocolVersion: "2024-11-05",
    capabilities: {},
    clientInfo: { name: "agent-workspace-linux-viewer-lifecycle-smoke", version: "0" },
  });
  notify("notifications/initialized", {});
  assert(
    /workspace_list_viewers/.test(String(initializeResult.instructions || "")) &&
      /workspace_close_viewer/.test(String(initializeResult.instructions || "")),
    `initialize instructions should expose repo-owned viewer lifecycle controls: ${initializeResult.instructions}`,
  );

  const tools = await request("tools/list", {});
  const toolByName = new Map((tools.tools || []).map((tool) => [tool.name, tool]));
  assert(toolByName.has("workspace_open_viewer"), "tools/list missing workspace_open_viewer");
  assert(toolByName.has("workspace_list_viewers"), "tools/list missing workspace_list_viewers");
  assert(toolByName.has("workspace_close_viewer"), "tools/list missing workspace_close_viewer");

  const catalog = await callTool("mcp_action_catalog");
  const catalogByName = new Map((catalog.tools || []).map((tool) => [tool.name, tool]));
  assert(
    catalogByName.get("workspace_list_viewers")?.read_only === true,
    `workspace_list_viewers should be read-only in catalog: ${JSON.stringify(catalogByName.get("workspace_list_viewers"))}`,
  );
  assert(
    catalogByName.get("workspace_close_viewer")?.open_world === true &&
      /viewer/.test(catalogByName.get("workspace_close_viewer")?.control_behavior || ""),
    `workspace_close_viewer should be viewer-control open-world in catalog: ${JSON.stringify(catalogByName.get("workspace_close_viewer"))}`,
  );

  const started = await callTool(
    "workspace_start",
    {
      id: workspaceId,
      acknowledge_hidden_workspace: true,
      purpose: "Direct MCP GPUI viewer lifecycle smoke",
      width: 800,
      height: 500,
    },
    15000,
  );
  assert(started.ok === true, `workspace_start failed: ${JSON.stringify(started)}`);

  try {
    const autoOpened = started.viewer_auto_open;
    assert(
      autoOpened?.ok === true &&
        autoOpened.launch?.pid > 0 &&
        autoOpened.launch?.exit_when_workspace_gone === true,
      `workspace_start should auto-open a target-bound viewer by default: ${JSON.stringify(started)}`,
    );

    const opened = await callTool("workspace_open_viewer", { id: workspaceId }, 15000);
    assert(opened.ok === true, `workspace_open_viewer failed: ${JSON.stringify(opened)}`);
    assert(
      opened.launch?.reused === true && opened.launch?.pid === autoOpened.launch.pid,
      `workspace_open_viewer should reuse the viewer auto-opened by workspace_start: ${JSON.stringify(opened)}`,
    );

    const listed = await poll("registered live viewer", async () => {
      const viewers = await callTool("workspace_list_viewers");
      return viewers.viewers?.find((viewer) => viewer.id === workspaceId && viewer.alive === true);
    });
    assert(
      listed.pid === autoOpened.launch.pid,
      `workspace_list_viewers should find the launched pid: listed=${JSON.stringify(listed)} opened=${JSON.stringify(opened)}`,
    );
    assert(pidExists(listed.pid), `launched viewer pid ${listed.pid} should exist before close`);

    const duplicate = await callTool(
      "workspace_open_viewer",
      { id: workspaceId, always_on_top: true },
      15000,
    );
    assert(
      duplicate.ok === true &&
        duplicate.launch?.reused === true &&
        duplicate.launch?.pid === listed.pid &&
        duplicate.launch?.always_on_top === autoOpened.launch.always_on_top,
      `workspace_open_viewer should reuse an existing viewer for the workspace instead of opening a second window: duplicate=${JSON.stringify(duplicate)} opened=${JSON.stringify(opened)}`,
    );

    const preview = await callTool("workspace_close_viewer", { id: workspaceId, dry_run: true });
    assert(
      preview.ok === true &&
        preview.close?.candidates?.some((entry) => entry.pid === listed.pid) &&
        preview.close?.closed?.length === 0,
      `workspace_close_viewer dry-run should preview the viewer pid: ${JSON.stringify(preview)}`,
    );

    const closed = await callTool("workspace_close_viewer", { id: workspaceId }, 8000);
    assert(
      closed.ok === true && closed.close?.closed?.some((entry) => entry.pid === listed.pid),
      `workspace_close_viewer should close the launched viewer: ${JSON.stringify(closed)}`,
    );

    await poll("viewer pid exit", async () => !pidExists(listed.pid), 5000);
    const afterClose = await callTool("workspace_list_viewers");
    assert(
      !afterClose.viewers?.some((viewer) => viewer.id === workspaceId && viewer.alive === true),
      `workspace_list_viewers should not retain a live viewer after close: ${JSON.stringify(afterClose)}`,
    );
  } finally {
    await closeAllViewers();
    const stopped = await callTool("workspace_stop", { id: workspaceId }, 15000);
    assert(stopped.ok === true, `workspace_stop failed: ${JSON.stringify(stopped)}`);
  }
}

main()
  .then(() => {
    child.kill("SIGTERM");
    fs.rmSync(tempDir, { recursive: true, force: true });
    console.log("direct mcp viewer lifecycle smoke passed");
  })
  .catch((error) => {
    closeAllViewers()
      .catch(() => {})
      .finally(() => {
        try {
          child.kill("SIGTERM");
        } catch {
          // ignore cleanup races
        }
        fs.rmSync(tempDir, { recursive: true, force: true });
        console.error(error.stack || error.message || String(error));
        console.error(stderr);
        process.exit(1);
      });
  });
