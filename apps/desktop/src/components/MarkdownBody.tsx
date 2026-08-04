import { createEffect, createMemo, createSignal, createStore, For, onCleanup, reconcile, Show } from "solid-js";
import { marked, Renderer } from "marked";
import { createHighlighter, createOnigurumaEngine } from "shiki";
import renderMathInElement from "katex/contrib/auto-render";
import {
  hydrateMermaidPlaceholders,
  createMermaidPlaceholder,
  MERMAID_LANG,
} from "../lib/mermaid-render";

// ── Shiki singleton ──

let hiPromise: ReturnType<typeof createHighlighter> | null = null;

function getHi() {
  if (!hiPromise) {
    hiPromise = createHighlighter({
      themes: ["github-light", "github-dark"],
      langs: [
        "ts", "tsx", "js", "jsx", "json", "yaml", "toml",
        "rs", "rust", "py", "python", "go", "java", "kt",
        "css", "scss", "html", "bash", "sh", "shell",
        "sql", "graphql", "md", "markdown", "diff",
        "c", "cpp", "zig", "nim",
      ],
      engine: createOnigurumaEngine(() => import("shiki/wasm")),
    }).catch((err) => {
      hiPromise = null;
      throw err;
    });
  }
  return hiPromise;
}

function detectTheme(): "github-light" | "github-dark" {
  if (typeof document === "undefined") return "github-dark";
  const theme = document.documentElement.getAttribute("data-theme");
  return theme === "dark" || theme === "dark-gray" ? "github-dark" : "github-light";
}

// ── Block types ──

interface MarkdownBlock {
  key: string;
  hash: string;
  raw: string;
  stable: boolean;
  kind: "text" | "code";
}

// ── Cached Renderer (avoids per-block allocation) ──

let cachedRenderer: Renderer | null = null;
let cachedTheme: string | null = null;

function buildRenderer(hi: Awaited<ReturnType<typeof getHi>>) {
  const theme = detectTheme();
  if (cachedRenderer && cachedTheme === theme) return cachedRenderer;

  const renderer = new Renderer();
  renderer.code = ({ text, lang }) => {
    if (lang === MERMAID_LANG) return createMermaidPlaceholder(text);
    const langId = !lang ? "text"
      : lang === "h" ? "c"
      : lang === "hpp" ? "cpp"
      : lang;
    const label = lang ? `<span class="code-lang-label">${lang}</span>` : "";
    const copyBtn = `<button class="code-copy-btn" aria-label="Copy code" title="Copy code"><svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><rect x="9" y="9" width="13" height="13" rx="2" ry="2"/><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"/></svg></button>`;
    try {
      const highlighted = hi.codeToHtml(text, { lang: langId, theme });
      return `<div class="code-block-wrapper">${copyBtn}${label}${highlighted}</div>`;
    } catch {
      return `<div class="code-block-wrapper">${copyBtn}${label}<pre><code>${text}</code></pre></div>`;
    }
  };

  cachedRenderer = renderer;
  cachedTheme = theme;
  return renderer;
}

// ── Markdown parsing ──

function parseMarkdown(raw: string, renderer?: Renderer): string {
  const html = marked.parse(raw, { async: false, gfm: true, breaks: false, renderer });
  if (typeof html !== "string") return "";
  const cleaned = html
    .replace(
      /(<pre\b[^>]*style=")([^"]*)(")/gi,
      (_, before, styles, after) =>
        before + styles.replace(/background-color\s*:\s*[^;]+;?/gi, "") + after,
    )
    .replace(/<pre\b([^>]*)\s+tabindex="0"([^>]*)>/gi, "<pre$1$2>");
  return renderMath(cleaned);
}

function renderMath(html: string): string {
  if (typeof document === "undefined" || (!html.includes("$") && !html.includes("\\("))) return html;
  const root = document.createElement("div");
  root.innerHTML = html;
  renderMathInElement(root, {
    delimiters: [
      { left: "$$", right: "$$", display: true },
      { left: "\\[", right: "\\]", display: true },
      { left: "\\(", right: "\\)", display: false },
      { left: "$", right: "$", display: false },
    ],
    ignoredTags: ["script", "noscript", "style", "textarea", "pre", "code", "option"],
    throwOnError: false,
  });
  return root.innerHTML;
}

