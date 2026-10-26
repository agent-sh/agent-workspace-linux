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
const stopViaMcp = process.env.AGENT_WORKSPACE_MCP_STOP !== "0";

if (!fs.existsSync(bin)) {
  throw new Error(`agent-workspace-linux binary not found at ${bin}; run cargo build first`);
}

const tempDir = fs.mkdtempSync(path.join(os.tmpdir(), "agent-workspace-mcp-smoke-"));
const permissionsPath = path.join(tempDir, "permissions.json");
const configDir = path.join(tempDir, "config");
const runtimeDir = path.join(tempDir, "runtime");
fs.mkdirSync(configDir, { recursive: true });
fs.mkdirSync(runtimeDir, { recursive: true, mode: 0o700 });
fs.chmodSync(runtimeDir, 0o700);
fs.writeFileSync(
  permissionsPath,
  `${JSON.stringify(
    {
      apps: { allow: ["sh"] },
    },
    null,
    2,
  )}\n`,
);

const childEnv = {
  ...process.env,
  XDG_CONFIG_HOME: configDir,
  XDG_RUNTIME_DIR: runtimeDir,
};

const child = childProcess.spawn(bin, ["mcp", "--headless", "--permissions", permissionsPath], {
  cwd: repoRoot,
  env: childEnv,
  stdio: ["pipe", "pipe", "pipe"],
});

