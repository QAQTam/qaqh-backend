import { createEffect, For, Show } from "solid-js";
import type { ProcessItem } from "../../presentation/processAggregation";

function detailText(item: ProcessItem): string {
  switch (item.kind) {
    case "reasoning": return item.content;
    case "assistant_progress": return item.markdown;
    case "tool": {
      const progress = item.progress?.map(event =>
        event.stream === "stderr" ? `[stderr] ${event.chunk}` : event.chunk,
      ).join("") ?? "";
      return item.output ?? progress ?? item.argsJson ?? "";
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
  const raw = () => detailText(props.item);
  const formattedJson = () => tryFormatJson(raw());
  const isJson = () => formattedJson() !== null;
  const displayText = () => isJson() ? formattedJson()! : raw();

  createEffect(
    () => displayText(),
    () => {
      if (preRef) preRef.scrollTop = preRef.scrollHeight;
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
          <pre ref={preRef} data-format={isJson() ? "json" : "text"}>{displayText()}</pre>
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
