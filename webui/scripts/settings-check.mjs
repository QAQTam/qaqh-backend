/**
 * 设置浮层的浏览器验收(开发工具,不进产品构建)。
 *
 * 跑的是真 `App` + 真设置组件 + 假宿主 IPC(`src/settings-check.tsx`):
 * 断言的是「webview 到底往外发了什么 patch」,而不是组件长什么样。
 *
 * 用法:
 *   pnpm exec vite --port 5173            # 另开一个终端
 *   pnpm exec node scripts/settings-check.mjs
 */
import { spawn } from "node:child_process";
import { mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const EDGE = process.env.QAQH_EDGE ?? "C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe";
const PORT = Number(process.env.QAQH_CDP_PORT ?? "9334");
const TARGET = process.env.SETTINGS_URL ?? `http://127.0.0.1:5173/settings-check.html`;
const SHOT = join(dirname(fileURLToPath(import.meta.url)), "..", "out", "settings-check.png");
const SHOT_BOTTOM = join(dirname(fileURLToPath(import.meta.url)), "..", "out", "settings-check-bottom.png");

const exceptions = [];
const consoleErrors = [];

async function waitForTarget(timeoutMs = 40_000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      const list = await fetch(`http://127.0.0.1:${PORT}/json/list`).then((res) => res.json());
      const page = list.find((item) => item.type === "page" && item.url === TARGET);
      if (page?.webSocketDebuggerUrl) return page.webSocketDebuggerUrl;
    } catch {
      /* 浏览器还没起来 */
    }
    await new Promise((resolve) => setTimeout(resolve, 250));
  }
  throw new Error(`CDP target not found: ${TARGET}(vite 起没起?)`);
}

function connect(url) {
  const ws = new WebSocket(url);
  const pending = new Map();
  let nextId = 0;
  ws.addEventListener("message", (event) => {
    const msg = JSON.parse(event.data);
    if (msg.id && pending.has(msg.id)) {
      pending.get(msg.id).resolve(msg);
      pending.delete(msg.id);
      return;
    }
    if (msg.method === "Runtime.exceptionThrown") {
      const detail = msg.params?.exceptionDetails;
      exceptions.push(detail?.exception?.description ?? detail?.text ?? "exception");
    }
    if (msg.method === "Runtime.consoleAPICalled" && msg.params?.type === "error") {
      consoleErrors.push((msg.params.args ?? []).map((arg) => arg.value ?? arg.description ?? "").join(" "));
    }
  });
  const open = new Promise((resolve, reject) => {
    ws.addEventListener("open", resolve, { once: true });
    ws.addEventListener("error", () => reject(new Error("CDP socket error")), { once: true });
  });
  const send = (method, params = {}) =>
    new Promise((resolve, reject) => {
      const id = ++nextId;
      pending.set(id, { resolve: (msg) => (msg.error ? reject(new Error(`${method}: ${msg.error.message}`)) : resolve(msg.result)) });
      ws.send(JSON.stringify({ id, method, params }));
    });
  return {
    open,
    send,
    async evaluate(expression, timeoutMs = 30_000) {
      for (let attempt = 0; ; attempt += 1) {
        try {
          const result = await Promise.race([
            send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true }),
            new Promise((_, reject) => setTimeout(() => reject(new Error(`evaluate 超时:${expression.slice(0, 60)}`)), timeoutMs)),
          ]);
          if (result.exceptionDetails) {
            throw new Error(result.exceptionDetails.exception?.description ?? result.exceptionDetails.text);
          }
          return result.result?.value;
        } catch (error) {
          if (/Execution context was destroyed|Cannot find context/i.test(String(error?.message)) && attempt < 4) {
            await new Promise((resolve) => setTimeout(resolve, 2_000));
            continue;
          }
          throw error;
        }
      }
    },
  };
}

