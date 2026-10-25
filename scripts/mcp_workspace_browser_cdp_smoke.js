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

function resolveBrowser() {
  if (process.env.BROWSER_BIN) return process.env.BROWSER_BIN;
  for (const candidate of ["google-chrome", "google-chrome-stable", "chromium", "chromium-browser"]) {
    const resolved = childProcess.spawnSync("sh", ["-lc", `command -v ${candidate}`], {
      encoding: "utf8",
    });
    if (resolved.status === 0 && resolved.stdout.trim()) {
      return resolved.stdout.trim();
    }
  }
  return null;
}

const browser = resolveBrowser();
if (!browser) {
  console.log("workspace browser CDP smoke skipped: Chrome/Chromium not found");
  process.exit(0);
}

const tempDir = fs.mkdtempSync(path.join(os.tmpdir(), "awl-cdp-"));
const configDir = path.join(tempDir, "config");
const runtimeDir = path.join(tempDir, "runtime");
const userDataDir = path.join(tempDir, "browser-profile");
fs.mkdirSync(configDir, { recursive: true });
fs.mkdirSync(runtimeDir, { recursive: true, mode: 0o700 });
fs.chmodSync(runtimeDir, 0o700);
fs.mkdirSync(userDataDir, { recursive: true });
fs.chmodSync(runtimeDir, 0o700);

const env = {
  ...process.env,
  XDG_CONFIG_HOME: configDir,
  XDG_RUNTIME_DIR: runtimeDir,
};

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

function dataUrl(title, body) {
  return `data:text/html;charset=utf-8,${encodeURIComponent(`<!doctype html>
<meta charset="utf-8">
<title>${title}</title>
<main>${body}</main>
`)}`;
}

function inputEventCount(events) {
  const inputKinds = new Set([
    "click",
    "click_window",
    "move_pointer",
    "move_pointer_window",
    "drag",
    "drag_window",
    "scroll",
    "scroll_window",
    "key",
    "key_window",
    "type_text",
    "type_window",
    "set_clipboard",
    "paste_text",
    "paste_window",
  ]);
  return (events || []).filter((event) => inputKinds.has(event.kind)).length;
}

function eventOfKind(events, kind) {
  return (events || []).find((event) => event.kind === kind);
}

function assertNoRawBrowserEventContent(event, extraForbidden = []) {
  const detail = event?.detail || {};
  for (const key of ["text", "headings", "links", "results", "text_excerpt", ...extraForbidden]) {
    assert(
      !Object.hasOwn(detail, key),
      `${event?.kind || "browser"} event should not persist raw browser content key ${key}: ${JSON.stringify(event)}`,
    );
  }
}

