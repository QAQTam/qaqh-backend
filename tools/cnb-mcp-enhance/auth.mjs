/**
 * auth.mjs — CNB token 生命周期管理 + OAuth2 设备授权流
 *
 * 职责：
 *   1. token 读取与校验（~/.cnb/token，与 cnb CLI 共享登录态）
 *   2. access_token 过期时自动 refresh（POST /oauth2/token, grant_type=refresh_token）
 *   3. refresh 失败/无 token 时发起设备授权流（RFC 8628）：
 *      POST /oauth2/device/auth → 用户访问 verification_uri_complete → 轮询 /oauth2/token
 *
 * 状态机：
 *   ok ──(临近过期)──► refresh ──成功──► ok
 *                        │失败
 *                        ▼
 *                   device_auth ──(用户浏览器确认)──► ok
 *
 * 被授权流阻塞时的回调：onAuthorizationRequired({ verification_url, user_code, expires_in })
 * —— MCP 层用它向客户端发 elicitation（弹授权卡片）。
 */
import { readFile, writeFile, mkdir } from "node:fs/promises";
import { homedir, platform } from "node:os";
import { join } from "node:path";

const API_BASE = process.env.CNB_API_ENDPOINT || "https://api.cnb.cool";
const CLIENT_ID = "cnb_cli";
const TOKEN_FILE = process.env.CNB_TOKEN_FILE || join(homedir(), ".cnb", "token");
const EXPIRY_SKEW_MS = 5 * 60 * 1000; // 提前 5 分钟视为过期

// ─────────────────────────── token 读写 ───────────────────────────

let cached = null; // { access_token, refresh_token, expires_at, ... }

export async function readTokenFile() {
  try {
    const raw = await readFile(TOKEN_FILE, "utf-8");
    cached = JSON.parse(raw);
    return cached;
  } catch {
    cached = null;
    return null;
  }
}

/** 当前可用 token；临近过期自动 refresh；失败返回 null（调用方应走授权流） */
export async function getValidToken() {
  let t = cached ?? (await readTokenFile());
  if (!t?.access_token) return null;
  if (isExpired(t)) {
    t = await tryRefresh(t).catch(() => null);
    if (!t) return null;
  }
  return t.access_token;
}

function isExpired(t) {
  // expires_at 是秒级时间戳，乘 1000 转毫秒再比较
  return !t.expires_at || Date.now() >= t.expires_at * 1000 - EXPIRY_SKEW_MS;
}

/** 用 refresh_token 换新 token；成功写回文件并更新缓存 */
export async function tryRefresh(t) {
  t = t || cached || (await readTokenFile());
  if (!t?.refresh_token) return null;
  const body = new URLSearchParams({
    grant_type: "refresh_token",
    refresh_token: t.refresh_token,
    client_id: CLIENT_ID,
  });
  try {
    const res = await fetch(`${API_BASE}/oauth2/token`, {
      method: "POST",
      headers: { "Content-Type": "application/x-www-form-urlencoded", Accept: "application/json" },
      body,
    });
    const data = await res.json();
    if (!res.ok || !data.access_token) return null;
    const fresh = { ...t, ...data, expires_at: Math.floor(Date.now() / 1000) + (data.expires_in ?? 8 * 3600) };
    await writeTokenFile(fresh);
    return fresh;
  } catch {
    return null;
  }
}

async function writeTokenFile(t) {
  cached = t;
  await mkdir(join(TOKEN_FILE, ".."), { recursive: true });
  await writeFile(TOKEN_FILE, JSON.stringify(t, null, 2));
}

// ─────────────────────────── 设备授权流（RFC 8628） ───────────────────────────

/**
 * 发起设备授权：
 *   1. POST /oauth2/device/auth → { device_code, user_code, verification_uri_complete, interval, expires_in }
 *   2. 打开浏览器到 verification_uri_complete
 *   3. 轮询 POST /oauth2/token（interval 秒一次）直到用户确认/超时
 *
 * @param {object} opts
 * @param {function} opts.onCode  收到 user_code 时回调（MCP 层用它弹授权卡片）；参数
 *                                { verification_url, user_code, expires_in }
 * @param {function} opts.onPoll  每次轮询回调（可选）：{ attempt, error }
 * @param {number}   opts.maxWaitMs 最长等待（默认 = expires_in）
 * @returns {{ ok: true } | { ok: false, reason: string }}
 */
