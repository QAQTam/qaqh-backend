// wire-capture-proxy.mjs — daemon 与真实 provider 之间的 wire 级抓包代理。
//
// 每个出站请求落一份独立 JSON（含完整 body），响应另存 .txt（SSE 原文），
// 编号单调递增保证时序可对。Authorization 头转发但不落盘。
//
// 用法：
//   QAQH_WIRE_TARGET=https://api.deepseek.com node scripts/wire-capture-proxy.mjs [端口]
// 然后把 daemon 的 profile base_url 指到 http://127.0.0.1:<端口>
// （例如原来是 https://api.deepseek.com/v1 → 改成 http://127.0.0.1:8787/v1，
//   代理按原路径原样转发）。
// 产物：<repo>/.qaqh/wire-capture/req-NNN.json + res-NNN.txt + index.jsonl

import http from "node:http";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const REPO = fileURLToPath(new URL("..", import.meta.url));
const TARGET = (process.env.QAQH_WIRE_TARGET ?? "").replace(/\/+$/, "");
if (!TARGET) {
  console.error("用法: QAQH_WIRE_TARGET=https://provider.example node scripts/wire-capture-proxy.mjs [端口]");
  process.exit(1);
}
const OUT = process.env.QAQH_WIRE_OUT
  ? path.resolve(process.env.QAQH_WIRE_OUT)
  : path.join(REPO, ".qaqh", "wire-capture");
fs.mkdirSync(OUT, { recursive: true });
const INDEX = path.join(OUT, "index.jsonl");
const PORT = Number(process.argv[2] ?? 8787);

let seq = 0;
const server = http.createServer(async (req, res) => {
  const id = String(++seq).padStart(3, "0");
  const chunks = [];
  req.on("data", (c) => chunks.push(c));
  req.on("end", async () => {
    const raw = Buffer.concat(chunks);
    const at = Date.now();
    let body = null;
    try {
      body = JSON.parse(raw.toString("utf8"));
    } catch {
      body = { _raw_bytes: raw.length };
    }
    const safeHeaders = { ...req.headers };
    delete safeHeaders.authorization;
    delete safeHeaders["x-api-key"];

    const upstreamHeaders = { ...req.headers };
    delete upstreamHeaders.host;
    delete upstreamHeaders.connection;
    delete upstreamHeaders["content-length"];

    const url = TARGET + req.url;
    let upstream;
    try {
      upstream = await fetch(url, {
        method: req.method,
        headers: upstreamHeaders,
        body: ["GET", "HEAD"].includes(req.method) ? undefined : raw,
      });
    } catch (error) {
      const detail = `proxy upstream error: ${error?.cause?.code ?? error?.message ?? error}`;
      fs.appendFileSync(
        path.join(OUT, `req-${id}.json`),
        JSON.stringify({ id, at, url, method: req.method, headers: safeHeaders, body, error: detail }, null, 2),
      );
      fs.appendFileSync(INDEX, JSON.stringify({ id, at, url, error: detail }) + "\n");
      res.writeHead(502, { "content-type": "application/json" });
      res.end(JSON.stringify({ error: { message: detail } }));
      return;
    }

    // 请求体落盘（独立 JSON，含完整 messages/tools/参数）。
    fs.writeFileSync(
      path.join(OUT, `req-${id}.json`),
      JSON.stringify(
        {
          id,
          at,
          url,
          method: req.method,
          upstream_status: upstream.status,
          model: body?.model ?? null,
          stream: body?.stream ?? null,
          n_messages: Array.isArray(body?.messages) ? body.messages.length : null,
          headers: safeHeaders,
          body,
        },
        null,
        2,
      ),
    );

    // 响应透传 + tee 落盘。
    const resHeaders = {};
    upstream.headers.forEach((v, k) => {
      if (!["content-encoding", "content-length", "transfer-encoding"].includes(k)) resHeaders[k] = v;
    });
    res.writeHead(upstream.status, resHeaders);
    const resFile = fs.createWriteStream(path.join(OUT, `res-${id}.txt`));
    const reader = upstream.body.getReader();
    try {
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        if (!res.destroyed) res.write(Buffer.from(value));
        resFile.write(Buffer.from(value));
      }
    } catch {
      /* 客户端断开（如 daemon 取消流）：已收到的部分保留在 res-*.txt */
    }
    resFile.end();
    try {
      res.end();
    } catch {
      /* ignore */
    }
    fs.appendFileSync(
      INDEX,
      JSON.stringify({
        id,
        at,
        url,
        status: upstream.status,
        stream: body?.stream ?? null,
        n_messages: Array.isArray(body?.messages) ? body.messages.length : null,
        model: body?.model ?? null,
      }) + "\n",
    );
    console.log(`[#${id}] ${req.method} ${req.url} → ${upstream.status} (${body?.messages?.length ?? "?"} msgs)`);
  });
});

server.listen(PORT, "127.0.0.1", () => {
  const bound = server.address().port;
  console.log(`wire capture proxy: http://127.0.0.1:${bound} → ${TARGET}`);
  console.log(`抓包目录: ${OUT}（req-NNN.json / res-NNN.txt / index.jsonl）`);
});
