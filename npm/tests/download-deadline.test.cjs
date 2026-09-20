#!/usr/bin/env node
"use strict";

// Exercises the real `download()` from npm/scripts/postinstall.js by running the
// whole unchanged script inside a vm context with a linux `process`, a fake
// `https`, and injected controllable timers. Real `fs` is used, so file writes
// and partial-file cleanup are production behaviour, not mocks.

const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const test = require("node:test");
const vm = require("node:vm");
const { EventEmitter } = require("node:events");
const { Readable } = require("node:stream");

const SCRIPT = path.join(__dirname, "..", "scripts", "postinstall.js");
const SOURCE = fs.readFileSync(SCRIPT, "utf8");
const IDLE_TIMEOUT = 60 * 1000;
const MAX_DURATION = 30 * 60 * 1000;

function makeClock() {
  let now = 0;
  let seq = 0;
  const timers = new Map();
  const nextDue = (target) => {
    let due = null;
    for (const [id, t] of timers) {
      if (t.at <= target && (due === null || t.at < timers.get(due).at)) due = id;
    }
    return due;
  };
  return {
    pending: () => timers.size,
    setTimeout: (fn, ms) => (timers.set(++seq, { fn, at: now + ms }), { id: seq }),
    clearTimeout: (h) => void (h && timers.delete(h.id)),
    advance(ms) {
      const target = now + ms;
      for (let due = nextDue(target); due !== null; due = nextDue(target)) {
        const t = timers.get(due);
        timers.delete(due);
        now = t.at;
        t.fn();
      }
      now = target;
    },
  };
}

// https mock: each queued handler receives (req, cb) for one request, so tests
// decide exactly when (or whether) headers and body arrive.
function makeHttps(handlers) {
  const requests = [];
  return {
    requests,
    get(url, opts, cb) {
      const req = Object.assign(new EventEmitter(), { url, destroyed: false });
      req.destroy = () => { req.destroyed = true; };
      requests.push(req);
      const handler = handlers.shift();
      queueMicrotask(() => { if (!req.destroyed && handler) handler(req, cb); });
      return req;
    },
  };
}

function makeResponse(statusCode, headers = {}) {
  return Object.assign(new Readable({ read() {} }), { statusCode, headers });
}

function loadDownload(httpsMock, clock, fsImpl = fs, scriptDir = path.dirname(SCRIPT), entry = "download") {
  const sandbox = {
    console: { log() {}, error() {} },
    process: { platform: "linux", arch: "x64", exit(code) { throw new Error(`installer exit ${code}`); } },
    setTimeout: clock.setTimeout,
    clearTimeout: clock.clearTimeout,
    Buffer,
    URL,
    queueMicrotask,
    __dirname: scriptDir,
    module: { exports: {} },
  };
  sandbox.exports = sandbox.module.exports;
  const fakeRequire = (id) => {
    if (id === "https") return httpsMock;
    if (id === "fs") return fsImpl;
    if (id === "../package.json") return { version: "0.3.2" };
    return require(id);
  };
  fakeRequire.main = undefined; // keeps main() from auto-running
  sandbox.require = fakeRequire;
  vm.createContext(sandbox);
  vm.runInContext(SOURCE, sandbox, { filename: SCRIPT });
  return vm.runInContext(entry, sandbox);
}

function track(promise) {
  const state = { settled: false, error: null };
  const done = (error) => Object.assign(state, { settled: true, error });
  state.done = promise.then(() => done(null), done);
  return state;
}

async function tick(n = 6) {
  for (let i = 0; i < n; i += 1) await new Promise((r) => setImmediate(r));
}

function withTmp(fn) {
  return async () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), "aw-download-deadline-"));
    let watchdog;
    try {
      await Promise.race([
        fn(path.join(dir, "asset.tmp")),
        new Promise((_, reject) => { watchdog = setTimeout(() => reject(new Error("test-owned watchdog: download remained unsettled")), 3000); }),
      ]);
    } finally {
      clearTimeout(watchdog);
      fs.rmSync(dir, { recursive: true, force: true });
    }
  };
}

const timedOut = (state) => String(state.error && state.error.message);

async function completion(state) {
  let watchdog;
  try {
    await Promise.race([
      state.done,
      new Promise((_, reject) => { watchdog = setTimeout(() => reject(new Error("download did not settle after fake timeout")), 2000); }),
    ]);
  } finally { clearTimeout(watchdog); }
}

