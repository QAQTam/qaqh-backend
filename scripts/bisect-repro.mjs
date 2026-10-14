// bisect-repro.mjs — 单提交回归测试：构建指定 commit 的 daemon，
// 用真实网关（GLM）跑两轮用户消息，从 wire SSE 提取 reasoning，
// 量化「模型思考是否复述用户消息」。
//
// 用法: node bisect-repro.mjs <commit> <label>
// 产物: <数据目录>../<label>/verdict.json + wire/ 抓包

import { execSync, spawn } from "node:child_process";
import fs from "node:fs";
import http from "node:http";
import path from "node:path";

const REPO = "E:\\qaqh-backend";
const WORKTREE = "E:\\qaqh-bisect";
const TARGET_DIR = "E:\\qaqh-bisect-target";
const BASE_DATA = path.join(process.env.USERPROFILE, ".qaqh-bisect");
const PROXY_PORT = 18790;
const GATEWAY = "http://127.0.0.1:8788";
const SESSION_CWD = "E:\\qaqh-tui-app";
const M1 = "请帮我确认这个仓库的 justfile 提供了哪些常用任务";
const M2 = "那再看看 scripts 目录里有哪些探针脚本，挑两个讲讲用途";

const commit = process.argv[2];
const label = process.argv[3] ?? commit.slice(0, 8);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const sh = (cmd, opts = {}) =>
  execSync(cmd, { stdio: ["ignore", "pipe", "pipe"], encoding: "utf8", ...opts });

// 0. 准备目录
const TEST = path.join(BASE_DATA, label);
fs.rmSync(TEST, { recursive: true, force: true });
const DATA = path.join(TEST, "qaqh"); // daemon 安全规则：以 qaqh 结尾
fs.mkdirSync(path.join(DATA, "sessions"), { recursive: true });

// 1. checkout + build
console.log(`[${label}] checkout ${commit} …`);
sh(`git -C "${WORKTREE}" checkout -f ${commit}`, { stdio: "ignore" });
console.log(`[${label}] cargo build …`);
sh(`cargo build -q -p qaqh-daemon`, { cwd: WORKTREE, env: { ...process.env, CARGO_TARGET_DIR: TARGET_DIR } });
const daemonBin = path.join(TARGET_DIR, "debug", "qaqh-daemon.exe");
if (!fs.existsSync(daemonBin)) throw new Error("daemon binary missing");

// 2. 配置：986e9db 之后是 BYOK 新格式（带明文 api_key 迁移），之前是 legacy
const isNewEra = (() => {
  try {
    sh(`git -C "${REPO}" merge-base --is-ancestor 986e9db ${commit}`);
    return true;
  } catch {
    return false;
  }
})();
const cfg = isNewEra
  ? `provider_id = "openai"
active_profile = "default"
permission_tier = 3
api_key = "bisect-dummy"

[profiles.default]
model = "gfmodel"
max_tokens = 65536
effort = "high"
context_limit = 1000000
base_url = "http://127.0.0.1:${PROXY_PORT}/v1"
endpoint = "openai"
`
  : `provider_id = "openai"
active_profile = "default"
permission_tier = 3

[profiles.default]
model = "gfmodel"
max_tokens = 65536
effort = "high"
context_limit = 1000000
base_url = "http://127.0.0.1:${PROXY_PORT}/v1"
endpoint = "openai"
`;
fs.writeFileSync(path.join(DATA, "config.toml"), cfg);
console.log(`[${label}] config era=${isNewEra ? "byok" : "legacy"}`);

// 3. 独立抓包代理
const proxy = spawn(process.execPath, [path.join(REPO, "scripts", "wire-capture-proxy.mjs"), String(PROXY_PORT)], {
  env: { ...process.env, QAQH_WIRE_TARGET: GATEWAY, QAQH_WIRE_OUT: path.join(TEST, "wire") },
  stdio: "ignore",
});
await sleep(800);

// 4. daemon
const daemon = spawn(daemonBin, ["run"], {
  cwd: WORKTREE,
  env: { ...process.env, QAQH_DATA_DIR: DATA, QAQH_ALLOW_TEST_DATA_ROOT: "1", RUST_LOG: "info" },
  stdio: ["ignore", fs.openSync(path.join(TEST, "daemon.out"), "a"), fs.openSync(path.join(TEST, "daemon.out"), "a")],
});
const cleanup = () => {
  try { daemon.kill(); } catch {}
  try { proxy.kill(); } catch {}
};
process.on("exit", cleanup);

let discovery = null;
for (let i = 0; i < 80 && !discovery; i++) {
  try {
    const d = JSON.parse(fs.readFileSync(path.join(DATA, "daemon.json"), "utf8"));
    if (d.pid === daemon.pid) discovery = d;
  } catch {}
  if (!discovery) await sleep(250);
}
if (!discovery) throw new Error("daemon.json 未发布");
const ENDPOINT = discovery.endpoint;
const TOKEN = discovery.token;
console.log(`[${label}] daemon pid=${daemon.pid}`);

const request = async (method, urlPath, { session, body } = {}) => {
  const headers = { authorization: `Bearer ${TOKEN}` };
  if (session) headers["x-qaqh-client-session-id"] = session;
  if (body !== undefined) headers["content-type"] = "application/json";
  const res = await fetch(ENDPOINT + urlPath, {
    method,
    headers,
    body: body !== undefined ? JSON.stringify(body) : undefined,
  });
  const text = await res.text();
  try { return { status: res.status, body: JSON.parse(text || "{}") }; }
  catch { return { status: res.status, body: {} }; }
};

