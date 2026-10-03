/**
 * 流式 Markdown 块组件(spec §7.5):闭合块渲染一次直接 append(真实 DOM 只追加,
 * 不重建),未闭合尾块**按帧**重绘;单帧绘制超预算时自动退回限频。
 *
 * 两层节奏:
 *  - `reveal`:store 里的文本是真相,一次突发可能带来几百字。直接画就是「蹦」,
 *    所以这里按帧把突发摊平(纯表现层,不改数据、不改 store)。
 *  - 预算自适应:尾块形状决定代价 —— 段落/代码每帧画得起,未闭合表格画不起。
 *    实测见 PAINT_BUDGET_MS 注释;超预算就退回 THROTTLE_MS,不再一刀切 120ms
 *    把观感钉在 ~8fps。
 *
 * `revision` 变化(block_checkpoint 全量替换/重置)时清空缓存整体重渲染。
 */
import { createEffect, onCleanup, type Component } from "solid-js";
import { splitBlocks } from "./split";
import { renderMarkdownHtml } from "./render";

/**
 * 单帧同步绘制预算(ms)。120Hz 一帧 8.3ms,留一半给布局与绘制。
 * 夹具实测(尾块持续增长 120 帧):段落 p50 0.6 / max 1.8ms;未闭合代码块
 * p50 0.3 / max 2.7ms;未闭合表格 p50 5.9 / p95 11.7 / max 15.5ms。
 */
const PAINT_BUDGET_MS = 4;
/** 超预算时的退回限频(ms)。 */
const THROTTLE_MS = 120;
/** 突发摊到几帧内露完:6 帧 ≈ 50ms@120Hz,连得上且不把延迟堆成长尾。 */
const REVEAL_FRAMES = 6;
/** 落后超过这个字符量说明不是实时流(快照整块落地/切标签回来),直接全量显示。 */
const CATCH_UP_CHARS = 4_000;

export const Markdown: Component<{ text: () => string; revision?: string | number }> = (props) => {
  let host!: HTMLDivElement;
  let tail: HTMLDivElement | undefined;
  let sealedCount = 0;
  let renderedRevision: string | number | null = null;

  let target = ""; // store 里的全文(真相)
  let shown = 0; // 已露出的字符数(表现)
  let revisionKey: string | number = 0;
  let lastCostMs = 0;
  let earliestAt = 0;
  let handle: number | null = null;

  const reset = (): void => {
    sealedCount = 0;
    while (host.firstChild && host.firstChild !== tail) host.firstChild.remove();
  };

  const sealInto = (html: string): void => {
    const node = document.createElement("div");
    node.className = "md-block";
    node.innerHTML = html; // 已净化输出
    host.insertBefore(node, tail!);
  };

  /** 绘制露出前缀,并记下这次花了多久(下一帧的节奏据此决定)。 */
  const draw = (): void => {
    const startedAt = performance.now();
    const text = shown >= target.length ? target : target.slice(0, shown);
    if (renderedRevision !== revisionKey) {
      renderedRevision = revisionKey;
      reset();
    }
    const blocks = splitBlocks(text);
    if (blocks.length < sealedCount) reset();
    // 闭合 [sealedCount, blocks.length-1) 的块:内容 append-only,不会再变——
    // 渲染一次直接写进 DOM,不再缓存 HTML 字符串(缓存等于把整篇输出存两遍)。
    while (sealedCount < blocks.length - 1) {
      const block = blocks[sealedCount]!;
      sealInto(renderMarkdownHtml(block.content));
      sealedCount += 1;
    }
    const tailBlock = blocks[blocks.length - 1];
    if (tail) tail.innerHTML = tailBlock ? renderMarkdownHtml(tailBlock.content) : "";
    lastCostMs = performance.now() - startedAt;
    earliestAt = performance.now() + (lastCostMs > PAINT_BUDGET_MS ? THROTTLE_MS : 0);
  };

  const tick = (): void => {
    handle = null;
    const backlog = target.length - shown;
    if (backlog <= 0) return;
    if (performance.now() < earliestAt) {
      handle = requestAnimationFrame(tick);
      return;
    }
    // 落后太多 = 不是逐帧流进来的(快照/回切),没必要再动画。
    shown = backlog > CATCH_UP_CHARS ? target.length : shown + Math.max(1, Math.ceil(backlog / REVEAL_FRAMES));
    draw();
    if (shown < target.length) handle = requestAnimationFrame(tick);
  };

  const schedule = (): void => {
    if (handle == null) handle = requestAnimationFrame(tick);
  };

  let primed = false; // 首帧不动画:静态文本块不该有「从零长出来」的入场

  createEffect(
    () => [props.text(), props.revision ?? 0] as const,
    ([text, revision]) => {
      // 全量替换(checkpoint / 权威快照重写)或换块(revision 变了):按真相整块画。
      const wholesale = !primed || revision !== revisionKey || (text !== target && !text.startsWith(target));
      target = text;
      revisionKey = revision;
      if (wholesale) {
        primed = true;
        shown = text.length;
        draw();
        return;
      }
      if (shown > text.length) shown = text.length;
      if (shown < text.length) schedule();
    },
  );

  onCleanup(() => {
    if (handle != null) cancelAnimationFrame(handle);
    handle = null;
  });

  return (
    <div class="md-host" ref={host}>
      <div class="md-tail" ref={tail} />
    </div>
  );
};
