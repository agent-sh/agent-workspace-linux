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

// Everything below asserts the shape of a session that can open a host-visible
// viewer: ready_for_host_viewer=true, viewer steps present in the task plan,
// auto-open on workspace_start. On a machine with no host display the MCP is
// right to report the viewer unavailable, so running these assertions there only
// produces a failure that says nothing about the code — which is exactly what a
// headless contributor hit on #69. mcp_no_host_display_viewer_smoke.js is the
// smoke that covers that session shape. Empty is absent, matching the
// non-empty-var check in host_display_check().
if (!process.env.DISPLAY && !process.env.WAYLAND_DISPLAY) {
  console.log(
    "non-headless mcp viewer smoke skipped: no DISPLAY or WAYLAND_DISPLAY (run it from a desktop session)",
  );
  process.exit(0);
}

const tempDir = fs.mkdtempSync(path.join(os.tmpdir(), "agent-workspace-non-headless-mcp-smoke-"));
const configDir = path.join(tempDir, "config");
const runtimeDir = path.join(tempDir, "runtime");
const browserDataDir = path.join(tempDir, "browser-data");
fs.mkdirSync(configDir, { recursive: true });
fs.mkdirSync(runtimeDir, { recursive: true });
fs.mkdirSync(browserDataDir, { recursive: true });

