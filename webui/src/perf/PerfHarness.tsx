/**
 * Streaming-render performance harness.
 *
 * Drives the streaming-markdown adapter with synthetic transcript-sized input so
 * we can prove (or disprove) that parse/DOM/memory cost is linear in characters.
 *
 * Two measurement modes:
 *  - wall `ms`      includes stream pacing (deltasPerFrame → animation frames)
 *  - `parseMs`      is CPU time inside smd.parser_write only — the renderer cost
 *
 * `?perf=1&autorun=1&blocks=&chars=&chunk=&mode=` runs one scenario on load and
 * parks the result on `window.__perfResult`, so the Bun.WebView runner can take a
 * fresh, uncontaminated heap reading per data point.
 */
import { createSignal, onSettled, Show } from "solid-js";
import { createStreamingMarkdown, type StreamingMarkdown } from "../lib/streaming-md";

export type PerfMode = "batched" | "immediate";

export type PerfOptions = {
  blocks: number;
  /** Approximate characters per block. */
  chars: number;
  /** SSE delta size. */
  chunk: number;
  mode: PerfMode;
  /** Deltas delivered per animation frame in batched mode (stream rate). */
  deltasPerFrame?: number;
};

export type PerfResult = {
  blocks: number;
  chars: number;
  chunk: number;
  mode: PerfMode;
  totalChars: number;
  deltas: number;
  parses: number;
  /** Wall clock, includes animation-frame pacing. */
  ms: number;
  /** CPU inside parser_write (parse + store + DOM update). */
  parseMs: number;
  parseMsPerKChar: number;
  domNodes: number;
  nodesPerKChar: number;
  heapUsed: number | null;
};

const UNIT = "**粗体** 与 `code` 与 [链接](https://example.com/x) 的混排文本，用于模拟真实转录。";

function makeBlock(index: number, chars: number): string {
  let body = "";
  while (body.length < chars) body += UNIT;
  return `## 回合 ${index + 1}\n\n${body.slice(0, chars)}`;
}

export type PerfApi = {
  run: (options: PerfOptions) => Promise<PerfResult>;
  reset: () => void;
  results: PerfResult[];
};

function autorunOptions(search: string): PerfOptions | null {
  const params = new URLSearchParams(search);
  if (!params.has("autorun")) return null;
  const mode = (params.get("mode") ?? "batched") as PerfMode;
  return {
    blocks: Number(params.get("blocks") ?? 1),
    chars: Number(params.get("chars") ?? 16_000),
    chunk: Number(params.get("chunk") ?? 8),
    mode,
    deltasPerFrame: params.has("dpf") ? Number(params.get("dpf")) : undefined,
  };
}

export default function PerfHarness() {
  let host: HTMLDivElement | undefined;
  let md: StreamingMarkdown | undefined;
  const [status, setStatus] = createSignal("idle");
  const [last, setLast] = createSignal<PerfResult | null>(null);

  const nextFrame = () => new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));

  async function run(options: PerfOptions): Promise<PerfResult> {
    md!.reset();
    await nextFrame();
    const text = Array.from({ length: options.blocks }, (_, i) => makeBlock(i, options.chars)).join("\n\n");
    const deltas: string[] = [];
    for (let i = 0; i < text.length; i += options.chunk) deltas.push(text.slice(i, i + options.chunk));

    const started = performance.now();
    if (options.mode === "immediate") {
      for (const delta of deltas) md!.writeNow(delta);
      md!.flush();
    } else {
      const perFrame = options.deltasPerFrame ?? 32;
      for (let i = 0; i < deltas.length; i += perFrame) {
        const end = Math.min(i + perFrame, deltas.length);
        for (let j = i; j < end; j += 1) md!.write(deltas[j]!);
        await nextFrame();
      }
      md!.flush();
      await nextFrame();
    }
    const ms = performance.now() - started;
    // In immediate mode every write landed in this same task; let the render
    // settle before counting DOM nodes (wall time is already captured).
    await Promise.resolve();
    await nextFrame();

    const stats = md!.stats();
    const domNodes = host?.querySelectorAll("*").length ?? 0;
    const memory = (performance as unknown as { memory?: { usedJSHeapSize: number } }).memory;
    const result: PerfResult = {
      blocks: options.blocks,
      chars: options.chars,
      chunk: options.chunk,
      mode: options.mode,
      totalChars: stats.chars,
      deltas: stats.chunks,
      parses: stats.parses,
      ms: Number(ms.toFixed(1)),
      parseMs: Number(stats.parseMs.toFixed(1)),
      parseMsPerKChar: Number((stats.parseMs / Math.max(1, stats.chars / 1000)).toFixed(3)),
      domNodes,
      nodesPerKChar: Number((domNodes / Math.max(1, stats.chars / 1000)).toFixed(3)),
      heapUsed: memory?.usedJSHeapSize ?? null,
    };
    api.results.push(result);
    setLast(result);
    setStatus("done");
    return result;
  }

  const api: PerfApi = {
    run,
    reset: () => {
      md?.reset();
      api.results = [];
      setLast(null);
      setStatus("idle");
    },
    results: [],
  };

  onSettled(() => {
    md = createStreamingMarkdown(host!);
    const w = window as unknown as { __perf: PerfApi; __perfHost: () => number; __perfResult?: PerfResult };
    w.__perf = api;
    w.__perfHost = () => host?.querySelectorAll("*").length ?? 0;
    const auto = autorunOptions(window.location.search);
    if (auto) {
      void run(auto).then((result) => {
        w.__perfResult = result;
      });
    }
    return () => {
      delete (window as unknown as { __perf?: PerfApi }).__perf;
    };
  });

  return (
    <main class="perf">
      <header class="bar">
        <div>
          <h1>流式渲染性能台</h1>
          <p class="meta">{"window.__perf.run({blocks, chars, chunk, mode})"}</p>
        </div>
        <span class="chip">{status()}</span>
      </header>
      <Show when={last()}>
        {(result) => (
          <p class="meta">
            {result().totalChars} chars · {result().deltas} deltas · {result().parses} parses · wall{" "}
            {result().ms.toFixed(0)}ms · cpu {result().parseMs.toFixed(0)}ms (
            {result().parseMsPerKChar.toFixed(2)}ms/kchar) · {result().domNodes} nodes
          </p>
        )}
      </Show>
      <section class="transcript" ref={host} />
    </main>
  );
}