test(
  "download resolves and clears both timeout timers on a normal 200 response",
  withTmp(async (tmpFile) => {
    const clock = makeClock();
    const httpsMock = makeHttps([
      (req, cb) => {
        const res = makeResponse(200);
        cb(res);
        res.push("binary-payload");
        res.push(null);
      },
    ]);

    await loadDownload(httpsMock, clock)("https://example.invalid/asset", tmpFile);

    assert.equal(fs.readFileSync(tmpFile, "utf8"), "binary-payload");
    assert.equal(clock.pending(), 0, "timeout timers must be cleared on success");
  })
);

test(
  "download rejects on a non-200 status and clears its timeout timers",
  withTmp(async (tmpFile) => {
    const clock = makeClock();
    const httpsMock = makeHttps([(req, cb) => cb(makeResponse(404))]);
    const download = loadDownload(httpsMock, clock);

    await assert.rejects(() => download("https://example.invalid/asset", tmpFile), /HTTP 404/);
    assert.equal(fs.existsSync(tmpFile), false);
    assert.equal(clock.pending(), 0);
  })
);

test(
  "download aborts the request when response headers are idle too long",
  withTmp(async (tmpFile) => {
    const clock = makeClock();
    const httpsMock = makeHttps([() => {}]); // headers never arrive
    const state = track(
      loadDownload(httpsMock, clock)("https://example.invalid/asset", tmpFile)
    );

    await tick();
    clock.advance(IDLE_TIMEOUT - 1);
    await tick();
    assert.equal(state.settled, false, "must not settle before the idle timeout");

    clock.advance(1);
    await completion(state);
    assert.equal(state.settled, true, "must settle once the idle timeout elapses");
    assert.match(timedOut(state), /no download progress/i);
    assert.equal(httpsMock.requests[0].destroyed, true, "request must be destroyed");
    assert.equal(fs.existsSync(tmpFile), false);
    assert.equal(clock.pending(), 0);
  })
);

test(
  "download destroys the response and removes the partial file when the body stalls",
  withTmp(async (tmpFile) => {
    const clock = makeClock();
    let response;
    const httpsMock = makeHttps([
      (req, cb) => {
        response = makeResponse(200);
        cb(response);
        response.push("partial"); // body then stalls open forever
      },
    ]);
    const state = track(
      loadDownload(httpsMock, clock)("https://example.invalid/asset", tmpFile)
    );

    await tick();
    assert.equal(state.settled, false);

    clock.advance(IDLE_TIMEOUT);
    await completion(state);
    assert.equal(state.settled, true);
    assert.match(timedOut(state), /no download progress/i);
    assert.equal(response.destroyed, true, "response must be destroyed");
    assert.equal(fs.existsSync(tmpFile), false, "partial file must be cleaned up");
    assert.equal(clock.pending(), 0);
  })
);

test(
  "redirect headers reset inactivity and the next hop gets a fresh idle window",
  withTmp(async (tmpFile) => {
    const clock = makeClock();
    let firstHop;
    const httpsMock = makeHttps([
      (req, cb) => { firstHop = cb; },
      () => {},
    ]);
    const state = track(
      loadDownload(httpsMock, clock)("https://example.invalid/asset", tmpFile)
    );

    await tick();
    clock.advance(IDLE_TIMEOUT - 1);
    firstHop(makeResponse(302, { location: "/asset-final" }));
    await tick();
    assert.equal(httpsMock.requests.length, 2, "relative redirect must be followed");
    assert.equal(httpsMock.requests[1].url, "https://example.invalid/asset-final");

    clock.advance(IDLE_TIMEOUT - 1);
    await tick();
    assert.equal(state.settled, false, "redirect headers must refresh inactivity");

    clock.advance(1);
    await completion(state);
    assert.equal(state.settled, true);
    assert.match(timedOut(state), /no download progress/i);
    assert.equal(httpsMock.requests[1].destroyed, true);
    assert.equal(clock.pending(), 0);
  })
);

