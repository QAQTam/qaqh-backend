// probe-user-position.mjs — user 消息在模型请求中的位置实证探针。
//
// 链路全真：真实 daemon + 本地 OpenAI-compatible mock provider。
// mock 在 lap 0-3 各返回一个 `read` 工具调用（只读、全档位免审批），
// lap 4 返回文本终态；每个流式请求的完整 body 追加记录到 requests.jsonl。
//
// 判据：逐 lap 打印 daemon 实际发送的 messages 序列，并定位 user "ok"
// 消息的下标——若主链路时序正确，ok 的下标在所有 lap 中保持不变
// （紧跟 system 之后），绝不会"跳"到工具结果之后成为最后一条消息。
//
// 用法：node scripts/probe-user-position.mjs
// 产物：.qaqh/probe-userpos/{requests.jsonl,daemon.out}

import http from "node:http";
import { spawn } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const REPO = fileURLToPath(new URL("..", import.meta.url));
const OUT = path.join(REPO, ".qaqh", "probe-userpos");
const DATA = path.join(OUT, "qaqh"); // daemon 安全规则：data root 必须以 qaqh 结尾
const WORK = path.join(OUT, "work");
const NOTE = path.join(WORK, "note.txt");
const LOG = path.join(OUT, "requests.jsonl");
const MARK = "ok-probe-msg"; // 用户消息标记（避免与 system/工具结果撞词）

for (const dir of [path.join(DATA, "sessions"), WORK]) fs.mkdirSync(dir, { recursive: true });
fs.writeFileSync(NOTE, "NOTE-CONTENT-42\n");
fs.writeFileSync(LOG, "");

// ── mock chat completions provider ─────────────────────────────────────────
// wire 级抓包：每个请求的完整 body 原样落一份独立 JSON（OUT/wire/req-NNN-*.json），
// 与 requests.jsonl（分析用摘要）并行。
const WIRE_DIR = path.join(OUT, "wire");
fs.rmSync(WIRE_DIR, { recursive: true, force: true }); // 每次运行从零开始，确保干净
fs.mkdirSync(WIRE_DIR, { recursive: true });
let wireSeq = 0;
const dumpWire = (kind, payload, meta) => {
  const seq = String(wireSeq++).padStart(3, "0");
  const file = path.join(WIRE_DIR, `req-${seq}-${kind}.json`);
  fs.writeFileSync(file, JSON.stringify({ seq, kind, at: Date.now(), ...meta, body: payload }, null, 2));
  return file;
};
let mainCalls = 0;
const server = http.createServer((req, res) => {
  const chunks = [];
  req.on("data", (c) => chunks.push(c));
  req.on("end", () => {
    let payload = {};
    try {
      payload = JSON.parse(Buffer.concat(chunks).toString() || "{}");
    } catch {
      /* ignore */
    }
    const meta = { url: req.url, stream: !!payload.stream, model: payload.model ?? null, n_messages: Array.isArray(payload.messages) ? payload.messages.length : null };
    // 标题生成等非流式旁路请求：直接回 JSON，不计数（但也留痕）。
    if (!payload.stream) {
      fs.appendFileSync(LOG, JSON.stringify({ lap: "non-stream", at: Date.now(), messages: payload.messages }) + "\n");
      dumpWire("nostream", payload, meta);
      res.writeHead(200, { "content-type": "application/json" });
      res.end(JSON.stringify({ choices: [{ message: { role: "assistant", content: "title" } }] }));
      return;
    }
    const lap = mainCalls++;
    dumpWire(`lap${lap}`, payload, meta);
    fs.appendFileSync(LOG, JSON.stringify({ lap, at: Date.now(), messages: payload.messages }) + "\n");
    res.writeHead(200, {
      "content-type": "text/event-stream",
      "cache-control": "no-cache",
    });
    const sse = (obj) => res.write(`data: ${JSON.stringify(obj)}\n\n`);
    let out;
    if (lap <= 3) {
      out = [
        { choices: [{ index: 0, delta: { role: "assistant" }, finish_reason: null }] },
        // 文本 + 工具调用混合：判别 assistant content 是否被回传
        { choices: [{ index: 0, delta: { content: "ACK-TEXT-XYZ" }, finish_reason: null }] },
        {
          choices: [
            {
              index: 0,
              delta: {
                tool_calls: [
                  {
                    index: 0,
                    id: `call_r${lap}`,
                    type: "function",
                    function: { name: "read", arguments: JSON.stringify({ path: NOTE }) },
                  },
                ],
              },
              finish_reason: null,
            },
          ],
        },
        { choices: [{ index: 0, delta: {}, finish_reason: "tool_calls" }] },
      ];
    } else {
      out = [
        {
          choices: [
            { index: 0, delta: { role: "assistant", content: `all done (lap ${lap})` }, finish_reason: null },
          ],
        },
        { choices: [{ index: 0, delta: {}, finish_reason: "stop" }] },
      ];
    }
    for (const c of out) sse(c);
    res.write("data: [DONE]\n\n");
    res.end();
  });
});
await new Promise((ok) => server.listen(0, "127.0.0.1", ok));
const PORT = server.address().port;
console.log(`mock provider: http://127.0.0.1:${PORT}/v1`);

