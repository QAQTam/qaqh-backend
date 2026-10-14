// smoke-wire-proxy.mjs — wire-capture-proxy 自测：起一个迷你上游，
// 经代理转发一个流式请求，验证透传与 req/res 落盘。
import http from "node:http";
import { spawn } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const REPO = fileURLToPath(new URL("..", import.meta.url));
const OUT = path.join(REPO, ".qaqh", "wire-capture");

// 迷你上游：回两个 SSE chunk。
const upstream = http.createServer((req, res) => {
  let raw = "";
  req.on("data", (c) => (raw += c));
  req.on("end", () => {
    const body = JSON.parse(raw || "{}");
    res.writeHead(200, { "content-type": body.stream ? "text/event-stream" : "application/json" });
    if (body.stream) {
      res.write(`data: {"choices":[{"delta":{"content":"hi"}}]}\n\n`);
      res.write("data: [DONE]\n\n");
    } else {
      res.write(JSON.stringify({ ok: true }));
    }
    res.end();
  });
});
await new Promise((ok) => upstream.listen(0, "127.0.0.1", ok));
const upstreamPort = upstream.address().port;

const proxy = spawn(process.execPath, [path.join(REPO, "scripts", "wire-capture-proxy.mjs"), "18787"], {
  env: { ...process.env, QAQH_WIRE_TARGET: `http://127.0.0.1:${upstreamPort}` },
  stdio: ["ignore", "pipe", "pipe"],
});
let proxyPort = null;
proxy.stdout.on("data", (d) => {
  const m = String(d).match(/http:\/\/127\.0\.0\.1:(\d+)/);
  if (m && !proxyPort) proxyPort = Number(m[1]);
});
await new Promise((r) => setTimeout(r, 800));
if (!proxyPort) {
  console.error("FAIL: 代理未启动");
  process.exit(1);
}

// ① 流式请求经代理
const res = await fetch(`http://127.0.0.1:${proxyPort}/v1/chat/completions`, {
  method: "POST",
  headers: { "content-type": "application/json", authorization: "Bearer secret-do-not-log" },
  body: JSON.stringify({ model: "m", stream: true, messages: [{ role: "user", content: "hello" }] }),
});
const streamed = await res.text();
const okStream = res.status === 200 && streamed.includes('"hi"') && streamed.includes("[DONE]");

// ② 非流式
const res2 = await fetch(`http://127.0.0.1:${proxyPort}/v1/chat/completions`, {
  method: "POST",
  headers: { "content-type": "application/json" },
  body: JSON.stringify({ model: "m", stream: false, messages: [] }),
});
const okJson = res2.status === 200 && (await res2.json()).ok === true;

await new Promise((r) => setTimeout(r, 300));
proxy.kill();
upstream.close();

const req1 = JSON.parse(fs.readFileSync(path.join(OUT, "req-001.json"), "utf8"));
const noAuth = !JSON.stringify(req1.headers).includes("secret-do-not-log");
const res1txt = fs.readFileSync(path.join(OUT, "res-001.txt"), "utf8");
const fullBody = req1.body?.messages?.[0]?.content === "hello" && req1.body.stream === true;

console.log(`流式透传: ${okStream ? "✓" : "✗"} ${JSON.stringify(streamed.slice(0, 80))}`);
console.log(`非流式透传: ${okJson ? "✓" : "✗"}`);
console.log(`req JSON 含完整 body: ${fullBody ? "✓" : "✗"}`);
console.log(`authorization 未落盘: ${noAuth ? "✓" : "✗"}`);
console.log(`res tee 内容: ${res1txt.includes("[DONE]") ? "✓" : "✗"}`);
process.exitCode = okStream && okJson && fullBody && noAuth && res1txt.includes("[DONE]") ? 0 : 1;