function renderBlockHTML(raw: string, hi: Awaited<ReturnType<typeof getHi>>): string {
  return parseMarkdown(raw, buildRenderer(hi));
}

function renderFallbackHTML(raw: string): string {
  return parseMarkdown(raw);
}

/**
 * 流式 live 块的实时内联渲染：只处理已闭合的内联语法（**bold**、`code`、
 * [link](url) 等），未闭合的语法由 marked 按字面文本输出，不会产生破损 HTML。
 * 这是“流式期间也能看到加粗/链接”的关键，块级/代码/图表仍等 final。
 */
function inlineLiveHTML(raw: string): string {
  if (!raw.trim()) return "";
  try {
    return marked.parseInline(raw, { async: false });
  } catch {
    return "";
  }
}

// ── Block splitting (replaces Web Worker + projectBlocksOffThread) ──

function blockHash(raw: string): string {
  if (raw.length <= 24) return String(raw.length);
  return `${raw.length}:${raw.slice(0, 10)}…${raw.slice(-10)}`;
}

function projectBlocks(text: string, final: boolean): MarkdownBlock[] {
  if (!text) return [];
  if (final) {
    return [{ key: "f", hash: blockHash(text), raw: text, stable: true, kind: "text" }];
  }

  // Open Timeline text is rendered as a cheap inline preview. Full block
  // lexing/highlighting is deferred until the producer seals the block.
  return [{ key: "l0", hash: blockHash(text), raw: text, stable: false, kind: "text" }];

}

// ── Component ──

interface MarkdownBodyProps {
  content: string;
  class?: string;
  final?: boolean;
}