async function main() {
  const workspaceId = `cdp-${process.pid}`;
  try {
    const initializeResult = await request("initialize", {
      protocolVersion: "2024-11-05",
      capabilities: {},
      clientInfo: { name: "agent-workspace-linux-browser-cdp-smoke", version: "0" },
    });
    notify("notifications/initialized", {});
    assert(
      /workspace_open_browser/.test(String(initializeResult.instructions || "")),
      `initialize instructions should expose workspace browser bootstrap controls: ${initializeResult.instructions}`,
    );

    const template = await callTool("profile_template", {
      kind: "browser-session",
      id: "browser-cdp",
      browser_path: browser,
      user_data_dir: userDataDir,
    });
    const startupCommand = template.profile?.startup_apps?.[0]?.command || [];
    assert(
      startupCommand.includes("--remote-debugging-address=127.0.0.1") &&
        startupCommand.includes("--remote-debugging-port=0"),
      `browser-session profile template should expose an ephemeral workspace CDP endpoint: ${JSON.stringify(template)}`,
    );

    const plan = await callTool("mcp_task_plan", {
      intent: "grocery shopping",
      workspace_id: workspaceId,
      browser_path: browser,
      user_data_dir: userDataDir,
      target_url: "https://example-grocery.test",
      shopping_list: "milk",
      fulfillment: "delivery",
      substitution_policy: "ask first",
      budget: "$50",
    });
    assert(
      plan.task_context?.safety_boundaries?.some((boundary) => /workspace-owned Chrome DevTools/.test(boundary)),
      `browser task plan should point agents at workspace-owned Chrome DevTools: ${JSON.stringify(plan.task_context)}`,
    );

    const started = await callTool(
      "workspace_start",
      {
        id: workspaceId,
        acknowledge_hidden_workspace: true,
        purpose: "Direct MCP workspace Chrome CDP smoke",
        width: 900,
        height: 620,
      },
      15000,
    );
    assert(started.ok === true, `workspace_start failed: ${JSON.stringify(started)}`);

    const initialUrl = dataUrl(
      "Workspace CDP Ready",
      `
        <p>workspace-cdp-ready</p>
        <button id="filter-chip" onclick="document.getElementById('clicked').textContent = 'filter clicked'">48GB filter</button>
        <p id="clicked">not clicked</p>
        <div data-component-type="s-search-result">
          <h2><a href="/dp/GPU48"><span>PNY Test GPU 48GB Workstation Card</span></a></h2>
          <span class="a-price"><span class="a-offscreen">$4,899.00</span></span>
          <span aria-label="4.6 out of 5 stars"></span>
          <a href="#customerReviews">(22)</a>
          <p>FREE delivery Tomorrow</p>
          <p>Only 2 left in stock - order soon.</p>
        </div>
        <div data-component-type="s-search-result">
          <h2><a href="/dp/GPU96"><span>RTX PRO Test GPU 96GB Blackwell Card</span></a></h2>
          <span class="a-price"><span class="a-offscreen">$10,199.00</span></span>
          <p>FREE delivery Fri, May 29</p>
        </div>
        <div data-component-type="s-search-result">
          <h2><a href="/dp/GPU16"><span>RTX Test GPU 16GB Entry Card</span></a></h2>
          <span class="a-price"><span class="a-offscreen">$799.00</span></span>
          <p>FREE delivery Monday</p>
        </div>
      `,
    );
    const openedBrowser = await callTool(
      "workspace_open_browser",
      {
        id: workspaceId,
        browser_path: browser,
        user_data_dir: userDataDir,
        url: initialUrl,
        window_timeout_ms: 15000,
        timeout_ms: 10000,
      },
      20000,
    );
    assert(openedBrowser.ok === true, `workspace_open_browser failed: ${JSON.stringify(openedBrowser)}`);
    assert(
      openedBrowser.app_id && openedBrowser.target_handles?.browser_target_ids?.length >= 1,
      `workspace_open_browser should return stable app and browser target handles: ${JSON.stringify(openedBrowser)}`,
    );
    const launchedAppId = openedBrowser.app_id;

    const mcpTargets = await callTool(
      "workspace_browser_targets",
      {
        id: workspaceId,
        app_id: launchedAppId,
        timeout_ms: 10000,
      },
      12000,
    );
    assert(
      mcpTargets.ok === true &&
        mcpTargets.app_id === launchedAppId &&
        /127\.0\.0\.1/.test(mcpTargets.devtools_endpoint || "") &&
        mcpTargets.targets?.some((candidate) => candidate.title === "Workspace CDP Ready"),
      `workspace_browser_targets should discover the workspace Chrome page: ${JSON.stringify(mcpTargets)}`,
    );

    const target =
      mcpTargets.targets.find((candidate) => candidate.type === "page" && /Workspace CDP Ready/.test(candidate.title || "")) ||
      mcpTargets.targets.find((candidate) => candidate.type === "page" && candidate.webSocketDebuggerUrl);
    assert(target, `DevTools target list should include a page target: ${JSON.stringify(mcpTargets.targets)}`);

    const initialState = await callTool(
      "workspace_browser_snapshot",
      {
        id: workspaceId,
        app_id: launchedAppId,
        target_id: target.id,
        max_text_chars: 4000,
        timeout_ms: 8000,
      },
      10000,
    );
    assert(
      initialState.ok === true &&
        initialState.page?.title === "Workspace CDP Ready" &&
        /workspace-cdp-ready/.test(initialState.page?.text || ""),
      `workspace_browser_snapshot should read the workspace Chrome page, not host Chrome: ${JSON.stringify(initialState)}`,
    );
    const snapshotEvents = await callTool("workspace_events", { id: workspaceId, tail: 10 });
    const snapshotEvent = eventOfKind(snapshotEvents.events, "browser_snapshot");
    assert(
      snapshotEvent,
      `workspace_browser_snapshot should record a browser_snapshot event: ${JSON.stringify(snapshotEvents.events)}`,
    );
    assert(
      snapshotEvent.detail?.raw_text_omitted === true,
      `browser_snapshot event should mark raw page text omitted: ${JSON.stringify(snapshotEvent)}`,
    );
    assertNoRawBrowserEventContent(snapshotEvent);

    const clicked = await callTool(
      "workspace_browser_click",
      {
        id: workspaceId,
        app_id: launchedAppId,
        target_id: target.id,
        text: "48GB filter",
        wait_ms: 250,
        snapshot: true,
        max_text_chars: 4000,
        timeout_ms: 8000,
      },
      10000,
    );
    assert(
      clicked.ok === true &&
        clicked.click?.match_kind === "text" &&
        /filter clicked/.test(clicked.page?.text || ""),
      `workspace_browser_click should click by visible text without window coordinates: ${JSON.stringify(clicked)}`,
    );
    const clickEvents = await callTool("workspace_events", { id: workspaceId, tail: 10 });
    const clickEvent = eventOfKind(clickEvents.events, "browser_click");
    assert(clickEvent, `workspace_browser_click should record a browser_click event: ${JSON.stringify(clickEvents.events)}`);
    assertNoRawBrowserEventContent(clickEvent, ["clicked_text"]);

    const structuredResults = await callTool(
      "workspace_browser_search_results",
      {
        id: workspaceId,
        app_id: launchedAppId,
        target_id: target.id,
        max_results: 5,
        min_vram_gb: 37,
        timeout_ms: 8000,
      },
      10000,
    );
    assert(
      structuredResults.ok === true &&
        structuredResults.page?.result_count === 2 &&
        structuredResults.page?.results?.some(
          (result) =>
            /48GB Workstation/.test(result.title || "") &&
            result.price === "$4,899.00" &&
            result.vram_gb === 48 &&
            /left in stock/i.test(result.availability || ""),
        ) &&
        structuredResults.page?.results?.some((result) => result.vram_gb === 96) &&
        !structuredResults.page?.results?.some((result) => /16GB Entry/.test(result.title || "")),
      `workspace_browser_search_results should extract structured product cards: ${JSON.stringify(structuredResults)}`,
    );
    const resultsEvents = await callTool("workspace_events", { id: workspaceId, tail: 10 });
    const resultsEvent = eventOfKind(resultsEvents.events, "browser_search_results");
    assert(
      resultsEvent,
      `workspace_browser_search_results should record a browser_search_results event: ${JSON.stringify(resultsEvents.events)}`,
    );
    assert(
      resultsEvent.detail?.raw_result_text_omitted === true,
      `browser_search_results event should mark raw result text omitted: ${JSON.stringify(resultsEvent)}`,
    );
    assertNoRawBrowserEventContent(resultsEvent);

    const baseline = await callTool("workspace_events", { id: workspaceId, tail: 1 });
    const baselineSequence = Math.max(0, ...(baseline.events || []).map((event) => event.sequence || 0));
    const navigated = await callTool(
      "workspace_browser_navigate",
      {
        id: workspaceId,
        app_id: launchedAppId,
        target_id: target.id,
        url: dataUrl("Workspace CDP Navigated", "workspace-cdp-navigated-without-coordinate-input"),
        wait_ms: 500,
        snapshot: true,
        max_text_chars: 4000,
        timeout_ms: 8000,
      },
      12000,
    );
    assert(
      navigated.ok === true &&
        navigated.page?.title === "Workspace CDP Navigated" &&
        /without-coordinate-input/.test(navigated.page?.text || ""),
      `workspace_browser_navigate should navigate and read the workspace Chrome page: ${JSON.stringify(navigated)}`,
    );

    const waited = await callTool(
      "workspace_wait_window",
      {
        id: workspaceId,
        app: launchedAppId,
        title: "Workspace CDP Navigated",
        timeout_ms: 10000,
      },
      12000,
    );
    assert(waited.ok === true, `workspace window should reflect CDP navigation: ${JSON.stringify(waited)}`);

    const events = await callTool("workspace_events", {
      id: workspaceId,
      since_sequence: baselineSequence,
      tail: 50,
    });
    assert(
      inputEventCount(events.events) === 0,
      `MCP browser control should not rely on workspace keyboard/mouse events: ${JSON.stringify(events.events)}`,
    );
    assert(
      eventOfKind(events.events, "browser_navigate"),
      `MCP browser control should record browser action events: ${JSON.stringify(events.events)}`,
    );
    const navigateEvent = eventOfKind(events.events, "browser_navigate");
    assert(
      navigateEvent.detail?.raw_text_omitted === true,
      `browser_navigate event should mark raw page text omitted: ${JSON.stringify(navigateEvent)}`,
    );
    assertNoRawBrowserEventContent(navigateEvent);

    const stopped = await callTool("workspace_stop", { id: workspaceId }, 15000);
    assert(stopped.ok === true, `workspace_stop failed: ${JSON.stringify(stopped)}`);
  } finally {
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
    console.log("workspace browser CDP smoke passed");
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
