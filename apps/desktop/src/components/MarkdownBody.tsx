import { createEffect, onCleanup } from "solid-js";
import { marked, Renderer } from "marked";
import { createHighlighter, createOnigurumaEngine } from "shiki";
import renderMathInElement from "katex/contrib/auto-render";
import {
  hydratePlaceholders,
  destroyAllGraphs,
  ATTR_TYPE,
  ATTR_RAW,
  PLACEHOLDER_CLASS,
} from "../lib/graph-renderer";

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

// ── Graph DSL placeholder ──

const GRAPH_LANGS = new Set(["mindmap", "graph"]);

function renderGraphPlaceholder(text: string, lang: string): string {
  const encoded = encodeURIComponent(text);
  return (
    `<div class="${PLACEHOLDER_CLASS}" ${ATTR_TYPE}="${lang}" ${ATTR_RAW}="${encoded}"` +
    ` style="min-height:200px;background:var(--md-code-bg,#1e1e2e);border-radius:8px;` +
    ` display:flex;align-items:center;justify-content:center;margin:1em 0;overflow:hidden;">` +
    `<span style="color:var(--md-text-secondary,#888);font-size:13px;">Rendering ${lang}…</span>` +
    `</div>`
  );
}

// ── P0: Block projection ──

interface MarkdownBlock {
  key: string;
  hash: string;
  raw: string;
  stable: boolean;   // true = parsed markdown block; false = live streaming tail
  html?: string;      // cached rendered HTML (stable blocks only)
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
    // Intercept graph DSL blocks — render as placeholder divs
    if (lang === "mindmap" || lang === "graph") {
      return renderGraphPlaceholder(text, lang);
    }

    const langId = !lang ? "text"
      : lang === "h" ? "c"
      : lang === "hpp" ? "cpp"
      : lang;
    try {
      return hi.codeToHtml(text, { lang: langId, theme });
    } catch {
      return `<pre><code>${text}</code></pre>`;
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
    const paced = paceText(liveRaw);
    blocks.push({ key: `l${blocks.length}`, hash: blockHash(paced), raw: paced, stable: false });
  }

  return blocks;
}

// ── P2: Word-boundary pacing ──

const TEXT_SNAP = /[\s.,!?;:)\]]/;

/** Pace live text: hide trailing partial words for smoother reveal. */
function paceText(text: string): string {
  if (text.length < 60) return text;
  // Search backwards for a word boundary within last 12 chars
  const start = Math.max(0, text.length - 12);
  for (let i = text.length - 1; i >= start; i--) {
    if (TEXT_SNAP.test(text[i]!)) return text.slice(0, i + 1);
  }
  return text;
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
    destroyAllGraphs();
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

      const blocks = projectBlocks(text, final, prevBlocks);

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
      // Hydrate any graph placeholders in the final HTML
      if (!isStale()) void hydratePlaceholders(container);
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
    // Hydrate any newly-appeared graph placeholders
    void hydratePlaceholders(container);
    prevBlocks = blocks;
    })();
  });

  return <div ref={container} class={props.class} />;
}