let nextId = 1;
let stdoutBuffer = "";
let stderr = "";
const pending = new Map();
let smokeDaemonPid = null;

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
    } catch (error) {
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
  const result = await request("tools/call", {
    name,
    arguments: args || {},
  }, timeoutMs, `tools/call ${name}`);
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
  const catalogByName = new Map(catalog.tools.map((tool) => [tool.name, tool]));
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

function childProcessesOf(pid) {
  const result = childProcess.spawnSync("ps", ["--ppid", String(pid), "-o", "pid=,stat=,cmd="], {
    encoding: "utf8",
  });
  if (result.status !== 0 && result.status !== 1) {
    fail(`ps --ppid ${pid} failed: ${result.stderr || result.stdout}`);
  }
  return String(result.stdout || "")
    .trim()
    .split("\n")
    .map((line) => line.trim())
    .filter(Boolean);
}

function processThreadsOf(pid) {
  const result = childProcess.spawnSync("ps", ["-T", "-p", String(pid), "-o", "pid=,tid=,stat=,wchan=,comm="], {
    encoding: "utf8",
  });
  return String(result.stdout || "").trim();
}

async function assertNoZombieChildren(pid) {
  const deadline = Date.now() + 2000;
  let zombies = [];
  do {
    const children = childProcessesOf(pid);
    zombies = children.filter((line) => {
      const fields = line.split(/\s+/);
      return fields[1]?.startsWith("Z");
    });
    if (zombies.length === 0) return;
    await new Promise((resolve) => setTimeout(resolve, 50));
  } while (Date.now() < deadline);
  fail(
    `MCP server left zombie child processes (daemon_pid=${smokeDaemonPid ?? "unknown"}):\n${zombies.join("\n")}\nMCP threads:\n${processThreadsOf(pid)}`,
  );
}

async function main() {
  const initializeResult = await request("initialize", {
    protocolVersion: "2024-11-05",
    capabilities: {},
    clientInfo: { name: "agent-workspace-linux-smoke", version: "0" },
  });
  notify("notifications/initialized", {});
  const instructions = String(initializeResult.instructions || "");
  assert(
    /configured=true/.test(instructions) &&
      /immutable spawn-time ceiling/.test(instructions) &&
      /clients may only narrow/.test(instructions),
    `initialize instructions should teach the explicit MCP ceiling contract: ${instructions}`,
  );
  assert(
    /configured=false/.test(instructions) &&
      /does not impose its own ceiling/.test(instructions) &&
      /host\/client harness boundary/.test(instructions),
    `initialize instructions should teach the clean MCP harness-owned contract: ${instructions}`,
  );
  assert(
    /confirmed_user_request=true/.test(instructions) && /reactivating/.test(instructions),
    `initialize instructions should teach explicit live-control reactivation: ${instructions}`,
  );
  assert(
    /--headless/.test(instructions) && /host-visible UI/.test(instructions),
    `initialize instructions should teach the headless host-visible UI boundary: ${instructions}`,
  );

  const tools = await request("tools/list", {});
  const toolByName = new Map((tools.tools || []).map((tool) => [tool.name, tool]));
  for (const name of [
    "mcp_permissions",
    "mcp_action_catalog",
    "mcp_session_brief",
    "mcp_task_plan",
    "mcp_control_state",
    "mcp_control_update",
    "workspace_launch_app",
    "workspace_run_app",
    "workspace_run_profile_setup",
    "workspace_launch_profile_apps",
    "workspace_click",
    "workspace_key",
    "workspace_type_text",
  ]) {
    const tool = toolByName.get(name);
    assert(tool, `tools/list did not include ${name}`);
    const openWorldHint = tool.annotations?.openWorldHint ?? tool.annotations?.open_world_hint;
    assert(
      openWorldHint !== true,
      `${name} should not request open-world approval; hidden-workspace approval and MCP ceilings own that boundary`,
    );
  }
  for (const name of ["workspace_start", "workspace_open_profile"]) {
    const tool = toolByName.get(name);
    assert(tool, `tools/list did not include ${name}`);
    const openWorldHint = tool.annotations?.openWorldHint ?? tool.annotations?.open_world_hint;
    assert(
      openWorldHint === true &&
        /viewer/i.test(tool.description || "") &&
        /open_viewer=false/.test(tool.description || "") &&
        /--headless/.test(tool.description || ""),
      `${name} should expose its default host-visible viewer auto-open and explicit opt-out: ${JSON.stringify(tool)}`,
    );
  }
  const permissionsDescription = toolByName.get("mcp_permissions")?.description || "";
  assert(
    /configured=true/.test(permissionsDescription) &&
      /immutable ceiling/.test(permissionsDescription) &&
      /configured=false/.test(permissionsDescription) &&
      /imposes no ceiling/.test(permissionsDescription),
    `mcp_permissions description should distinguish explicit ceilings from clean/default MCP: ${permissionsDescription}`,
  );
  const launchDescription = toolByName.get("workspace_launch_app")?.description || "";
  assert(
    /app_id/.test(launchDescription) && /titles? often change/i.test(launchDescription),
    "workspace_launch_app description should tell agents to reuse returned app_id instead of mutable titles",
  );
  const pasteDescription = toolByName.get("workspace_paste_window")?.description || "";
  assert(
    /Prefer app_id/.test(pasteDescription) && /titles can change/i.test(pasteDescription),
    "workspace_paste_window description should prefer app_id for launched GUI apps",
  );
  const viewerDescription = toolByName.get("workspace_open_viewer")?.description || "";
  assert(
    /host-visible/.test(viewerDescription) &&
      /outside the MCP stdio server/.test(viewerDescription) &&
      /--headless/.test(viewerDescription) &&
      /always_on_top/.test(viewerDescription),
    "workspace_open_viewer description should make the host-visible child-process, headless, and opt-in always-on-top boundaries explicit",
  );
  const viewerOpenWorldHint =
    toolByName.get("workspace_open_viewer")?.annotations?.openWorldHint ??
    toolByName.get("workspace_open_viewer")?.annotations?.open_world_hint;
  assert(
    viewerOpenWorldHint === true,
    "workspace_open_viewer should be annotated as open-world because it opens a host-visible UI",
  );
  const viewerDenied = await callTool("workspace_open_viewer", { id: "default" });
  assert(
    viewerDenied.ok === false && /--headless/.test(String(viewerDenied.message || "")),
    "workspace_open_viewer should refuse to open a host-visible window when MCP runs --headless",
  );
  const actionCatalog = await callTool("mcp_action_catalog");
  assert(
    actionCatalog.version === 1 && Array.isArray(actionCatalog.tools),
    `mcp_action_catalog returned an unexpected payload: ${JSON.stringify(actionCatalog)}`,
  );
  assert(
    actionCatalog.notes?.some((note) => /configured=false/.test(note) && /advisory action classification/.test(note)) &&
      actionCatalog.notes?.some((note) => /configured=true/.test(note) && /immutable ceilings/.test(note)),
    `mcp_action_catalog should distinguish clean/default classification from configured ceilings: ${JSON.stringify(actionCatalog.notes)}`,
  );
  const catalogByName = assertCatalogMatchesTools(actionCatalog, toolByName, "locked MCP");
  assert(
    /configured=false/.test(catalogByName.get("mcp_permissions")?.notes || "") &&
      /no MCP ceiling/.test(catalogByName.get("mcp_permissions")?.notes || "") &&
      /configured=true/.test(catalogByName.get("mcp_permissions")?.notes || "") &&
      /only narrow/.test(catalogByName.get("mcp_permissions")?.notes || ""),
    `mcp_action_catalog mcp_permissions entry should describe both clean and configured permission states: ${JSON.stringify(catalogByName.get("mcp_permissions"))}`,
  );
  assert(
    catalogByName.get("workspace_open_viewer")?.open_world === true &&
      /headless/.test(catalogByName.get("workspace_open_viewer")?.control_behavior || ""),
    "mcp_action_catalog should classify workspace_open_viewer as headless-gated open-world",
  );
  assert(
    catalogByName
      .get("workspace_open_viewer")
      ?.parameter_notes?.some(
        (note) =>
          note.parameter === "always_on_top" &&
          /overlay|above/i.test(note.effect || "") &&
          /explicitly asks|explicitly want/i.test(note.approval_hint || ""),
      ),
    "mcp_action_catalog should document always_on_top as an opt-in host-visible viewer parameter",
  );
  assert(
    catalogByName.get("workspace_start")?.control_behavior ===
      "blocked_when_not_active_unless_dry_run" &&
      catalogByName.get("workspace_start")?.open_world === true &&
      catalogByName
        .get("workspace_start")
        ?.parameter_notes?.some((note) => note.parameter === "open_viewer" && /suppress/i.test(note.effect || "")) &&
      catalogByName.get("workspace_open_profile")?.open_world === true &&
      catalogByName
        .get("workspace_open_profile")
        ?.parameter_notes?.some((note) => note.parameter === "open_viewer" && /suppress/i.test(note.effect || "")),
    "mcp_action_catalog should classify workspace starts as dry-run-previewable mutations with default host-visible viewer auto-open and explicit open_viewer opt-out",
  );
  assert(
    catalogByName.get("workspace_stop")?.control_behavior === "safety_stop_allowed",
    "mcp_action_catalog should keep workspace_stop available as a safety stop",
  );
  assert(
    catalogByName.get("mcp_control_update")?.control_behavior === "control_plane_allowed",
    "mcp_action_catalog should classify mcp_control_update as a control-plane action",
  );
  assert(
    catalogByName
      .get("mcp_control_update")
      ?.parameter_notes?.some(
        (note) =>
          note.parameter === "confirmed_user_request" &&
          /re-enable mutating actions/i.test(note.live_control || "") &&
          /explicit user approval/i.test(note.approval_hint || ""),
      ),
    "mcp_action_catalog should document confirmed_user_request for live-control reactivation",
  );
  assert(
    catalogByName.get("workspace_status")?.read_only === true,
    "mcp_action_catalog should classify workspace_status as read-only",
  );
  assert(
    catalogByName.get("mcp_task_plan")?.read_only === true,
    "mcp_action_catalog should classify mcp_task_plan as read-only",
  );
  for (const tool of actionCatalog.tools) {
    if (tool.control_behavior === "blocked_when_not_active_unless_dry_run") {
      assert(
        tool.parameter_notes?.some((note) => note.parameter === "dry_run"),
        `${tool.name} should document its dry_run live-control behavior`,
      );
    }
  }
  assert(
    catalogByName
      .get("profile_export")
      ?.parameter_notes?.some(
        (note) =>
          note.parameter === "output_path" &&
          /host file|host filesystem/i.test(note.effect || "") &&
          /active/i.test(note.live_control || ""),
      ),
    "mcp_action_catalog should make profile_export output_path host writes explicit",
  );
  assert(
    catalogByName
      .get("workspace_wait_app")
      ?.parameter_notes?.some(
        (note) =>
          note.parameter === "kill_on_timeout" &&
          /terminate|termination/i.test(`${note.effect || ""} ${note.approval_hint || ""}`) &&
          /active/i.test(note.live_control || ""),
      ),
    "mcp_action_catalog should make workspace_wait_app kill_on_timeout conditional termination explicit",
  );
  assert(
    catalogByName
      .get("workspace_screenshot")
      ?.parameter_notes?.some(
        (note) =>
          note.parameter === "output_path" &&
          /host path|host file/i.test(note.effect || "") &&
          /observation/i.test(note.live_control || ""),
      ),
    "mcp_action_catalog should make screenshot output_path advisory writes explicit",
  );
  const sessionBrief = await callTool("mcp_session_brief");
  assert(
    sessionBrief.version === 1 &&
      sessionBrief.headless === true &&
      sessionBrief.permissions?.restricted === true &&
      Array.isArray(sessionBrief.recommendations),
    `mcp_session_brief returned an unexpected payload: ${JSON.stringify(sessionBrief)}`,
  );
  assert(
    sessionBrief.control?.mode === "active",
    `mcp_session_brief should report active control by default: ${JSON.stringify(sessionBrief.control)}`,
  );
  assert(
    sessionBrief.recommendations.some(
      (action) =>
        action.id === "review_permission_ceiling" &&
        action.action_type === "read_only" &&
        action.idempotent === true,
    ),
    "mcp_session_brief should recommend reviewing a configured restrictive ceiling",
  );
  assert(
    sessionBrief.recommendations.some(
      (action) =>
        action.id === "plan_app_qa_without_profile" &&
        action.tool === "mcp_task_plan" &&
        action.arguments?.intent === "app QA",
    ),
    "mcp_session_brief should offer a read-only app QA plan before default workspace starts",
  );
  assert(
    sessionBrief.recommendations.some(
      (action) =>
        action.id === "plan_browser_or_grocery_task" &&
        action.tool === "mcp_task_plan" &&
        /grocery/i.test(action.arguments?.intent || "") &&
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
    "mcp_session_brief should offer a read-only browser/grocery plan with structured real-world approval metadata",
  );
  assert(
    sessionBrief.approval_summary?.next_boundary?.kind === "real_world_action" &&
      sessionBrief.approval_summary?.approval_kinds?.includes("real_world_action"),
    `mcp_session_brief should summarize recommendation approval boundaries for hosts: ${JSON.stringify(sessionBrief)}`,
  );
  assert(
    !sessionBrief.recommendations.some((action) => action.tool === "workspace_open_viewer"),
    "mcp_session_brief should not recommend opening a host-visible viewer in --headless mode",
  );
  assert(
    !sessionBrief.recommendations.some((action) =>
      action.approval_checkpoints?.some((checkpoint) => checkpoint.kind === "host_visible_ui"),
    ),
    "mcp_session_brief --headless recommendations should not expose host-visible UI checkpoints",
  );
  const profileStorePath = path.join(configDir, "agent-workspace-linux", "profiles.json");
  fs.mkdirSync(path.dirname(profileStorePath), { recursive: true });
  fs.writeFileSync(
    profileStorePath,
    `${JSON.stringify(
      {
        profiles: [
          {
            id: "saved-too-open",
            setup_commands: [],
            startup_apps: [{ command: ["xmessage", "outside the allowlist"] }],
          },
        ],
      },
      null,
      2,
    )}\n`,
  );
  const sessionBriefWithProfile = await callTool("mcp_session_brief");
  assert(
    sessionBriefWithProfile.recommendations.some(
      (action) =>
        action.id === "plan_saved_profile_task" &&
        action.tool === "mcp_task_plan" &&
        action.arguments?.profile_id === "saved-too-open",
    ),
    `mcp_session_brief should derive a read-only task plan recommendation from saved profiles: ${JSON.stringify(sessionBriefWithProfile)}`,
  );
  const blockedSavedProfilePlan = await callTool("mcp_task_plan", {
    intent: "app QA",
    profile_id: "saved-too-open",
  });
  assert(
    blockedSavedProfilePlan.steps.some(
      (step) =>
        step.id === "preview_project_profile" &&
        !step.ready_to_call &&
        step.permission_blockers?.some((blocker) => /permission ceiling/.test(blocker)),
    ),
    `mcp_task_plan should preflight saved profiles against the active ceiling: ${JSON.stringify(blockedSavedProfilePlan)}`,
  );
  const blockedSavedBrowserPlan = await callTool("mcp_task_plan", {
    intent: "grocery shopping",
    profile_id: "saved-too-open",
  });
  assert(
    blockedSavedBrowserPlan.steps.some(
      (step) =>
        step.id === "run_browser_session_after_approval" &&
        !step.ready_to_call &&
        step.depends_on?.includes("preview_browser_session") &&
        step.permission_blockers?.some((blocker) => /permission ceiling/.test(blocker)) &&
        step.required_input?.some((input) => /target_url/.test(input)) &&
        step.required_input?.some((input) => /shopping_list/.test(input)) &&
        step.required_input?.some((input) => /checkout|purchases|account changes/i.test(input)),
    ),
    `mcp_task_plan should include a gated saved-browser run step with permission blockers and real-world approval text: ${JSON.stringify(blockedSavedBrowserPlan)}`,
  );
  const appQaPlan = await callTool("mcp_task_plan", {
    intent: "app QA",
    project_path: repoRoot,
  });
  assert(
      appQaPlan.version === 1 &&
      appQaPlan.normalized_intent === "app_qa" &&
      appQaPlan.recommended_profile_kind === "project-dev" &&
      appQaPlan.steps.some(
        (step) =>
          step.id === "template_project_profile" &&
          step.ready_to_call === true &&
          !step.permission_blockers?.length,
      ) &&
      appQaPlan.steps.some(
        (step) =>
          step.id === "dry_run_save_project_profile" &&
          !step.ready_to_call &&
          step.depends_on?.includes("template_project_profile") &&
          step.required_input?.some((input) => /WorkspaceProfile/.test(input)),
      ) &&
      appQaPlan.steps.some(
        (step) =>
          step.id === "run_project_profile_after_save" &&
          !step.ready_to_call &&
          step.depends_on?.includes("save_project_profile_after_review") &&
          !step.permission_blockers?.length,
      ) &&
      appQaPlan.steps.some(
        (step) =>
          step.id === "observe_project_workspace" &&
          !step.ready_to_call &&
          step.read_only === true &&
          step.depends_on?.includes("run_project_profile_after_save"),
      ) &&
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
          step.tool === "workspace_read_app_log" &&
          step.required_input?.some((input) => /app_id/i.test(input)),
      ) &&
      appQaPlan.steps.some(
        (step) =>
          step.id === "capture_project_window_after_start" &&
          !step.ready_to_call &&
          step.required_input?.some((input) => /active_window|app_id/i.test(input)),
      ),
    `mcp_task_plan should produce an end-to-end project-dev app QA plan that the app allowlist does not block: ${JSON.stringify(appQaPlan)}`,
  );
  assert(
    appQaPlan.task_context?.action_boundaries?.some(
      (boundary) =>
        boundary.id === "start_or_attach_project_workspace" &&
        boundary.action_type === "hidden_workspace_start" &&
        boundary.approval_required === true &&
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
          boundary.approval_required === true &&
          boundary.approval_kind === "project_file_write",
      ),
    `mcp_task_plan should expose app-QA action boundaries: ${JSON.stringify(appQaPlan)}`,
  );
  assert(
    appQaPlan.approval_summary?.next_boundary?.kind === "required_input" &&
      appQaPlan.approval_summary?.next_boundary?.step_id === "dry_run_save_project_profile" &&
      appQaPlan.approval_summary?.next_boundary?.blocks_step === true,
    `mcp_task_plan should summarize the profile preview as the next app-QA boundary when the app allowlist does not block project-dev: ${JSON.stringify(appQaPlan)}`,
  );
  const groceryPlan = await callTool("mcp_task_plan", { intent: "grocery shopping" });
  assert(
    groceryPlan.viewer_available === false &&
      /--headless/.test(String(groceryPlan.viewer_unavailable_reason || "")),
    `mcp_task_plan should explain viewer unavailability in --headless mode: ${JSON.stringify(groceryPlan)}`,
  );
  assert(
    groceryPlan.normalized_intent === "browser_task" &&
      groceryPlan.task_context?.task_kind === "browser_task" &&
      groceryPlan.task_context?.missing_inputs?.some((input) => input.name === "browser_user_data") &&
      groceryPlan.task_context?.missing_inputs?.some((input) => input.name === "target_url") &&
      groceryPlan.task_context?.missing_inputs?.some((input) => input.name === "shopping_list") &&
      groceryPlan.task_context?.approval_kinds?.includes("required_input") &&
      groceryPlan.recommended_profile_kind === "browser-session" &&
      groceryPlan.needs_user_input.some((need) => /browser user-data/i.test(need)) &&
      groceryPlan.needs_user_input.some((need) => /target_url/.test(need)) &&
      groceryPlan.needs_user_input.some((need) => /shopping_list/.test(need)) &&
      groceryPlan.needs_user_input.some((need) => /substitution_policy/.test(need)) &&
      groceryPlan.steps.some(
        (step) =>
          step.id === "template_browser_session" &&
          !step.ready_to_call &&
          step.required_input?.some((input) => /user_data_dir/.test(input)),
      ) &&
      groceryPlan.approval_checkpoints?.some(
        (checkpoint) =>
          checkpoint.step_id === "template_browser_session" &&
          checkpoint.kind === "required_input" &&
          checkpoint.blocks_step === true &&
          checkpoint.required_input?.some((input) => /user_data_dir/.test(input)),
      ) &&
      !groceryPlan.approval_checkpoints?.some((checkpoint) => checkpoint.kind === "host_visible_ui"),
    `mcp_task_plan should require explicit browser profile input before viewer or browser run work: ${JSON.stringify(groceryPlan)}`,
  );
  assert(
    groceryPlan.approval_summary?.next_boundary?.kind === "required_input" &&
      groceryPlan.approval_summary?.next_boundary?.step_id === "template_browser_session" &&
      groceryPlan.approval_summary?.next_boundary?.required_input?.some((input) => /user_data_dir/.test(input)),
    `mcp_task_plan should summarize the next browser required input for host UI: ${JSON.stringify(groceryPlan)}`,
  );
  const groceryBlockedPlan = await callTool("mcp_task_plan", {
    intent: "grocery shopping",
    user_data_dir: tempDir,
  });
  assert(
    groceryBlockedPlan.viewer_available === false &&
      /--headless/.test(String(groceryBlockedPlan.viewer_unavailable_reason || "")),
    `mcp_task_plan should preserve headless viewer unavailability with task inputs: ${JSON.stringify(groceryBlockedPlan)}`,
  );
  assert(
    groceryBlockedPlan.normalized_intent === "browser_task" &&
      groceryBlockedPlan.task_context?.provided_inputs?.some((input) => input.name === "user_data_dir") &&
      groceryBlockedPlan.task_context?.missing_inputs?.some((input) => input.name === "target_url") &&
      groceryBlockedPlan.task_context?.approval_kinds?.includes("permission_ceiling") &&
      groceryBlockedPlan.task_context?.approval_kinds?.includes("hidden_workspace") &&
      groceryBlockedPlan.task_context?.approval_kinds?.includes("cart_mutation") &&
      groceryBlockedPlan.task_context?.approval_kinds?.includes("real_world_action") &&
      groceryBlockedPlan.task_context?.action_boundaries?.some(
        (boundary) =>
          boundary.id === "draft_cart_changes" &&
          boundary.action_type === "cart_mutation" &&
          boundary.approval_required === true &&
          boundary.approval_kind === "cart_mutation" &&
          boundary.approved === false &&
          boundary.missing_approvals?.includes("explicit_cart_mutation_approval") &&
          boundary.required_inputs?.includes("shopping_list"),
      ) &&
      groceryBlockedPlan.task_context?.action_boundaries?.some(
        (boundary) =>
          boundary.id === "checkout_order_or_account_change" &&
          boundary.action_type === "real_world_action" &&
          boundary.approval_required === true &&
          boundary.approval_kind === "real_world_action" &&
          boundary.approved === false &&
          boundary.missing_approvals?.includes("explicit_checkout_approval"),
      ) &&
      groceryBlockedPlan.steps.some(
        (step) =>
          step.id === "template_browser_session" &&
          !step.ready_to_call &&
          step.permission_blockers?.some((blocker) => /permission ceiling/.test(blocker)),
      ) &&
      groceryBlockedPlan.steps.some(
        (step) =>
          step.id === "run_browser_session_after_save" &&
          !step.ready_to_call &&
          step.depends_on?.includes("save_browser_profile_after_review") &&
          step.permission_blockers?.some((blocker) => /permission ceiling/.test(blocker)) &&
          step.required_input?.some((input) => /target_url/.test(input)) &&
          step.required_input?.some((input) => /shopping_list/.test(input)) &&
          step.required_input?.some((input) => /checkout|purchases|account changes/i.test(input)),
      ) &&
      groceryBlockedPlan.steps.some(
        (step) =>
          step.id === "read_browser_events_after_start" &&
          !step.ready_to_call &&
          step.read_only === true &&
          step.depends_on?.includes("observe_browser_workspace"),
      ) &&
      groceryBlockedPlan.steps.some(
        (step) =>
          step.id === "capture_browser_window_after_start" &&
          !step.ready_to_call &&
          step.required_input?.some((input) => /active_window|app_id/i.test(input)),
      ) &&
      groceryBlockedPlan.steps.some(
        (step) =>
          step.id === "confirm_real_world_boundary_after_start" &&
          !step.ready_to_call &&
          step.required_input?.some((input) => /fulfillment/.test(input)) &&
          step.required_input?.some((input) => /substitution_policy/.test(input)) &&
          step.required_input?.some((input) => /checkout|account changes/i.test(input)),
      ) &&
      groceryBlockedPlan.approval_checkpoints?.some(
        (checkpoint) =>
          checkpoint.kind === "permission_ceiling" &&
          checkpoint.permission_blockers?.some((blocker) => /permission ceiling/.test(blocker)),
      ) &&
      groceryBlockedPlan.approval_checkpoints?.some(
        (checkpoint) =>
          checkpoint.step_id === "run_browser_session_after_save" &&
          checkpoint.kind === "hidden_workspace" &&
          checkpoint.approval_required === true,
      ) &&
      groceryBlockedPlan.approval_checkpoints?.some(
        (checkpoint) =>
          checkpoint.step_id === "run_browser_session_after_save" &&
          checkpoint.kind === "real_world_action" &&
          checkpoint.approval_required === true,
      ),
    `mcp_task_plan should preflight browser-session against the active ceiling and expose structured approval checkpoints: ${JSON.stringify(groceryBlockedPlan)}`,
  );
  const cleanupPlan = await callTool("mcp_task_plan", { intent: "cleanup stopped workspaces" });
  assert(
    cleanupPlan.normalized_intent === "cleanup" &&
      cleanupPlan.steps.some(
        (step) =>
          step.id === "cleanup_after_approval" &&
          !step.ready_to_call &&
          step.destructive === true &&
          step.depends_on?.includes("preview_cleanup") &&
          step.required_input?.some((input) => /dry-run result/i.test(input)),
      ) &&
      cleanupPlan.steps.some(
        (step) =>
          step.id === "verify_cleanup" &&
          !step.ready_to_call &&
          step.read_only === true &&
          step.depends_on?.includes("cleanup_after_approval"),
      ),
    `mcp_task_plan should make cleanup an approval-gated sequence with verification: ${JSON.stringify(cleanupPlan)}`,
  );

  const initialControl = await callTool("mcp_control_state");
  assert(
    initialControl.ok === true && initialControl.status?.state?.mode === "active",
    `initial MCP control state should be active: ${JSON.stringify(initialControl)}`,
  );
  const readOnlyControl = await callTool("mcp_control_update", {
    mode: "read_only",
    reason: "mcp smoke verifies live read-only boundary",
  });
  assert(
    readOnlyControl.ok === true && readOnlyControl.status?.state?.mode === "read_only",
    `mcp_control_update did not switch to read_only: ${JSON.stringify(readOnlyControl)}`,
  );
  const readOnlyBrief = await callTool("mcp_session_brief");
  assert(
    readOnlyBrief.control?.mode === "read_only" &&
      readOnlyBrief.control?.updated_by === "mcp_control_update" &&
      /read-only boundary/.test(readOnlyBrief.control?.reason || "") &&
      readOnlyBrief.recommendations.some(
        (action) =>
          action.id === "respect_live_control" &&
          action.blocked_by_live_control === false &&
          action.action_type === "read_only" &&
          /confirmed_user_request=true/.test(`${action.reason || ""} ${action.approval_hint || ""}`),
      ),
    `mcp_session_brief should orient agents around read-only live control: ${JSON.stringify(readOnlyBrief)}`,
  );
  const readOnlyCleanupPlan = await callTool("mcp_task_plan", { intent: "cleanup" });
  assert(
    readOnlyCleanupPlan.assumptions?.some(
      (assumption) => /not active/i.test(assumption) && /confirmed_user_request=true/.test(assumption),
    ) &&
      readOnlyCleanupPlan.approval_checkpoints?.some(
        (checkpoint) =>
          checkpoint.kind === "live_control" &&
          checkpoint.step_id === "cleanup_after_approval" &&
          checkpoint.blocks_step === true &&
          checkpoint.required_input?.some((input) => /confirmed_user_request=true/.test(input)),
      ),
    `mcp_task_plan should expose live-control blockers while read-only: ${JSON.stringify(readOnlyCleanupPlan)}`,
  );
  const readOnlyStartPreview = await callTool("workspace_start", {
    id: `read-only-preview-${process.pid}`,
    dry_run: true,
    purpose: "read-only preview remains available",
  });
  assert(
    readOnlyStartPreview.ok === true && readOnlyStartPreview.start_preview,
    `workspace_start dry-run preview should remain available while read-only: ${JSON.stringify(readOnlyStartPreview)}`,
  );
  const readOnlyProfileExport = await callTool("profile_export", { id: "saved-too-open" });
  assert(
    readOnlyProfileExport.ok === true && readOnlyProfileExport.export?.profile?.id === "saved-too-open",
    `profile_export without output_path should remain read-only: ${JSON.stringify(readOnlyProfileExport)}`,
  );
  const readOnlyExportPath = path.join(tempDir, "blocked-read-only-profile-export.json");
  const readOnlyExportDenied = await callTool("profile_export", {
    id: "saved-too-open",
    output_path: readOnlyExportPath,
  });
  assert(
    readOnlyExportDenied.ok === false &&
      /read-only/.test(String(readOnlyExportDenied.message || "")) &&
      !fs.existsSync(readOnlyExportPath),
    `profile_export output_path should be blocked while read-only and avoid writing host files: ${JSON.stringify(readOnlyExportDenied)}`,
  );
  const blockedStart = await callTool("workspace_start", {
    id: `blocked-read-only-${process.pid}`,
    acknowledge_hidden_workspace: true,
  });
  assert(
    blockedStart.ok === false &&
      /read-only/.test(String(blockedStart.message || "")) &&
      /confirmed_user_request=true/.test(String(blockedStart.message || "")),
    `workspace_start should be blocked while MCP control is read_only: ${JSON.stringify(blockedStart)}`,
  );
  const pausedControl = await callTool("mcp_control_update", {
    mode: "paused",
    reason: "mcp smoke verifies paused boundary",
  });
  assert(
    pausedControl.ok === true && pausedControl.status?.state?.mode === "paused",
    `mcp_control_update did not switch to paused: ${JSON.stringify(pausedControl)}`,
  );
  const pausedStartPreview = await callTool("workspace_start", {
    id: `paused-preview-${process.pid}`,
    dry_run: true,
    purpose: "paused preview remains available",
  });
  assert(
    pausedStartPreview.ok === true && pausedStartPreview.start_preview,
    `workspace_start dry-run preview should remain available while paused: ${JSON.stringify(pausedStartPreview)}`,
  );
  const pausedBlockedStart = await callTool("workspace_start", {
    id: `blocked-paused-${process.pid}`,
    acknowledge_hidden_workspace: true,
  });
  assert(
    pausedBlockedStart.ok === false &&
      /paused/.test(String(pausedBlockedStart.message || "")) &&
      /confirmed_user_request=true/.test(String(pausedBlockedStart.message || "")),
    `workspace_start should be blocked while MCP control is paused: ${JSON.stringify(pausedBlockedStart)}`,
  );
  const deniedReactivation = await callTool("mcp_control_update", { mode: "active" });
  assert(
    deniedReactivation.ok === false &&
      /confirmed_user_request=true/.test(String(deniedReactivation.message || "")),
    `mcp_control_update should require explicit user confirmation before reactivating from paused/read-only: ${JSON.stringify(deniedReactivation)}`,
  );
  const activeControl = await callTool("mcp_control_update", {
    mode: "active",
    confirmed_user_request: true,
    reason: "mcp smoke restores active after explicit confirmation",
  });
  assert(
    activeControl.ok === true && activeControl.status?.state?.mode === "active",
    `mcp_control_update did not restore active mode: ${JSON.stringify(activeControl)}`,
  );

  const permissions = await callTool("mcp_permissions");
  assert(permissions.configured === true, "mcp_permissions did not report configured=true");
  assert(permissions.restricted === true, "mcp_permissions did not report restricted=true");
  assert(
    JSON.stringify(permissions.ceiling?.apps?.allow) === JSON.stringify(["sh"]) &&
      permissions.ceiling?.network === undefined &&
      permissions.ceiling?.mounts === undefined,
    `mcp_permissions did not report the app allowlist ceiling: ${JSON.stringify(permissions.ceiling)}`,
  );

  const disallowedApp = await callTool("profile_put", {
    dry_run: true,
    profile: {
      id: "bad-app",
      setup_commands: [],
      startup_apps: [{ command: ["bash", "-lc", "true"] }],
    },
  });
  assert(disallowedApp.ok === false, "profile_put should reject app outside allowlist");
  assert(
    String(disallowedApp.message || "").includes("not allowed by the MCP app allowlist"),
    "profile_put app rejection did not mention the app allowlist",
  );

  const allowed = await callTool("profile_put", {
    dry_run: false,
    profile: {
      id: "allowed",
      setup_commands: [],
      startup_apps: [{ command: ["sh", "-lc", "true"] }],
    },
  });
  assert(allowed.ok === true, "profile_put should allow narrowed profile");
  assert(allowed.saved === true && allowed.created === true, "profile_put save shape changed");

  const workspaceId = `mcp-smoke-${process.pid}`;
  const started = await callTool(
    "workspace_start",
    {
      id: workspaceId,
      profile: "allowed",
      acknowledge_hidden_workspace: true,
      purpose: "MCP compact response smoke",
    },
    15000,
  );
  assert(started.ok === true, "workspace_start should create the smoke workspace");
  smokeDaemonPid = started.status?.daemon_pid || null;

  try {
    const run = await callTool(
      "workspace_run_app",
      {
        id: workspaceId,
        name: "mcp-compact-probe",
        profile: "allowed",
        command: ["sh", "-lc", "echo mcp-run-ok"],
        timeout_ms: 5000,
        tail_bytes: 2000,
      },
      15000,
    );
    assert(
      run.ok === true && run.run?.succeeded === true,
      `workspace_run_app did not succeed: ${JSON.stringify(run)}`,
    );
    assert(
      run.run?.stdout?.content?.includes("mcp-run-ok"),
      "workspace_run_app stdout did not include expected output",
    );
    assert(
      Array.isArray(run.run?.launch?.apps) && run.run.launch.apps.length === 1,
      "workspace_run_app launch did not include the affected app",
    );
    assert(
      Array.isArray(run.run?.wait?.apps) && run.run.wait.apps.length === 1,
      "workspace_run_app wait did not include the affected app",
    );
    assert(
      Array.isArray(run.run?.launch?.status?.apps) && run.run.launch.status.apps.length === 0,
      "workspace_run_app launch status should not include full app history",
    );
    assert(
      Array.isArray(run.run?.wait?.status?.apps) && run.run.wait.status.apps.length === 0,
      "workspace_run_app wait status should not include full app history",
    );

    const status = await callTool("workspace_status", { id: workspaceId }, 5000);
    assert(
      Array.isArray(status.apps) && status.apps.length >= 1,
      "workspace_status should still expose full app history",
    );
    assert(
      Array.isArray(status.status?.apps) && status.status.apps.length >= 1,
      "workspace_status status should still expose full app history",
    );
    const liveBrief = await callTool("mcp_session_brief", {}, 5000);
    const liveActivity = liveBrief.workspaces?.activity?.find((entry) => entry.id === workspaceId);
    assert(
      liveActivity?.running === true &&
        liveActivity.profile_id === "allowed" &&
        liveActivity.inferred_intent === "app QA" &&
        liveActivity.app_count >= 1 &&
        liveActivity.apps?.some((app) => app.label === "mcp-compact-probe"),
      `mcp_session_brief should summarize live workspace app activity: ${JSON.stringify(liveBrief.workspaces)}`,
    );
    assert(
      liveBrief.recommendations?.some(
        (action) =>
          action.id === "plan_running_workspace_task" &&
          action.tool === "mcp_task_plan" &&
          action.arguments?.workspace_id === workspaceId &&
          action.arguments?.profile_id === "allowed" &&
          action.arguments?.intent === "app QA" &&
          action.action_type === "read_only",
      ),
      `mcp_session_brief should derive a read-only task plan from live workspace activity: ${JSON.stringify(liveBrief.recommendations)}`,
    );
    const runningAppQaPlan = await callTool("mcp_task_plan", {
      intent: "app QA",
      workspace_id: workspaceId,
      profile_id: "allowed",
    });
    assert(
      runningAppQaPlan.normalized_intent === "app_qa" &&
        runningAppQaPlan.assumptions?.some((assumption) => /already running/i.test(assumption)) &&
        !runningAppQaPlan.steps?.some((step) => step.tool === "workspace_open_profile") &&
        runningAppQaPlan.steps?.some(
          (step) =>
            step.id === "observe_running_project_workspace" &&
            step.ready_to_call === true &&
            step.read_only === true,
        ) &&
        runningAppQaPlan.steps?.some(
          (step) =>
            step.id === "list_running_project_apps" &&
            step.ready_to_call === true &&
            step.depends_on?.includes("observe_running_project_workspace"),
        ) &&
        runningAppQaPlan.steps?.some(
          (step) =>
            step.id === "read_recent_project_events" &&
            step.ready_to_call === true &&
            step.read_only === true &&
            step.depends_on?.includes("observe_running_project_workspace"),
        ) &&
        runningAppQaPlan.steps?.some(
          (step) =>
            step.id === "read_project_app_log_after_app_id" &&
            step.ready_to_call === false &&
            step.tool === "workspace_read_app_log" &&
            step.required_input?.some((input) => /app_id/i.test(input)),
        ) &&
        runningAppQaPlan.steps?.some(
          (step) =>
            step.id === "capture_active_project_window" &&
            step.ready_to_call === false &&
            step.required_input?.some((input) => /active_window|app_id/i.test(input)),
        ) &&
        !runningAppQaPlan.steps?.some((step) => step.id === "open_viewer_for_running_project"),
      `mcp_task_plan should continue an already-running app QA workspace without starting another profile in --headless mode: ${JSON.stringify(runningAppQaPlan)}`,
    );
  } finally {
    if (stopViaMcp) {
      const stopped = await callTool("workspace_stop", { id: workspaceId }, 15000);
      assert(
        stopped.ok === true && stopped.status?.ready === false,
        `workspace_stop did not stop the smoke workspace: ${JSON.stringify(stopped)}`,
      );
      const stoppedBrief = await callTool("mcp_session_brief", {}, 5000);
      const stoppedActivity = stoppedBrief.workspaces?.activity?.find((entry) => entry.id === workspaceId);
      assert(
        stoppedActivity?.running === false &&
          stoppedActivity.app_count >= 1 &&
          !stoppedActivity.error,
        `mcp_session_brief should summarize stopped manifest activity without daemon-connect noise: ${JSON.stringify(stoppedBrief.workspaces)}`,
      );
      const cleanupPreview = await callTool(
        "workspace_cleanup_stale",
        { id: workspaceId, dry_run: true },
        15000,
      );
      assert(
        Array.isArray(cleanupPreview.candidates) &&
          cleanupPreview.candidates.some((candidate) => candidate.id === workspaceId),
        `workspace_cleanup_stale dry run did not find stopped smoke workspace: ${JSON.stringify(cleanupPreview)}`,
      );
      const cleanup = await callTool(
        "workspace_cleanup_stale",
        { id: workspaceId, dry_run: false },
        15000,
      );
      assert(
        Array.isArray(cleanup.removed) &&
          cleanup.removed.some((removed) => removed.id === workspaceId),
        `workspace_cleanup_stale did not remove stopped smoke workspace: ${JSON.stringify(cleanup)}`,
      );
    } else {
      childProcess.spawnSync(bin, ["workspace", "stop", "--id", workspaceId], {
        cwd: repoRoot,
        env: childEnv,
        stdio: "ignore",
        timeout: 15000,
      });
      childProcess.spawnSync(bin, ["workspace", "cleanup", "--id", workspaceId], {
        cwd: repoRoot,
        env: childEnv,
        stdio: "ignore",
        timeout: 15000,
      });
    }
  }

  await assertNoZombieChildren(child.pid);

  child.stdin.end();
  await new Promise((resolve, reject) => {
    child.once("exit", (code) => {
      if (code === 0) resolve();
      else reject(new Error(`MCP server exited with code ${code}, stderr=${stderr}`));
    });
  });
}

main()
  .then(() => {
    fs.rmSync(tempDir, { recursive: true, force: true });
    console.log("mcp permissions smoke passed");
  })
  .catch((error) => {
    try {
      child.kill("SIGTERM");
    } catch {
      // ignore cleanup races
    }
    console.error(error instanceof Error ? error.stack || error.message : String(error));
    if (stderr.trim()) {
      console.error(`MCP stderr:\n${stderr.trim()}`);
    }
    console.error(`preserved temp dir: ${tempDir}`);
    process.exit(1);
  });
