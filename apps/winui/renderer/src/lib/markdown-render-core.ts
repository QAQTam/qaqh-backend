// Markdown 渲染共享核心（主线程与 Worker 共用）。
//
// 设计：marked + shiki（占总耗时大头，纯字符串管道）在此模块内实现，
// 可在 Web Worker 中运行；katex（renderMath，需要 DOM）留在主线程收尾。
// theme 显式传入而非 detectTheme()：Worker 无 document。

import { marked, Renderer } from "marked";
import { createHighlighter, createOnigurumaEngine } from "shiki";
import { createMermaidPlaceholder, MERMAID_LANG } from "./mermaid-render";

export type RenderTheme = "github-light" | "github-dark";
export const RENDER_THEMES: RenderTheme[] = ["github-light", "github-dark"];

export type Highlighter = Awaited<ReturnType<typeof createHighlighter>>;

// ── Shiki 单例（主线程与 Worker 各自持有；失败后重置可重试）──

let hiPromise: Promise<Highlighter> | null = null;

export function getShiki(): Promise<Highlighter> {
  if (!hiPromise) {
    hiPromise = createHighlighter({
      themes: RENDER_THEMES,
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

// ── Cached Renderer（避免 per-block 分配；theme 显式传入）──

let cachedRenderer: Renderer | null = null;
let cachedTheme: string | null = null;

export function buildMarkdownRenderer(hi: Highlighter, theme: RenderTheme): Renderer {
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

// ── Markdown 解析核心（无 katex：Worker 无 DOM）──

export function cleanMarkedHTML(html: string): string {
  return html
    .replace(
      /(<pre\b[^>]*style=")([^"]*)(")/gi,
      (_, before, styles, after) =>
        before + styles.replace(/background-color\s*:\s*[^;]+;?/gi, "") + after,
    )
    .replace(/<pre\b([^>]*)\s+tabindex="0"([^>]*)>/gi, "<pre$1$2>");
}

export function parseMarkdownCore(raw: string, theme: RenderTheme, hi?: Highlighter): string {
  const renderer = hi ? buildMarkdownRenderer(hi, theme) : undefined;
  const html = marked.parse(raw, { async: false, gfm: true, breaks: false, renderer });
  if (typeof html !== "string") return "";
  return cleanMarkedHTML(html);
}

// ── Worker 管道（Worker 入口调用；测试可直接调用验证）──

export async function renderMarkdownInWorkerScope(
  raw: string,
  theme: RenderTheme,
): Promise<string> {
  const hi = await getShiki();
  return parseMarkdownCore(raw, theme, hi);
}
