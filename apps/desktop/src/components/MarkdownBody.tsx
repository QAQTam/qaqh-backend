import { createEffect, onCleanup } from "solid-js";
import { marked, Renderer } from "marked";
import { createHighlighter, createOnigurumaEngine } from "shiki";
import renderMathInElement from "katex/contrib/auto-render";
import {
  hydrateMermaidPlaceholders,
  createMermaidPlaceholder,
  MERMAID_LANG,
} from "../lib/mermaid-render";

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

// ── P0: Block projection ──

interface MarkdownBlock {
  key: string;
  hash: string;
  raw: string;
  stable: boolean;   // true = parsed markdown block; false = live streaming tail
  html?: string;      // cached rendered HTML (stable blocks only)
}

let projectionWorker: Worker | null | undefined;
let projectionId = 0;
type ProjectionRequest = {
  id: number;
  text: string;
  final: boolean;
  resolve: (blocks: MarkdownBlock[]) => void;
};
let activeProjection: ProjectionRequest | undefined;
let queuedProjection: ProjectionRequest | undefined;

function dispatchProjection(request: ProjectionRequest) {
  activeProjection = request;
  projectionWorker!.postMessage({ id: request.id, text: request.text, final: request.final });
}

function abortProjectionQueue() {
  activeProjection?.resolve([]);
  queuedProjection?.resolve([]);
  activeProjection = undefined;
  queuedProjection = undefined;
}

function projectBlocksOffThread(text: string, final: boolean): Promise<MarkdownBlock[]> {
  // Vitest's jsdom Worker shim does not execute module workers. Keep its DOM
  // contract synchronous while production Chromium uses the worker below.
  if (import.meta.env.MODE === "test" || typeof Worker === "undefined") {
    return Promise.resolve(projectBlocks(text, final, []));
  }
  if (projectionWorker === undefined) {
    projectionWorker = new Worker(new URL("./markdownProjection.worker.ts", import.meta.url), { type: "module" });
    projectionWorker.onmessage = ({ data }: MessageEvent<{ id: number; blocks: MarkdownBlock[] }>) => {
      const request = activeProjection;
      if (!request || request.id !== data.id) return;
      activeProjection = undefined;
      request.resolve(data.blocks);
      const next = queuedProjection;
      queuedProjection = undefined;
      if (next && projectionWorker) dispatchProjection(next);
    };
    projectionWorker.onerror = () => {
      projectionWorker?.terminate();
      projectionWorker = null;
      abortProjectionQueue();
    };
  }
  if (!projectionWorker) return Promise.resolve(projectBlocks(text, final, []));
  const id = ++projectionId;
  return new Promise(resolve => {
    const request = { id, text, final, resolve };
    if (!activeProjection) {
      dispatchProjection(request);
      return;
    }
    // The current render generation will ignore superseded results. Retain
    // only the newest projection so a fast model cannot create an unbounded
    // backlog of full-document lexer jobs in the worker.
    queuedProjection?.resolve([]);
    queuedProjection = request;
  });
}

function canProjectOffThread(): boolean {
  return import.meta.env.MODE !== "test" && typeof Worker !== "undefined";
}

function reuseCachedHtml(blocks: MarkdownBlock[], previous: MarkdownBlock[]): MarkdownBlock[] {
  return blocks.map(block => ({
    ...block,
    html: block.stable ? previous.find(candidate => candidate.key === block.key && candidate.hash === block.hash)?.html : undefined,
  }));
}

function blockHash(raw: string): string {
  if (raw.length <= 24) return String(raw.length);
  return `${raw.length}:${raw.slice(0, 10)}…${raw.slice(-10)}`;
}

/** Build a marked Renderer with Shiki highlighting. */
function buildRenderer(hi: Awaited<ReturnType<typeof getHi>>) {
  const theme = detectTheme();
  const renderer = new Renderer();
  renderer.code = ({ text, lang }) => {
    // Mermaid diagrams → placeholder div, hydrated after DOM patch
    if (lang === MERMAID_LANG) {
      return createMermaidPlaceholder(text);
    }

    const langId = !lang ? "text"
      : lang === "h" ? "c"
      : lang === "hpp" ? "cpp"
      : lang;
    const label = lang ? `<span class="code-lang-label">${lang}</span>` : "";
    try {
      const highlighted = hi.codeToHtml(text, { lang: langId, theme });
      return `<div class="code-block-wrapper">${label}${highlighted}</div>`;
    } catch {
      return `<div class="code-block-wrapper">${label}<pre><code>${text}</code></pre></div>`;
    }
  };
  return renderer;
}

