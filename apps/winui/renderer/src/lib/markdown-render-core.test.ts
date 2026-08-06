import { describe, expect, it } from "vitest";
import { cleanMarkedHTML, parseMarkdownCore } from "./markdown-render-core";

const fakeHi = {
  codeToHtml: (text: string) => `<pre class="shiki"><code>${text}</code></pre>`,
} as never;

describe("markdown-render-core", () => {
  it("cleans shiki background-color and tabindex from pre blocks", () => {
    const html = `<pre style="background-color:#fff;color:#000" tabindex="0"><code>x</code></pre>`;
    expect(cleanMarkedHTML(html)).toBe(`<pre style="color:#000"><code>x</code></pre>`);
  });

  it("parses plain markdown without a highlighter", () => {
    const html = parseMarkdownCore("**bold** and `code`", "github-light");
    expect(html).toContain("<strong>bold</strong>");
    expect(html).toContain("<code>code</code>");
  });

  it("emits mermaid placeholder and highlighted code with a highlighter", () => {
    const html = parseMarkdownCore(
      "```mermaid\ngraph TD\nA-->B\n```\n\n```ts\nconst a = 1;\n```",
      "github-light",
      fakeHi,
    );
    expect(html).toContain("mermaid-placeholder");
    expect(html).toContain("shiki");
  });

  it("keeps code literal when no highlighter is available", () => {
    const html = parseMarkdownCore("```ts\nconst a = 1;\n```", "github-dark");
    expect(html).toContain("const a = 1");
    expect(html).not.toContain("shiki");
  });

  it("renders identical output regardless of theme parameter (theme only affects shiki)", () => {
    const light = parseMarkdownCore("**same**", "github-light", fakeHi);
    const dark = parseMarkdownCore("**same**", "github-dark", fakeHi);
    expect(light).toBe(dark);
  });
});
