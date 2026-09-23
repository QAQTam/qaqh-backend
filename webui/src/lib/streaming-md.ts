/**
 * Streaming markdown block renderer.
 *
 * Architecture note (measured, not assumed): driving an unbounded, append-only
 * stream through Solid's reactive arrays is the wrong tool. Every append makes
 * the framework re-visit the container array, so a paragraph with thousands of
 * inline runs ends up super-linear (measured: 32k→128k chars cost x3.4~x5.7 per
 * doubling with `For`, worse with `Repeat` for element children).
 *
 * The parser's own renderer protocol is append-only by construction, so the
 * block owns its DOM subtree and appends into it: O(1) per token, linear in the
 * stream. Solid still owns *whether* the block exists, its placement, and every
 * interactive surface around it (tool cards, panels, composer).
 */
import {
  default_renderer,
  HREF,
  parser,
  parser_end,
  parser_write,
  SRC,
  type Any_Renderer,
  type Attr,
  type Default_Renderer_Data,
  type Parser,
  type Renderer,
} from "streaming-markdown";
import { safeMarkdownHref, safeMarkdownImageSrc } from "./safe-url";

export type StreamingStats = {
  chunks: number;
  parses: number;
  chars: number;
  parseMs: number;
};

export type StreamingMarkdown = {
  /** Queue a delta; coalesced into one parse per animation frame. */
  write: (chunk: string) => void;
  /** Parse immediately (non-streaming paths, tests). */
  writeNow: (chunk: string) => void;
  /** Parse whatever is queued right now; returns the parsed length. */
  flush: () => number;
  /** Flush and finish the markdown stream (block sealed). */
  end: () => void;
  /** Drop all content and start a new stream (block_checkpoint replacement). */
  reset: () => void;
  element: () => HTMLElement;
  stats: () => StreamingStats;
};

function safeRenderer(root: HTMLElement): Renderer<Default_Renderer_Data> {
  const base = default_renderer(root);
  const setAttr = (data: Default_Renderer_Data, type: Attr, value: string) => {
    if (type === HREF) {
      const node = data.nodes[data.index];
      const href = safeMarkdownHref(value);
      if (href) {
        node.setAttribute("href", href);
        node.setAttribute("target", "_blank");
        node.setAttribute("rel", "noopener noreferrer");
        node.setAttribute("referrerpolicy", "no-referrer");
      } else {
        node.removeAttribute("href");
        node.setAttribute("aria-disabled", "true");
        node.classList.add("md-link-blocked");
      }
      return;
    }
    if (type === SRC) {
      const node = data.nodes[data.index];
      const src = safeMarkdownImageSrc(value);
      if (src) {
        node.setAttribute("src", src);
        node.setAttribute("referrerpolicy", "no-referrer");
      } else {
        node.removeAttribute("src");
        node.setAttribute("alt", "blocked image");
        node.classList.add("md-image-blocked");
      }
      return;
    }
    base.set_attr(data, type, value);
  };

  return {
    data: base.data,
    add_token: base.add_token,
    end_token: base.end_token,
    add_text: base.add_text,
    set_attr: setAttr,
  };
}

export function createStreamingMarkdown(host: HTMLElement): StreamingMarkdown {
  const element = host;
  let renderer: Any_Renderer = safeRenderer(element);
  let parserInstance: Parser = parser(renderer);

  let pending: string[] = [];
  let scheduled = false;
  let chunks = 0;
  let parses = 0;
  let chars = 0;
  let parseMs = 0;

  const parseNow = (chunk: string) => {
    if (!chunk) return;
    const started = performance.now();
    parser_write(parserInstance, chunk);
    parseMs += performance.now() - started;
    parses += 1;
  };

  const flush = (): number => {
    scheduled = false;
    if (pending.length === 0) return 0;
    const joined = pending.length === 1 ? pending[0]! : pending.join("");
    pending = [];
    parseNow(joined);
    return joined.length;
  };

  return {
    element: () => element,
    write: (chunk) => {
      if (!chunk) return;
      chunks += 1;
      chars += chunk.length;
      pending.push(chunk);
      if (!scheduled) {
        scheduled = true;
        requestAnimationFrame(() => {
          flush();
        });
      }
    },
    writeNow: (chunk) => {
      if (!chunk) return;
      chunks += 1;
      chars += chunk.length;
      parseNow(chunk);
    },
    flush,
    end: () => {
      flush();
      parser_end(parserInstance);
    },
    reset: () => {
      pending = [];
      scheduled = false;
      parser_end(parserInstance);
      element.replaceChildren();
      renderer = safeRenderer(element);
      parserInstance = parser(renderer);
      chunks = 0;
      parses = 0;
      chars = 0;
      parseMs = 0;
    },
    stats: () => ({ chunks, parses, chars, parseMs }),
  };
}
