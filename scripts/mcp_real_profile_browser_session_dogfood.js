#!/usr/bin/env node
"use strict";

const childProcess = require("node:child_process");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

const repoRoot = path.resolve(__dirname, "..");
const defaultBin = path.join(repoRoot, "target", "debug", "agent-workspace-linux");

const failureModes = [
  "missing explicit --approved-user-data-dir approval",
  "approved path is not a Chrome/Chromium user-data directory",
  "approved path has Singleton* or DevToolsActivePort lock markers from a running browser",
  "approved path has invalid Local State JSON",
  "Chrome/Chromium binary or built agent-workspace-linux binary is missing",
  "profile copy fails or the copied profile cannot be mounted",
  "workspace/browser startup fails because Xvfb, window manager, bubblewrap, or Chrome fails",
  "workspace browser does not expose a loopback DevTools endpoint",
  "Slack/GitHub redirects to sign-in or the logged-in page heuristic does not match",
];

const siteDefaults = {
  github: {
    url: "https://github.com/notifications",
    waitMs: 2000,
    loggedOut: [/Sign in to GitHub/i, /Join GitHub/i, /Create an account/i],
    loggedIn: [/Notifications/i, /Inbox/i, /Unread/i, /Participating/i, /Watching/i],
  },
  slack: {
    url: "https://app.slack.com/client",
    waitMs: 5000,
    loggedOut: [/Sign in to Slack/i, /Continue to sign in/i, /Create a workspace/i],
    loggedIn: [/Slack/i, /Home/i, /DMs/i, /Channels/i, /Mentions/i],
  },
};

function usage() {
  console.log(`Usage:
  node scripts/mcp_real_profile_browser_session_dogfood.js --approved-user-data-dir PATH --site github|slack [options]

Options:
  --approved-user-data-dir PATH  Required. Human-approved Chrome/Chromium user-data-dir to copy.
  --site github|slack            Site heuristic to validate. Default: github.
  --url URL                      Override the default site URL.
  --expect-text REGEX            Additional logged-in proof expected in the page text/title/url.
  --browser-bin PATH             Chrome/Chromium binary. Defaults to BROWSER_BIN or PATH lookup.
  --workspace-id ID              Workspace id. Defaults to rp-<pid>.
  --copy-dir PATH                Disposable copy destination. Defaults to a temp directory.
  --keep-copy                    Preserve the disposable browser profile copy after the run.
  --keep-workspace               Leave the workspace running for manual inspection.
  --help                         Show this help.
  --self-test                    Run local preflight/copy checks without launching a browser.

Examples:
  node scripts/mcp_real_profile_browser_session_dogfood.js \\
    --approved-user-data-dir ~/.config/google-chrome \\
    --site github

  BROWSER_BIN=chromium node scripts/mcp_real_profile_browser_session_dogfood.js \\
    --approved-user-data-dir ~/.config/chromium \\
    --site slack

The helper always prefers a disposable copy of the approved profile. It refuses
obvious active-profile hazards instead of attaching to host Chrome or the host
Chrome bridge.`);
}

function parseArgs(argv) {
  const args = {
    site: "github",
    keepCopy: false,
    keepWorkspace: false,
    selfTest: false,
  };
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    const next = () => {
      if (index + 1 >= argv.length) {
        throw new Error(`${arg} requires a value`);
      }
      index += 1;
      return argv[index];
    };
    switch (arg) {
      case "--approved-user-data-dir":
        args.approvedUserDataDir = next();
        break;
      case "--site":
        args.site = next();
        break;
      case "--url":
        args.url = next();
        break;
      case "--expect-text":
        args.expectText = next();
        break;
      case "--browser-bin":
        args.browserBin = next();
        break;
      case "--workspace-id":
        args.workspaceId = next();
        break;
      case "--copy-dir":
        args.copyDir = next();
        break;
      case "--keep-copy":
        args.keepCopy = true;
        break;
      case "--keep-workspace":
        args.keepWorkspace = true;
        break;
      case "--self-test":
        args.selfTest = true;
        break;
      case "--help":
      case "-h":
        args.help = true;
        break;
      default:
        throw new Error(`unknown argument ${arg}`);
    }
  }
  return args;
}

