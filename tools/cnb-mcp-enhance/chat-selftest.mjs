#!/usr/bin/env node
/**
 * Deterministic regression test for the CNB chat bridge pagination/drain logic.
 *
 * Starts a local mock CNB API with 250 comments and runs the real CLI against
 * it. No network write or CNB token is required.
 */
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import http from "node:http";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const SERVER = join(HERE, "server.mjs");
const REPO = "QAQ-Harness/qaqh-backend";
const COMMENTS = Array.from({ length: 250 }, (_, index) => {
  const id = String(1001 + index);
  return {
    id,
    body: `message ${id}`,
    author: { username: "AnyBuddy", nickname: "Tam", is_npc: false },
    created_at: new Date(Date.UTC(2026, 0, 1, 0, 0, index)).toISOString(),
    updated_at: new Date(Date.UTC(2026, 0, 1, 0, 0, index)).toISOString(),
  };
});

const api = http.createServer((req, res) => {
  const url = new URL(req.url, "http://127.0.0.1");
  const match = url.pathname.match(/\/(issues|pulls)\/1\/comments$/);
  if (!match) {
    res.writeHead(404, { "content-type": "application/json" });
    res.end(JSON.stringify({ error: "not found" }));
    return;
  }

  const page = Math.max(1, Number(url.searchParams.get("page") || 1));
  const pageSize = Math.max(1, Number(url.searchParams.get("page_size") || 100));
  const source = match[1] === "issues" ? [...COMMENTS].reverse() : COMMENTS;
  const data = source.slice((page - 1) * pageSize, page * pageSize);
  res.writeHead(200, {
    "content-type": "application/json",
    "x-cnb-total": String(COMMENTS.length),
  });
  res.end(JSON.stringify({
    status: 200,
    header: { "x-cnb-total": String(COMMENTS.length) },
    data,
    total: COMMENTS.length,
  }));
});

function run(args, env) {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [SERVER, ...args], {
      env,
      stdio: ["ignore", "pipe", "pipe"],
    });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk) => { stdout += chunk; });
    child.stderr.on("data", (chunk) => { stderr += chunk; });
    child.on("error", reject);
    child.on("close", (code) => resolve({ code, stdout, stderr }));
  });
}

await new Promise((resolve) => api.listen(0, "127.0.0.1", resolve));
const port = api.address().port;
const tempDir = await mkdtemp(join(tmpdir(), "cnb-chat-selftest-"));
const tokenFile = join(tempDir, "token.json");
await writeFile(tokenFile, JSON.stringify({
  access_token: "test-token",
  refresh_token: "test-refresh",
  expires_at: Math.floor(Date.now() / 1000) + 3600,
}));

const env = {
  ...process.env,
  CNB_API_ENDPOINT: `http://127.0.0.1:${port}`,
  CNB_TOKEN_FILE: tokenFile,
  CNB_REPO: REPO,
};

try {
  // P1: wait/listen must drain the earliest 100 messages, not jump to the
  // latest 100 and advance the cursor past unread messages.
  const waited = await run([
    "--chat-wait", "--pr", "1", "--after-id", "1000",
    "--timeout-ms", "0", "--json",
  ], env);
  assert.equal(waited.code, 0, waited.stderr);
  const waitResult = JSON.parse(waited.stdout);
  assert.equal(waitResult.count, 100);
  assert.equal(waitResult.comments[0].id, "1001");
  assert.equal(waitResult.comments.at(-1).id, "1100");

  // P2: PR comments are ascending; reading the latest message requires
  // paginating through all pages instead of stopping after 3 pages.
  const read = await run(["--chat-read", "--pr", "1", "--limit", "1", "--json"], env);
  assert.equal(read.code, 0, read.stderr);
  const readResult = JSON.parse(read.stdout);
  assert.equal(readResult.pages, 3);
  assert.equal(readResult.total, 250);
  assert.equal(readResult.truncated, false);
  assert.equal(readResult.comments[0].id, "1250");

  console.log("chat bridge selftest passed");
} finally {
  await new Promise((resolve) => api.close(resolve));
  await rm(tempDir, { recursive: true, force: true });
}
