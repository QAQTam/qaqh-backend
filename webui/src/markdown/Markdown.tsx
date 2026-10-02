/**
 * 流式 Markdown 块组件(spec §7.5):闭合块渲染一次并缓存(真实 DOM 只追加,
 * 不重建),未闭合尾块每次文本变化重解析;rAF 合并由上游 store 保证。
 *
 * `revision` 变化(block_checkpoint 全量替换/重置)时清空缓存整体重渲染。
 */
import { createEffect, type Component } from "solid-js";
import { splitBlocks } from "./split";
import { renderMarkdownHtml } from "./render";

export const Markdown: Component<{ text: () => string; revision?: string | number }> = (props) => {
  let host!: HTMLDivElement;
  let tail: HTMLDivElement | undefined;
  const cache: string[] = [];
  let sealedCount = 0;
  let renderedRevision = props.revision ?? 0;

  const reset = (): void => {
    cache.length = 0;
    sealedCount = 0;
    while (host.firstChild && host.firstChild !== tail) host.firstChild.remove();
  };

  const sealInto = (html: string): void => {
    const node = document.createElement("div");
    node.className = "md-block";
    node.innerHTML = html; // 已净化输出
    host.insertBefore(node, tail!);
  };

  createEffect(
    () => [props.text(), props.revision ?? 0] as const,
    ([text, revision]) => {
      if (revision !== renderedRevision) {
        renderedRevision = revision;
        reset();
      }
      const blocks = splitBlocks(text);
    if (blocks.length < sealedCount) reset();
    // 闭合 [sealedCount, blocks.length-1) 的块:内容 append-only,不会再变。
    while (sealedCount < blocks.length - 1) {
      const block = blocks[sealedCount]!;
      cache[sealedCount] = renderMarkdownHtml(block.content);
      sealInto(cache[sealedCount]!);
      sealedCount += 1;
    }
      const tailBlock = blocks[blocks.length - 1];
      if (tail) tail.innerHTML = tailBlock ? renderMarkdownHtml(tailBlock.content) : "";
    },
  );

  return (
    <div class="md-host" ref={host}>
      <div class="md-tail" ref={tail} />
    </div>
  );
};