function shellQuote(value) {
  return `'${String(value).replace(/'/g, "'\\''")}'`;
}

function expandHome(value) {
  if (value === "~") return os.homedir();
  if (value.startsWith("~/")) return path.join(os.homedir(), value.slice(2));
  return value;
}

function resolvePath(value) {
  return path.resolve(expandHome(value));
}

function findBrowser(explicit) {
  if (explicit) return resolvePath(explicit);
  if (process.env.BROWSER_BIN) return process.env.BROWSER_BIN;
  for (const candidate of ["google-chrome", "google-chrome-stable", "chromium", "chromium-browser"]) {
    const resolved = childProcess.spawnSync("sh", ["-lc", `command -v ${candidate}`], {
      encoding: "utf8",
    });
    if (resolved.status === 0 && resolved.stdout.trim()) {
      return resolved.stdout.trim();
    }
  }
  throw new Error(
    "Chrome/Chromium binary not found; set --browser-bin or BROWSER_BIN. Failure mode: missing browser binary.",
  );
}

function assert(condition, message) {
  if (!condition) throw new Error(message);
}

function looksLikeProfileSubdir(dir) {
  const basename = path.basename(dir);
  if (basename === "Default" || /^Profile \d+$/.test(basename)) {
    return fs.existsSync(path.join(path.dirname(dir), "Local State"));
  }
  return false;
}

function readJsonIfPresent(file) {
  if (!fs.existsSync(file)) return null;
  try {
    return JSON.parse(fs.readFileSync(file, "utf8"));
  } catch (error) {
    throw new Error(`obvious profile corruption hazard: ${file} is not valid JSON: ${error.message}`);
  }
}

function checkApprovedProfileSource(sourceDir) {
  const hazards = [];
  const warnings = [];
  assert(fs.existsSync(sourceDir), `approved user-data-dir does not exist: ${sourceDir}`);
  assert(fs.statSync(sourceDir).isDirectory(), `approved user-data-dir is not a directory: ${sourceDir}`);
  if (looksLikeProfileSubdir(sourceDir)) {
    throw new Error(
      `approved path looks like a profile subdirectory (${path.basename(
        sourceDir,
      )}); pass the parent Chrome user-data-dir instead: ${path.dirname(sourceDir)}`,
    );
  }

  for (const marker of ["SingletonLock", "SingletonSocket", "SingletonCookie", "DevToolsActivePort"]) {
    const markerPath = path.join(sourceDir, marker);
    if (fs.existsSync(markerPath)) {
      hazards.push(markerPath);
    }
  }

  readJsonIfPresent(path.join(sourceDir, "Local State"));
  if (!fs.existsSync(path.join(sourceDir, "Local State"))) {
    const hasProfileDir = fs
      .readdirSync(sourceDir, { withFileTypes: true })
      .some((entry) => entry.isDirectory() && (entry.name === "Default" || /^Profile \d+$/.test(entry.name)));
    if (!hasProfileDir) {
      warnings.push("path does not contain Local State or obvious Default/Profile N directories");
    }
  }

  if (hazards.length > 0) {
    throw new Error(
      [
        "approved user-data-dir appears to be active or stale-locked:",
        ...hazards.map((hazard) => `  ${hazard}`),
        "Close Chrome/Chromium and rerun, or approve an already disposable stopped copy.",
      ].join("\n"),
    );
  }

  return { warnings };
}