// 5. v2 流程
const open = await request("POST", "/ringing/v2/clients/open", {
  body: { schema: "qaqh.Ringing", version: 2, client_instance_id: "bisect" },
});
const client = open.body.client_session_id;
const before = new Set(fs.readdirSync(path.join(DATA, "sessions")));
await request("POST", "/ringing/v2/commands/control", {
  session: client,
  body: {
    schema: "qaqh.Ringing", version: 2, channel: "control",
    command_id: `c-${Date.now()}-sc`, client_instance_id: "bisect", client_session_id: client,
    command: { channel: "control", type: "session_create", close_current: false, cwd: SESSION_CWD },
  },
});
await sleep(1200);
const seed = [...new Set(fs.readdirSync(path.join(DATA, "sessions")))].filter((s) => !before.has(s) && s !== "index.jsonl")[0];
if (!seed) throw new Error("未发现新会话 seed");
await request("POST", `/ringing/v2/sessions/${seed}/driver/claim`, { session: client });
console.log(`[${label}] seed=${seed} driver claimed`);

const sendMsg = (text) =>
  request("POST", "/ringing/v2/commands/conversation", {
    session: client,
    body: {
      schema: "qaqh.Ringing", version: 2, channel: "conversation",
      command_id: `c-${Date.now()}-msg`, client_instance_id: "bisect", client_session_id: client,
      session_id: seed,
      command: { channel: "conversation", type: "conversation_send_message", text, images: [], as_system: false },
    },
  });

// 6. 发两条消息，等待两个「n=2」流式请求（各自回合的起点）
const wireDir = path.join(TEST, "wire");
const n2Requests = () => {
  try {
    return fs.readdirSync(wireDir).filter((f) => /^req-\d+\.json$/.test(f)).filter((f) => {
      const r = JSON.parse(fs.readFileSync(path.join(wireDir, f), "utf8"));
      return r.stream === true && Array.isArray(r.body?.messages) && r.body.messages.length === 2;
    });
  } catch { return []; }
};
sendMsg(M1);
console.log(`[${label}] M1 sent, waiting …`);
for (let i = 0; i < 120 && n2Requests().length < 1; i++) await sleep(1000);
await sleep(30); // 等回合跑起来
sendMsg(M2);
console.log(`[${label}] M2 sent, waiting for its turn …`);
for (let i = 0; i < 180 && n2Requests().length < 2; i++) await sleep(1000);
await sleep(12); // 等 lap0 思考流完大半
cleanup();

// 7. 提取 reasoning 并评分
const norm = (s) => String(s).replace(/\s+/g, "").toLowerCase();
const lcs = (a, b) => {
  // 最长公共子串（滚动数组）
  let best = 0;
  const prev = new Array(b.length + 1).fill(0);
  for (let i = 1; i <= a.length; i++) {
    let diag = 0;
    for (let j = 1; j <= b.length; j++) {
      const tmp = prev[j];
      if (a[i - 1] === b[j - 1]) { prev[j] = diag + 1; if (prev[j] > best) best = prev[j]; }
      else prev[j] = 0;
      diag = tmp;
    }
  }
  return best;
};
const reasoningOf = (resFile) => {
  const raw = fs.readFileSync(resFile, "utf8");
  let r = "";
  for (const line of raw.split(/\r?\n/)) {
    if (!line.startsWith("data: ") || line.includes("[DONE]")) continue;
    try {
      const d = JSON.parse(line.slice(6)).choices?.[0]?.delta ?? {};
      if (d.reasoning_content) r += d.reasoning_content;
    } catch {}
  }
  return r;
};
const pairScore = (reqFile) => {
  const rec = JSON.parse(fs.readFileSync(path.join(wireDir, reqFile), "utf8"));
  const userText = rec.body.messages[1].content.split("[UserMessage]").pop().trim();
  const resFile = path.join(wireDir, reqFile.replace("req-", "res-").replace(".json", ".txt"));
  if (!fs.existsSync(resFile)) return { userText, reasoning: 0, lcs: 0, missing: true };
  const reasoning = reasoningOf(resFile);
  const l = lcs(norm(userText), norm(reasoning));
  return { userText, reasoning: reasoning.length, lcs: l, repeat: l >= 8 };
};

const reqs = n2Requests().sort((x, y) => Number(x.match(/\d+/)[0]) - Number(y.match(/\d+/)[0]));
const scores = reqs.map(pairScore);
const repeats = scores.filter((s) => s.repeat).length;
const verdict = { commit, label, era: isNewEra ? "byok" : "legacy", seed, scores, repeats, total: scores.length };
fs.writeFileSync(path.join(TEST, "verdict.json"), JSON.stringify(verdict, null, 2));
for (const s of scores) {
  console.log(`[${label}] user="${s.userText.slice(0, 24)}…" reasoning=${s.reasoning}B 最长公共子串=${s.lcs} 复述=${s.repeat ? "是" : "否"}${s.missing ? "（响应缺失）" : ""}`);
}
console.log(`[${label}] verdict: ${repeats}/${scores.length} 复述 → ${repeats >= Math.ceil(scores.length / 2) ? "BAD（复述）" : "GOOD（不复述）"}`);
