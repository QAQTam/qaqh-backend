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
