// @vitest-environment jsdom

import { createSignal } from "solid-js";
import { render } from "@solidjs/web";
import { expect, it, vi } from "vitest";

const shikiState = vi.hoisted(() => {
  type Highlighter = { codeToHtml: (text: string) => string };
  let resolve!: (value: Highlighter) => void;
  let reject!: (error?: unknown) => void;
  let promise!: Promise<Highlighter>;

  const reset = () => {
    promise = new Promise<Highlighter>((resolvePromise, rejectPromise) => {
      resolve = resolvePromise;
      reject = rejectPromise;
    });
  };
  reset();
  return {
    get promise() { return promise; },
    resolve: (highlighter: Highlighter) => resolve(highlighter),
    reject: (error: unknown) => reject(error),
    reset,
  };
});

vi.mock("shiki", () => ({
  createHighlighter: vi.fn(() => shikiState.promise),
  createOnigurumaEngine: vi.fn(() => ({})),
}));

import MarkdownBody from "./MarkdownBody";

it("falls back to plain Markdown rendering when Shiki fails", async () => {
  const host = document.createElement("div");
  const dispose = render(
    () => <MarkdownBody content="**fallback answer**" final={true} />,
    host,
  );

  await Promise.resolve();
  shikiState.reject(new Error("highlighter unavailable"));

  await vi.waitFor(() => expect(host.querySelector("strong")?.textContent).toBe("fallback answer"));
  dispose();
  shikiState.reset();
});

it("keeps the streaming DOM visible until final Markdown rendering completes", async () => {
  const host = document.createElement("div");
  const [content, setContent] = createSignal("partial stream");
  const [final, setFinal] = createSignal(false);
  const dispose = render(
    () => <MarkdownBody content={content()} final={final()} />,
    host,
  );

  expect(host.textContent).toContain("partial stream");
  setFinal(true);
  setContent("**final answer**");

  expect(host.textContent).toContain("partial stream");
  expect(host.textContent).not.toContain("**final answer**");

  shikiState.resolve({
    codeToHtml: text => `<pre><code>${text}</code></pre>`,
  });

  await vi.waitFor(() => expect(host.querySelector("strong")?.textContent).toBe("final answer"));
  expect(host.textContent).not.toContain("partial stream");
  dispose();
});

it("does not allow an older asynchronous final render to overwrite newer content", async () => {
  const host = document.createElement("div");
  const [content, setContent] = createSignal("old answer");
  const dispose = render(
    () => <MarkdownBody content={content()} final={true} />,
    host,
  );

  setContent("new answer");

  await vi.waitFor(() => expect(host.textContent).toContain("new answer"));
  expect(host.textContent).not.toContain("old answer");
  dispose();
});

it("renders inline and display LaTeX while preserving Markdown code as literal text", async () => {
  const host = document.createElement("div");
  const dispose = render(
    () => <MarkdownBody content={'Inline: $x^2$.\n\n$$\\frac{a}{b}$$\n\n`$not_math$`'} final={true} />,
    host,
  );

  await vi.waitFor(() => expect(host.querySelectorAll(".katex").length).toBe(2));
  expect(host.querySelector(".katex-display")).not.toBeNull();
  expect(host.querySelector("code")?.textContent).toBe("$not_math$");
  dispose();
});

it("replaces streaming code-block HTML with the single final block", async () => {
  const host = document.createElement("div");
  const [content, setContent] = createSignal("intro\n\n```ts\nconst a = 1;\n```\n\nafter");
  const [final, setFinal] = createSignal(false);
  const dispose = render(
    () => <MarkdownBody content={content()} final={final()} />,
    host,
  );

  // Streaming path: the code block gets a rendered wrapper (Shiki or fallback).
  shikiState.resolve({
    codeToHtml: text => `<pre class="shiki"><code>${text}</code></pre>`,
  });
  await vi.waitFor(() =>
    expect(host.querySelector(".code-block-wrapper")).not.toBeNull(),
  );
  expect(host.textContent).toContain("const a = 1");

  // Final render: the whole answer becomes one "f" block; streaming block
  // HTML is dropped so no second copy of the answer stays resident.
  setFinal(true);
  setContent("**final answer**");
  await vi.waitFor(() => expect(host.querySelector("strong")?.textContent).toBe("final answer"));

  expect(host.querySelector(".code-block-wrapper")).toBeNull();
  expect(host.querySelector('[data-key="b1"]')).toBeNull();
  expect(host.textContent).not.toContain("const a = 1");
  expect(host.textContent).not.toContain("intro");
  dispose();
  shikiState.reset();
});

it("renders closed inline markdown on the live tail while streaming", async () => {
  const host = document.createElement("div");
  const [content, setContent] = createSignal("partial **bold");
  const dispose = render(
    () => <MarkdownBody content={content()} final={false} />,
    host,
  );

  // 未闭合的强调保持字面（不产生破损 HTML）
  expect(host.textContent).toContain("**bold");

  // 闭合后立即实时渲染，无需等待流式结束
  setContent("partial **bold** text");
  await vi.waitFor(() => expect(host.querySelector("strong")?.textContent).toBe("bold"));
  expect(host.textContent).toContain("partial");

  // 链接同样实时生效
  setContent("see [docs](https://example.com) now");
  await vi.waitFor(() => {
    const link = host.querySelector("a") as HTMLAnchorElement | null;
    expect(link?.textContent).toBe("docs");
    expect(link?.getAttribute("href")).toBe("https://example.com");
  });
  dispose();
});
