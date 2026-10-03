/**
 * 合成压力夹具(只给 dev 用:`pnpm exec vite` 起服务后打开 /stress.html)。
 *
 * 目的:在没有 daemon、没有 TUI 的情况下验证 spec §14 的分页与窗口淘汰、快照重对齐
 * 的 churn,以及大文本 + 重 Markdown 下的 DOM 规模。走的是真组件与真 store:只有
 * `transport.timelinePage` 被挂了一个内存假页,不写任何后端、不发任何命令。
 */
import { render } from "@solidjs/web";
import { untrack } from "solid-js";
import { SessionStore } from "./session/store";
import { applyEntry } from "./session/reducer";
import { SessionView } from "./session/SessionView";
import { transport } from "./lib/transport";
import type { Tab } from "./tabs/store";
import "./styles/app.css";

/** 会话总回合数(权威 total_turns)。 */
const TOTAL = 200;
/** 假页水位;实时条目序号从 liveSeq 往后走。 */
let watermark = 2000;
let liveSeq = 2001;
/** 翻页请求记录(测分页是否真在跑)。 */
let pageRequests: Array<{ limit: number; beforeIndex: number | null }> = [];

/** 确定性伪随机:同一 index 每次生成同样的内容,重对齐才比得出 churn。 */
function mulberry32(seedValue: number): () => number {
  let a = seedValue >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

const SENTENCES = [
  "系统把这条时间线按 seq 单调追加，前端只做归约不做推断。",
  "水位一旦缺口就整表校正一次，校正按两秒防抖，不连环拉。",
  "折叠容器收起后要卸载子树，常驻内存才不会只增不减。",
  "列表按稳定 key 挂载，权威重对齐才不会把整屏连滚动位置一起丢掉。",
  "回合的耗时是前端时钟采样的，后端条目不带 epoch-ms。",
];

function prose(rand: () => number, chars: number): string {
  const lines: string[] = [];
  let size = 0;
  while (size < chars) {
    const line = SENTENCES[Math.floor(rand() * SENTENCES.length)]!;
    lines.push(line);
    size += line.length;
  }
  return lines.join("\n");
}

/** 重 Markdown:标题/段落/有序无序列表/表格/引用/两种语言围栏/行内混排。 */
function markdownOf(rand: () => number, index: number, chars: number): string {
  const parts: string[] = [
    `## 第 ${index} 答：结论与依据`,
    "",
    prose(rand, Math.floor(chars * 0.3)),
    "",
    "- 要点一：窗口淘汰只换占位不删数据的话，翻页越深攒下的全量文本越多",
    "- 要点二：`Object.values(turns)` 挂在 UI 热路径上会让整条链每帧重算",
    "  - 子项：派生答案要倒排成按 id 取键的单值",
    "",
    "1. 先测再改",
    "2. 再测一次对账",
    "3. 把结论钉成回归门",
    "",
    "| 指标 | 修复前 | 修复后 | 口径 |",
    "| --- | ---: | ---: | --- |",
    "| DOM 节点 | 17213 | 1263 | DEV |",
    "| 挂载字符 | 804220 | 118080 | DEV |",
    "| 每帧工作量 | 24ms | 0.2ms | 长任务 0 |",
    "| JS 堆 | 223MB | 27MB | usedJSHeap |",
    "",
    "> 注意：`.turn` 带 `content-visibility: auto`，离屏回合按估算高计入滚动范围，",
    "> 一次 `scrollTop = scrollHeight` 会被夹住，贴底要按帧推进。",
    "",
    "```ts",
    "const keyed = (slot: Slot): string => slot.key;",
    "for (const raw of page.snapshot?.turns ?? []) {",
    "  const turn = buildTurn(raw, ts); // 结构与顺序事实只来自后端",
    "  installTurns(draft, [turn]);",
    "}",
    "```",
    "",
    "```rust",
    "pub fn page_plan(before_index: Option<u64>, limit: usize, total: usize, window_base: usize) -> PagePlan {",
    "    let end = before_index.unwrap_or(total).min(total);",
    "    PagePlan { start: end.saturating_sub(limit), end, from_archive: end < window_base + limit }",
    "}",
    "```",
    "",
    "内联代码 `draft.turns = {}` 与链接 [reducer](https://example.com/session/reducer.ts) 混排，",
    "粗体 **保住原对象**、斜体 *内容变了才换*、删除线 ~~整表重建~~ 都要走到。",
    "",
    "---",
    "",
    prose(rand, Math.floor(chars * 0.4)),
    "",
  ];
  return parts.join("\n");
}

function ansiLog(rand: () => number, lines: number): string {
  const out: string[] = [];
  for (let i = 0; i < lines; i += 1) {
    const color = rand() < 0.25 ? "\u001b[32m" : rand() < 0.4 ? "\u001b[31m" : "\u001b[0m";
    out.push(`${color}   ${String(i).padStart(4, "0")} | running test ${i} … ${rand() < 0.5 ? "ok" : "FAILED"}`);
  }
  return out.join("\n");
}

/** 一半带 `diff --git` 头,一半只有裸 unified —— 用来验解析器对两种形态的实际表现。 */
function unifiedDiff(index: number, withGitHeader: boolean, files: number): string {
  const chunks: string[] = [];
  for (let f = 0; f < files; f += 1) {
    const path = `crates/qaqh-runtime/src/session_${index}_${f}.rs`;
    if (withGitHeader) {
      chunks.push(`diff --git a/${path} b/${path}`, "index 1111111..2222222 100644", `--- a/${path}`, `+++ b/${path}`);
    } else {
      chunks.push(`--- a/${path}`, `+++ b/${path}`);
    }
    chunks.push("@@ -1,6 +1,9 @@");
    for (let line = 0; line < 6; line += 1) chunks.push(` 保留行 ${line}: let x = ${line} + ${index};`);
    for (let line = 0; line < 3; line += 1) chunks.push(`+新增行 ${line}: draft.turns[known] = buildTurn(raw, ts);`);
    chunks.push(`-删除行 ${f}: draft.turns = {};`);
  }
  return chunks.join("\n");
}

function rawTurnAt(index: number): Record<string, unknown> {
  const rand = mulberry32(9_000 + index);
  const withHeader = index % 2 === 0;
  const blocks: Array<Record<string, unknown>> = [
    { block_id: `r${index}`, kind: "reasoning", state: "sealed", text: prose(rand, 1_400 + Math.floor(rand() * 900)) },
    {
      block_id: `e${index}`,
      kind: "tool",
      state: "sealed",
      tool: {
        name: "exec",
        state: "succeeded",
        args_json: JSON.stringify({ command: "cargo test -p qaqh-runtime --jobs 8 -- --nocapture" }),
        display: {
          summary: "exec: cargo test -p qaqh-runtime",
          header: { kind: "shell" },
          body: { kind: "streams", stdout: ansiLog(rand, 200), stderr: index % 5 === 0 ? "warning: 1 unreached match arm" : "", exit_code: 0 },
          metrics: { elapsed_ms: 1_800 + Math.floor(rand() * 900) },
        },
        metrics: { elapsed_ms: 1_800 + Math.floor(rand() * 900) },
        progress: ansiLog(rand, 260),
      },
    },
    {
      block_id: `d${index}`,
      kind: "tool",
      state: "sealed",
      tool: {
        name: "edit",
        state: index % 7 === 3 ? "failed" : "succeeded",
        args_json: JSON.stringify({ path: `webui/src/session/reducer_${index}.ts`, edits: 3 }),
        display: {
          summary: `edit webui/src/session/reducer_${index}.ts`,
          header: { kind: "diff" },
          body: { kind: "diff", unified: unifiedDiff(index, withHeader, 2), truncated: false },
        },
        metrics: { elapsed_ms: 40 + Math.floor(rand() * 60) },
        ...(index % 7 === 3 ? { failure: { code: "apply_failed", message: "锚点未唯一匹配,已回滚" } } : {}),
      },
    },
    {
      block_id: `n${index}`,
      kind: "tool",
      state: "sealed",
      tool: {
        name: "read",
        state: "succeeded",
        args_json: JSON.stringify({ path: "crates/qaqh-runtime/src/timeline.rs", limit: 2_000 }),
        display: {
          summary: "read crates/qaqh-runtime/src/timeline.rs",
          header: { kind: "text" },
          body: { kind: "text", text: prose(rand, 6_000 + Math.floor(rand() * 4_000)), truncated: index % 3 === 0 },
        },
        metrics: { elapsed_ms: 12 },
      },
    },
    { block_id: `x${index}`, kind: "text", state: "sealed", text: markdownOf(rand, index, 7_000 + Math.floor(rand() * 3_000)) },
  ];
  return {
    turn_id: `t${index}`,
    turn_index: index,
    user_text: `第 ${index} 问：${prose(rand, 90)}`,
    state: index % 7 === 3 ? "failed" : "completed",
    rounds: [{ round_num: 0, blocks }],
  };
}

function pageFor(query: string): Record<string, unknown> {
  const params = new URLSearchParams(query.startsWith("?") ? query.slice(1) : query);
  const limit = Number(params.get("limit") ?? "50");
  const beforeRaw = params.get("before_index");
  const beforeIndex = beforeRaw == null ? null : Number(beforeRaw);
  pageRequests.push({ limit, beforeIndex });
  // 权威页水位 = 服务端此刻真实写到的位置(与生产一致:它跟着流前进)。
  const pageWatermark = Math.max(watermark, liveSeq - 1);
  const end = Math.min(TOTAL, beforeIndex ?? TOTAL);
  const start = Math.max(0, end - limit);
  const turns: Array<Record<string, unknown>> = [];
  for (let index = start; index < end; index += 1) turns.push(rawTurnAt(index));
  return {
    schema: "qaqh.Ringing",
    version: 2,
    server_epoch: "stress-epoch",
    session_id: "stress",
    snapshot: { watermark: pageWatermark, turns },
    has_more: start > 0,
    total_turns: TOTAL,
    truncated_before: false,
  };
}

// 假页挂载点:store 只通过 transport.timelinePage 取权威数据。延迟 60ms 模拟一次
// 真实往返——不然前插在同一帧内就落地,滚动补偿根本来不及量。
(transport as unknown as { timelinePage: (seed: string, query?: string) => Promise<unknown> }).timelinePage = async (
  _seed: string,
  query = "",
): Promise<unknown> => {
  await new Promise((resolve) => setTimeout(resolve, 60));
  return pageFor(query);
};

const store = new SessionStore("stress");
const tab: Tab = { id: "tab-stress", seed: "stress", store };

function messagesEl(): HTMLElement {
  return document.getElementById("messages") as HTMLElement;
}

// ── 测量 ─────────────────────────────────────────────────────────────────────

const longTasks: number[] = [];
try {
  new PerformanceObserver((list) => {
    for (const item of list.getEntries()) longTasks.push(Math.round(item.duration));
  }).observe({ type: "longtask", buffered: true });
} catch {
  /* 不支持 longtask 的浏览器:留空 */
}

let churnAdded = 0;
let churnRemoved = 0;
let churnObserver: MutationObserver | null = null;

function churnReset(): void {
  churnObserver?.disconnect();
  churnAdded = 0;
  churnRemoved = 0;
  churnObserver = new MutationObserver((records) => {
    for (const record of records) {
      churnAdded += record.addedNodes.length;
      churnRemoved += record.removedNodes.length;
    }
  });
  churnObserver.observe(messagesEl(), { childList: true, subtree: true });
}

function retainedChars(): number {
  return untrack(() => {
    const state = store.state[0];
    let total = 0;
    for (const key of Object.keys(state.turns)) {
      const turn = state.turns[key]!;
      total += turn.user.text.length + (turn.answer?.text.length ?? 0);
      for (const step of turn.steps) {
        if ("text" in step) total += step.text.length;
        if (step.kind === "tool") total += (step.output?.text?.length ?? 0) + (step.progressTail?.length ?? 0);
      }
    }
    return total;
  });
}

function metrics(): Record<string, unknown> {
  const el = messagesEl();
  const state = store.state[0];
  return untrack(() => ({
    slots: state.slots.length,
    loadedTurns: Object.keys(state.turns).length,
    runningTurns: state.runningTurns,
    failedTurns: state.failedTurns,
    activeTurnKey: state.activeTurnKey,
    oldestIndex: state.oldestIndex,
    hasMore: state.hasMore,
    watermark: state.watermark,
    retainedChars: retainedChars(),
    domNodes: el.querySelectorAll("*").length,
    mountedChars: el.textContent?.length ?? 0,
    turns: el.querySelectorAll(".turn").length,
    placeholders: el.querySelectorAll(".turn-placeholder").length,
    toolHeads: el.querySelectorAll(".tool-head").length,
    toolDetails: el.querySelectorAll(".tool-detail").length,
    timelines: el.querySelectorAll(".timeline").length,
    codeBlocks: el.querySelectorAll("pre").length,
    tables: el.querySelectorAll("table").length,
    diffNodes: el.querySelectorAll("[class*='diff']").length,
    scrollHeight: el.scrollHeight,
    scrollTop: Math.round(el.scrollTop),
    clientHeight: el.clientHeight,
    distToBottom: Math.round(el.scrollHeight - el.scrollTop - el.clientHeight),
    horizontalOverflow: el.scrollWidth - el.clientWidth,
    longTasks,
    churn: { added: churnAdded, removed: churnRemoved },
    pageRequests: pageRequests.length,
  }));
}

const wait = (ms: number): Promise<void> => new Promise((resolve) => setTimeout(resolve, ms));
const frames = async (count: number): Promise<void> => {
  for (let i = 0; i < count; i += 1) await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));
};

