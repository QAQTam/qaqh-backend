#!/usr/bin/env node
/**
 * widget-directive §5 冒烟门：当前模型配置跑 N 轮可视化请求，测 marker 合规率。
 *
 * 忠实复刻 daemon 的 prompt 面（[Environment] widgets_dir + qaqh-visualize
 * bundled 正文），并带一个真 `write` 工具回路（fragment 落盘到临时
 * widgets_dir），直到模型 end_turn。合规判据（每轮全满足）：
 *   1) 最终正文含 ≥1 条 BLOCK marker（U+E200 visualize U+E202 {json} U+E201，
 *      JSON 可解析、path 以 .html 结尾且落在 widgets_dir 内）；
 *   2) marker 引用的 path 已被 write 工具真实落盘。
 *
 * 配置来源：~/.qaqh/providers.toml（base_url/model）+ secrets.toml（key）。
 * 用法：node probes/widget-marker-smoke.mjs [轮数=10]
 */
import { mkdirSync, readFileSync, existsSync, rmSync, writeFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { join } from "node:path";
import { tmpdir } from "node:os";

const HOME = process.env.USERPROFILE || process.env.HOME;
const providers = readFileSync(join(HOME, ".qaqh", "providers.toml"), "utf8");
const secrets = readFileSync(join(HOME, ".qaqh", "secrets.toml"), "utf8");

// —— 解析当前激活 provider（Qwen Token-Plan CN / wire=messages）——
const providerBlock = providers.split("[providers.").find((chunk) => chunk.startsWith("qwen-token-plan-cn-provider]"));
if (!providerBlock) throw new Error("providers.toml: qwen-token-plan-cn-provider 未找到");
const baseUrl = /base_url = "([^"]+)"/.exec(providerBlock)?.[1];
if (!baseUrl) throw new Error("base_url 未找到");
let apiKey = process.env.QAQH_SMOKE_KEY ?? null;
if (!apiKey) {
  const raw = /^qwen-token-plan-cn-provider = "([^"]+)"/m.exec(secrets)?.[1] ?? "";
  if (raw.startsWith("dpapi:")) {
    // Windows DPAPI（CurrentUser 域）：与 daemon 运行时同一把钥匙，经 powershell 解密。
    const script =
      "Add-Type -AssemblyName System.Security;" +
      `$b=[Convert]::FromBase64String('${raw.slice(6)}');` +
      "[Text.Encoding]::UTF8.GetString([System.Security.Cryptography.ProtectedData]::Unprotect($b,$null,[System.Security.Cryptography.DataProtectionScope]::CurrentUser))";
    const r = spawnSync("powershell.exe", ["-NoProfile", "-Command", script], { encoding: "utf8" });
    if (r.status !== 0 || !r.stdout.trim()) throw new Error("dpapi decrypt failed: " + String(r.stderr).slice(0, 200));
    apiKey = r.stdout.trim();
  } else {
    apiKey = raw || null;
  }
}
if (!apiKey) throw new Error("secrets.toml: provider key 未找到");
const model = process.env.QAQH_SMOKE_MODEL ?? "qwen3.8-flash";

const skillBody = readFileSync(
  join(import.meta.dirname, "..", "crates", "qaqh-skills", "assets", "bundled", "qaqh-visualize", "SKILL.md"),
  "utf8",
);

const workspace = join(tmpdir(), `qaqh-widget-smoke-${Date.now()}`);
const widgetsDir = join(workspace, ".qaqh", "widgets", "SES-SMOKE").replace(/\\/g, "/");
mkdirSync(widgetsDir, { recursive: true });

const system =
  "You are QAQ, an autonomous coding agent operating on the user's workspace.\n" +
  "Work through the available tools.\n\n" +
  "[Environment]\n" +
  `workspace: ${workspace}\n` +
  `widgets_dir: ${widgetsDir}\n` +
  `model: ${model}\n` +
  "tools: write\n\n" +
  "<available_skills>\n- qaqh-visualize (bundled, full text inlined below)\n</available_skills>\n\n" +
  skillBody;

const TASKS = [
  "帮我做一个交互式进度条 widget，可以拖动调整百分比，并把最终值回报给对话。",
  "做一个番茄钟 widget：25 分钟倒计时、开始/暂停/重置按钮，倒计时结束通过 follow-up 提醒。",
  "用 widget 画一个折线图，展示一周 7 天步数（数据你自己编），鼠标悬停显示数值。",
  "做一个开关面板 widget：三个开关（Wi-Fi/蓝牙/飞行模式），状态持久化，改完把状态表回报。",
  "做一个三步报名表单 widget（姓名、年龄、同意条款），校验后一键提交到对话。",
  "给我一个 BMI 计算器 widget：身高体重输入、即时计算、分级色条显示。",
  "做一个看板式任务板 widget：三列（待办/进行中/完成），卡片可跨列移动，移动后同步状态。",
  "做一个颜色选择器 widget：三个滑杆控制 RGB，实时预览色块，确认后把十六进制值回报。",
  "做一个交互式问卷 widget：5 道单选题，最后一屏汇总答案并 follow-up 提交。",
  "做一个可折叠树 widget：展示三层文件目录，可展开/收起节点，记住展开状态。",
];

