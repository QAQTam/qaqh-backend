/**
 * QAQH 压测 mock 服务器（Bun + hono，复用 proxybun 依赖）。
 *
 * 行为：收到 POST /chat/completions 后，以固定速率（默认 1000 token/s）
 * 流式输出 `PAYLOAD`（3.md 内容）重复 N 次（默认 5000），然后 finish + [DONE]。
 *
 * 1 token ≈ 4 字符（英文近似）；1000 tok/s → 每 tick(10ms) 发送 4 字符。
 * 目标总文本 ≈ 2393 B × 5000 ≈ 11.5 MB ≈ 287 万 token（约 48 分钟流）。
 * 支持 STRESS_SPEED 环境变量加速（如 20 → 20000 字符/s，2.4 分钟流完）。
 *
 * 启动：PORT=18899 bun run stress_mock.ts
 */
import { Hono } from "hono";
import { readFileSync } from "node:fs";

const PORT = Number(process.env.PORT ?? 18899);
const REPEATS = Number(process.env.REPEATS ?? 5000);
// 每 10ms tick 发送的字符数（4 字符/tick = 1000 tok/s @ 4字符/token）
const CHARS_PER_TICK = Math.max(1, Number(process.env.STRESS_SPEED ?? 10)) * 4;

const payload = readFileSync("C:/Users/QAQTam/Desktop/3.md", "utf-8");
const FULL_TEXT = payload.repeat(REPEATS);
const TOTAL_CHARS = FULL_TEXT.length;
console.log(
  `[stress-mock] payload=${payload.length}B x${REPEATS} = ${TOTAL_CHARS} chars ` +
    `(~${Math.round(TOTAL_CHARS / 4)} tokens), rate=${CHARS_PER_TICK} chars/s`,
);

const app = new Hono();

app.post("/v1/chat/completions", async (c) => {
  const body = await c.req.json().catch(() => ({}) as any);
  const stream = body?.stream !== false;
  console.log(`[stress-mock] request: model=${body?.model} stream=${stream}`);

  if (!stream) {
    // 非流式请求（如标题生成等辅助调用）：小回复
    return c.json({
      id: "stress-nonstream",
      choices: [
        { index: 0, message: { role: "assistant", content: "ok" }, finish_reason: "stop" },
      ],
      usage: { prompt_tokens: 10, completion_tokens: 1, total_tokens: 11 },
    });
  }

  const sse = new ReadableStream({
    async start(controller) {
      const enc = new TextEncoder();
      let offset = 0;
      let emitted = 0;
      let first = true;
      const t0 = Date.now();

      const sendChunk = (delta: Record<string, unknown>) => {
        controller.enqueue(
          enc.encode(`data: ${JSON.stringify({ choices: [{ index: 0, delta }] })}\n\n`),
        );
      };

      // 开场 role chunk
      sendChunk({ role: "assistant", content: "" });
      first = false;

      while (offset < TOTAL_CHARS) {
        const slice = FULL_TEXT.slice(offset, offset + CHARS_PER_TICK);
        offset += slice.length;
        // reasoning / content 交替路径都走 content（长 reasoning 块正是压测目标，
        // 但 worker 侧把 reasoning_content 投影为 reasoning 块；两者同受 timeline
        // 预算约束。此处用 reasoning_content 制造最坏情形：无限长的思考块）。
        sendChunk({ reasoning_content: slice });
        emitted += slice.length;
        await new Promise((r) => setTimeout(r, 10));
      }

      sendChunk({});
      controller.enqueue(
        enc.encode(
          `data: ${JSON.stringify({
            choices: [{ index: 0, delta: {}, finish_reason: "stop" }],
            usage: {
              prompt_tokens: 100,
              completion_tokens: Math.round(TOTAL_CHARS / 4),
              total_tokens: Math.round(TOTAL_CHARS / 4) + 100,
            },
          })}\n\n`,
        ),
      );
      controller.enqueue(enc.encode("data: [DONE]\n\n"));
      controller.close();
      console.log(
        `[stress-mock] stream done: ${emitted} chars in ${((Date.now() - t0) / 1000).toFixed(1)}s`,
      );
    },
  });

  return new Response(sse, {
    headers: {
      "Content-Type": "text/event-stream; charset=utf-8",
      "Cache-Control": "no-cache",
      Connection: "keep-alive",
    },
  });
});

console.log(`[stress-mock] listening on http://127.0.0.1:${PORT}`);
Bun.serve({ port: PORT, fetch: app.fetch });
