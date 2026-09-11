/**
 * 压测驱动 v2：控制通道命令建会话，会话通道命令发消息。
 */
import { readFileSync, existsSync, readdirSync, statSync } from "node:fs";

const DISCOVERY = "D:/project/QAQ-Harness/.stress/qaqh-home/.qaqh/daemon.json";
const TIMELINE_DIR = "D:/project/QAQ-Harness/.stress/qaqh-home/.qaqh/ringing/ringing-timeline";
const SCHEMA = "qaqh.Ringing";
const VERSION = 1;

const disc = JSON.parse(readFileSync(DISCOVERY, "utf-8"));
const BASE = disc.endpoint;
const AUTH = { Authorization: `Bearer ${disc.token}`, "Content-Type": "application/json" };

async function openLease() {
  const r = await fetch(`${BASE}/ringing/v1/clients/open`, {
    method: "POST",
    headers: AUTH,
    body: JSON.stringify({
      schema: SCHEMA,
      version: VERSION,
      client_instance_id: `stress-${Date.now()}`,
    }),
  });
  if (!r.ok) throw new Error(`open ${r.status}: ${await r.text()}`);
  return r.json();
}

async function sendCommand(sessionId, clientInstanceId, channel, seed, command) {
  const cid = `cmd-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
  const env = {
    schema: SCHEMA,
    version: VERSION,
    channel,
    command_id: cid,
    client_instance_id: clientInstanceId,
    client_session_id: sessionId,
    ...(seed ? { seed } : {}),
    command,
  };
  const r = await fetch(`${BASE}/ringing/v1/commands/${channel}`, {
    method: "POST",
    headers: { ...AUTH, "x-qaqh-client-session-id": sessionId },
    body: JSON.stringify(env),
  });
  const text = await r.text();
  let body;
  try { body = JSON.parse(text); } catch { body = text; }
  return { status: r.status, body, commandId: cid };
}

async function getService(sessionId, method, params) {
  const r = await fetch(`${BASE}/ringing/v1/service/${method}`, {
    method: "POST",
    headers: { ...AUTH, "x-qaqh-client-session-id": sessionId },
    body: JSON.stringify(params),
  });
  const text = await r.text();
  let body;
  try { body = JSON.parse(text); } catch { body = text; }
  return { status: r.status, body };
}

function dirSize(dir) {
  if (!existsSync(dir)) return 0;
  let total = 0;
  for (const f of readdirSync(dir)) {
    try { total += statSync(`${dir}/${f}`).size; } catch {}
  }
  return total;
}

const fmt = (n) => (n / 1024 / 1024).toFixed(2);
const listSeeds = (body) =>
  Array.isArray(body) ? body.map((s) => s.seed) : (body?.sessions ?? []).map((s) => s.seed);

async function main() {
  const open = await openLease();
  const sessionId = open.client_session_id;
  const instanceId = `stress-${Date.now()}`;
  console.log(`[driver] lease opened: session=${sessionId.slice(0, 12)}...`);

  // 1. 会话列表（空则经控制通道创建）
  let list = await getService(sessionId, "session.list", {});
  let seeds = listSeeds(list.body);
  console.log(`[driver] session.list -> ${list.status}, seeds=${JSON.stringify(seeds)}`);
  if (seeds.length === 0) {
    const created = await sendCommand(sessionId, instanceId, "control", null, {
      channel: "control",
      type: "session_create",
      close_current: false,
    });
    console.log(`[driver] session_create -> ${created.status}: ${JSON.stringify(created.body).slice(0, 160)}`);
    for (let i = 0; i < 20 && seeds.length === 0; i++) {
      await new Promise((r) => setTimeout(r, 600));
      list = await getService(sessionId, "session.list", {});
      seeds = listSeeds(list.body);
    }
    console.log(`[driver] seeds after create: ${JSON.stringify(seeds)}`);
  }
  const seed = seeds[0];
  if (!seed) throw new Error("no session seed available");

  // 2. 发消息（会话通道）触发长流
  const send = await sendCommand(sessionId, instanceId, "conversation", seed, {
    channel: "conversation",
    type: "conversation_send_message",
    text: "请直接开始输出长文本，不要调用任何工具，不要停止，直到内容自然结束。",
  });
  console.log(`[driver] conversation_send_message -> ${send.status}: ${JSON.stringify(send.body).slice(0, 160)}`);

  // 3. 采样 timeline 目录
  const t0 = Date.now();
  let lastSize = 0;
  const seen = new Set();
  while (Date.now() - t0 < 90 * 60_000) {
    await new Promise((r) => setTimeout(r, 10_000));
    const size = dirSize(TIMELINE_DIR);
    const files = existsSync(TIMELINE_DIR) ? readdirSync(TIMELINE_DIR) : [];
    for (const f of files) seen.add(f);
    const min = ((Date.now() - t0) / 60000).toFixed(1);
    console.log(`[sample ${min}min] timeline_dir=${fmt(size)}MB (D${((size - lastSize) / 1024).toFixed(0)}KB/10s) files=${files.join(",")}`);
    lastSize = size;
    try {
      const mockLog = readFileSync("D:/project/QAQ-Harness/.stress/mock_out.log", "utf-8");
      if (mockLog.includes("stream done")) {
        console.log("[driver] mock stream finished; settling 60s...");
        await new Promise((r) => setTimeout(r, 60_000));
        break;
      }
    } catch {}
  }
  console.log(`[driver] FINAL timeline_dir=${fmt(dirSize(TIMELINE_DIR))}MB`);
}

main().catch((e) => {
  console.error(`[driver] FATAL: ${e.message}`);
  process.exit(1);
});