// ── daemon 配置与启动 ───────────────────────────────────────────────────────
fs.writeFileSync(
  path.join(DATA, "config.toml"),
  `provider_id = "openai"
active_profile = "default"
permission_tier = 2
api_key = "probe-dummy-key"

[profiles.default]
model = "fake-model"
max_tokens = 4096
effort = "low"
context_limit = 100000
base_url = "http://127.0.0.1:${PORT}/v1"
endpoint = "openai"
`,
);

const daemonBin = path.join(REPO, "target", "debug", process.platform === "win32" ? "qaqh-daemon.exe" : "qaqh-daemon");
const daemonEnv = {
  ...process.env,
  QAQH_DATA_DIR: DATA,
  QAQH_ALLOW_TEST_DATA_ROOT: "1",
  QAQH_REQUEST_LOG: "1", // daemon 原生 dump：每请求把传输层消息列表写入 sessions/<seed>/request-log.jsonl
  RUST_LOG: process.env.RUST_LOG ?? "info",
  NO_PROXY: "127.0.0.1,localhost",
  no_proxy: "127.0.0.1,localhost",
};
const daemon = spawn(daemonBin, ["run"], {
  cwd: REPO,
  env: daemonEnv,
  stdio: ["ignore", fs.openSync(path.join(OUT, "daemon.out"), "a"), fs.openSync(path.join(OUT, "daemon.out"), "a")],
  detached: false,
});

const stopDaemon = () => {
  try {
    daemon.kill();
  } catch {
    /* ignore */
  }
};
process.on("exit", stopDaemon);
process.on("SIGINT", () => process.exit(130));

let discovery = null;
for (let i = 0; i < 100; i++) {
  try {
    discovery = JSON.parse(fs.readFileSync(path.join(DATA, "daemon.json"), "utf8"));
    if (discovery.pid === daemon.pid) break;
  } catch {
    /* retry */
  }
  await new Promise((r) => setTimeout(r, 200));
}
if (!discovery || discovery.pid !== daemon.pid) {
  console.error("FAIL: daemon 未发布 daemon.json（详见 daemon.out）");
  process.exit(1);
}
const ENDPOINT = discovery.endpoint;
const TOKEN = discovery.token;
console.log(`daemon pid=${daemon.pid} endpoint=${ENDPOINT}`);

const request = async (method, urlPath, { session, body } = {}) => {
  const headers = { authorization: `Bearer ${TOKEN}` };
  if (session) headers["x-qaqh-client-session-id"] = session;
  if (body !== undefined) {
    headers["content-type"] = "application/json";
  }
  const res = await fetch(ENDPOINT + urlPath, {
    method,
    headers,
    body: body !== undefined ? JSON.stringify(body) : undefined,
  });
  const text = await res.text();
  let json = null;
  try {
    json = JSON.parse(text);
  } catch {
    json = { raw: text };
  }
  return { status: res.status, body: json };
};

const openClient = async (instance) => {
  const { status, body } = await request("POST", "/ringing/v2/clients/open", {
    body: { schema: "qaqh.Ringing", version: 2, client_instance_id: instance },
  });
  if (status !== 200) throw new Error(`open failed: ${status} ${JSON.stringify(body)}`);
  return body.client_session_id;
};

