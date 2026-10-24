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

function commandExists(name) {
  const resolved = childProcess.spawnSync("sh", ["-lc", `command -v ${name}`], {
    encoding: "utf8",
  });
  return resolved.status === 0 && resolved.stdout.trim();
}

if (!commandExists("xterm") || !commandExists("tmux")) {
  console.log("workspace terminal TUI smoke skipped: xterm or tmux not found");
  process.exit(0);
}

const tempDir = fs.mkdtempSync(path.join(os.tmpdir(), "awl-tui-"));
const configDir = path.join(tempDir, "config");
const runtimeDir = path.join(tempDir, "runtime");
fs.mkdirSync(configDir, { recursive: true });
fs.mkdirSync(runtimeDir, { recursive: true });
fs.chmodSync(runtimeDir, 0o700);

const env = {
  ...process.env,
  XDG_CONFIG_HOME: configDir,
  XDG_RUNTIME_DIR: runtimeDir,
};

for (const name of ["AGENT_WORKSPACE_ID", "AGENT_WORKSPACE_SESSION_ID", "AGENT_WORKSPACE_RUNTIME_DIR", "AGENT_WORKSPACE_SOCKET", "TMUX"]) delete env[name];

const child = childProcess.spawn(bin, ["mcp", "--headless"], {
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

function abort(message) {
  try {
    child.kill("SIGTERM");
  } catch {
    // ignore cleanup races
  }
  throw new Error(message);
}

function assert(condition, message) {
  if (!condition) throw new Error(message);
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

async function callTool(name, args, timeoutMs, allowError = false) {
  const result = await request(
    "tools/call",
    {
      name,
      arguments: args || {},
    },
    timeoutMs,
    `tools/call ${name}`,
  );
  if (result.isError && !allowError) {
    throw new Error(`tool ${name} returned MCP error: ${JSON.stringify(result)}`);
  }
  if (result.structuredContent && typeof result.structuredContent === "object") {
    return result.structuredContent;
  }
  const text = result.content?.find((entry) => entry?.type === "text")?.text;
  return text ? JSON.parse(text) : null;
}

async function waitForTerminalText(workspaceId, terminalId, pattern) {
  const deadline = Date.now() + 10000;
  let last;
  while (Date.now() < deadline) {
    last = await callTool("workspace_terminal_read", {
      id: workspaceId,
      terminal_id: terminalId,
      preserve_trailing_spaces: true,
    });
    if (pattern.test(last.screen?.text || "")) return last;
    await new Promise((resolve) => setTimeout(resolve, 250));
  }
  throw new Error(`terminal text did not match ${pattern}: ${JSON.stringify(last)}`);
}

const ownedSockets = new Set();
const extraWorkspaceIds = [];
function terminalSocket(id, terminalId) {
  const socket = path.join(runtimeDir, "agent-workspace-linux", id, `terminal-${terminalId}.tmux.sock`);
  ownedSockets.add(socket);
  return socket;
}
function tmux(socket, args, commandEnv = env) {
  const result = childProcess.spawnSync("tmux", ["-S", socket, ...args], { env: commandEnv, encoding: "utf8", timeout: 10000 });
  assert(result.status === 0, `tmux ${args[0]} failed: ${result.stderr}`);
  return result.stdout.trim();
}
function terminalProcesses(socket, target) {
  const [server, pane] = tmux(socket, ["display-message", "-p", "-t", target, "#{pid} #{pane_pid}"]).split(/\s+/).map(Number);
  assert(server > 1 && pane > 1, "terminal process identities must be available");
  return { server, pane };
}
function processRunning(pid) {
  try {
    const stat = fs.readFileSync(`/proc/${pid}/stat`, "utf8");
    return !["Z", "X"].includes(stat.slice(stat.lastIndexOf(") ") + 2).split(" ")[0]);
  } catch (error) {
    if (error.code === "ENOENT") return false;
    throw error;
  }
}
async function waitForProcessesStopped(processes, label) {
  const pids = processes.flatMap(({ server, pane }) => [server, pane]);
  const deadline = Date.now() + 10000;
  while (Date.now() < deadline && pids.some(processRunning)) {
    await new Promise(resolve => setTimeout(resolve, 50));
  }
  assert(!pids.some(processRunning), `${label}: managed processes survived: ${pids.filter(processRunning)}`);
}
function workspaceCli(...args) {
  const result = childProcess.spawnSync(bin, ["workspace", ...args], { env, encoding: "utf8", timeout: 15000 });
  assert(result.status === 0, `workspace CLI failed: ${result.stderr}`);
  return JSON.parse(result.stdout);
}
async function startExtraTerminal(id, terminalId) {
  extraWorkspaceIds.push(id);
  const started = await callTool("workspace_start", { id, acknowledge_hidden_workspace: true, purpose: "terminal cleanup regression", width: 900, height: 620 }, 15000);
  assert(started.ok, JSON.stringify(started));
  const socket = terminalSocket(id, terminalId);
  const launched = await callTool("workspace_run_in_terminal", { id, terminal_id: terminalId, command: ["sleep", "600"], window_timeout_ms: 15000, timeout_ms: 10000 }, 20000);
  assert(launched.ok, JSON.stringify(launched));
  return { socket, processes: terminalProcesses(socket, terminalId) };
}

async function main() {
  const workspaceId = `tui-${process.pid}`;
  const terminalId = "game";
  const socket = terminalSocket(workspaceId, terminalId);
  try {
    const initializeResult = await request("initialize", {
      protocolVersion: "2024-11-05",
      capabilities: {},
      clientInfo: { name: "agent-workspace-linux-terminal-tui-smoke", version: "0" },
    });
    notify("notifications/initialized", {});
    assert(
      /workspace_terminal_read/.test(String(initializeResult.instructions || "")),
      `initialize instructions should expose terminal text controls: ${initializeResult.instructions}`,
    );

    const started = await callTool(
      "workspace_start",
      {
        id: workspaceId,
        acknowledge_hidden_workspace: true,
        purpose: "Direct MCP terminal TUI smoke",
        width: 900,
        height: 620,
      },
      15000,
    );
    assert(started.ok === true, `workspace_start failed: ${JSON.stringify(started)}`);

    const launched = await callTool(
      "workspace_run_in_terminal",
      {
        id: workspaceId,
        terminal_id: terminalId,
        title: "terminal-tui-smoke",
        command: [
          "sh",
          "-lc",
          "printf 'terminal-ready\\n'; read line; printf 'got:%s\\n' \"$line\"; while :; do sleep 1; done",
        ],
        window_timeout_ms: 15000,
        timeout_ms: 10000,
      },
      20000,
    );
    assert(
      launched.ok === true &&
        launched.terminal?.terminal_id === terminalId &&
        launched.target_handles?.terminal_ids?.includes(terminalId),
      `workspace_run_in_terminal should return terminal handles: ${JSON.stringify(launched)}`,
    );

    const ready = await waitForTerminalText(workspaceId, terminalId, /terminal-ready/);
    assert(
      /terminal-ready/.test(ready.screen?.text || ""),
      `workspace_terminal_read should capture terminal text: ${JSON.stringify(ready)}`,
    );

    const input = await callTool(
      "workspace_terminal_input",
      {
        id: workspaceId,
        terminal_id: terminalId,
        text: "hello-from-batch",
        keys: ["Return"],
      },
      10000,
    );
    assert(
      input.ok === true &&
        input.input?.normalized_keys?.includes("Enter") &&
        input.input?.text_bytes === "hello-from-batch".length,
      `workspace_terminal_input should send literal text and batched keys: ${JSON.stringify(input)}`,
    );

    const echoed = await waitForTerminalText(workspaceId, terminalId, /got:hello-from-batch/);
    assert(
      /got:hello-from-batch/.test(echoed.screen?.text || ""),
      `workspace_terminal_read should see post-input TUI state: ${JSON.stringify(echoed)}`,
    );

    const events = await callTool("workspace_events", { id: workspaceId, tail: 20 });
    const terminalEvent = (events.events || []).find((event) => event.kind === "terminal_input");
    assert(terminalEvent, `terminal input should record metadata event: ${JSON.stringify(events.events)}`);
    assert(
      terminalEvent.detail?.text_bytes === "hello-from-batch".length &&
        !Object.hasOwn(terminalEvent.detail || {}, "text"),
      `terminal input event should omit raw text: ${JSON.stringify(terminalEvent)}`,
    );

    const live = terminalProcesses(socket, terminalId);
    const detachedSocket = terminalSocket(workspaceId, "detached");
    const detached = await callTool("workspace_run_in_terminal", { id: workspaceId, terminal_id: "detached", command: ["sleep", "600"], window_timeout_ms: 15000, timeout_ms: 10000 }, 20000);
    const detachedProcesses = terminalProcesses(detachedSocket, "detached");
    await callTool("workspace_kill_app", { id: workspaceId, app_id: detached.terminal.app_id });
    assert(processRunning(detachedProcesses.server), "closing a client must leave its terminal session available until workspace stop");

    const sentinelSocket = path.join(tempDir, "sentinel.sock");
    ownedSockets.add(sentinelSocket);
    tmux(sentinelSocket, ["new-session", "-d", "-s", "sentinel", "--", "sleep", "600"]);
    const sentinel = terminalProcesses(sentinelSocket, "sentinel");
    const stopped = await callTool("workspace_stop", { id: workspaceId }, 15000);
    assert(stopped.ok === true, `workspace_stop failed: ${JSON.stringify(stopped)}`);
    await waitForProcessesStopped([live, detachedProcesses], "workspace stop");
    assert(processRunning(sentinel.server) && processRunning(sentinel.pane), "unrelated tmux session was stopped");

    const staleId = `${workspaceId}-stale`;
    const stale = await startExtraTerminal(staleId, "stale");
    const staleStatus = workspaceCli("status", "--id", staleId);
    process.kill(staleStatus.daemon_pid, "SIGKILL");
    await waitForProcessesStopped([{server:staleStatus.daemon_pid,pane:staleStatus.daemon_pid}], "daemon crash");
    const preview = workspaceCli("cleanup", "--id", staleId, "--dry-run");
    assert(preview.candidates.some(entry => entry.id === staleId), JSON.stringify(preview));
    assert(processRunning(stale.processes.server), "dry run stopped a terminal server");
    const cleaned = workspaceCli("cleanup", "--id", staleId);
    assert(cleaned.removed.some(entry => entry.id === staleId), JSON.stringify(cleaned));
    await waitForProcessesStopped([stale.processes], "stale cleanup");

    const guardedId = `${workspaceId}-guarded`;
    const guarded = await startExtraTerminal(guardedId, "guarded");
    tmux(guarded.socket, ["kill-server"]);
    await waitForProcessesStopped([guarded.processes], "replace stopped fixture");
    tmux(guarded.socket, ["new-session", "-d", "-s", "guarded", "--", "sleep", "600"]);
    const foreign = terminalProcesses(guarded.socket, "guarded");
    const refused = await callTool("workspace_stop", {id:guardedId}, 15000, true);
    assert(refused.ok === false, "stop accepted a terminal from a different workspace session");
    const guardedStatus = workspaceCli("status", "--id", guardedId);
    assert(fs.existsSync(guardedStatus.socket_path) && processRunning(foreign.server), "failed stop lost control or killed an unrelated server");
    process.kill(guardedStatus.daemon_pid, "SIGKILL");
    await waitForProcessesStopped([{server:guardedStatus.daemon_pid,pane:guardedStatus.daemon_pid}], "guarded daemon crash");
    const skipped = workspaceCli("cleanup", "--id", guardedId);
    assert(skipped.skipped.some(entry => entry.id === guardedId) && fs.existsSync(guardedStatus.runtime_dir), "unverified runtime was removed");
    assert(processRunning(foreign.server), "cleanup killed a mismatched-session server");
    tmux(guarded.socket, ["kill-server"]);
    await waitForProcessesStopped([foreign], "foreign fixture cleanup");
    workspaceCli("cleanup", "--id", guardedId);
    assert(processRunning(sentinel.server) && processRunning(sentinel.pane), "stale cleanup stopped unrelated tmux");

    const taggedId = `${workspaceId}-tagged`;
    const tagged = await startExtraTerminal(taggedId, "tagged");
    const taggedStatus = workspaceCli("status", "--id", taggedId);
    tmux(tagged.socket, ["kill-server"]);
    await waitForProcessesStopped([tagged.processes], "tagged fixture replacement");
    const outsideSocket = path.join(tempDir, "tagged-sentinel.sock");
    ownedSockets.add(outsideSocket);
    const taggedEnv = { ...env, AGENT_WORKSPACE_ID: taggedId, AGENT_WORKSPACE_SESSION_ID: taggedStatus.session_id };
    tmux(outsideSocket, ["new-session", "-d", "-s", "tagged", "--", "sleep", "600"], taggedEnv);
    const outside = terminalProcesses(outsideSocket, "tagged");
    if (fs.existsSync(tagged.socket)) fs.unlinkSync(tagged.socket);
    fs.linkSync(outsideSocket, tagged.socket);
    const aliasRefused = await callTool("workspace_stop", {id:taggedId}, 15000, true);
    assert(aliasRefused.ok === false && processRunning(outside.server), "shared environment tags authorized an unrelated socket alias");
    assert(fs.existsSync(taggedStatus.socket_path), "socket-alias refusal lost workspace control");
    tmux(outsideSocket, ["kill-server"]);
    await waitForProcessesStopped([outside], "tagged sentinel cleanup");
    const taggedStopped = await callTool("workspace_stop", {id:taggedId}, 15000);
    assert(taggedStopped.ok, JSON.stringify(taggedStopped));

  } finally {
    // Run fallback cleanup after assertions, so it cannot hide a runtime leak.
    for (const socket of ownedSockets) {
      childProcess.spawnSync("tmux", ["-S", socket, "kill-server"], { env, stdio: "ignore", timeout: 1000 });
    }
    for (const id of extraWorkspaceIds) {
      try { await callTool("workspace_stop", { id }, 5000); } catch {}
      try { workspaceCli("cleanup", "--id", id); } catch {}
    }
    try {
      await callTool("workspace_stop", { id: workspaceId }, 5000);
    } catch {
      // ignore cleanup races
    }
    try {
      child.kill("SIGTERM");
    } catch {
      // ignore cleanup races
    }
  }
}

main()
  .then(() => {
    fs.rmSync(tempDir, { recursive: true, force: true });
    console.log("workspace terminal TUI smoke passed");
  })
  .catch((error) => {
    console.error(error && error.stack ? error.stack : error);
    console.error(stderr);
    console.error(`preserved temp dir: ${tempDir}`);
    try {
      child.kill("SIGTERM");
    } catch {
      // ignore cleanup races
    }
    process.exit(1);
  });