function parseMarkdown(raw: string, renderer?: Renderer): string {
  const html = marked.parse(raw, {
    async: false,
    gfm: true,
    breaks: false,
    renderer,
  });
  if (typeof html !== "string") return "";
  // Strip Shiki's inline background-color so CSS variables control the theme.
  const cleaned = html
    .replace(
      /(<pre\b[^>]*style=")([^"]*)(")/gi,
      (_, before, styles, after) =>
        before + styles.replace(/background-color\s*:\s*[^;]+;?/gi, "") + after,
    )
    // Strip Shiki's tabindex to prevent code blocks from stealing focus
    // and interfering with native text selection behavior.
    .replace(/<pre\b([^>]*)\s+tabindex="0"([^>]*)>/gi, "<pre$1$2>");
  return renderMath(cleaned);
}

/** Render TeX only after Markdown is parsed, so code fences remain literal. */
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

/** Render a single stable block's raw markdown to HTML. */
function renderBlockHTML(raw: string, hi: Awaited<ReturnType<typeof getHi>>): string {
  return parseMarkdown(raw, buildRenderer(hi));
}

/** Render Markdown without Shiki when the highlighter is unavailable. */
function renderFallbackHTML(raw: string): string {
  return parseMarkdown(raw);
}

/** P0: Split markdown text into stable blocks + live streaming tail. */
function projectBlocks(text: string, final: boolean, prev: MarkdownBlock[]): MarkdownBlock[] {
  if (final) {
    const hash = blockHash(text);
    const cached = prev[0];
    if (cached && cached.key === "f" && cached.hash === hash && cached.html) {
      return [cached];
    }
    return [{ key: "f", hash, raw: text, stable: true }];
  }

  // Streaming: use marked.lexer() to find block boundaries
  const tokens = marked.lexer(text);

  // Find the last non-space token — this is the "live" tail
  let tailIdx = tokens.length;
  while (tailIdx > 0 && tokens[tailIdx - 1]?.type === "space") tailIdx--;

  if (tailIdx === 0) {
    return [{ key: "l0", hash: blockHash(text), raw: text, stable: false }];
  }
  tailIdx--; // index of the last content token

  // Promote a structurally-complete table to stable so it renders
  // as HTML immediately during streaming instead of waiting for final.
  const lastToken = tokens[tailIdx];
  const lastIsCompleteTable =
    lastToken?.type === "table" &&
    (lastToken as any).align != null &&
    (lastToken as any).align.length > 0;
  if (lastIsCompleteTable) {
    tailIdx++; // move table from live tail into stable zone
  }

  const blocks: MarkdownBlock[] = [];

  // Stable blocks: all tokens before the live tail
  for (let i = 0; i < tailIdx; i++) {
    const token = tokens[i];
    if (!token || token.type === "space") continue;
    let raw = token.raw;
    // Absorb trailing whitespace tokens into this block
    while (i + 1 < tailIdx && tokens[i + 1]?.type === "space") raw += tokens[++i]!.raw;
    const key = `b${blocks.length}`;
    const hash = blockHash(raw);
    // Reuse cached HTML if this block hasn't changed
    const cached = prev.find(p => p.key === key && p.hash === hash);
    blocks.push({ key, hash, raw, stable: true, html: cached?.html });
  }

  // Live tail: raw text of the last token(s), possibly incomplete.
  // When the table was promoted above, live tail is empty.
  if (tailIdx < tokens.length) {
    const liveRaw = tokens.slice(tailIdx).map(t => t.raw).join("");
    blocks.push({ key: `l${blocks.length}`, hash: blockHash(liveRaw), raw: liveRaw, stable: false });
  }

  return blocks;
}

// ── P1: DOM patching via data-key + data-hash ──

/** Create a wrapper div for a stable block's rendered HTML. */
function createStableEl(block: MarkdownBlock): HTMLDivElement {
  const el = document.createElement("div");
  el.dataset.key = block.key;
  el.dataset.hash = block.hash;
  el.innerHTML = block.html ?? "";
  return el;
}

/** Create a text node wrapper for the live tail. */
function createLiveEl(block: MarkdownBlock): HTMLDivElement {
  const el = document.createElement("div");
  el.dataset.key = block.key;
  el.dataset.hash = block.hash;
  el.textContent = block.raw;
  return el;
}

