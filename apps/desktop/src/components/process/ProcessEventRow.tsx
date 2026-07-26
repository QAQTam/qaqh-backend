import type { ProcessItem } from "../../presentation/processAggregation";
import ProcessDetail from "./ProcessDetail";

function label(item: ProcessItem): string {
  switch (item.kind) {
    case "reasoning": return "分析";
    case "assistant_progress": return "阶段结论";
    case "tool": return truncate(item.summary || item.toolName, 48);
    case "group": return item.label;
    case "interaction": return `${item.label}: ${item.resolution}`;
    case "notice": return item.message;
  }
}

function truncate(text: string, max: number): string {
  return text.length > max ? text.slice(0, max) + "..." : text;
}

function toolHint(item: ProcessItem): string | null {
  if (item.kind !== "tool") return null;
  const parts: string[] = [];
  if (item.success === true) parts.push("✓");
  else if (item.success === false) parts.push("✗");
  if (item.output != null) {
    const n = item.output.length;
    parts.push(n > 1024 ? `${(n / 1024).toFixed(1)}k` : `${n}`);
  }
  return parts.length ? parts.join(" ") : null;
}

export default function ProcessEventRow(props: {
  item: ProcessItem;
  expanded: () => boolean;
  onToggle: () => void;
}) {
  const failed = () => props.item.kind === "tool" && props.item.success === false;
  const pending = () => props.item.kind === "tool" && props.item.output == null && props.item.success == null;

  return (
    // @ts-expect-error SolidJS 2.x: tsc children type mismatch on div
    <div
      class={{ "process-event-row": true, "is-failed": failed(), "is-pending": pending() }}
      data-process-row
      data-kind={props.item.kind}
      aria-expanded={String(props.expanded())}
      role="listitem"
    >
      <button type="button" class="process-event-trigger" onClick={props.onToggle}>
        <span class="process-event-icon" aria-hidden="true">
          {failed() ? "!" : pending() ? <span class="process-spinner" /> : props.item.kind === "tool" ? "›_" : "·"}
        </span>
        <span class="process-event-label">{label(props.item)}</span>
        {toolHint(props.item) != null && <span class="process-event-hint">{toolHint(props.item)!}</span>}
        <span class="process-event-expand" aria-hidden="true">{props.expanded() ? "−" : "+"}</span>
      </button>
      {props.expanded() && <ProcessDetail item={props.item} />}
    </div>
  );
}