test(
  "slow but progressing bodies can run well past five minutes",
  withTmp(async (tmpFile) => {
    const clock = makeClock();
    let response;
    const httpsMock = makeHttps([
      (req, cb) => {
        response = makeResponse(200);
        cb(response);
        response.push("start");
      },
    ]);
    const state = track(
      loadDownload(httpsMock, clock)("https://example.invalid/asset", tmpFile)
    );

    await tick();
    for (let i = 0; i < 10; i += 1) {
      clock.advance(IDLE_TIMEOUT - 1000);
      response.push(`chunk-${i}`);
      await tick();
      assert.equal(state.settled, false, "progress must refresh inactivity");
    }
    response.push(null);
    await completion(state);
    assert.equal(state.error, null);
    assert.equal(clock.pending(), 0);
    assert.match(fs.readFileSync(tmpFile, "utf8"), /chunk-9/);
  })
);

test(
  "the 30-minute hard ceiling still bounds a continuously progressing transfer",
  withTmp(async (tmpFile) => {
    const clock = makeClock();
    let response;
    const httpsMock = makeHttps([
      (req, cb) => {
        response = makeResponse(200);
        cb(response);
        response.push("start");
      },
    ]);
    const state = track(
      loadDownload(httpsMock, clock)("https://example.invalid/asset", tmpFile)
    );

    await tick();
    let elapsed = 0;
    while (elapsed + IDLE_TIMEOUT - 1000 < MAX_DURATION) {
      clock.advance(IDLE_TIMEOUT - 1000);
      elapsed += IDLE_TIMEOUT - 1000;
      response.push("x");
      await tick();
      assert.equal(state.settled, false, "hard cap must not fire early");
    }
    clock.advance(MAX_DURATION - elapsed);
    await completion(state);
    assert.equal(state.settled, true);
    assert.match(timedOut(state), /download exceeded/i);
    assert.equal(response.destroyed, true);
    assert.equal(fs.existsSync(tmpFile), false);
    assert.equal(clock.pending(), 0);
  })
);

test(
  "a retry after an idle timeout gets fresh timers and still succeeds",
  withTmp(async (tmpFile) => {
    const clock = makeClock();
    const httpsMock = makeHttps([
      () => {}, // attempt one stalls
      (req, cb) => {
        const res = makeResponse(200);
        cb(res);
        res.push("retried-payload");
        res.push(null);
      },
    ]);
    const download = loadDownload(httpsMock, clock);

    const first = track(download("https://example.invalid/asset", tmpFile));
    await tick();
    clock.advance(IDLE_TIMEOUT);
    await completion(first);
    assert.equal(first.settled, true);
    assert.equal(clock.pending(), 0);

    await download("https://example.invalid/asset", tmpFile);
    assert.equal(fs.readFileSync(tmpFile, "utf8"), "retried-payload");
    assert.equal(clock.pending(), 0, "retry must not leak a timer either");
  })
);


test("timeout remains the reason when destroying a request emits an immediate error", withTmp(async (tmpFile) => {
  const clock = makeClock();
  const httpsMock = makeHttps([(req) => {
    req.destroy = () => { req.destroyed = true; req.emit("error", new Error("aborted transport")); };
  }]);
  const pending = loadDownload(httpsMock, clock)("https://example.invalid/asset", tmpFile);
  const assertion = assert.rejects(pending, /no download progress/i);
  await tick();
  clock.advance(IDLE_TIMEOUT);
  await assertion;
  assert.equal(clock.pending(), 0);
}));

test("response failures close the output before rejection and leave no late partial file", withTmp(async (tmpFile) => {
  const clock = makeClock();
  let response, output;
  const fsImpl = { ...fs, createWriteStream(...args) { output = fs.createWriteStream(...args); return output; } };
  const httpsMock = makeHttps([(req, cb) => { response = makeResponse(200); cb(response); response.push("partial"); }]);
  const pending = loadDownload(httpsMock, clock, fsImpl)("https://example.invalid/asset", tmpFile);
  const assertion = assert.rejects(pending, /connection lost/);
  await tick();
  response.destroy(new Error("connection lost"));
  await assertion;
  assert.equal(output.closed, true, "output FD must be closed when the caller may retry");
  assert.equal(httpsMock.requests[0].destroyed, true);
  assert.equal(fs.existsSync(tmpFile), false);
  assert.equal(clock.pending(), 0);
}));

test("synchronous HTTPS construction failure clears both timers", withTmp(async (tmpFile) => {
  const clock = makeClock();
  const httpsMock = { get() { throw new Error("invalid request URL"); } };
  await assert.rejects(loadDownload(httpsMock, clock)("https://example.invalid/asset", tmpFile), /invalid request URL/);
  assert.equal(clock.pending(), 0);
}));

