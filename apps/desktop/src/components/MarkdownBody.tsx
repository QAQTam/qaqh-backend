import { createEffect, createMemo, createStore, For, onCleanup, Show } from "solid-js";
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

// ── Block splitting (replaces Web Worker + projectBlocksOffThread) ──

function blockHash(raw: string): string {
  if (raw.length <= 24) return String(raw.length);
  return `${raw.length}:${raw.slice(0, 10)}…${raw.slice(-10)}`;
}

function projectBlocks(text: string, final: boolean): MarkdownBlock[] {
  if (!text) return [];
  if (final) {
    return [{ key: "f", hash: blockHash(text), raw: text, stable: true }];
  }

  const tokens = marked.lexer(text);
  let tailIdx = tokens.length;
  while (tailIdx > 0 && tokens[tailIdx - 1]?.type === "space") tailIdx--;
  if (tailIdx === 0) {
    return [{ key: "l0", hash: blockHash(text), raw: text, stable: false }];
  }
  tailIdx--;

  const lastToken = tokens[tailIdx];
  const lastIsCompleteTable =
    lastToken?.type === "table" &&
    (lastToken as any).align != null &&
    (lastToken as any).align.length > 0;
  if (lastIsCompleteTable) tailIdx++;

  const blocks: MarkdownBlock[] = [];
  for (let i = 0; i < tailIdx; i++) {
    const token = tokens[i];
    if (!token || token.type === "space") continue;
    let raw = token.raw;
    while (i + 1 < tailIdx && tokens[i + 1]?.type === "space") raw += tokens[++i]!.raw;
    blocks.push({ key: `b${blocks.length}`, hash: blockHash(raw), raw, stable: true });
  }

  if (tailIdx < tokens.length) {
    const liveRaw = tokens.slice(tailIdx).map(t => t.raw).join("");
    blocks.push({ key: `l${blocks.length}`, hash: blockHash(liveRaw), raw: liveRaw, stable: false });
  }

  return blocks;
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
  let prevVisible: MarkdownBlock[] = [];

  onCleanup(() => {
    disposed = true;
    renderGeneration += 1;
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
    const pending = currentBlocks.filter(
      b => b.stable && !blockHtml[b.key],
    );
    if (pending.length === 0) return;

    let hi: Awaited<ReturnType<typeof getHi>>;
    try {
      hi = await getHi();
    } catch {
      // Shiki unavailable — render with fallback
      for (const block of pending) {
        if (isStale()) return;
        setBlockHtml(s => { s[block.key] = renderFallbackHTML(block.raw); });
      }
      return;
    }
    if (isStale() || !hi) return;

    for (const block of pending) {
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

      // Streaming: show blocks immediately, render stable ones async
      if (!props.final) {
        setVisibleBlocks(() => currentBlocks);
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
      })();
    },
  );

  // ── Mermaid hydration (deferred until next microtask after DOM update) ──
  createEffect(
    () => props.final,
    (final) => {
      if (final && container) {
        queueMicrotask(() => hydrateMermaidPlaceholders(container));
      }
    },
    { defer: true },
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
          return (
            <Show
              when={block.stable && html()}
              fallback={
                <div
                  data-key={block.key}
                  data-hash={block.hash}
                  textContent={block.raw}
                />
              }
            >
              <div
                data-key={block.key}
                data-hash={block.hash}
                innerHTML={html()!}
              />
            </Show>
          );
        }}
      </For>
    </div>
  );
}
