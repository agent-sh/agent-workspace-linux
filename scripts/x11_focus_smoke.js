#!/usr/bin/env node
"use strict";

// Native CLI coverage. Every process and X11 request belongs to this hidden workspace.
const assert = require("node:assert/strict");
const cp = require("node:child_process");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const bin = path.resolve(process.env.AGENT_WORKSPACE_BIN || "target/debug/agent-workspace-linux");
const cacheRoot = path.join(os.homedir(), ".cache");
fs.mkdirSync(cacheRoot, { recursive: true, mode: 0o700 });
const root = fs.mkdtempSync(path.join(cacheRoot, "af-"));
const env = { ...process.env, XDG_RUNTIME_DIR: path.join(root, "r"), XDG_CONFIG_HOME: path.join(root, "c") };
fs.mkdirSync(env.XDG_RUNTIME_DIR, { recursive: true, mode: 0o700 });
fs.mkdirSync(env.XDG_CONFIG_HOME, { recursive: true });
let status;
let passed = false;
const id = "focus";
const marker = path.join(root, "received");
const childIdFile = path.join(root, "child-id");
function run(program, args, extra = {}) {
  const output = cp.execFileSync(program, args, { env: { ...env, ...extra }, encoding: "utf8", timeout: 20000 });
  fs.appendFileSync(path.join(root, "commands.jsonl"), JSON.stringify({ program, args, output }) + "\n");
  return output;
}
function cli(...args) {
  const response = JSON.parse(run(bin, ["workspace", ...args]));
  if (args[0] === "cleanup") assert.ok(Array.isArray(response.removed), JSON.stringify(response));
  else assert.equal(response.ok, true, JSON.stringify(response));
  return response;
}
function waitForFile(file, phase) {
  if (fs.existsSync(file)) return Promise.resolve();
  return new Promise((resolve, reject) => {
    const watcher = fs.watch(path.dirname(file), () => {
      if (fs.existsSync(file)) { clearTimeout(timer); watcher.close(); resolve(); }
    });
    const timer = setTimeout(() => {
      watcher.close(); reject(new Error(`${phase} timed out after 5 seconds`));
    }, 5000);
    if (fs.existsSync(file)) { clearTimeout(timer); watcher.close(); resolve(); }
  });
}
function assertNoApplicationFocus() {
  const empty = JSON.parse(run(bin, ["workspace", "active-window", "--id", id]));
  assert.equal(empty.ok, false, "empty workspace has no application focus");
  assert.equal(empty.message, "workspace active window not found");
  assert.equal(empty.active_window ?? null, null);
  assert.equal(cli("observe", "--id", id).active_window ?? null, null, "empty observation excludes WM internals");
}
async function main() {
let primaryError;
try {
  status = cli("start", "--id", id, "--ack-hidden-workspace", "--purpose", "Native focus regression smoke").status;
  const display = { DISPLAY: status.display, XAUTHORITY: status.xauthority_path };
  assertNoApplicationFocus();
  const inputApp = cli("launch", "--id", id, "--name", "input", "--",
    "wish", path.join(__dirname, "fixtures", "focus-popup.tcl"), marker, childIdFile);
  await waitForFile(childIdFile, "Popup fixture readiness");
  const windowId = run("xdotool", ["search", "--sync", "--onlyvisible", "--name", "^Focus smoke$"], display).trim().split("\n")[0];
  assert.match(windowId, /^\d+$/);
  assert.match(run("xwininfo", ["-id", windowId], display), /Override Redirect State: yes/);
  cli("focus-window", "--id", id, windowId);
  cli("type", "--id", id, "FOCUS_SMOKE_OK");
  cli("key", "--id", id, "Return");
  await waitForFile(marker, "Popup keyboard input");
  assert.equal(fs.readFileSync(marker, "utf8").trim(), "FOCUS_SMOKE_OK");

  const childId = String(Number(fs.readFileSync(childIdFile, "utf8").trim()));
  // Keep Tk from repairing focus while the native descendant contract is checked.
  const inputGroup = inputApp.apps[0].process_group_id;
  assert.ok(Number.isInteger(inputGroup) && inputGroup > 1);
  process.kill(-inputGroup, "SIGSTOP");
  try {
    run("xdotool", ["windowfocus", childId], display);
    assert.equal(run("xdotool", ["getwindowfocus", "-f"], display).trim(), childId);
    cli("focus-window", "--id", id, windowId);
    assert.equal(run("xdotool", ["getwindowfocus", "-f"], display).trim(), childId);
    // A destroyed window can remain in EWMH metadata while native focus is valid.
    run("xprop", ["-root", "-f", "_NET_ACTIVE_WINDOW", "32x", "-set", "_NET_ACTIVE_WINDOW", "0xffffff"], display);
    assert.equal(run("xdotool", ["getactivewindow"], display).trim(), "16777215");
    assert.equal(cli("active-window", "--id", id).active_window?.id, childId);
    assert.equal(cli("observe", "--id", id).active_window?.id, childId);
  } finally {
    process.kill(-inputGroup, "SIGCONT");
  }

  const managedMarker = path.join(root, "managed-received");
  const managedChildIdFile = path.join(root, "managed-child");
  const managedApp = cli("launch", "--id", id, "--name", "managed-input", "--", "wish",
    path.join(__dirname, "fixtures", "focus-popup.tcl"), managedMarker, managedChildIdFile, "managed");
  await waitForFile(managedChildIdFile, "Managed fixture readiness");
  const managedId = run("xdotool", ["search", "--sync", "--onlyvisible", "--name", "^Managed focus smoke$"], display).trim().split("\n")[0];
  cli("focus-window", "--id", id, windowId);
  cli("focus-window", "--id", id, managedId);
  cli("type", "--id", id, "MANAGED_FOCUS_OK");
  cli("key", "--id", id, "Return");
  await waitForFile(managedMarker, "Managed keyboard input");
  assert.equal(fs.readFileSync(managedMarker, "utf8").trim(), "MANAGED_FOCUS_OK");
  cli("kill-app", "--id", id, inputApp.apps[0].id);
  cli("kill-app", "--id", id, managedApp.apps.find(app => app.name === "managed-input").id);
  assertNoApplicationFocus();
  passed = true;
 } catch (error) {
  primaryError = error;
 } finally {
  const cleanupErrors = [];
  if (status) {
    for (const args of [["stop", "--id", id, "--timeout-ms", "10000"], ["cleanup", "--id", id]]) {
      try { cli(...args); } catch (error) { cleanupErrors.push(error); console.error(`Cleanup failed: ${error.message}`); }
    }
  }
  if (passed && cleanupErrors.length === 0) fs.rmSync(root, { recursive: true });
  else console.error(`Focus smoke evidence retained: ${root}`);
  if (cleanupErrors.length) {
    throw new AggregateError(primaryError ? [primaryError, ...cleanupErrors] : cleanupErrors, "Native focus smoke cleanup failed");
  }
 }
 if (primaryError) throw primaryError;
 console.log(JSON.stringify({ ok: true, unmanaged_keyboard_input: true, managed_keyboard_input: true, descendant_focus_preserved: true, stale_active_window: true, empty_workspace: true, last_window_closed: true, cleanup: true }));
}
main().catch((error) => { console.error(error); process.exitCode = 1; });
