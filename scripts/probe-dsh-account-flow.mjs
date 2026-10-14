#!/usr/bin/env node
/**
 * 完整链路只读探针：DeepSeek 平台「官网授权」→ 账号凭据 → 只读拉资料/余额/赠额。
 *
 * 参照实现（E:\dpsk-harness = github.com/deepseek-ai/deepseek-harness）：
 * - 授权：packages/credentials/deepseek-account-platform/src/index.ts
 *     POST /auth-api/v0/dsh/auth_init      body {code_challenge,code_challenge_method:'S256',state,redirect_uri,locale,login_source}
 *     回环回调路径固定 /oauth/callback（:540-560）；浏览器只去 /dsh/authorize（:566）
 *     POST /auth-api/v0/dsh/auth_exchange  body {code,code_verifier,redirect_uri,device_id,device_model,os_version}（:583-586）
 * - 只读接口：src/details.ts
 *     GET  /auth-api/v0/users/current                → 资料
 *     GET  /api/v0/users/get_user_summary            → normal_wallets / bonus_wallets（**金额**）
 *     GET  /api/v0/users/get_unnotified_bonuses      → 赠额明细
 * - 推理鉴权：packages/llm/llm-deepseek-account/src/index.ts:22-25
 *     header `x-dsh-auth-token`（无 Bearer），目标 https://api.deepseek.com
 * - 响应信封 {code, msg, data:{biz_code, biz_msg, biz_data}}
 *
 * 纪律（与参照实现一致）：token 只留进程内存、**不写盘**、不打印全量（只打前后各几位的掩码）；
 * 默认**不调 logout**（不撤销、不动你的浏览器会话）。要作废就跑 `--logout`。
 *
 * 用法：
 *   node scripts/probe-dsh-account-flow.mjs                 # 自动开浏览器，等你点授权
 *   node scripts/probe-dsh-account-flow.mjs --no-open       # 自己复制 URL
 *   node scripts/probe-dsh-account-flow.mjs --logout        # 读完后在平台侧注销该授权
 */

import { createHash, randomBytes, randomUUID } from 'node:crypto'
import { createServer } from 'node:http'
import { spawn } from 'node:child_process'
import { arch, platform, release } from 'node:os'

const args = process.argv.slice(2)
const flag = (name, fallback) => {
  const at = args.indexOf(`--${name}`)
  return at >= 0 && args[at + 1] !== undefined ? args[at + 1] : fallback
}
const has = (name) => args.includes(`--${name}`)

const ORIGIN = flag('origin', 'https://platform.deepseek.com')
const INFERENCE = flag('inference', 'https://api.deepseek.com')
const PORT = Number(flag('port', '51337'))
const VERSION = flag('version', '0.2.1-alpha.2')
const TIMEOUT_MS = Number(flag('timeout', '600')) * 1000
const LOCALE = 'zh_CN'
const TZ_OFFSET = String(-new Date().getTimezoneOffset() * 60)
const DO_LOGOUT = has('logout')

const b64url = (buf) => buf.toString('base64').replaceAll('+', '-').replaceAll('/', '_').replaceAll('=', '')
const state = b64url(randomBytes(16))
const verifier = b64url(randomBytes(32))
const challenge = b64url(createHash('sha256').update(verifier).digest())
const redirectUri = `http://127.0.0.1:${PORT}/oauth/callback`

/** 参照实现里 provider 独占的五个客户端头（上一发已实测：服务端不把它们当准入）。 */
const clientHeaders = {
  'x-client-bundle-id': '',
  'x-client-platform': `desktop-${platform() === 'win32' ? 'win' : 'mac'}`,
  'x-client-version': VERSION,
  'x-client-locale': LOCALE,
  'x-client-timezone-offset': TZ_OFFSET,
}

const mask = (value) => (typeof value === 'string' && value.length > 12
  ? `${value.slice(0, 6)}…${value.slice(-4)} (${value.length}B)`
  : '<short or absent>')

/** 原样打印响应前先抹掉凭据：本探针吐的就是 token 本身，日志不该留全量。 */
const redact = (text) => text.replace(
  /"(token|access_token|auth_token|code_verifier|code)"\s*:\s*"[^"]{6,}"/g,
  '"$1":"<redacted>"',
)

