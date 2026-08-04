import { createEffect, For, Show } from "solid-js";
import type { ProcessItem } from "../../presentation/processAggregation";
import { extractToolResultText } from "../../presentation/toolSemantics";

function detailText(item: ProcessItem): string {
  switch (item.kind) {
    case "reasoning": return item.content;
    case "assistant_progress": return item.markdown;
    case "tool": {
      // 有结果时优先结果文本（JSON 结果经 extractToolResultText 过滤
      // 噪音字段/提取关键信息；diff 与普通文本原样显示）。
      if (item.output != null) return extractToolResultText(item.output);
      const progress = item.progress?.map(event =>
        event.stream === "stderr" ? `[stderr] ${event.chunk}` : event.chunk,
      ).join("") ?? "";
      if (progress) return progress;
      return item.argsJson ?? "";
    }
    case "interaction": return item.resolution;
    case "notice": return item.message;
    case "group": return "";
  }
}

/** Attempt JSON parse + pretty-print. Returns null if not valid JSON. */
function tryFormatJson(raw: string): string | null {
  const trimmed = raw.trim();
  if (!trimmed || (trimmed[0] !== "{" && trimmed[0] !== "[")) return null;
  try {
    const parsed = JSON.parse(trimmed);
    return JSON.stringify(parsed, null, 2);
  } catch {
    return null;
  }
}

export default function ProcessDetail(props: { item: ProcessItem }) {
  let preRef!: HTMLPreElement;
  // 增量文本渲染游标：长思考链 / 长 exec 输出流式追加时，只把新增 delta
  // 追加为文本节点，避免每帧对完整累积文本做 <pre> 全量替换（O(n²) DOM
  // 更新，主线程被占满导致 UI 冻结——即使不渲染 Markdown）。
  let lastText = "";
  let lastTextLen = 0;

  /** 流式中的工具输出：跳过 JSON 重排（大 JSON 输出每次 delta parse 会卡死），
   *  等输出稳定（tool 完成/无 progress）后再格式化。 */
  const isStreamingTail = () =>
    props.item.kind === "tool" && (props.item.progress?.length ?? 0) > 0;

  const displayText = () => {
    const text = detailText(props.item);
    if (isStreamingTail()) return text;
    const formatted = tryFormatJson(text);
    return formatted !== null ? formatted : text;
  };
  const isJson = () => {
    if (isStreamingTail()) return false;
    return tryFormatJson(detailText(props.item)) !== null;
  };

  createEffect(
    () => displayText(),
    (text) => {
      const pre = preRef;
      if (!pre) return;
      const appendable = !isJson() && text.length >= lastTextLen && text.startsWith(lastText);
      if (appendable) {
        if (text.length > lastTextLen) {
          // 纯追加：只处理新增部分（O(delta)）
          pre.appendChild(document.createTextNode(text.slice(lastTextLen)));
        }
      } else {
        // 内容跳变（换块 / JSON 重排 / 回退）：全量替换
        pre.textContent = text;
      }
      lastText = text;
      lastTextLen = text.length;
      pre.scrollTop = pre.scrollHeight;
    },
  );

  const statusBadge = () => {
    if (props.item.kind !== "tool") return null;
    if (props.item.status === "ok" || props.item.status === "backgrounded") {
      return <span class="process-tool-status success">✅ 成功</span>;
    }
    if (props.item.status === "error" || props.item.status === "partial" || props.item.status === "cancelled") {
      return <span class="process-tool-status failure">❌ 失败</span>;
    }
    return <span class="process-tool-status pending">⏳ 等待中</span>;
  };

  return (
    <div class="process-detail">
      <Show when={props.item.kind === "group"} fallback={
        <>
          <div class="process-detail-badges">
            <Show when={statusBadge()}>{statusBadge()}</Show>
            <Show when={isJson()}>
              <span class="process-tool-status json" aria-label="JSON 格式">JSON</span>
            </Show>
          </div>
          <pre ref={preRef} data-format={isJson() ? "json" : "text"} />
        </>
      }>
        <ul class="process-group-children">
          <For each={(props.item as Extract<ProcessItem, { kind: "group" }>).children}>
            {(child) => <li>{child.kind === "tool" ? child.summary : child.kind}</li>}
          </For>
        </ul>
      </Show>
    </div>
  );
}
