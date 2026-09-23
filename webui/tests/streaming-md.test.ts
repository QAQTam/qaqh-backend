import { expect, test } from "bun:test";
import { Window } from "happy-dom";
import { createStreamingMarkdown } from "../src/lib/streaming-md";

const window = new Window({ url: "http://127.0.0.1:41234/" });
Object.defineProperties(globalThis, {
  window: { value: window, configurable: true },
  document: { value: window.document, configurable: true },
  HTMLElement: { value: window.HTMLElement, configurable: true },
  location: { value: window.location, configurable: true },
});

function render(markdown: string): HTMLElement {
  const host = document.createElement("div");
  const renderer = createStreamingMarkdown(host);
  renderer.writeNow(markdown);
  renderer.end();
  return host;
}

test("safe HTTP(S) links are normalized and isolated", () => {
  const host = render("[docs](https://example.com/a?b=1)");
  const link = host.querySelector("a");
  expect(link?.getAttribute("href")).toBe("https://example.com/a?b=1");
  expect(link?.getAttribute("target")).toBe("_blank");
  expect(link?.getAttribute("rel")).toBe("noopener noreferrer");
  expect(link?.getAttribute("referrerpolicy")).toBe("no-referrer");
});

test("active and non-HTTP markdown schemes are inert", () => {
  for (const value of [
    "javascript:alert(1)",
    "JaVaScRiPt:alert(1)",
    "java%0Ascript:alert(1)",
    "java&#x73;cript:alert(1)",
    "data:text/html,<script>alert(1)</script>",
    "vbscript:msgbox(1)",
    "file:///etc/passwd",
    "blob:https://example.com/id",
  ]) {
    const host = render(`[click](${value})`);
    const link = host.querySelector("a");
    const href = link?.getAttribute("href")?.toLowerCase() ?? "";
    for (const scheme of ["javascript:", "data:", "vbscript:", "file:", "blob:"]) {
      expect(href.startsWith(scheme)).toBe(false);
    }
    expect(host.querySelector("script")).toBeNull();
  }
});

test("images load only from the gateway origin", () => {
  const sameOrigin = render("![ok](/assets/pixel.png)");
  expect(sameOrigin.querySelector("img")?.getAttribute("src")).toBe(
    "http://127.0.0.1:41234/assets/pixel.png",
  );

  for (const value of [
    "https://evil.example/pixel.png",
    "//evil.example/pixel.png",
    "data:image/svg+xml,<svg onload=alert(1)>",
    "javascript:alert(1)",
  ]) {
    const host = render(`![blocked](${value})`);
    expect(host.querySelector("img")?.hasAttribute("src")).toBe(false);
  }
});

test("raw HTML and script-looking model output stays text", () => {
  const host = render(
    '<img src=x onerror="globalThis.pwned=true"><script>globalThis.pwned=true</script><svg onload="globalThis.pwned=true">',
  );
  expect(host.querySelector("img")).toBeNull();
  expect(host.querySelector("script")).toBeNull();
  expect(host.querySelector("svg")).toBeNull();
  expect(host.textContent).toContain("<script>");
});