const command = (session, channel, commandBody, seed = undefined) =>
  request("POST", `/ringing/v2/commands/${channel}`, {
    session,
    body: {
      schema: "qaqh.Ringing",
      version: 2,
      channel,
      command_id: crypto.randomUUID(),
      client_instance_id: "probe-userpos",
      client_session_id: session,
      command: commandBody,
      ...(seed ? { session_id: seed } : {}),
    },
  });

const bootstrap = (session, seed) => request("GET", `/ringing/v2/sessions/${seed}/bootstrap`, { session });

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// ── 会话与 driver 席位 ──────────────────────────────────────────────────────
const client = await openClient("probe-userpos");
const before = new Set(fs.readdirSync(path.join(DATA, "sessions")));
const ack = await command(client, "control", {
  channel: "control",
  type: "session_create",
  close_current: false,
  cwd: WORK,
});
if (ack.status !== 200 || ack.body.status !== "accepted") {
  console.error(`FAIL: session_create ${ack.status} ${JSON.stringify(ack.body)}`);
  process.exit(1);
}
const created = new Set(fs.readdirSync(path.join(DATA, "sessions"))).difference(before);
created.delete("index.jsonl");
if (created.size !== 1) {
  console.error(`FAIL: 期望新建 1 个会话，实际 ${[...created]}`);
  process.exit(1);
}
const seed = [...created][0];
console.log(`seed=${seed}`);

const claim = await request("POST", `/ringing/v2/sessions/${seed}/driver/claim`, { session: client });
if (claim.status !== 200 || claim.body.accepted !== true) {
  console.error(`FAIL: driver claim ${claim.status} ${JSON.stringify(claim.body)}`);
  process.exit(1);
}
let ready = false;
for (let i = 0; i < 60 && !ready; i++) {
  const { body } = await bootstrap(client, seed);
  const driver = body?.control?.state?.driver;
  ready = driver?.holder === client;
  if (!ready) await sleep(250);
}
if (!ready) {
  console.error("FAIL: driver 席位未就绪");
  process.exit(1);
}
console.log("driver seat acquired");

// ── 发送 "ok"，等 5 个流式请求（lap 0-4）落地 ───────────────────────────────
const send = await command(
  client,
  "conversation",
  {
    channel: "conversation",
    type: "conversation_send_message",
    text: MARK,
    images: [],
    as_system: false,
  },
  seed,
);
if (send.status !== 200) {
  console.error(`FAIL: send message ${send.status} ${JSON.stringify(send.body)}`);
  process.exit(1);
}
console.log(`sent "${MARK}", waiting for 5 laps...`);

let done = false;
for (let i = 0; i < 360 && !done; i++) {
  if (i > 0 && i % 20 === 0) await request("POST", "/ringing/v2/leases/renew", { session: client }).catch(() => {});
  done = mainCalls >= 5;
  if (!done) await sleep(250);
}
stopDaemon();
if (!done) {
  console.error(`FAIL: 等待 5 个模型请求超时（实际 ${mainCalls}，详见 daemon.out）`);
  process.exit(1);
}
await sleep(500); // 让 daemon 把日志刷完
server.closeAllConnections?.();
server.close();

// ── 分析：每个 lap 的 messages 布局 ─────────────────────────────────────────
const laps = fs
  .readFileSync(LOG, "utf8")
  .split(/\r?\n/)
  .filter(Boolean)
  .map((line) => JSON.parse(line));

const describe = (m) => {
  const raw = m.content;
  let text = "";
  if (typeof raw === "string") text = raw;
  else if (Array.isArray(raw))
    text = raw
      .map((b) => (typeof b === "string" ? b : (b.text ?? (b.type === "tool_use" ? `[tool_use ${b.name}]` : `[${b.type}]`)) ))
      .join("|");
  const tc = (m.tool_calls ?? []).map((t) => `[tool_call ${t.function?.name} id=${t.id}]`).join(",");
  const preview = String(text).replace(/\s+/g, " ").slice(0, 64);
  return `${m.role}${m.name ? `(${m.name})` : ""} ${tc} ${tc || text ? "" : ""}${preview}`.trimEnd();
};