/** 帧间隔采样:p50/p95/max + 掉帧数(>50ms 算一次卡顿)。 */
async function sampleFrames(ms: number, work: () => Promise<void>): Promise<Record<string, unknown>> {
  const gaps: number[] = [];
  let last = performance.now();
  let stop = false;
  const tick = (): void => {
    if (stop) return;
    const now = performance.now();
    gaps.push(Math.round((now - last) * 10) / 10);
    last = now;
    requestAnimationFrame(tick);
  };
  requestAnimationFrame(tick);
  await work();
  await wait(ms);
  stop = true;
  gaps.shift();
  const sorted = [...gaps].sort((a, b) => a - b);
  return {
    frames: gaps.length,
    p50: sorted[Math.floor(sorted.length * 0.5)] ?? 0,
    p95: sorted[Math.floor(sorted.length * 0.95)] ?? 0,
    max: sorted[sorted.length - 1] ?? 0,
    janks: gaps.filter((gap) => gap > 50).length,
  };
}

function feed(turnId: string, event: Record<string, unknown>, seq?: number): void {
  const target = store as unknown as { onTimelineEntry(entry: { timeline_seq: number; turn_id: string; event: Record<string, unknown> }): void };
  target.onTimelineEntry({ timeline_seq: seq ?? liveSeq++, turn_id: turnId, event });
}

