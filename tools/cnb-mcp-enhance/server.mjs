#!/usr/bin/env node
/**
 * cnb-mcp-enhance — CNB MCP 官方服务器（@cnbcool/mcp-server）的 QAQ-Harness 增强层。
 *
 * 两个模式：
 *   1. MCP stdio server（默认）：提供 6 个官方缺失的工具
 *   2. --watch 独立监视器：命令行直接跑，输出 wave 全景快照
 *
 * 零 npm 依赖：MCP stdio 用原生 JSON-RPC over stdio 实现（协议核心极小），
 * HTTP 直接 fetch（Node >= 18 内置）。
 *
 * 认证（三层恢复链，见 auth.mjs）：
 *   1. ~/.cnb/token 的 access_token（与 cnb CLI 共享登录态）
 *   2. 过期自动 refresh_token 续期
 *   3. 都失败 → MCP elicitation 弹授权卡片（verification URL + user_code）→ 用户浏览器确认 → 自动重试原请求
 *
 * 环境变量：
 *   CNB_TOKEN   — 可选；不设则用 ~/.cnb/token
 *   CNB_REPO    — 默认仓库（如 QAQ-Harness/qaqh-backend）
 *   CNB_API_ENDPOINT — 默认 https://api.cnb.cool
 */
import { createInterface } from "node:readline";
import { getValidToken, tryRefresh, deviceAuth } from "./auth.mjs";

const API_BASE = process.env.CNB_API_ENDPOINT || "https://api.cnb.cool";
const DEFAULT_REPO = process.env.CNB_REPO || "QAQ-Harness/qaqh-backend";
const UA = "qaqh-cnb-mcp-enhance/0.1";

// ─────────────────────────── CNB HTTP API 封装 ───────────────────────────

// 401 恢复时的钩子：MCP 层把它接到 elicitation（弹授权卡）；--watch 模式打印到控制台
let authHooks = { onAuthorizationRequired: null, onPoll: null };
export function setAuthHooks(h) { authHooks = { ...authHooks, ...h }; }

/** 单次带 token 的请求（不处理 401） */
async function rawApi(path, { method = "GET", body } = {}) {
  const token = await getValidToken();
  if (!token) {
    const err = new Error("CNB API 401: no valid token (run device auth)");
    err.status = 401;
    throw err;
  }
  const res = await fetch(`${API_BASE}${path}`, {
    method,
    headers: {
      Authorization: `Bearer ${token}`,
      "Content-Type": "application/json",
      Accept: "application/json",
      "User-Agent": UA,
    },
    body: body ? JSON.stringify(body) : undefined,
    // 30s 连接+响应超时：挂死的 API 调用不应拖垮整个队列（AbortSignal.timeout 内置）
    signal: AbortSignal.timeout(30_000),
  });
  const text = await res.text();
  let json;
  try { json = JSON.parse(text); } catch { json = { raw: text }; }
  if (!res.ok) {
    const err = new Error(`CNB API ${res.status}: ${JSON.stringify(json).slice(0, 300)}`);
    err.status = res.status;
    err.body = json;
    throw err;
  }
  return json;
}

/** 带认证恢复链的请求：401 → refresh → 仍 401 → 设备授权（弹卡）→ 重试 */
async function api(path, { method = "GET", body } = {}) {
  try {
    return await rawApi(path, { method, body });
  } catch (e) {
    if (e.status !== 401) throw e;
    try {
      const fresh = await tryRefresh();
      if (fresh) return await rawApi(path, { method, body });
    } catch { /* fallthrough */ }
    const r = await deviceAuth({
      onCode: (info) => authHooks.onAuthorizationRequired?.(info),
      onPoll: (info) => authHooks.onPoll?.(info),
    });
    if (!r.ok) throw new Error(`CNB authorization failed: ${r.reason}`);
    return await rawApi(path, { method, body });
  }
}

const enc = encodeURIComponent;
const repoPath = (repo) => `/${repo || DEFAULT_REPO}`;

// ── build ──
const getBuildStatus = (repo, sn) => api(`${repoPath(repo)}/-/build/status/${enc(sn)}`);
const getBuildStage = (repo, sn, pipelineId, stageId) =>
  api(`/${repo || DEFAULT_REPO}/-/build/logs/stage/${enc(sn)}/${enc(pipelineId)}/${enc(stageId)}`);