const okIndex = (messages) =>
  messages.findIndex((m) => m.role === "user" && JSON.stringify(m.content ?? "").includes(MARK));

let failures = 0;
let baseline = null;
for (const { lap, messages } of laps) {
  if (typeof lap !== "number") continue; // 标题生成等非流式旁路请求不参与判定
  console.log(`\n=== lap ${lap}：${messages.length} 条消息 ===`);
  messages.forEach((m, i) => console.log(`  [${String(i).padStart(2)}] ${describe(m)}`));
  const idx = okIndex(messages);
  console.log(`  >>> user "${MARK}" 位于下标 ${idx}`);
  const firstAssistant = messages.findIndex((m) => m.role === "assistant");
  const userAfterTools = messages.some(
    (m, i) =>
      m.role === "user" &&
      JSON.stringify(m.content ?? "").includes(MARK) &&
      firstAssistant !== -1 &&
      i > firstAssistant,
  );
  if (lap === 0) {
    baseline = idx;
    const ok = idx === messages.length - 1;
    console.log(`  判定 lap0：${ok ? "✓ ok 是最后一条（回合起点）" : "✗ 形态异常"}`);
    if (!ok) failures++;
  } else {
    const stable = idx === baseline;
    const tail = idx === messages.length - 1;
    console.log(
      `  判定 lap${lap}：位置${stable ? "稳定 ✓" : `漂移 ✗（${baseline}→${idx}）`}；${tail ? "挂在末尾 ✗（模型会把已答过的 user 消息当最新输入）" : "未挂末尾 ✓"}`,
    );
    if (!stable || tail || userAfterTools) failures++;
  }
}

console.log(`\n===== 结论：${failures === 0 ? "PASS —— user 消息位置全程按真实上下文排列，未跳到工具结果之后" : `FAIL —— ${failures} 处异常`} =====`);

// ── 原生 dump 交叉验证：request-log.jsonl（daemon 侧传输层视图）──────────────
// 判定「user 消息是否每次都重新发 / 内容是否漂移」：
//   · 每个请求恰好 1 条含标记的 user 消息（不多发、不重复拼接）；
//   · 其文本逐字节相同（annotation 冻结后不漂移）。
const reqLogPath = path.join(DATA, "sessions", seed, "request-log.jsonl");
if (fs.existsSync(reqLogPath)) {
  const reqLog = fs
    .readFileSync(reqLogPath, "utf8")
    .split(/\r?\n/)
    .filter(Boolean)
    .map((l) => JSON.parse(l));
  console.log(`\n===== daemon 原生 dump（request-log.jsonl，${reqLog.length} 个请求）=====`);
  let baselineText = null;
  let dupFailures = 0;
  let driftFailures = 0;
  for (const rec of reqLog) {
    const users = rec.messages.filter((m) => m.role === "user" && m.text.includes(MARK));
    const dup = users.length !== 1 ? `✗ 出现 ${users.length} 次` : "✓ 恰好 1 次";
    const text = users[0]?.text ?? "";
    const drift =
      baselineText === null ? "（基线）" : text === baselineText ? "✓ 与基线逐字节相同" : "✗ 文本漂移";
    if (users.length !== 1) dupFailures++;
    if (baselineText !== null && text !== baselineText) driftFailures++;
    baselineText ??= text;
    console.log(
      `  rev=${String(rec.rev).padStart(3)} n=${String(rec.n).padStart(2)} turns=${rec.turns} trailing=${rec.trailing} | user标记${dup} | ${drift} | len=${text.length}`,
    );
  }
  console.log(
    `===== dump 结论：${dupFailures + driftFailures === 0 ? "PASS —— user 消息每轮恰好发送 1 次、内容零漂移（历史复用，非重新生成）" : `FAIL —— 重复 ${dupFailures} 处 / 漂移 ${driftFailures} 处`} =====`,
  );
  if (dupFailures + driftFailures > 0) process.exitCode = 1;
} else {
  console.log("\n（未找到 request-log.jsonl —— QAQH_REQUEST_LOG 未生效或 seed 不符）");
  process.exitCode = 1;
}
process.exitCode = failures === 0 ? 0 : 1;