const show = (label, status, contentType, text, limit = 1600) => {
  const safe = redact(text)
  process.stdout.write(`\n--- ${label} ---\n`)
  process.stdout.write(`status: ${status}${contentType ? `  content-type: ${contentType}` : ''}\n`)
  process.stdout.write(`raw(${safe.length}B): ${safe.slice(0, limit)}${safe.length > limit ? '…' : ''}\n`)
  let parsed
  try {
    parsed = JSON.parse(text)
  } catch {
    return undefined
  }
  // 信封 {code,msg,data:{biz_code,biz_msg,biz_data}}；共享 reader 会把 biz_data 解包出来。
  const payload = parsed?.data?.biz_data ?? parsed?.data ?? parsed
  process.stdout.write(`key paths: code=${parsed?.code} biz_code=${parsed?.data?.biz_code} biz_msg=${JSON.stringify(parsed?.data?.biz_msg)}\n`)
  process.stdout.write(`payload keys: ${payload && typeof payload === 'object' ? JSON.stringify(Object.keys(payload)) : JSON.stringify(payload)}\n`)
  return payload
}

async function post(path, body, headers) {
  const response = await fetch(`${ORIGIN}${path}`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', ...headers },
    body: JSON.stringify(body),
    redirect: 'error',
  })
  const text = await response.text()
  return { status: response.status, contentType: response.headers.get('content-type') ?? '', text }
}

async function get(url, headers) {
  const response = await fetch(url, { method: 'GET', headers, redirect: 'error' })
  const text = await response.text()
  return { status: response.status, contentType: response.headers.get('content-type') ?? '', text }
}

// ── 1) 起回环监听（redirect_uri 必须真在监听，浏览器才接得住） ──

let settle
const callback = new Promise((resolve) => { settle = resolve })
const server = createServer((req, res) => {
  const url = new URL(req.url ?? '/', 'http://127.0.0.1')
  if (req.method !== 'GET' || url.pathname !== '/oauth/callback') {
    res.writeHead(404, { 'cache-control': 'no-store' }).end()
    return
  }
  res.writeHead(200, { 'content-type': 'text/html; charset=utf-8', 'cache-control': 'no-store' })
    .end('<meta charset="utf-8"><h3>已收到授权，可以关闭此页面了。</h3><p>本次探针到此为止，凭据不会落盘。</p>')
  settle(Object.fromEntries(url.searchParams))
})

await new Promise((resolve) => server.listen(PORT, '127.0.0.1', resolve))
process.stdout.write(`[probe] callback listening on ${redirectUri}\n`)

// ── 2) auth_init ──

const init = await post('/auth-api/v0/dsh/auth_init', {
  code_challenge: challenge,
  code_challenge_method: 'S256',
  state,
  redirect_uri: redirectUri,
  locale: LOCALE,
  login_source: 'desktop',
}, clientHeaders)
const initPayload = show('auth_init', init.status, init.contentType, init.text)
const authorizeUrl = initPayload?.authorize_url
if (typeof authorizeUrl !== 'string') {
  process.stdout.write('\n[probe] auth_init 没有给出 authorize_url，停止。\n')
  server.close()
  process.exit(1)
}

process.stdout.write(`\n[probe] 授权地址（有效期 ${initPayload?.expires_in ?? '?'}s）：\n${authorizeUrl}\n\n`)
if (!has('no-open')) {
  const opener = platform() === 'win32'
    ? spawn('cmd', ['/c', 'start', '', authorizeUrl], { detached: true, stdio: 'ignore' })
    : spawn('open', [authorizeUrl], { detached: true, stdio: 'ignore' })
  opener.on('error', (error) => process.stdout.write(`[probe] 自动打开浏览器失败（${error.message}），请手动复制上面的地址。\n`))
  opener.unref()
}

const params = await Promise.race([
  callback,
  new Promise((resolve) => setTimeout(() => resolve(null), TIMEOUT_MS)),
])
if (params === null) {
  process.stdout.write(`\n[probe] ${TIMEOUT_MS / 1000}s 内没有收到回调，超时退出。\n`)
  server.close()
  process.exit(1)
}

