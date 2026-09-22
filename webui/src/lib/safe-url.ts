/**
 * URL policy for model-controlled Markdown.
 *
 * Markdown is untrusted input. Links may leave the gateway only through
 * ordinary HTTP(S), while images may load only from the gateway origin.
 */

const CONTROL_OR_WHITESPACE_ESCAPE = /[\u0000-\u001f\u007f]/;

function parseHttpUrl(value: string, base: string): URL | null {
  const candidate = value.trim();
  if (!candidate || CONTROL_OR_WHITESPACE_ESCAPE.test(candidate)) return null;

  let parsed: URL;
  try {
    parsed = new URL(candidate, base);
  } catch {
    return null;
  }
  if (parsed.protocol !== "http:" && parsed.protocol !== "https:") return null;
  if (parsed.username || parsed.password) return null;
  return parsed;
}

function baseUrl(): string {
  return globalThis.location?.origin ?? "http://127.0.0.1";
}

/** Returns a normalized HTTP(S) href, or null when the link must be inert. */
export function safeMarkdownHref(value: string): string | null {
  return parseHttpUrl(value, baseUrl())?.href ?? null;
}

/** Returns a normalized same-origin image URL, or null when it must not load. */
export function safeMarkdownImageSrc(value: string): string | null {
  const origin = new URL(baseUrl()).origin;
  const parsed = parseHttpUrl(value, origin);
  return parsed?.origin === origin ? parsed.href : null;
}