function shouldCopyEntry(sourceRoot, entryPath) {
  const relative = path.relative(sourceRoot, entryPath);
  if (!relative || relative === ".") return true;
  const basename = path.basename(entryPath);
  if (["SingletonLock", "SingletonSocket", "SingletonCookie", "DevToolsActivePort"].includes(basename)) {
    return false;
  }
  if (/^BrowserMetrics/.test(basename)) {
    return false;
  }
  const parts = relative.split(path.sep);
  return !parts.some((part) =>
    [
      "Cache",
      "Code Cache",
      "GPUCache",
      "GrShaderCache",
      "ShaderCache",
      "DawnCache",
      "Crashpad",
      "Safe Browsing",
      "OptimizationHints",
    ].includes(part),
  );
}

function copyProfileSource(sourceDir, destinationDir) {
  if (fs.existsSync(destinationDir) && fs.readdirSync(destinationDir).length > 0) {
    throw new Error(`copy destination already exists and is not empty: ${destinationDir}`);
  }
  fs.mkdirSync(destinationDir, { recursive: true });
  fs.cpSync(sourceDir, destinationDir, {
    recursive: true,
    dereference: false,
    errorOnExist: false,
    force: false,
    filter: (entryPath) => shouldCopyEntry(sourceDir, entryPath),
  });
  for (const marker of ["SingletonLock", "SingletonSocket", "SingletonCookie", "DevToolsActivePort"]) {
    fs.rmSync(path.join(destinationDir, marker), { force: true, recursive: true });
  }
}

function extractPageText(page) {
  return [page?.title, page?.url, page?.text].filter(Boolean).join("\n");
}

function validateLoggedInPage(site, page, expectText) {
  const defaults = siteDefaults[site] || siteDefaults.github;
  const text = extractPageText(page);
  const loggedOut = defaults.loggedOut.find((pattern) => pattern.test(text));
  if (loggedOut) {
    throw new Error(
      `${site} appears logged out or redirected to sign-in (${loggedOut}); approve a logged-in profile or refresh the disposable copy`,
    );
  }
  if (expectText) {
    const pattern = new RegExp(expectText, "i");
    if (!pattern.test(text)) {
      throw new Error(`--expect-text ${expectText} did not match title/url/text from the workspace browser`);
    }
    return { matched: `custom:${expectText}` };
  }
  const loggedIn = defaults.loggedIn.find((pattern) => pattern.test(text));
  if (!loggedIn) {
    throw new Error(
      `${site} logged-in heuristic did not match. Rerun with --expect-text for this account/page if the page is visibly logged in.`,
    );
  }
  return { matched: String(loggedIn) };
}

function redactedEvidence(page, validation, targetInfo, copyDir) {
  return {
    site_validation: validation,
    final_url: page?.url || null,
    title: page?.title || null,
    text_chars: page?.text ? page.text.length : 0,
    workspace_devtools_endpoint: targetInfo.devtools_endpoint || null,
    browser_target_count: targetInfo.targets ? targetInfo.targets.length : 0,
    disposable_copy: copyDir,
    host_bridge_used: false,
    host_bridge_note:
      "This script only calls agent-workspace-linux MCP tools and validates the workspace loopback DevTools endpoint.",
  };
}

class McpClient {
  constructor(bin, env) {
    this.child = childProcess.spawn(bin, ["mcp", "--headless"], {
      cwd: repoRoot,
      env,
      stdio: ["pipe", "pipe", "pipe"],
    });
    this.nextId = 1;
    this.stdoutBuffer = "";
    this.stderr = "";
    this.pending = new Map();
    this.child.stderr.on("data", (chunk) => {
      this.stderr += String(chunk);
    });
    this.child.stdout.on("data", (chunk) => this.onStdout(chunk));
    this.child.on("exit", (code, signal) => {
      for (const slot of this.pending.values()) {
        clearTimeout(slot.timer);
        slot.reject(
          new Error(
            `MCP server exited before ${slot.method} response, code=${code}, signal=${signal}, stderr=${this.stderr}`,
          ),
        );
      }
      this.pending.clear();
    });
  }