test("expiry during a pending file open closes and removes it before retry", withTmp(async (tmpFile) => {
  const clock = makeClock();
  const httpsMock = makeHttps([
    (req, cb) => { const res = makeResponse(200); cb(res); clock.advance(IDLE_TIMEOUT); },
    (req, cb) => { const res = makeResponse(200); cb(res); res.push("retry survives"); res.push(null); },
  ]);
  const download = loadDownload(httpsMock, clock);
  await assert.rejects(download("https://example.invalid/asset", tmpFile), /no download progress/i);
  assert.equal(fs.existsSync(tmpFile), false);
  await download("https://example.invalid/asset", tmpFile);
  await tick();
  assert.equal(fs.readFileSync(tmpFile, "utf8"), "retry survives");
  assert.equal(clock.pending(), 0);
}));

test("redirect limit rejection clears the timer and aborts outstanding transport", withTmp(async (tmpFile) => {
  const clock = makeClock();
  const httpsMock = makeHttps(Array.from({ length: 6 }, () => (req, cb) => {
    const res = makeResponse(302, { location: "https://example.invalid/again" });
    cb(res); res.push(null);
  }));
  await assert.rejects(loadDownload(httpsMock, clock)("https://example.invalid/asset", tmpFile), /too many redirects/i);
  assert.equal(httpsMock.requests.length, 6);
  assert.equal(clock.pending(), 0);
  assert.equal(httpsMock.requests.every((req) => req.destroyed), true);
}));

test("a late header callback after expiry cannot create a download", withTmp(async (tmpFile) => {
  const clock = makeClock();
  let headers;
  const httpsMock = makeHttps([(req, cb) => { headers = cb; }]);
  const pending = loadDownload(httpsMock, clock)("https://example.invalid/asset", tmpFile);
  const assertion = assert.rejects(pending, /no download progress/i);
  await tick(); clock.advance(IDLE_TIMEOUT); await assertion;
  const res = makeResponse(200); headers(res); await tick();
  assert.equal(res.destroyed, true);
  assert.equal(fs.existsSync(tmpFile), false);
  assert.equal(clock.pending(), 0);
}));


test("the actual installer completes verified binary publication with the idle-bounded downloader", withTmp(async (tmpFile) => {
  const clock = makeClock();
  const scriptDir = path.join(path.dirname(tmpFile), "npm", "scripts");
  fs.mkdirSync(scriptDir, { recursive: true });
  const bytes = Buffer.from("synthetic verified executable");
  const hash = require("node:crypto").createHash("sha256").update(bytes).digest("hex");
  const sidecar = `${hash}  agent-workspace-linux-x86_64-unknown-linux-gnu\n`;
  const httpsMock = makeHttps([bytes, sidecar].map((data) => (req, cb) => {
    const response = makeResponse(200); cb(response); response.push(data); response.push(null);
  }));
  await loadDownload(httpsMock, clock, fs, scriptDir, "main")();
  const dest = path.join(scriptDir, "..", "bin", "agent-workspace-linux");
  assert.deepEqual(fs.readFileSync(dest), bytes);
  assert.equal(fs.statSync(dest).mode & 0o777, 0o755);
  assert.equal(httpsMock.requests.length, 2);
  assert.equal(clock.pending(), 0);
}));

test("the actual installer still refuses mismatched checksums without replacing a prior executable", withTmp(async (tmpFile) => {
  const clock = makeClock();
  const scriptDir = path.join(path.dirname(tmpFile), "npm", "scripts");
  const binDir = path.join(scriptDir, "..", "bin");
  fs.mkdirSync(scriptDir, { recursive: true }); fs.mkdirSync(binDir);
  const dest = path.join(binDir, "agent-workspace-linux");
  fs.writeFileSync(dest, "known-good-old"); fs.chmodSync(dest, 0o755);
  const sidecar = `${"0".repeat(64)}  agent-workspace-linux-x86_64-unknown-linux-gnu\n`;
  const httpsMock = makeHttps(["wrong-bytes", sidecar].map((data) => (req, cb) => {
    const response = makeResponse(200); cb(response); response.push(data); response.push(null);
  }));
  await assert.rejects(loadDownload(httpsMock, clock, fs, scriptDir, "main")(), /installer exit 1/);
  assert.equal(fs.readFileSync(dest, "utf8"), "known-good-old");
  assert.equal(fs.statSync(dest).mode & 0o777, 0o755);
  assert.equal(clock.pending(), 0);
}));
