// M0 daemon 鉴权/配对 端到端验证（临时脚本，非仓库资产）。
// 用法：NODE_TLS_REJECT_UNAUTHORIZED=0 ADMIN_TOKEN=xxx node e2e-m0.mjs
const BASE = process.env.BASE || 'https://127.0.0.1:64413';
const ADMIN = process.env.ADMIN_TOKEN;

let pass = 0;
let fail = 0;
function check(name, cond, extra = '') {
  if (cond) {
    pass++;
    console.log(`PASS  ${name}${extra ? '  ' + extra : ''}`);
  } else {
    fail++;
    console.log(`FAIL  ${name}${extra ? '  ' + extra : ''}`);
  }
}

async function req(path, { method = 'GET', token, body, headers = {} } = {}) {
  const h = { ...headers };
  if (token) h['authorization'] = `Bearer ${token}`;
  if (body !== undefined) h['content-type'] = 'application/json';
  const res = await fetch(BASE + path, {
    method,
    headers: h,
    body: body !== undefined ? JSON.stringify(body) : undefined,
  });
  const text = await res.text();
  let json = null;
  try {
    json = text ? JSON.parse(text) : null;
  } catch {
    json = text;
  }
  return { status: res.status, json };
}

const openBody = (instance) => ({ schema: 'qaqh.Ringing', version: 2, client_instance_id: instance });

async function pairAndOpen(scope, label) {
  const t = await req('/ringing/v2/pairing/tokens', {
    method: 'POST',
    token: ADMIN,
    body: { scope_grant: scope, device_name: label, platform: 'node' },
  });
  if (t.status !== 200) return { error: `pairing/tokens ${t.status}` };
  const p = await req('/ringing/v2/pair', {
    method: 'POST',
    body: { pairing_token: t.json.pairing_token, device_name: label, platform: 'node' },
  });
  if (p.status !== 200) return { error: `pair ${p.status}` };
  const o = await req('/ringing/v2/clients/open', {
    method: 'POST',
    token: p.json.device_token,
    body: openBody(`spoof-${label}`),
  });
  if (o.status !== 200) return { error: `open ${o.status}` };
  return {
    device_token: p.json.device_token,
    device_id: p.json.device_id,
    scope: p.json.scope,
    cs: o.json.client_session_id,
    open: o.json,
  };
}

// ── 1. health / 免鉴权 ─────────────────────────────────────────────
let r = await req('/health');
check('1  /health 免鉴权 200', r.status === 200, `status=${r.status}`);

// ── 2. 配对令牌签发 ───────────────────────────────────────────────
r = await req('/ringing/v2/pairing/tokens', {
  method: 'POST',
  token: ADMIN,
  body: { scope_grant: 'view', device_name: 'e2e-view', platform: 'node' },
});
check('2  pairing/tokens (admin) 200', r.status === 200, `status=${r.status}`);
const ptView = r.json?.pairing_token;
const tlsFp = r.json?.tls_fp;
check('2a 返回 pairing_token + tls_fp(sha256:)', !!ptView && typeof tlsFp === 'string' && tlsFp.startsWith('sha256:'), `fp=${tlsFp}`);
check('2b 无 Bearer 调 pairing/tokens 401', (await req('/ringing/v2/pairing/tokens', { method: 'POST', body: {} })).status === 401);

// ── 3. 配对换取设备凭证（view）────────────────────────────────────
r = await req('/ringing/v2/pair', { method: 'POST', body: { pairing_token: ptView, device_name: 'e2e-view', platform: 'node' } });
check('3  /pair 200', r.status === 200, `status=${r.status}`);
const devView = r.json;
check('3a scope=view 且下发 device_token', devView?.scope === 'view' && typeof devView?.device_token === 'string' && devView.device_token.length > 0);

// ── 4. 令牌一次性 ────────────────────────────────────────────────
r = await req('/ringing/v2/pair', { method: 'POST', body: { pairing_token: ptView } });
check('4  重放 pairing_token → 403 pairing_used', r.status === 403 && r.json?.code === 'pairing_used', `status=${r.status} code=${r.json?.code}`);
r = await req('/ringing/v2/pair', { method: 'POST', body: { pairing_token: 'bogus' } });
check('4a 伪造 pairing_token → 403 pairing_invalid', r.status === 403 && r.json?.code === 'pairing_invalid', `code=${r.json?.code}`);

// ── 5. device open（身份由 token 反推）────────────────────────────
r = await req('/ringing/v2/clients/open', { method: 'POST', token: devView.device_token, body: openBody('spoofed-instance-should-be-ignored') });
check('5  device open 200', r.status === 200, `status=${r.status}`);
const csView = r.json?.client_session_id;
check('5a capabilities.pairing=true', r.json?.capabilities?.pairing === true, JSON.stringify(r.json?.capabilities));

// ── 6. 设备管理（admin）──────────────────────────────────────────
r = await req('/ringing/v2/devices', { token: ADMIN });
check('6  devices 列表含该设备', r.status === 200 && r.json?.devices?.some?.((d) => d.device_id === devView.device_id), `n=${r.json?.devices?.length}`);
check('6a 列表不含任何 token 明文', !JSON.stringify(r.json).includes(devView.device_token));