export default function MarkdownBody(props: MarkdownBodyProps) {
  let container!: HTMLDivElement;
  let renderGeneration = 0;
  let disposed = false;
  let livePreviewFrame: number | undefined;
  let pendingLiveBlock: MarkdownBlock | undefined;
  let prevVisible: MarkdownBlock[] = [];
  // 增量 live 解析游标：记录上次已解析的完整 raw，避免每帧对完整累积文本
  // 重新 parseInline（长流式下 O(n²) 退化，主线程被占满导致 UI 冻结）。
  // 注意不能依赖 block.hash 判重：blockHash 含 raw.length，流式增长时每帧
  // 都变化，必须用前缀关系（next.raw 以已解析文本开头 = 同一 live 块增长）。
  let livePrevRaw = "";

  interface LiveChunk { key: string; html: string }
  const [liveChunks, setLiveChunks] = createSignal<{ hash: string; chunks: LiveChunk[] }>(
    { hash: "", chunks: [] },
  );
  /** chunk 数上限：超过后全量重解析合并（跨 chunk 未闭合语法修正），
   *  浏览器原生 innerHTML 解析远快于 marked，合并频率低（每帧一个 chunk）。 */
  const MAX_LIVE_CHUNKS = 256;

  const cancelLivePreview = () => {
    if (livePreviewFrame === undefined) return;
    if (typeof cancelAnimationFrame === "function") cancelAnimationFrame(livePreviewFrame);
    else clearTimeout(livePreviewFrame);
    livePreviewFrame = undefined;
  };

  const scheduleLivePreview = (block: MarkdownBlock | undefined) => {
    pendingLiveBlock = block;
    if (livePreviewFrame !== undefined) return;
    const schedule = typeof requestAnimationFrame === "function"
      ? requestAnimationFrame
      : (callback: FrameRequestCallback) => setTimeout(() => callback(Date.now()), 0) as unknown as number;
    livePreviewFrame = schedule(() => {
      livePreviewFrame = undefined;
      const next = pendingLiveBlock;
      pendingLiveBlock = undefined;
      if (disposed || !next) return;
      const isSameLiveBlock = next.raw.length >= livePrevRaw.length
        && next.raw.startsWith(livePrevRaw);
      if (!isSameLiveBlock) {
        // 新 block / 内容回退 / 跳变：重置游标，首帧全量解析。
        livePrevRaw = next.raw;
        setLiveChunks({ hash: next.hash, chunks: [{ key: `${next.hash}-0`, html: inlineLiveHTML(next.raw) }] });
        return;
      }
      if (next.raw.length === livePrevRaw.length) return;
      // 增量：只解析新增 delta（O(delta)），DOM 侧按 keyed <For> 追加，
      // 已渲染的旧 chunk 不会被浏览器重新解析。
      const delta = next.raw.slice(livePrevRaw.length);
      livePrevRaw = next.raw;
      setLiveChunks(current => {
        const chunks = [...current.chunks, { key: `${next.hash}-${livePrevRaw.length}`, html: inlineLiveHTML(delta) }];
        if (chunks.length <= MAX_LIVE_CHUNKS) return { hash: current.hash, chunks };
        // 合并：全量重解析一次（修正跨 chunk 未闭合的内联语法），
        // 之后继续增量。频率 ≈ 每 MAX_LIVE_CHUNKS 帧一次。
        return { hash: current.hash, chunks: [{ key: `${next.hash}-full-${livePrevRaw.length}`, html: inlineLiveHTML(next.raw) }] };
      });
    });
  };

  onCleanup(() => {
    disposed = true;
    renderGeneration += 1;
    cancelLivePreview();
    pendingLiveBlock = undefined;
    livePrevRaw = "";
    // Release Shiki HTML strings retained in the store
    setBlockHtml(reconcile({} as Record<string, string>));
    setVisibleBlocks(reconcile([] as MarkdownBlock[]));
  });

  // Preload the highlighter eagerly so the first streaming delta does not
  // block while shiki downloads and instantiates its WASM engine.
  void getHi();

  // ── Raw blocks: derived from content + final ──
  const rawBlocks = createMemo(() => projectBlocks(props.content, !!props.final));

  // ── Rendered HTML cache (reactive store) ──
  const [blockHtml, setBlockHtml] = createStore<Record<string, string>>({});

  // ── Visible blocks (stale-while-revalidate for final transition) ──
  const [visibleBlocks, setVisibleBlocks] = createStore<MarkdownBlock[]>([]);

  // ── Helper: render stable blocks (streaming path) ──
  async function renderStreamingBlocks(
    currentBlocks: MarkdownBlock[],
    gen: number,
    isStale: () => boolean,
  ) {
    // Text blocks: render immediately with plain markdown (no Shiki overhead)
    const textBlocks = currentBlocks.filter(
      b => b.stable && b.kind === "text" && !blockHtml[b.key],
    );
    for (const block of textBlocks) {
      if (isStale()) return;
      setBlockHtml(s => { s[block.key] = renderFallbackHTML(block.raw); });
    }

    // Code blocks: render async with Shiki syntax highlighting
    const codeBlocks = currentBlocks.filter(
      b => b.stable && b.kind === "code" && !blockHtml[b.key],
    );
    if (codeBlocks.length === 0) return;

    let hi: Awaited<ReturnType<typeof getHi>>;
    try {
      hi = await getHi();
    } catch {
      // Shiki unavailable — render code blocks with fallback
      for (const block of codeBlocks) {
        if (isStale()) return;
        setBlockHtml(s => { s[block.key] = renderFallbackHTML(block.raw); });
      }
      return;
    }
    if (isStale() || !hi) return;

    for (const block of codeBlocks) {
      if (isStale()) return;
      try {
        setBlockHtml(s => { s[block.key] = renderBlockHTML(block.raw, hi); });
      } catch {
        try {
          setBlockHtml(s => { s[block.key] = renderFallbackHTML(block.raw); });
        } catch {
          // Leave as raw text
        }
      }
    }
  }

  // ── Main effect: react to block changes ──
  createEffect(
    () => rawBlocks(),
    (currentBlocks) => {
      const gen = ++renderGeneration;
      const isStale = () => disposed || gen !== renderGeneration;

      // Streaming: update blocks incrementally — only the last (live) block
      // changes every frame. Avoid full array replacement to prevent
      // <For> reconciling all historical blocks on every delta.
      if (!props.final) {
        scheduleLivePreview(currentBlocks.at(-1));
        setVisibleBlocks(s => {
          const prevLen = s.length;
          if (prevLen === 0) {
            // First render: push all blocks
            for (const b of currentBlocks) s.push(b);
          } else {
            // Update last block in-place (live text growth)
            if (currentBlocks[prevLen - 1] && s[prevLen - 1].hash !== currentBlocks[prevLen - 1].hash) {
              s[prevLen - 1] = currentBlocks[prevLen - 1];
            }
            // Append new stable blocks that just completed
            for (let i = prevLen; i < currentBlocks.length; i++) {
              s.push(currentBlocks[i]);
            }
          }
        });
        prevVisible = currentBlocks;
        void renderStreamingBlocks(currentBlocks, gen, isStale);
        return;
      }

      // Final: render all blocks async, keep previous content visible
      // until new HTML is ready (stale-while-revalidate).
      void (async () => {
        let hi: Awaited<ReturnType<typeof getHi>> | null = null;
        try {
          hi = await getHi();
        } catch {
          // Shiki unavailable — will use fallback below
        }
        if (isStale()) return;

        for (const block of currentBlocks) {
          if (isStale()) return;
          if (!block.stable) continue;
          try {
            const html = hi
              ? renderBlockHTML(block.raw, hi)
              : renderFallbackHTML(block.raw);
            setBlockHtml(s => { s[block.key] = html; });
          } catch {
            try {
              setBlockHtml(s => { s[block.key] = renderFallbackHTML(block.raw); });
            } catch {
              // Leave as raw text
            }
          }
        }

        if (isStale()) return;
        setVisibleBlocks(() => currentBlocks);
        prevVisible = currentBlocks;
        // The final render is authoritative (single "f" block covering the whole
        // answer). Drop the streaming-path block HTML (b0..bN) — it was only a
        // preview and keeping it would retain a second full copy of the answer.
        setBlockHtml(s => {
          for (const k of Object.keys(s)) {
            if (k !== "f") delete s[k];
          }
        });
        // Hydrate mermaid placeholders after DOM has the rendered blocks.
        // Must happen here (not in a separate effect) because the async
        // final rendering runs after mount — a parallel effect would fire
        // before blockHtml is written to the DOM.
        if (container) {
          queueMicrotask(() => hydrateMermaidPlaceholders(container));
        }
      })();
    },
  );

  function onCopyClick(e: MouseEvent) {
    const btn = (e.target as HTMLElement).closest?.(".code-copy-btn");
    if (!btn) return;
    e.preventDefault();
    const wrapper = btn.closest?.(".code-block-wrapper");
    if (!wrapper) return;
    const code = wrapper.querySelector("pre code")?.textContent ?? "";
    navigator.clipboard.writeText(code).catch(() => {});
    const original = btn.innerHTML;
    btn.innerHTML = `<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="#159555" stroke-width="2.5"><polyline points="20 6 9 17 4 12"/></svg>`;
    setTimeout(() => { btn.innerHTML = original; }, 1500);
  }

  return (
    <div ref={container} class={props.class} onClick={onCopyClick}>
      <For each={visibleBlocks}>
        {(block) => {
          const html = () => blockHtml[block.key];
          const isCode = () => block.kind === "code";
          return (
            <Show
              when={!isCode() || html()}
              fallback={
                <div
                  data-key={block.key}
                  data-hash={block.hash}
                  class="code-block-skeleton"
                  aria-busy="true"
                >
                  <pre><code>{block.raw}</code></pre>
                </div>
              }
            >
              <Show
                when={block.stable && html()}
                fallback={
                  <Show
                    when={!block.stable && block.kind === "text" && liveChunks().chunks.length > 0}
                    fallback={
                      <div
                        data-key={block.key}
                        data-hash={block.hash}
                        textContent={block.raw}
                      />
                    }
                  >
                    {/* 流式 live 块：增量解析（每帧只解析新增 delta），按 keyed
                        <For> 追加 span，已渲染 chunk 不被浏览器重解析；chunk
                        数超限时全量重解析合并一次（修正跨 chunk 未闭合语法）。
                        内容滞后至多一帧，delta 到达时不会回退成原始文字（消除
                        md↔raw 帧级交替闪烁）；未闭合语法由 parseInline 按
                        字面输出，不会产生破损 HTML。 */}
                    <div
                      data-key={block.key}
                      data-hash={block.hash}
                    >
                      <For each={liveChunks().chunks}>
                        {(chunk) => <span data-live-chunk={chunk.key} innerHTML={chunk.html} />}
                      </For>
                    </div>
                  </Show>
                }
              >
                <div
                  data-key={block.key}
                  data-hash={block.hash}
                  innerHTML={html()!}
                />
              </Show>
            </Show>
          );
        }}
      </For>
    </div>
  );
}