const child = childProcess.spawn(bin, ["mcp"], {
  cwd: repoRoot,
  env: {
    ...process.env,
    XDG_CONFIG_HOME: configDir,
    XDG_RUNTIME_DIR: runtimeDir,
  },
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

function assertCatalogMatchesTools(catalog, toolByName, label) {
  const catalogNames = catalog.tools?.map((tool) => tool.name) || [];
  const duplicateCatalogNames = catalogNames.filter((name, index) => catalogNames.indexOf(name) !== index);
  assert(
    duplicateCatalogNames.length === 0,
    `${label} action catalog should not contain duplicate tool entries: ${JSON.stringify(duplicateCatalogNames)}`,
  );
  const catalogByName = new Map((catalog.tools || []).map((tool) => [tool.name, tool]));
  for (const name of toolByName.keys()) {
    assert(catalogByName.has(name), `${label} action catalog missing tools/list entry ${name}`);
  }
  for (const name of catalogByName.keys()) {
    assert(toolByName.has(name), `${label} action catalog contains stale/nonexistent tool ${name}`);
  }
  assert(
    catalogByName.size === toolByName.size,
    `${label} action catalog should exactly match tools/list size, got catalog=${catalogByName.size} tools=${toolByName.size}`,
  );
  return catalogByName;
}

function assertNoPermissionBlockers(plan, label) {
  const blockers = plan.steps.flatMap((step) => step.permission_blockers || []);
  assert(
    blockers.length === 0,
    `${label} should not invent permission blockers when MCP has no permissions file: ${JSON.stringify(blockers)}`,
  );
}

function hasHostVisibleCheckpoint(plan, stepId) {
  return plan.approval_checkpoints?.some(
    (checkpoint) =>
      checkpoint.step_id === stepId &&
      checkpoint.kind === "host_visible_ui" &&
      checkpoint.approval_required === true,
  );
}

async function main() {
  const initializeResult = await request("initialize", {
    protocolVersion: "2024-11-05",
    capabilities: {},
    clientInfo: { name: "agent-workspace-linux-non-headless-viewer-smoke", version: "0" },
  });
  notify("notifications/initialized", {});
  const instructions = String(initializeResult.instructions || "");
  assert(
    /configured=false/.test(instructions) &&
      /host-visible\/open-world/.test(instructions) &&
      /workspace_start/.test(instructions) &&
      /auto-open/.test(instructions) &&
      /--headless/.test(instructions),
    `non-headless MCP initialize instructions should explain clean permissions and host-visible UI boundaries: ${instructions}`,
  );

  const tools = await request("tools/list", {});
  const toolByName = new Map((tools.tools || []).map((tool) => [tool.name, tool]));
  const viewerTool = toolByName.get("workspace_open_viewer");
  assert(viewerTool, "tools/list did not include workspace_open_viewer");
  const viewerDescription = viewerTool.description || "";
  assert(
    /host-visible/.test(viewerDescription) &&
      /--headless/.test(viewerDescription) &&
      /always_on_top/.test(viewerDescription),
    `workspace_open_viewer description should expose viewer boundaries: ${viewerDescription}`,
  );
  assert(
    (viewerTool.annotations?.openWorldHint ?? viewerTool.annotations?.open_world_hint) === true,
    "workspace_open_viewer should be annotated as open-world",
  );
  assert(
    (viewerTool.annotations?.idempotentHint ?? viewerTool.annotations?.idempotent_hint) === true,
    "workspace_open_viewer should be annotated as idempotent because repeated calls reuse the existing viewer",
  );
  for (const name of ["workspace_start", "workspace_open_profile"]) {
    const tool = toolByName.get(name);
    assert(tool, `tools/list did not include ${name}`);
    const description = tool.description || "";
    assert(
      (tool.annotations?.openWorldHint ?? tool.annotations?.open_world_hint) === true &&
        /GPUI viewer|viewer/i.test(description) &&
        /open_viewer=false/.test(description) &&
        /--headless/.test(description),
      `${name} should be annotated as host-visible/open-world and document default viewer auto-open: ${description}`,
    );
  }

  const permissions = await callTool("mcp_permissions");
  assert(
    permissions.configured === false && permissions.restricted === false,
    `non-headless clean MCP should not invent a permission ceiling: ${JSON.stringify(permissions)}`,
  );

  const catalog = await callTool("mcp_action_catalog");
  const catalogByName = assertCatalogMatchesTools(catalog, toolByName, "non-headless clean MCP");
  const viewerCatalog = catalogByName.get("workspace_open_viewer");
  for (const name of ["workspace_start", "workspace_open_profile"]) {
    const catalogEntry = catalogByName.get(name);
    assert(
      catalogEntry?.open_world === true &&
        /viewer/i.test(catalogEntry.notes || "") &&
        catalogEntry?.parameter_notes?.some(
          (note) =>
            note.parameter === "open_viewer" &&
            /suppress/i.test(note.effect || "") &&
            /does not make the MCP headless/i.test(note.live_control || ""),
        ),
      `${name} catalog entry should expose default viewer auto-open and explicit non-headless opt-out: ${JSON.stringify(catalogEntry)}`,
    );
  }
  assert(
    viewerCatalog?.open_world === true &&
      viewerCatalog?.idempotent === true &&
      /headless/.test(viewerCatalog.control_behavior || ""),
    `workspace_open_viewer should remain headless-gated open-world: ${JSON.stringify(viewerCatalog)}`,
  );
  assert(
    /reuse|second window|another window/i.test(viewerCatalog.notes || ""),
    `workspace_open_viewer catalog entry should explain duplicate calls reuse the existing viewer: ${JSON.stringify(viewerCatalog)}`,
  );
  assert(
    /read_only|paused/.test(`${viewerCatalog.notes || ""} ${catalog.notes?.join(" ") || ""}`),
    `workspace_open_viewer catalog entry should say it remains available while read-only or paused: ${JSON.stringify(viewerCatalog)}`,
  );
  assert(
    viewerCatalog.parameter_notes?.some(
      (note) =>
        note.parameter === "always_on_top" &&
        /overlay|above/i.test(note.effect || "") &&
        /read_only|paused/.test(note.live_control || "") &&
        /host-visible|open-world/i.test(note.live_control || "") &&
        /explicitly asks|always-on-top/i.test(note.approval_hint || ""),
    ),
    `workspace_open_viewer should document always_on_top as an opt-in host-visible parameter: ${JSON.stringify(viewerCatalog)}`,
  );

  const sessionBrief = await callTool("mcp_session_brief");
  assert(
    sessionBrief.headless === false && sessionBrief.permissions?.configured === false,
    `default MCP should report non-headless clean state: ${JSON.stringify(sessionBrief)}`,
  );
  assert(
    sessionBrief.recommendations.some(
      (action) =>
        action.id === "plan_browser_or_grocery_task" &&
        action.approval_summary?.next_boundary?.kind === "real_world_action",
    ) &&
      sessionBrief.approval_summary?.approval_kinds?.includes("real_world_action"),
    `default non-headless MCP should expose recommendation approval summaries: ${JSON.stringify(sessionBrief)}`,
  );

  const appQaPlan = await callTool("mcp_task_plan", {
    intent: "app QA",
    project_path: repoRoot,
  });
  assert(appQaPlan.headless === false, `app QA plan should report non-headless state: ${JSON.stringify(appQaPlan)}`);
  assert(
    appQaPlan.host_viewer_ready === true &&
      appQaPlan.viewer_available === true &&
      !appQaPlan.viewer_unavailable_reason,
    `non-headless app QA plan should expose viewer availability: ${JSON.stringify(appQaPlan)}`,
  );
  assertNoPermissionBlockers(appQaPlan, "non-headless app QA plan");
  const projectViewerStep = appQaPlan.steps.find((step) => step.id === "open_viewer_when_project_runs");
  assert(
    projectViewerStep?.tool === "workspace_open_viewer" &&
      projectViewerStep.open_world === true &&
      projectViewerStep.ready_to_call === false &&
      projectViewerStep.depends_on?.some((dependency) => /run_project_profile/.test(dependency)) &&
      /host-visible/.test(projectViewerStep.approval_hint || ""),
    `non-headless app QA plan should offer an optional viewer after the workspace run: ${JSON.stringify(appQaPlan)}`,
  );
  assert(
    hasHostVisibleCheckpoint(appQaPlan, "open_viewer_when_project_runs") &&
      appQaPlan.task_context?.approval_kinds?.includes("host_visible_ui"),
    `non-headless app QA plan should expose host-visible UI checkpoints: ${JSON.stringify(appQaPlan)}`,
  );
  assert(
    appQaPlan.task_context?.action_boundaries?.some(
      (boundary) =>
        boundary.id === "start_or_attach_project_workspace" &&
        boundary.action_type === "hidden_workspace_start" &&
        boundary.approval_kind === "hidden_workspace",
    ) &&
      appQaPlan.task_context?.action_boundaries?.some(
        (boundary) =>
          boundary.id === "drive_workspace_app" &&
          boundary.action_type === "workspace_input" &&
          boundary.required_inputs?.includes("stable_app_id_or_window"),
      ) &&
      appQaPlan.task_context?.action_boundaries?.some(
        (boundary) =>
          boundary.id === "write_mounted_project_files" &&
          boundary.action_type === "project_file_mutation" &&
          boundary.approval_kind === "project_file_write",
      ),
    `non-headless app QA plan should expose structured action boundaries for host UI: ${JSON.stringify(appQaPlan)}`,
  );
  assert(
    appQaPlan.approval_summary?.next_boundary?.kind === "required_input" &&
      appQaPlan.approval_summary?.next_boundary?.step_id === "dry_run_save_project_profile" &&
      appQaPlan.approval_summary?.approval_kinds?.includes("host_visible_ui") &&
      appQaPlan.approval_summary?.approval_kinds?.includes("project_file_write"),
    `non-headless app QA plan should summarize the next host UI boundary: ${JSON.stringify(appQaPlan)}`,
  );

  const groceryPlan = await callTool("mcp_task_plan", {
    intent: "grocery shopping",
    user_data_dir: browserDataDir,
    target_url: "https://example-grocery.test",
    shopping_list: "milk 2L, eggs 12, bananas 1kg",
    budget: "under 120 ILS",
    fulfillment: "delivery tomorrow morning",
    substitution_policy: "ask before substituting must-have items",
  });
  assert(groceryPlan.headless === false, `grocery plan should report non-headless state: ${JSON.stringify(groceryPlan)}`);
  assert(
    groceryPlan.host_viewer_ready === true &&
      groceryPlan.viewer_available === true &&
      !groceryPlan.viewer_unavailable_reason,
    `non-headless grocery plan should expose viewer availability: ${JSON.stringify(groceryPlan)}`,
  );
  assertNoPermissionBlockers(groceryPlan, "non-headless grocery plan");
  const browserViewerStep = groceryPlan.steps.find((step) => step.id === "open_viewer_when_browser_runs");
  assert(
    browserViewerStep?.tool === "workspace_open_viewer" &&
      browserViewerStep.open_world === true &&
      browserViewerStep.ready_to_call === false &&
      browserViewerStep.depends_on?.includes("run_browser_session_after_save"),
    `non-headless grocery plan should offer an optional viewer after browser workspace start: ${JSON.stringify(groceryPlan)}`,
  );
  assert(
    hasHostVisibleCheckpoint(groceryPlan, "open_viewer_when_browser_runs") &&
      groceryPlan.task_context?.approval_kinds?.includes("host_visible_ui"),
    `non-headless grocery plan should expose host-visible UI checkpoints: ${JSON.stringify(groceryPlan)}`,
  );

  const readOnlyControl = await callTool("mcp_control_update", {
    mode: "read_only",
    reason: "non-headless smoke verifies viewer remains available as a control surface",
  });
  assert(
    readOnlyControl.ok === true && readOnlyControl.status?.state?.mode === "read_only",
    `mcp_control_update did not switch to read_only: ${JSON.stringify(readOnlyControl)}`,
  );
  const readOnlyPlan = await callTool("mcp_task_plan", {
    intent: "app QA",
    project_path: repoRoot,
  });
  const readOnlyViewerStep = readOnlyPlan.steps.find((step) => step.id === "open_viewer_when_project_runs");
  const readOnlyRunStep = readOnlyPlan.steps.find((step) => step.id === "run_project_profile_after_save");
  assert(
    readOnlyRunStep?.blocked_by_live_control === true &&
      readOnlyPlan.approval_checkpoints?.some(
        (checkpoint) =>
          checkpoint.step_id === "run_project_profile_after_save" &&
          checkpoint.kind === "live_control" &&
          checkpoint.blocks_step === true,
      ),
    `read-only plan should block real workspace starts on live control: ${JSON.stringify(readOnlyPlan)}`,
  );
  assert(
    readOnlyViewerStep?.tool === "workspace_open_viewer" &&
      readOnlyViewerStep.open_world === true &&
      readOnlyViewerStep.blocked_by_live_control === false &&
      hasHostVisibleCheckpoint(readOnlyPlan, "open_viewer_when_project_runs") &&
      !readOnlyPlan.approval_checkpoints?.some(
        (checkpoint) =>
          checkpoint.step_id === "open_viewer_when_project_runs" &&
          checkpoint.kind === "live_control",
      ),
    `read-only plan should keep the host-visible viewer available for observe/control recovery: ${JSON.stringify(readOnlyPlan)}`,
  );

  const pausedControl = await callTool("mcp_control_update", {
    mode: "paused",
    reason: "non-headless smoke verifies paused viewer recovery surface",
  });
  assert(
    pausedControl.ok === true && pausedControl.status?.state?.mode === "paused",
    `mcp_control_update did not switch to paused: ${JSON.stringify(pausedControl)}`,
  );
  const pausedPlan = await callTool("mcp_task_plan", {
    intent: "grocery shopping",
    user_data_dir: browserDataDir,
    target_url: "https://example-grocery.test",
    shopping_list: "milk 2L, eggs 12, bananas 1kg",
    budget: "under 120 ILS",
    fulfillment: "delivery tomorrow morning",
    substitution_policy: "ask before substituting must-have items",
  });
  const pausedViewerStep = pausedPlan.steps.find((step) => step.id === "open_viewer_when_browser_runs");
  const pausedRunStep = pausedPlan.steps.find((step) => step.id === "run_browser_session_after_save");
  assert(
    pausedRunStep?.blocked_by_live_control === true &&
      pausedPlan.approval_checkpoints?.some(
        (checkpoint) =>
          checkpoint.step_id === "run_browser_session_after_save" &&
          checkpoint.kind === "live_control" &&
          checkpoint.blocks_step === true,
      ),
    `paused plan should block real browser starts on live control: ${JSON.stringify(pausedPlan)}`,
  );
  assert(
    pausedViewerStep?.tool === "workspace_open_viewer" &&
      pausedViewerStep.open_world === true &&
      pausedViewerStep.blocked_by_live_control === false &&
      hasHostVisibleCheckpoint(pausedPlan, "open_viewer_when_browser_runs") &&
      !pausedPlan.approval_checkpoints?.some(
        (checkpoint) =>
          checkpoint.step_id === "open_viewer_when_browser_runs" &&
          checkpoint.kind === "live_control",
      ),
    `paused plan should keep the host-visible viewer available for observe/control recovery: ${JSON.stringify(pausedPlan)}`,
  );

  const activeControl = await callTool("mcp_control_update", {
    mode: "active",
    confirmed_user_request: true,
    reason: "non-headless smoke restores active after explicit confirmation",
  });
  assert(
    activeControl.ok === true && activeControl.status?.state?.mode === "active",
    `mcp_control_update did not restore active mode: ${JSON.stringify(activeControl)}`,
  );
}

main()
  .then(() => {
    child.kill("SIGTERM");
    fs.rmSync(tempDir, { recursive: true, force: true });
    console.log("non-headless mcp viewer smoke passed");
  })
  .catch((error) => {
    try {
      child.kill("SIGTERM");
    } catch {
      // ignore cleanup races
    }
    console.error(error.stack || error.message || String(error));
    console.error(stderr);
    process.exit(1);
  });