const report: Array<Record<string, unknown>> = [];

const scenarios = {
  /** 流式一个新回合:思考 + 工具 + 大段 Markdown 作答,测每帧工作量。 */
  async stream(): Promise<Record<string, unknown>> {
    const id = "t200";
    churnReset();
    const requestsAtStart = pageRequests.length;
    let fedAnswerChars = 0;
    let fedThinkingChars = 0;
    feed(id, { type: "turn_opened", user_text: "第 200 问：接着刚才的继续说" });
    feed(id, { type: "block_opened", block: { block_id: "r200", kind: "reasoning", state: "open" } });
    const cadence = await sampleFrames(120, async () => {
      for (let i = 0; i < 40; i += 1) {
        const delta = prose(mulberry32(i), 60);
        fedThinkingChars += delta.length;
        feed(id, { type: "text_delta", block_id: "r200", delta });
        await wait(8);
      }
    });
    feed(id, { type: "block_sealed", block_id: "r200" });
    feed(id, { type: "block_opened", block: { block_id: "e200", kind: "tool", tool: { name: "exec", state: "running", args_json: "{}" } } });
    await wait(60);
    feed(id, {
      type: "tool_updated",
      block_id: "e200",
      tool: { name: "exec", state: "succeeded", display: { summary: "exec: just now", body: { kind: "text", text: ansiLog(mulberry32(200), 120), truncated: false } }, metrics: { elapsed_ms: 900 } },
    });
    feed(id, { type: "block_opened", block: { block_id: "x200", kind: "text", state: "open" } });
    // 视觉更新节奏:尾块(.md-tail)每被写一次 DOM 就是一次「用户看到文字变多」。
    const paintAt: number[] = [];
    let seenRecords = 0;
    const paintWatch = new MutationObserver((records) => {
      seenRecords += records.length;
      if (records.some((record) => (record.target as HTMLElement).classList?.contains("md-tail"))) paintAt.push(performance.now());
    });
    paintWatch.observe(messagesEl(), { childList: true, subtree: true, characterData: true });
    const answerCadence = await sampleFrames(150, async () => {
      const text = markdownOf(mulberry32(700), 200, 9_000);
      for (let i = 0; i < text.length; i += 240) {
        fedAnswerChars += Math.min(240, text.length - i);
        feed(id, { type: "text_delta", block_id: "x200", delta: text.slice(i, i + 240) });
        await wait(6);
      }
    });
    paintWatch.disconnect();
    const gaps = paintAt.slice(1).map((at, i) => Math.round(at - paintAt[i]!));
    const sortedGaps = [...gaps].sort((a, b) => a - b);
    const paintCadence = {
      paints: paintAt.length,
      seenRecords,
      tailNodes: messagesEl().querySelectorAll(".md-tail").length,
      // store 侧真相 vs 喂进去的字符数:差额就是被「缺口判定」丢掉的帧。
      resnapshotsDuringStream: pageRequests.length - requestsAtStart,
      fedThinkingChars,
      fedAnswerChars,
      ...untrack(() => {
        const turn = store.state[0].turns[id];
        const thinking = turn?.steps.find((step) => step.id === "r200");
        const answer = turn?.steps.find((step) => step.id === "x200");
        return {
          steps: turn?.steps.length ?? -1,
          storedThinkingChars: thinking != null && "text" in thinking ? thinking.text.length : -1,
          storedAnswerChars: answer != null && "text" in answer ? answer.text.length : -1,
          watermark: store.state[0].watermark,
          liveSeqUsed: liveSeq,
        };
      }),
      p50Ms: sortedGaps[Math.floor(sortedGaps.length * 0.5)] ?? 0,
      p95Ms: sortedGaps[Math.floor(sortedGaps.length * 0.95)] ?? 0,
      maxMs: sortedGaps[sortedGaps.length - 1] ?? 0,
    };
    await frames(20); // 等 reveal 与贴底循环都收敛,再量 dist
    const out = { name: "stream", thinkingCadence: cadence, answerCadence, paintCadence, settledChurn: { added: churnAdded, removed: churnRemoved }, ...metrics() };
    report.push(out);
    return out;
  },

  /** 水位缺口 → 整表快照校正:测重对齐到底重挂多少节点。 */
  async realign(): Promise<Record<string, unknown>> {
    churnReset();
    const before = metrics();
    const jump = liveSeq + 40; // 跳号 → 缺口
    watermark = jump - 1; // 校正页水位正好接上
    feed("t200", { type: "text_delta", block_id: "x200", delta: "" }, jump); // 该条目被丢弃,只触发校正
    liveSeq = jump + 1;
    await wait(400);
    await frames(6);
    const after = metrics();
    const out = {
      name: "realign",
      churn: { added: churnAdded, removed: churnRemoved },
      before: { slots: before.slots, domNodes: before.domNodes, mountedChars: before.mountedChars, loadedTurns: before.loadedTurns },
      after,
    };
    report.push(out);
    return out;
  },

  /** 展开时间线与工具详情(顺带验 turn.key 在重对齐之后仍然可用)。 */
  async expand(): Promise<Record<string, unknown>> {
    const el = messagesEl();
    const before = el.scrollTop;
    const rows = Array.from(el.querySelectorAll<HTMLElement>(".collapsed-row")).slice(0, 3);
    for (const row of rows) row.click();
    await frames(20);
    const timelinesAfterRows = el.querySelectorAll(".timeline").length;
    const heads = Array.from(el.querySelectorAll<HTMLElement>(".tool-head")).slice(0, 6);
    for (const head of heads) head.click();
    await frames(20);
    const out = {
      name: "expand",
      clickedRows: rows.length,
      timelinesAfterRows,
      clickedHeads: heads.length,
      scrollDeltaFromExpand: Math.round(el.scrollTop - before),
      ...metrics(),
    };
    report.push(out);
    return out;
  },

  /** 触顶翻页:滚轮到顶 → 每轮 loadOlder 一页;测滚动补偿(spec §14.2)。 */
  async toTop(): Promise<Record<string, unknown>> {
    const el = messagesEl();
    el.dispatchEvent(new WheelEvent("wheel", { deltaY: -1_200, bubbles: true }));
    pageRequests = [];
    const rounds: Array<Record<string, unknown>> = [];
    for (let i = 0; i < 8; i += 1) {
      // 位置不变就不会有 scroll 事件(测试里必须自己制造一次变化)。
      el.scrollTop = Math.min(60, Math.max(1, el.scrollTop));
      await frames(2);
      el.scrollTop = 0;
      el.dispatchEvent(new WheelEvent("wheel", { deltaY: -1_200, bubbles: true }));
      // 锚点取「触顶这一刻视口里的行」——等这一页落地后再比,才等于用户体感:
      // 前插不该把正在看的内容从视口里挪走。
      const box = el.getBoundingClientRect();
      const anchor = Array.from(el.querySelectorAll<HTMLElement>("[data-slot-key]")).find((node) => {
        const rect = node.getBoundingClientRect();
        return rect.bottom > box.top + 4 && rect.top < box.bottom - 4;
      });
      const key = anchor?.dataset.slotKey ?? "";
      const topBefore = anchor?.getBoundingClientRect().top ?? 0;
      await wait(260);
      await frames(12);
      const moved = el.querySelector<HTMLElement>(`[data-slot-key="${CSS.escape(key)}"]`);
      rounds.push({
        round: i,
        requests: pageRequests.length,
        slots: store.state[0].slots.length,
        oldestIndex: store.state[0].oldestIndex,
        anchorKey: key,
        anchorMovedPx: moved == null ? "anchor-gone" : Math.round(moved.getBoundingClientRect().top - topBefore),
        scrollTopAfter: Math.round(el.scrollTop),
      });
      if (pageRequests.length >= 5) break;
    }
    await wait(400);
    await frames(12);
    const out = { name: "toTop", rounds, ...metrics() };
    report.push(out);
    return out;
  },

  /** 回到底部:自动跟随是否收敛 + 窗口淘汰是否生效。 */
  async toBottom(): Promise<Record<string, unknown>> {
    const el = messagesEl();
    const steps: Array<Record<string, number>> = [];
    for (let i = 0; i < 40; i += 1) {
      el.dispatchEvent(new WheelEvent("wheel", { deltaY: 3_000, bubbles: true }));
      el.scrollTop = Math.max(0, el.scrollHeight - el.clientHeight - 120);
      await frames(2);
      el.scrollTop = el.scrollHeight;
      await wait(50);
      const dist = Math.round(el.scrollHeight - el.scrollTop - el.clientHeight);
      steps.push({ i, dist });
      if (dist < 60) break;
    }
    const settled = Math.round(el.scrollHeight - el.scrollTop - el.clientHeight);
    await wait(600); // 给 requestIdleCallback 的 refit(窗口淘汰)时间
    await frames(20);
    const out = { name: "toBottom", distAfterScroll: settled, scrollSteps: steps.length, buttonVisible: document.querySelector(".back-bottom") != null, ...metrics() };
    report.push(out);
    return out;
  },

  /**
   * 尾块重绘代价:模拟「每帧重解析未闭合尾块」这条路径(现在被
   * Markdown.tsx 的 TAIL_THROTTLE_MS=120 限到 ~8fps),量它在不同尾块长度下
   * 单帧要多久 —— 120Hz 的预算是 8.3ms/帧,90Hz 是 11.1ms/帧。
   */
  async markdownCost(): Promise<Record<string, unknown>> {
    const split = await import("./markdown/split");
    const render = await import("./markdown/render");
    const rand = mulberry32(4242);
    const results: Array<Record<string, unknown>> = [];
    for (const [label, make] of [
      ["段落(CJK,每 240 字增长)", () => prose(rand, 240)],
      ["代码围栏(未闭合,持续增长)", () => "for (let i = 0; i < n; i += 1) { await flush(delta[" + Math.floor(rand() * 99) + "]); }\n"],
      ["表格(未闭合,每帧一行)", () => `| w-${Math.floor(rand() * 999)} | ${Math.floor(rand() * 977)}ms | ${Math.floor(rand() * 260)}px | auto | ok |\n`],
      ["单段无换行 150KB(病态)", () => "字".repeat(1000)],
      ["未闭合代码围栏 150KB(病态)", () => "let x = " + Math.floor(rand() * 999) + "; // 注释\n".repeat(40)],
    ] as Array<[string, () => string]>) {
      let text = "";
      const per: number[] = [];
      for (let step = 0; step < (label.includes("病态") ? 150 : 120); step += 1) {
        text += make();
        const t0 = performance.now();
        const blocks = split.splitBlocks(text);
        const html = render.renderMarkdownHtml(blocks[blocks.length - 1]!.content);
        const host = document.createElement("div");
        host.innerHTML = html;
        per.push(Math.round((performance.now() - t0) * 100) / 100);
      }
      const sorted = [...per].sort((a, b) => a - b);
      results.push({
        shape: label,
        tailChars: text.length,
        paintP50Ms: sorted[Math.floor(sorted.length * 0.5)],
        paintP95Ms: sorted[Math.floor(sorted.length * 0.95)],
        paintMaxMs: sorted[sorted.length - 1],
        totalMs: Math.round(per.reduce((sum, ms) => sum + ms, 0)),
        overBudget120: per.filter((ms) => ms > 8.3).length,
      });
    }
    const out = { name: "markdownCost", results };
    report.push(out);
    return out;
  },

  /**
   * 昂贵形状:持续流一张 200 行的表格(实测这种尾块单帧重解析 p50 5.9ms、
   * max 15.5ms,超 120Hz 的 8.3ms 预算)。用来验 Markdown 的预算自适应确实会
   * 从「每帧画」退回「限频画」——即绘制间隔应出现 ~120ms 的档位。
   */
  async tableStream(): Promise<Record<string, unknown>> {
    const id = "t201";
    const el = messagesEl();
    const paintAt: number[] = [];
    const watch = new MutationObserver((records) => {
      if (records.some((record) => (record.target as HTMLElement).classList?.contains("md-tail"))) paintAt.push(performance.now());
    });
    feed(id, { type: "turn_opened", user_text: "第 201 问：把窗口逐行列出来" });
    feed(id, { type: "block_opened", block: { block_id: "x201", kind: "text", state: "open" } });
    await frames(2);
    watch.observe(el, { childList: true, subtree: true });
    const cadence = await sampleFrames(200, async () => {
      for (let row = 0; row < 200; row += 1) {
        feed(id, { type: "text_delta", block_id: "x201", delta: `| w-${row} | ${(row * 7919) % 977}ms | ${(row * 31) % 260}px | ${row % 2 ? "auto" : "content"} | ok |\n` });
        await wait(8);
      }
    });
    watch.disconnect();
    feed(id, { type: "turn_sealed", state: "completed" });
    await frames(20);
    const gaps = paintAt.slice(1).map((at, i) => Math.round(at - paintAt[i]!));
    const sorted = [...gaps].sort((a, b) => a - b);
    const out = {
      name: "tableStream",
      frameCadence: cadence,
      paints: paintAt.length,
      paintGapP50Ms: sorted[Math.floor(sorted.length * 0.5)] ?? 0,
      paintGapP95Ms: sorted[Math.floor(sorted.length * 0.95)] ?? 0,
      throttledPaints: gaps.filter((gap) => gap > 60).length,
      frameRatePaints: gaps.filter((gap) => gap <= 40).length,
      tableRows: el.querySelectorAll("table tr").length,
      ...metrics(),
    };
    report.push(out);
    return out;
  },

  /**
   * 只做动画、不做测量:驱动那边用 CDP Performance.getMetrics 取累计
   * Layout/RecalcStyle/Script 时长与 FrameCount,算出每帧成本
   * (`long-animation-frame` 只报 >50ms 的帧,量不出 8.3ms 预算)。
   */
  async animate(target: "collapse" | "tool" | "dock", times: number): Promise<Record<string, unknown>> {
    const el = messagesEl();
    const settle = async (frames: number): Promise<void> => {
      for (let i = 0; i < frames; i += 1) await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));
    };
    let acted = 0;
    if (target === "dock") {
      const head = document.querySelector<HTMLElement>(".todo-head");
      for (let i = 0; i < times && head != null; i += 1) {
        head.click();
        await settle(20); // 260ms 过渡 @60Hz ≈ 16 帧
        acted += 1;
      }
    } else {
      const selector = target === "collapse" ? ".collapsed-row" : ".tool-head";
      const rows = Array.from(el.querySelectorAll<HTMLElement>(selector)).slice(0, times);
      for (const row of rows) {
        row.click();
        await settle(16); // 180ms 展开过渡
        acted += 1;
      }
      for (const row of rows.slice().reverse()) {
        row.click();
        await settle(14); // 140ms 收起过渡
      }
    }
    return { name: "animate", target, acted, ...metrics() };
  },

  /**
   * 高频工具输出 A/B:`merged` 走 store 的按帧归并(现实现);`direct` 逐条直接写
   * store —— 正是改动前 `onTimelineEntry` 对 `tool_progress` 的行为。两边都在同一
   * 个构建里跑,不需要回退代码。工具详情展开,让 `.tool-progress` 真的挂在 DOM 上。
   */
  async progressStream(mode: "merged" | "direct" = "merged", linesPerChunk = 3): Promise<Record<string, unknown>> {
    const id = mode === "merged" ? "t202" : "t203";
    const blockId = mode === "merged" ? "p202" : "p203";
    // 先造好数据:排除夹具自己的字符串生成开销。
    const chunks = Array.from({ length: 150 }, (_, i) => ansiLog(mulberry32(i), linesPerChunk));
    feed(id, { type: "turn_opened", user_text: `第 ${id.slice(1)} 问：跑个长任务` });
    feed(id, { type: "block_opened", block: { block_id: blockId, kind: "tool", tool: { name: "exec", state: "running" } } });
    await frames(4);
    document.querySelector<HTMLElement>(`[data-slot-key="${CSS.escape(id)}"] .tool-head`)?.click();
    await frames(6);
    const el = messagesEl();
    let progressWrites = 0;
    const watch = new MutationObserver((records) => {
      // characterData 记录的 target 是文本节点,没有 closest —— 先归一到元素。
      const hit = records.some((record) => {
        const raw = record.target as Node;
        const node = raw.nodeType === 1 ? (raw as HTMLElement) : raw.parentElement;
        return node?.closest(".tool-progress") != null;
      });
      if (hit) progressWrites += 1;
    });
    churnReset();
    const t0 = performance.now();
    watch.observe(el, { childList: true, subtree: true, characterData: true });
    const cadence = await sampleFrames(150, async () => {
      for (const chunk of chunks) {
        if (mode === "merged") {
          feed(id, { type: "tool_progress", block_id: blockId, chunk });
        } else {
          const seq = liveSeq++;
          store.state[1]((draft) => {
            applyEntry(draft, { timeline_seq: seq, turn_id: id, event: { type: "tool_progress", block_id: blockId, chunk } });
            draft.watermark = seq;
          });
        }
        await wait(2);
      }
    });
    watch.disconnect();
    const elapsedMs = Math.round(performance.now() - t0);
    const tailChars = untrack(() => {
      const step = store.state[0].turns[id]?.steps.find((item) => item.id === blockId);
      return step != null && step.kind === "tool" ? step.progressTail?.length ?? 0 : -1;
    });
    feed(id, { type: "tool_updated", block_id: blockId, tool: { name: "exec", state: "succeeded", display: { summary: "exec done", body: { kind: "text", text: "done\n", truncated: false } } } });
    await frames(10);
    const out = { name: `progressStream:${mode}`, mode, linesPerChunk, cadence, elapsedMs, tailChars, fedChunks: chunks.length, progressWrites, churn: { added: churnAdded, removed: churnRemoved }, ...metrics() };
    report.push(out);
    return out;
  },

  /**
   * 展开工具详情时 `.tool-progress` 走的 ANSI 路径:16KB 尾窗每次更新都要
   * `hasAnsi` + `parseAnsi` 全量重解析。量它在不同长度下的单帧代价。
   */
  async ansiCost(): Promise<Record<string, unknown>> {
    const ansi = await import("./lib/ansi");
    const shapes: Array<[string, number]> = [["1KB", 14], ["4KB", 55], ["16KB(尾窗上限)", 220]];
    const results = shapes.map(([label, lines]) => {
      const text = ansiLog(mulberry32(77), lines);
      const per: number[] = [];
      for (let i = 0; i < 40; i += 1) {
        const t0 = performance.now();
        const parsed = ansi.parseAnsi(text);
        const has = ansi.hasAnsi(text);
        per.push(Math.round((performance.now() - t0) * 100) / 100);
        if (parsed.length === 0 && !has) void 0;
      }
      const sorted = [...per].sort((a, b) => a - b);
      return { size: label, chars: text.length, p50Ms: sorted[20], p95Ms: sorted[38], maxMs: sorted[39], over8_3ms: per.filter((ms) => ms > 8.3).length };
    });
    const out = { name: "ansiCost", results };
    report.push(out);
    return out;
  },

  /** 版式:横向溢出与表格/代码块/长行的表现。 */
  layout(): Record<string, unknown> {
    const el = messagesEl();
    const offenders: string[] = [];
    for (const node of Array.from(el.querySelectorAll<HTMLElement>(".turn, table, pre, .tool-text, .bubble, .thinking-full"))) {
      if (node.scrollWidth - node.clientWidth > 2) offenders.push(`${node.className}|+${node.scrollWidth - node.clientWidth}`);
    }
    const out = {
      name: "layout",
      innerWidth: window.innerWidth,
      offenders: offenders.slice(0, 10),
      turnWidths: Array.from(el.querySelectorAll<HTMLElement>(".turn")).slice(0, 3).map((node) => Math.round(node.getBoundingClientRect().width)),
      zeroHeightTurns: Array.from(el.querySelectorAll<HTMLElement>(".turn")).filter((node) => node.getBoundingClientRect().height < 40).length,
      ...metrics(),
    };
    report.push(out);
    return out;
  },
};

// ── 启动 ─────────────────────────────────────────────────────────────────────

declare global {
  interface Window {
    /** 供 CDP/console 驱动的夹具入口。 */
    __stress: Record<string, unknown>;
  }
}

const bootStart = performance.now();
// 复刻真实滚动链(#app > header#top[40px] + main#main > #session > #messages):
// 少了头部行,main#main 会占 40px 那行,所有视口相关测量都失真。
render(
  () => (
    <div id="app">
      <header id="top">
        <span class="stress-title">stress fixture</span>
      </header>
      <main id="main">
        <SessionView tab={tab} />
      </main>
    </div>
  ),
  document.getElementById("root") as HTMLElement,
);

churnReset();
void (async () => {
  await store.resnapshot();
  await frames(2);
  const mountMs = Math.round(performance.now() - bootStart);
  await frames(30);
  report.push({ name: "boot", mountMs, bootChurn: { added: churnAdded, removed: churnRemoved }, ...metrics() });
})();

window.__stress = {
  store,
  metrics,
  report,
  scenarios,
  churnReset,
  retainedChars,
  resetRequests: () => {
    pageRequests = [];
  },
};