const BOOTSTRAP = `
window.__t = {
  wait: (ms) => new Promise((r) => setTimeout(r, ms)),
  modal: () => document.querySelector(".settings-modal"),
  field: (label) => [...document.querySelectorAll(".field")]
    .find((item) => item.querySelector(".field-label")?.textContent === label)
    ?.querySelector("input,select"),
  value: (label) => window.__t.field(label)?.value ?? null,
  type: (label, next) => {
    const el = window.__t.field(label);
    el.value = next;
    el.dispatchEvent(new Event("input", { bubbles: true }));
  },
  pick: (label, next) => {
    const el = window.__t.field(label);
    el.value = next;
    el.dispatchEvent(new Event("change", { bubbles: true }));
  },
  click: (sel) => document.querySelector(sel)?.click(),
  text: (sel) => document.querySelector(sel)?.innerText ?? "",
  key: (key, opts = {}) => window.dispatchEvent(new KeyboardEvent("keydown", { key, bubbles: true, ...opts })),
  calls: (method) => window.__qaqhProbe.calls.filter((item) => item.method === method),
  saves: () => window.__qaqhProbe.saves,
  lastSave: () => window.__qaqhProbe.saves[window.__qaqhProbe.saves.length - 1] ?? null,
  foot: () => document.querySelector(".settings-foot-state")?.innerText ?? "",
};
"ok";
`;