export async function deviceAuth({ onCode, onPoll } = {}) {
  // ⚠️ OAuth 端点固定在 https://cnb.cool（主站域名），与 API_BASE（api.cnb.cool）无关 ——
  // cnb login --debug 实录：POST https://cnb.cool/oauth2/device/auth（api.cnb.cool 返 404）
  const platformBase = "https://cnb.cool";
  // 1. 发起设备授权
  const res = await fetch(`${platformBase}/oauth2/device/auth`, {
    method: "POST",
    headers: { "Content-Type": "application/x-www-form-urlencoded", Accept: "application/json" },
    body: new URLSearchParams({ client_id: CLIENT_ID }),
  });
  const init = await res.json();
  if (!init.device_code) throw new Error(`device auth init failed: ${JSON.stringify(init).slice(0, 200)}`);

  const verificationUrl = init.verification_uri_complete ||
    `${init.verification_uri}?user_code=${init.user_code}`;
  const intervalMs = (init.interval ?? 5) * 1000;
  const deadline = Date.now() + (init.expires_in ?? 600) * 1000;

  // 2. 弹给用户（onCode 回调 + 尽力自动开浏览器）
  if (onCode) onCode({ verification_url: verificationUrl, user_code: init.user_code, expires_in: init.expires_in });
  openBrowser(verificationUrl);

  // 3. 轮询
  let attempt = 0;
  while (Date.now() < deadline) {
    await sleep(intervalMs);
    attempt++;
    const res = await fetch("https://cnb.cool/oauth2/token", {
      method: "POST",
      headers: { "Content-Type": "application/x-www-form-urlencoded", Accept: "application/json" },
      body: new URLSearchParams({
        grant_type: "urn:ietf:params:oauth:grant-type:device_code",
        device_code: init.device_code,
        client_id: CLIENT_ID,
      }),
    });
    const pdata = await res.ok ? await res.json() : await res.json().catch(() => ({}));

    if (res.ok && pdata.access_token) {
      const fresh = {
        access_token: pdata.access_token,
        refresh_token: pdata.refresh_token,
        expires_at: Math.floor(Date.now() / 1000) + (pdata.expires_in ?? 8 * 3600),
        platform_url: "https://cnb.cool",
        client_id: CLIENT_ID,
        login_host: API_BASE,
      };
      await writeTokenFile(fresh);
      return { ok: true };
    }
    const err = pdata.error || "unknown";
    if (err === "authorization_pending") { onPoll?.({ attempt, error: err }); continue; }
    if (err === "slow_down") { await sleep(3000); continue; }
    return { ok: false, reason: err }; // expired_token / access_denied / ...
  }
  return { ok: false, reason: "timeout" };
}

function openBrowser(url) {
  import("node:child_process").then(({ spawn }) => {
    try {
      if (platform() === "win32") spawn("cmd", ["/c", "start", "", url], { detached: true, stdio: "ignore" }).unref();
      else if (platform() === "darwin") spawn("open", [url], { detached: true, stdio: "ignore" }).unref();
      else spawn("xdg-open", [url], { detached: true, stdio: "ignore" }).unref();
    } catch { /* 静默：用户可手动打开 onCode 给出的 URL */ }
  });
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// ─────────────────────────── 请求包装：401 自动恢复 ───────────────────────────

/**
 * 带 token 的 API 请求。401 时的恢复链：
 *   access 过期 → refresh → 重试
 *   refresh 失败 → deviceAuth（弹授权）→ 成功后重试原请求
 *
 * @param {function} apiCall 接收 token 的请求函数： (token) => Promise<Response>
 * @param {object}   hooks   { onAuthorizationRequired, onPoll } 透传给 deviceAuth
 */
export async function withAuth(apiCall, hooks = {}) {
  const token = await getValidToken();
  let res = await apiCall(token);
  if (res.status !== 401) return res;

  // 先试 refresh
  const fresh = await tryRefresh();
  if (fresh) {
    res = await apiCall(fresh.access_token);
    if (res.status !== 401) return res;
  }

  // 走设备授权（阻塞式：等用户浏览器确认）
  const r = await deviceAuth({
    onCode: hooks.onAuthorizationRequired,
    onPoll: hooks.onPoll,
  });
  if (!r.ok) throw new Error(`authorization failed: ${r.reason}`);

  const t = await getValidToken();
  return apiCall(t); // 重试原请求
}
