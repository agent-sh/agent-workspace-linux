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

const tempDir = fs.mkdtempSync(path.join(os.tmpdir(), "awl-nhd-"));
const configDir = path.join(tempDir, "config");
const runtimeDir = path.join(tempDir, "runtime");
const browserDataDir = path.join(tempDir, "browser-data");
fs.mkdirSync(configDir, { recursive: true });
fs.mkdirSync(runtimeDir, { recursive: true, mode: 0o700 });
fs.chmodSync(runtimeDir, 0o700);
fs.mkdirSync(browserDataDir, { recursive: true });

const env = {
  ...process.env,
  XDG_CONFIG_HOME: configDir,
  XDG_RUNTIME_DIR: runtimeDir,
};
delete env.DISPLAY;
delete env.WAYLAND_DISPLAY;
delete env.WAYLAND_SOCKET;
delete env.AGENT_WORKSPACE_VIEWER_BACKEND;

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

function assertNoViewerStep(plan, label) {
  assert(
    plan.headless === false,
    `${label} should still report non-headless MCP mode: ${JSON.stringify(plan)}`,
  );
  assert(
    plan.host_viewer_ready === false &&
      plan.viewer_available === false &&
      /ready_for_host_viewer=false|DISPLAY|WAYLAND_DISPLAY/.test(String(plan.viewer_unavailable_reason || "")),
    `${label} should explain why host-visible viewer steps are unavailable: ${JSON.stringify(plan)}`,
  );
  assert(
    plan.assumptions?.some((assumption) => /ready_for_host_viewer=false|DISPLAY|WAYLAND_DISPLAY/.test(assumption)),
    `${label} should carry the viewer unavailability reason in assumptions: ${JSON.stringify(plan)}`,
  );
  assert(
    !plan.steps?.some((step) => step.tool === "workspace_open_viewer"),
    `${label} should not suggest a host-visible viewer without a host display: ${JSON.stringify(plan)}`,
  );
  assert(
    !plan.approval_checkpoints?.some((checkpoint) => checkpoint.kind === "host_visible_ui"),
    `${label} should not expose host-visible checkpoints without a host display: ${JSON.stringify(plan)}`,
  );
  assert(
    !plan.task_context?.approval_kinds?.includes("host_visible_ui"),
    `${label} should not expose host-visible approval kind without a host display: ${JSON.stringify(plan)}`,
  );
}

async function main() {
  const initializeResult = await request("initialize", {
    protocolVersion: "2024-11-05",
    capabilities: {},
    clientInfo: { name: "agent-workspace-linux-no-host-display-viewer-smoke", version: "0" },
  });
  notify("notifications/initialized", {});
  const instructions = String(initializeResult.instructions || "");
  assert(
    /configured=false/.test(instructions) && /--headless/.test(instructions),
    `no-host-display MCP initialize instructions should preserve clean non-headless contract: ${instructions}`,
  );

  const permissions = await callTool("mcp_permissions");
  assert(
    permissions.configured === false && permissions.restricted === false,
    `no-host-display clean MCP should not invent a permission ceiling: ${JSON.stringify(permissions)}`,
  );

  const sessionBrief = await callTool("mcp_session_brief");
  assert(
    sessionBrief.headless === false,
    `no-host-display MCP should not silently become headless without the flag: ${JSON.stringify(sessionBrief)}`,
  );
  assert(
    sessionBrief.doctor?.ready_for_host_viewer === false,
    `session brief should report host viewer unavailable without DISPLAY/WAYLAND_DISPLAY: ${JSON.stringify(sessionBrief)}`,
  );
  assert(
    sessionBrief.doctor?.viewer_blockers?.some((blocker) => /DISPLAY|WAYLAND_DISPLAY/.test(blocker)),
    `session brief should explain the missing host display: ${JSON.stringify(sessionBrief)}`,
  );
  assert(
    !sessionBrief.recommendations?.some((action) => action.tool === "workspace_open_viewer"),
    `session brief should not recommend opening a viewer without a host display: ${JSON.stringify(sessionBrief)}`,
  );

  const workspaceId = `nhd-${process.pid}`;
  const started = await callTool(
    "workspace_start",
    {
      id: workspaceId,
      acknowledge_hidden_workspace: true,
      purpose: "No-host-display viewer smoke",
      width: 800,
      height: 500,
    },
    15000,
  );
  assert(started.ok === true, `workspace_start should still work without a host display: ${JSON.stringify(started)}`);
  assert(
    started.viewer_auto_open?.requested === true &&
      started.viewer_auto_open?.attempted === false &&
      started.viewer_auto_open?.ok === false &&
      /ready_for_host_viewer=false|DISPLAY|WAYLAND_DISPLAY/.test(started.viewer_auto_open?.message || ""),
    `workspace_start should report why default viewer auto-open was skipped without silently becoming headless: ${JSON.stringify(started)}`,
  );
  try {
    const runningBrief = await callTool("mcp_session_brief");
    assert(
      runningBrief.workspaces?.running_ids?.includes(workspaceId),
      `session brief should see the running no-host-display workspace: ${JSON.stringify(runningBrief)}`,
    );
    assert(
      !runningBrief.recommendations?.some((action) => action.tool === "workspace_open_viewer"),
      `running-workspace brief should not recommend opening a viewer without a host display: ${JSON.stringify(runningBrief)}`,
    );
  } finally {
    const stopped = await callTool("workspace_stop", { id: workspaceId }, 15000);
    assert(stopped.ok === true, `workspace_stop should clean up no-host-display smoke workspace: ${JSON.stringify(stopped)}`);
  }

  const appQaPlan = await callTool("mcp_task_plan", {
    intent: "app QA",
    project_path: repoRoot,
  });
  assertNoViewerStep(appQaPlan, "no-host-display app QA plan");

  const groceryPlan = await callTool("mcp_task_plan", {
    intent: "grocery shopping",
    user_data_dir: browserDataDir,
    target_url: "https://example.invalid/grocery",
    shopping_list: "milk, eggs",
    budget: "under 50 ILS",
    fulfillment: "delivery",
    substitution_policy: "ask before replacing",
  });
  assertNoViewerStep(groceryPlan, "no-host-display grocery plan");

  const viewerDenied = await callTool("workspace_open_viewer", { id: "default" });
  assert(
    viewerDenied.ok === false && /DISPLAY|WAYLAND_DISPLAY|host-visible viewer/.test(String(viewerDenied.message || "")),
    `workspace_open_viewer should refuse with a clear host-display message: ${JSON.stringify(viewerDenied)}`,
  );

  console.log("no-host-display mcp viewer smoke passed");
}

main()
  .catch((error) => {
    console.error(error.stack || String(error));
    process.exitCode = 1;
  })
  .finally(() => {
    try {
      child.kill("SIGTERM");
    } catch {
      // ignore cleanup races
    }
    fs.rmSync(tempDir, { recursive: true, force: true });
  });
