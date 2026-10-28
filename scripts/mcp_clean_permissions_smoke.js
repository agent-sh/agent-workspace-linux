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

// Keep private D-Bus socket paths short when TMPDIR is an on-disk cache.
const tempDir = fs.mkdtempSync(path.join(os.tmpdir(), "awl-clean-"));
const configDir = path.join(tempDir, "config");
const runtimeDir = path.join(tempDir, "runtime");
const browserDataDir = path.join(tempDir, "browser-data");
fs.mkdirSync(configDir, { recursive: true });
fs.mkdirSync(runtimeDir, { recursive: true, mode: 0o700 });
fs.chmodSync(runtimeDir, 0o700);
fs.mkdirSync(browserDataDir, { recursive: true });

const child = childProcess.spawn(bin, ["mcp", "--headless"], {
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

async function main() {
  const initializeResult = await request("initialize", {
    protocolVersion: "2024-11-05",
    capabilities: {},
    clientInfo: { name: "agent-workspace-linux-clean-smoke", version: "0" },
  });
  notify("notifications/initialized", {});
  const instructions = String(initializeResult.instructions || "");
  assert(
    /configured=false/.test(instructions) &&
      /does not impose its own ceiling/.test(instructions) &&
      /host\/client harness boundary/.test(instructions),
    `clean MCP initialize instructions should point to the harness-owned boundary: ${instructions}`,
  );
  assert(
    /mcp_action_catalog/.test(instructions) &&
      /read-only/.test(instructions) &&
      /idempotent/.test(instructions) &&
      /host-visible\/open-world/.test(instructions),
    `clean MCP initialize instructions should keep action taxonomy advisory: ${instructions}`,
  );
  assert(
    /confirmed_user_request=true/.test(instructions) && /reactivating/.test(instructions),
    `clean MCP initialize instructions should teach explicit live-control reactivation: ${instructions}`,
  );

  const tools = await request("tools/list", {});
  const toolByName = new Map((tools.tools || []).map((tool) => [tool.name, tool]));
  const toolNames = new Set(toolByName.keys());
  for (const name of ["mcp_permissions", "mcp_action_catalog", "mcp_session_brief", "mcp_task_plan"]) {
    assert(toolNames.has(name), `tools/list did not include ${name}`);
  }
  const permissionsDescription = toolByName.get("mcp_permissions")?.description || "";
  assert(
    /configured=false/.test(permissionsDescription) && /imposes no ceiling/.test(permissionsDescription),
    `clean MCP mcp_permissions description should distinguish clean/default MCP: ${permissionsDescription}`,
  );

  const permissions = await callTool("mcp_permissions");
  assert(permissions.configured === false, `clean MCP should report configured=false: ${JSON.stringify(permissions)}`);
  assert(permissions.restricted === false, `clean MCP should report restricted=false: ${JSON.stringify(permissions)}`);
  assert(
    !permissions.ceiling?.network &&
      (permissions.ceiling?.mounts || []).length === 0 &&
      (permissions.ceiling?.apps?.allow || []).length === 0,
    `clean MCP should not synthesize a permission ceiling: ${JSON.stringify(permissions)}`,
  );
  assert(
    /host\/client session controls workspace permissions/.test(permissions.message || ""),
    `clean MCP should point agents to the harness/session boundary: ${JSON.stringify(permissions)}`,
  );

  const catalog = await callTool("mcp_action_catalog");
  assert(
    catalog.notes?.some((note) => /configured=false/.test(note) && /advisory action classification/.test(note)),
    `clean MCP action catalog should say configured=false is advisory classification: ${JSON.stringify(catalog)}`,
  );
  const catalogByName = assertCatalogMatchesTools(catalog, toolByName, "clean MCP");
  assert(
    /configured=false/.test(catalogByName.get("mcp_permissions")?.notes || "") &&
      /no MCP ceiling/.test(catalogByName.get("mcp_permissions")?.notes || ""),
    `clean MCP action catalog should not describe mcp_permissions as an imposed ceiling: ${JSON.stringify(catalogByName.get("mcp_permissions"))}`,
  );
  assert(
    catalogByName.get("workspace_start")?.control_behavior === "blocked_when_not_active_unless_dry_run",
    "clean MCP should still classify workspace_start behavior instead of adding a ceiling",
  );
  for (const name of ["workspace_start", "workspace_open_profile"]) {
    const tool = toolByName.get(name);
    const catalogEntry = catalogByName.get(name);
    const openWorldHint = tool?.annotations?.openWorldHint ?? tool?.annotations?.open_world_hint;
    assert(
      openWorldHint === true &&
        catalogEntry?.open_world === true &&
        /open_viewer=false/.test(tool?.description || "") &&
        catalogEntry?.parameter_notes?.some((note) => note.parameter === "open_viewer"),
      `clean MCP should expose ${name} viewer auto-open as host-visible/open-world with an explicit opt-out: tool=${JSON.stringify(tool)} catalog=${JSON.stringify(catalogEntry)}`,
    );
  }
  assert(
    catalogByName.get("workspace_open_viewer")?.open_world === true,
    "clean MCP should still classify host-visible viewer as open-world",
  );

  const sessionBrief = await callTool("mcp_session_brief");
  assert(sessionBrief.permissions?.configured === false, "session brief should preserve clean MCP permissions");
  assert(
    !sessionBrief.permissions?.ceiling?.network &&
      (sessionBrief.permissions?.ceiling?.mounts || []).length === 0 &&
      (sessionBrief.permissions?.ceiling?.apps?.allow || []).length === 0,
    `session brief should not synthesize a clean MCP ceiling: ${JSON.stringify(sessionBrief)}`,
  );
  assert(
    sessionBrief.recommendations.some(
      (action) =>
        action.id === "classify_action_before_acting" &&
        action.action_type === "read_only" &&
        action.idempotent === true,
    ),
    `clean MCP should recommend advisory action classification: ${JSON.stringify(sessionBrief)}`,
  );
  assert(
    sessionBrief.recommendations.some((action) => action.id === "plan_app_qa_without_profile"),
    `clean MCP should recommend read-only app QA planning: ${JSON.stringify(sessionBrief)}`,
  );
  assert(
    sessionBrief.recommendations.some(
      (action) =>
        action.id === "plan_browser_or_grocery_task" &&
        action.action_type === "read_only" &&
        action.idempotent === true &&
        action.approval_summary?.next_boundary?.kind === "real_world_action" &&
        action.approval_summary?.approval_kinds?.includes("real_world_action") &&
        action.approval_checkpoints?.some(
          (checkpoint) =>
            checkpoint.kind === "real_world_action" &&
            checkpoint.approval_required === true &&
            checkpoint.blocks_action === false,
        ),
    ),
    `clean MCP should recommend read-only browser/grocery planning with real-world approval metadata: ${JSON.stringify(sessionBrief)}`,
  );
  assert(
    sessionBrief.approval_summary?.next_boundary?.kind === "real_world_action" &&
      sessionBrief.approval_summary?.approval_kinds?.includes("real_world_action"),
    `clean MCP session brief should expose a compact recommendation approval summary: ${JSON.stringify(sessionBrief)}`,
  );
  assert(
    !sessionBrief.recommendations.some((action) =>
      action.approval_checkpoints?.some((checkpoint) => checkpoint.kind === "host_visible_ui"),
    ),
    `clean MCP --headless recommendations should not expose host-visible UI checkpoints: ${JSON.stringify(sessionBrief)}`,
  );

  const generatedProjectProfile = await callTool("profile_template", {
    kind: "project-dev",
    id: "clean-project-dev",
    host_path: repoRoot,
  });
  assert(
    generatedProjectProfile.ok === true && generatedProjectProfile.profile,
    `clean MCP should not block project-dev profile generation: ${JSON.stringify(generatedProjectProfile)}`,
  );

  const projectProfileDryRun = await callTool("profile_put", {
    dry_run: true,
    profile: generatedProjectProfile.profile,
  });
  assert(
    projectProfileDryRun.ok === true &&
      projectProfileDryRun.dry_run === true &&
      projectProfileDryRun.would_create === true,
    `clean MCP should not enforce its own ceiling during profile_put dry-run: ${JSON.stringify(projectProfileDryRun)}`,
  );

  const generatedBrowserProfile = await callTool("profile_template", {
    kind: "browser-session",
    id: "clean-browser-session",
    user_data_dir: browserDataDir,
  });
  assert(
    generatedBrowserProfile.ok === true && generatedBrowserProfile.profile,
    `clean MCP should not block browser-session profile generation: ${JSON.stringify(generatedBrowserProfile)}`,
  );

  const appQaPlan = await callTool("mcp_task_plan", {
    intent: "app QA",
    project_path: repoRoot,
  });
  assert(appQaPlan.permissions?.configured === false, "app QA plan should preserve clean MCP permissions");
  assertNoPermissionBlockers(appQaPlan, "app QA plan");
  assert(
    appQaPlan.steps.some((step) => step.id === "template_project_profile" && step.ready_to_call === true),
    `clean app QA plan should allow project profile template generation: ${JSON.stringify(appQaPlan)}`,
  );
  assert(
    appQaPlan.steps.some(
      (step) =>
        step.id === "run_project_profile_after_save" &&
        !step.ready_to_call &&
        step.depends_on?.includes("save_project_profile_after_review"),
    ),
    `clean app QA plan should carry generated project profiles through approved run: ${JSON.stringify(appQaPlan)}`,
  );
  assert(
    appQaPlan.steps.some(
      (step) =>
        step.id === "observe_project_workspace" &&
        !step.ready_to_call &&
        step.read_only === true &&
        step.depends_on?.includes("run_project_profile_after_save"),
    ),
    `clean app QA plan should include post-start observation: ${JSON.stringify(appQaPlan)}`,
  );
  assert(
    appQaPlan.steps.some(
      (step) =>
        step.id === "read_project_events_after_start" &&
        !step.ready_to_call &&
        step.read_only === true &&
        step.depends_on?.includes("observe_project_workspace"),
    ) &&
      appQaPlan.steps.some(
        (step) =>
          step.id === "read_project_app_log_after_start" &&
          !step.ready_to_call &&
          step.required_input?.some((input) => /app_id/i.test(input)),
      ) &&
      appQaPlan.steps.some(
        (step) =>
          step.id === "capture_project_window_after_start" &&
          !step.ready_to_call &&
          step.required_input?.some((input) => /active_window|app_id/i.test(input)),
    ),
    `clean app QA plan should collect evidence after start before input: ${JSON.stringify(appQaPlan)}`,
  );
  assert(
    appQaPlan.normalized_intent === "app_qa" &&
      appQaPlan.task_context?.task_kind === "app_qa" &&
      appQaPlan.task_context?.action_boundaries?.some(
        (boundary) =>
          boundary.id === "observe_project_state" &&
          boundary.action_type === "read_only_observation" &&
          boundary.ready === true &&
          boundary.approval_required === false,
      ) &&
      appQaPlan.task_context?.action_boundaries?.some(
        (boundary) =>
          boundary.id === "start_or_attach_project_workspace" &&
          boundary.action_type === "hidden_workspace_start" &&
          boundary.ready === true &&
          boundary.approval_required === true &&
          boundary.approval_kind === "hidden_workspace",
      ) &&
      appQaPlan.task_context?.action_boundaries?.some(
        (boundary) =>
          boundary.id === "drive_workspace_app" &&
          boundary.action_type === "workspace_input" &&
          boundary.approval_required === false &&
          boundary.required_inputs?.includes("stable_app_id_or_window"),
      ) &&
      appQaPlan.task_context?.action_boundaries?.some(
        (boundary) =>
          boundary.id === "write_mounted_project_files" &&
          boundary.action_type === "project_file_mutation" &&
          boundary.approval_required === true &&
          boundary.approval_kind === "project_file_write" &&
          boundary.required_inputs?.includes("explicit_code_change_request"),
      ) &&
      appQaPlan.task_context?.approval_kinds?.includes("project_file_write"),
    `clean app QA plan should expose structured app-QA action boundaries: ${JSON.stringify(appQaPlan)}`,
  );
  assert(
    appQaPlan.approval_summary?.next_boundary?.kind === "required_input" &&
      appQaPlan.approval_summary?.next_boundary?.step_id === "dry_run_save_project_profile" &&
      appQaPlan.approval_summary?.next_boundary?.blocks_step === true &&
      appQaPlan.approval_summary?.approval_kinds?.includes("project_file_write"),
    `clean app QA plan should expose a compact next approval/input boundary: ${JSON.stringify(appQaPlan)}`,
  );

  const naturalAppQaPlan = await callTool("mcp_task_plan", {
    intent: "test the local UI",
    project_path: repoRoot,
  });
  assert(
    naturalAppQaPlan.normalized_intent === "app_qa" &&
      naturalAppQaPlan.task_context?.task_kind === "app_qa" &&
      naturalAppQaPlan.task_context?.action_boundaries?.some(
        (boundary) =>
          boundary.id === "start_or_attach_project_workspace" &&
          boundary.action_type === "hidden_workspace_start",
      ) &&
      naturalAppQaPlan.task_context?.action_boundaries?.some(
        (boundary) => boundary.id === "drive_workspace_app" && boundary.action_type === "workspace_input",
      ),
    `clean MCP should infer app-QA boundaries from natural testing language: ${JSON.stringify(naturalAppQaPlan)}`,
  );

  const groceryPlan = await callTool("mcp_task_plan", {
    intent: "grocery shopping",
    user_data_dir: browserDataDir,
  });
  assert(groceryPlan.permissions?.configured === false, "grocery plan should preserve clean MCP permissions");
  assert(
    groceryPlan.viewer_available === false &&
      /--headless/.test(String(groceryPlan.viewer_unavailable_reason || "")),
    `clean MCP --headless grocery plan should explain viewer unavailability: ${JSON.stringify(groceryPlan)}`,
  );
  assertNoPermissionBlockers(groceryPlan, "grocery plan");
  assert(
    groceryPlan.task_context?.task_kind === "browser_task" &&
      groceryPlan.task_context?.provided_inputs?.some((input) => input.name === "user_data_dir") &&
      groceryPlan.task_context?.missing_inputs?.some((input) => input.name === "target_url") &&
      groceryPlan.task_context?.missing_inputs?.some((input) => input.name === "shopping_list") &&
      groceryPlan.task_context?.approval_kinds?.includes("profile_write") &&
      groceryPlan.task_context?.approval_kinds?.includes("hidden_workspace") &&
      groceryPlan.task_context?.approval_kinds?.includes("cart_mutation") &&
      groceryPlan.task_context?.approval_kinds?.includes("real_world_action"),
    `clean grocery plan should expose structured task context without permission blockers: ${JSON.stringify(groceryPlan)}`,
  );
  assert(
    groceryPlan.approval_summary?.next_boundary?.kind === "required_input" &&
      groceryPlan.approval_summary?.next_boundary?.blocks_step === true &&
      groceryPlan.approval_summary?.approval_kinds?.includes("cart_mutation") &&
      groceryPlan.approval_summary?.approval_kinds?.includes("real_world_action"),
    `clean grocery plan should expose compact approval summary for host UI: ${JSON.stringify(groceryPlan)}`,
  );
  assert(
    groceryPlan.task_context?.action_boundaries?.some(
      (boundary) =>
        boundary.id === "navigate_and_search" &&
        boundary.action_type === "browser_navigation" &&
        boundary.approval_required === false &&
        boundary.ready === false &&
        boundary.required_inputs?.includes("target_url"),
    ) &&
      groceryPlan.task_context?.action_boundaries?.some(
        (boundary) =>
          boundary.id === "draft_cart_changes" &&
          boundary.action_type === "cart_mutation" &&
          boundary.approval_required === true &&
          boundary.approval_kind === "cart_mutation" &&
          boundary.approved === false &&
          boundary.missing_approvals?.includes("explicit_cart_mutation_approval") &&
          boundary.required_inputs?.includes("shopping_list"),
      ) &&
      groceryPlan.task_context?.action_boundaries?.some(
        (boundary) =>
          boundary.id === "checkout_order_or_account_change" &&
          boundary.action_type === "real_world_action" &&
          boundary.approval_required === true &&
          boundary.approval_kind === "real_world_action" &&
          boundary.approved === false &&
          boundary.missing_approvals?.includes("final_cart_review") &&
          boundary.required_inputs?.includes("explicit_checkout_approval"),
      ),
    `clean grocery plan should expose structured browser/cart/checkout action boundaries: ${JSON.stringify(groceryPlan)}`,
  );
  assert(
    groceryPlan.needs_user_input.some((need) => /target_url/.test(need)) &&
      groceryPlan.needs_user_input.some((need) => /shopping_list/.test(need)) &&
      groceryPlan.needs_user_input.some((need) => /fulfillment/.test(need)) &&
      groceryPlan.needs_user_input.some((need) => /substitution_policy/.test(need)) &&
      groceryPlan.needs_user_input.some((need) => /budget/.test(need)),
    `clean grocery plan should ask for shopping task inputs without treating them as permissions: ${JSON.stringify(groceryPlan)}`,
  );
  assert(
    groceryPlan.steps.some((step) => step.id === "template_browser_session" && step.ready_to_call === true),
    `clean grocery plan should allow browser-session template generation after user_data_dir is explicit: ${JSON.stringify(groceryPlan)}`,
  );
  assert(
    groceryPlan.approval_checkpoints?.some(
      (checkpoint) =>
        checkpoint.step_id === "dry_run_save_browser_profile" &&
        checkpoint.kind === "preview_surface" &&
        checkpoint.approval_required === false,
    ),
    `clean grocery plan should expose profile-save dry-run as a structured approval surface: ${JSON.stringify(groceryPlan)}`,
  );
  assert(
    groceryPlan.approval_checkpoints?.some(
      (checkpoint) =>
        checkpoint.step_id === "save_browser_profile_after_review" &&
        checkpoint.kind === "profile_write" &&
        checkpoint.approval_required === true &&
        checkpoint.blocks_step === true,
    ),
    `clean grocery plan should expose profile writes as approval checkpoints: ${JSON.stringify(groceryPlan)}`,
  );
  assert(
    groceryPlan.approval_checkpoints?.some(
      (checkpoint) =>
        checkpoint.step_id === "run_browser_session_after_save" &&
        checkpoint.kind === "hidden_workspace" &&
        checkpoint.approval_required === true,
    ) &&
      groceryPlan.approval_checkpoints?.some(
        (checkpoint) =>
          checkpoint.step_id === "run_browser_session_after_save" &&
          checkpoint.kind === "real_world_action" &&
          checkpoint.approval_required === true,
      ),
    `clean grocery plan should expose hidden-workspace and real-world approval checkpoints: ${JSON.stringify(groceryPlan)}`,
  );
  assert(
    groceryPlan.steps.some(
      (step) =>
        step.id === "read_browser_events_after_start" &&
        !step.ready_to_call &&
        step.read_only === true &&
        step.depends_on?.includes("observe_browser_workspace"),
    ) &&
      groceryPlan.steps.some(
        (step) =>
          step.id === "capture_browser_window_after_start" &&
          !step.ready_to_call &&
          step.required_input?.some((input) => /active_window|app_id/i.test(input)),
      ) &&
      groceryPlan.steps.some(
        (step) =>
          step.id === "confirm_real_world_boundary_after_start" &&
          !step.ready_to_call &&
          step.required_input?.some((input) => /target_url/.test(input)) &&
          step.required_input?.some((input) => /shopping_list/.test(input)) &&
          step.required_input?.some((input) => /substitution_policy/.test(input)) &&
          step.required_input?.some((input) => /checkout|account changes/i.test(input)),
      ),
    `clean grocery plan should collect browser evidence and reconfirm real-world boundaries after start: ${JSON.stringify(groceryPlan)}`,
  );
  assert(
    !groceryPlan.approval_checkpoints?.some((checkpoint) => checkpoint.kind === "host_visible_ui"),
    `clean MCP --headless grocery plan should not expose a host-visible viewer checkpoint: ${JSON.stringify(groceryPlan)}`,
  );

  const naturalShoppingPlan = await callTool("mcp_task_plan", {
    intent: "buy milk and eggs for delivery",
    user_data_dir: browserDataDir,
  });
  assert(
    naturalShoppingPlan.normalized_intent === "browser_task" &&
      naturalShoppingPlan.task_context?.task_kind === "browser_task" &&
      naturalShoppingPlan.needs_user_input.some((need) => /target_url/.test(need)) &&
      naturalShoppingPlan.needs_user_input.some((need) => /shopping_list/.test(need)) &&
      naturalShoppingPlan.task_context?.approval_kinds?.includes("cart_mutation") &&
      naturalShoppingPlan.task_context?.approval_kinds?.includes("real_world_action") &&
      naturalShoppingPlan.task_context?.action_boundaries?.some(
        (boundary) =>
          boundary.id === "checkout_order_or_account_change" &&
          boundary.action_type === "real_world_action" &&
          boundary.approval_required === true,
      ),
    `clean MCP should infer browser/grocery boundaries from natural buying language: ${JSON.stringify(naturalShoppingPlan)}`,
  );

  const completeGroceryPlan = await callTool("mcp_task_plan", {
    intent: "grocery shopping",
    user_data_dir: browserDataDir,
    target_url: "https://example-grocery.test",
    shopping_list: "milk 2L, eggs 12, bananas 1kg",
    budget: "under 120 ILS",
    fulfillment: "delivery tomorrow morning",
    substitution_policy: "ask before substituting must-have items",
  });
  assert(
    completeGroceryPlan.permissions?.configured === false,
    "complete grocery plan should preserve clean MCP permissions",
  );
  assert(
    completeGroceryPlan.viewer_available === false &&
      /--headless/.test(String(completeGroceryPlan.viewer_unavailable_reason || "")),
    `clean MCP --headless complete grocery plan should explain viewer unavailability: ${JSON.stringify(completeGroceryPlan)}`,
  );
  assertNoPermissionBlockers(completeGroceryPlan, "complete grocery plan");
  const providedNames = new Set(completeGroceryPlan.task_context?.provided_inputs?.map((input) => input.name) || []);
  const missingNames = new Set(completeGroceryPlan.task_context?.missing_inputs?.map((input) => input.name) || []);
  for (const name of [
    "user_data_dir",
    "target_url",
    "shopping_list",
    "budget",
    "fulfillment",
    "substitution_policy",
  ]) {
    assert(
      providedNames.has(name),
      `complete grocery plan should preserve provided ${name}: ${JSON.stringify(completeGroceryPlan)}`,
    );
    assert(
      !missingNames.has(name),
      `complete grocery plan should not report supplied ${name} as missing: ${JSON.stringify(completeGroceryPlan)}`,
    );
    assert(
      !(completeGroceryPlan.needs_user_input || []).some((need) => need.includes(name)),
      `complete grocery plan should not ask again for supplied ${name}: ${JSON.stringify(completeGroceryPlan)}`,
    );
  }
  assert(
    missingNames.size === 0 &&
      completeGroceryPlan.task_context?.approval_kinds?.includes("profile_write") &&
      completeGroceryPlan.task_context?.approval_kinds?.includes("hidden_workspace") &&
      completeGroceryPlan.task_context?.approval_kinds?.includes("cart_mutation") &&
      completeGroceryPlan.task_context?.approval_kinds?.includes("real_world_action"),
    `complete grocery plan should move all task details to provided inputs while retaining approval boundaries: ${JSON.stringify(completeGroceryPlan)}`,
  );
  assert(
    completeGroceryPlan.approval_summary?.next_boundary?.kind === "required_input" &&
      completeGroceryPlan.approval_summary?.next_boundary?.step_id === "dry_run_save_browser_profile" &&
      completeGroceryPlan.approval_summary?.approval_kinds?.includes("cart_mutation") &&
      completeGroceryPlan.approval_summary?.approval_kinds?.includes("real_world_action"),
    `complete grocery plan should summarize the next host approval/input boundary: ${JSON.stringify(completeGroceryPlan)}`,
  );
  assert(
    completeGroceryPlan.task_context?.action_boundaries?.some(
      (boundary) =>
        boundary.id === "navigate_and_search" &&
        boundary.action_type === "browser_navigation" &&
        boundary.approval_required === false &&
        boundary.ready === true &&
        (boundary.required_inputs || []).length === 0,
    ) &&
      completeGroceryPlan.task_context?.action_boundaries?.some(
        (boundary) =>
          boundary.id === "compare_items_and_prices" &&
          boundary.action_type === "shopping_research" &&
          boundary.approval_required === false &&
          boundary.ready === true &&
          (boundary.required_inputs || []).length === 0,
      ) &&
      completeGroceryPlan.task_context?.action_boundaries?.some(
        (boundary) =>
          boundary.id === "draft_cart_changes" &&
          boundary.action_type === "cart_mutation" &&
          boundary.approval_required === true &&
          boundary.approval_kind === "cart_mutation" &&
          boundary.ready === true &&
          boundary.approved === false &&
          boundary.missing_approvals?.includes("explicit_cart_mutation_approval") &&
          (boundary.required_inputs || []).length === 0,
      ) &&
      completeGroceryPlan.task_context?.action_boundaries?.some(
        (boundary) =>
          boundary.id === "checkout_order_or_account_change" &&
          boundary.action_type === "real_world_action" &&
          boundary.approval_required === true &&
          boundary.approval_kind === "real_world_action" &&
          boundary.ready === false &&
          boundary.approved === false &&
          boundary.missing_approvals?.includes("final_cart_review") &&
          boundary.required_inputs?.includes("explicit_checkout_approval"),
      ),
    `complete grocery plan should mark research/cart scope ready while keeping checkout blocked: ${JSON.stringify(completeGroceryPlan)}`,
  );
  assert(
    completeGroceryPlan.steps.some(
      (step) =>
        step.id === "run_browser_session_after_save" &&
        !step.required_input?.some((input) => /target_url|shopping_list|budget|fulfillment|substitution_policy/.test(input)) &&
        step.required_input?.some((input) => /checkout|purchases|account changes/i.test(input)),
    ) &&
      completeGroceryPlan.steps.some(
        (step) =>
          step.id === "confirm_real_world_boundary_after_start" &&
          !step.required_input?.some((input) => /target_url|shopping_list|budget|fulfillment|substitution_policy/.test(input)) &&
          step.required_input?.some((input) => /checkout|account changes/i.test(input)),
      ),
    `complete grocery plan should not repeat supplied grocery inputs in post-start gates: ${JSON.stringify(completeGroceryPlan)}`,
  );
  assert(
    !completeGroceryPlan.approval_checkpoints?.some((checkpoint) => checkpoint.kind === "host_visible_ui"),
    `clean MCP --headless complete grocery plan should not expose a host-visible viewer checkpoint: ${JSON.stringify(completeGroceryPlan)}`,
  );

  const viewerDenied = await callTool("workspace_open_viewer", { id: "default" });
  assert(
    viewerDenied.ok === false && /--headless/.test(String(viewerDenied.message || "")),
    "clean MCP --headless should still be the host-visible viewer boundary",
  );
}

main()
  .then(() => {
    child.kill("SIGTERM");
    fs.rmSync(tempDir, { recursive: true, force: true });
    console.log("clean mcp permissions smoke passed");
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
