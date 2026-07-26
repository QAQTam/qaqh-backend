import { createSignal, For } from "solid-js";
import type { ProcessItem } from "../../presentation/processAggregation";
import ProcessEventRow from "./ProcessEventRow";

export default function ProcessTimeline(props: {
  items: ProcessItem[];
  expandable?: boolean;
  defaultExpanded?: boolean;
  onExpand?: () => void;
}) {
  const expandAll = props.defaultExpanded ?? false;
  const [expandedId, setExpandedId] = createSignal<string | null>(expandAll ? "__auto__" : null);

  const isExpanded = (id: string) => {
    if (expandAll) return expandedId() !== null;
    return expandedId() === id;
  };

  const onToggle = (id: string) => {
    props.onExpand?.();
    setExpandedId(current => current === id ? null : id);
  };

  return (
    <div class="process-timeline" role="list">
      <For each={props.items} keyed={false}>
        {(item) => (
          <ProcessEventRow
            item={item()}
            expanded={() => isExpanded(item().id)}
            onToggle={() => onToggle(item().id)}
          />
        )}
      </For>
    </div>
  );
}
