/**
 * Mermaid diagram renderer — lazy-loads mermaid.js on first use.
 *
 * Protocol:
 *   1. MarkdownBody.renderer.code intercepts ```mermaid blocks
 *   2. Outputs <div class="mermaid-placeholder"> with <script> child
 *   3. After DOM patch, hydrateMermaidPlaceholders() scans for placeholders
 *   4. Each placeholder is rendered to SVG via mermaid.render()
 *
 * Diagram text travels through a <script type="text/mermaid"> child
 * element so newlines survive the innerHTML round-trip.  HTML data-*
 * attributes normalise U+000A to space per the HTML5 parsing algorithm,
 * which silently corrupts multi-line Mermaid source.
 */

let mermaidPromise: Promise<typeof import("mermaid")> | null = null;

function getMermaid(): Promise<typeof import("mermaid")> {
  if (!mermaidPromise) {
    mermaidPromise = import("mermaid").then(mod => {
      mod.default.initialize({
        startOnLoad: false,
        theme: "base",
        themeVariables: {
          primaryColor: "#f4f2ef",
          primaryTextColor: "#1e1e1e",
          primaryBorderColor: "#c8c4bc",
          lineColor: "#c8c4bc",
          secondaryColor: "#ebe8e3",
          tertiaryColor: "#e0dcd6",
          // flowchart
          nodeBorder: "#c8c4bc",
          nodeTextColor: "#1e1e1e",
          // sequence
          actorBorder: "#c8c4bc",
          actorBkg: "#f4f2ef",
          actorTextColor: "#1e1e1e",
          signalColor: "#1e1e1e",
          signalTextColor: "#1e1e1e",
          // edge labels
          edgeLabelBackground: "#f4f2ef",
          // general
          fontFamily: "'MiSans','Noto Sans SC','Inter',-apple-system,BlinkMacSystemFont,'Segoe UI',sans-serif",
          fontSize: "11px",
        },
        flowchart: {
          useMaxWidth: true,
          htmlLabels: true,
          padding: 8,
          nodeSpacing: 16,
          rankSpacing: 25,
        },
        sequence: {
          useMaxWidth: true,
          boxMargin: 8,
          mirrorActors: false,
        },
        deterministicIds: true,
        fontFamily: "system-ui, sans-serif",
      });
      return mod;
    }).catch(err => {
      mermaidPromise = null;
      throw err;
    });
  }
  return mermaidPromise;
}

export const MERMAID_LANG = "mermaid";

let renderCounter = 0;

/**
 * Render a single Mermaid diagram to SVG.
 * Returns the SVG string, or an error HTML div on failure.
 */
export async function renderMermaid(text: string): Promise<string> {
  const mermaid = await getMermaid();
  const id = `mermaid-${++renderCounter}-${Date.now().toString(36)}`;
  try {
    const { svg } = await mermaid.default.render(id, text);
    return svg;
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    // Strip mermaid's verbose parse error prefix
    const short = msg.replace(/^Parse error on line \d+:\s*/, "").slice(0, 200);
    return `<div class="mermaid-error" style="padding:16px;color:#c44;font-size:13px;border:1px solid #e0d5c1;border-radius:8px;background:#fefaf2;">Diagram error: ${short}</div>`;
  }
}

/**
 * Create a placeholder div for a Mermaid diagram.
 * Called from MarkdownBody's marked renderer.code callback during streaming.
 *
 * The diagram text is stored inside a &lt;script type=&quot;text/mermaid&quot;&gt; child
 * element so that newlines survive the innerHTML round-trip.  HTML data-*
 * attributes normalise U+000A to space per the HTML5 parsing algorithm,
 * which silently corrupts multi-line Mermaid source.
 */
export function createMermaidPlaceholder(text: string): string {
  // &lt;script&gt; content is raw text in HTML; the only sequence that
  // prematurely closes the element is &lt;/script&gt;.
  const safe = text.replace(/<\/script>/gi, "<\\/script>");
  return (
    `<div class="mermaid-placeholder"` +
    ` style="min-height:120px;background:var(--md-code-bg,#f5efe0);border-radius:8px;` +
    ` display:flex;align-items:center;justify-content:center;margin:1em auto;overflow:hidden;">` +
    `<script type="text/mermaid">${safe}</script>` +
    `<span style="color:var(--md-text-secondary,#8b8578);font-size:13px;">` +
    `Rendering diagram…</span></div>`
  );
}

/**
 * Scan root for Mermaid placeholders and hydrate them.
 * Call after MarkdownBody patches the DOM.
 * Returns a cleanup function.
 */
export async function hydrateMermaidPlaceholders(root: HTMLElement): Promise<() => void> {
  const placeholders = root.querySelectorAll<HTMLElement>(".mermaid-placeholder");
  if (placeholders.length === 0) return () => {};

  // Lazy-load Mermaid
  try {
    await getMermaid();
  } catch {
    // Mermaid failed to load — leave placeholders as-is
    return () => {};
  }

  const rendered: HTMLElement[] = [];

  for (const ph of placeholders) {
    if (ph.dataset.mermaidRendered === "1") continue;

    // Read diagram text from <script type="text/mermaid"> child.
    // textContent preserves newlines correctly (unlike data-* attributes
    // which are subject to HTML5 attribute-value newline normalisation).
    const script = ph.querySelector<HTMLScriptElement>('script[type="text/mermaid"]');
    const text = script?.textContent;
    if (!text) continue;

    ph.dataset.mermaidRendered = "1";
    try {
      const svg = await renderMermaid(text);
      ph.innerHTML = svg;
      ph.style.minHeight = "auto";
      ph.style.background = "transparent";
      ph.style.display = "block";
      ph.style.margin = "1em auto";
      ph.style.maxWidth = "100%";
      ph.style.maxHeight = "520px";
      ph.style.overflow = "auto";
      // Force SVG to fill container width.  setAttribute("width","100%")
      // trumps CSS-only max-width and prevents the browser from using
      // the viewBox height to upsample a tall diagram into huge dimensions.
      const svgEl = ph.querySelector("svg");
      if (svgEl) {
        svgEl.removeAttribute("width");
        svgEl.removeAttribute("height");
        svgEl.setAttribute("width", "100%");
        svgEl.style.height = "auto";
      }
      rendered.push(ph);
    } catch {
      ph.innerHTML = `<div style="padding:16px;color:#c44;">Render failed</div>`;
    }
  }

  return () => {
    for (const el of rendered) {
      el.innerHTML = "";
      el.dataset.mermaidRendered = undefined;
    }
  };
}