/** P1: Patch container DOM children to match blocks array. */
function patchDOM(container: HTMLDivElement, blocks: MarkdownBlock[]) {
  // Clean up orphan text nodes left by earlier container.textContent assignments.
  for (let i = container.childNodes.length - 1; i >= 0; i--) {
    if (container.childNodes[i]!.nodeType === Node.TEXT_NODE) {
      container.childNodes[i]!.remove();
    }
  }

  for (let i = 0; i < blocks.length; i++) {
    const block = blocks[i]!;
    const existing = container.children[i] as HTMLDivElement | undefined;

    // Skip if existing child matches key + hash
    if (
      existing instanceof HTMLDivElement &&
      existing.dataset.key === block.key &&
      existing.dataset.hash === block.hash
    ) {
      // For live blocks, update textContent in place (minimal flicker)
      if (!block.stable && existing.textContent !== block.raw) {
        existing.textContent = block.raw;
      }
      continue;
    }

    // Need to create or replace this child
    if (block.stable && block.html) {
      const el = createStableEl(block);
      if (existing) {
        existing.replaceWith(el);
      } else {
        container.appendChild(el);
      }
    } else {
      // Live blocks and stable blocks without HTML yet: show raw text.
      const el = createLiveEl(block);
      if (existing) {
        existing.replaceWith(el);
      } else {
        container.appendChild(el);
      }
    }
  }

  // Remove excess children
  while (container.children.length > blocks.length) {
    container.lastElementChild?.remove();
  }
}

// ── Component ──

interface MarkdownBodyProps {
  content: string;
  class?: string;
  final?: boolean;
}

export default function MarkdownBody(props: MarkdownBodyProps) {
  let container!: HTMLDivElement;
  let prevBlocks: MarkdownBlock[] = [];
  let renderGeneration = 0;
  let disposed = false;
  let lastDeps = "";

  onCleanup(() => {
    disposed = true;
    renderGeneration += 1;
  });

  // Preload the highlighter eagerly so the first streaming delta does not
  // block for ~500 ms while shiki downloads and instantiates its WASM engine.
  void getHi();

  createEffect(
    // Use a string key so SolidJS compares by value, not array reference.
    // Returning an array here would cause the effect to re-fire on every
    // parent re-render, even when the markdown text hasn't changed.
    () => JSON.stringify([props.content, props.final]),
    (serializedDeps) => { void (async () => {
      const [text, final] = JSON.parse(serializedDeps) as [string, boolean];
      const nextDepsKey = `${text}|${final}`;
      if (nextDepsKey === lastDeps) return;
      lastDeps = nextDepsKey;

      const generation = ++renderGeneration;
      const isStale = () => disposed || generation !== renderGeneration;

      if (!text) {
        container.innerHTML = "";
        container.classList.remove("final");
        prevBlocks = [];
        return;
      }

      const projected = canProjectOffThread()
        ? await projectBlocksOffThread(text, final)
        : projectBlocks(text, final, []);
      const blocks = reuseCachedHtml(projected, prevBlocks);
      if (isStale()) return;

    if (final) {
      let html: string;
      try {
        const hi = await getHi();
        if (isStale()) return;
        html = renderBlockHTML(blocks[0]!.raw, hi);
      } catch {
        if (isStale()) return;
        try {
          html = renderFallbackHTML(blocks[0]!.raw);
        } catch {
          if (!isStale()) prevBlocks = blocks;
          return;
        }
      }
      if (isStale()) return;
      blocks[0]!.html = html;
      container.replaceChildren(createStableEl(blocks[0]!));
      container.classList.add("final");
      if (!isStale()) void hydrateMermaidPlaceholders(container);
      prevBlocks = blocks;
      return;
    }

    container.classList.remove("final");
    const needsRender = blocks.some(block => block.stable && !block.html);
    if (needsRender) {
      let hi: Awaited<ReturnType<typeof getHi>>;
      try {
        hi = await getHi();
      } catch {
        if (!isStale()) prevBlocks = blocks;
        return;
      }
      if (isStale() || !hi) return;
      for (const block of blocks) {
        if (block.stable && !block.html) {
          block.html = renderBlockHTML(block.raw, hi);
        }
      }
    }

    if (isStale()) return;
    patchDOM(container, blocks);
    void hydrateMermaidPlaceholders(container);
    prevBlocks = blocks;
    })();
  });

  return <div ref={container} class={props.class} />;
}