const steps = [
  {
    name: "Ctrl+, 打开浮层并读一次 config.load",
    code: `
      __t.key(",", { ctrlKey: true });
      await __t.wait(600);
      // 2026-10-06 起宿主启动时会多拉一次 config.load(主题预取),故 ≥1。
      return { ok: __t.modal() != null && __t.calls("config.load").length >= 1,
               detail: { modal: __t.modal() != null, loads: __t.calls("config.load").length } };
    `,
  },
  {
    name: "全量字段落位(读模型逐字段进表单)",
    code: `
      const sections = document.querySelectorAll(".settings-section").length;
      const inputs = document.querySelectorAll(".settings-body input, .settings-body select").length;
      const readonlyTables = document.querySelectorAll(".readonly-table").length;
      // 2026-10-06 起 8 个分区:BYOK/档案/权限/上下文/外观/子代理/设备配对/MCP-LSP。
      return { ok: sections === 8 && inputs >= 26 && readonlyTables === 2
                 && __t.value("model") === "ox-alpha-free" && __t.value("maxTokens") === "96000",
               detail: { sections, inputs, readonlyTables, model: __t.value("model"), maxTokens: __t.value("maxTokens") } };
    `,
  },
  {
    name: "改 model 只发这一项(Merge Patch 最小化)",
    code: `
      __t.type("model", "ox-beta");
      await __t.wait(200);
      const foot = __t.foot();
      __t.click(".primary-mini");
      await __t.wait(600);
      const patch = __t.lastSave();
      return { ok: foot.includes("1 项") && JSON.stringify(Object.keys(patch ?? {})) === '["model"]'
                 && patch?.model === "ox-beta" && __t.value("model") === "ox-beta",
               detail: { foot, patchKeys: Object.keys(patch ?? {}), saved: __t.saves().length } };
    `,
  },
  {
    name: "清空 model 不产生改动(后端「空串 = 保持现值」)",
    code: `
      __t.type("model", "");
      await __t.wait(200);
      return { ok: __t.foot().includes("没有改动") && __t.saves().length === 1, detail: { foot: __t.foot(), saves: __t.saves().length } };
    `,
  },
  {
    name: "密钥框:掩码不回填、填了新值才发",
    code: `
      const placeholder = __t.field("API key").placeholder;
      __t.type("API key", "sk-fixture-new");
      await __t.wait(200);
      const foot = __t.foot();
      __t.click(".primary-mini");
      await __t.wait(600);
      return { ok: placeholder.includes("已配置") && foot.includes("1 项") && __t.lastSave()?.apiKey === "sk-fixture-new",
               detail: { placeholder, foot, apiKey: __t.lastSave()?.apiKey } };
    `,
  },
  {
    name: "BYOK 端点改动:endpoint + wire 各发一项",
    code: `
      __t.pick("wire", "anthropic");
      await __t.wait(250);
      __t.type("endpoint", "https://other.example/v1");
      await __t.wait(250);
      const foot = __t.foot();
      const wire = __t.value("wire");
      const endpoint = __t.value("endpoint");
      __t.click(".primary-mini");
      await __t.wait(600);
      const patch = __t.lastSave() ?? {};
      return { ok: foot.includes("2 项") && wire === "anthropic" && endpoint === "https://other.example/v1"
                 && patch.wire === "anthropic" && patch.baseUrl === "https://other.example/v1",
               detail: { foot, wire, endpoint, patchKeys: Object.keys(patch) } };
    `,
  },
  {
    name: "档位降到 3:先拦下,打字确认后才写",
    code: `
      const before = __t.saves().length;
      __t.click('input[name="permission-tier"][value="3"]');
      await __t.wait(200);
      __t.click(".primary-mini");
      await __t.wait(400);
      const blocked = document.querySelector(".bypass-confirm") != null && __t.saves().length === before;
      return { ok: blocked, detail: { blocked, confirmShown: document.querySelector(".bypass-confirm") != null } };
    `,
  },
  {
    name: "确认后写入 permissionLevel=3",
    code: `
      const ack = document.querySelector(".bypass-ack");
      ack.value = "3";
      ack.dispatchEvent(new Event("input", { bubbles: true }));
      await __t.wait(200);
      __t.click(".primary-mini");
      await __t.wait(600);
      return { ok: __t.lastSave()?.permissionLevel === 3, detail: { patch: __t.lastSave() } };
    `,
  },
  {
    name: "脏草稿下拒绝对 profile 动手(不静默覆盖草稿)",
    code: `
      __t.type("model", "dirt-while-profile");
      await __t.wait(200);
      const before = __t.calls("profile.delete").length;
      // 目标必须是非活动 profile:活动档的删除按钮本来就 disabled。
      document.querySelector(".profile-row:not(.active) .ghost-mini.is-danger")?.click();
      await __t.wait(400);
      const foot = __t.foot();
      return { ok: foot.includes("有未保存的改动") && __t.calls("profile.delete").length === before,
               detail: { foot, deletes: __t.calls("profile.delete").length } };
    `,
  },
  {
    name: "重新读取:丢掉草稿并回到基线",
    code: `
      const loads = __t.calls("config.load").length;
      [...document.querySelectorAll(".settings-foot-actions .ghost-mini")].find((b) => b.textContent.includes("重新读取"))?.click();
      await __t.wait(600);
      return { ok: __t.calls("config.load").length > loads && __t.value("model") === "ox-beta",
               detail: { model: __t.value("model"), loads: __t.calls("config.load").length } };
    `,
  },
  {
    name: "Esc 关闭遇脏草稿 → 先问放弃/留下",
    code: `
      __t.type("model", "esc-dirty");
      await __t.wait(200);
      __t.key("Escape");
      await __t.wait(300);
      const asked = document.querySelector(".settings-discard") != null && __t.modal() != null;
      [...document.querySelectorAll(".settings-discard .ghost-mini")].find((b) => b.textContent.includes("继续编辑"))?.click();
      await __t.wait(200);
      const stayed = __t.modal() != null && __t.value("model") === "esc-dirty";
      [...document.querySelectorAll(".settings-foot-actions .ghost-mini")].find((b) => b.textContent.includes("重新读取"))?.click();
      await __t.wait(600);
      __t.key("Escape");
      await __t.wait(300);
      [...document.querySelectorAll(".settings-discard .ghost-mini")].find((b) => b.textContent.includes("放弃并关闭"))?.click();
      await __t.wait(300);
      return { ok: asked && stayed && __t.modal() == null, detail: { asked, stayed, closed: __t.modal() == null } };
    `,
  },
  {
    name: "授权卡压在遮罩之上(超时不被设置页吃掉)",
    code: `
      // 真 #approval-slot 只在有待处理授权时挂载(零会话夹具里没有),
      // 所以插一个同 id 的探针元素来验规则本身是否进了样式表。
      const probe = document.createElement("div");
      probe.id = "approval-slot";
      document.body.appendChild(probe);
      const style = getComputedStyle(probe);
      const got = { zIndex: style.zIndex, position: style.position };
      probe.remove();
      return { ok: got.zIndex === "45" && got.position === "relative", detail: got };
    `,
  },
  {
    name: "零会话也能开设置(宿主对 config.* 不注入 seed)",
    code: `
      const bootErrorShown = document.querySelector(".boot-error") != null;
      __t.key(",", { ctrlKey: true });
      await __t.wait(600);
      const loads = __t.calls("config.load");
      return { ok: bootErrorShown && __t.modal() != null && loads.length >= 1 && loads.every((c) => c.params && Object.keys(c.params).length === 0),
               detail: { bootErrorShown, opened: __t.modal() != null, params: loads.map((c) => c.params) } };
    `,
  },
];