process.stdout.write(`\n--- callback ---\n${JSON.stringify({ ...params, code: mask(params.code) })}\n`)
server.close()
if (params.state !== state) {
  process.stdout.write(`[probe] state 不匹配（期望 ${state}），拒绝继续。\n`)
  process.exit(1)
}
if (typeof params.code !== 'string' || params.code.length === 0) {
  process.stdout.write('[probe] 回调里没有 code，停止。\n')
  process.exit(1)
}

// ── 3) auth_exchange ──

const exchange = await post('/auth-api/v0/dsh/auth_exchange', {
  code: params.code,
  code_verifier: verifier,
  redirect_uri: redirectUri,
  device_id: randomUUID(),
  device_model: `${platform()}-${arch()}`,
  os_version: release(),
}, clientHeaders)
const exchangePayload = show('auth_exchange', exchange.status, exchange.contentType, exchange.text)

// token 可能落在 payload.token / payload.access_token / payload.biz_data.token 等位置。
const tokenCandidates = [exchangePayload?.token, exchangePayload?.access_token, exchangePayload?.auth_token]
const token = tokenCandidates.find((value) => typeof value === 'string' && value.length > 0)
process.stdout.write(`\n[probe] token: ${token ? mask(token) : '<未在预期字段找到，见上方原始返回>'}\n`)
process.stdout.write(`[probe] authorized_url: ${exchangePayload?.authorized_url ?? '<absent>'}\n`)

if (!token) {
  process.stdout.write('\n[probe] 拿不到 token，只读接口无法继续。\n')
  process.exit(1)
}

// ── 4) 只读：资料 / 余额（金额）/ 赠额 ──

const authHeaders = { ...clientHeaders, 'x-dsh-auth-token': token }

const profile = await get(`${ORIGIN}/auth-api/v0/users/current`, authHeaders)
show('profile  GET /auth-api/v0/users/current', profile.status, profile.contentType, profile.text)

const summary = await get(`${ORIGIN}/api/v0/users/get_user_summary`, authHeaders)
const summaryPayload = show('balance  GET /api/v0/users/get_user_summary', summary.status, summary.contentType, summary.text)
if (summaryPayload) {
  process.stdout.write(`[probe] 充值钱包 normal_wallets: ${JSON.stringify(summaryPayload.normal_wallets)}\n`)
  process.stdout.write(`[probe] 赠金钱包 bonus_wallets:  ${JSON.stringify(summaryPayload.bonus_wallets)}\n`)
}

const bonuses = await get(`${ORIGIN}/api/v0/users/get_unnotified_bonuses`, authHeaders)
show('bonus    GET /api/v0/users/get_unnotified_bonuses', bonuses.status, bonuses.contentType, bonuses.text)

// ── 5) 推理侧：账号 token 能否用于 api.deepseek.com（只列模型，不产生 token 费用） ──

const modelsWithToken = await get(`${INFERENCE}/models`, { 'x-dsh-auth-token': token })
show('inference GET /models (x-dsh-auth-token)', modelsWithToken.status, modelsWithToken.contentType, modelsWithToken.text, 600)
const modelsWithout = await get(`${INFERENCE}/models`, {})
show('control   GET /models (无凭据)', modelsWithout.status, modelsWithout.contentType, modelsWithout.text, 300)

// ── 6) 可选注销 ──

if (DO_LOGOUT) {
  const logout = await post('/auth-api/v0/users/logout', {}, { ...clientHeaders, 'x-dsh-auth-token': token })
  show('logout   POST /auth-api/v0/users/logout', logout.status, logout.contentType, logout.text, 400)
} else {
  process.stdout.write('\n[probe] 未调用 logout：该授权在平台侧仍然有效，token 只在本进程内存里，已经随进程结束丢弃。\n')
  process.stdout.write('[probe] 想在平台侧作废它，重跑并加 --logout，或到 platform.deepseek.com 手动退出。\n')
}
process.stdout.write('[probe] done\n')
process.exit(0)