  onStdout(chunk) {
    this.stdoutBuffer += String(chunk);
    for (;;) {
      const newlineIndex = this.stdoutBuffer.indexOf("\n");
      if (newlineIndex === -1) return;
      const line = this.stdoutBuffer.slice(0, newlineIndex).trim();
      this.stdoutBuffer = this.stdoutBuffer.slice(newlineIndex + 1);
      if (!line) continue;
      let message;
      try {
        message = JSON.parse(line);
      } catch {
        throw new Error(`invalid JSON-RPC line from MCP server: ${line}`);
      }
      const slot = this.pending.get(message.id);
      if (!slot) continue;
      this.pending.delete(message.id);
      clearTimeout(slot.timer);
      if (message.error) {
        slot.reject(new Error(JSON.stringify(message.error)));
      } else {
        slot.resolve(message.result);
      }
    }
  }

  request(method, params, timeoutMs = 5000, label = method) {
    const id = this.nextId;
    this.nextId += 1;
    this.child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id, method, params })}\n`);
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`timed out waiting for ${label}`));
      }, timeoutMs);
      this.pending.set(id, { resolve, reject, timer, method: label });
    });
  }

  notify(method, params) {
    this.child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", method, params })}\n`);
  }

  async initialize() {
    const result = await this.request("initialize", {
      protocolVersion: "2024-11-05",
      capabilities: {},
      clientInfo: { name: "agent-workspace-linux-real-profile-dogfood", version: "0" },
    });
    this.notify("notifications/initialized", {});
    return result;
  }

  async callTool(name, args, timeoutMs) {
    const result = await this.request(
      "tools/call",
      { name, arguments: args || {} },
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

  stop() {
    try {
      this.child.kill("SIGTERM");
    } catch {
      // ignore cleanup races
    }
  }
}

async function runSelfTest() {
  const tempDir = fs.mkdtempSync(path.join(os.tmpdir(), "awl-real-profile-self-test-"));
  try {
    const source = path.join(tempDir, "chrome");
    fs.mkdirSync(path.join(source, "Default", "Cache"), { recursive: true });
    fs.writeFileSync(path.join(source, "Local State"), "{}\n");
    fs.writeFileSync(path.join(source, "Default", "Cookies"), "cookie marker");
    fs.writeFileSync(path.join(source, "Default", "Cache", "ignored"), "cache");
    const preflight = checkApprovedProfileSource(source);
    assert(preflight.warnings.length === 0, "valid fake profile should not warn");
    const copy = path.join(tempDir, "copy");
    copyProfileSource(source, copy);
    assert(fs.existsSync(path.join(copy, "Default", "Cookies")), "copy should preserve profile data");
    assert(!fs.existsSync(path.join(copy, "Default", "Cache", "ignored")), "copy should skip cache data");

    const locked = path.join(tempDir, "locked");
    fs.mkdirSync(locked, { recursive: true });
    fs.writeFileSync(path.join(locked, "Local State"), "{}\n");
    fs.writeFileSync(path.join(locked, "SingletonLock"), "host-pid");
    let refused = false;
    try {
      checkApprovedProfileSource(locked);
    } catch (error) {
      refused = /active or stale-locked/.test(error.message);
    }
    assert(refused, "locked profile should be refused");
    console.log("real-profile dogfood preflight self-test passed");
  } finally {
    fs.rmSync(tempDir, { recursive: true, force: true });
  }
}

function printCommand(args, browser) {
  const command = [
    process.env.AGENT_WORKSPACE_BIN ? `AGENT_WORKSPACE_BIN=${shellQuote(process.env.AGENT_WORKSPACE_BIN)}` : null,
    browser ? `BROWSER_BIN=${shellQuote(browser)}` : null,
    "node",
    "scripts/mcp_real_profile_browser_session_dogfood.js",
    "--approved-user-data-dir",
    shellQuote(args.approvedUserDataDir),
    "--site",
    shellQuote(args.site),
    args.url ? "--url" : null,
    args.url ? shellQuote(args.url) : null,
    args.expectText ? "--expect-text" : null,
    args.expectText ? shellQuote(args.expectText) : null,
  ].filter(Boolean);
  console.log(`reproducible command:\n  ${command.join(" ")}`);
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  if (args.help) {
    usage();
    return;
  }
  if (args.selfTest) {
    await runSelfTest();
    return;
  }
  if (!siteDefaults[args.site]) {
    throw new Error(`unknown --site ${args.site}; expected github or slack`);
  }
  if (!args.approvedUserDataDir) {
    throw new Error(
      "missing --approved-user-data-dir. The only human step is explicitly approving the Chrome/Chromium user-data-dir to copy.",
    );
  }

  args.approvedUserDataDir = resolvePath(args.approvedUserDataDir);
  const browser = findBrowser(args.browserBin);
  const bin = process.env.AGENT_WORKSPACE_BIN || defaultBin;
  if (!fs.existsSync(bin)) {
    throw new Error(`agent-workspace-linux binary not found at ${bin}; run cargo build first`);
  }
  const site = siteDefaults[args.site];
  const targetUrl = args.url || site.url;
  const workspaceId = args.workspaceId || `rp-${process.pid}`;
  const tempDir = fs.mkdtempSync(path.join(os.tmpdir(), "awl-rp-"));
  const configDir = path.join(tempDir, "config");
  const runtimeDir = path.join(tempDir, "runtime");
  const copyDir = args.copyDir ? resolvePath(args.copyDir) : path.join(tempDir, "browser-user-data-copy");
  fs.mkdirSync(configDir, { recursive: true });
  fs.mkdirSync(runtimeDir, { recursive: true });
  fs.chmodSync(runtimeDir, 0o700);

  printCommand(args, browser);
  console.log(`approved source: ${args.approvedUserDataDir}`);
  const preflight = checkApprovedProfileSource(args.approvedUserDataDir);
  for (const warning of preflight.warnings) {
    console.warn(`profile warning: ${warning}`);
  }
  console.log(`copying approved profile to disposable directory: ${copyDir}`);
  copyProfileSource(args.approvedUserDataDir, copyDir);

  const env = { ...process.env, XDG_CONFIG_HOME: configDir, XDG_RUNTIME_DIR: runtimeDir };
  const mcp = new McpClient(bin, env);
  let stopped = false;
  try {
    const initializeResult = await mcp.initialize();
    assert(
      /workspace_browser_targets/.test(String(initializeResult.instructions || "")),
      "MCP instructions did not expose workspace browser tools",
    );

    const profileId = `bs-rp-${process.pid}`;
    const template = await mcp.callTool(
      "profile_template",
      {
        kind: "browser-session",
        id: profileId,
        browser_path: browser,
        user_data_dir: copyDir,
      },
      8000,
    );
    assert(template.profile?.id === profileId, `browser-session template failed: ${JSON.stringify(template)}`);
    assert(
      template.profile?.mounts?.[0]?.host_path === copyDir &&
        template.profile?.mounts?.[0]?.workspace_path === "/workspace/browser-user-data",
      `browser-session template should mount the disposable copy: ${JSON.stringify(template.profile?.mounts)}`,
    );

    const put = await mcp.callTool("profile_put", { profile: template.profile }, 8000);
    assert(put.ok === true && put.saved === true, `profile_put failed: ${JSON.stringify(put)}`);

    const opened = await mcp.callTool(
      "workspace_open_profile",
      {
        id: workspaceId,
        profile: profileId,
        acknowledge_hidden_workspace: true,
        purpose: `${args.site} real-profile browser-session dogfood`,
        startup_wait_window: true,
        startup_window_timeout_ms: 30000,
        open_viewer: false,
        width: 1280,
        height: 900,
      },
      45000,
    );
    assert(opened.ok === true && opened.open?.ready === true, `workspace_open_profile failed: ${JSON.stringify(opened)}`);

    const startupApps = opened.open?.startup?.launched?.flatMap((entry) => entry.apps || []) || [];
    const browserApp = startupApps.find((app) => app.name === "browser-session-no-sandbox") || startupApps[0];
    assert(browserApp?.id, `browser-session startup did not return a browser app: ${JSON.stringify(opened.open?.startup)}`);

    const targets = await mcp.callTool(
      "workspace_browser_targets",
      { id: workspaceId, app_id: browserApp.id, timeout_ms: 15000 },
      20000,
    );
    assert(targets.ok === true, `workspace_browser_targets failed: ${JSON.stringify(targets)}`);
    assert(
      /^http:\/\/127\.0\.0\.1:/.test(targets.devtools_endpoint || ""),
      `workspace browser did not expose a loopback DevTools endpoint: ${JSON.stringify(targets)}`,
    );
    assert(
      !/agent-chrome-bridge/i.test(JSON.stringify(targets)),
      `target discovery unexpectedly mentioned agent-chrome-bridge: ${JSON.stringify(targets)}`,
    );
    const target =
      targets.targets?.find((candidate) => candidate.type === "page" && candidate.webSocketDebuggerUrl) ||
      targets.targets?.find((candidate) => candidate.webSocketDebuggerUrl);
    assert(target, `no browser page target found: ${JSON.stringify(targets.targets)}`);

    const navigated = await mcp.callTool(
      "workspace_browser_navigate",
      {
        id: workspaceId,
        app_id: browserApp.id,
        target_id: target.id,
        url: targetUrl,
        wait_ms: site.waitMs,
        snapshot: true,
        max_text_chars: 12000,
        timeout_ms: 30000,
      },
      40000,
    );
    assert(navigated.ok === true, `workspace_browser_navigate failed: ${JSON.stringify(navigated)}`);
    const snapshot = await mcp.callTool(
      "workspace_browser_snapshot",
      {
        id: workspaceId,
        app_id: browserApp.id,
        target_id: target.id,
        max_text_chars: 12000,
        timeout_ms: 15000,
      },
      20000,
    );
    assert(snapshot.ok === true, `workspace_browser_snapshot failed: ${JSON.stringify(snapshot)}`);
    const page = snapshot.page || navigated.page;
    const validation = validateLoggedInPage(args.site, page, args.expectText);
    const events = await mcp.callTool("workspace_events", { id: workspaceId, tail: 25 }, 8000);
    assert(
      events.events?.some((event) => event.kind === "browser_navigate"),
      `workspace event log did not record browser_navigate: ${JSON.stringify(events.events)}`,
    );
    assert(
      events.events?.some((event) => event.kind === "browser_snapshot"),
      `workspace event log did not record browser_snapshot: ${JSON.stringify(events.events)}`,
    );

    console.log("read-only logged-in page validation passed");
    console.log(JSON.stringify(redactedEvidence(page, validation, targets, copyDir), null, 2));

    if (!args.keepWorkspace) {
      const stop = await mcp.callTool("workspace_stop", { id: workspaceId }, 15000);
      assert(stop.ok === true, `workspace_stop failed: ${JSON.stringify(stop)}`);
      stopped = true;
    } else {
      console.log(`workspace preserved for inspection: ${workspaceId}`);
    }
  } finally {
    if (!stopped && !args.keepWorkspace) {
      try {
        await mcp.callTool("workspace_stop", { id: workspaceId }, 5000);
      } catch {
        // ignore cleanup races
      }
    }
    mcp.stop();
    if (!args.keepCopy && !args.copyDir) {
      fs.rmSync(tempDir, { recursive: true, force: true });
    } else {
      console.log(`preserved disposable copy/temp data: ${args.copyDir ? copyDir : tempDir}`);
    }
  }
}

main().catch((error) => {
  console.error(error && error.stack ? error.stack : error);
  console.error("known failure modes:");
  for (const mode of failureModes) {
    console.error(`- ${mode}`);
  }
  process.exit(1);
});
