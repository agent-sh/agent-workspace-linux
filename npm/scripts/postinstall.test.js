#!/usr/bin/env node
"use strict";

const assert = require("node:assert/strict");
const crypto = require("node:crypto");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const test = require("node:test");

const {
  parseSha256Sidecar,
  verifyChecksum,
} = require("./postinstall");

const ASSET = "agent-workspace-linux-x86_64-unknown-linux-gnu";

function sha256(contents) {
  return crypto.createHash("sha256").update(contents).digest("hex");
}

test("parseSha256Sidecar accepts the release workflow sha256sum format", () => {
  const checksum = sha256("binary");
  assert.equal(
    parseSha256Sidecar(`${checksum}  ${ASSET}\n`, ASSET),
    checksum
  );
});

test("parseSha256Sidecar rejects a sidecar for the wrong asset", () => {
  const checksum = sha256("binary");
  assert.throws(
    () => parseSha256Sidecar(`${checksum}  other-asset\n`, ASSET),
    /does not contain an entry/
  );
});

test("verifyChecksum accepts a matching downloaded binary", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "agent-workspace-test-"));
  try {
    const binaryPath = path.join(dir, ASSET);
    const sidecarPath = `${binaryPath}.sha256`;
    const contents = Buffer.from("downloaded binary");
    const checksum = sha256(contents);

    fs.writeFileSync(binaryPath, contents);
    fs.writeFileSync(sidecarPath, `${checksum}  ${ASSET}\n`);

    await verifyChecksum(binaryPath, sidecarPath, ASSET);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test("verifyChecksum rejects a mismatched downloaded binary", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "agent-workspace-test-"));
  try {
    const binaryPath = path.join(dir, ASSET);
    const sidecarPath = `${binaryPath}.sha256`;
    const checksum = sha256("different binary");

    fs.writeFileSync(binaryPath, "downloaded binary");
    fs.writeFileSync(sidecarPath, `${checksum}  ${ASSET}\n`);

    await assert.rejects(
      () => verifyChecksum(binaryPath, sidecarPath, ASSET),
      /checksum mismatch/
    );
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
