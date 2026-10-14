#!/usr/bin/env node
/**
 * 只读探针：DeepSeek 平台的 DSH 授权入口是否接受「非 DSH 客户端」的 auth_init。
 *
 * 参照实现（E:\dpsk-harness = github.com/deepseek-ai/deepseek-harness）：
 * - `packages/credentials/deepseek-account-platform/src/index.ts:560-567`
 *     POST {platformOrigin}/auth-api/v0/dsh/auth_init
 *     body = { code_challenge, code_challenge_method:'S256', state, redirect_uri, locale, login_source }
 *   回调路径固定 `/oauth/callback`；浏览器只允许去 `/dsh/authorize`（同文件 :540-566）。
 * - 五个客户端头：`packages/credentials/deepseek-account/src/index.ts:156-161`
 *     x-client-bundle-id(故意空) / x-client-platform / x-client-version / x-client-locale / x-client-timezone-offset
 * - platformOrigin = https://platform.deepseek.com
 *     （apps/desktop/tests/account-backend.spec.ts:8 的 authorizeUrl 带出该 origin）
 * - 响应信封 { code, data: { biz_code, biz_data } }（tests/account.spec.ts:95）
 *
 * 本探针**只申请待授权 URL**：不发 code、不换 token、不登录、不写任何文件。
 * code_verifier 只留在进程内，**不打印**（PKCE 秘密不进日志，与参照实现同纪律）。
 *
 * 用法：
 *   node scripts/probe-dsh-auth-init.mjs                # 三发对比：全头 / 无客户端头 / 无 login_source
 *   node scripts/probe-dsh-auth-init.mjs --dry-run      # 只打印将要发的请求，不发
 *   node scripts/probe-dsh-auth-init.mjs --port 51234 --version 0.2.1-alpha.2
 */

import { createHash, randomBytes } from 'node:crypto'

const args = process.argv.slice(2)
const flag = (name, fallback) => {
  const at = args.indexOf(`--${name}`)
  return at >= 0 && args[at + 1] !== undefined ? args[at + 1] : fallback
}
const has = (name) => args.includes(`--${name}`)

const ORIGIN = flag('origin', 'https://platform.deepseek.com')
const PORT = Number(flag('port', '51337'))
const VERSION = flag('version', '0.2.1-alpha.2')
const LOCALE = flag('locale', 'zh_CN')
const TIMEZONE_OFFSET = flag('tz', String(-new Date().getTimezoneOffset() * 60))
const DRY = has('dry-run')

const b64url = (buf) => buf.toString('base64').replaceAll('+', '-').replaceAll('/', '_').replaceAll('=', '')
const state = b64url(randomBytes(16))
// 只在进程内保留；打印里不出现。
const verifier = b64url(randomBytes(32))
const challenge = b64url(createHash('sha256').update(verifier).digest())
const redirectUri = `http://127.0.0.1:${PORT}/oauth/callback`

/** 参照实现的五个客户端头（provider 独占，部署配置不许覆盖）。 */
const clientHeaders = {
  'x-client-bundle-id': '',
  'x-client-platform': 'desktop-win',
  'x-client-version': VERSION,
  'x-client-locale': LOCALE,
  'x-client-timezone-offset': TIMEZONE_OFFSET,
}

const fullBody = {
  code_challenge: challenge,
  code_challenge_method: 'S256',
  state,
  redirect_uri: redirectUri,
  locale: LOCALE,
  login_source: 'desktop',
}

const variants = [
  { name: 'full', headers: clientHeaders, body: fullBody },
  { name: 'no-client-headers', headers: {}, body: fullBody },
  { name: 'no-login-source', headers: clientHeaders, body: { ...fullBody, login_source: undefined } },
]

const url = `${ORIGIN}/auth-api/v0/dsh/auth_init`

/** 从 { code, data:{ biz_code, biz_data } } 里挑出可判断的字段。 */
function summarize(text, contentType) {
  if (!contentType.includes('json')) return { note: `非 JSON 响应（${contentType || 'no content-type'}），疑似 WAF/网关页` }
  let payload
  try {
    payload = JSON.parse(text)
  } catch {
    return { note: 'JSON 解析失败' }
  }
  const data = payload?.data ?? {}
  const biz = data?.biz_data ?? {}
  const out = {
    code: payload?.code,
    biz_code: data?.biz_code,
    msg: data?.msg ?? payload?.msg,
    biz_data_keys: biz && typeof biz === 'object' ? Object.keys(biz) : biz,
  }
  if (typeof biz?.authorize_url === 'string') {
    out.authorize_url_shape = biz.authorize_url.split('?')[0]
    out.authorize_url_has_state = /[?&]state=/.test(biz.authorize_url)
  }
  if (biz?.expires_in !== undefined) out.expires_in = biz.expires_in
  if (biz?.authorize_id !== undefined) out.authorize_id = '<present>'
  return out
}

async function send(variant) {
  const headers = { 'content-type': 'application/json', ...variant.headers }
  const body = JSON.stringify(variant.body)
  process.stdout.write(`\n=== variant: ${variant.name} ===\n`)
  process.stdout.write(`POST ${url}\n`)
  process.stdout.write(`headers: ${JSON.stringify(headers, null, 0)}\n`)
  process.stdout.write(`body: ${body.replace(verifier, '<verifier>')}\n`)
  if (DRY) {
    process.stdout.write('(--dry-run：未发送)\n')
    return
  }
  const started = Date.now()
  const controller = new AbortController()
  const timer = setTimeout(() => controller.abort(), 15_000)
  try {
    const response = await fetch(url, {
      method: 'POST',
      headers,
      body,
      redirect: 'error',
      signal: controller.signal,
    })
    const text = await response.text()
    const contentType = response.headers.get('content-type') ?? ''
    process.stdout.write(`status: ${response.status} ${response.statusText} (${Date.now() - started}ms)\n`)
    process.stdout.write(`content-type: ${contentType || '(none)'}\n`)
    process.stdout.write(`raw(${text.length}B): ${text.slice(0, 1200)}${text.length > 1200 ? '…' : ''}\n`)
    process.stdout.write(`parsed: ${JSON.stringify(summarize(text, contentType))}\n`)
  } catch (error) {
    process.stdout.write(`error: ${error?.name}: ${error?.message} (${Date.now() - started}ms)\n`)
  } finally {
    clearTimeout(timer)
  }
}

process.stdout.write(`probe target: ${url}\n`)
process.stdout.write(`state(challenge 侧可见): ${state}\n`)
process.stdout.write(`code_challenge: ${challenge}  (code_verifier 保留在内存，不打印)\n`)
process.stdout.write(`redirect_uri: ${redirectUri}\n`)

for (const variant of variants) await send(variant)

process.stdout.write('\n判定提示：full 成功 + no-client-headers 失败 ⇒ 五个客户端头是服务端硬校验；\n')
process.stdout.write('两者都成功 ⇒ 无 client_id 的公开客户端，任何 App 都能起授权（我们可直连）。\n')