// ── 7. view token 越权矩阵 ───────────────────────────────────────
const hdr = { 'x-qaqh-client-session-id': csView };
r = await req('/ringing/v2/sessions/seed-e2e/bootstrap', { token: devView.device_token, headers: hdr });
check('7  view 读未 attach 会话 bootstrap → 403 forbidden_not_owner', r.status === 403 && r.json?.code === 'forbidden_not_owner', `status=${r.status} code=${r.json?.code}`);
r = await req('/ringing/v2/sessions/seed-e2e/approvals', { token: devView.device_token, headers: hdr });
check('7a view 读未 attach 会话 approvals → 403', r.status === 403, `status=${r.status}`);
r = await req('/ringing/v2/commands/control', { method: 'POST', token: devView.device_token, headers: hdr, body: {} });
check('7b view 发命令(畸形体) → 400 invalid_body', r.status === 400, `status=${r.status}`);
const viewEnv = (cmd) => ({
  schema: 'qaqh.Ringing', version: 2, channel: 'control', command_id: `cmd-${Math.random().toString(36).slice(2)}`,
  client_instance_id: 'inst', client_session_id: csView, session_id: 'seed-e2e', command: cmd,
});
r = await req('/ringing/v2/commands/control', { method: 'POST', token: devView.device_token, headers: hdr, body: viewEnv({ channel: 'control', type: 'session_close', session_id: 'seed-e2e' }) });
check('7b2 view 发非 attach 命令 → 403 insufficient_scope', r.status === 403 && r.json?.code === 'insufficient_scope', `status=${r.status} code=${r.json?.code}`);
r = await req('/ringing/v2/commands/control', { method: 'POST', token: devView.device_token, headers: hdr, body: viewEnv({ channel: 'control', type: 'session_attach', session_id: 'seed-e2e' }) });
check('7b3 view 可 SessionAttach（仅建立归属）→ 200', r.status === 200, `status=${r.status} code=${r.json?.code}`);
r = await req('/ringing/v2/sessions/seed-e2e/bootstrap', { token: devView.device_token, headers: hdr });
check('7b4 attach 后 view 读 bootstrap 不再 403', r.status !== 403 && r.status !== 401, `status=${r.status}`);

// ── 8. admin 豁免（越权检查不误伤桌面侧）──────────────────────────
r = await req('/ringing/v2/clients/open', { method: 'POST', token: ADMIN, body: openBody('admin-shell') });
const csAdmin = r.json?.client_session_id;
r = await req('/ringing/v2/sessions/seed-e2e/bootstrap', { token: ADMIN, headers: { 'x-qaqh-client-session-id': csAdmin } });
check('8  admin 读任意会话 bootstrap 非 403/401', r.status !== 403 && r.status !== 401, `status=${r.status} code=${r.json?.code}`);

// ── 9. interact 设备：attach 建立归属 ────────────────────────────
const int = await pairAndOpen('interact', 'e2e-interact');
check('9  interact 设备配对+open 成功', !int.error, int.error || `device=${int.device_id}`);
const hdrInt = { 'x-qaqh-client-session-id': int.cs };
r = await req('/ringing/v2/sessions/seed-e2e/bootstrap', { token: int.device_token, headers: hdrInt });
check('9a attach 前读 bootstrap → 403', r.status === 403, `status=${r.status}`);
const envelope = {
  schema: 'qaqh.Ringing',
  version: 2,
  channel: 'control',
  command_id: 'cmd-attach-1',
  client_instance_id: 'dev-int',
  client_session_id: int.cs,
  session_id: 'seed-e2e',
  command: { channel: 'control', type: 'session_attach', session_id: 'seed-e2e' },
};
r = await req('/ringing/v2/commands/control', { method: 'POST', token: int.device_token, headers: hdrInt, body: envelope });
check('9b SessionAttach 被接受(200/ack)', r.status === 200, `status=${r.status} body=${JSON.stringify(r.json)?.slice(0, 120)}`);
r = await req('/ringing/v2/sessions/seed-e2e/bootstrap', { token: int.device_token, headers: hdrInt });
check('9c attach 后读 bootstrap 不再 403（归属已建立）', r.status !== 403 && r.status !== 401, `status=${r.status} code=${r.json?.code}`);

// ── 10. 吊销 ─────────────────────────────────────────────────────
r = await req(`/ringing/v2/devices/${devView.device_id}/revoke`, { method: 'POST', token: ADMIN });
check('10 吊销设备 204', r.status === 204, `status=${r.status}`);
r = await req('/ringing/v2/sessions/seed-e2e/bootstrap', { token: devView.device_token, headers: hdr });
check('10a 吊销后 device_token 一切请求 401', r.status === 401, `status=${r.status}`);

console.log(`\n==== ${pass} passed, ${fail} failed ====`);
process.exit(fail === 0 ? 0 : 1);