const getBuildLogs = (repo, query = {}) => {
  const q = new URLSearchParams(query).toString();
  return api(`${repoPath(repo)}/-/build/logs${q ? "?" + q : ""}`);
};
const startBuild = (repo, body) => api(`${repoPath(repo)}/-/build/start`, { method: "POST", body });

// ── issues ──
const listIssueComments = (repo, number, { sort = "-created", pageSize = 3 } = {}) =>
  api(`${repoPath(repo)}/-/issues/${number}/comments?sort=${sort}&page_size=${pageSize}`);

// ── pulls ──
const listPulls = (repo, state = "open") =>
  api(`${repoPath(repo)}/-/pulls?state=${state}&page_size=50`);

// ── npc-observability ──
const npcObservability = (repo, kind, query = {}) => {
  const q = new URLSearchParams(query).toString();
  return api(`${repoPath(repo)}/-/npc-observability/${kind}${q ? "?" + q : ""}`);
};

// ─────────────────────────── 增强工具实现 ───────────────────────────

/** 从 issue 评论文本提取轻量信号（不解析全文，只判断特征） */
function commentSignals(comments) {
  const latest = comments?.[0];
  if (!latest) return { author: "-", npc: false, blocked: false, done: false, hasPr: false };
  const body = latest.body || "";
  return {
    author: latest.author?.username || "?",
    is_npc: latest.author?.is_npc === true,
    blocked: /403|repo-code:rw|无法推送|推送被|替我上班/.test(body),
    done: /已完成|已修复|根因排查完成|可合并|push 完成/.test(body),
    hasPr: /pulls?\/\d+|PR[: #]*\d+/.test(body),
    excerpt: body.replace(/\s+/g, " ").slice(0, 120),
    comment_at: latest.created_at,
  };
}

/** 单个 issue 的聚合状态：构建 + 评论 + PR */
async function issueAggregate(repo, issueNumber, buildSns = []) {
  const result = { issue: issueNumber, builds: [], prs: [], comment: null, verdict: "unknown" };
  // 构建状态
  for (const sn of buildSns) {
    try {
      const st = await getBuildStatus(repo, sn);
      const data = st?.data ?? st;
      const pipelines = data?.pipelinesStatus ?? {};
      const stages = [];
      for (const p of Object.values(pipelines)) {
        for (const s of p?.stages ?? []) {
          if (s?.name === "npc go") stages.push({ status: s.status, duration: s.duration });
        }
      }
      result.builds.push({ sn, status: data?.status, npc_go: stages[0] ?? null });
    } catch (e) {
      result.builds.push({ sn, error: String(e.message).slice(0, 120) });
    }
  }
  // 最新评论信号
  try {
    const comments = await listIssueComments(repo, issueNumber, { pageSize: 2 });
    const arr = comments?.data ?? comments ?? [];
    result.comment = commentSignals(Array.isArray(arr) ? arr : []);
  } catch (e) {
    result.comment = { error: String(e.message).slice(0, 120) };
  }
  // 关联 PR（按标题 Closes #N 匹配）
  try {
    const pulls = await listPulls(repo, "open");
    const arr = pulls?.data ?? pulls ?? [];
    for (const p of arr) {
      if (new RegExp(`Closes #${issueNumber}\\b`).test(p.title || "")) {
        result.prs.push({ number: p.number, title: p.title, mergeable: p.mergeable_state });
      }
    }
  } catch { /* 忽略：PR 匹配是尽力而为 */ }
  // 判定
  const buildDone = result.builds.length > 0 && result.builds.every(
    (b) => b.status === "success" || b.status === "error");
  if (result.prs.length > 0) result.verdict = "has-pr";
  else if (result.comment?.blocked) result.verdict = "blocked-push";
  else if (result.comment?.done) result.verdict = "npc-done-no-pr";
  else if (!buildDone && result.builds.length > 0) result.verdict = "building";
  else if (buildDone) result.verdict = result.builds.every((b) => b.status === "success")
    ? "build-success" : "build-failed";
  return result;
}

/** watch：一批 issue 的全景快照 */
async function watchWave(repo, issues, buildMap = {}) {
  const rows = [];
  for (const n of issues) {
    rows.push(await issueAggregate(repo, n, buildMap[n] || []));
  }
  const summary = {
    repo,
    at: new Date().toISOString(),
    total: rows.length,
    byVerdict: rows.reduce((acc, r) => { acc[r.verdict] = (acc[r.verdict] || 0) + 1; return acc; }, {}),
    rows,
  };
  return summary;
}

/** 构建失败时拉 npc go stage 日志尾部 */
async function buildTail(repo, sn, tailLines = 40) {
  const st = await getBuildStatus(repo, sn);
  const data = st?.data ?? st;
  const pipelines = data?.pipelinesStatus ?? {};
  const tails = [];
  for (const [pipelineId, p] of Object.entries(pipelines)) {
    for (const s of p?.stages ?? []) {
      if (s?.name !== "npc go") continue;
      try {
        const stage = await getBuildStage(repo, sn, pipelineId, s.id);
        const content = stage?.data?.content ?? [];
        const clean = content.map((l) => l.replace(/\u001b\[[0-9;]*m/g, ""));
        const errIdx = clean.findIndex((l) => /error|failed|panic|Exception/i.test(l));
        tails.push({
          pipelineId, stageId: s.id, status: s.status, duration: s.duration,
          error: stage?.data?.error || null,
          tail: clean.slice(-tailLines),
          error_context: errIdx >= 0 ? clean.slice(Math.max(0, errIdx - 3), errIdx + 8) : null,
        });
      } catch (e) {
        tails.push({ pipelineId, stageId: s.id, error: String(e.message).slice(0, 120) });
      }
    }
  }
  return { sn, build_status: data?.status, tails };
}

// ── pulls：合并队列 ──
const getPull = (repo, number) => api(`${repoPath(repo)}/-/pulls/${number}`);
const listPullCommentsAll = (repo, number) =>
  api(`${repoPath(repo)}/-/pulls/${number}/comments?page_size=100`);
const mergePull = (repo, number, body) =>
  api(`${repoPath(repo)}/-/pulls/${number}/merge`, { method: "PUT", body });

/** 拉 PR 的预检结论：最新一条含「预检完成」的评论里的【可合并】/【需修改】。
 *  判别器用「预检完成」——派发评论含「预检（只读」但不含「预检完成」。 */
async function precheckVerdict(repo, prNumber) {
  try {
    const r = await listPullCommentsAll(repo, prNumber);
    const arr = r?.data ?? r ?? [];
    const list = Array.isArray(arr) ? arr : [];
    const sorted = [...list].sort((a, b) =>
      String(a.created_at || "").localeCompare(String(b.created_at || "")));
    for (let i = sorted.length - 1; i >= 0; i--) {
      const body = sorted[i]?.body || "";
      if (!body.includes("预检完成")) continue;
      const v = body.match(/【(可合并|需修改)】/);
      return { verdict: v ? v[1] : "格式异常", at: sorted[i].created_at };
    }
    return { verdict: null };
  } catch (e) {
    return { verdict: null, error: String(e.message).slice(0, 120) };
  }
}

/** 合并队列：逐个串行（前一合并会改变后续 PR 的 merge-base）。
 *  预检【可合并】→ squash merge → 解析标题 Closes #N → 关单（state+state_reason 双参数）。
 *  预检非【可合并】且未 ignore_verdict → 跳过不合并。 */
async function mergeQueue(repo, prs, { ignore_verdict = false, close_issues = true, dry_run = false } = {}) {
  const results = [];
  for (const n of prs) {
    const item = { pr: n };
    try {
      const p = await getPull(repo, n);
      const d = p?.data ?? p;
      item.title = d?.title;
      item.state = d?.state;
      if (item.state && item.state !== "open") {
        item.skipped = `state=${item.state}`;
        results.push(item);
        continue;
      }
      const v = await precheckVerdict(repo, n);
      item.verdict = v.verdict;
      if (v.verdict !== "可合并" && !ignore_verdict) {
        item.skipped = v.verdict ? `precheck=${v.verdict}` : "precheck=无结果";
        results.push(item);
        continue;
      }
      if (dry_run) {
        item.would_merge = true;
        item.would_close = close_issues
          ? [...String(item.title || "").matchAll(/Closes #(\d+)/gi)].map((m) => +m[1])
          : [];
        results.push(item);
        continue;
      }
      const mr = await mergePull(repo, n, {
        merge_style: "squash",
        ...(item.title ? { commit_title: item.title } : {}),
      });
      const s = JSON.stringify(mr);
      // 判定认 merged:true 子串（CLI YAML 假象教训：不要按引号格式匹配）
      item.merged = mr?.merged === true || mr?.data?.merged === true || s.includes('"merged":true');
      if (close_issues && item.title) {
        item.closed_issues = [];
        for (const m of [...String(item.title).matchAll(/Closes #(\d+)/gi)]) {
          const inum = +m[1];
          try {
            await api(`${repoPath(repo)}/-/issues/${inum}`, {
              method: "PATCH",
              body: { state: "closed", state_reason: "completed" },
            });
            item.closed_issues.push(inum);
          } catch (e) {
            (item.close_errors ??= []).push({ issue: inum, error: String(e.message).slice(0, 120) });
          }
        }
      }
    } catch (e) {
      item.error = String(e.message).slice(0, 200);
    }
    results.push(item);
  }
  const summary = results.reduce((a, r) => {
    const k = r.error ? "error"
      : r.skipped ? "skipped"
      : r.merged ? "merged"
      : r.would_merge ? "dry_run_ok"
      : "unknown";
    a[k] = (a[k] || 0) + 1;
    return a;
  }, {});
  return { repo, at: new Date().toISOString(), summary, results };
}

// ─────────────────────────── MCP stdio 协议 ───────────────────────────

const TOOLS = [
  {
    name: "cnb_npc_dispatch",
    description: "触发 api_trigger_wm 流水线执行 NPC 修复任务（可信 scope，等价 UI 勾「替我上班」）。userPrompt 会注入 QAQ-Harness 铁律 systemPrompt。返回 sn 供后续 watch。",
    inputSchema: {
      type: "object",
      properties: {
        repo: { type: "string", description: "仓库路径，默认 " + DEFAULT_REPO },
        task: { type: "string", description: "任务描述（给 NPC 的 userPrompt）" },
        title: { type: "string", description: "构建标题" },
      },
      required: ["task"],
    },
  },
  {
    name: "cnb_npc_observations",
    description: "查 npc-observability：NPC 的调用记录(actions)/push 记录(commits)/PR 记录(prs)",
    inputSchema: {
      type: "object",
      properties: {
        repo: { type: "string" },
        kind: { type: "string", enum: ["actions", "commits", "prs"] },
        start_time: { type: "string", description: "ISO8601，如 2026-09-13T00:00:00Z" },
        end_time: { type: "string" },
      },
      required: ["kind"],
    },
  },
  {
    name: "cnb_watch_wave",
    description: "wave 全景监视器：聚合一批 issue 的构建状态 + 最新 NPC 评论信号 + 已开 PR。一次调用看全部。",
    inputSchema: {
      type: "object",
      properties: {
        repo: { type: "string" },
        issues: { type: "array", items: { type: "number" }, description: "issue 编号列表" },
        build_map: {
          type: "object",
          description: '可选：{ "6": ["sn1"], "7": ["sn2"] } issue → 构建号列表（缺省时只用评论+PR 判定）',
          additionalProperties: { type: "array", items: { type: "string" } },
        },
      },
      required: ["issues"],
    },
  },
  {
    name: "cnb_build_tail",
    description: "拉构建的 npc go stage 日志尾部（失败排查用，含 error 上下文提取）",
    inputSchema: {
      type: "object",
      properties: {
        repo: { type: "string" },
        sn: { type: "string", description: "构建 ID" },
        lines: { type: "number", description: "尾部行数，默认 40" },
      },
      required: ["sn"],
    },
  },
  {
    name: "cnb_issue_close",
    description: "关单一步到位：state=closed + state_reason=completed（CNB 要求两个参数同时传才生效）",
    inputSchema: {
      type: "object",
      properties: {
        repo: { type: "string" },
        number: { type: "number", description: "issue 编号" },
      },
      required: ["number"],
    },
  },
  {
    name: "cnb_merge_queue",
    description: "批量 squash 合并队列：串行逐个 PR——预检结论【可合并】才放行 → squash merge → 解析标题 Closes #N 自动关单（双参数）。预检非【可合并】默认跳过。dry_run=true 只预演不合并。",
    inputSchema: {
      type: "object",
      properties: {
        repo: { type: "string" },
        prs: { type: "array", items: { type: "number" }, description: "PR 编号列表，按合并顺序（串行执行）" },
        ignore_verdict: { type: "boolean", description: "忽略预检结论强制合并（默认 false）" },
        close_issues: { type: "boolean", description: "合并后自动关标题 Closes #N 的 issue（默认 true）" },
        dry_run: { type: "boolean", description: "只检查预检结论与预演，不真正合并" },
      },
      required: ["prs"],
    },
  },
];

async function callTool(name, args) {
  const repo = args.repo || DEFAULT_REPO;
  switch (name) {
    case "cnb_npc_dispatch": {
      const body = {
        event: "api_trigger_wm",
        env: { WAVE3_TASK: args.task },
        title: args.title || `NPC dispatch: ${args.task.slice(0, 40)}`,
      };
      const r = await startBuild(repo, body);
      return { sn: r?.data?.sn ?? r?.sn, url: r?.data?.buildLogUrl, raw: r };
    }
    case "cnb_npc_observations":
      return await npcObservability(repo, args.kind, {
        ...(args.start_time ? { start_time: args.start_time } : {}),
        ...(args.end_time ? { end_time: args.end_time } : {}),
      });
    case "cnb_watch_wave":
      return await watchWave(repo, args.issues, args.build_map || {});
    case "cnb_build_tail":
      return await buildTail(repo, args.sn, args.lines || 40);
    case "cnb_issue_close": {
      // PATCH issue：state=closed + state_reason=completed（组合必须同时传）
      return await api(`${repoPath(repo)}/-/issues/${args.number}`, {
        method: "PATCH",
        body: { state: "closed", state_reason: "completed" },
      });
    }
    case "cnb_merge_queue":
      return await mergeQueue(repo, args.prs, {
        ignore_verdict: args.ignore_verdict === true,
        close_issues: args.close_issues !== false,
        dry_run: args.dry_run === true,
      });
    default:
      throw new Error(`unknown tool: ${name}`);
  }
}

function jsonRpc(id, result) {
  return JSON.stringify({ jsonrpc: "2.0", id, result });
}
function jsonRpcError(id, code, message) {
  return JSON.stringify({ jsonrpc: "2.0", id, error: { code, message } });
}

function jsonRpcNotification(method, params) {
  return JSON.stringify({ jsonrpc: "2.0", method, params });
}

/**
 * MCP elicitation 授权流：
 *   1. deviceAuth 发起（拿到 verification URL + user_code）
 *   2. elicitation/create 弹卡片：message 带可点击 URL 和 user_code，用户在浏览器确认后点“确认”
 *      （设备授权流本身在后台轮询，确认即成功；elicitation 的响应只是「我已打开/看到了」）
 *   3. 若客户端不支持 elicitation，降级为 stdout 打印链接（用户手动打开）并继续轮询
 */
async function authorizeViaElicitation(rpcId) {
  return new Promise((resolve) => {
    let settled = false;
    deviceAuth({
      onCode: ({ verification_url, user_code, expires_in }) => {
        // 先尝试 MCP elicitation（ElicitationCreateRequest）
        process.stdout.write(JSON.stringify({
          jsonrpc: "2.0", id: `elicit-${rpcId}`,
          method: "elicitation/create",
          params: {
            message: `CNB 授权请求：请在浏览器中打开下方链接并确认授权（user_code: ${user_code}，${Math.round(expires_in / 60)} 分钟内有效）\n${verification_url}`,
            requestedSchema: {
              type: "object",
              properties: { confirm: { type: "boolean", title: "我已在浏览器完成授权" } },
              required: ["confirm"],
            },
          },
        }) + "\n");
      },
      onPoll: ({ attempt }) => {
        if (attempt % 12 === 0) process.stdout.write(JSON.stringify(jsonRpcNotification("notifications/message", {
          level: "info", data: `等待浏览器授权中… (${attempt} 次轮询)`,
        })) + "\n");
      },
    }).then((r) => {
      if (!settled) { settled = true; resolve(r); }
    }).catch((e) => {
      if (!settled) { settled = true; resolve({ ok: false, reason: String(e.message).slice(0, 200) }); }
    });
  });
}

async function handleRpc(msg) {
  const { id, method, params } = msg;
  if (method === "initialize") {
    return jsonRpc(id, {
      protocolVersion: params?.protocolVersion || "2024-11-05",
      capabilities: { tools: {} },
      serverInfo: { name: "cnb-mcp-enhance", version: "0.1.0" },
    });
  }
  if (method === "notifications/initialized") return null; // 通知无需响应
  if (method === "ping") return jsonRpc(id, {});
  if (method === "tools/list") {
    return jsonRpc(id, { tools: TOOLS.map((t) => ({ name: t.name, description: t.description, inputSchema: t.inputSchema })) });
  }
  if (method === "tools/call") {
    const { name, arguments: args } = params || {};
    try {
      const result = await callTool(name, args || {});
      return jsonRpc(id, {
        content: [{ type: "text", text: JSON.stringify(result, null, 2) }],
        isError: false,
      });
    } catch (e) {
      // 401 → 弹授权卡（MCP elicitation），用户确认后自动重试原调用
      if (e.status === 401) {
        const retry = await authorizeViaElicitation(id);
        if (retry.ok) {
          try {
            const result = await callTool(name, args || {});
            return jsonRpc(id, {
              content: [{ type: "text", text: JSON.stringify(result, null, 2) }, {
                type: "text", text: "(authorized via device flow, request retried)",
              }],
              isError: false,
            });
          } catch (e2) {
            return jsonRpc(id, {
              content: [{ type: "text", text: `authorized but call still failed: ${e2.message}` }],
              isError: true,
            });
          }
        }
        return jsonRpc(id, {
          content: [{ type: "text", text: `授权未完成（${retry.reason}）。请重新发起调用再次触发授权，或手动执行: cnb login` }],
          isError: true,
        });
      }
      return jsonRpc(id, {
        content: [{ type: "text", text: `ERROR: ${e.message}` }],
        isError: true,
      });
    }
  }
  if (id !== undefined) return jsonRpcError(id, -32601, `method not found: ${method}`);
  return null;
}

async function runServer() {
  const rl = createInterface({ input: process.stdin });
  for await (const line of rl) {
    if (!line.trim()) continue;
    let msg;
    try { msg = JSON.parse(line); } catch { continue; }
    const resp = await handleRpc(msg);
    if (resp) {
      process.stdout.write(resp + "\n");
    }
  }
}

// ─────────────────────────── 独立监视器（--watch） ───────────────────────────

function parseArgs(argv) {
  const args = { watch: false, json: false, issues: [], builds: [] };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--watch") args.watch = true;
    else if (a === "--json") args.json = true;
    else if (a === "--issues") args.issues = (argv[++i] || "").split(",").map(Number).filter(Boolean);
    else if (a === "--builds") args.builds = (argv[++i] || "").split(",").filter(Boolean);
  }
  return args;
}

async function runWatch(args) {
  if (!process.env.CNB_TOKEN) { try { await loadToken(); } catch {} }
  const buildMapPath = new URL("./wave3_builds.json", import.meta.url);
  let buildMap = {};
  try {
    const { readFile } = await import("node:fs/promises");
    // 尝试从仓库 .qaqh/wave3_builds.json 读 SN 映射
    const meta = JSON.parse(await readFile(process.env.BUILD_MAP || buildMapPath.href.replace("file:///", "").replace(/\//g, "\\"), "utf-8"));
    for (const [sn, issue] of Object.entries(meta.map || {})) {
      const key = typeof issue === "number" ? issue : parseInt(issue, 10);
      if (!Number.isNaN(key)) (buildMap[key] ??= []).push(sn);
    }
  } catch { /* 无映射文件时只用评论+PR 判定 */ }

  const issues = args.issues.length ? args.issues : Object.keys(buildMap).map(Number);
  const summary = await watchWave(DEFAULT_REPO, issues, buildMap);
  if (args.json) {
    console.log(JSON.stringify(summary, null, 2));
  } else {
    console.log(`CNB wave 监视 ${summary.at} — ${summary.repo}`);
    console.log(`判定分布: ${JSON.stringify(summary.byVerdict)}`);
    for (const r of rows_sort(summary.rows)) {
      const b = r.builds.map((x) => `${x.sn?.slice(-8)}:${x.npc_go?.status ?? x.status ?? x.error}`).join(" ") || "-";
      const c = r.comment ?? {};
      console.log(
        `  #${r.issue}\t${r.verdict.padEnd(16)}\tbuild[${b}]\tpr[${r.prs.map((p) => "!" + p.number).join(",") || "-"}]\t` +
        `${c.blocked ? "⛔" : c.done ? "✅" : c.is_npc ? "💬" : "…"} ${c.excerpt ?? ""}`
      );
    }
  }
}
const rows_sort = (rows) => [...rows].sort((a, b) => a.issue - b.issue);

// ─────────────────────────── 入口 ───────────────────────────

const argv = process.argv.slice(2);
if (argv.includes("--watch")) {
  runWatch(parseArgs(argv)).catch((e) => { console.error(e); process.exit(1); });
} else {
  runServer().catch((e) => { console.error("server crashed:", e); process.exit(1); });
}