const child = spawn(
  EDGE,
  [
    "--headless=new",
    `--remote-debugging-port=${PORT}`,
    `--user-data-dir=${mkdtempSync(join(tmpdir(), "qaqh-settings-"))}`,
    "--window-size=1280,1000",
    "--disable-gpu",
    "--no-first-run",
    "--disable-backgrounding-occluded-windows",
    "--disable-background-timer-throttling",
    TARGET,
  ],
  { stdio: "ignore" },
);

let failed = 0;
try {
  const wsUrl = await waitForTarget();
  const cdp = connect(wsUrl);
  await cdp.open;
  await cdp.send("Runtime.enable");
  await cdp.send("Page.enable");

  // vite 首次请求会补优化依赖并整页 reload → 注入过的 helper 会跟着没了。
  // 所以每步之前确认 helper 在,不在就重注入(探针 `__qaqhProbe` 由页面自己装)。
  const ensure = async () => {
    for (let attempt = 0; attempt < 60; attempt += 1) {
      const state = await cdp
        .evaluate(`(() => ({ probe: !!window.__qaqhProbe, helper: !!window.__t }))()`, 10_000)
        .catch(() => ({ probe: false, helper: false }));
      if (state.probe && state.helper) return;
      if (state.probe && !state.helper) {
        await cdp.evaluate(BOOTSTRAP);
        continue;
      }
      await new Promise((resolve) => setTimeout(resolve, 500));
    }
    throw new Error("夹具页面未就绪(__qaqhProbe 缺失)");
  };

  await ensure();
  await cdp.evaluate(`(async () => { await window.__t.wait(1_200); return "ready"; })()`);

  const results = [];
  for (const step of steps) {
    await ensure();
    const value = await cdp.evaluate(`(() => { const __t = window.__t; return (async () => {${step.code}})(); })()`);
    const ok = value?.ok === true;
    if (!ok) failed += 1;
    results.push({ name: step.name, ok, detail: value?.detail });
    console.log(`${ok ? "PASS" : "FAIL"}  ${step.name}`);
    if (!ok) console.log(`      ${JSON.stringify(value?.detail)}`);
  }

  // 截图:重新打开浮层,停在有改动状态。
  await ensure();
  await cdp.evaluate(`(() => { const __t = window.__t; return (async () => {
    if (__t.modal() == null) __t.key(",", { ctrlKey: true });
    await __t.wait(700);
    __t.type("model", "ox-two"); __t.pick("theme", "dark");
    document.querySelector(".settings-body").scrollTop = 0;
    await __t.wait(400); return "shot-ready";
  })(); })()`);
  mkdirSync(dirname(SHOT), { recursive: true });
  const shot = await cdp.send("Page.captureScreenshot", { format: "png" });
  writeFileSync(SHOT, Buffer.from(shot.data, "base64"));
  // 第二张:滚到底,看子代理/profile/MCP 只读区。
  await cdp.evaluate(`(() => { const __t = window.__t; return (async () => {
    const body = document.querySelector(".settings-body");
    body.scrollTop = body.scrollHeight;
    await __t.wait(300);
    return document.querySelector(".settings-foot-state")?.innerText ?? "";
  })(); })()`);
  const bottom = await cdp.send("Page.captureScreenshot", { format: "png" });
  writeFileSync(SHOT_BOTTOM, Buffer.from(bottom.data, "base64"));

  const noise = [...exceptions, ...consoleErrors];
  console.log(`\n结果:${steps.length - failed}/${steps.length} 通过;截图 ${SHOT}`);
  if (noise.length > 0) {
    console.log(`页面异常/控制台报错 ${noise.length} 条:`);
    for (const line of noise.slice(0, 6)) console.log(`  ${line.split("\n")[0]}`);
    failed += 1;
  }
} catch (error) {
  failed += 1;
  console.error(`驱动失败:${error?.message ?? error}`);
} finally {
  child.kill();
}
process.exit(failed === 0 ? 0 : 1);
