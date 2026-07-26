import { For, Show } from "solid-js";
import type { createFollowUpQueue } from "../../store/followUpQueue";

type Queue = ReturnType<typeof createFollowUpQueue>;
export default function ComposerQueue(props: { queue: Queue }) {
  return <Show when={props.queue.items().length > 0}>
    <div class="composer-queue">
      <div>{props.queue.items().length} 条后续任务已排队</div>
      <For each={props.queue.items()}>{item => <div class="composer-queue-item"><span>{item.text}</span><button onClick={() => props.queue.remove(item.id)}>×</button></div>}</For>
    </div>
  </Show>;
}
