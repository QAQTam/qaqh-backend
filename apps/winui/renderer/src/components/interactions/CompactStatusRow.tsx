import { createSignal, Show } from "solid-js";

interface CompactStatusRowProps {
  active: boolean;
  text: string;
  status: "active" | "complete" | "failed";
  turnsCompacted?: number;
  onExpand?: () => void;
}

export default function CompactStatusRow(props: CompactStatusRowProps) {
  const [expanded, setExpanded] = createSignal(false);
  const displayText = () => props.text.trim() || (props.active ? "正在整理上下文…" : "");
  const toggleExpanded = () => {
    setExpanded((current) => {
      const next = !current;
      if (next) props.onExpand?.();
      return next;
    });
  };

  return (
    <div
      class={[`compact-row compact-${props.status}`, {
        "compact-active": props.active,
        "compact-complete": props.status === "complete",
        "compact-failed": props.status === "failed",
      }]}
    >
      <div class="compact-row-inner">
        {/* Status indicator */}
        <span class="compact-indicator">
          <Show when={props.active}>
            <span class="compact-spinner" />
          </Show>
          <Show when={props.status === "complete"}>✓</Show>
          <Show when={props.status === "failed"}>✕</Show>
        </span>

        {/* Text (collapsed or expanded) */}
        <span
          class={{ "compact-text": true, "compact-text-expanded": expanded() }}
          onClick={toggleExpanded}
        >
          {expanded()
            ? displayText()
            : displayText().slice(0, 50) + (displayText().length > 50 ? "…" : "")}
        </span>

        {/* Turns compacted (complete state) */}
        <Show when={props.status === "complete" && props.turnsCompacted != null}>
          <span class="compact-result">
            — {props.turnsCompacted} 轮对话
          </span>
        </Show>

        {/* Expand toggle */}
        <Show when={props.onExpand}>
          <button class="compact-expand-btn" onClick={toggleExpanded}>
            {expanded() ? "收起" : "展开"}
          </button>
        </Show>
      </div>
    </div>
  );
}