const WRITE_TOOL = {
  name: "write",
  description: "Write a file (fragment) to an absolute path. Creates parent directories.",
  input_schema: {
    type: "object",
    properties: {
      path: { type: "string", description: "absolute file path" },
      content: { type: "string", description: "file content, literal" },
    },
    required: ["path", "content"],
  },
};

// 与 webui/src/widget/directive.ts 同一 BLOCK 文法（合规判定的唯一标准）。
const BLOCK_RE = /^(?: {0,3})\uE200\s*([a-z][a-z0-9_-]*)\s*\uE202\s*([^\uE201\r\n]*?)\s*\uE201[ \t]*(?:\r?\n|$)/gm;

async function askRound(task) {
  const messages = [{ role: "user", content: task }];
  const writtenPaths = new Set();
  for (let step = 0; step < 10; step += 1) {
    const response = await fetch(`${baseUrl.replace(/\/+$/, "")}/v1/messages`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "x-api-key": apiKey,
        // 兼容层要求 x-api-key 之外同时携带 Authorization: Bearer（同 qaqh-gate messages_sdk）。
        "Authorization": `Bearer ${apiKey}`,
        "anthropic-version": "2023-06-01",
      },
      body: JSON.stringify({ model, max_tokens: 16384, system, messages, tools: [WRITE_TOOL] }),
    });
    if (!response.ok) throw new Error(`provider ${response.status}: ${(await response.text()).slice(0, 300)}`);
    const json = await response.json();
    if (json.stop_reason === "max_tokens") {
      throw new Error(`stop=max_tokens（${json.content?.length} blocks，预算 16384 不够：本轮判 MISS`);
    }
    messages.push({ role: "assistant", content: json.content });
    const shapes = json.content.map((b) => b.type).join("+") || "EMPTY";
    console.log(`    step ${step}: stop=${json.stop_reason} blocks=${shapes}`);
    if (json.stop_reason !== "tool_use") {
      const text = json.content.filter((b) => b.type === "text").map((b) => b.text).join("\n");
      if (step > 0 && text.trim() === "") {
        // 有工具轮但终轮零文本：marker 没机会出现，如实记 MISS 而非静默。
        throw new Error(`empty final turn [stop=${json.stop_reason} blocks=${shapes}]`);
      }
      return { text, writtenPaths, shapes };
    }
    const results = json.content
      .filter((block) => block.type === "tool_use")
      .map((block) => {
        if (block.name === "write") {
          const target = String(block.input?.path ?? "");
          try {
            mkdirSync(join(target, ".."), { recursive: true });
            writeFileSync(target, String(block.input?.content ?? ""), "utf8");
            writtenPaths.add(target);
            return { type: "tool_result", tool_use_id: block.id, content: `wrote ${target}` };
          } catch (error) {
            return { type: "tool_result", tool_use_id: block.id, content: `write failed: ${error}`, is_error: true };
          }
        }
        return { type: "tool_result", tool_use_id: block.id, content: "unknown tool", is_error: true };
      });
    messages.push({ role: "user", content: results });
  }
  return { text: "", writtenPaths };
}

function normalizePath(p) {
  return p.replace(/\\/g, "/").toLowerCase();
}

function judge(text, writtenPaths) {
  BLOCK_RE.lastIndex = 0;
  const wanted = [...writtenPaths].map(normalizePath);
  let count = 0;
  for (const match of text.matchAll(BLOCK_RE)) {
    let payload;
    try {
      payload = JSON.parse(match[2]);
    } catch {
      continue;
    }
    const path = typeof payload.path === "string" ? payload.path : "";
    const normalized = normalizePath(path);
    const inWidgets = normalized.startsWith(normalizePath(widgetsDir) + "/");
    // slash-only 文法（skill 规则）：parse 后 path 里出现反斜杠 = 文法违规。
    const sane = !path.includes("\\");
    const written = wanted.includes(normalized) || existsSync(path);
    if (inWidgets && sane && normalized.endsWith(".html") && written) {
      count += 1;
    }
  }
  return count;
}

const total = Number(process.argv[2] ?? 10);
let compliant = 0;
const rows = [];
for (let i = 0; i < total && i < TASKS.length; i += 1) {
  process.stdout.write(`round ${i + 1}/${total}: ${TASKS[i].slice(0, 24)}… `);
  try {
    const { text, writtenPaths } = await askRound(TASKS[i]);
    const markers = judge(text, writtenPaths);
    const ok = markers >= 1;
    if (ok) compliant += 1;
    rows.push({ task: i + 1, markers, ok });
    console.log(ok ? `OK (${markers} marker)` : `MISS [final text ${text.length} chars]`);
    if (!ok) {
      const sample = text.replace(/[\uE200-\uE202]/g, (c) => `<U+${c.charCodeAt(0).toString(16).toUpperCase()}>`);
      console.log("  ── 末段正文 ──\n" + sample.slice(0, 600) + "\n  ──────────────");
    }
  } catch (error) {
    rows.push({ task: i + 1, markers: 0, ok: false, error: String(error).slice(0, 200) });
    console.log("ERROR", String(error).slice(0, 160));
  }
}
const rate = (compliant / Math.min(total, TASKS.length)) * 100;
console.log(`\nmarker 合规率: ${compliant}/${Math.min(total, TASKS.length)} = ${rate.toFixed(0)}%（门: >=80%）`);
rmSync(workspace, { recursive: true, force: true });
process.exit(rate >= 80 ? 0 : 1);
